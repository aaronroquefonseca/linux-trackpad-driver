//! USB/BT core-init. Zero dependencies on clicks code.
//!
//! USB sequence (via hidraw `HIDIOCSFEATURE`, buf[0] = report ID):
//!   1. multitouch enable  `02 01`
//!   2. host-click enable  `21 01`
//!   3. polling interval   `99 F2 X`  (0x99, 0xF2; third byte is `X`)
//!
//! Polling posture (corrected): default request is the capped device maximum
//! ~110 Hz (`X = 0x23 = 35`, `3850/35 ≈ 110`). Never request above the
//! demonstrated ceiling (e.g. never 500 Hz): independent USB-C measurement
//! confirms `99 F2 XX` with freq ≈ `3850/XX`, max demonstrated `XX = 0x23` →
//! ~111 Hz; divisor values below 35 (`X < 35`) are unverified and the device
//! may NAK/ignore them. `resolve_polling` therefore clamps the *request* to
//! 110 Hz first, then maps to `X >= 35`. A user `-p` above 110 is clamped the
//! same way (defense in depth), never sent to hardware.
//! Sources: benjaminwestern/linux-apple-trackpad,
//! mr-cal/Linux-Magic-Trackpad-2-USB-C-Driver#26.
//!
//! Ordering (hidraw/probe race): the kernel `hid-magicmouse` probe sends its
//! own init, so userspace `HIDIOCSFEATURE` at `add` time can race it. Callers
//! must wait for the hidraw node + driver bind, then send with backoff retry
//! and verify via `HIDIOCGFEATURE` ([`send_with_retry`], [`verify_report`]).
//! Re-apply on resume: Trackpad 2 USB drops out of multitouch mode across
//! power transition (report 0x02 shrinks 21→8 bytes; clicks survive, motion
//! lost until re-init), so the resume hook MUST re-send multitouch enable —
//! [`init_usb`] always sends it first and is idempotent.
//!
//! Bluetooth path: single node, no init dance — straight to device-ready
//! (bring-up + init verification scope). Note: hidraw coexists with
//! `hid-magicmouse` on BT (no exclusivity); the kernel keeps emitting evdev
//! unless a later Phase-1 step unbinds the driver for that VID:PID or grabs
//! the physical node behind the opt-in flag — else double-input. No known BT
//! polling-rate equivalent; rate tuning is USB-first.
//!
//! Clicks integration: [`DeviceReadyHub`] is the stub hook the future clicks
//! pipeline will subscribe to. Init never calls into clicks; it only emits.
//! Downstream contract note (Phase-1, not implemented here): the proxy node
//! must emit `res == 0`, arbitrary 0..253 range, Type-B slots + tracking IDs,
//! and must NOT claim grams/newtons or advertise `EV_FF`/`FF_HAPTIC`.

use std::cell::RefCell;
use std::os::unix::io::AsRawFd;
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};

/// USB multitouch-enable feature report.
pub const MULTITOUCH_USB: &[u8] = &[0x02, 0x01];
/// USB host-click enable (`21 01`) / disable (`21 00`).
pub const HOST_CLICK_USB_ON: &[u8] = &[0x21, 0x01];
pub const HOST_CLICK_USB_OFF: &[u8] = &[0x21, 0x00];
/// Polling report prefix bytes: 153 == 0x99, 242 == 0xF2; third byte is `X`.
pub const POLLING_PREFIX: [u8; 2] = [153, 242];

/// Numerator of the polling formula `effective_hz = BASE / X`.
pub const POLLING_BASE_HZ: f64 = 3850.0;
/// Demonstrated device ceiling; requests above this are clamped, never sent.
/// Max demonstrated `X = 0x23 = 35` → ~111 Hz (benjaminwestern measurement).
pub const EFFECTIVE_CAP_HZ: f64 = 110.0;
/// Minimum `X` ever placed on the wire (`0x23 = 35`). Values below 35 are
/// unverified and may be NAKed/ignored by the device.
pub const MIN_X_FOR_CAP: u8 = 0x23;
/// Lowest rate the `X` byte can express (`3850/255 ≈ 15.1 Hz`). Requests
/// below this are rejected instead of silently rounding up ~15×.
pub const MIN_EXPRESSIBLE_HZ: u32 = 16;

/// Transport of a discovered device.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Transport {
    Usb,
    Bluetooth,
}

/// Resolved polling decision: what was asked vs. what was sent.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PollingPlan {
    /// Requested Hz (config or `-p` override).
    pub requested_hz: u32,
    /// `X` byte actually sent (`>= 0x23` after cap clamp).
    pub x: u8,
    /// Formula output `3850/raw_x` for the raw request, before the device
    /// ceiling (`X >= 0x23`, `~110 Hz`) is applied — i.e. what the request
    /// *means*, even when the wire byte is clamped.
    pub theoretical_hz: f64,
    /// Output after honest cap (`min(3850/X, 110)`).
    pub effective_hz: f64,
}

/// Map a requested rate to the `X` byte. The request is capped at
/// [`EFFECTIVE_CAP_HZ`] (~110 Hz) *before* mapping, so `X >= 0x23` always;
/// values that would imply `X < 35` are never sent (unverified, may NAK).
///
/// Errors on `0` or out-of-`u16` values (already validated at CLI/config
/// layer, but checked here so the function is total on its own).
pub fn resolve_polling(requested_hz: u32) -> Result<PollingPlan> {
    if requested_hz == 0 || requested_hz > u16::MAX as u32 {
        anyhow::bail!("invalid polling rate {requested_hz} (want 1..=65535)");
    }
    if requested_hz < MIN_EXPRESSIBLE_HZ {
        anyhow::bail!(
            "polling rate {requested_hz} Hz below device floor (~{MIN_EXPRESSIBLE_HZ} Hz; X saturates at 255)"
        );
    }
    let raw_x = (POLLING_BASE_HZ / f64::from(requested_hz)).round() as u32;
    let raw_x = raw_x.clamp(1, 255) as u8;
    let theoretical_hz = POLLING_BASE_HZ / f64::from(raw_x);
    let x = raw_x.max(MIN_X_FOR_CAP);
    let effective_hz = (POLLING_BASE_HZ / f64::from(x)).min(EFFECTIVE_CAP_HZ);
    Ok(PollingPlan {
        requested_hz,
        x,
        theoretical_hz,
        effective_hz,
    })
}

/// Build the 3-byte polling feature report for a resolved `X`.
pub fn polling_report(x: u8) -> [u8; 3] {
    [POLLING_PREFIX[0], POLLING_PREFIX[1], x]
}

/// Direction bits for both hidraw feature ioctls: the kernel defines them as
/// `_IOC(_IOC_WRITE|_IOC_READ, 'H', ...)` (see `linux/hidraw.h`:
/// `HIDIOCSFEATURE` nr 0x06, `HIDIOCGFEATURE` nr 0x07), so dir = 1|2 = 3.
/// A dir of 1 or 2 encodes a *different* ioctl number and fails with ENOTTY.
const HIDIOC_DIR: u64 = 3;
const HIDIOC_TYPE: u64 = 0x48;

/// Compute the `HIDIOCSFEATURE(len)` ioctl request code (Linux, nr `0x06`).
/// Pure so tests can pin the encoding.
pub fn hidiocsfeature_request(len: usize) -> u64 {
    const NR: u64 = 0x06;
    (HIDIOC_DIR << 30) | ((len as u64) << 16) | (HIDIOC_TYPE << 8) | NR
}

