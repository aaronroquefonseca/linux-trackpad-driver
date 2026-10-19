//! Daemon lifecycle: `run` loop + `reconnect` udev entry.
//!
//! `run`: acquires [`SocketLock`], discovers devices, runs init per node
//! (USB feature reports / BT single-node), emits [`DeviceReadyEvent`] on the
//! shared [`DeviceReadyHub`], then idles until signal (SIGINT/SIGTERM/SIGHUP).
//! No devices = log + idle (graceful, exit 0 on signal), never panic.
//!
//! `reconnect <dev>`: idempotent per-node re-init for udev `add`. Daemon
//! socket absent without `-f` → disengage path (host-click off, no init).
//! `-p` overrides the polling request (same ~110 Hz cap). Always re-sends
//! multitouch enable first: the resume regression (report 0x02 21→8 bytes
//! across power transition) is only fixed by re-init.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use anyhow::{Context, Result};

use crate::config::Config;
use crate::discovery::{self, DiscoveredDevice};
use crate::init::{
    self, DeviceReadyEvent, DeviceReadyHub, HidrawSender, Transport, EFFECTIVE_CAP_HZ,
};
use crate::lock::{self, SocketLock};
use crate::resume::{self, ResumeReport};

/// Shared device-ready hub for this process. The daemon proxy subscribes.
pub fn hub() -> DeviceReadyHub {
    DeviceReadyHub::new()
}

/// Subscribe the opt-in click proxy to `ready_hub` (daemon only).
/// Gate-off (`enhanced_clicks=false`) still subscribes — the proxy then
/// short-circuits to passthrough. The gate is evaluated from the startup
/// config; there is no live reload, so a config change needs a restart.
/// Never blocks init: callbacks do bounded attach/detach only, and grab
/// failure degrades to passthrough with a log.
///
/// Production pump: `attach` builds the real path — BOTH uinput nodes
/// (`UI_DEV_CREATE` pointer FIRST, then extras), hidraw→eventN
/// auto-resolution (`$LTD_EVDEV_NODE` override wins, else sysfs walk, else
/// name scan), then hidraw open, then `set_pump_ready(true)` + grab.
/// `detach` tears down in reverse (release grab, latch reset, trailing
/// `SYN` on both nodes, source to idle, BOTH nodes destroyed). Single
/// active node: first `Ready` wins, siblings ignored until `Detached`
/// clears it — except a `Ready` for the active node whose session died on
/// its own (disconnect self-release): that tears down the stale wiring
/// and re-attaches fresh fds instead of early-returning into a dead
/// session. A single pump thread drains the proxy for the process
/// lifetime; frame rate is device-bounded.
///
/// Two-node failure isolation: pointer-create failure → passthrough (no
/// touch emit path, so no grab — same as the old single node). Extras-
/// create failure is NON-fatal: the session continues pointer-only
/// (motion/buttons live, combos dropped) with a loud log, and teardown
/// always destroys both nodes so no sibling leaks.
///
/// Attach decision for a `Ready` event (see [`attach_decision`]):
/// - no active session → adopt (full attach);
/// - same node + live proxy → ignore (duplicate);
/// - same node + dead proxy → teardown + full re-attach;
/// - another node active → ignore (sibling).
pub fn subscribe_click_proxy(cfg: &Config, ready_hub: &DeviceReadyHub) -> impl Fn(&str) -> bool {
    use crate::proxy::{
        AsyncHaptic, ClickProxy, DualSink, ProxySettings, RealGrab, RealHaptic, RealUinput,
        UinputSink,
    };
    use crate::pump::PumpSource;

    type DaemonProxy =
        ClickProxy<RealGrab, DualSink<RealUinput, RealUinput>, AsyncHaptic<RealHaptic>, PumpSource>;

    struct PumpManager {
        proxy: std::sync::Arc<std::sync::Mutex<DaemonProxy>>,
        cfg: Config,
        active: std::sync::Mutex<Option<String>>,
        haptic_node: std::sync::Mutex<Option<String>>,
        _stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
        _thread: std::thread::JoinHandle<()>,
    }

    impl PumpManager {
        fn attach(&self, event: &crate::init::DeviceReadyEvent) {
            use crate::init::ReadyKind;
            if event.kind != ReadyKind::Ready {
                return;
            }
            if !self.cfg.enhanced_clicks {
                self.proxy
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .attach(event);
                return;
            }
            // Enumeration can expose a silent feature interface before
            // the If1 touch source. Do not let that early sibling own the
            // single pump; its later If1 event will wake reconciliation.
            if event.transport == Transport::Usb
                && !touch_interface_eligible(discovery::interface_number_for_node(&event.devnode))
            {
                log::debug!(
                    "pump: {} is not the USB touch interface; waiting for If1",
                    event.devnode
                );
                return;
            }
            {
                let active = self.active.lock().unwrap_or_else(|e| e.into_inner());
                let live = self
                    .proxy
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .is_attached();
                match attach_decision(active.as_deref(), &event.devnode, live) {
                    AttachDecision::Adopt => {}
                    AttachDecision::IgnoreDuplicate => return,
                    AttachDecision::IgnoreSibling => {
                        log::info!(
                            "pump: {} ready but {} active; ignoring sibling",
                            event.devnode,
                            active.as_deref().unwrap_or("?"),
                        );
                        return;
                    }
                    AttachDecision::Reattach => {
                        log::info!(
                            "pump: {} known but session dead; tearing down + re-attaching",
                            event.devnode
                        );
                        drop(active);
                        self.detach(&init::detached(&event.devnode, event.transport));
                    }
                }
            }
            // Proven-selection log (INFO): which node, which USB interface,
            // and WHY it sorts first (If1 pump rule preferred, MT abs bits
            // tiebreak only). Owner 2026-10-15: silent hidraw8=If0 won on
            // position/MT-rank alone while hidraw9=If1 streamed — this line
            // is the journal proof of the fix.
            {
                let iface = discovery::interface_number_for_node(&event.devnode);
                let reason = discovery::selection_reason_for_node(&event.devnode);
                log::info!(
                    "pump: attaching {} (interface {:?}, {})",
                    event.devnode,
                    iface,
                    reason,
                );
            }
            // 1. Create the virtual nodes BEFORE grab: pointer first (the
            // touch emit path grab safety depends on), then extras. A
            // pointer failure stays passthrough; an extras failure only
            // drops combos (pointer-only session, loud log).
            let created = {
                let px = self.proxy.lock().unwrap_or_else(|e| e.into_inner());
                match px.uinput().create_pointer() {
                    Ok(()) => true,
                    Err(e) => {
                        log::warn!(
                            "pump: pointer uinput create failed for {}: {e:#}; passthrough",
                            event.devnode
                        );
                        false
                    }
                }
            };
            if !created {
                self.proxy
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .attach(event);
                return;
            }
            {
                let px = self.proxy.lock().unwrap_or_else(|e| e.into_inner());
                if let Err(e) = px.uinput().create_extras() {
                    log::warn!(
                        "pump: extras uinput create failed for {}: {e:#}; continuing pointer-only (combos dropped)",
                        event.devnode
                    );
                }
            }
            // 2. Resolve the physical node (env override → sysfs → name).
            let Some((evdev, src)) = crate::evdev::resolve_evdev_node(&event.devnode) else {
                log::warn!(
                    "pump: no evdev for {}; destroying uinput nodes, passthrough",
                    event.devnode
                );
                self.proxy
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .uinput()
                    .destroy();
                return;
            };
            if !crate::evdev::grab_allowed(&src) {
                // NameScan hit: untrusted for grabbing (no hidraw
                // parentage proof — could be a foreign node). Refuse the
                // grab, destroy the node we just created, passthrough
                // with a LOUD log. An input-less feature interface
                // (hidraw10/11) lands here by design; the touch
                // interface's sysfs-backed Ready attaches separately.
                log::error!(
                    "pump: evdev for {} resolved via {:?} (UNTRUSTED, no grab); destroying uinput nodes, passthrough",
                    event.devnode,
                    src,
                );
                self.proxy
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .uinput()
                    .destroy();
                return;
            }
            // 3. Open the hidraw source nonblocking.
            let source = match crate::pump::HidrawSource::open(&event.devnode) {
                Ok(s) => s,
                Err(e) => {
                    log::warn!(
                        "pump: hidraw open {} failed: {e:#}; destroying uinput nodes, passthrough",
                        event.devnode
                    );
                    self.proxy
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .uinput()
                        .destroy();
                    return;
                }
            };
            // 3b. Attach-time stream verify: nonblocking poll of the chosen
            // node + siblings with a short timeout. Chosen silent while a
            // sibling streams = the 2026-10-15 misbind (silent hidraw8 vs
            // live hidraw9): loud-warn naming both nodes + hint. All
            // silent = finger-off idle, debug only (no false alarm).
            verify_attach_stream(&event.devnode);
            // 4. Wire nodes, flip ready, then grab (attach checks both gates).
            // Haptics go to the interface-2 sibling when the USB layout
            // exposes one (actuator interface); otherwise the touch node
            // itself, loudly logged as pending HW verify.
            {
                let mut px = self.proxy.lock().unwrap_or_else(|e| e.into_inner());
                px.grab().set_node(&evdev.to_string_lossy());
                let (hnode, hiface) = crate::evdev::haptic_node_for(&event.devnode);
                px.haptic_mut().inner().set_node(&hnode);
                *self.haptic_node.lock().unwrap_or_else(|e| e.into_inner()) = Some(hnode.clone());
                log::info!(
                    "pump: haptic node {hnode} (interface {hiface:?}) for {}",
                    event.devnode
                );
                px.haptic_mut()
                    .inner()
                    .set_bluetooth(event.transport == crate::init::Transport::Bluetooth);
                px.replace_source(PumpSource::Live(source));
                px.set_pump_ready(true);
                px.attach(event);
                if px.is_attached() {
                    *self.active.lock().unwrap_or_else(|e| e.into_inner()) =
                        Some(event.devnode.clone());
                } else {
                    // Grab refused: single teardown unwinds to idle (held
                    // keys + virtual contacts terminated on the wire when
                    // possible) + destroy both so no orphan nodes.
                    px.teardown_session("attach rollback: grab refused");
                    px.replace_source(PumpSource::Idle);
                    px.uinput().destroy();
                }
            }
        }

        /// Idempotent manager teardown: single proxy teardown (contacts +
        /// keys ended on the wire) + source to idle + both virtual nodes
        /// destroyed. Used by shutdown (via `Drop`); detach/rollback run
        /// the same proxy teardown through `detach`/`teardown_session`
        /// plus idle+destroy.
        fn teardown_locked(proxy: &mut DaemonProxy, reason: &str) {
            proxy.teardown_session(reason);
            proxy.replace_source(PumpSource::Idle);
            proxy.uinput().destroy();
        }

        fn detach(&self, event: &crate::init::DeviceReadyEvent) {
            use crate::init::ReadyKind;
            if event.kind != ReadyKind::Detached {
                return;
            }
            let owned = {
                let mut active = self.active.lock().unwrap_or_else(|e| e.into_inner());
                if active.as_deref() == Some(event.devnode.as_str()) {
                    active.take();
                    true
                } else {
                    false
                }
            };
            if !owned {
                // Sibling vanishing while another node owns the pump: touch
                // nothing. The proxy itself also ignores foreign Detached
                // events (ownership gate in `ClickProxy::detach`), so the
                // live grab survives even if this gate is ever bypassed.
                log::debug!(
                    "pump: ignoring Detached for unowned {} (active: {:?})",
                    event.devnode,
                    self.active.lock().unwrap_or_else(|e| e.into_inner())
                );
                return;
            }
            {
                let mut px = self.proxy.lock().unwrap_or_else(|e| e.into_inner());
                // `detach` runs the single idempotent proxy teardown
                // (contacts + keys ended on the wire); manager then idles
                // the source and destroys both virtual nodes.
                px.detach(event);
                px.replace_source(PumpSource::Idle);
                px.uinput().destroy();
                log::info!("pump: {} torn down", event.devnode);
            }
        }
    }

    impl Drop for PumpManager {
        /// Shutdown path: terminate virtual contacts + keys, idle the
        /// source, destroy both virtual nodes. Idempotent: safe after an
        /// explicit detach already tore the session down. The pump thread
        /// is signaled to stop; its handle detaches with the manager at
        /// process teardown (existing lifecycle, see finding #12).
        fn drop(&mut self) {
            self._stop.store(true, std::sync::atomic::Ordering::SeqCst);
            let mut px = self.proxy.lock().unwrap_or_else(|e| e.into_inner());
            Self::teardown_locked(&mut px, "shutdown");
        }
    }

    let uinput_node = std::env::var("LTD_UINPUT_NODE").unwrap_or("/dev/uinput".to_string());
    let proxy = std::sync::Arc::new(std::sync::Mutex::new(DaemonProxy::new(
        ProxySettings::from(cfg),
        RealGrab::new(""),
        DualSink::new(
            RealUinput::new(&uinput_node),
            RealUinput::new_extras(&uinput_node),
        ),
        AsyncHaptic::new(RealHaptic::new()),
        PumpSource::Idle,
    )));
    let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let thread = crate::pump::spawn_pump(
        "daemon".to_string(),
        std::sync::Arc::clone(&proxy),
        cfg.clone(),
        std::sync::Arc::clone(&stop),
    );
    let manager = std::sync::Arc::new(PumpManager {
        proxy,
        cfg: cfg.clone(),
        active: std::sync::Mutex::new(None),
        haptic_node: std::sync::Mutex::new(None),
        _stop: stop,
        _thread: thread,
    });
    // NOTE: `PumpManager._thread` is never joined — the pump lives until
    // process exit by design. `JoinHandle` is held (not detached) so a
    // panic inside the pump does not silently vanish; the handle is
    // dropped with the manager at process teardown.
    let m = std::sync::Arc::clone(&manager);
    ready_hub.subscribe(move |event: &crate::init::DeviceReadyEvent| {
        use crate::init::ReadyKind;
        match event.kind {
            ReadyKind::Ready => m.attach(event),
            ReadyKind::Detached => m.detach(event),
        }
    });
    log::debug!(
        "click proxy subscribed (enhanced_clicks={})",
        cfg.enhanced_clicks
    );
    // Query actual proxy health, independent of hardware polling state.
    // Siblings wait while an owner exists; a dead owner retries its own
    // node first, using the existing teardown/re-attach path.
    move |devnode| {
        if !manager.cfg.enhanced_clicks
            || !touch_interface_eligible(discovery::interface_number_for_node(devnode))
        {
            return false;
        }
        let active = manager.active.lock().unwrap_or_else(|e| e.into_inner());
        let mut proxy = manager.proxy.lock().unwrap_or_else(|e| e.into_inner());
        let live = proxy.is_attached();
        if live && active.as_deref() == Some(devnode) {
            // A later actuator sibling must not require a re-grab of the
            // healthy touch session. Resolve quietly; log only a route change.
            let (node, _) = crate::evdev::haptic_node_for_quiet(devnode);
            let mut target = manager
                .haptic_node
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            if target.as_deref() != Some(node.as_str()) {
                proxy.haptic_mut().inner().set_node(&node);
                log::info!("pump: late sibling updated haptic node to {node} for {devnode}");
                *target = Some(node);
            }
        }
        session_needs_attach(
            manager.cfg.enhanced_clicks,
            active.as_deref(),
            devnode,
            live,
        )
    }
}

