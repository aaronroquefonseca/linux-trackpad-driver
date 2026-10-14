//! Mock hidraw/evdev test harness (task `tests-hw-mock`).
//!
//! Test-only fakes; no production code depends on this module
//! (wired as `#[cfg(test)]` in `main.rs`). All assertions go through the
//! existing public APIs in [`crate::init`] — this file adds no init logic,
//! no config schema, and no parser code.
//!
//! ## Coverage vs. gaps
//!
//! * `FakeHidrawDevice` implements [`crate::init::FeatureSender`], so
//!   [`crate::init::init_usb`], `init_usb_with_retry`, `disengage_usb`, and
//!   `disengage_usb_with_retry` run unmodified against scripted responses.
//! * `FakeHidrawDevice::verify` exercises the pure
//!   [`crate::init::reports_match`] helper with scripted `GET_FEATURE`
//!   buffers (match / NAK-mismatch / read-error).
//! * GAP: [`crate::init::send_feature_report_hidraw`],
//!   `get_feature_report_hidraw`, `verify_report`, and `send_with_retry` take
//!   a devnode path and issue real `ioctl`s — they cannot be injected without
//!   restructuring sibling-owned code, so they are NOT covered here.
//!   `FakeHidrawDevice::set_feature` mirrors their documented contract
//!   (`EPIPE`/`EIO`-after-mode-switch counts as success, everything else
//!   propagates; length guard `1..=4096`) and the EIO/NAK tests below pin
//!   that contract at the harness level. The real-ioctl paths need hardware
//!   or a future injectable-ioctl refactor (sibling owner decision).
//! * GAP: no production evdev/uinput code exists yet (Phase-0 is init-only),
//!   so `FakeEvdev` is a forward stub for the future `apple-parse` /
//!   `uinput-proxy` tasks. It binds to no public API today.

use std::cell::{Cell, RefCell};
use std::collections::{HashMap, VecDeque};

use anyhow::Result;

use crate::init::FeatureSender;

/// Queued `SET_FEATURE` outcome: `None` = success, `Some(errno)` = fail the
/// next [`FakeHidrawDevice::set_feature`] call with that errno.
pub type QueuedErrno = Option<i32>;

/// In-memory fake hidraw node: write log + scripted GET/SET responses.
///
/// * Every `set_feature` call appends to `writes` (attempts included, so a
///   whole-sequence retry shows one entry per attempt up to its failure).
/// * `EPIPE`/`EIO` are swallowed as mode-switch success (production parity
///   with `send_feature_report_hidraw`), counted in `eio_swallowed`.
/// * Any other errno propagates as `Err`, driving the caller's retry policy.
/// * `GET_FEATURE` returns the scripted buffer for the report ID, a scripted
///   read error when set, or a zero-filled `[report_id, 0, ..]` default.
#[derive(Debug, Default)]
pub struct FakeHidrawDevice {
    writes: RefCell<Vec<Vec<u8>>>,
    get_scripts: RefCell<HashMap<u8, Vec<u8>>>,
    get_errors: RefCell<HashMap<u8, i32>>,
    set_scripts: RefCell<VecDeque<QueuedErrno>>,
    eio_swallowed: Cell<usize>,
}

impl FakeHidrawDevice {
    /// Empty fake: no scripts, zero reads.
    pub fn new() -> Self {
        Self::default()
    }

    /// Script the next `SET_FEATURE` outcomes in order. `None` succeeds.
    pub fn script_set_errors(&self, outcomes: &[QueuedErrno]) {
        self.set_scripts
            .borrow_mut()
            .extend(outcomes.iter().copied());
    }

    /// Script a `GET_FEATURE` response buffer for `report_id`.
    pub fn script_get(&self, report_id: u8, response: Vec<u8>) {
        self.get_scripts.borrow_mut().insert(report_id, response);
    }

    /// Script a `GET_FEATURE` read failure (errno) for `report_id`.
    pub fn script_get_error(&self, report_id: u8, errno: i32) {
        self.get_errors.borrow_mut().insert(report_id, errno);
    }

    /// All `SET_FEATURE` attempts so far, in order.
    pub fn writes(&self) -> Vec<Vec<u8>> {
        self.writes.borrow().clone()
    }

    /// Most recent write, if any.
    pub fn last_write(&self) -> Option<Vec<u8>> {
        self.writes.borrow().last().cloned()
    }

    /// How many `EPIPE`/`EIO` outcomes were swallowed as success.
    pub fn eio_swallowed(&self) -> usize {
        self.eio_swallowed.get()
    }