/// Send one feature report to a hidraw node via `HIDIOCSFEATURE`.
///
/// `EPIPE`/`EIO` after a mode-switch report are treated as success-with-log
/// (kernel precedent: device drops the node mid-switch; re-init on
/// replug/resume retries). All other errors propagate.
pub fn send_feature_report_hidraw(devnode: &str, report: &[u8]) -> Result<()> {
    if report.is_empty() || report.len() > 4096 {
        anyhow::bail!("refusing feature report of length {}", report.len());
    }
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(devnode)
        .with_context(|| format!("opening hidraw node '{devnode}'"))?;
    let req = hidiocsfeature_request(report.len());
    // Kernel copies `len` bytes from the buffer; buf[0] is the report ID.
    let ret = unsafe {
        libc::ioctl(
            file.as_raw_fd(),
            req as libc::c_ulong,
            report.as_ptr() as *const libc::c_void,
        )
    };
    if ret < 0 {
        let err = std::io::Error::last_os_error();
        let raw = err.raw_os_error().unwrap_or(0);
        if raw == libc::EPIPE || raw == libc::EIO {
            log::warn!(
                "feature report {:02X?} on {devnode} returned {} — treating as mode-switch success",
                report,
                err
            );
            return Ok(());
        }
        return Err(err).with_context(|| format!("HIDIOCSFEATURE on '{devnode}'"));
    }
    log::debug!("feature report {:02X?} sent to {devnode}", report);
    Ok(())
}

/// Compute the `HIDIOCGFEATURE(len)` ioctl request code (Linux, nr `0x07`).
/// Pure so tests can pin the encoding.
pub fn hidiocgfeature_request(len: usize) -> u64 {
    const NR: u64 = 0x07;
    (HIDIOC_DIR << 30) | ((len as u64) << 16) | (HIDIOC_TYPE << 8) | NR
}

/// Read back a feature report via `HIDIOCGFEATURE` for verify-after-write.
/// `report_id` goes in `buf[0]`; returns the raw bytes as the kernel gave them.
pub fn get_feature_report_hidraw(devnode: &str, report_id: u8, len: usize) -> Result<Vec<u8>> {
    if len == 0 || len > 4096 {
        anyhow::bail!("refusing feature read of length {len}");
    }
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(devnode)
        .with_context(|| format!("opening hidraw node '{devnode}'"))?;
    let mut buf = vec![0u8; len];
    buf[0] = report_id;
    let req = hidiocgfeature_request(len);
    let ret = unsafe {
        libc::ioctl(
            file.as_raw_fd(),
            req as libc::c_ulong,
            buf.as_mut_ptr() as *mut libc::c_void,
        )
    };
    if ret < 0 {
        let err = std::io::Error::last_os_error();
        return Err(err).with_context(|| format!("HIDIOCGFEATURE on '{devnode}'"));
    }
    Ok(buf)
}

/// Byte-exact comparison helper for verify-after-write (pure, unit-tested).
pub fn reports_match(got: &[u8], expected: &[u8]) -> bool {
    got.len() >= expected.len() && &got[..expected.len()] == expected
}

/// Verify a just-written report by reading it back. Returns `Ok(true)` on
/// match, `Ok(false)` on mismatch (caller decides retry policy), `Err` on I/O.
pub fn verify_report(devnode: &str, expected: &[u8]) -> Result<bool> {
    if expected.is_empty() {
        anyhow::bail!("cannot verify an empty report");
    }
    let got = get_feature_report_hidraw(devnode, expected[0], expected.len())?;
    Ok(reports_match(&got, expected))
}

/// Attempts for [`send_with_retry`]: first try + 4 backoff retries.
pub const DEFAULT_SEND_ATTEMPTS: u32 = 5;

/// True for errors worth retrying: the hidraw node mid-probe/bind (`ENOENT`
/// — udev `add` fired before the driver bound the node).
///
/// Programming errors (`EINVAL` bad length, `ENOTTY` wrong node/driver,
/// `EACCES` permissions) fail fast instead of burning 5 sleeps. Note
/// `EPIPE`/`EIO` never reach here: [`send_feature_report_hidraw`] already
/// treats them as mode-switch success one layer down.
pub fn is_transient_send_error(e: &anyhow::Error) -> bool {
    e.chain().any(|cause| {
        cause
            .downcast_ref::<std::io::Error>()
            .is_some_and(|io| io.raw_os_error() == Some(libc::ENOENT))
    })
}

/// Retry wrapper over any [`FeatureSender`]; `label` names the node in logs.
/// Split from [`send_with_retry`] so tests can drive the policy with a mock.
pub fn send_with_retry_on(
    sender: &dyn FeatureSender,
    report: &[u8],
    max_attempts: u32,
    label: &str,
) -> Result<()> {
    let attempts = max_attempts.max(1);
    let mut delay_ms = 25u64;
    let mut attempt = 0u32;
    loop {
        attempt += 1;
        match sender.send(report) {
            Ok(()) => return Ok(()),
            Err(e) => {
                if attempt >= attempts || !is_transient_send_error(&e) {
                    if attempt < attempts {
                        log::debug!("permanent send error on {label}, not retrying: {e:#}");
                    }
                    return Err(e).with_context(|| {
                        format!("feature report failed after {attempt} attempt(s) on '{label}'")
                    });
                }
                log::warn!(
                    "feature report {:02X?} on {label} attempt {attempt}/{attempts} failed ({e:#}); retrying in {delay_ms} ms",
                    report
                );
                std::thread::sleep(std::time::Duration::from_millis(delay_ms));
                delay_ms = (delay_ms * 2).min(500);
            }
        }
    }
}

/// Send with backoff retry against the hidraw/probe race (see
/// [`is_transient_send_error`] for what retries vs. fails fast).
pub fn send_with_retry(devnode: &str, report: &[u8], max_attempts: u32) -> Result<()> {
    let sender = HidrawSender {
        devnode: devnode.to_string(),
    };
    send_with_retry_on(&sender, report, max_attempts, devnode)
}
/// Sink for feature reports; real hardware vs. test recorder.
pub trait FeatureSender {
    fn send(&self, report: &[u8]) -> Result<()>;
}

/// [`FeatureSender`] that writes to a hidraw node, each report with backoff
/// retry against the probe/bind race.
pub struct HidrawSender {
    pub devnode: String,
}

/// Single-shot hidraw writer: one `HIDIOCSFEATURE`, no retry. Retry policy
/// is applied exactly once, by [`HidrawSender`] via [`send_with_retry_on`];
/// the sequence layer ([`init_usb_with_retry`]) adds sequence-level retries
/// on top, with no hidden per-report multiplier beyond
/// [`DEFAULT_SEND_ATTEMPTS`].
struct RawHidrawSender<'a> {
    devnode: &'a str,
}

impl FeatureSender for RawHidrawSender<'_> {
    fn send(&self, report: &[u8]) -> Result<()> {
        send_feature_report_hidraw(self.devnode, report)
    }
}

impl FeatureSender for HidrawSender {
    fn send(&self, report: &[u8]) -> Result<()> {
        send_with_retry_on(
            &RawHidrawSender {
                devnode: &self.devnode,
            },
            report,
            DEFAULT_SEND_ATTEMPTS,
            &self.devnode,
        )
    }
}