// Unknown layouts (including Bluetooth) keep their existing source path.
fn touch_interface_eligible(iface: Option<u8>) -> bool {
    iface.is_none_or(|number| number == 1)
}

fn session_needs_attach(enhanced: bool, active: Option<&str>, devnode: &str, live: bool) -> bool {
    enhanced
        && matches!(
            attach_decision(active, devnode, live),
            AttachDecision::Adopt | AttachDecision::Reattach
        )
}

/// Attach decision for a `Ready` event against manager state.
///
/// `Reattach` is the same-name-replug path: the node is still `active`
/// but its session died on its own (disconnect self-release), so the
/// caller must tear down the stale wiring and attach fresh fds instead
/// of early-returning into a permanently dead session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AttachDecision {
    Adopt,
    IgnoreDuplicate,
    IgnoreSibling,
    Reattach,
}

pub(crate) fn attach_decision(
    active: Option<&str>,
    devnode: &str,
    proxy_attached: bool,
) -> AttachDecision {
    match active {
        None => AttachDecision::Adopt,
        Some(cur) if cur == devnode => {
            if proxy_attached {
                AttachDecision::IgnoreDuplicate
            } else {
                AttachDecision::Reattach
            }
        }
        Some(_) => AttachDecision::IgnoreSibling,
    }
}

/// Per-node probe budget for [`verify_attach_stream`] (short: hub
/// callbacks must stay bounded; 4 interfaces × 40 ms ≈ 160 ms worst).
const ATTACH_PROBE_MS_PER_NODE: u64 = 40;

/// Attach-time stream verify (see call site): poll the chosen node plus
/// live scan siblings nonblocking; loud-warn on silent-chosen-while-
/// sibling-streams, debug otherwise. Pure decision via
/// [`discovery::check_stream_health`]; this wrapper does the I/O + log.
fn verify_attach_stream(chosen: &str) {
    let siblings: Vec<String> = discovery::scan_trackpads()
        .unwrap_or_default()
        .into_iter()
        .map(|d| d.devnode)
        .collect();
    // Probe chosen first so a live chosen short-circuits to Healthy
    // without waiting on silent siblings; then each sibling.
    let mut ordered = vec![chosen.to_string()];
    for s in siblings {
        if s != chosen {
            ordered.push(s);
        }
    }
    let mut streaming = Vec::new();
    for node in &ordered {
        if discovery::poll_has_report(node, ATTACH_PROBE_MS_PER_NODE) {
            streaming.push(node.clone());
        }
    }
    match discovery::check_stream_health(chosen, &streaming) {
        discovery::StreamHealth::Healthy => {
            log::debug!("pump: attach verify: {chosen} streaming");
        }
        discovery::StreamHealth::SilentWhileSiblingStreams { streaming: live } => {
            log::warn!(
                "pump: chosen {chosen} silent while sibling {live} streams; \
                 hidraw numbers shift across enumeration — check MT-proven selection (prefer abs-MT node), \
                 consider LTD_EVDEV_NODE pin or re-attach to {live}",
            );
        }
        discovery::StreamHealth::IdleNoTraffic => {
            log::debug!("pump: attach verify: no touch traffic (idle, finger off)");
        }
    }
}

/// Initialize one discovered node and return its ready event.
/// USB: defer (node+bind) → retrying feature reports → verify → ready.
/// BT: no reports, straight to ready. Native input is suppressed only by a
/// live proxy's reversible evdev grab, never by hardware initialization.
/// Device setup runs independently of the enhanced-clicks gate.
pub fn init_node(
    dev: &DiscoveredDevice,
    polling_hz: u32,
    ready_hub: &DeviceReadyHub,
) -> Result<DeviceReadyEvent> {
    match dev.transport {
        Transport::Bluetooth => {
            let ev = init::init_bluetooth(&dev.devnode);
            ready_hub.emit(&ev);
            Ok(ev)
        }
        Transport::Usb => {
            // Defer-retry-verify: node may lag udev `add`, bind may lag node;
            // retrying writes + read-back verify carry the rest.
            let (node, bound) = discovery::wait_for_ready(&dev.devnode, 2000);
            if !node {
                log::warn!(
                    "node {} not present after 2 s wait; attempting init anyway",
                    dev.devnode
                );
            } else if !bound {
                log::debug!(
                    "node {} present, driver bind not yet visible; retrying writes will cover it",
                    dev.devnode
                );
            }
            let sender = HidrawSender {
                devnode: dev.devnode.clone(),
            };
            let verifier = init::HidrawVerifier {
                devnode: dev.devnode.clone(),
            };
            let iface = discovery::interface_number_for_node(&dev.devnode);
            let outcome = init::init_usb_verified(&sender, &verifier, polling_hz, iface)?;
            log::debug!(
                "USB init on {} accepted (iface {:?}, polling [99 F2 {:02X}])",
                dev.devnode,
                outcome.iface,
                outcome.plan.x,
            );
            let ev = init::usb_ready(&dev.devnode);
            ready_hub.emit(&ev);
            Ok(ev)
        }
    }
}

