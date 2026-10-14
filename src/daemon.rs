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
/// Safety posture: the proxy starts pump-unready (`NullSource`, no pump
/// loop, no created uinput node), so `attach` degrades to passthrough and
/// never grabs. The transport task flips readiness once the emit path
/// exists. The evdev node is never guessed: without `LTD_EVDEV_NODE` the
/// proxy refuses and stays passthrough.
pub fn subscribe_click_proxy(cfg: &Config, ready_hub: &DeviceReadyHub) {
    use crate::proxy::{
        ClickProxy, DaemonGrab, NullSource, ProxySettings, RealGrab, RealHaptic, RealUinput,
        RefusingGrab,
    };
    let grab = match std::env::var("LTD_EVDEV_NODE") {
        Ok(node) => DaemonGrab::Real(RealGrab::new(&node)),
        Err(_) => {
            log::warn!(
                "proxy: LTD_EVDEV_NODE unset, refusing evdev guess (hidraw→eventN lands with transport); passthrough until configured"
            );
            DaemonGrab::Refusing(RefusingGrab)
        }
    };
    let uinput_node = std::env::var("LTD_UINPUT_NODE").unwrap_or("/dev/uinput".to_string());
    let proxy = std::sync::Arc::new(std::sync::Mutex::new(ClickProxy::new(
        ProxySettings::from(cfg),
        grab,
        RealUinput::new(&uinput_node),
        RealHaptic::new(),
        NullSource,
    )));
    crate::proxy::subscribe_to_hub(ready_hub, proxy);
    log::debug!(
        "click proxy subscribed (enhanced_clicks={})",
        cfg.enhanced_clicks
    );
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
    // is the primary post-sleep re-init; this tick covers add/change/remove
    // that arrive with no sleep hook (re-verify polling via GET_FEATURE,
    // multitouch-first re-init on mismatch). Initial init above always runs
    // first; ticks never reorder it.
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
    let devs = discovery::scan_trackpads().context("scanning hidraw for resume")?;
    if devs.is_empty() {
        log::info!("resume: no supported trackpads found; nothing to re-init");
    }
    Ok(resume::run_resume_scan(
        &devs,
        polling_hz,
        cfg.enhanced_clicks,
    ))
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
    /// Node vanished → `Detached` emitted (remove path).
    Detached(String),
}

/// Daemon-side resume/add/change/remove watch. Owns the last-seen device
/// list; each [`ResumeWatch::tick`] diffs a fresh scan against it:
///
/// - added node → full [`init_node_with_config`] (same ordering as boot init);
/// - still-present known USB node → `GET_FEATURE` re-verify, multitouch-first
///   re-init on mismatch via [`resume::resume_usb_node`];
/// - still-present known BT node → nothing (no verify exists; the hook +
///   add events cover it; re-emitting `Ready` every pass would spam clicks);
/// - vanished node → `Detached` emit so clicks releases it.
///
/// Hub emits happen after each node's init step, never before — clicks
/// re-attaches without reordering init.
pub struct ResumeWatch {
    known: Vec<DiscoveredDevice>,
}

impl ResumeWatch {
    pub fn new(initial: &[DiscoveredDevice]) -> Self {
        Self {
            known: initial.to_vec(),
        }
    }

    pub fn tick(&mut self, cfg: &Config, ready_hub: &DeviceReadyHub) -> Vec<TickAction> {
        let current = match discovery::scan_trackpads() {
            Ok(devs) => devs,
            Err(e) => {
                log::warn!("resume tick: scan failed, keeping last-seen list: {e:#}");
                return Vec::new();
            }
        };
        let mut actions = Vec::new();
        for dev in &current {
            if self.known.iter().any(|k| k.devnode == dev.devnode) {
                if dev.transport == Transport::Usb {
                    match resume::verify_node_polling(&dev.devnode, cfg.polling_rate) {
                        Ok(true) => actions.push(TickAction::UpToDate(dev.devnode.clone())),
                        other => {
                            if other.is_err() {
                                log::warn!(
                                    "resume tick: {} verify unreadable; re-initing",
                                    dev.devnode
                                );
                            }
                            match resume::resume_usb_node(
                                &dev.devnode,
                                cfg.polling_rate,
                                Some(ready_hub),
                            ) {
                                Ok(resume::ResumeAction::Reinitialized) => {
                                    actions.push(TickAction::Reinitialized(dev.devnode.clone()))
                                }
                                Ok(resume::ResumeAction::Detached) => {
                                    actions.push(TickAction::Detached(dev.devnode.clone()))
                                }
                                Ok(resume::ResumeAction::UpToDate) => {
                                    actions.push(TickAction::UpToDate(dev.devnode.clone()))
                                }
                                Err(e) => {
                                    log::error!(
                                        "resume tick: re-init failed for {}: {e:#}",
                                        dev.devnode
                                    );
                                }
                            }
                        }
                    }
                }
                // Known BT: steady state, no hub traffic.
                continue;
            }
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
        }
        for old in &self.known {
            if !current.iter().any(|d| d.devnode == old.devnode) {
                log::info!("resume tick: {} vanished; emitting Detached", old.devnode);
                ready_hub.emit(&init::detached(&old.devnode, old.transport));
                actions.push(TickAction::Detached(old.devnode.clone()));
            }
        }
        self.known = current;
        actions
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::init::Recorder;

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