/// In-memory [`FeatureSender`] for unit tests.
#[derive(Default)]
pub struct Recorder {
    pub sent: RefCell<Vec<Vec<u8>>>,
}

impl FeatureSender for Recorder {
    fn send(&self, report: &[u8]) -> Result<()> {
        self.sent.borrow_mut().push(report.to_vec());
        Ok(())
    }
}

/// Full USB init sequence: multitouch → host-click → polling.
/// Idempotent: safe to re-run on udev-add / resume.
pub fn init_usb(sender: &dyn FeatureSender, requested_hz: u32) -> Result<PollingPlan> {
    let plan = resolve_polling(requested_hz)?;
    sender.send(MULTITOUCH_USB)?;
    sender.send(HOST_CLICK_USB_ON)?;
    sender.send(&polling_report(plan.x))?;
    log::info!(
        "USB init: multitouch 02 01, host-click 21 01, polling [99 F2 {:02X}] \
         (requested {} Hz → effective {:.0} Hz, device cap ~110 Hz)",
        plan.x,
        plan.requested_hz,
        plan.effective_hz,
    );
    Ok(plan)
}

/// USB disengage path (`-d`, or daemon-socket absent without `-f`):
/// host-click off only; multitouch/polling untouched.
pub fn disengage_usb(sender: &dyn FeatureSender) -> Result<()> {
    sender.send(HOST_CLICK_USB_OFF)?;
    log::info!("USB disengage: host-click 21 00");
    Ok(())
}

/// Attempts for whole-sequence retry in [`init_usb_with_retry`].
pub const DEFAULT_INIT_ATTEMPTS: u32 = 3;

/// [`init_usb`] with whole-sequence retry: if any report fails after its own
/// per-report backoff, re-run the sequence from multitouch enable (required:
/// resume regression means multitouch must be re-sent first anyway).
pub fn init_usb_with_retry(sender: &dyn FeatureSender, requested_hz: u32) -> Result<PollingPlan> {
    let mut last_err = None;
    for attempt in 1..=DEFAULT_INIT_ATTEMPTS {
        match init_usb(sender, requested_hz) {
            Ok(plan) => return Ok(plan),
            Err(e) => {
                if !is_transient_send_error(&e) {
                    log::debug!("permanent USB init error, not retrying sequence: {e:#}");
                    return Err(e)
                        .with_context(|| format!("USB init failed after {attempt} attempt(s)"));
                }
                log::warn!("USB init attempt {attempt}/{DEFAULT_INIT_ATTEMPTS} failed: {e:#}");
                last_err = Some(e);
                std::thread::sleep(std::time::Duration::from_millis(100 * u64::from(attempt)));
            }
        }
    }
    Err(last_err.expect("retry loop always errors here"))
        .with_context(|| format!("USB init failed after {DEFAULT_INIT_ATTEMPTS} attempts"))
}

/// [`disengage_usb`] with the same whole-sequence retry posture.
pub fn disengage_usb_with_retry(sender: &dyn FeatureSender) -> Result<()> {
    let mut last_err = None;
    for attempt in 1..=DEFAULT_INIT_ATTEMPTS {
        match disengage_usb(sender) {
            Ok(()) => return Ok(()),
            Err(e) => {
                if !is_transient_send_error(&e) {
                    log::debug!("permanent USB disengage error, not retrying: {e:#}");
                    return Err(e).with_context(|| {
                        format!("USB disengage failed after {attempt} attempt(s)")
                    });
                }
                log::warn!("USB disengage attempt {attempt}/{DEFAULT_INIT_ATTEMPTS} failed: {e:#}");
                last_err = Some(e);
                std::thread::sleep(std::time::Duration::from_millis(100 * u64::from(attempt)));
            }
        }
    }
    Err(last_err.expect("retry loop always errors here"))
        .with_context(|| format!("USB disengage failed after {DEFAULT_INIT_ATTEMPTS} attempts"))
}

/// Whether a [`DeviceReadyEvent`] announces a usable device or a detach.
/// Disengage and engage emit *different* kinds so subscribers never confuse
/// "device ready" with "device going away".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReadyKind {
    Ready,
    Detached,
}

/// Bluetooth multitouch-enable report (`F1 02 01`, kernel `magicmouse` BT
/// path). Documented for bring-up verification only: the BT path sends NO
/// init reports (single-node, straight to use); the constant exists so tests
/// and future verify steps pin the expected bytes without drifting.
pub const MULTITOUCH_BT: &[u8] = &[0xF1, 0x02, 0x01];
/// Bluetooth host-click report builder (`F2 21 0X`). Same status as
/// [`MULTITOUCH_BT`]: documented, not sent by the BT bring-up path.
pub fn host_click_bt(enabled: bool) -> [u8; 3] {
    [0xF2, 0x21, u8::from(enabled)]
}

/// Outcome of one idempotent USB init run: the polling plan plus which
/// interface node the sequence was written to (per-interface routing
/// groundwork).
///
/// Trust limits — sibling routing must NOT treat these as device-confirmed
/// acceptance:
/// - `iface` is caller-supplied (sysfs-derived at call time for the node the
///   reports were written to), not a device confirmation.
/// - `multitouch_accepted` is true iff the full sequence — starting with the
///   multitouch write — returned success on this node. A sibling interface
///   must still be initialized and verified itself; never skip it because
///   another node reported success.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct UsbInitOutcome {
    pub plan: PollingPlan,
    /// Caller-supplied hint: interface number of the node written to
    /// (`None` = unknown layout; full sequence still sent).
    pub iface: Option<u8>,
    /// True iff the multitouch-first sequence wrote without error on this node.
    pub multitouch_accepted: bool,
}

/// [`init_usb`] recording which interface accepted multitouch.
/// Idempotent: safe to re-run on the same node or a sibling interface node.
pub fn init_usb_recorded(
    sender: &dyn FeatureSender,
    requested_hz: u32,
    iface: Option<u8>,
) -> Result<UsbInitOutcome> {
    let plan = init_usb(sender, requested_hz)?;
    if let Some(n) = iface {
        log::debug!("USB init accepted on interface {n} (multitouch 02 01 sent)");
    } else {
        log::debug!("USB init accepted on unknown interface (multitouch 02 01 sent)");
    }
    Ok(UsbInitOutcome {
        plan,
        iface,
        multitouch_accepted: true,
    })
}

/// Resume re-init: re-send multitouch enable FIRST.
///
/// Resume regression (RESEARCH §6): Trackpad 2 USB drops out of multitouch
/// mode across a power transition (report `0x02` shrinks 21→8 bytes; clicks
/// survive, motion lost until re-init). The resume path MUST re-run the full
/// USB sequence starting at multitouch enable.
///
/// Production entry for the resume path (`ltd resume` hook/one-shot and the
/// daemon belt-and-braces tick): idempotent, same bytes as init.
pub fn resume_usb(
    sender: &dyn FeatureSender,
    requested_hz: u32,
    iface: Option<u8>,
) -> Result<UsbInitOutcome> {
    log::info!("resume re-init: re-sending multitouch enable first (21→8 byte dropout fix)");
    init_usb_recorded(sender, requested_hz, iface)
}