/// `run` daemon loop. Holds the socket lock, inits all current devices,
/// then parks until signal. Clean exit removes the socket.
pub fn run_daemon(cfg: &Config) -> Result<()> {
    log::info!("starting {}", crate::build_identity::IDENTITY);
    let sock = lock::socket_path();
    let _lock = SocketLock::acquire(&sock)?;
    log::info!("lock acquired at {sock}");
    log::info!(
        "config: polling {} Hz, enhanced_clicks={}",
        cfg.device.polling_rate,
        cfg.enhanced_clicks
    );
    if cfg.device.polling_rate > EFFECTIVE_CAP_HZ as u32 {
        log::warn!(
            "requested {} Hz above device cap; clamped to ~110 Hz (X=0x23)",
            cfg.device.polling_rate
        );
    }

    let running = Arc::new(AtomicBool::new(true));
    let stop = Arc::clone(&running);
    // ctrlc covers SIGINT/SIGTERM/SIGHUP on Unix.
    ctrlc::set_handler(move || {
        log::info!("received shutdown signal, shutting down...");
        stop.store(false, Ordering::SeqCst);
    })
    .map_err(|e| anyhow::anyhow!("installing signal handler: {e}"))?;

    let mut monitor = match crate::hotplug::Monitor::open() {
        Ok(monitor) => {
            log::info!("hotplug: kernel device monitor active (30 s verification fallback)");
            Some(monitor)
        }
        Err(error) => {
            log::warn!("hotplug: monitor unavailable: {error}; using 30 s discovery fallback");
            None
        }
    };
    let ready_hub = hub();
    let needs_attach = subscribe_click_proxy(cfg, &ready_hub);
    let initial: Vec<DiscoveredDevice> = match discovery::scan_trackpads() {
        Ok(devs) if devs.is_empty() => {
            log::info!("no supported trackpads found; idling until signal");
            Vec::new()
        }
        Ok(devs) => {
            log::info!("found {} trackpad node(s)", devs.len());
            devs
        }
        Err(e) => {
            log::warn!("device scan failed (continuing idle): {e:#}");
            Vec::new()
        }
    };

    // Belt-and-braces resume detection: the system-sleep hook (`ltd resume`)
    // is the primary post-sleep re-init (immediate); this tick covers
    // add/change/remove that arrive with no sleep hook (re-verify polling
    // via GET_FEATURE, multitouch-first re-init on mismatch), plus
    // independent retry of missing/dead enhanced sessions. Each pass is
    // bounded by the existing hardware init/attach budgets.
    // Start empty so failed hardware init remains pending on later passes.
    let mut watch = ResumeWatch::new(&[]);
    watch.tick_with_scan(cfg, &ready_hub, initial, &needs_attach);
    let mut schedule = crate::hotplug::Schedule::new(std::time::Instant::now());
    if watch.has_pending(cfg, &needs_attach) {
        schedule.wake(std::time::Instant::now());
    }
    let mut overflow_reported = false;
    while running.load(Ordering::SeqCst) {
        std::thread::sleep(crate::hotplug::LOOP_INTERVAL);
        let now = std::time::Instant::now();
        if let Some(source) = &monitor {
            match source.drain() {
                Ok(true) => schedule.wake(now),
                Ok(false) => {}
                Err(error) if error.raw_os_error() == Some(libc::ENOBUFS) => {
                    if !overflow_reported {
                        log::warn!("hotplug: kernel event overflow; rescanning, monitor and 30 s fallback retained");
                        overflow_reported = true;
                    }
                    schedule.wake(now);
                }
                Err(error) => {
                    // A lost/overflowed stream is only a wake hint. The
                    // authoritative scan and periodic fallback remain safe.
                    log::warn!("hotplug: monitor failed: {error}; rescan now, then 30 s fallback");
                    schedule.wake(now);
                    monitor = None;
                }
            }
        }
        if let Some(work) = schedule.take_due(now, running.load(Ordering::SeqCst)) {
            match work {
                crate::hotplug::Work::Hotplug => {
                    watch.scan(cfg, &ready_hub, &needs_attach, false);
                }
                crate::hotplug::Work::Verify => {
                    watch.tick(cfg, &ready_hub, &needs_attach);
                }
            }
            if schedule.completed(
                std::time::Instant::now(),
                watch.has_pending(cfg, &needs_attach),
            ) {
                log::warn!("hotplug: fast readiness retries exhausted; pending setup/session will retry on the 30 s pass");
            }
        }
    }
    log::info!("shutdown complete");
    Ok(())
}

/// Classify a devnode against a scanned device list. Fails closed: an
/// unrecognized node is rejected, never defaulted to USB — a stray BT node
/// given the USB feature-report dance would just collect NAKs, and a
/// non-trackpad node must not receive init reports at all.
pub fn classify_node(devnode: &str, devs: &[DiscoveredDevice]) -> Result<Transport> {
    devs.iter()
        .find(|d| d.devnode == devnode)
        .map(|d| d.transport)
        .with_context(|| format!("device node '{devnode}' is not a recognized trackpad"))
}

/// `reconnect` mode decision: pure, unit-tested.
///
/// Semantics (udev entry):
/// - `-d`/`--disengage` always wins → host-click off, no init.
/// - Daemon socket absent without `-f` → disengage mode (safe default: never
///   init behind a dead daemon; udev rule relies on this).
/// - Daemon socket absent with `-f` → init anyway (forced bring-up).
/// - Daemon live (with or without `-f`) → init, unless `-d` was given.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReconnectMode {
    Init,
    Disengage,
}

pub fn resolve_reconnect_mode(daemon_live: bool, force: bool, disengage: bool) -> ReconnectMode {
    if disengage {
        return ReconnectMode::Disengage;
    }
    if !daemon_live && !force {
        return ReconnectMode::Disengage;
    }
    ReconnectMode::Init
}

/// Validate a polling request for the USB arm: range, device floor
/// (`MIN_EXPRESSIBLE_HZ`), cap warning. Kept out of the disengage/BT arms
/// on purpose — both discard the rate, so rejecting there would break
/// valid hotplug events. Pure so the USB-only gating is unit-tested
/// without hardware.
pub fn validate_polling_rate(polling_hz: u32) -> Result<()> {
    if polling_hz == 0 || polling_hz > u16::MAX as u32 {
        anyhow::bail!("invalid polling rate {polling_hz} (want 1..=65535)");
    }
    if polling_hz < crate::init::MIN_EXPRESSIBLE_HZ {
        anyhow::bail!(
            "polling rate {polling_hz} Hz below device floor (~{} Hz)",
            crate::init::MIN_EXPRESSIBLE_HZ
        );
    }
    if polling_hz > EFFECTIVE_CAP_HZ as u32 {
        log::warn!("requested {polling_hz} Hz above device cap; clamped to ~110 Hz (X=0x23)");
    }
    Ok(())
}

/// `reconnect` entry. Idempotent; safe to re-run on every udev `add`.
/// Missing node / missing hardware → `Err` (caller logs + exits nonzero),
/// never a panic.
///
/// Order: defer (wait for the node) first, then scan + fail-closed
/// classify — classifying before the node appears would reject live
/// hardware on a stale scan.
pub fn run_reconnect(
    devnode: &str,
    force: bool,
    polling_override: Option<u32>,
    disengage: bool,
    cfg: &Config,
) -> Result<()> {
    if devnode.is_empty() {
        anyhow::bail!("reconnect requires a hidraw device path");
    }

    let sock = lock::socket_path();
    let daemon_live = lock::daemon_socket_present(&sock);
    let mode = resolve_reconnect_mode(daemon_live, force, disengage);
    if disengage && daemon_live {
        log::info!("disengage requested while daemon live; applying host-click off");
    } else if !daemon_live && !force {
        log::info!(
            "daemon socket absent ({sock}); proceeding in disengage mode (use -f to init anyway)"
        );
    } else if !daemon_live {
        log::info!("daemon socket absent but -f given; proceeding with init");
    }

    // Defer first: the node may lag udev `add` (probe/bind race).
    if !discovery::wait_for_node(devnode, 2000) {
        anyhow::bail!("device node '{devnode}' not present");
    }
    let devs = discovery::scan_trackpads().context("scanning hidraw for reconnect")?;
    let transport = classify_node(devnode, &devs)?;
    reconnect_classified_node(devnode, transport, mode, polling_override, cfg)
}

/// Hardware setup after discovery has validated the reconnect target.
/// A one-shot owns no proxy or live hub; it must leave native BT input alone
/// in every mode. Only the daemon can acquire a reversible evdev grab.
fn reconnect_classified_node(
    devnode: &str,
    transport: Transport,
    mode: ReconnectMode,
    polling_override: Option<u32>,
    cfg: &Config,
) -> Result<()> {
    if mode == ReconnectMode::Disengage {
        match transport {
            Transport::Bluetooth => {
                // No BT host-click report or driver binding changes.
                log::info!("BT {devnode}: disengage (native input retained)");
                return Ok(());
            }
            Transport::Usb => {
                let sender = HidrawSender {
                    devnode: devnode.to_string(),
                };
                init::disengage_usb_with_retry(&sender)?;
                return Ok(());
            }
        }
    }

    match transport {
        Transport::Bluetooth => {
            // No BT polling-rate equivalent (USB-first scope): -p/ config rate
            // is intentionally ignored here, never placed on the wire.
            if polling_override.is_some() {
                log::warn!(
                    "BT {devnode}: skipping polling rate (unsupported on Bluetooth; USB-first)"
                );
            }
            init::init_bluetooth(devnode);
            Ok(())
        }
        Transport::Usb => {
            // Rate validation lives here only: disengage + BT discard the
            // rate, so they must not reject it.
            let polling_hz = polling_override.unwrap_or(cfg.device.polling_rate);
            validate_polling_rate(polling_hz)?;
            // Node is present (deferred above); still check the driver bind —
            // retrying writes + read-back verify cover a lagging bind.
            let (_, bound) = discovery::wait_for_ready(devnode, 2000);
            if !bound {
                log::debug!(
                    "node {devnode} present, driver bind not yet visible; retrying writes will cover it"
                );
            }
            let sender = HidrawSender {
                devnode: devnode.to_string(),
            };
            let verifier = init::HidrawVerifier {
                devnode: devnode.to_string(),
            };
            let iface = discovery::interface_number_for_node(devnode);
            let outcome = init::init_usb_verified(&sender, &verifier, polling_hz, iface)?;
            log::info!(
                "USB init on {devnode} accepted (iface {:?}, polling [99 F2 {:02X}])",
                outcome.iface,
                outcome.plan.x,
            );
            Ok(())
        }
    }
}

/// `resume` entry (system-sleep hook + resume unit + manual re-init).
/// System-side only: re-verifies every discovered node and re-runs the USB
/// sequence from multitouch enable on mismatch. Unlike `reconnect`, this
/// always inits — it is the post-sleep authority and must work whether or
/// not the daemon socket is present (the daemon may not have survived
/// suspend). Per-node failures are collected in the report, never fatal to
/// siblings; only a failed scan is an `Err`.
pub fn run_resume(cfg: &Config, polling_override: Option<u32>) -> Result<ResumeReport> {
    let polling_hz = polling_override.unwrap_or(cfg.device.polling_rate);
    validate_polling_rate(polling_hz)?;
    let sock = lock::socket_path();
    if lock::daemon_socket_present(&sock) {
        log::debug!("resume: daemon socket present ({sock}); re-init covers live daemon too");
    } else {
        log::info!("resume: daemon socket absent ({sock}); re-initing hardware directly");
    }
    // Journal trace proving the hook/one-shot ran (hook failures are
    // swallowed to exit 0, so this line is the post-sleep evidence).
    log::info!("resume: hook/one-shot pass starting (polling {polling_hz} Hz)");
    // The hook can fire before the node re-enumerates: rescan bounded
    // instead of recording a silent empty no-op. Anything slower is the
    // daemon tick's job (≤ RESUME_REVERIFY_INTERVAL_SECS).
    let devs = resume::scan_with_rescan(
        &|| discovery::scan_trackpads().unwrap_or_default(),
        resume::RESUME_RESCAN_TIMEOUT_MS,
        resume::RESUME_RESCAN_INTERVAL_MS,
    );
    if devs.is_empty() {
        log::warn!(
            "resume: no supported trackpads found after rescan; nothing to re-init; {}",
            resume::empty_scan_recovery_hint()
        );
    }
    Ok(resume::run_resume_scan(&devs, polling_hz))
}

/// One daemon tick action, for logs/tests.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TickAction {
    /// Node appeared since the last pass → full init (add path).
    Initialized(String),
    /// Known USB node drifted → multitouch-first re-init (change path).
    Reinitialized(String),
    /// Known USB node still armed; nothing sent.
    UpToDate(String),
    /// Known USB node unreadable several passes running; assuming armed,
    /// nothing sent (reprobe every [`resume::UNREADABLE_RETRY_EVERY_TICKS`]).
    AssumedArmed(String),
    /// Hardware is initialized but the enhanced session needs attachment.
    AttachmentRetried(String),
    /// Node vanished or was replaced → `Detached` emitted (remove path).
    Detached(String),
}

