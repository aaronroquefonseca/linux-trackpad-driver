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
//!   without sleep hooks (USB: re-verify polling via `GET_FEATURE`, re-init on
//!   mismatch; BT: no `GET_FEATURE` equivalent, so the tick re-sends the
//!   idempotent BT init pair best-effort on the same cadence).
//!
//! Clicks contract: re-init emits `Ready` on the [`DeviceReadyHub`] *after*
//! the sequence succeeds, so the clicks pipeline re-attaches without ever
//! reordering init. The daemon also retries missing/dead enhanced sessions
//! without repeating hardware init, even when polling matches or BT keeps
//! its name. Those emits only happen on a live daemon hub: the one-shot
//! owns no listeners. Cross-process re-attach starts on a later daemon
//! pass: kernel hotplug events wake discovery promptly with bounded readiness
//! retries; [`RESUME_REVERIFY_INTERVAL_SECS`] remains the full verification
//! and missed-event fallback. Init/attachment work adds to scheduling time.
//! A node that vanished emits `Detached` instead, never `Ready`.

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
/// Bounded post-resume rescan wait when the first scan sees no trackpad:
/// the hook can fire before the node re-enumerates, and a single scan would
/// then record a silent no-op. The resume entry rescans until this budget
/// runs out; the daemon tick backstops anything slower.
pub const RESUME_RESCAN_TIMEOUT_MS: u64 = 5000;
/// Interval between resume rescan passes.
pub const RESUME_RESCAN_INTERVAL_MS: u64 = 100;

/// Outcome of one resume pass for one node.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResumeAction {
    /// USB polling read-back matched; BT present but the best-effort re-send
    /// failed (warned, passthrough retained). No hub traffic.
    UpToDate,
    /// USB mismatch (or unreadable verify) → multitouch-first re-init sent;
    /// BT node present → init pair re-sent best-effort. Hub `Ready` is
    /// emitted only on the USB daemon-tick arm with a live hub (the one-shot
    /// passes none — its hub would reach zero listeners); the BT arm never
    /// emits, so a periodic re-send never churns a live grab.
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
        log::warn!(
            "resume: node {devnode} gone after suspend; recording Detached (tick adopts it within one pass if it re-enumerates under a new name)"
        );
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

/// Re-arm one BT node after resume: liveness check, then a best-effort
/// re-send of the BT init pair (`F1 02 01` multitouch + `F2 21 01`
/// host-click ON) via [`init::send_bt_init_hidraw`].
///
/// Why (RESEARCH H3/H5): after a BT dropout (BlueZ suspend, renegotiated
/// connection interval, re-pair) the device can sit in basic-mouse mode
/// with no re-arm — dead multitouch/force/middle until daemon restart.
/// There is no BT `GET_FEATURE` equivalent (USB-first), so — unlike
/// [`resume_usb_node`] — this path cannot verify first and re-sends
/// unconditionally. The pair is idempotent (same two bytes every time),
/// so a spurious pass is harmless.
///
/// Best-effort: a send failure warns and falls through to `UpToDate`
/// (passthrough stays live; the pass never fails). A vanished node is
/// recorded `Detached`, mirroring the USB arm. Emits nothing in any mode:
/// the one-shot owns no hub, and the daemon tick deliberately adds no
/// `Ready` here — clicks re-attach flows only through init/attach paths.
pub fn resume_bt_node(devnode: &str) -> ResumeAction {
    resume_bt_node_inner(devnode, &init::send_bt_init_hidraw)
}