    /// Fake `HIDIOCSFEATURE`: log + scripted outcome, with production-parity
    /// handling (`EPIPE`/`EIO` → `Ok`, other errnos → `Err`, length guard).
    pub fn set_feature(&self, report: &[u8]) -> Result<()> {
        if report.is_empty() || report.len() > 4096 {
            anyhow::bail!("refusing feature report of length {}", report.len());
        }
        self.writes.borrow_mut().push(report.to_vec());
        let next = self.set_scripts.borrow_mut().pop_front().flatten();
        match next {
            None => Ok(()),
            Some(errno) if errno == libc::EPIPE || errno == libc::EIO => {
                log::warn!(
                    "fake {report:02X?} returned errno {errno} — mode-switch success (production parity)"
                );
                self.eio_swallowed.set(self.eio_swallowed.get() + 1);
                Ok(())
            }
            Some(errno) => Err(std::io::Error::from_raw_os_error(errno)).map_err(|e| {
                anyhow::Error::from(e).context(format!("fake SET_FEATURE errno {errno}"))
            }),
        }
    }

    /// Fake `HIDIOCGFEATURE`: scripted buffer / scripted error / zero default.
    pub fn get_feature(&self, report_id: u8, len: usize) -> Result<Vec<u8>> {
        if len == 0 || len > 4096 {
            anyhow::bail!("refusing feature read of length {len}");
        }
        if let Some(errno) = self.get_errors.borrow().get(&report_id).copied() {
            return Err(std::io::Error::from_raw_os_error(errno))
                .map_err(|e| anyhow::Error::from(e).context("fake GET_FEATURE error"));
        }
        if let Some(buf) = self.get_scripts.borrow().get(&report_id).cloned() {
            return Ok(buf);
        }
        let mut buf = vec![0u8; len];
        buf[0] = report_id;
        Ok(buf)
    }

    /// Fake verify-after-write through the real [`crate::init::reports_match`].
    pub fn verify(&self, expected: &[u8]) -> Result<bool> {
        if expected.is_empty() {
            anyhow::bail!("cannot verify an empty report");
        }
        let got = self.get_feature(expected[0], expected.len())?;
        Ok(crate::init::reports_match(&got, expected))
    }
}

impl FeatureSender for FakeHidrawDevice {
    fn send(&self, report: &[u8]) -> Result<()> {
        self.set_feature(report)
    }
}

/// Minimal fake evdev node (forward stub — no production evdev API exists yet).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FakeInputEvent {
    pub ev_type: u16,
    pub code: u16,
    pub value: i32,
}

#[derive(Debug, Default)]
pub struct FakeEvdev {
    grabbed: Cell<bool>,
    events: RefCell<Vec<FakeInputEvent>>,
    grab_log: RefCell<Vec<bool>>,
}

impl FakeEvdev {
    /// Ungrabbed fake with an empty event log.
    pub fn new() -> Self {
        Self::default()
    }

    /// Grab (exclusive) — records the transition.
    pub fn grab(&self) {
        self.grabbed.set(true);
        self.grab_log.borrow_mut().push(true);
    }

    /// Release the grab — records the transition.
    pub fn ungrab(&self) {
        self.grabbed.set(false);
        self.grab_log.borrow_mut().push(false);
    }

    /// Currently grabbed.
    pub fn is_grabbed(&self) -> bool {
        self.grabbed.get()
    }

    /// Emit one input event into the log.
    pub fn emit(&self, ev_type: u16, code: u16, value: i32) {
        self.events.borrow_mut().push(FakeInputEvent {
            ev_type,
            code,
            value,
        });
    }

    /// Emitted events in order.
    pub fn events(&self) -> Vec<FakeInputEvent> {
        self.events.borrow().clone()
    }