/// [`resume_usb`] with whole-sequence retry: same posture as
/// [`init_usb_with_retry`] — transient failures re-run from multitouch
/// enable, permanent errors fail fast.
pub fn resume_usb_with_retry(
    sender: &dyn FeatureSender,
    requested_hz: u32,
    iface: Option<u8>,
) -> Result<UsbInitOutcome> {
    let mut last_err = None;
    for attempt in 1..=DEFAULT_INIT_ATTEMPTS {
        match resume_usb(sender, requested_hz, iface) {
            Ok(outcome) => return Ok(outcome),
            Err(e) => {
                if !is_transient_send_error(&e) {
                    log::debug!("permanent resume re-init error, not retrying sequence: {e:#}");
                    return Err(e).with_context(|| {
                        format!("resume re-init failed after {attempt} attempt(s)")
                    });
                }
                log::warn!(
                    "resume re-init attempt {attempt}/{DEFAULT_INIT_ATTEMPTS} failed: {e:#}"
                );
                last_err = Some(e);
                std::thread::sleep(std::time::Duration::from_millis(100 * u64::from(attempt)));
            }
        }
    }
    Err(last_err.expect("retry loop always errors here"))
        .with_context(|| format!("resume re-init failed after {DEFAULT_INIT_ATTEMPTS} attempts"))
}

/// [`resume_usb`] + best-effort `HIDIOCGFEATURE` verify of the polling report.
///
/// Same contract as [`init_usb_verified`]: only an `Ok(false)` mismatch
/// re-runs the whole sequence from multitouch enable; an unreadable verify
/// (`Err`) is a best-effort warning, not fatal.
pub fn resume_usb_verified(
    sender: &dyn FeatureSender,
    verifier: &dyn ReportVerifier,
    requested_hz: u32,
    iface: Option<u8>,
) -> Result<UsbInitOutcome> {
    let mut last_mismatch = 0u32;
    for attempt in 1..=DEFAULT_INIT_ATTEMPTS {
        let outcome = resume_usb_with_retry(sender, requested_hz, iface)?;
        let expected = polling_report(outcome.plan.x);
        match verifier.verify(&expected) {
            Ok(true) => {
                return Ok(outcome);
            }
            Ok(false) => {
                last_mismatch += 1;
                if attempt >= DEFAULT_INIT_ATTEMPTS {
                    break;
                }
                log::warn!(
                    "resume polling verify mismatch (attempt {attempt}/{DEFAULT_INIT_ATTEMPTS}); re-running from multitouch enable"
                );
                std::thread::sleep(std::time::Duration::from_millis(100 * u64::from(attempt)));
            }
            Err(e) => {
                log::warn!(
                    "resume polling verify unreadable: {e:#}; accepting re-init (firmware may NAK reads)"
                );
                return Ok(outcome);
            }
        }
    }
    anyhow::bail!("resume polling verify mismatch after {DEFAULT_INIT_ATTEMPTS} attempts ({last_mismatch} mismatches)");
}

/// Read-back side of verify-after-write. Real hardware vs. test stub.
pub trait ReportVerifier {
    fn verify(&self, expected: &[u8]) -> Result<bool>;
}

/// [`ReportVerifier`] that reads back via `HIDIOCGFEATURE`.
pub struct HidrawVerifier {
    pub devnode: String,
}

impl ReportVerifier for HidrawVerifier {
    fn verify(&self, expected: &[u8]) -> Result<bool> {
        verify_report(&self.devnode, expected)
    }
}

/// In-memory [`ReportVerifier`] for unit tests.
#[derive(Default)]
pub struct StubVerifier {
    pub results: RefCell<Vec<Result<bool>>>,
    pub calls: RefCell<Vec<Vec<u8>>>,
}

impl StubVerifier {
    pub fn with_results(results: Vec<Result<bool>>) -> Self {
        Self {
            results: RefCell::new(results.into_iter().rev().collect()),
            calls: RefCell::new(Vec::new()),
        }
    }
}

impl ReportVerifier for StubVerifier {
    fn verify(&self, expected: &[u8]) -> Result<bool> {
        self.calls.borrow_mut().push(expected.to_vec());
        self.results.borrow_mut().pop().unwrap_or(Ok(true))
    }
}

/// [`init_usb`] + best-effort `HIDIOCGFEATURE` verify of the polling report.
///
/// Only an `Ok(false)` mismatch re-runs the whole sequence from multitouch
/// enable (up to [`DEFAULT_INIT_ATTEMPTS`]×, no sleep after the final
/// attempt). An unreadable verify (`Err` — some firmware NAKs reads) is a
/// best-effort warning, not fatal: init still succeeds.
pub fn init_usb_verified(
    sender: &dyn FeatureSender,
    verifier: &dyn ReportVerifier,
    requested_hz: u32,
    iface: Option<u8>,
) -> Result<UsbInitOutcome> {
    let mut last_mismatch = 0u32;
    for attempt in 1..=DEFAULT_INIT_ATTEMPTS {
        let plan = init_usb_with_retry(sender, requested_hz)?;
        let expected = polling_report(plan.x);
        match verifier.verify(&expected) {
            Ok(true) => {
                return Ok(UsbInitOutcome {
                    iface,
                    multitouch_accepted: true,
                    plan,
                });
            }
            Ok(false) => {
                last_mismatch += 1;
                if attempt >= DEFAULT_INIT_ATTEMPTS {
                    break;
                }
                log::warn!(
                    "polling verify mismatch (attempt {attempt}/{DEFAULT_INIT_ATTEMPTS}); re-running from multitouch enable"
                );
                std::thread::sleep(std::time::Duration::from_millis(100 * u64::from(attempt)));
            }
            Err(e) => {
                log::warn!(
                    "polling verify unreadable: {e:#}; accepting init (firmware may NAK reads)"
                );
                return Ok(UsbInitOutcome {
                    iface,
                    multitouch_accepted: true,
                    plan,
                });
            }
        }
    }
    anyhow::bail!("polling verify mismatch after {DEFAULT_INIT_ATTEMPTS} attempts ({last_mismatch} mismatches)");
}

/// BT double-input policy (RESEARCH §6: hidraw coexists with
/// `hid-magicmouse` on BT — NO exclusivity).
///
/// Default MUST leave kernel passthrough intact (plain libinput pointer
/// keeps working with init alone). Only when `enhanced_clicks=true` (opt-in
/// clicks pipeline) may we suppress the physical node's events — by
/// unbinding the driver for our VID:PID or evdev-grabbing the physical node
/// in the clicks pipeline (Phase-1 `uinput-proxy` owns the grab; here we do
/// the best-effort unbind so bring-up is testable without clicks code).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BtCoexistence {
    /// Leave the kernel device alone; passthrough continues, no double-input
    /// handling. Default.
    Passthrough,
    /// Opt-in: suppress the physical node so only the proxy emits.
    SuppressPhysical,
}

/// Pure policy decision: only the explicit opt-in suppresses BT passthrough.
pub fn bt_coexistence_policy(enhanced_clicks: bool) -> BtCoexistence {
    if enhanced_clicks {
        BtCoexistence::SuppressPhysical
    } else {
        BtCoexistence::Passthrough
    }
}

