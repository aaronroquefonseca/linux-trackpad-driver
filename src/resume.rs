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
//!   mismatch. Hardware-only: the one-shot owns no listeners, so it emits
//!   nothing — re-arming the device is its whole job. System-side only: no
//!   user-session IPC (user.slice is frozen while sleep hooks run).
//! - Daemon side: the `run` loop calls [`ResumeWatch::tick`] every
//!   [`RESUME_REVERIFY_INTERVAL_SECS`], covering add/change/remove between or
//!   without sleep hooks (re-verify polling via `GET_FEATURE`, re-init on
//!   mismatch).
//!
//! Clicks contract: re-init emits `Ready` on the [`DeviceReadyHub`] *after*
//! the sequence succeeds, so the clicks pipeline re-attaches without ever
//! reordering init. Those emits only happen on a live daemon hub (boot init,
//! tick): the one-shot's hub would be a per-process orphan reaching zero
//! listeners, so cross-process re-attach is tick-mediated (≤
//! [`RESUME_REVERIFY_INTERVAL_SECS`] latency) plus the udev `add` path for
//! re-enumerated nodes. A node that vanished emits `Detached` instead, never
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
/// Ticks between reprobes of a node whose `GET_FEATURE` stays unreadable.
/// Some firmware NAKs reads forever; without a cooldown every tick would
/// send a full 3-report re-init + `Ready` for zero new information.
pub const UNREADABLE_RETRY_EVERY_TICKS: u32 = 20;

/// Outcome of one resume pass for one node.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResumeAction {
    /// Polling read-back matched; no reports sent, no hub traffic.
    UpToDate,
    /// Mismatch (or unreadable verify) → multitouch-first re-init sent.
    /// Hub `Ready` is emitted only when a live hub is passed (daemon tick);
    /// the one-shot passes none (its hub would reach zero listeners).
    Reinitialized,
    /// Node gone → recorded as detached (`Detached` emitted only with a
    /// live hub), never `Ready`.
    Detached,
}

/// Pure decision: only a matching read-back skips re-init. A mismatch means
/// the dropout regression (or a mode switch) and an unreadable verify means
/// we cannot prove multitouch is armed — both re-init (idempotent, safe).
/// Callers layer the [`unreadable_disposition`] cooldown on top so a
/// permanently-unreadable node does not re-init every tick.
pub fn needs_reinit(verify: &Result<bool>) -> bool {
    !matches!(verify, Ok(true))
}

/// Cooldown verdict for a consecutively-unreadable node.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnreadableDisposition {
    /// Send the re-init now.
    ReinitNow,
    /// Assume armed; send nothing this pass.
    Suppress,
}

