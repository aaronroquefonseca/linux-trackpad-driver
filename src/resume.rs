//! Resume re-init: system-side post-sleep path + daemon belt-and-braces.
//!
//! Why this exists (RESEARCH §6): Trackpad 2 USB drops out of multitouch mode
//! across a power transition (report `0x02` shrinks 21→8 bytes; clicks survive,
//! motion lost until re-init). The fix is re-running the full USB sequence
//! starting at multitouch enable — [`crate::init::resume_usb`].
//!
//! Two layers, either sufficient alone:
//! - System side: the `system-sleep` hook / resume unit runs `ltd resume`
//!   (see [`run_resume_scan`]), which re-verifies every node and re-inits on
//!   mismatch. System-side only: no user-session IPC (user.slice is frozen
//!   while sleep hooks run).
//! - Daemon side: the `run` loop calls [`ResumeWatch::tick`] every
//!   [`RESUME_REVERIFY_INTERVAL_SECS`], covering add/change/remove between or
//!   without sleep hooks (re-verify polling via `GET_FEATURE`, re-init on
//!   mismatch).
//!
//! Clicks contract: re-init emits `Ready` on the [`DeviceReadyHub`] *after*
//! the sequence succeeds, so the future clicks pipeline re-attaches without
//! ever reordering init. A node that vanished emits `Detached` instead, never
//! `Ready`.

use anyhow::Result;

use crate::discovery::{self, DiscoveredDevice};
use crate::init::{self, DeviceReadyHub, Transport};

/// Seconds between daemon belt-and-braces re-verify passes. One `GET_FEATURE`
/// per known USB node per pass — cheap, no wakeups worth avoiding.
pub const RESUME_REVERIFY_INTERVAL_SECS: u64 = 30;
/// Bounded post-resume wait per node: the node normally survives suspend, so
/// a short defer suffices and the daemon loop / hook never stalls on it.
pub const RESUME_READY_TIMEOUT_MS: u64 = 1000;

/// Outcome of one resume pass for one node.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResumeAction {
    /// Polling read-back matched; no reports sent, no hub traffic.
    UpToDate,
    /// Mismatch (or unreadable verify) → multitouch-first re-init sent,
    /// `Ready` emitted for clicks re-attach.
    Reinitialized,
    /// Node gone → `Detached` emitted, never `Ready`.
    Detached,
}

/// Pure decision: only a matching read-back skips re-init. A mismatch means
/// the dropout regression (or a mode switch) and an unreadable verify means
/// we cannot prove multitouch is armed — both re-init (idempotent, safe).
pub fn needs_reinit(verify: &Result<bool>) -> bool {
    !matches!(verify, Ok(true))
}

/// Expected polling report bytes for a requested rate (pure, unit-tested).
pub fn expected_polling(polling_hz: u32) -> Result<[u8; 3]> {
    let plan = init::resolve_polling(polling_hz)?;
    Ok(init::polling_report(plan.x))
}

/// Best-effort `GET_FEATURE` read-back of the polling report on a live node.
/// `Ok(true)` = armed as requested, `Ok(false)` = mismatch, `Err` =
/// unreadable (some firmware NAKs reads).
pub fn verify_node_polling(devnode: &str, polling_hz: u32) -> Result<bool> {
    let expected = expected_polling(polling_hz)?;
    init::verify_report(devnode, &expected)
}

/// Resume one USB node: bounded wait, verify, multitouch-first re-init on
/// mismatch, `Ready` emit for clicks re-attach. Missing node → `Detached`.
pub fn resume_usb_node(
    devnode: &str,
    polling_hz: u32,
    ready_hub: &DeviceReadyHub,
) -> Result<ResumeAction> {
    let (node, bound) = discovery::wait_for_ready(devnode, RESUME_READY_TIMEOUT_MS);
    if !node {
        log::warn!("resume: node {devnode} gone after suspend; emitting Detached");
        ready_hub.emit(&init::detached(devnode, Transport::Usb));
        return Ok(ResumeAction::Detached);
    }
    if !bound {
        log::debug!("resume: node {devnode} present, bind not yet visible; verify will cover it");
    }
    match verify_node_polling(devnode, polling_hz) {
        Ok(true) => {
            log::debug!("resume: {devnode} polling read-back matches; no re-init needed");
            Ok(ResumeAction::UpToDate)
        }
        other => {
            if other.is_err() {
                log::warn!("resume: {devnode} verify unreadable; re-initing anyway");
            } else {
                log::warn!("resume: {devnode} polling mismatch (dropout?); re-sending from multitouch enable");
            }
            let sender = init::HidrawSender {
                devnode: devnode.to_string(),
            };
            let verifier = init::HidrawVerifier {
                devnode: devnode.to_string(),
            };
            let iface = discovery::interface_number_for_node(devnode);
            let outcome = init::resume_usb_verified(&sender, &verifier, polling_hz, iface)?;
            log::info!(
                "resume: {devnode} re-init accepted (iface {:?}, polling [99 F2 {:02X}])",
                outcome.iface,
                outcome.plan.x,
            );
            ready_hub.emit(&init::usb_ready(devnode));
            Ok(ResumeAction::Reinitialized)
        }
    }
}

/// Resume one BT node: no `GET_FEATURE` verify exists on this path, so always
/// re-emit `Ready` (clicks re-attach) and re-apply the coexistence policy
/// (re-suppresses when the opt-in is set — the unbind did not survive sleep).
pub fn resume_bt_node(
    devnode: &str,
    enhanced_clicks: bool,
    ready_hub: &DeviceReadyHub,
) -> ResumeAction {
    let ev = init::init_bluetooth(devnode);
    ready_hub.emit(&ev);
    let _ = init::apply_bt_coexistence(devnode, enhanced_clicks);
    log::info!("resume: BT {devnode} re-attached (Ready re-emitted)");
    ResumeAction::Reinitialized
}