/// Apply the BT coexistence policy. Returns `true` when the physical node
/// was actually suppressed, `false` when passthrough was (correctly) left
/// intact or suppression was best-effort skipped. Never fails init: unbind
/// errors are warnings.
pub fn apply_bt_coexistence(devnode: &str, enhanced_clicks: bool) -> Result<bool> {
    match bt_coexistence_policy(enhanced_clicks) {
        BtCoexistence::Passthrough => {
            log::debug!("BT {devnode}: passthrough intact (enhanced_clicks=false, no unbind/grab)");
            Ok(false)
        }
        BtCoexistence::SuppressPhysical => {
            // LOUD: uinput-proxy is not merged yet, so suppressing the
            // physical node removes BT input with NO proxy replacement —
            // the trackpad goes silent until re-bind/replug. This stays
            // behind the explicit opt-in until the proxy lands.
            log::warn!(
                "BT {devnode}: enhanced_clicks=true with no uinput-proxy yet — suppressing physical input with no replacement; expect a silent trackpad until re-bind"
            );
            log::info!("BT {devnode}: enhanced_clicks=true — suppressing physical node (unbind hid-magicmouse for our VID:PID)");
            match attempt_driver_unbind(devnode) {
                Ok(true) => Ok(true),
                Ok(false) => {
                    log::warn!(
                        "BT {devnode}: unbind skipped (node not resolved); clicks pipeline must evdev-grab the physical node instead"
                    );
                    Ok(false)
                }
                Err(e) => {
                    log::warn!("BT {devnode}: unbind best-effort failed ({e:#}); continuing with passthrough");
                    Ok(false)
                }
            }
        }
    }
}

/// HID device id (`XXXX:XXXX:XXXX.NNNN`) for a hidraw node, from the
/// `device` symlink target's basename. `None` when unresolvable.
fn hid_id_for_node(devnode: &str, class_root: &str) -> Option<String> {
    let name = crate::discovery::hidraw_name(devnode)?;
    let device_link = std::path::PathBuf::from(format!("{class_root}/{name}/device"));
    let target = std::fs::read_link(&device_link).ok()?;
    let hid_id = target
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    if hid_id.is_empty() {
        return None;
    }
    Some(hid_id)
}

/// Bound driver name for a hidraw node, from the `device/driver` symlink
/// target's basename (e.g. `magicmouse`, `hid-generic`). `None` when the
/// node is unbound or the layout is unknown.
fn bound_driver_for_node(devnode: &str, class_root: &str) -> Option<String> {
    let name = crate::discovery::hidraw_name(devnode)?;
    let driver_link = std::path::PathBuf::from(format!("{class_root}/{name}/device/driver"));
    let target = std::fs::read_link(&driver_link).ok()?;
    let driver = target
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    if driver.is_empty() {
        return None;
    }
    Some(driver)
}

/// Whether an (un)bind write error means "already (un)bound elsewhere, try
/// the next driver" (`ENODEV`/`ENXIO`/`ENOENT`) rather than a real failure.
fn is_expected_bind_race(errno: i32) -> bool {
    matches!(errno, libc::ENODEV | libc::ENXIO | libc::ENOENT)
}

/// Best-effort re-bind of the HID driver for one hidraw node: mirror of
/// [`attempt_driver_unbind`] for the disengage path. Unbind is sticky — a
/// suppressed BT node stays dead until re-bound or replugged — so disengage
/// must restore passthrough.
///
/// Stateless by necessity (udev one-shots share no memory): the HID id is
/// written to each known driver's `bind` file; the kernel rejects with
/// `ENODEV`/`ENXIO`/`ENOENT` where the device does not belong, and with
/// `EBUSY`/similar when already bound — all skipped, never fatal.
/// Returns `Ok(true)` when a bind write succeeded, `Ok(false)` when nothing
/// was done. Permission/I/O errors propagate (caller downgrades to warning).
pub fn restore_bt_passthrough(devnode: &str) -> Result<bool> {
    restore_with_root(devnode, "/sys/class/hidraw", "/sys/bus/hid/drivers")
}

fn restore_with_root(devnode: &str, class_root: &str, drivers_root: &str) -> Result<bool> {
    let Some(hid_id) = hid_id_for_node(devnode, class_root) else {
        return Ok(false);
    };
    for driver in ["magicmouse", "hid-generic"] {
        let bind = std::path::PathBuf::from(format!("{drivers_root}/{driver}/bind"));
        if std::fs::symlink_metadata(&bind).is_err() {
            continue;
        }
        match std::fs::write(&bind, &hid_id) {
            Ok(()) => {
                log::info!("re-bound HID device {hid_id} to {driver}");
                return Ok(true);
            }
            Err(e) => {
                let errno = e.raw_os_error().unwrap_or(0);
                // EBUSY = already bound: nothing to do, not an error.
                if is_expected_bind_race(errno) || errno == libc::EBUSY {
                    log::debug!("bind {hid_id} to {driver}: {e} — trying next driver");
                    continue;
                }
                return Err(e.into());
            }
        }
    }
    Ok(false)
}

/// Best-effort unbind of the HID driver for one hidraw node.
/// Writes the HID device id to the *currently bound* driver's `unbind`
/// file (resolved via the `device/driver` symlink), falling back to the
/// known driver names when the link is unreadable. A write to the wrong
/// driver fails with `ENODEV`/`ENXIO`/`ENOENT` and is skipped, never fatal;
/// permission/I/O errors (`EACCES`/`EIO`/…) propagate to the caller (which
/// downgrades to a warning anyway). Returns `Ok(false)` when nothing was
/// done.
pub fn attempt_driver_unbind(devnode: &str) -> Result<bool> {
    attempt_driver_unbind_with_root(devnode, "/sys/class/hidraw", "/sys/bus/hid/drivers")
}

fn attempt_driver_unbind_with_root(
    devnode: &str,
    class_root: &str,
    drivers_root: &str,
) -> Result<bool> {
    let Some(hid_id) = hid_id_for_node(devnode, class_root) else {
        return Ok(false);
    };
    // Bound driver first, then the known fallbacks (deduped).
    let mut drivers: Vec<String> = Vec::new();
    if let Some(bound) = bound_driver_for_node(devnode, class_root) {
        drivers.push(bound);
    }
    for fallback in ["magicmouse", "hid-generic"] {
        if !drivers.iter().any(|d| d == fallback) {
            drivers.push(fallback.to_string());
        }
    }
    for driver in &drivers {
        let unbind = std::path::PathBuf::from(format!("{drivers_root}/{driver}/unbind"));
        if std::fs::symlink_metadata(&unbind).is_err() {
            continue;
        }
        match std::fs::write(&unbind, &hid_id) {
            Ok(()) => {
                log::info!("unbound HID device {hid_id} from {driver}");
                return Ok(true);
            }
            Err(e) => {
                let errno = e.raw_os_error().unwrap_or(0);
                if is_expected_bind_race(errno) {
                    log::debug!("unbind {hid_id} from {driver}: {e} — trying next driver");
                    continue;
                }
                return Err(e.into());
            }
        }
    }
    Ok(false)
}
/// Device-ready event emitted after successful init (or immediately for BT).
/// The future clicks pipeline subscribes via [`DeviceReadyHub`]; init holds
/// no reference to clicks code.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceReadyEvent {
    pub devnode: String,
    pub transport: Transport,
    pub kind: ReadyKind,
}