/// [`resume_bt_node`] with an injectable send: production passes
/// [`init::send_bt_init_hidraw`]; unit tests drive a [`init::Recorder`]
/// (or a failing sender) without hardware.
fn resume_bt_node_inner(devnode: &str, send: &dyn Fn(&str) -> Result<()>) -> ResumeAction {
    if !std::path::Path::new(devnode).exists() {
        log::warn!(
            "resume: BT node {devnode} gone after suspend; recording Detached (tick adopts it within one pass if it re-enumerates under a new name)"
        );
        return ResumeAction::Detached;
    }
    match send(devnode) {
        Ok(()) => {
            log::info!("resume: BT {devnode} re-sent init (F1 02 01 + F2 21 01)");
            ResumeAction::Reinitialized
        }
        Err(e) => {
            log::warn!(
                "resume: BT {devnode} re-init best-effort failed ({e:#}); continuing (passthrough still live)"
            );
            ResumeAction::UpToDate
        }
    }
}

/// Exact recovery command named by every resume dead-end log: no path may
/// require a manual `systemctl restart` without saying so up front.
/// `-f` forces re-init even while the daemon is stopped (plain `resume`
/// without a live daemon is an inactive no-op by the activation contract).
pub fn empty_scan_recovery_hint() -> &'static str {
    "retry with: sudo systemctl start linux-trackpad-driver-resume.service (or: sudo ltd resume -f)"
}

/// Pure scan diff by devnode: renumbers (hidraw2→hidraw3) read as one
/// removal plus one addition, so the tick's remove path (`Detached`) and
/// add path (full init) together self-heal within one tick. An empty
/// known-set (daemon restarted mid-sleep) adopts the whole scan.
pub fn diff_scan(
    known: &[DiscoveredDevice],
    current: &[DiscoveredDevice],
) -> (Vec<DiscoveredDevice>, Vec<DiscoveredDevice>) {
    let added: Vec<DiscoveredDevice> = current
        .iter()
        .filter(|d| !known.iter().any(|k| k.devnode == d.devnode))
        .cloned()
        .collect();
    let removed: Vec<DiscoveredDevice> = known
        .iter()
        .filter(|k| !current.iter().any(|d| d.devnode == k.devnode))
        .cloned()
        .collect();
    (added, removed)
}