/// Daemon-side resume/add/change/remove watch. Owns the last-seen device
/// list plus per-node unreadable streaks; each [`ResumeWatch::tick`] diffs a
/// fresh scan against it:
///
/// - added/replaced node or previously failed init → full
///   [`init_node`] (same ordering as boot init);
/// - still-present known USB node → one `GET_FEATURE` re-verify, threaded
///   into [`resume::resume_usb_node`] (no double read); multitouch-first
///   re-init on mismatch, [`resume::needs_reinit`] as the decision;
/// - still-present known USB node with a consecutively-unreadable verify →
///   re-init once (assume transient), then assume armed until the periodic
///   reprobe ([`resume::unreadable_disposition`]) — firmware that NAKs reads
///   must not cause a re-init + `Ready` storm every pass;
/// - still-present known BT node → no hardware init or binding changes;
/// - vanished/replaced instance → `Detached` emit so clicks releases it;
/// - initialized hardware with a missing/dead enhanced session → bounded
///   attachment-only `Ready`, independent of USB verify or unchanged BT
///   hardware.
///
/// Healthy sessions and siblings do not receive retry events.
///
/// Removal `Detached`s emit before the scan loop so a same-window
/// renumber self-heals (stale session torn down, then the new node
/// adopted); per-node init emits happen after each node's init step —
/// clicks re-attaches without reordering init.
pub struct ResumeWatch {
    known: Vec<NodeState>,
}

struct NodeState {
    dev: DiscoveredDevice,
    unreadable_streak: u32,
    // A required setup failure invalidates historical success. Pending
    // setup must complete the full sequence before attachment is allowed.
    initialized: bool,
    identity: Option<NodeIdentity>,
}

/// devtmpfs instance identity, not just the reusable hidraw path/rdev.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct NodeIdentity {
    filesystem: u64,
    inode: u64,
    device: u64,
}

fn node_identity(devnode: &str) -> Option<NodeIdentity> {
    use std::os::unix::fs::MetadataExt;
    let meta = std::fs::metadata(devnode).ok()?;
    Some(NodeIdentity {
        filesystem: meta.dev(),
        inode: meta.ino(),
        device: meta.rdev(),
    })
}

// Small I/O seam: lifecycle regressions inject hardware outcomes without
// opening trackpads, grabbing evdev or creating virtual devices.
trait WatchHardware {
    fn identity(&self, devnode: &str) -> Option<NodeIdentity> {
        node_identity(devnode)
    }
    fn initialize(&self, dev: &DiscoveredDevice) -> Result<()>;
    fn verify(&self, devnode: &str) -> Result<bool>;
    fn resume(&self, devnode: &str, verify: &Result<bool>) -> Result<resume::ResumeAction>;
}

struct LiveWatchHardware<'a> {
    cfg: &'a Config,
    hub: &'a DeviceReadyHub,
}

impl WatchHardware for LiveWatchHardware<'_> {
    fn initialize(&self, dev: &DiscoveredDevice) -> Result<()> {
        init_node(dev, self.cfg.device.polling_rate, self.hub)?;
        Ok(())
    }
    fn verify(&self, devnode: &str) -> Result<bool> {
        resume::verify_node_polling(devnode, self.cfg.device.polling_rate)
    }
    fn resume(&self, devnode: &str, verify: &Result<bool>) -> Result<resume::ResumeAction> {
        resume::resume_usb_node(
            devnode,
            self.cfg.device.polling_rate,
            Some(self.hub),
            verify,
        )
    }
}

impl ResumeWatch {
    /// Seed with successfully initialized devices only. Production starts
    /// empty and processes its boot scan through the same retry path.
    pub fn new(initial: &[DiscoveredDevice]) -> Self {
        Self {
            known: initial
                .iter()
                .map(|dev| NodeState {
                    dev: dev.clone(),
                    unreadable_streak: 0,
                    initialized: true,
                    identity: node_identity(&dev.devnode),
                })
                .collect(),
        }
    }

    pub fn tick(
        &mut self,
        cfg: &Config,
        ready_hub: &DeviceReadyHub,
        needs_attach: &dyn Fn(&str) -> bool,
    ) -> Vec<TickAction> {
        self.scan(cfg, ready_hub, needs_attach, true)
    }

    fn has_pending(&self, cfg: &Config, needs_attach: &dyn Fn(&str) -> bool) -> bool {
        self.known.iter().any(|state| {
            !state.initialized || (cfg.enhanced_clicks && needs_attach(&state.dev.devnode))
        })
    }

    fn scan(
        &mut self,
        cfg: &Config,
        ready_hub: &DeviceReadyHub,
        needs_attach: &dyn Fn(&str) -> bool,
        verify_hardware: bool,
    ) -> Vec<TickAction> {
        let current = match discovery::scan_trackpads() {
            Ok(devs) => devs,
            Err(e) => {
                log::warn!("resume tick: scan failed, keeping last-seen list: {e:#}");
                return Vec::new();
            }
        };
        self.reconcile_with_hardware(
            cfg,
            ready_hub,
            current,
            needs_attach,
            &LiveWatchHardware {
                cfg,
                hub: ready_hub,
            },
            verify_hardware,
        )
    }

    /// One pass over an injected scan (production passes the live scan
    /// from [`ResumeWatch::tick`]; tests inject renumbers/replugs).
    fn tick_with_scan(
        &mut self,
        cfg: &Config,
        ready_hub: &DeviceReadyHub,
        current: Vec<DiscoveredDevice>,
        needs_attach: &dyn Fn(&str) -> bool,
    ) -> Vec<TickAction> {
        self.tick_with_hardware(
            cfg,
            ready_hub,
            current,
            needs_attach,
            &LiveWatchHardware {
                cfg,
                hub: ready_hub,
            },
        )
    }

    fn tick_with_hardware(
        &mut self,
        cfg: &Config,
        ready_hub: &DeviceReadyHub,
        current: Vec<DiscoveredDevice>,
        needs_attach: &dyn Fn(&str) -> bool,
        hardware: &dyn WatchHardware,
    ) -> Vec<TickAction> {
        self.reconcile_with_hardware(cfg, ready_hub, current, needs_attach, hardware, true)
    }