/// Bluetooth path: no init dance, straight to device-ready.
pub fn init_bluetooth(devnode: &str) -> DeviceReadyEvent {
    log::info!("BT {devnode}: single-node path, no init dance — device ready");
    DeviceReadyEvent {
        devnode: devnode.to_string(),
        transport: Transport::Bluetooth,
        kind: ReadyKind::Ready,
    }
}

/// Constructor for the USB device-ready event (call after [`init_usb`]).
pub fn usb_ready(devnode: &str) -> DeviceReadyEvent {
    DeviceReadyEvent {
        devnode: devnode.to_string(),
        transport: Transport::Usb,
        kind: ReadyKind::Ready,
    }
}

/// Constructor for the detach event (call after a disengage path).
/// Distinct from [`usb_ready`]/[`init_bluetooth`] so subscribers can tell
/// detach apart from ready.
pub fn detached(devnode: &str, transport: Transport) -> DeviceReadyEvent {
    DeviceReadyEvent {
        devnode: devnode.to_string(),
        transport,
        kind: ReadyKind::Detached,
    }
}

/// Listener callback type for [`DeviceReadyHub`].
pub type DeviceReadyCallback = dyn Fn(&DeviceReadyEvent) + Send + Sync;

/// Stub fan-out hook for device-ready events. Clicks subscribes later;
/// init only emits. Listeners run on snapshots outside the lock, so a
/// listener may (un)subscribe re-entrantly without deadlock; a poisoned
/// mutex is recovered instead of panicking (daemon never-panic contract).
#[derive(Default)]
pub struct DeviceReadyHub {
    listeners: Mutex<Vec<Arc<DeviceReadyCallback>>>,
}

impl DeviceReadyHub {
    pub fn new() -> Self {
        Self::default()
    }

    /// Subscribe a listener (future: clicks pipeline). Returns index.
    pub fn subscribe<F>(&self, f: F) -> usize
    where
        F: Fn(&DeviceReadyEvent) + Send + Sync + 'static,
    {
        let mut guard = self.listeners.lock().unwrap_or_else(|e| e.into_inner());
        guard.push(Arc::new(f));
        guard.len() - 1
    }

