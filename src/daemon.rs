//! Daemon lifecycle: `run` loop + `reconnect` udev entry.
//!
//! `run`: acquires [`SocketLock`], discovers devices, runs init per node
//! (USB feature reports / BT single-node), emits [`DeviceReadyEvent`] on the
//! shared [`DeviceReadyHub`], then idles until SIGINT/SIGTERM. No devices =
//! log + idle (graceful, exit 0 on signal), never panic.
//!
//! `reconnect <dev>`: idempotent per-node re-init for udev `add`. Daemon
//! socket absent without `-f` → disengage path (host-click off, no init).
//! `-p` overrides the polling request (same ~110 Hz cap). Always re-sends
//! multitouch enable first: the resume regression (report 0x02 21→8 bytes
//! across power transition) is only fixed by re-init.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use anyhow::Result;

use crate::config::Config;
use crate::discovery::{self, DiscoveredDevice};
use crate::init::{self, DeviceReadyEvent, DeviceReadyHub, HidrawSender, Transport};
use crate::lock::{self, SocketLock};

/// Shared device-ready hub for this process. Clicks subscribes in Phase 1.
pub fn hub() -> DeviceReadyHub {
    DeviceReadyHub::new()
}

/// Initialize one discovered node and return its ready event.
/// USB: wait-for-node → retrying feature reports → verify → ready.
/// BT: single-node, no reports, straight to ready.
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
            // Probe/bind race: node may lag udev `add`; brief wait, then
            // retrying writes carry the rest.
            if !discovery::wait_for_node(&dev.devnode, 2000) {
                log::warn!(
                    "node {} not present after 2 s wait; attempting init anyway",
                    dev.devnode
                );
            }
            let sender = HidrawSender {
                devnode: dev.devnode.clone(),
            };
            let plan = init::init_usb_with_retry(&sender, polling_hz)?;
            // Best-effort verify of the polling report; mismatch is a warning
            // (some firmware NAKs reads), not a fatal error.
            match init::verify_report(&dev.devnode, &init::polling_report(plan.x)) {
                Ok(true) => log::debug!("polling verify OK on {}", dev.devnode),
                Ok(false) => log::warn!("polling verify mismatch on {}", dev.devnode),
                Err(e) => log::warn!("polling verify unreadable on {}: {e:#}", dev.devnode),
            }
            let ev = init::usb_ready(&dev.devnode);
            ready_hub.emit(&ev);
            Ok(ev)
        }
    }
}

/// `run` daemon loop. Holds the socket lock, inits all current devices,
/// then parks until SIGINT/SIGTERM. Clean exit removes the socket.
pub fn run_daemon(cfg: &Config) -> Result<()> {
    let sock = lock::socket_path();
    let _lock = SocketLock::acquire(&sock)?;
    log::info!("lock acquired at {sock}");
    log::info!(
        "config: polling {} Hz, enhanced_clicks={}",
        cfg.polling_rate,
        cfg.enhanced_clicks
    );

    let running = Arc::new(AtomicBool::new(true));
    let stop = Arc::clone(&running);
    ctrlc::set_handler(move || {
        log::info!("received SIGINT/SIGTERM, shutting down...");
        stop.store(false, Ordering::SeqCst);
    })
    .map_err(|e| anyhow::anyhow!("installing signal handler: {e}"))?;

    let ready_hub = hub();
    match discovery::scan_trackpads() {
        Ok(devs) if devs.is_empty() => {
            log::info!("no supported trackpads found; idling until signal");
        }
        Ok(devs) => {
            log::info!("found {} trackpad node(s)", devs.len());
            for dev in &devs {
                if let Err(e) = init_node(dev, cfg.polling_rate, &ready_hub) {
                    log::error!("init failed for {}: {e:#}", dev.devnode);
                }
            }
        }
        Err(e) => log::warn!("device scan failed (continuing idle): {e:#}"),
    }

    while running.load(Ordering::SeqCst) {
        std::thread::sleep(std::time::Duration::from_millis(500));
    }
    log::info!("shutdown complete");
    Ok(())
}

