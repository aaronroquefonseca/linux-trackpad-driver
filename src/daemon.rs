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

/// Shared device-ready hub for this process. Clicks subscribes in Phase 1.
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
pub fn subscribe_click_proxy(cfg: &Config, ready_hub: &DeviceReadyHub) {
    use crate::proxy::{
        ClickProxy, DualSink, ProxySettings, RealGrab, RealHaptic, RealUinput, UinputSink,
    };
    use crate::pump::PumpSource;

    type DaemonProxy =
        ClickProxy<RealGrab, DualSink<RealUinput, RealUinput>, RealHaptic, PumpSource>;

    struct PumpManager {
        proxy: std::sync::Arc<std::sync::Mutex<DaemonProxy>>,
        cfg: Config,
        active: std::sync::Mutex<Option<String>>,
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
                px.haptic_mut().set_node(&hnode);
                log::info!(
                    "pump: haptic node {hnode} (interface {hiface:?}) for {}",
                    event.devnode
                );
                px.haptic_mut()
                    .set_bluetooth(event.transport == crate::init::Transport::Bluetooth);
                px.replace_source(PumpSource::Live(source));
                px.set_pump_ready(true);
                px.attach(event);
                if px.is_attached() {
                    *self.active.lock().unwrap_or_else(|e| e.into_inner()) =
                        Some(event.devnode.clone());
                } else {
                    // Grab refused: unwind to idle + destroy both so no orphan nodes.
                    px.set_pump_ready(false);
                    px.replace_source(PumpSource::Idle);
                    px.uinput().destroy();
                }
            }
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
                px.detach(event);
                px.set_pump_ready(false);
                px.replace_source(PumpSource::Idle);
                px.uinput().destroy();
                log::info!("pump: {} torn down", event.devnode);
            }
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
        RealHaptic::new(),
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
    // Keep the manager alive for the daemon lifetime: the hub holds only
    // the closure (which holds an Arc), so leak one Arc here. The pump
    // thread + proxy die with the process.
    std::mem::forget(manager);
    log::debug!(
        "click proxy subscribed (enhanced_clicks={})",
        cfg.enhanced_clicks
    );
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
/// BT: single-node, no reports, straight to ready + coexistence policy.
///
/// FOOTGUN: this convenience leaves `enhanced_clicks=false` (BT passthrough
/// untouched). Production paths (`run`/`reconnect`) must use
/// [`init_node_with_config`] so the opt-in is honored.
pub fn init_node(
    dev: &DiscoveredDevice,
    polling_hz: u32,
    ready_hub: &DeviceReadyHub,
) -> Result<DeviceReadyEvent> {
    init_node_with_config(dev, polling_hz, false, ready_hub)
}