/// Pure cooldown: the first unreadable re-inits (assume transient), further
/// consecutive unreadables suppress until every [`UNREADABLE_RETRY_EVERY_TICKS`]
/// reprobe. `prior_streak` counts consecutive unreadables *before* this pass;
/// a matching read-back resets it (caller-owned).
pub fn unreadable_disposition(prior_streak: u32) -> UnreadableDisposition {
    if prior_streak == 0 || (prior_streak + 1).is_multiple_of(UNREADABLE_RETRY_EVERY_TICKS) {
        UnreadableDisposition::ReinitNow
    } else {
        UnreadableDisposition::Suppress
    }
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

/// Resume one USB node: bounded wait, then verify-or-reinit on the caller's
/// `first_verify` — the tick and the one-shot read `GET_FEATURE` once per
/// pass and thread it in, so the steady state costs exactly one read per
/// node (the re-init path does its own inner verify, which is new
/// information, not a duplicate).
///
/// `ready_hub` is `Some` on the daemon tick (live listeners: `Ready`
/// re-attaches clicks) and `None` in the one-shot (hardware-only).
/// Missing node → `Detached` (emitted only with a live hub).
pub fn resume_usb_node(
    devnode: &str,
    polling_hz: u32,
    ready_hub: Option<&DeviceReadyHub>,
    first_verify: &Result<bool>,
) -> Result<ResumeAction> {
    let (node, bound) = discovery::wait_for_ready(devnode, RESUME_READY_TIMEOUT_MS);
    if !node {
        log::warn!("resume: node {devnode} gone after suspend; recording Detached");
        if let Some(hub) = ready_hub {
            hub.emit(&init::detached(devnode, Transport::Usb));
        }
        return Ok(ResumeAction::Detached);
    }
    if !bound {
        log::debug!("resume: node {devnode} present, bind not yet visible; re-init covers it");
    }
    if !needs_reinit(first_verify) {
        log::debug!("resume: {devnode} polling read-back matches; no re-init needed");
        return Ok(ResumeAction::UpToDate);
    }
    if first_verify.is_err() {
        log::warn!("resume: {devnode} verify unreadable; re-initing anyway");
    } else {
        log::warn!(
            "resume: {devnode} polling mismatch (dropout?); re-sending from multitouch enable"
        );
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
    if let Some(hub) = ready_hub {
        hub.emit(&init::usb_ready(devnode));
    }
    Ok(ResumeAction::Reinitialized)
}

/// Resume one BT node (one-shot hardware arm): no `GET_FEATURE` verify exists
/// on this path, so re-apply the coexistence policy (re-suppresses when the
/// opt-in is set — the unbind did not survive sleep). Emits nothing: clicks
/// re-attach is tick/udev-mediated (daemon re-emits `Ready` when the node
/// re-appears or the tick re-inits it).
pub fn resume_bt_node(devnode: &str, enhanced_clicks: bool) -> ResumeAction {
    let _ = init::apply_bt_coexistence(devnode, enhanced_clicks);
    log::info!("resume: BT {devnode} policy re-applied (hub re-attach is tick-mediated)");
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
/// Hardware-only: passes no hub, emits nothing — the caller's process owns
/// no listeners. Per-node failures are collected, never fatal to siblings.
///
/// Single scan, no rescan loop: each node gets one bounded
/// [`RESUME_READY_TIMEOUT_MS`] wait, then verify-or-reinit. A node that
/// vanishes mid-pass is recorded `Detached`; the daemon tick reconciles any
/// skew on its next pass. Hook, tick, and udev interleave benignly: every
/// path sends the same idempotent multitouch-first sequence.
pub fn run_resume_scan(
    devs: &[DiscoveredDevice],
    polling_hz: u32,
    enhanced_clicks: bool,
) -> ResumeReport {
    let mut report = ResumeReport::default();
    for dev in devs {
        let outcome = match dev.transport {
            Transport::Usb => {
                let first_verify = verify_node_polling(&dev.devnode, polling_hz);
                resume_usb_node(&dev.devnode, polling_hz, None, &first_verify)
            }
            Transport::Bluetooth => Ok(resume_bt_node(&dev.devnode, enhanced_clicks)),
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
    fn bt_oneshot_reapplies_policy_without_hub_traffic() {
        // One-shot BT re-arms hardware only (coexistence policy); it owns no
        // listeners, so it takes no hub and emits nothing. Ready re-emit is
        // tick/udev-mediated.
        let action = resume_bt_node("/dev/hidraw9", false);
        assert_eq!(action, ResumeAction::Reinitialized);
    }

    #[test]
    fn detach_then_reattach_sequence_is_distinct() {
        // Hub contract clicks relies on: "device going away" (Detached)
        // first, then a fresh Ready — never two Readys, never Ready for gone.
        // Pinned at hub level; the tick is the production emitter.
        let hub = DeviceReadyHub::new();
        let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let capture = std::sync::Arc::clone(&seen);
        hub.subscribe(move |ev| capture.lock().expect("seen lock").push(ev.clone()));
        hub.emit(&init::detached("/dev/hidraw9", Transport::Bluetooth));
        hub.emit(&init::init_bluetooth("/dev/hidraw9"));
        let seen = seen.lock().expect("seen lock");
        assert_eq!(
            seen.iter().map(|e| e.kind).collect::<Vec<_>>(),
            vec![ReadyKind::Detached, ReadyKind::Ready]
        );
    }

    #[test]
    fn unreadable_cooldown_reinits_once_then_suppresses() {
        use UnreadableDisposition::{ReinitNow, Suppress};
        // First sighting re-inits (assume transient).
        assert_eq!(unreadable_disposition(0), ReinitNow);
        // Then suppress until the periodic reprobe.
        for streak in 1..UNREADABLE_RETRY_EVERY_TICKS - 1 {
            assert_eq!(unreadable_disposition(streak), Suppress, "streak {streak}");
        }
        assert_eq!(
            unreadable_disposition(UNREADABLE_RETRY_EVERY_TICKS - 1),
            ReinitNow
        );
        assert_eq!(
            unreadable_disposition(UNREADABLE_RETRY_EVERY_TICKS),
            Suppress
        );
    }

    #[test]
    fn missing_usb_node_records_detached_never_ready() {
        // One-shot (no hub): recorded only. Tick (live hub): single Detached
        // emit for clicks release.
        let gone: Result<bool> = Err(anyhow::anyhow!("no node"));
        let action = resume_usb_node("/nonexistent-hidraw-xyz", 110, None, &gone).unwrap();
        assert_eq!(action, ResumeAction::Detached);
        let hub = DeviceReadyHub::new();
        let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let capture = std::sync::Arc::clone(&seen);
        hub.subscribe(move |ev| capture.lock().expect("seen lock").push(ev.clone()));
        let action = resume_usb_node("/nonexistent-hidraw-xyz", 110, Some(&hub), &gone).unwrap();
        assert_eq!(action, ResumeAction::Detached);
        let seen = seen.lock().expect("seen lock");
        assert_eq!(seen.len(), 1);
        assert_eq!(seen[0].kind, ReadyKind::Detached);
    }

    #[test]
    fn scan_core_collects_per_node_errors() {
        // /dev/null exists but is not a trackpad: USB resume fails on
        // ioctl, BT re-arms fine — one node's failure never takes the
        // pass (or the sibling) down.
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
        let report = run_resume_scan(&devs, 110, false);
        assert_eq!(report.nodes.len(), 1);
        assert_eq!(report.nodes[0].action, ResumeAction::Reinitialized);
        assert_eq!(report.errors.len(), 1);
        assert!(report.failed());
    }
}