/// Bounded rescan for the resume entry: the first scan may run before the
/// node re-enumerates. Returns the first non-empty scan, or the last (empty)
/// one once `timeout_ms` runs out. The scanner is a parameter so unit tests
/// cover the late-node path without hardware.
pub fn scan_with_rescan(
    scanner: &dyn Fn() -> Vec<DiscoveredDevice>,
    timeout_ms: u64,
    interval_ms: u64,
) -> Vec<DiscoveredDevice> {
    let waited = std::time::Instant::now();
    let budget = std::time::Duration::from_millis(timeout_ms);
    let mut devs = scanner();
    while devs.is_empty() && waited.elapsed() < budget {
        std::thread::sleep(std::time::Duration::from_millis(interval_ms));
        devs = scanner();
    }
    devs
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
/// Single scan, no rescan loop (the caller rescans: see
/// [`scan_with_rescan`]): each node gets one bounded
/// [`RESUME_READY_TIMEOUT_MS`] wait, then verify-or-reinit. A node that
/// vanishes mid-pass is recorded `Detached`; the daemon tick reconciles any
/// skew on its next pass. Hook, tick, and udev interleave benignly: every
/// path sends the same idempotent multitouch-first sequence.
///
/// No automatic USB unbind/bind cycle: feature re-send is the demonstrated
/// fix, and re-enumeration is destructive (fail-closed posture). A node that
/// stays mismatched after the retry budget lands in `errors` with the exact
/// recovery command; the tick keeps re-trying so a late recovery self-heals.
pub fn run_resume_scan(devs: &[DiscoveredDevice], polling_hz: u32) -> ResumeReport {
    log::info!("resume: starting pass over {} node(s)", devs.len());
    let mut report = ResumeReport::default();
    for dev in devs {
        let outcome = match dev.transport {
            Transport::Usb => {
                let first_verify = verify_node_polling(&dev.devnode, polling_hz);
                resume_usb_node(&dev.devnode, polling_hz, None, &first_verify)
            }
            Transport::Bluetooth => Ok(resume_bt_node(&dev.devnode)),
        };
        match outcome {
            Ok(action) => report.nodes.push(NodeResume {
                devnode: dev.devnode.clone(),
                action,
            }),
            Err(e) => {
                log::error!(
                    "resume: re-init failed for {}: {e:#}; {}",
                    dev.devnode,
                    empty_scan_recovery_hint()
                );
                report.errors.push(format!(
                    "{}: {e:#}; {}",
                    dev.devnode,
                    empty_scan_recovery_hint()
                ));
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
    fn reverify_interval_matches_install_claim() {
        // INSTALL.md promises a 30 s daemon tick; pin the constant so the
        // doc cannot drift silently.
        assert_eq!(RESUME_REVERIFY_INTERVAL_SECS, 30);
    }

    #[test]
    fn expected_polling_pins_wire_bytes() {
        assert_eq!(expected_polling(110).unwrap(), [153, 242, 0x23]);
        assert!(expected_polling(0).is_err());
        assert!(expected_polling(1).is_err());
    }

    #[test]
    fn bt_resend_uses_pr1_pair_bytes() {
        // The re-send reuses the PR1 helper bytes exactly: multitouch first,
        // then host-click ON — no duplicated report literals here.
        let node = tempfile::NamedTempFile::new().unwrap();
        let path = node.path().to_str().unwrap().to_string();
        let rec = init::Recorder::default();
        let action = resume_bt_node_inner(&path, &|_: &str| init::send_bt_init(&rec));
        assert_eq!(action, ResumeAction::Reinitialized);
        assert_eq!(
            *rec.sent.borrow(),
            vec![vec![0xF1, 0x02, 0x01], vec![0xF2, 0x21, 0x01]]
        );
    }

    #[test]
    fn bt_resend_failure_warns_and_continues() {
        // Best-effort: a failed send is `UpToDate` (passthrough retained),
        // never an error — the pass must not fail for a sibling.
        let node = tempfile::NamedTempFile::new().unwrap();
        let path = node.path().to_str().unwrap().to_string();
        let action = resume_bt_node_inner(&path, &|_: &str| -> Result<()> {
            Err(anyhow::anyhow!("no hardware"))
        });
        assert_eq!(action, ResumeAction::UpToDate);
    }

    #[test]
    fn bt_missing_node_never_sends() {
        let calls = std::cell::Cell::new(0);
        let action = resume_bt_node_inner("/nonexistent-hidraw-bt-xyz", &|_: &str| -> Result<()> {
            calls.set(calls.get() + 1);
            Ok(())
        });
        assert_eq!(action, ResumeAction::Detached);
        assert_eq!(calls.get(), 0);
    }

    #[test]
    fn bt_oneshot_best_effort_never_fails() {
        // Real-FS arm: a present-but-not-hidraw node fails the best-effort
        // send (warn, `UpToDate`), a vanished node records `Detached`.
        // Either way the one-shot never errors.
        let node = tempfile::NamedTempFile::new().unwrap();
        let path = node.path().to_str().unwrap().to_string();
        assert_eq!(resume_bt_node(&path), ResumeAction::UpToDate);
        drop(node);
        assert_eq!(resume_bt_node(&path), ResumeAction::Detached);
    }

    #[test]
    fn bt_resume_scan_without_daemon_best_effort() {
        let node = tempfile::NamedTempFile::new().unwrap();
        let dev = DiscoveredDevice {
            devnode: node.path().to_str().unwrap().into(),
            vendor_id: 0x004c,
            product_id: 0x0324,
            transport: Transport::Bluetooth,
        };
        // This is the core used by the no-daemon resume entry. Invalid USB
        // polling cannot affect BT; the helper owns no enhanced session.
        // The non-hidraw tempfile fails the best-effort send → warned
        // `UpToDate`, never a pass failure.
        let report = run_resume_scan(&[dev], 0);
        assert_eq!(report.nodes.len(), 1);
        assert_eq!(report.nodes[0].action, ResumeAction::UpToDate);
        assert_eq!(report.reinitialized(), 0);
        assert!(!report.failed());
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

    fn usb_node(devnode: &str) -> DiscoveredDevice {
        DiscoveredDevice {
            devnode: devnode.into(),
            vendor_id: 0x05ac,
            product_id: 0x0324,
            transport: Transport::Usb,
        }
    }

    #[test]
    fn renumber_is_add_plus_remove_never_stuck() {
        // (a) hidraw2→hidraw3 across sleep: old name must read as removed
        // (Detached path) AND the new node as added (adopt path). If either
        // side were missing the trackpad would need a manual restart.
        let known = vec![usb_node("/dev/hidraw2")];
        let current = vec![usb_node("/dev/hidraw3")];
        let (added, removed) = diff_scan(&known, &current);
        assert_eq!(added, vec![usb_node("/dev/hidraw3")]);
        assert_eq!(removed, vec![usb_node("/dev/hidraw2")]);
    }

    #[test]
    fn empty_known_rebuilds_from_scan() {
        // (c) daemon restarted by systemd mid-sleep loses its known-set;
        // a tick built on an empty snapshot must adopt everything scanned.
        let (added, removed) = diff_scan(&[], &[usb_node("/dev/hidraw2")]);
        assert_eq!(added, vec![usb_node("/dev/hidraw2")]);
        assert!(removed.is_empty());
        let (added, removed) = diff_scan(&[usb_node("/dev/hidraw2")], &[usb_node("/dev/hidraw2")]);
        assert!(added.is_empty());
        assert!(removed.is_empty());
    }

    #[test]
    fn rescan_adopts_late_enumerating_node() {
        // (b) hook fired before the node re-enumerated: a single scan sees
        // nothing and would record a silent no-op. Rescan must pick the
        // late node up within the bounded wait.
        let calls = std::cell::Cell::new(0);
        let scanned = scan_with_rescan(
            &|| {
                calls.set(calls.get() + 1);
                if calls.get() < 3 {
                    Vec::new()
                } else {
                    vec![usb_node("/dev/hidraw2")]
                }
            },
            5000,
            10,
        );
        assert_eq!(scanned, vec![usb_node("/dev/hidraw2")]);
    }

    #[test]
    fn rescan_gives_up_bounded_on_still_empty() {
        let scanned = scan_with_rescan(&Vec::new, 40, 10);
        assert!(scanned.is_empty());
    }

    #[test]
    fn empty_scan_hint_names_recovery_command() {
        // Every dead-end names its recovery: no silent `Detached`/empty pass.
        let hint = empty_scan_recovery_hint();
        assert!(
            hint.contains("systemctl start linux-trackpad-driver-resume.service"),
            "hint must name the resume unit: {hint}"
        );
        assert!(
            hint.contains("ltd resume"),
            "hint must name ltd resume: {hint}"
        );
    }

    #[test]
    fn scan_core_collects_per_node_errors() {
        // /dev/null exists but is not a trackpad: USB resume fails on
        // ioctl, BT fails its best-effort send (warned `UpToDate`) — one
        // node's failure never takes the pass (or the sibling) down.
        let devs = vec![
            DiscoveredDevice {
                devnode: "/dev/null".into(),
                vendor_id: 0x05ac,
                product_id: 0x0324,
                transport: Transport::Usb,
            },
            DiscoveredDevice {
                devnode: "/dev/null".into(),
                vendor_id: 0x004c,
                product_id: 0x0324,
                transport: Transport::Bluetooth,
            },
        ];
        let report = run_resume_scan(&devs, 110);
        assert_eq!(report.nodes.len(), 1);
        assert_eq!(report.nodes[0].action, ResumeAction::UpToDate);
        assert_eq!(report.errors.len(), 1);
        assert!(report.failed());
    }
}