/// Classify a devnode's transport for the reconnect path without a full
/// sysfs scan: BT vendor nodes contain no USB markers. Falls back to sysfs
/// matching when available, else USB (init dance is idempotent anyway).
fn transport_for_node(devnode: &str) -> Transport {
    match discovery::scan_trackpads() {
        Ok(devs) => devs
            .iter()
            .find(|d| d.devnode == devnode)
            .map(|d| d.transport)
            .unwrap_or(Transport::Usb),
        Err(_) => Transport::Usb,
    }
}

/// `reconnect` entry. Idempotent; safe to re-run on every udev `add`.
/// Missing node / missing hardware → `Err` (caller logs + exits nonzero),
/// never a panic.
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
    let polling_hz = polling_override.unwrap_or(cfg.polling_rate);
    if polling_hz == 0 || polling_hz > u16::MAX as u32 {
        anyhow::bail!("invalid polling rate {polling_hz} (want 1..=65535)");
    }

    let sock = lock::socket_path();
    let daemon_live = lock::daemon_socket_present(&sock);
    let disengage_mode = disengage || (!daemon_live && !force);
    if disengage && daemon_live {
        log::info!("disengage requested while daemon live; applying host-click off");
    } else if !daemon_live && !force {
        log::info!(
            "daemon socket absent ({sock}); proceeding in disengage mode (use -f to init anyway)"
        );
    } else if !daemon_live {
        log::info!("daemon socket absent but -f given; proceeding with init");
    }

    let transport = transport_for_node(devnode);
    let ready_hub = hub();

    if disengage_mode {
        match transport {
            Transport::Bluetooth => {
                // No host-click report defined for the BT bring-up path;
                // disengage = log + ready event so subscribers can detach.
                log::info!("BT {devnode}: disengage (no reports sent)");
                ready_hub.emit(&init::init_bluetooth(devnode));
                return Ok(());
            }
            Transport::Usb => {
                if !discovery::wait_for_node(devnode, 2000) {
                    anyhow::bail!("device node '{devnode}' not present");
                }
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
            let ev = init::init_bluetooth(devnode);
            ready_hub.emit(&ev);
            Ok(())
        }
        Transport::Usb => {
            if !discovery::wait_for_node(devnode, 2000) {
                anyhow::bail!("device node '{devnode}' not present");
            }
            let sender = HidrawSender {
                devnode: devnode.to_string(),
            };
            let plan = init::init_usb_with_retry(&sender, polling_hz)?;
            match init::verify_report(devnode, &init::polling_report(plan.x)) {
                Ok(true) => log::debug!("polling verify OK on {devnode}"),
                Ok(false) => log::warn!("polling verify mismatch on {devnode}"),
                Err(e) => log::warn!("polling verify unreadable on {devnode}: {e:#}"),
            }
            ready_hub.emit(&init::usb_ready(devnode));
            Ok(())
        }
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
    fn reconnect_rejects_bad_polling() {
        let cfg = Config::default();
        assert!(run_reconnect("/dev/hidraw0", false, Some(0), false, &cfg).is_err());
    }

    #[test]
    fn reconnect_missing_node_errors_without_panic() {
        let cfg = Config::default();
        // No hardware in test env; disengage path still needs the node.
        let r = run_reconnect("/nonexistent-hidraw-xyz", true, Some(110), false, &cfg);
        assert!(r.is_err());
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
    }

    #[test]
    fn retrying_usb_sequence_hits_recorder() {
        let rec = Recorder::default();
        let plan = init::init_usb_with_retry(&rec, 110).unwrap();
        assert_eq!(plan.x, 0x23);
        assert_eq!(rec.sent.borrow().len(), 3);
    }
}