/// Per-node outcome of [`run_resume_scan`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NodeResume {
    pub devnode: String,
    pub action: ResumeAction,
}

/// Summary of one resume pass over all discovered nodes.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct ResumeReport {
    pub nodes: Vec<NodeResume>,
    pub errors: Vec<String>,
}

impl ResumeReport {
    pub fn failed(&self) -> bool {
        !self.errors.is_empty()
    }

    pub fn reinitialized(&self) -> usize {
        self.nodes
            .iter()
            .filter(|n| n.action == ResumeAction::Reinitialized)
            .count()
    }
}

/// One resume pass over an explicit device list (scan lives with the caller
/// so both the `ltd resume` one-shot and the daemon tick share this core).
/// Per-node failures are collected, never fatal to sibling nodes.
pub fn run_resume_scan(
    devs: &[DiscoveredDevice],
    polling_hz: u32,
    enhanced_clicks: bool,
    ready_hub: &DeviceReadyHub,
) -> ResumeReport {
    let mut report = ResumeReport::default();
    for dev in devs {
        let outcome = match dev.transport {
            Transport::Usb => resume_usb_node(&dev.devnode, polling_hz, ready_hub),
            Transport::Bluetooth => Ok(resume_bt_node(&dev.devnode, enhanced_clicks, ready_hub)),
        };
        match outcome {
            Ok(action) => report.nodes.push(NodeResume {
                devnode: dev.devnode.clone(),
                action,
            }),
            Err(e) => {
                log::error!("resume: re-init failed for {}: {e:#}", dev.devnode);
                report.errors.push(format!("{}: {e:#}", dev.devnode));
            }
        }
    }
    log::info!(
        "resume: pass done ({} re-init, {} up-to-date, {} errors)",
        report.reinitialized(),
        report
            .nodes
            .iter()
            .filter(|n| n.action == ResumeAction::UpToDate)
            .count(),
        report.errors.len(),
    );
    report
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::init::ReadyKind;

    #[test]
    fn only_matching_readback_skips_reinit() {
        assert!(!needs_reinit(&Ok(true)));
        assert!(needs_reinit(&Ok(false)));
        assert!(needs_reinit(&Err(anyhow::anyhow!("firmware NAKs reads"))));
    }

    #[test]
    fn expected_polling_pins_wire_bytes() {
        assert_eq!(expected_polling(110).unwrap(), [153, 242, 0x23]);
        assert!(expected_polling(0).is_err());
        assert!(expected_polling(1).is_err());
    }

    #[test]
    fn bt_resume_reattaches_ready_via_hub() {
        let hub = DeviceReadyHub::new();
        let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let capture = std::sync::Arc::clone(&seen);
        hub.subscribe(move |ev| capture.lock().expect("seen lock").push(ev.clone()));
        let action = resume_bt_node("/dev/hidraw9", false, &hub);
        assert_eq!(action, ResumeAction::Reinitialized);
        let seen = seen.lock().expect("seen lock");
        assert_eq!(seen.len(), 1);
        assert_eq!(seen[0].kind, ReadyKind::Ready);
        assert_eq!(seen[0].transport, Transport::Bluetooth);
    }

    #[test]
    fn detach_then_reattach_sequence_is_distinct() {
        // Clicks must tell "device going away" from "device back": Detached
        // first, then a fresh Ready — never two Readys, never Ready for gone.
        let hub = DeviceReadyHub::new();
        let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let capture = std::sync::Arc::clone(&seen);
        hub.subscribe(move |ev| capture.lock().expect("seen lock").push(ev.clone()));
        hub.emit(&init::detached("/dev/hidraw9", Transport::Bluetooth));
        resume_bt_node("/dev/hidraw9", false, &hub);
        let seen = seen.lock().expect("seen lock");
        assert_eq!(
            seen.iter().map(|e| e.kind).collect::<Vec<_>>(),
            vec![ReadyKind::Detached, ReadyKind::Ready]
        );
    }

    #[test]
    fn missing_usb_node_emits_detached_never_ready() {
        let hub = DeviceReadyHub::new();
        let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let capture = std::sync::Arc::clone(&seen);
        hub.subscribe(move |ev| capture.lock().expect("seen lock").push(ev.clone()));
        let action = resume_usb_node("/nonexistent-hidraw-xyz", 110, &hub).unwrap();
        assert_eq!(action, ResumeAction::Detached);
        let seen = seen.lock().expect("seen lock");
        assert_eq!(seen.len(), 1);
        assert_eq!(seen[0].kind, ReadyKind::Detached);
    }

    #[test]
    fn scan_core_collects_per_node_errors() {
        // /dev/null exists but is not a trackpad: USB resume fails on
        // ioctl, BT re-attaches fine — one node's failure never takes the
        // pass (or the sibling) down.
        let hub = DeviceReadyHub::new();
        let devs = vec![
            DiscoveredDevice {
                devnode: "/dev/null".into(),
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
        ];
        let report = run_resume_scan(&devs, 110, false, &hub);
        assert_eq!(report.nodes.len(), 1);
        assert_eq!(report.nodes[0].action, ResumeAction::Reinitialized);
        assert_eq!(report.errors.len(), 1);
        assert!(report.failed());
    }
}