    /// Emit to all listeners; a panicking listener is isolated so init
    /// (and passthrough) is never taken down by clicks.
    pub fn emit(&self, event: &DeviceReadyEvent) {
        let snapshot: Vec<Arc<DeviceReadyCallback>> = self
            .listeners
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        for listener in snapshot.iter() {
            let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| listener(event)));
        }
    }

    pub fn listener_count(&self) -> usize {
        self.listeners
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    #[test]
    fn default_110_maps_to_0x23() {
        let plan = resolve_polling(crate::config::DEFAULT_POLLING_HZ).unwrap();
        assert_eq!(plan.requested_hz, 110);
        assert_eq!(plan.x, MIN_X_FOR_CAP);
        assert_eq!(plan.x, 0x23);
        assert!((plan.effective_hz - 110.0).abs() < 1e-6);
    }

    #[test]
    fn above_cap_is_clamped_never_sent() {
        // Defense in depth: a 500 Hz override must still land on X >= 0x23.
        let plan = resolve_polling(500).unwrap();
        assert_eq!(plan.x, MIN_X_FOR_CAP);
        assert!(plan.effective_hz <= EFFECTIVE_CAP_HZ + 1e-6);
        // Theoretical reflects the request's meaning, not the clamped wire byte.
        assert!((plan.theoretical_hz - 3850.0 / 8.0).abs() < 1e-9);
    }

    #[test]
    fn low_rate_passes_through() {
        let plan = resolve_polling(60).unwrap();
        assert_eq!(plan.x, 64); // round(3850/60)
        assert!((plan.effective_hz - 60.15625).abs() < 1e-6);
    }

    #[test]
    fn ninety_hz_maps_honestly() {
        let plan = resolve_polling(90).unwrap();
        assert_eq!(plan.x, 43); // round(3850/90); raw 43 > 35 so unclamped
        assert!((plan.theoretical_hz - 3850.0 / 43.0).abs() < 1e-9);
    }

    #[test]
    fn rejects_zero_huge_and_below_floor() {
        assert!(resolve_polling(0).is_err());
        assert!(resolve_polling(100_000).is_err());
        // 1 Hz would silently round to X=255 (~15.1 Hz): reject instead.
        assert!(resolve_polling(1).is_err());
        assert!(resolve_polling(15).is_err());
        let plan = resolve_polling(16).unwrap();
        assert_eq!(plan.x, 241); // round(3850/16)
    }

    #[test]
    fn polling_report_bytes() {
        assert_eq!(polling_report(35), [153, 242, 35]);
        assert_eq!(polling_report(8), [0x99, 0xF2, 0x08]);
    }

    #[test]
    fn ioctl_encoding_pins_linux_layout() {
        // dir=3 (_IOC_WRITE|_IOC_READ) << 30 | len << 16 | 'H'(0x48) << 8 | nr.
        // Ground truth: linux/hidraw.h HIDIOCSFEATURE nr 0x06, HIDIOCGFEATURE nr 0x07.
        assert_eq!(hidiocsfeature_request(2), 0xC002_4806);
        assert_eq!(hidiocsfeature_request(3), 0xC003_4806);
        assert_eq!(hidiocgfeature_request(3), 0xC003_4807);
    }

    #[test]
    fn ioctl_dir_matches_host_header() {
        // Cross-check the direction bits against the host kernel header so a
        // wrong-dir regression fails loudly instead of passing pinned values.
        let Ok(text) = std::fs::read_to_string("/usr/include/linux/hidraw.h") else {
            return; // headers absent (non-dev machine): pinned test above still guards.
        };
        for macro_name in ["HIDIOCSFEATURE", "HIDIOCGFEATURE"] {
            let line = text
                .lines()
                .find(|l| l.contains(macro_name))
                .unwrap_or_else(|| panic!("{macro_name} missing from host hidraw.h"));
            assert!(
                line.contains("_IOC_WRITE|_IOC_READ") || line.contains("_IOC_READ|_IOC_WRITE"),
                "{macro_name} not _IOC_WRITE|_IOC_READ in host header: {line}"
            );
        }
        // And our encoder must agree with dir=3 for those macros.
        assert_eq!(hidiocsfeature_request(2) >> 30, 3);
        assert_eq!(hidiocgfeature_request(3) >> 30, 3);
    }

    #[test]
    fn verify_comparison_is_prefix_exact() {
        assert!(reports_match(&[0x02, 0x01, 0xFF], &[0x02, 0x01]));
        assert!(!reports_match(&[0x02, 0x00], &[0x02, 0x01]));
        assert!(!reports_match(&[0x02], &[0x02, 0x01]));
    }

    #[test]
    fn usb_sequence_order() {
        let rec = Recorder::default();
        let plan = init_usb(&rec, 110).unwrap();
        assert_eq!(plan.x, 0x23);
        assert_eq!(
            *rec.sent.borrow(),
            vec![vec![0x02, 0x01], vec![0x21, 0x01], vec![153, 242, 35],]
        );
    }

    #[test]
    fn disengage_sends_host_click_off_only() {
        let rec = Recorder::default();
        disengage_usb(&rec).unwrap();
        assert_eq!(*rec.sent.borrow(), vec![vec![0x21, 0x00]]);
    }

    #[test]
    fn bt_path_has_no_reports() {
        let ev = init_bluetooth("/dev/hidraw9");
        assert_eq!(ev.transport, Transport::Bluetooth);
        assert_eq!(ev.devnode, "/dev/hidraw9");
        assert_eq!(ev.kind, ReadyKind::Ready);
    }

    #[test]
    fn detach_event_distinct_from_ready() {
        let ready = usb_ready("/dev/hidraw1");
        let off = detached("/dev/hidraw1", Transport::Usb);
        assert_eq!(ready.kind, ReadyKind::Ready);
        assert_eq!(off.kind, ReadyKind::Detached);
        assert_ne!(ready, off);
        let bt_off = detached("/dev/hidraw9", Transport::Bluetooth);
        assert_eq!(bt_off.kind, ReadyKind::Detached);
    }

    #[test]
    fn hub_supports_reentrant_subscribe() {
        let hub = Arc::new(DeviceReadyHub::new());
        let hub2 = Arc::clone(&hub);
        hub.subscribe(move |_| {
            hub2.subscribe(|_| {});
        });
        hub.emit(&usb_ready("/dev/hidraw1")); // must not deadlock
        assert_eq!(hub.listener_count(), 2);
    }

    /// Failing sender with a programmed errno + attempt counter.
    struct FailSender {
        attempts: std::cell::Cell<usize>,
        errno: i32,
    }

    impl FeatureSender for FailSender {
        fn send(&self, _report: &[u8]) -> Result<()> {
            self.attempts.set(self.attempts.get() + 1);
            Err(anyhow::Error::from(std::io::Error::from_raw_os_error(
                self.errno,
            )))
        }
    }

    #[test]
    fn retry_transient_until_attempts_exhausted() {
        let sender = FailSender {
            attempts: std::cell::Cell::new(0),
            errno: libc::ENOENT,
        };
        let r = send_with_retry_on(&sender, &[0x02, 0x01], 3, "test-node");
        assert!(r.is_err());
        assert_eq!(sender.attempts.get(), 3);
        assert!(is_transient_send_error(&r.unwrap_err()));
    }

    #[test]
    fn permanent_error_fails_fast_without_sleep() {
        for errno in [libc::EINVAL, libc::ENOTTY, libc::EACCES] {
            let sender = FailSender {
                attempts: std::cell::Cell::new(0),
                errno,
            };
            let before = std::time::Instant::now();
            let r = send_with_retry_on(&sender, &[0x02, 0x01], 5, "test-node");
            assert!(r.is_err(), "errno {errno} should fail");
            assert_eq!(sender.attempts.get(), 1, "errno {errno} must not retry");
            assert!(
                before.elapsed() < std::time::Duration::from_millis(25),
                "errno {errno} must fail fast (no backoff sleep)"
            );
            assert!(!is_transient_send_error(&r.unwrap_err()));
        }
    }
    #[test]
    fn hub_fans_out_and_isolates_panics() {
        let hub = DeviceReadyHub::new();
        let count = Arc::new(AtomicUsize::new(0));
        let c = Arc::clone(&count);
        hub.subscribe(move |_| {
            c.fetch_add(1, Ordering::SeqCst);
        });
        hub.subscribe(|_| panic!("clicks listener blew up"));
        hub.emit(&usb_ready("/dev/hidraw1"));
        assert_eq!(hub.listener_count(), 2);
        assert_eq!(count.load(Ordering::SeqCst), 1);
    }

    /// Flaky sender: fails the first `failures` sends with `errno`, then records.
    struct FlakySender {
        failures: RefCell<usize>,
        errno: i32,
        sent: RefCell<Vec<Vec<u8>>>,
    }

    impl FeatureSender for FlakySender {
        fn send(&self, report: &[u8]) -> Result<()> {
            let mut f = self.failures.borrow_mut();
            if *f > 0 {
                *f -= 1;
                return Err(anyhow::Error::from(std::io::Error::from_raw_os_error(
                    self.errno,
                )));
            }
            self.sent.borrow_mut().push(report.to_vec());
            Ok(())
        }
    }

    #[test]
    fn retry_recovers_from_transient_flakes() {
        let s = FlakySender {
            failures: RefCell::new(2),
            errno: libc::ENOENT,
            sent: RefCell::new(Vec::new()),
        };
        // init_usb_with_retry re-runs the whole sequence per attempt; 2 transient
        // flakes across attempts still converge to one full 3-report sequence.
        let plan = init_usb_with_retry(&s, 110).unwrap();
        assert_eq!(plan.x, 0x23);
        assert_eq!(s.sent.borrow().len(), 3);
        assert_eq!(s.sent.borrow()[0], vec![0x02, 0x01]);
    }

    #[test]
    fn retry_gives_up_after_persistent_transient_failure() {
        let s = FlakySender {
            failures: RefCell::new(usize::MAX / 2),
            errno: libc::ENOENT,
            sent: RefCell::new(Vec::new()),
        };
        assert!(init_usb_with_retry(&s, 110).is_err());
    }

    #[test]
    fn retry_fails_fast_on_permanent_error() {
        // EINVAL is permanent: no whole-sequence retry, no backoff sleep.
        let s = FlakySender {
            failures: RefCell::new(usize::MAX / 2),
            errno: libc::EINVAL,
            sent: RefCell::new(Vec::new()),
        };
        let before = std::time::Instant::now();
        assert!(init_usb_with_retry(&s, 110).is_err());
        assert!(s.sent.borrow().is_empty());
        assert!(before.elapsed() < std::time::Duration::from_millis(100));
    }

    #[test]
    fn sequence_retry_attempts_exact_count_no_multiplier() {
        // Always-transient sender: each sequence attempt fails on its first
        // report and the mock performs no per-report retry of its own, so the
        // total write count is exactly DEFAULT_INIT_ATTEMPTS — no nesting
        // multiplier between the sequence and per-report layers.
        let sender = FailSender {
            attempts: std::cell::Cell::new(0),
            errno: libc::ENOENT,
        };
        assert!(init_usb_with_retry(&sender, 110).is_err());
        assert_eq!(
            sender.attempts.get(),
            DEFAULT_INIT_ATTEMPTS as usize,
            "sequence layer must attempt exactly DEFAULT_INIT_ATTEMPTS writes"
        );
    }

    #[test]
    fn idempotent_rerun_sends_same_sequence() {
        // Safe re-run on the same node: two consecutive inits emit the same
        // 3-report sequence (multitouch first — resume regression fix).
        let rec = Recorder::default();
        let first = init_usb_recorded(&rec, 110, Some(1)).unwrap();
        let second = init_usb_recorded(&rec, 110, Some(1)).unwrap();
        assert_eq!(first.plan, second.plan);
        assert!(first.multitouch_accepted && second.multitouch_accepted);
        assert_eq!(first.iface, Some(1));
        let sent = rec.sent.borrow();
        assert_eq!(sent.len(), 6);
        assert_eq!(sent[0], vec![0x02, 0x01]);
        assert_eq!(sent[3], vec![0x02, 0x01]);
    }

    #[test]
    fn resume_resends_multitouch_first() {
        let rec = Recorder::default();
        let out = resume_usb(&rec, 110, Some(2)).unwrap();
        assert!(out.multitouch_accepted);
        assert_eq!(rec.sent.borrow()[0], vec![0x02, 0x01]);
        assert_eq!(rec.sent.borrow().len(), 3);
    }

    #[test]
    fn resume_with_retry_recovers_from_transient_flakes() {
        let s = FlakySender {
            failures: RefCell::new(2),
            errno: libc::ENOENT,
            sent: RefCell::new(Vec::new()),
        };
        let out = resume_usb_with_retry(&s, 110, Some(2)).unwrap();
        assert_eq!(out.plan.x, 0x23);
        assert_eq!(out.iface, Some(2));
        assert!(out.multitouch_accepted);
        assert_eq!(s.sent.borrow().len(), 3);
        assert_eq!(s.sent.borrow()[0], vec![0x02, 0x01]);
    }

    #[test]
    fn resume_with_retry_fails_fast_on_permanent_error() {
        let s = FlakySender {
            failures: RefCell::new(usize::MAX / 2),
            errno: libc::EINVAL,
            sent: RefCell::new(Vec::new()),
        };
        let before = std::time::Instant::now();
        assert!(resume_usb_with_retry(&s, 110, None).is_err());
        assert!(s.sent.borrow().is_empty());
        assert!(before.elapsed() < std::time::Duration::from_millis(100));
    }

    #[test]
    fn resume_verified_retries_whole_sequence_on_mismatch() {
        let rec = Recorder::default();
        let verifier = StubVerifier::with_results(vec![Ok(false), Ok(true)]);
        let out = resume_usb_verified(&rec, &verifier, 110, Some(1)).unwrap();
        assert_eq!(out.plan.x, 0x23);
        assert_eq!(out.iface, Some(1));
        // Two full sequences (multitouch first each time).
        assert_eq!(rec.sent.borrow().len(), 6);
        assert_eq!(rec.sent.borrow()[0], vec![0x02, 0x01]);
        assert_eq!(rec.sent.borrow()[3], vec![0x02, 0x01]);
    }

    #[test]
    fn resume_verified_tolerates_unreadable_readback() {
        let rec = Recorder::default();
        let verifier =
            StubVerifier::with_results(vec![Err(anyhow::anyhow!("firmware NAKs reads"))]);
        // Warning, not fatal.
        assert!(resume_usb_verified(&rec, &verifier, 110, None).is_ok());
        assert_eq!(rec.sent.borrow()[0], vec![0x02, 0x01]);
    }

    #[test]
    fn verified_init_accepts_matching_readback() {
        let rec = Recorder::default();
        let verifier = StubVerifier::with_results(vec![Ok(true)]);
        let out = init_usb_verified(&rec, &verifier, 110, Some(1)).unwrap();
        assert_eq!(out.plan.x, 0x23);
        assert_eq!(verifier.calls.borrow().len(), 1);
    }

    #[test]
    fn verified_init_retries_whole_sequence_on_mismatch() {
        let rec = Recorder::default();
        let verifier = StubVerifier::with_results(vec![Ok(false), Ok(true)]);
        let out = init_usb_verified(&rec, &verifier, 110, None).unwrap();
        assert_eq!(out.plan.x, 0x23);
        // Two full sequences (multitouch first each time).
        assert_eq!(rec.sent.borrow().len(), 6);
        assert_eq!(rec.sent.borrow()[3], vec![0x02, 0x01]);
    }

    #[test]
    fn verified_init_tolerates_unreadable_readback() {
        let rec = Recorder::default();
        let verifier =
            StubVerifier::with_results(vec![Err(anyhow::anyhow!("firmware NAKs reads"))]);
        // Warning, not fatal.
        assert!(init_usb_verified(&rec, &verifier, 110, None).is_ok());
    }

    #[test]
    fn bt_report_constants_pinned() {
        assert_eq!(MULTITOUCH_BT, &[0xF1, 0x02, 0x01]);
        assert_eq!(host_click_bt(true), [0xF2, 0x21, 0x01]);
        assert_eq!(host_click_bt(false), [0xF2, 0x21, 0x00]);
    }

    #[test]
    fn bt_policy_defaults_to_passthrough() {
        assert_eq!(bt_coexistence_policy(false), BtCoexistence::Passthrough);
        assert_eq!(bt_coexistence_policy(true), BtCoexistence::SuppressPhysical);
    }

    #[test]
    fn bt_passthrough_never_suppresses() {
        // Default: leaves kernel passthrough intact, reports no suppression.
        assert!(!apply_bt_coexistence("/dev/hidraw9", false).unwrap());
    }

    #[test]
    fn bt_unbind_skipped_for_unresolvable_node() {
        // enhanced=true but no sysfs node: best-effort skip, never an error.
        assert!(!attempt_driver_unbind_with_root(
            "/dev/hidraw77",
            "/nonexistent-xyz",
            "/nonexistent-xyz"
        )
        .unwrap());
        assert!(!apply_bt_coexistence("/dev/hidraw77", true).unwrap());
    }

    /// Fake sysfs with a bound driver: `device` and `device/driver` symlinks
    /// plus a writable `unbind`/`bind` file standing in for sysfs.
    fn fake_bound_sysfs() -> (tempfile::TempDir, String, String) {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::create_dir_all(root.join("devices/0005:004C:0324.000B")).unwrap();
        std::fs::create_dir_all(root.join("class/hidraw3")).unwrap();
        std::os::unix::fs::symlink(
            "../../devices/0005:004C:0324.000B",
            root.join("class/hidraw3/device"),
        )
        .unwrap();
        std::os::unix::fs::symlink("magicmouse", root.join("class/hidraw3/device/driver")).unwrap();
        std::fs::create_dir_all(root.join("drivers/magicmouse")).unwrap();
        std::fs::write(root.join("drivers/magicmouse/unbind"), "").unwrap();
        std::fs::write(root.join("drivers/magicmouse/bind"), "").unwrap();
        let class = root.join("class").to_str().unwrap().to_string();
        let drivers = root.join("drivers").to_str().unwrap().to_string();
        (dir, class, drivers)
    }

    #[test]
    fn unbind_writes_hid_id_to_bound_driver() {
        let (_dir, class, drivers) = fake_bound_sysfs();
        assert!(attempt_driver_unbind_with_root("/dev/hidraw3", &class, &drivers).unwrap());
        let written = std::fs::read_to_string(format!("{drivers}/magicmouse/unbind")).unwrap();
        assert_eq!(written, "0005:004C:0324.000B");
    }

    #[test]
    fn unbind_falls_back_when_driver_link_absent() {
        let (_dir, class, drivers) = fake_bound_sysfs();
        // Remove the driver symlink: bound-driver resolution fails, the
        // fallback list must still reach magicmouse.
        std::fs::remove_file(format!("{class}/hidraw3/device/driver")).unwrap();
        assert!(attempt_driver_unbind_with_root("/dev/hidraw3", &class, &drivers).unwrap());
    }

    #[test]
    fn restore_rebinds_after_unbind() {
        let (_dir, class, drivers) = fake_bound_sysfs();
        assert!(restore_with_root("/dev/hidraw3", &class, &drivers).unwrap());
        let written = std::fs::read_to_string(format!("{drivers}/magicmouse/bind")).unwrap();
        assert_eq!(written, "0005:004C:0324.000B");
        assert!(!restore_with_root("/dev/hidraw77", &class, &drivers).unwrap());
    }
}