    fn reconcile_with_hardware(
        &mut self,
        cfg: &Config,
        ready_hub: &DeviceReadyHub,
        current: Vec<DiscoveredDevice>,
        needs_attach: &dyn Fn(&str) -> bool,
        hardware: &dyn WatchHardware,
        verify_hardware: bool,
    ) -> Vec<TickAction> {
        let mut actions = Vec::new();
        // Nodes recorded Detached mid-pass (present in the scan but gone by
        // the wait) are dropped from the stored snapshot below — trusting
        // the fresher wait over the stale scan. They re-enter as added nodes
        // if they come back.
        let mut next: Vec<NodeState> = current
            .into_iter()
            .map(|dev| {
                let identity = hardware.identity(&dev.devnode);
                let prior = self.known.iter().find(|k| {
                    // Missing metadata alone does not prove a replacement.
                    k.dev == dev
                        && !(k.identity.is_some() && identity.is_some() && k.identity != identity)
                });
                NodeState {
                    unreadable_streak: prior.map_or(0, |k| k.unreadable_streak),
                    initialized: prior.is_some_and(|k| k.initialized),
                    identity: identity.or_else(|| prior.and_then(|k| k.identity)),
                    dev,
                }
            })
            .collect();
        // Retire old instances BEFORE any Ready, including replacements
        // reusing the same node name entirely between two scans.
        for old in &self.known {
            let survives = next.iter().any(|n| {
                n.dev == old.dev
                    && !(old.identity.is_some()
                        && n.identity.is_some()
                        && old.identity != n.identity)
            });
            if !survives {
                ready_hub.emit(&init::detached(&old.dev.devnode, old.dev.transport));
                actions.push(TickAction::Detached(old.dev.devnode.clone()));
            }
        }
        let mut detached: Vec<String> = Vec::new();
        let mut ready_sent: Vec<String> = Vec::new();
        for state in &mut next {
            let dev = &state.dev;
            if !state.initialized {
                match hardware.initialize(dev) {
                    Ok(()) => {
                        state.initialized = true;
                        state.unreadable_streak = 0;
                        // Mode-switch init can itself replace the node;
                        // remember the instance accepted by the sequence.
                        state.identity = hardware.identity(&dev.devnode).or(state.identity);
                        ready_sent.push(dev.devnode.clone());
                        actions.push(TickAction::Initialized(dev.devnode.clone()));
                    }
                    Err(e) => log::error!("resume tick: init failed for {}: {e:#}", dev.devnode),
                }
                continue;
            }
            if dev.transport != Transport::Usb
                || (!verify_hardware && (!cfg.enhanced_clicks || !needs_attach(&dev.devnode)))
            {
                // Healthy known hardware readback stays on the 30 s pass.
                // A dead USB session still verifies before reacquiring a
                // grab: polling drift may require a full rearm first.
                continue;
            }
            let verify = hardware.verify(&dev.devnode);
            if !resume::needs_reinit(&verify) {
                state.unreadable_streak = 0;
                actions.push(TickAction::UpToDate(dev.devnode.clone()));
                continue;
            }
            if verify.is_err() {
                let prior = state.unreadable_streak;
                state.unreadable_streak = prior + 1;
                match resume::unreadable_disposition(prior) {
                    resume::UnreadableDisposition::Suppress => {
                        if prior == 1 {
                            log::warn!(
                                "resume tick: {} verify unreadable again; assuming armed, suppressing re-init (reprobe every {} ticks)",
                                dev.devnode,
                                resume::UNREADABLE_RETRY_EVERY_TICKS,
                            );
                        } else {
                            log::debug!(
                                "resume tick: {} still unreadable (streak {}); assuming armed",
                                dev.devnode,
                                prior + 1,
                            );
                        }
                        actions.push(TickAction::AssumedArmed(dev.devnode.clone()));
                        continue;
                    }
                    resume::UnreadableDisposition::ReinitNow => {
                        log::warn!(
                            "resume tick: {} verify unreadable; re-initing anyway",
                            dev.devnode
                        );
                    }
                }
            } else {
                state.unreadable_streak = 0;
            }
            match hardware.resume(&dev.devnode, &verify) {
                Ok(resume::ResumeAction::Reinitialized) => {
                    state.identity = hardware.identity(&dev.devnode).or(state.identity);
                    ready_sent.push(dev.devnode.clone());
                    actions.push(TickAction::Reinitialized(dev.devnode.clone()))
                }
                Ok(resume::ResumeAction::Detached) => {
                    log::info!(
                        "resume tick: {} detached mid-pass; dropping from snapshot",
                        dev.devnode
                    );
                    detached.push(dev.devnode.clone());
                    actions.push(TickAction::Detached(dev.devnode.clone()))
                }
                Ok(resume::ResumeAction::UpToDate) => {
                    actions.push(TickAction::UpToDate(dev.devnode.clone()))
                }
                Err(e) => {
                    // Polling may match later even if multitouch enable
                    // failed. Require full setup, not attachment-only
                    // recovery or another polling-only decision.
                    state.initialized = false;
                    log::error!("resume tick: re-init failed for {}: {e:#}", dev.devnode);
                }
            }
        }
        // Reconcile sessions separately: hardware success does not imply
        // an attached proxy session. At most one Ready per node
        // per scheduled pass, including init/re-init emits. Healthy owners and
        // siblings produce no additional hub traffic; gate-off never retries.
        next.retain(|s| !detached.contains(&s.dev.devnode));
        for state in &next {
            if cfg.enhanced_clicks
                && state.initialized
                && !ready_sent.contains(&state.dev.devnode)
                && needs_attach(&state.dev.devnode)
            {
                let event = match state.dev.transport {
                    Transport::Usb => init::usb_ready(&state.dev.devnode),
                    Transport::Bluetooth => init::init_bluetooth(&state.dev.devnode),
                };
                ready_hub.emit(&event);
                actions.push(TickAction::AttachmentRetried(state.dev.devnode.clone()));
            }
        }
        self.known = next;
        actions
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::init::Recorder;

    use crate::init::ReadyKind;
    use crate::proxy::{ClickProxy, GrabDevice, LogHaptic, ProxySettings, UinputSink};
    use crate::pump::{HidrawSource, ScriptReader, ScriptStep};
    use std::cell::Cell;
    use std::sync::Mutex;

    #[derive(Default)]
    struct TestGrab {
        fail: AtomicBool,
        grabbed: AtomicBool,
    }
    impl GrabDevice for TestGrab {
        fn grab(&self) -> Result<()> {
            if self.fail.load(Ordering::SeqCst) {
                anyhow::bail!("transient grab failure");
            }
            self.grabbed.store(true, Ordering::SeqCst);
            Ok(())
        }
        fn ungrab(&self) {
            self.grabbed.store(false, Ordering::SeqCst);
        }
        fn is_grabbed(&self) -> bool {
            self.grabbed.load(Ordering::SeqCst)
        }
    }
    #[derive(Default)]
    struct TestSink {
        fail: AtomicBool,
        destroyed: AtomicBool,
    }
    impl UinputSink for TestSink {
        fn emit(&self, _: u16, _: u16, _: i32) -> Result<()> {
            anyhow::ensure!(!self.fail.load(Ordering::SeqCst), "fatal output failure");
            Ok(())
        }
        fn is_ready(&self) -> bool {
            !self.destroyed.load(Ordering::SeqCst)
        }
        fn destroy(&self) {
            self.destroyed.store(true, Ordering::SeqCst);
        }
    }
    type TestProxy = ClickProxy<TestGrab, TestSink, LogHaptic, HidrawSource<ScriptReader>>;
    struct TestSession {
        proxy: TestProxy,
        active: Option<String>,
        attempts: usize,
        events: Vec<ReadyKind>,
    }
    impl TestSession {
        fn subscribe(hub: &DeviceReadyHub, fail: bool) -> Arc<Mutex<Self>> {
            Self::subscribe_filtered(hub, fail, |_| true)
        }

        fn subscribe_filtered(
            hub: &DeviceReadyHub,
            fail: bool,
            eligible: impl Fn(&DeviceReadyEvent) -> bool + Send + Sync + 'static,
        ) -> Arc<Mutex<Self>> {
            let session = Arc::new(Mutex::new(Self {
                proxy: TestProxy::new(
                    ProxySettings { enhanced: true },
                    TestGrab::default(),
                    TestSink::default(),
                    LogHaptic,
                    HidrawSource::new("fixture", ScriptReader::default()),
                ),
                active: None,
                attempts: 0,
                events: Vec::new(),
            }));
            session
                .lock()
                .unwrap()
                .proxy
                .grab()
                .fail
                .store(fail, Ordering::SeqCst);
            let capture = Arc::clone(&session);
            hub.subscribe(move |event| {
                let mut s = capture.lock().unwrap();
                s.events.push(event.kind);
                if event.kind == ReadyKind::Detached {
                    if s.active.as_deref() == Some(&event.devnode) {
                        // `detach` runs the full teardown; no second pass needed.
                        s.proxy.detach(event);
                        s.active = None;
                    }
                    return;
                }
                if !eligible(event) {
                    return;
                }
                if !session_needs_attach(
                    true,
                    s.active.as_deref(),
                    &event.devnode,
                    s.proxy.is_attached(),
                ) {
                    return;
                }
                s.attempts += 1;
                // Disposable replacement source, mirroring the manager's
                // fresh-FD wiring after a failed grab or dead source.
                s.proxy
                    .replace_source(HidrawSource::new(&event.devnode, ScriptReader::default()));
                // The real manager recreates retired output nodes before
                // setting pump readiness and acquiring a fresh grab.
                s.proxy.uinput().destroyed.store(false, Ordering::SeqCst);
                s.proxy.set_pump_ready(true);
                s.proxy.attach(event);
                if s.proxy.is_attached() {
                    s.active = Some(event.devnode.clone());
                }
            });
            session
        }
        fn needs_attach(&self, node: &str) -> bool {
            session_needs_attach(true, self.active.as_deref(), node, self.proxy.is_attached())
        }
    }

    struct TestHardware<'a> {
        hub: &'a DeviceReadyHub,
        init_fails: Cell<bool>,
        resume_fails: Cell<bool>,
        verify: Cell<Option<bool>>,
        verifies: Cell<usize>,
        inits: Cell<usize>,
        reinits: Cell<usize>,
    }
    impl<'a> TestHardware<'a> {
        fn new(hub: &'a DeviceReadyHub) -> Self {
            Self {
                hub,
                init_fails: Cell::new(false),
                resume_fails: Cell::new(false),
                verify: Cell::new(Some(true)),
                verifies: Cell::new(0),
                inits: Cell::new(0),
                reinits: Cell::new(0),
            }
        }
        fn ready(&self, dev: &DiscoveredDevice) {
            self.hub.emit(&match dev.transport {
                Transport::Usb => init::usb_ready(&dev.devnode),
                Transport::Bluetooth => init::init_bluetooth(&dev.devnode),
            });
        }
    }
    impl WatchHardware for TestHardware<'_> {
        fn initialize(&self, dev: &DiscoveredDevice) -> Result<()> {
            self.inits.set(self.inits.get() + 1);
            if self.init_fails.get() {
                anyhow::bail!("transient init failure");
            }
            self.ready(dev);
            Ok(())
        }
        fn verify(&self, _: &str) -> Result<bool> {
            self.verifies.set(self.verifies.get() + 1);
            self.verify
                .get()
                .ok_or_else(|| anyhow::anyhow!("unreadable firmware"))
        }
        fn resume(&self, devnode: &str, _: &Result<bool>) -> Result<resume::ResumeAction> {
            self.reinits.set(self.reinits.get() + 1);
            if self.resume_fails.get() {
                anyhow::bail!("USB multitouch rearm failed");
            }
            self.hub.emit(&init::usb_ready(devnode));
            Ok(resume::ResumeAction::Reinitialized)
        }
    }

    fn enhanced_config() -> Config {
        Config {
            enhanced_clicks: true,
            ..Config::default()
        }
    }

    #[test]
    fn hotplug_after_periodic_scan_observes_short_connection_and_coalesces_duplicates() {
        use crate::hotplug::{Schedule, Work};
        use std::time::{Duration, Instant};
        let hub = hub();
        let session = TestSession::subscribe(&hub, false);
        let hardware = TestHardware::new(&hub);
        let cfg = enhanced_config();
        let mut watch = ResumeWatch::new(&[]);
        let start = Instant::now();
        let mut schedule = Schedule::new(start);
        let health = |node: &str| session.lock().unwrap().needs_attach(node);
        let dev = fixture_devs().remove(0);
        let mut passes = 0;
        let mut first_attach = None;
        let mut removal_seen = false;
        // Empty periodic scan just ran at 30 s; a connection at 30.1 s
        // must not wait for the next periodic pass at 60 s.
        for ms in (30_000..=32_000).step_by(100) {
            let now = start + Duration::from_millis(ms);
            if ms == 30_100 || ms == 30_800 || ms == 31_300 {
                for _ in 0..20 {
                    schedule.wake(now);
                } // interface burst
            }
            if let Some(work) = schedule.take_due(now, true) {
                passes += 1;
                let present = (30_100..30_800).contains(&ms) || ms >= 31_300;
                let scan = if present { vec![dev.clone()] } else { vec![] };
                let actions = watch.reconcile_with_hardware(
                    &cfg,
                    &hub,
                    scan,
                    &health,
                    &hardware,
                    work == Work::Verify,
                );
                if first_attach.is_none() && session.lock().unwrap().proxy.is_attached() {
                    first_attach = Some(ms);
                }
                removal_seen |= actions.contains(&TickAction::Detached(dev.devnode.clone()));
                schedule.completed(now, watch.has_pending(&cfg, &health));
            }
        }
        assert!(first_attach.unwrap() - 30_100 <= 1_000);
        assert!(
            removal_seen,
            "subsecond connection must be adopted and removed"
        );
        assert!(passes < 10, "duplicate notifications coalesce");
        let s = session.lock().unwrap();
        assert!(s.proxy.is_attached());
        assert_eq!(s.attempts, 2, "one grab per connection");
        assert_eq!(
            s.events,
            vec![ReadyKind::Ready, ReadyKind::Detached, ReadyKind::Ready]
        );
        assert_eq!(hardware.inits.get(), 2);
        assert_eq!(
            hardware.verifies.get(),
            0,
            "healthy duplicates need no readback"
        );
    }

    #[test]
    fn hotplug_retries_late_touch_init_and_evdev_without_adopting_early_silent_interface() {
        use crate::hotplug::{Schedule, Work};
        use std::time::{Duration, Instant};
        let hub = hub();
        let session = TestSession::subscribe_filtered(&hub, true, |ev| {
            touch_interface_eligible(Some(if ev.devnode.ends_with('0') { 0 } else { 1 }))
        });
        let hardware = TestHardware::new(&hub);
        let cfg = enhanced_config();
        let mut watch = ResumeWatch::new(&[]);
        let start = Instant::now();
        let mut schedule = Schedule::new(start);
        let touch = fixture_devs().remove(0);
        let early = DiscoveredDevice {
            devnode: "/dev/hidraw0".into(),
            ..touch.clone()
        };
        let health = |node: &str| {
            touch_interface_eligible(Some(if node.ends_with('0') { 0 } else { 1 }))
                && session.lock().unwrap().needs_attach(node)
        };
        let mut attached_at = None;
        for ms in (0..=3_000).step_by(100) {
            let now = start + Duration::from_millis(ms);
            if ms == 0 || ms == 400 {
                schedule.wake(now);
            }
            hardware.init_fails.set((400..600).contains(&ms));
            session
                .lock()
                .unwrap()
                .proxy
                .grab()
                .fail
                .store(ms < 1_000, Ordering::SeqCst);
            if let Some(work) = schedule.take_due(now, true) {
                let scan = if ms < 400 {
                    vec![early.clone()]
                } else {
                    vec![touch.clone(), early.clone()]
                };
                watch.reconcile_with_hardware(
                    &cfg,
                    &hub,
                    scan,
                    &health,
                    &hardware,
                    work == Work::Verify,
                );
                if session.lock().unwrap().proxy.is_attached() && attached_at.is_none() {
                    attached_at = Some(ms);
                }
                schedule.completed(now, watch.has_pending(&cfg, &health));
            }
            if ms < 400 {
                assert_eq!(session.lock().unwrap().attempts, 0);
            }
        }
        assert!(
            attached_at.unwrap() - 1_000 <= 1_000,
            "retry within 1 s once evdev is ready"
        );
        let s = session.lock().unwrap();
        assert_eq!(s.active.as_deref(), Some(touch.devnode.as_str()));
        assert_eq!(s.attempts, 2, "failed grab then successful attach");
        assert_eq!(
            hardware.inits.get(),
            3,
            "early node once, touch init fails then succeeds"
        );
        assert_eq!(hardware.reinits.get(), 0);
        assert_eq!(
            hardware.verifies.get(),
            1,
            "only the unhealthy touch session is verified"
        );
    }

    #[test]
    fn hotplug_dead_usb_verifies_before_attach_and_pending_rearm_cannot_be_bypassed() {
        let hub = hub();
        let session = TestSession::subscribe(&hub, false);
        let hardware = TestHardware::new(&hub);
        let cfg = enhanced_config();
        let mut watch = ResumeWatch::new(&[]);
        let dev = fixture_devs().remove(0);
        let health = |node: &str| session.lock().unwrap().needs_attach(node);
        watch.reconcile_with_hardware(&cfg, &hub, vec![dev.clone()], &health, &hardware, false);
        session.lock().unwrap().proxy.set_pump_ready(false);
        hardware.verify.set(Some(false));
        hardware.resume_fails.set(true);
        hardware.init_fails.set(true);
        watch.reconcile_with_hardware(&cfg, &hub, vec![dev.clone()], &health, &hardware, false);
        assert!(!watch.known[0].initialized);
        assert!(!session.lock().unwrap().proxy.grab().is_grabbed());
        hardware.verify.set(Some(true));
        watch.reconcile_with_hardware(&cfg, &hub, vec![dev.clone()], &health, &hardware, false);
        assert_eq!(session.lock().unwrap().attempts, 1);
        hardware.init_fails.set(false);
        watch.reconcile_with_hardware(&cfg, &hub, vec![dev], &health, &hardware, false);
        assert!(session.lock().unwrap().proxy.is_attached());
        assert_eq!(session.lock().unwrap().attempts, 2);
        assert_eq!(hardware.verifies.get(), 1);
        assert_eq!(hardware.reinits.get(), 1);
    }

    #[test]
    fn hotplug_same_name_replacement_retires_owner_before_fresh_init() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("hidraw");
        std::fs::write(&path, []).unwrap();
        let mut dev = fixture_devs().remove(0);
        dev.devnode = path.to_string_lossy().into_owned();
        let hub = hub();
        let session = TestSession::subscribe(&hub, false);
        let hardware = TestHardware::new(&hub);
        let cfg = enhanced_config();
        let mut watch = ResumeWatch::new(&[]);
        let health = |node: &str| session.lock().unwrap().needs_attach(node);
        watch.reconcile_with_hardware(&cfg, &hub, vec![dev.clone()], &health, &hardware, false);
        // Preserve the old inode so the fixture cannot reuse its identity.
        std::fs::rename(&path, dir.path().join("retired")).unwrap();
        std::fs::write(&path, []).unwrap();
        let actions =
            watch.reconcile_with_hardware(&cfg, &hub, vec![dev.clone()], &health, &hardware, false);
        assert_eq!(
            actions,
            vec![
                TickAction::Detached(dev.devnode.clone()),
                TickAction::Initialized(dev.devnode)
            ]
        );
        let s = session.lock().unwrap();
        assert!(s.proxy.is_attached());
        assert_eq!(s.attempts, 2);
        assert_eq!(
            s.events,
            vec![ReadyKind::Ready, ReadyKind::Detached, ReadyKind::Ready]
        );
        assert_eq!(hardware.inits.get(), 2);
        assert_eq!(hardware.verifies.get(), 0);
    }

    #[test]
    fn hotplug_gate_off_skips_session_work_and_known_hardware_reads() {
        let hub = hub();
        let hardware = TestHardware::new(&hub);
        let cfg = Config {
            enhanced_clicks: false,
            ..Config::default()
        };
        let devs = fixture_devs();
        let mut watch = ResumeWatch::new(&devs);
        let actions = watch.reconcile_with_hardware(
            &cfg,
            &hub,
            devs,
            &|_| panic!("gate off"),
            &hardware,
            false,
        );
        assert!(actions.is_empty());
        assert_eq!(hardware.inits.get(), 0);
        assert_eq!(hardware.verifies.get(), 0);
    }

    #[test]
    fn failed_attachment_recovers_with_unchanged_usb_or_bt_hardware() {
        for dev in fixture_devs() {
            let hub = hub();
            let session = TestSession::subscribe(&hub, true);
            let hardware = TestHardware::new(&hub);
            let cfg = enhanced_config();
            let mut watch = ResumeWatch::new(&[]);
            let health = |node: &str| session.lock().unwrap().needs_attach(node);
            // Boot init succeeds, but the physical grab fails. No second
            // attempt in the same pass; the scan name never changes.
            watch.tick_with_hardware(&cfg, &hub, vec![dev.clone()], &health, &hardware);
            assert_eq!(session.lock().unwrap().attempts, 1);
            assert!(session.lock().unwrap().proxy.is_passthrough());
            session
                .lock()
                .unwrap()
                .proxy
                .grab()
                .fail
                .store(false, Ordering::SeqCst);
            let actions =
                watch.tick_with_hardware(&cfg, &hub, vec![dev.clone()], &health, &hardware);
            assert!(actions.contains(&TickAction::AttachmentRetried(dev.devnode.clone())));
            assert!(session.lock().unwrap().proxy.is_attached());
            assert_eq!(session.lock().unwrap().attempts, 2);
            // Healthy sessions stay quiet for subsequent passes.
            for _ in 0..5 {
                watch.tick_with_hardware(&cfg, &hub, vec![dev.clone()], &health, &hardware);
            }
            assert_eq!(session.lock().unwrap().attempts, 2);
            assert_eq!(session.lock().unwrap().events.len(), 2);
            assert_eq!(hardware.inits.get(), 1);
            assert_eq!(hardware.reinits.get(), 0);
        }
    }

    #[test]
    fn failed_initial_hardware_init_remains_pending_until_success() {
        let hub = hub();
        let session = TestSession::subscribe(&hub, false);
        let hardware = TestHardware::new(&hub);
        hardware.init_fails.set(true);
        let cfg = enhanced_config();
        let mut watch = ResumeWatch::new(&[]);
        let dev = fixture_devs().remove(0);
        let health = |node: &str| session.lock().unwrap().needs_attach(node);
        for _ in 0..3 {
            watch.tick_with_hardware(&cfg, &hub, vec![dev.clone()], &health, &hardware);
        }
        assert_eq!(hardware.inits.get(), 3, "one init attempt per pass");
        assert!(session.lock().unwrap().events.is_empty());
        hardware.init_fails.set(false);
        watch.tick_with_hardware(&cfg, &hub, vec![dev], &health, &hardware);
        assert!(session.lock().unwrap().proxy.is_attached());
        assert_eq!(hardware.inits.get(), 4);
    }

    #[test]
    fn unchanged_scan_recovers_a_self_released_source_session() {
        for dev in fixture_devs() {
            let hub = hub();
            let session = TestSession::subscribe(&hub, false);
            let hardware = TestHardware::new(&hub);
            let cfg = enhanced_config();
            let mut watch = ResumeWatch::new(&[]);
            let health = |node: &str| session.lock().unwrap().needs_attach(node);
            watch.tick_with_hardware(&cfg, &hub, vec![dev.clone()], &health, &hardware);
            {
                let mut s = session.lock().unwrap();
                s.proxy.replace_source(HidrawSource::new(
                    &dev.devnode,
                    ScriptReader::new(vec![ScriptStep::IoError(libc::ENODEV)]),
                ));
                s.proxy.handle_frame(&cfg).unwrap();
                assert!(!s.proxy.is_attached());
                assert!(!s.proxy.grab().is_grabbed());
                assert_eq!(s.active.as_deref(), Some(dev.devnode.as_str()));
            }
            watch.tick_with_hardware(&cfg, &hub, vec![dev], &health, &hardware);
            assert!(session.lock().unwrap().proxy.is_attached());
            assert_eq!(session.lock().unwrap().attempts, 2);
            assert_eq!(hardware.inits.get(), 1);
            assert_eq!(hardware.reinits.get(), 0);
        }
    }

    #[test]
    fn unchanged_hardware_recovers_after_fatal_output_cleanup() {
        for dev in fixture_devs() {
            let hub = hub();
            let session = TestSession::subscribe(&hub, false);
            let hardware = TestHardware::new(&hub);
            let cfg = enhanced_config();
            let mut watch = ResumeWatch::new(&[]);
            let health = |node: &str| session.lock().unwrap().needs_attach(node);
            watch.tick_with_hardware(&cfg, &hub, vec![dev.clone()], &health, &hardware);
            let (report_id, prefix_len) = match dev.transport {
                Transport::Usb => (0x02, 12),
                Transport::Bluetooth => (0x31, 4),
            };
            let mut report = vec![0; prefix_len];
            report[0] = report_id;
            {
                let mut s = session.lock().unwrap();
                s.proxy.uinput().fail.store(true, Ordering::SeqCst);
                // An empty decoded report still emits the touch/tool/SYN
                // frame, exercising the fallible sink and fatal cleanup.
                s.proxy.replace_source(HidrawSource::new(
                    &dev.devnode,
                    ScriptReader::new(vec![ScriptStep::Bytes(report.clone())]),
                ));
                assert!(s.proxy.handle_frame(&cfg).is_err());
                assert!(!s.proxy.is_attached());
                assert!(!s.proxy.grab().is_grabbed());
                assert!(s.proxy.uinput().destroyed.load(Ordering::SeqCst));
                assert_eq!(s.active.as_deref(), Some(dev.devnode.as_str()));
                s.proxy.uinput().fail.store(false, Ordering::SeqCst);
            }
            let actions =
                watch.tick_with_hardware(&cfg, &hub, vec![dev.clone()], &health, &hardware);
            assert!(actions.contains(&TickAction::AttachmentRetried(dev.devnode.clone())));
            {
                let mut s = session.lock().unwrap();
                assert!(s.proxy.is_attached());
                assert!(s.proxy.grab().is_grabbed());
                assert!(!s.proxy.uinput().destroyed.load(Ordering::SeqCst));
                assert_eq!(s.attempts, 2);
                s.proxy.replace_source(HidrawSource::new(
                    &dev.devnode,
                    ScriptReader::new(vec![ScriptStep::Bytes(report)]),
                ));
                s.proxy.handle_frame(&cfg).unwrap();
                assert!(s.proxy.is_attached());
            }
            // Recovery does not re-init hardware or repeat attachment
            // once the recreated output session is healthy.
            watch.tick_with_hardware(&cfg, &hub, vec![dev], &health, &hardware);
            assert_eq!(hardware.inits.get(), 1);
            assert_eq!(hardware.reinits.get(), 0);
            assert_eq!(session.lock().unwrap().attempts, 2);
        }
    }

    // Lose an initialized session, require USB rearm, then fail that
    // required setup. Neither failure path may reacquire the grab.
    fn failed_usb_rearm_requires_full_initialization(output_loss: bool) {
        let hub = hub();
        let session = TestSession::subscribe(&hub, false);
        let hardware = TestHardware::new(&hub);
        let cfg = enhanced_config();
        let mut watch = ResumeWatch::new(&[]);
        let dev = fixture_devs().remove(0);
        assert_eq!(dev.transport, Transport::Usb);
        let health = |node: &str| session.lock().unwrap().needs_attach(node);
        watch.tick_with_hardware(&cfg, &hub, vec![dev.clone()], &health, &hardware);
        {
            let mut s = session.lock().unwrap();
            if output_loss {
                s.proxy.uinput().fail.store(true, Ordering::SeqCst);
                let mut report = vec![0; 12];
                report[0] = 0x02;
                s.proxy.replace_source(HidrawSource::new(
                    &dev.devnode,
                    ScriptReader::new(vec![ScriptStep::Bytes(report)]),
                ));
                assert!(s.proxy.handle_frame(&cfg).is_err());
                assert!(s.proxy.uinput().destroyed.load(Ordering::SeqCst));
                s.proxy.uinput().fail.store(false, Ordering::SeqCst);
            } else {
                s.proxy.replace_source(HidrawSource::new(
                    &dev.devnode,
                    ScriptReader::new(vec![ScriptStep::IoError(libc::ENODEV)]),
                ));
                s.proxy.handle_frame(&cfg).unwrap();
            }
            assert!(!s.proxy.is_attached());
            assert!(!s.proxy.grab().is_grabbed());
        }
        hardware.verify.set(Some(false));
        hardware.resume_fails.set(true);
        hardware.init_fails.set(true);
        let actions = watch.tick_with_hardware(&cfg, &hub, vec![dev.clone()], &health, &hardware);
        {
            let s = session.lock().unwrap();
            assert!(!s.proxy.is_attached(),
                "failed USB rearm reattached: actions={actions:?}, events={:?}, attempts={}, grabbed={}",
                s.events, s.attempts, s.proxy.grab().is_grabbed());
            assert!(!s.proxy.grab().is_grabbed());
            assert_eq!(s.attempts, 1);
            assert_eq!(s.events, vec![ReadyKind::Ready]);
        }
        assert!(!watch.known[0].initialized);
        assert_eq!(hardware.reinits.get(), 1);
        assert_eq!(hardware.verifies.get(), 1);

        // Matching polling alone cannot prove the multitouch-enable step
        // succeeded. Persistent setup failures must retry the full sequence
        // once per pass, without Ready events, grabs or verification bypass.
        hardware.verify.set(Some(true));
        for pass in 1..=3 {
            assert!(watch
                .tick_with_hardware(&cfg, &hub, vec![dev.clone()], &health, &hardware)
                .is_empty());
            assert_eq!(hardware.inits.get(), 1 + pass);
            assert_eq!(hardware.reinits.get(), 1);
            assert_eq!(hardware.verifies.get(), 1);
            let mut s = session.lock().unwrap();
            let mut short_report = vec![0; 8];
            short_report[0] = 0x02;
            s.proxy.replace_source(HidrawSource::new(
                &dev.devnode,
                ScriptReader::new(vec![ScriptStep::Bytes(short_report)]),
            ));
            assert!(s.proxy.handle_frame(&cfg).unwrap().is_none());
            assert!(!s.proxy.is_attached());
            assert!(!s.proxy.grab().is_grabbed());
            assert!(s.proxy.is_passthrough());
            assert_eq!(s.attempts, 1);
            assert_eq!(s.events, vec![ReadyKind::Ready]);
        }
        hardware.init_fails.set(false);
        let actions = watch.tick_with_hardware(&cfg, &hub, vec![dev.clone()], &health, &hardware);
        assert_eq!(actions, vec![TickAction::Initialized(dev.devnode.clone())]);
        assert!(watch.known[0].initialized);
        {
            let s = session.lock().unwrap();
            assert!(s.proxy.is_attached());
            assert!(s.proxy.grab().is_grabbed());
            assert_eq!(s.attempts, 2);
            assert_eq!(s.events, vec![ReadyKind::Ready, ReadyKind::Ready]);
        }
        // Successful full setup restores ordinary quiet polling/health
        // reconciliation; historical failure cannot trigger extra init.
        let actions = watch.tick_with_hardware(&cfg, &hub, vec![dev.clone()], &health, &hardware);
        assert_eq!(actions, vec![TickAction::UpToDate(dev.devnode)]);
        assert_eq!(hardware.inits.get(), 5);
        assert_eq!(hardware.reinits.get(), 1);
        assert_eq!(hardware.verifies.get(), 2);
        assert_eq!(session.lock().unwrap().attempts, 2);
    }

    #[test]
    fn failed_usb_rearm_after_source_loss_blocks_attachment_until_full_setup() {
        failed_usb_rearm_requires_full_initialization(false);
    }

    #[test]
    fn failed_usb_rearm_after_fatal_output_blocks_attachment_until_full_setup() {
        failed_usb_rearm_requires_full_initialization(true);
    }

    #[test]
    fn same_name_replacement_detaches_before_init_even_if_proxy_still_looks_live() {
        let fixture = tempfile::tempdir().unwrap();
        let path = fixture.path().join("hidraw");
        std::fs::write(&path, []).unwrap();
        let mut dev = fixture_devs().remove(0);
        dev.devnode = path.to_string_lossy().into_owned();
        let hub = hub();
        let session = TestSession::subscribe(&hub, false);
        let hardware = TestHardware::new(&hub);
        let cfg = enhanced_config();
        let mut watch = ResumeWatch::new(&[]);
        let health = |node: &str| session.lock().unwrap().needs_attach(node);
        watch.tick_with_hardware(&cfg, &hub, vec![dev.clone()], &health, &hardware);
        let first = node_identity(&dev.devnode).unwrap();
        let steady = watch.tick_with_hardware(&cfg, &hub, vec![dev.clone()], &health, &hardware);
        assert_eq!(steady, vec![TickAction::UpToDate(dev.devnode.clone())]);
        assert_eq!(session.lock().unwrap().attempts, 1);
        assert_eq!(hardware.inits.get(), 1);
        // Keep the old inode alive under a different name so the fixture
        // cannot accidentally reuse it; the scan never sees an absence.
        std::fs::rename(&path, fixture.path().join("retired")).unwrap();
        std::fs::write(&path, []).unwrap();
        assert_ne!(Some(first), node_identity(&dev.devnode));
        let actions = watch.tick_with_hardware(&cfg, &hub, vec![dev.clone()], &health, &hardware);
        assert_eq!(
            actions,
            vec![
                TickAction::Detached(dev.devnode.clone()),
                TickAction::Initialized(dev.devnode)
            ]
        );
        let s = session.lock().unwrap();
        assert_eq!(
            s.events,
            vec![ReadyKind::Ready, ReadyKind::Detached, ReadyKind::Ready]
        );
        assert!(s.proxy.is_attached());
        assert_eq!(hardware.inits.get(), 2);
    }

    #[test]
    fn persistent_attachment_failures_are_limited_to_one_attempt_per_pass() {
        let hub = hub();
        let session = TestSession::subscribe(&hub, true);
        let hardware = TestHardware::new(&hub);
        // Exercise unreadable cooldown too: suppressed hardware init must
        // not suppress independent session recovery.
        hardware.verify.set(None);
        let cfg = enhanced_config();
        let mut watch = ResumeWatch::new(&[]);
        let dev = fixture_devs().remove(0);
        let health = |node: &str| session.lock().unwrap().needs_attach(node);
        for pass in 1..=6 {
            watch.tick_with_hardware(&cfg, &hub, vec![dev.clone()], &health, &hardware);
            assert_eq!(session.lock().unwrap().attempts, pass);
        }
        assert_eq!(hardware.inits.get(), 1);
        assert_eq!(hardware.reinits.get(), 1);
        assert!(session.lock().unwrap().proxy.is_passthrough());
    }

    #[test]
    fn gate_off_and_owned_siblings_do_not_receive_attachment_retries() {
        let hub = hub();
        let hardware = TestHardware::new(&hub);
        let devs = fixture_devs();
        let mut watch = ResumeWatch::new(&devs);
        let events = Arc::new(Mutex::new(Vec::new()));
        let capture = Arc::clone(&events);
        hub.subscribe(move |ev| capture.lock().unwrap().push(ev.clone()));
        let health_queries = Cell::new(0);
        // Gate-off must not request session work, or emit Ready.
        let disabled = Config {
            enhanced_clicks: false,
            ..Config::default()
        };
        watch.tick_with_hardware(
            &disabled,
            &hub,
            devs.clone(),
            &|_| {
                health_queries.set(health_queries.get() + 1);
                true
            },
            &hardware,
        );
        assert_eq!(health_queries.get(), 0);
        assert!(events.lock().unwrap().is_empty());
        assert_eq!(hardware.inits.get(), 0);
        assert_eq!(hardware.reinits.get(), 0);
        for live in [false, true] {
            let needs_attach =
                |node: &str| session_needs_attach(true, Some(&devs[0].devnode), node, live);
            assert!(!needs_attach(&devs[1].devnode));
        }
        // Healthy owner blocks duplicates for itself and all siblings.
        watch.tick_with_hardware(
            &enhanced_config(),
            &hub,
            devs.clone(),
            &|node| session_needs_attach(true, Some(&devs[0].devnode), node, true),
            &hardware,
        );
        assert!(events.lock().unwrap().is_empty());
    }

    #[test]
    fn reconnect_rejects_empty_node() {
        let cfg = Config::default();
        assert!(run_reconnect("", false, None, false, &cfg).is_err());
    }

    #[test]
    fn resume_rejects_bad_polling_without_touching_hardware() {
        let cfg = Config::default();
        assert!(run_resume(&cfg, Some(0)).is_err());
        assert!(run_resume(&cfg, Some(70_000)).is_err());
    }

    #[test]
    fn resume_tick_emits_detached_for_vanished_node() {
        // Watch thinks an impossible node exists; the real scan can never
        // contain it → vanish path → Detached on the hub, never Ready.
        let hub = DeviceReadyHub::new();
        let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let capture = std::sync::Arc::clone(&seen);
        hub.subscribe(move |ev| capture.lock().expect("seen lock").push(ev.clone()));
        let mut watch = ResumeWatch::new(&[DiscoveredDevice {
            devnode: "/dev/hidraw-nope".into(),
            vendor_id: 0x05ac,
            product_id: 0x0324,
            transport: Transport::Usb,
        }]);
        let cfg = Config::default();
        let actions = watch.tick(&cfg, &hub, &|_| false);
        assert_eq!(
            actions,
            vec![TickAction::Detached("/dev/hidraw-nope".into())]
        );
        let seen = seen.lock().expect("seen lock");
        assert_eq!(seen.len(), 1);
        assert_eq!(seen[0].kind, crate::init::ReadyKind::Detached);
        // Second tick: steady state, no repeat traffic.
        let actions = watch.tick(&cfg, &hub, &|_| false);
        assert!(actions.is_empty());
        assert_eq!(seen.len(), 1);
    }

    #[test]
    fn renumber_inside_one_tick_heals_on_new_node() {
        // Same-window renumber hidraw8→hidraw12: removals must emit
        // BEFORE the scan loop adopts the addition, so the new node's
        // Ready is live and the stale Detached fires exactly once.
        let hub = DeviceReadyHub::new();
        let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let capture = std::sync::Arc::clone(&seen);
        hub.subscribe(move |ev| capture.lock().expect("seen lock").push(ev.clone()));
        let mut watch = ResumeWatch::new(&[DiscoveredDevice {
            devnode: "/dev/hidraw8".into(),
            vendor_id: 0x05ac,
            product_id: 0x0324,
            transport: Transport::Usb,
        }]);
        let cfg = Config::default();
        let current = vec![DiscoveredDevice {
            devnode: "/dev/hidraw12".into(),
            vendor_id: 0x004c,
            product_id: 0x0324,
            transport: Transport::Bluetooth,
        }];
        let actions = watch.tick_with_scan(&cfg, &hub, current, &|_| false);
        // Removal first, then adoption — the order that heals.
        assert_eq!(
            actions,
            vec![
                TickAction::Detached("/dev/hidraw8".into()),
                TickAction::Initialized("/dev/hidraw12".into()),
            ]
        );
        let seen = seen.lock().expect("seen lock");
        assert_eq!(seen.len(), 2);
        assert_eq!(seen[0].devnode, "/dev/hidraw8");
        assert_eq!(seen[0].kind, crate::init::ReadyKind::Detached);
        assert_eq!(seen[1].devnode, "/dev/hidraw12");
        assert_eq!(seen[1].kind, crate::init::ReadyKind::Ready);
        // Snapshot rebuilt on the new node: next pass is steady state.
        let actions = watch.tick_with_scan(
            &cfg,
            &hub,
            vec![DiscoveredDevice {
                devnode: "/dev/hidraw12".into(),
                vendor_id: 0x004c,
                product_id: 0x0324,
                transport: Transport::Bluetooth,
            }],
            &|_| false,
        );
        assert!(actions.is_empty());
        assert_eq!(seen.len(), 2, "stale Detached must fire exactly once");
    }

    #[test]
    fn attach_decision_matrix() {
        use AttachDecision::*;
        // Fresh: adopt.
        assert_eq!(attach_decision(None, "/dev/hidraw8", false), Adopt);
        assert_eq!(attach_decision(None, "/dev/hidraw8", true), Adopt);
        // Live duplicate: ignore, never rebind a healthy session.
        assert_eq!(
            attach_decision(Some("/dev/hidraw8"), "/dev/hidraw8", true),
            IgnoreDuplicate
        );
        // Same name, dead session (disconnect self-release): teardown +
        // re-attach. Early-return here would brick the node forever: no
        // further Ready is owed for an already-known devnode.
        assert_eq!(
            attach_decision(Some("/dev/hidraw8"), "/dev/hidraw8", false),
            Reattach
        );
        // Sibling while another owns the pump: ignore either way.
        assert_eq!(
            attach_decision(Some("/dev/hidraw8"), "/dev/hidraw10", false),
            IgnoreSibling
        );
        assert_eq!(
            attach_decision(Some("/dev/hidraw8"), "/dev/hidraw10", true),
            IgnoreSibling
        );
    }

    #[test]
    fn reconnect_rejects_bad_polling() {
        // USB-arm validation is pure: no hardware, no 2 s node wait.
        assert!(validate_polling_rate(0).is_err());
        assert!(validate_polling_rate(70_000).is_err());
        assert!(validate_polling_rate(110).is_ok());
        // Above cap warns but still validates (clamped downstream).
        assert!(validate_polling_rate(500).is_ok());
    }

    #[test]
    fn reconnect_missing_node_errors_without_panic() {
        let cfg = Config::default();
        // No hardware in test env; unrecognized node fails closed, no panic.
        let r = run_reconnect("/nonexistent-hidraw-xyz", true, Some(110), false, &cfg);
        assert!(r.is_err());
    }

    #[test]
    fn reconnect_rejects_unrecognized_live_node() {
        let cfg = Config::default();
        // /dev/null exists but is not a trackpad: fail closed, never init-dance it.
        let r = run_reconnect("/dev/null", true, Some(110), false, &cfg);
        assert!(r.is_err());
    }

    #[test]
    fn reconnect_rejects_below_floor() {
        assert!(validate_polling_rate(1).is_err());
        assert!(validate_polling_rate(15).is_err());
        assert!(validate_polling_rate(16).is_ok());
    }

    fn fixture_devs() -> Vec<DiscoveredDevice> {
        vec![
            DiscoveredDevice {
                devnode: "/dev/hidraw1".into(),
                vendor_id: 0x05ac,
                product_id: 0x0324,
                transport: Transport::Usb,
            },
            DiscoveredDevice {
                devnode: "/dev/hidraw9".into(),
                vendor_id: 0x004c,
                product_id: 0x0324,
                transport: Transport::Bluetooth,
            },
        ]
    }

    #[test]
    fn classify_node_known_transports() {
        let devs = fixture_devs();
        assert_eq!(
            classify_node("/dev/hidraw1", &devs).unwrap(),
            Transport::Usb
        );
        assert_eq!(
            classify_node("/dev/hidraw9", &devs).unwrap(),
            Transport::Bluetooth
        );
    }

    #[test]
    fn classify_node_unknown_fails_closed() {
        let devs = fixture_devs();
        // Must err — never fall back to USB for a stray node.
        assert!(classify_node("/dev/hidraw0", &devs).is_err());
        assert!(classify_node("/dev/null", &devs).is_err());
    }

    #[test]
    fn init_node_bt_emits_ready_without_reports() {
        let hub = DeviceReadyHub::new();
        let dev = DiscoveredDevice {
            devnode: "/dev/hidraw9".into(),
            vendor_id: 0x004c,
            product_id: 0x0324,
            transport: Transport::Bluetooth,
        };
        let ev = init_node(&dev, 110, &hub).unwrap();
        assert_eq!(ev.transport, Transport::Bluetooth);
        assert_eq!(ev.kind, crate::init::ReadyKind::Ready);
    }

    #[test]
    fn retrying_usb_sequence_hits_recorder() {
        let rec = Recorder::default();
        let plan = init::init_usb_with_retry(&rec, 110).unwrap();
        assert_eq!(plan.x, 0x23);
        assert_eq!(rec.sent.borrow().len(), 3);
    }

    #[test]
    fn reconnect_mode_matrix() {
        // All 8 (live, force, disengage) combos: -d always wins.
        for (live, force) in [(true, true), (true, false), (false, true), (false, false)] {
            assert_eq!(
                resolve_reconnect_mode(live, force, true),
                ReconnectMode::Disengage,
                "disengage must win for live={live} force={force}"
            );
        }
        // Socket absent without -f → disengage (safe default for udev entry).
        assert_eq!(
            resolve_reconnect_mode(false, false, false),
            ReconnectMode::Disengage
        );
        // Socket absent with -f → init anyway.
        assert_eq!(
            resolve_reconnect_mode(false, true, false),
            ReconnectMode::Init
        );
        // Daemon live → init (with or without -f).
        assert_eq!(
            resolve_reconnect_mode(true, false, false),
            ReconnectMode::Init
        );
        assert_eq!(
            resolve_reconnect_mode(true, true, false),
            ReconnectMode::Init
        );
    }

    #[test]
    fn socket_absent_without_force_disengages_not_inits() {
        // Decision-level coverage for the udev socket-absent path: without
        // -f the mode must be Disengage (host-click off only, no init).
        assert_eq!(
            resolve_reconnect_mode(
                /* daemon_live */ false, /* force */ false, /* disengage */ false
            ),
            ReconnectMode::Disengage
        );
    }

    #[test]
    fn bt_reconnect_needs_no_enhanced_session_in_every_mode() {
        // Exercise the classified one-shot path without host discovery or
        // socket access. No daemon/force/disengage combination owns a proxy.
        let node = tempfile::NamedTempFile::new().unwrap();
        for enhanced_clicks in [false, true] {
            let cfg = Config {
                enhanced_clicks,
                ..Config::default()
            };
            for daemon_live in [false, true] {
                for force in [false, true] {
                    for disengage in [false, true] {
                        let mode = resolve_reconnect_mode(daemon_live, force, disengage);
                        // BT ignores USB polling options even when invalid.
                        reconnect_classified_node(
                            node.path().to_str().unwrap(),
                            Transport::Bluetooth,
                            mode,
                            Some(0),
                            &cfg,
                        )
                        .unwrap();
                    }
                }
            }
        }
    }

    #[test]
    fn bt_init_without_proxy_and_failed_listener_keep_siblings_independent() {
        let hub = DeviceReadyHub::new();
        let mut devs = fixture_devs();
        // No daemon/proxy listener: init remains hardware-only, even at an
        // invalid USB rate. No enhanced path exists to replace native input.
        let first = devs.pop().unwrap();
        let ev = init_node(&first, 0, &hub).unwrap();
        assert_eq!(ev, init::init_bluetooth(&first.devnode));
        hub.subscribe(|_| panic!("simulated attachment failure"));
        let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
        let capture = Arc::clone(&seen);
        hub.subscribe(move |ev| capture.lock().unwrap().push(ev.clone()));
        let second = DiscoveredDevice {
            devnode: "/dev/hidraw-other-bt".into(),
            ..first.clone()
        };
        for dev in [&first, &second] {
            init_node(dev, 0, &hub).unwrap();
        }
        let seen = seen.lock().unwrap();
        assert_eq!(seen.len(), 2);
        assert_eq!(seen[0].devnode, first.devnode);
        assert_eq!(seen[1].devnode, second.devnode);
        assert!(seen.iter().all(|ev| ev.kind == init::ReadyKind::Ready));
    }

    #[test]
    fn known_bt_ticks_leave_live_session_ownership_alone() {
        let dev = fixture_devs().pop().unwrap();
        let hub = DeviceReadyHub::new();
        let events = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let capture = Arc::clone(&events);
        hub.subscribe(move |_| {
            capture.fetch_add(1, Ordering::SeqCst);
        });
        for enhanced_clicks in [false, true] {
            let cfg = Config {
                enhanced_clicks,
                ..Config::default()
            };
            let mut watch = ResumeWatch::new(std::slice::from_ref(&dev));
            for _ in 0..20 {
                assert!(watch
                    .tick_with_scan(&cfg, &hub, vec![dev.clone()], &|node| session_needs_attach(
                        enhanced_clicks,
                        Some(&dev.devnode),
                        node,
                        true
                    ))
                    .is_empty());
            }
        }
        assert_eq!(events.load(Ordering::SeqCst), 0);
    }
}
