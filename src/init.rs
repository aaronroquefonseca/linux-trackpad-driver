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
use std::sync::Mutex;

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
    /// `X` byte actually sent (`>= 35` after cap clamp).
    pub x: u8,
    /// Theoretical output `3850/X` before cap.
    pub theoretical_hz: f64,
    /// Output after honest cap (`min(theoretical, 110)`).
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
    let capped = requested_hz.min(EFFECTIVE_CAP_HZ as u32);
    let x = (POLLING_BASE_HZ / f64::from(capped)).round() as u32;
    let x = (x.clamp(1, 255) as u8).max(MIN_X_FOR_CAP);
    let theoretical_hz = POLLING_BASE_HZ / f64::from(x);
    let effective_hz = theoretical_hz.min(EFFECTIVE_CAP_HZ);
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

/// Compute the `HIDIOCSFEATURE(len)` ioctl request code (Linux `_IOC_WRITE`,
/// type `'H'`, nr `0x06`). Pure so tests can pin the encoding.
pub fn hidiocsfeature_request(len: usize) -> u64 {
    const IOC_WRITE: u64 = 1;
    const TYPE_H: u64 = 0x48;
    const NR: u64 = 0x06;
    (IOC_WRITE << 30) | ((len as u64) << 16) | (TYPE_H << 8) | NR
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

/// Compute the `HIDIOCGFEATURE(len)` ioctl request code (Linux `_IOC_READ`,
/// type `'H'`, nr `0x07`). Pure so tests can pin the encoding.
pub fn hidiocgfeature_request(len: usize) -> u64 {
    const IOC_READ: u64 = 2;
    const TYPE_H: u64 = 0x48;
    const NR: u64 = 0x07;
    (IOC_READ << 30) | ((len as u64) << 16) | (TYPE_H << 8) | NR
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

/// Send with backoff retry against the hidraw/probe race: the kernel probe
/// owns init too, so an `add`-time write may hit `ENOENT` (node not yet
/// bound) or `EPIPE`/`EIO` (mode-switch drop). Sleeps 25→50→100→200 ms
/// between attempts; `EPIPE`/`EIO` count as mode-switch success (see
/// [`send_feature_report_hidraw`]).
pub fn send_with_retry(devnode: &str, report: &[u8], max_attempts: u32) -> Result<()> {
    let attempts = max_attempts.max(1);
    let mut delay_ms = 25u64;
    for attempt in 1..=attempts {
        match send_feature_report_hidraw(devnode, report) {
            Ok(()) => return Ok(()),
            Err(e) => {
                if attempt == attempts {
                    return Err(e).with_context(|| {
                        format!("feature report failed after {attempts} attempts on '{devnode}'")
                    });
                }
                log::warn!(
                    "feature report {:02X?} on {devnode} attempt {attempt}/{attempts} failed ({e:#}); retrying in {delay_ms} ms",
                    report
                );
                std::thread::sleep(std::time::Duration::from_millis(delay_ms));
                delay_ms = (delay_ms * 2).min(500);
            }
        }
    }
    Ok(())
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
                log::warn!("USB disengage attempt {attempt}/{DEFAULT_INIT_ATTEMPTS} failed: {e:#}");
                last_err = Some(e);
                std::thread::sleep(std::time::Duration::from_millis(100 * u64::from(attempt)));
            }
        }
    }
    Err(last_err.expect("retry loop always errors here"))
        .with_context(|| format!("USB disengage failed after {DEFAULT_INIT_ATTEMPTS} attempts"))
}

/// Device-ready event emitted after successful init (or immediately for BT).
/// The future clicks pipeline subscribes via [`DeviceReadyHub`]; init holds
/// no reference to clicks code.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceReadyEvent {
    pub devnode: String,
    pub transport: Transport,
}

/// Bluetooth path: no init dance, straight to device-ready.
pub fn init_bluetooth(devnode: &str) -> DeviceReadyEvent {
    log::info!("BT {devnode}: single-node path, no init dance — device ready");
    DeviceReadyEvent {
        devnode: devnode.to_string(),
        transport: Transport::Bluetooth,
    }
}

/// Constructor for the USB device-ready event (call after [`init_usb`]).
pub fn usb_ready(devnode: &str) -> DeviceReadyEvent {
    DeviceReadyEvent {
        devnode: devnode.to_string(),
        transport: Transport::Usb,
    }
}

/// Listener callback type for [`DeviceReadyHub`].
pub type DeviceReadyCallback = dyn Fn(&DeviceReadyEvent) + Send + Sync;

/// Stub fan-out hook for device-ready events. Clicks subscribes later;
/// init only emits.
#[derive(Default)]
pub struct DeviceReadyHub {
    listeners: Mutex<Vec<Box<DeviceReadyCallback>>>,
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
        let mut guard = self.listeners.lock().expect("hub lock");
        guard.push(Box::new(f));
        guard.len() - 1
    }

    /// Emit to all listeners; a panicking listener is isolated so init
    /// (and passthrough) is never taken down by clicks.
    pub fn emit(&self, event: &DeviceReadyEvent) {
        let guard = self.listeners.lock().expect("hub lock");
        for listener in guard.iter() {
            let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| listener(event)));
        }
    }

    pub fn listener_count(&self) -> usize {
        self.listeners.lock().expect("hub lock").len()
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
    fn rejects_zero_and_huge() {
        assert!(resolve_polling(0).is_err());
        assert!(resolve_polling(100_000).is_err());
    }

    #[test]
    fn polling_report_bytes() {
        assert_eq!(polling_report(35), [153, 242, 35]);
        assert_eq!(polling_report(8), [0x99, 0xF2, 0x08]);
    }

    #[test]
    fn ioctl_encoding_pins_linux_layout() {
        // _IOC_WRITE(1)<<30 | len<<16 | 'H'(0x48)<<8 | 0x06
        assert_eq!(hidiocsfeature_request(2), 0x4000_4806 + (2 << 16));
        assert_eq!(hidiocsfeature_request(3), 0x4003_4806);
        // _IOC_READ(2)<<30 | len<<16 | 'H'(0x48)<<8 | 0x07
        assert_eq!(hidiocgfeature_request(3), 0x8003_4807);
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