/// [`init_node`] with the clicks opt-in plumbed through (BT double-input
/// handling only; USB path is identical either way).
pub fn init_node_with_config(
    dev: &DiscoveredDevice,
    polling_hz: u32,
    enhanced_clicks: bool,
    ready_hub: &DeviceReadyHub,
) -> Result<DeviceReadyEvent> {
    match dev.transport {
        Transport::Bluetooth => {
            let ev = init::init_bluetooth(&dev.devnode);
            ready_hub.emit(&ev);
            // Best-effort, never fails init; default leaves passthrough intact.
            let _ = init::apply_bt_coexistence(&dev.devnode, enhanced_clicks);
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
    let sock = lock::socket_path();
    let _lock = SocketLock::acquire(&sock)?;
    log::info!("lock acquired at {sock}");
    log::info!(
        "config: polling {} Hz, enhanced_clicks={}",
        cfg.polling_rate,
        cfg.enhanced_clicks
    );
    if cfg.polling_rate > EFFECTIVE_CAP_HZ as u32 {
        log::warn!(
            "requested {} Hz above device cap; clamped to ~110 Hz (X=0x23)",
            cfg.polling_rate
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

    let ready_hub = hub();
    subscribe_click_proxy(cfg, &ready_hub);
    let initial: Vec<DiscoveredDevice> = match discovery::scan_trackpads() {
        Ok(devs) if devs.is_empty() => {
            log::info!("no supported trackpads found; idling until signal");
            Vec::new()
        }
        Ok(devs) => {
            log::info!("found {} trackpad node(s)", devs.len());
            for dev in &devs {
                if let Err(e) =
                    init_node_with_config(dev, cfg.polling_rate, cfg.enhanced_clicks, &ready_hub)
                {
                    log::error!("init failed for {}: {e:#}", dev.devnode);
                }
            }
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
    // via GET_FEATURE, multitouch-first re-init on mismatch), at most
    // RESUME_REVERIFY_INTERVAL_SECS of cross-process re-attach latency.
    // Initial init above always runs first; ticks never reorder it.
    let mut watch = ResumeWatch::new(&initial);
    let mut last_tick = std::time::Instant::now();
    while running.load(Ordering::SeqCst) {
        // 100 ms poll: prompt shutdown without burning CPU in the idle loop.
        std::thread::sleep(std::time::Duration::from_millis(100));
        if last_tick.elapsed().as_secs() >= resume::RESUME_REVERIFY_INTERVAL_SECS {
            last_tick = std::time::Instant::now();
            watch.tick(cfg, &ready_hub);
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
    // No proxy subscription here: reconnect is a short-lived udev process
    // and must never grab. Its hub is a per-process orphan reaching zero
    // listeners; the daemon owns the proxy and re-attaches tick-mediated
    // (rescan → init on its own hub, ≤30 s latency).
    let ready_hub = hub();

    if mode == ReconnectMode::Disengage {
        match transport {
            Transport::Bluetooth => {
                // No host-click report defined for the BT bring-up path.
                // Unbind is sticky: restore passthrough for a previously
                // suppressed node (best-effort no-op when already bound).
                // Resume re-suppresses via the init arm when the opt-in is
                // set; the Detached event below lets clicks release the
                // node (re-attach is tick-mediated: rescan → init → Ready
                // on the daemon hub).
                log::info!("BT {devnode}: disengage (no reports sent)");
                if let Err(e) = init::restore_bt_passthrough(devnode) {
                    log::warn!("BT {devnode}: re-bind best-effort failed ({e:#})");
                }
                ready_hub.emit(&init::detached(devnode, Transport::Bluetooth));
                return Ok(());
            }
            Transport::Usb => {
                let sender = HidrawSender {
                    devnode: devnode.to_string(),
                };
                init::disengage_usb_with_retry(&sender)?;
                // Stateless restore: no-op when already bound (EBUSY skip),
                // re-binds when a previous suppress left the node unbound.
                if let Err(e) = init::restore_bt_passthrough(devnode) {
                    log::warn!("USB {devnode}: re-bind best-effort failed ({e:#})");
                }
                ready_hub.emit(&init::detached(devnode, Transport::Usb));
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
            let ev = init::init_bluetooth(devnode);
            ready_hub.emit(&ev);
            // Default leaves kernel passthrough intact; only
            // enhanced_clicks=true suppresses the physical node.
            let _ = init::apply_bt_coexistence(devnode, cfg.enhanced_clicks);
            Ok(())
        }
        Transport::Usb => {
            // Rate validation lives here only: disengage + BT discard the
            // rate, so they must not reject it.
            let polling_hz = polling_override.unwrap_or(cfg.polling_rate);
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
            ready_hub.emit(&init::usb_ready(devnode));
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
    let polling_hz = polling_override.unwrap_or(cfg.polling_rate);
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
    Ok(resume::run_resume_scan(
        &devs,
        polling_hz,
        cfg.enhanced_clicks,
    ))
}
/// Ticks between BT coexistence re-applies for known nodes (10 × 30 s =
/// 5 min). The hook's one-shot is the primary re-suppressor; this is the
/// backstop for hook-missed sleeps. Never emits hub traffic.
pub const BT_REAPPLY_EVERY_TICKS: u64 = 10;

/// Pure cadence: re-apply on every Nth tick (unit-tested; the tick itself
/// needs hardware).
pub fn bt_reapply_due(ticks: u64) -> bool {
    ticks > 0 && ticks.is_multiple_of(BT_REAPPLY_EVERY_TICKS)
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
    /// Node vanished → `Detached` emitted (remove path).
    Detached(String),
}

/// Daemon-side resume/add/change/remove watch. Owns the last-seen device
/// list plus per-node unreadable streaks; each [`ResumeWatch::tick`] diffs a
/// fresh scan against it:
///
/// - added node → full [`init_node_with_config`] (same ordering as boot init);
/// - still-present known USB node → one `GET_FEATURE` re-verify, threaded
///   into [`resume::resume_usb_node`] (no double read); multitouch-first
///   re-init on mismatch, [`resume::needs_reinit`] as the decision;
/// - still-present known USB node with a consecutively-unreadable verify →
///   re-init once (assume transient), then assume armed until the periodic
///   reprobe ([`resume::unreadable_disposition`]) — firmware that NAKs reads
///   must not cause a re-init + `Ready` storm every pass;
/// - still-present known BT node → slow-cadence coexistence re-apply only
///   (every [`BT_REAPPLY_EVERY_TICKS`]; the hook is primary, this backstops
///   hook-missed sleeps; never emits — re-emitting `Ready` every pass would
///   spam clicks);
/// - vanished node → `Detached` emit so clicks releases it.
///
/// Removal `Detached`s emit before the scan loop so a same-window
/// renumber self-heals (stale session torn down, then the new node
/// adopted); per-node init emits happen after each node's init step —
/// clicks re-attaches without reordering init.
pub struct ResumeWatch {
    known: Vec<NodeState>,
    /// Pass counter for slow-cadence work (BT re-apply).
    ticks: u64,
}

struct NodeState {
    dev: DiscoveredDevice,
    unreadable_streak: u32,
}

impl ResumeWatch {
    pub fn new(initial: &[DiscoveredDevice]) -> Self {
        Self {
            known: initial
                .iter()
                .map(|dev| NodeState {
                    dev: dev.clone(),
                    unreadable_streak: 0,
                })
                .collect(),
            ticks: 0,
        }
    }

    pub fn tick(&mut self, cfg: &Config, ready_hub: &DeviceReadyHub) -> Vec<TickAction> {
        self.ticks += 1;
        let current = match discovery::scan_trackpads() {
            Ok(devs) => devs,
            Err(e) => {
                log::warn!("resume tick: scan failed, keeping last-seen list: {e:#}");
                return Vec::new();
            }
        };
        self.tick_with_scan(cfg, ready_hub, current)
    }

    /// One pass over an injected scan (production passes the live scan
    /// from [`ResumeWatch::tick`]; tests inject renumbers/replugs).
    fn tick_with_scan(
        &mut self,
        cfg: &Config,
        ready_hub: &DeviceReadyHub,
        current: Vec<DiscoveredDevice>,
    ) -> Vec<TickAction> {
        let mut actions = Vec::new();
        // Nodes recorded Detached mid-pass (present in the scan but gone by
        // the wait) are dropped from the stored snapshot below — trusting
        // the fresher wait over the stale scan. They re-enter as added nodes
        // if they come back.
        let known_devs: Vec<DiscoveredDevice> = self.known.iter().map(|k| k.dev.clone()).collect();
        // Removals emit BEFORE the scan loop: a same-window renumber
        // (hidraw8→hidraw12) surfaces as one removal plus one addition,
        // and the new node's Ready must not be rejected as a sibling of
        // a stale session that is about to be torn down. Emit-then-adopt
        // self-heals within this tick; the reverse order bricks the new
        // node (Ready rejected, then Detached clears, no Ready ever).
        let (_added, removed) = resume::diff_scan(&known_devs, &current);
        for old in &removed {
            log::info!("resume tick: {} vanished; emitting Detached", old.devnode);
            ready_hub.emit(&init::detached(&old.devnode, old.transport));
            actions.push(TickAction::Detached(old.devnode.clone()));
        }
        let mut detached: Vec<String> = Vec::new();
        for dev in &current {
            let Some(pos) = self.known.iter().position(|k| k.dev.devnode == dev.devnode) else {
                debug_assert!(_added.iter().any(|d| d.devnode == dev.devnode));
                match init_node_with_config(dev, cfg.polling_rate, cfg.enhanced_clicks, ready_hub) {
                    Ok(_) => {
                        log::info!("resume tick: initialized newly appeared {}", dev.devnode);
                        actions.push(TickAction::Initialized(dev.devnode.clone()));
                    }
                    Err(e) => log::error!(
                        "resume tick: init failed for newly appeared {}: {e:#}",
                        dev.devnode
                    ),
                }
                continue;
            };
            if dev.transport != Transport::Usb {
                // Known BT: steady state, no hub traffic — except a
                // slow-cadence coexistence re-apply (the hook's one-shot is
                // primary; this backstops hook-missed sleeps). Never emits.
                if bt_reapply_due(self.ticks) {
                    log::debug!(
                        "resume tick: re-applying BT coexistence for {}",
                        dev.devnode
                    );
                    let _ = init::apply_bt_coexistence(&dev.devnode, cfg.enhanced_clicks);
                }
                continue;
            }
            let verify = resume::verify_node_polling(&dev.devnode, cfg.polling_rate);
            if !resume::needs_reinit(&verify) {
                self.known[pos].unreadable_streak = 0;
                actions.push(TickAction::UpToDate(dev.devnode.clone()));
                continue;
            }
            if verify.is_err() {
                let prior = self.known[pos].unreadable_streak;
                self.known[pos].unreadable_streak = prior + 1;
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
                self.known[pos].unreadable_streak = 0;
            }
            match resume::resume_usb_node(&dev.devnode, cfg.polling_rate, Some(ready_hub), &verify)
            {
                Ok(resume::ResumeAction::Reinitialized) => {
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
                    log::error!("resume tick: re-init failed for {}: {e:#}", dev.devnode);
                }
            }
        }
        // Rebuild from the fresh scan, preserving streaks of survivors and
        // dropping mid-pass detachments (list-skew fix).
        self.known = current
            .into_iter()
            .filter(|d| !detached.contains(&d.devnode))
            .map(|dev| {
                let unreadable_streak = self
                    .known
                    .iter()
                    .find(|k| k.dev.devnode == dev.devnode)
                    .map_or(0, |k| k.unreadable_streak);
                NodeState {
                    dev,
                    unreadable_streak,
                }
            })
            .collect();
        actions
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::init::Recorder;

    #[test]
    fn bt_reapply_fires_every_nth_tick_only() {
        assert!(!bt_reapply_due(0));
        assert!(!bt_reapply_due(1));
        assert!(!bt_reapply_due(BT_REAPPLY_EVERY_TICKS - 1));
        assert!(bt_reapply_due(BT_REAPPLY_EVERY_TICKS));
        assert!(!bt_reapply_due(BT_REAPPLY_EVERY_TICKS + 1));
        assert!(bt_reapply_due(2 * BT_REAPPLY_EVERY_TICKS));
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
        let actions = watch.tick(&cfg, &hub);
        assert_eq!(
            actions,
            vec![TickAction::Detached("/dev/hidraw-nope".into())]
        );
        let seen = seen.lock().expect("seen lock");
        assert_eq!(seen.len(), 1);
        assert_eq!(seen[0].kind, crate::init::ReadyKind::Detached);
        // Second tick: steady state, no repeat traffic.
        let actions = watch.tick(&cfg, &hub);
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
        let actions = watch.tick_with_scan(&cfg, &hub, current);
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
    fn bt_node_with_config_plumbs_coexistence_without_reports() {
        // BT bring-up emits Ready (never Detached) and applies the (default
        // passthrough) policy — and takes no FeatureSender at all, so no
        // feature report can be emitted on this path by construction.
        let hub = DeviceReadyHub::new();
        let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let capture = std::sync::Arc::clone(&seen);
        hub.subscribe(move |ev| capture.lock().expect("seen lock").push(ev.clone()));
        let dev = DiscoveredDevice {
            devnode: "/dev/hidraw9".into(),
            vendor_id: 0x004c,
            product_id: 0x0324,
            transport: Transport::Bluetooth,
        };
        let ev = init_node_with_config(&dev, 500, false, &hub).unwrap();
        assert_eq!(ev.transport, Transport::Bluetooth);
        assert_eq!(ev.kind, crate::init::ReadyKind::Ready);
        let seen = seen.lock().expect("seen lock");
        assert_eq!(*seen, vec![ev]);
    }
}