    /// Grab/ungrab transitions (`true` = grab) in order.
    pub fn grab_transitions(&self) -> Vec<bool> {
        self.grab_log.borrow().clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::init::{disengage_usb, init_usb, EFFECTIVE_CAP_HZ, MIN_X_FOR_CAP, POLLING_BASE_HZ};

    // ---- polling-cap posture (task item b) ----

    #[test]
    fn default_request_is_capped_0x23() {
        let plan = crate::init::resolve_polling(crate::config::DEFAULT_POLLING_HZ).unwrap();
        assert_eq!(plan.requested_hz, 110);
        assert_eq!(plan.x, 0x23);
        assert_eq!(plan.x, MIN_X_FOR_CAP);
        assert_eq!(crate::init::polling_report(plan.x), [0x99, 0xF2, 0x23]);

        // Same bytes reach the fake wire through the public init path.
        let fake = FakeHidrawDevice::new();
        let plan = init_usb(&fake, crate::config::DEFAULT_POLLING_HZ).unwrap();
        assert_eq!(plan.x, 0x23);
        assert_eq!(
            fake.writes(),
            vec![vec![0x02, 0x01], vec![0x21, 0x01], vec![0x99, 0xF2, 0x23]]
        );
    }

    #[test]
    fn above_cap_never_reaches_wire() {
        for requested in [111, 200, 500, 1000, 8000, u16::MAX as u32] {
            let plan = crate::init::resolve_polling(requested).unwrap();
            assert_eq!(plan.x, MIN_X_FOR_CAP, "requested {requested}");
            assert!(plan.effective_hz <= EFFECTIVE_CAP_HZ + 1e-9);

            let fake = FakeHidrawDevice::new();
            let plan = init_usb(&fake, requested).unwrap();
            assert_eq!(fake.last_write().unwrap(), vec![0x99, 0xF2, 0x23]);
            assert_eq!(plan.x, 0x23);
        }
    }

    #[test]
    fn x_below_35_never_on_wire() {
        // Below the device floor (~16 Hz, X saturates at 255) requests are
        // rejected, not silently rounded up: pin the rejection.
        for requested in 1..crate::init::MIN_EXPRESSIBLE_HZ {
            assert!(
                crate::init::resolve_polling(requested).is_err(),
                "requested {requested} must be rejected (below floor)"
            );
        }
        // Pure mapping sweep: every representable request stays >= 35.
        for requested in crate::init::MIN_EXPRESSIBLE_HZ..=u16::MAX as u32 {
            let plan = crate::init::resolve_polling(requested).unwrap();
            assert!(
                plan.x >= MIN_X_FOR_CAP,
                "requested {requested} -> X={}",
                plan.x
            );
            assert!(plan.x >= 35);
        }
        // Wire sweep over representative rates through the public init path.
        for requested in [16, 30, 60, 90, 100, 110, 111, 200, 500, 65535] {
            let fake = FakeHidrawDevice::new();
            init_usb(&fake, requested).unwrap();
            let wire = fake.last_write().unwrap();
            assert_eq!(&wire[..2], &[0x99, 0xF2]);
            assert!(
                wire[2] >= 35,
                "requested {requested} put X={} on wire",
                wire[2]
            );
        }
    }

    #[test]
    fn mapping_table_3850_over_x() {
        // (requested Hz, expected X); X = round(3850/capped), floor 35.
        let table: &[(u32, u8)] = &[
            (110, 35), // 3850/110 = 35.0 exactly → cap floor
            (100, 39), // round(38.5) = 39
            (90, 43),  // round(42.78) = 43
            (60, 64),  // round(64.17) = 64
            (45, 86),  // round(85.56) = 86
            (30, 128), // round(128.33) = 128
        ];
        for &(requested, expected_x) in table {
            let plan = crate::init::resolve_polling(requested).unwrap();
            assert_eq!(plan.x, expected_x, "requested {requested}");
            let theoretical = POLLING_BASE_HZ / f64::from(expected_x);
            assert!(
                (plan.theoretical_hz - theoretical).abs() < 1e-9,
                "requested {requested}"
            );
            assert!(
                (plan.effective_hz - theoretical.min(EFFECTIVE_CAP_HZ)).abs() < 1e-9,
                "requested {requested}"
            );
            assert_eq!(crate::init::polling_report(plan.x)[2], expected_x);
        }
    }

    // ---- harness: write log + scripted GET/SET ----

    #[test]
    fn write_log_records_usb_order() {
        let fake = FakeHidrawDevice::new();
        init_usb(&fake, 110).unwrap();
        assert_eq!(
            fake.writes(),
            vec![vec![0x02, 0x01], vec![0x21, 0x01], vec![0x99, 0xF2, 0x23]]
        );
    }

    #[test]
    fn disengage_logs_host_click_off_only() {
        let fake = FakeHidrawDevice::new();
        disengage_usb(&fake).unwrap();
        assert_eq!(fake.writes(), vec![vec![0x21, 0x00]]);
    }

    #[test]
    fn scripted_get_verify_match_and_nak_mismatch() {
        let fake = FakeHidrawDevice::new();
        // Exact + prefix match → true (production `reports_match` semantics).
        fake.script_get(0x99, vec![0x99, 0xF2, 0x23]);
        assert!(fake.verify(&[0x99, 0xF2, 0x23]).unwrap());
        fake.script_get(0x02, vec![0x02, 0x01, 0xFF]);
        assert!(fake.verify(&[0x02, 0x01]).unwrap());
        // NAK-style wrong payload → Ok(false), not Err.
        fake.script_get(0x02, vec![0x02, 0x00]);
        assert!(!fake.verify(&[0x02, 0x01]).unwrap());
        // Short read → Ok(false).
        fake.script_get(0x02, vec![0x02]);
        assert!(!fake.verify(&[0x02, 0x01]).unwrap());
    }

    #[test]
    fn scripted_get_error_propagates() {
        let fake = FakeHidrawDevice::new();
        fake.script_get_error(0x99, libc::EIO);
        assert!(fake.verify(&[0x99, 0xF2, 0x23]).is_err());
    }

    #[test]
    fn set_nak_fails_and_whole_sequence_retries() {
        // Persistent NAK (EINVAL, not EPIPE/EIO): init fails on the first
        // report. EINVAL is permanent (bad length / wrong node), so both the
        // per-report and whole-sequence layers fail fast — one logged attempt.
        let fake = FakeHidrawDevice::new();
        fake.script_set_errors(&[Some(libc::EINVAL), Some(libc::EINVAL), Some(libc::EINVAL)]);
        assert!(init_usb(&fake, 110).is_err());
        assert_eq!(fake.writes().len(), 1);

        let fake = FakeHidrawDevice::new();
        fake.script_set_errors(&[Some(libc::EINVAL), Some(libc::EINVAL), Some(libc::EINVAL)]);
        let err = crate::init::init_usb_with_retry(&fake, 110).unwrap_err();
        assert!(format!("{err:#}").contains("1 attempt(s)"));
        assert_eq!(fake.writes().len(), 1);
        assert_eq!(fake.eio_swallowed(), 0);
    }

    #[test]
    fn set_persistent_transient_retries_per_report() {
        // Persistent transient (ENOENT, node mid-bind): per-report backoff
        // exhausts all attempts, then propagates.
        let fake = FakeHidrawDevice::new();
        fake.script_set_errors(&[Some(libc::ENOENT); 6]);
        let err = crate::init::send_with_retry_on(
            &fake,
            &[0x02, 0x01],
            crate::init::DEFAULT_SEND_ATTEMPTS,
            "fake",
        )
        .unwrap_err();
        assert!(format!("{err:#}").contains("5 attempt(s)"));
        assert_eq!(fake.writes().len(), 5);
    }

    #[test]
    fn set_transient_nak_recovers_on_retry() {
        // First attempt hits ENOENT (node mid-bind) on the opening report;
        // second attempt runs clean.
        let fake = FakeHidrawDevice::new();
        fake.script_set_errors(&[Some(libc::ENOENT), None, None, None]);
        let plan = crate::init::init_usb_with_retry(&fake, 110).unwrap();
        assert_eq!(plan.x, 0x23);
        assert_eq!(fake.writes().len(), 4); // 1 failed + 3 clean
    }

    #[test]
    fn set_eio_after_switch_counts_as_success() {
        for errno in [libc::EIO, libc::EPIPE] {
            let fake = FakeHidrawDevice::new();
            fake.script_set_errors(&[Some(errno)]);
            let plan = init_usb(&fake, 110).unwrap();
            assert_eq!(plan.x, 0x23);
            assert_eq!(fake.writes().len(), 3);
            assert_eq!(fake.eio_swallowed(), 1);
        }
    }

    #[test]
    fn set_length_guard_matches_production() {
        let fake = FakeHidrawDevice::new();
        assert!(fake.set_feature(&[]).is_err());
        assert!(fake.get_feature(0x99, 0).is_err());
        assert!(fake.verify(&[]).is_err());
    }

    // ---- harness: fake evdev stub ----

    #[test]
    fn evdev_grab_emit_order() {
        let ev = FakeEvdev::new();
        assert!(!ev.is_grabbed());
        ev.grab();
        ev.emit(3, 47, 1); // EV_ABS / ABS_MT_SLOT
        ev.emit(1, 272, 1); // EV_KEY / BTN_LEFT
        ev.ungrab();
        assert!(!ev.is_grabbed());
        assert_eq!(ev.grab_transitions(), vec![true, false]);
        assert_eq!(
            ev.events(),
            vec![
                FakeInputEvent {
                    ev_type: 3,
                    code: 47,
                    value: 1
                },
                FakeInputEvent {
                    ev_type: 1,
                    code: 272,
                    value: 1
                },
            ]
        );
    }
}
