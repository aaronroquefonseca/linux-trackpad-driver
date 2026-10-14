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

impl FeatureSender for HidrawSender {
    fn send(&self, report: &[u8]) -> Result<()> {
        send_with_retry(&self.devnode, report, DEFAULT_SEND_ATTEMPTS)
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
}
