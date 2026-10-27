//! Apple Magic Trackpad 2 hidraw multitouch parser (pure, no I/O).
//!
//! Reimplements the per-contact decode of the kernel `hid-magicmouse`
//! `MAGICTRACKPAD2` path from scratch (no code copied): each contact is a
//! 9-byte block, pressure lives at block offset 7.
//!
//! # 9-byte contact layout (MT2)
//!
//! | Byte | Content |
//! |------|---------|
//! | 0 | X low 8 bits |
//! | 1 | X high 5 bits (low bits) + Y low 3 bits (high bits) |
//! | 2 | Y middle 8 bits |
//! | 3 | Y high 2 bits (low bits); top 2 bits (`& 0xC0`) = contact state |
//! | 4 | touch major |
//! | 5 | touch minor |
//! | 6 | contact size |
//! | 7 | **pressure** (offset 7) |
//! | 8 | high 3 bits = orientation + 4; low 4 bits = tracking id |
//!
//! Decoded (32-bit wrap then arithmetic shift, mirroring the kernel):
//! - `id = b[8] & 0x0f`
//! - `x = ((b[1] << 27) | (b[0] << 19)) >> 19` (13-bit signed)
//! - `y = -(((b[3] << 30) | (b[2] << 22) | (b[1] << 14)) >> 19)`
//! - `orientation = (b[8] >> 5) - 4` (−4..=3, raw; the kernel negates it
//!   for `ABS_MT_ORIENTATION`, so uinput-proxy must emit `-(raw)`)
//! - `contact = (b[3] & 0xC0) == 0x80`
//!
//! # Pressure semantics (offset 7)
//!
//! Pressure is a raw byte with nominal device range **0..=253**,
//! resolution **0** — i.e. arbitrary units, NOT grams/newtons. Downstream
//! must keep `res == 0` on any virtual device and must not advertise
//! `EV_FF`/`FF_HAPTIC`. Values 254..=255 are unobserved/reserved: they
//! pass through untouched here (matching the kernel, which reports the
//! byte unclamped), and the downstream uinput stage MUST clamp to
//! [`PRESSURE_VALID_MAX`] (kernel `absmax`) before emitting
//! `ABS_MT_PRESSURE`.
//!
//! # Frame framing
//!
//! - USB report `0x02`: 12-byte prefix + N × 9-byte contacts.
//! - BT report `0x31` and legacy `0x28` alias: 4-byte prefix + N × 9-byte
//!   contacts. The kernel `hid-magicmouse` `MAGICMOUSE2`/`MT2` path and the
//!   old userspace decoder parse `0x28` identically to `0x31`; dropping
//!   `0x28` here would discard 100% of such frames.
//! - `clicks` = raw `report[1]`; button state is `clicks & 1` — bits 6..7
//!   are device-timestamp bits, not buttons (for later stages).
//! - Double report `0xf7`: two back-to-back sub-reports, see
//!   [`parse_double_report`] (strict: both halves must decode) and
//!   [`parse_double_report_partial`] (lossy: returns each good half, so a
//!   malformed sibling cannot drop good contacts — mirroring the kernel,
//!   which processes each half independently); [`parse_frame`] rejects
//!   `0xf7` so frame boundaries are never silently merged.
//! - Max 15 contacts per frame; trailing bytes after the last full 9-byte
//!   block are ignored with a `debug!` log when at least one full contact
//!   is present (BT firmware may append status bytes — dropping the whole
//!   frame for those would stick fingers / cause extra clicks). A body that
//!   holds zero full contacts plus trailing bytes is still an error.
//! - Device timestamp bits exist in the prefix but are unused (as in the
//!   kernel), so `timestamp_ms` is always 0.
//!
//! # Tiers
//!
//! [`AppleHidraw`] reports [`CapabilityTier::L2p`] (proprietary
//! device-autonomous haptic pad) with [`PressureUnits::Arbitrary`]: the
//! pressure channel behaves as an L1-class arbitrary channel (`res == 0`,
//! 0..253, not grams). See `docs/RESEARCH.md` §4/§6.

use anyhow::{bail, Result};

/// Length of one per-contact block.
pub const CONTACT_BLOCK_LEN: usize = 9;
/// Offset of the pressure byte inside a contact block.
pub const PRESSURE_OFFSET: usize = 7;
/// Nominal device pressure ceiling (values above pass through, see docs).
pub const PRESSURE_VALID_MAX: u8 = 253;
/// Kernel cap: at most 15 contacts per frame.
pub const MAX_CONTACTS: usize = 15;

/// USB multitouch report: 12-byte prefix + N × 9 B.
pub const TRACKPAD2_USB_REPORT_ID: u8 = 0x02;
/// Bluetooth multitouch report: 4-byte prefix + N × 9 B.
pub const TRACKPAD2_BT_REPORT_ID: u8 = 0x31;
/// Legacy report id: decoded identically to [`TRACKPAD2_BT_REPORT_ID`]
/// (4-byte prefix + N × 9 B), matching the kernel `hid-magicmouse` MT2 path
/// and the old userspace decoder.
pub const TRACKPAD_LEGACY_REPORT_ID: u8 = 0x28;
/// Double report: two sub-reports back-to-back.
pub const DOUBLE_REPORT_ID: u8 = 0xf7;

const USB_PREFIX_LEN: usize = 12;
const BT_PREFIX_LEN: usize = 4;

/// Capability tiers (adopted split, see `docs/RESEARCH.md` §4):
/// L0 none, L1a area-proxy (`res=0`), L1b true force (grams, `res!=0`),
/// L2s standard `0x0E` host-controlled haptic, L2p proprietary
/// device-autonomous haptic.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CapabilityTier {
    L0,
    L1a,
    L1b,
    L2s,
    L2p,
}

/// Machine-checked pressure semantics so L1a/L1b can never be conflated.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PressureUnits {
    /// `res == 0`: size-proxy byte, no gram mapping (Apple MT2).
    Arbitrary,
    /// Calibrated force in grams/newtons.
    Grams,
}

/// One decoded contact.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FingerContact {
    /// Tracking id (`block[8] & 0x0f`).
    pub id: u8,
    pub x: i32,
    pub y: i32,
    /// Contact size (`block[6]`).
    pub size: u8,
    /// `(block[8] >> 5) - 4`, range −4..=3.
    pub orientation: i8,
    pub touch_major: u8,
    pub touch_minor: u8,
    /// Raw pressure byte (`block[7]`), nominal 0..=253, arbitrary units.
    pub pressure: u8,
    /// `(block[3] & 0xC0) == 0x80`.
    pub contact: bool,
}

/// One decoded frame.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TouchFrame {
    pub fingers: Vec<FingerContact>,
    /// Raw physical-button byte (`report[1]`): button state is `clicks & 1`
    /// (bits 6..7 carry device-timestamp bits, not buttons).
    pub clicks: u8,
    /// Always 0: device timestamp bits are unused.
    pub timestamp_ms: u64,
}

/// Source of touch frames. Implementing crate owns transport; this task
/// only provides the pure decode plus the tier/units contract.
pub trait TouchSource {
    fn tier(&self) -> CapabilityTier;
    fn pressure_units(&self) -> PressureUnits;
    fn poll_frame(&mut self) -> Result<Option<TouchFrame>>;
    /// Nonblocking hidraw fd for readiness waits (`poll(2)`). `None` means
    /// the source cannot wait on input (tests, stubs): the pump then waits
    /// on control/deadline notifications only.
    fn poll_fd(&self) -> Option<std::os::unix::io::RawFd> {
        None
    }
    /// True when a decoded frame is queued without touching the fd (e.g.
    /// the second half of a `0xf7` double report). The pump drains these
    /// immediately instead of blocking.
    fn has_pending_frame(&self) -> bool {
        false
    }
}

/// Apple Magic Trackpad 2 hidraw source (decode only in this task).
///
/// Transport wiring belongs to `uinput-proxy`; [`TouchSource::poll_frame`]
/// therefore returns a clear error instead of touching hardware.
#[derive(Debug, Default)]
pub struct AppleHidraw;

impl TouchSource for AppleHidraw {
    fn tier(&self) -> CapabilityTier {
        CapabilityTier::L2p
    }

    fn pressure_units(&self) -> PressureUnits {
        PressureUnits::Arbitrary
    }

    fn poll_frame(&mut self) -> Result<Option<TouchFrame>> {
        bail!("AppleHidraw transport not wired in apple-parse; uinput-proxy owns hidraw reads")
    }
}

/// Decode one 9-byte MT2 contact block. Needs at least 9 bytes; only the
/// first 9 are consumed.
pub fn parse_contact(block: &[u8]) -> Result<FingerContact> {
    if block.len() < CONTACT_BLOCK_LEN {
        bail!(
            "short contact block: {} bytes, need {CONTACT_BLOCK_LEN}",
            block.len()
        );
    }
    let b = &block[..CONTACT_BLOCK_LEN];
    // u32 OR then reinterpret as i32: exact 32-bit wrap, arithmetic >>.
    let raw_x = (u32::from(b[1]) << 27) | (u32::from(b[0]) << 19);
    let x = (raw_x as i32) >> 19;
    let raw_y = (u32::from(b[3]) << 30) | (u32::from(b[2]) << 22) | (u32::from(b[1]) << 14);
    let y = -((raw_y as i32) >> 19);
    Ok(FingerContact {
        id: b[8] & 0x0f,
        x,
        y,
        size: b[6],
        orientation: ((b[8] >> 5) as i8) - 4,
        touch_major: b[4],
        touch_minor: b[5],
        pressure: b[PRESSURE_OFFSET],
        contact: b[3] & 0xC0 == 0x80,
    })
}

/// Decode one single-frame report (`0x02` USB, `0x31` BT, or legacy `0x28`
/// as a BT alias with the same 4-byte prefix).
///
/// Rejects `0xf7` (use [`parse_double_report`] for strict both-halves or
/// [`parse_double_report_partial`] for lossy per-half decode) so frame
/// boundaries are never silently merged.
pub fn parse_frame(report: &[u8]) -> Result<TouchFrame> {
    let Some(&id) = report.first() else {
        bail!("empty report");
    };
    match id {
        TRACKPAD2_USB_REPORT_ID => parse_frame_with_prefix(report, USB_PREFIX_LEN),
        TRACKPAD2_BT_REPORT_ID | TRACKPAD_LEGACY_REPORT_ID => {
            parse_frame_with_prefix(report, BT_PREFIX_LEN)
        }
        DOUBLE_REPORT_ID => {
            bail!("double report 0xf7 holds two frames; use parse_double_report")
        }
        _ => bail!("unknown report id {id:#04x}"),
    }
}

fn parse_frame_with_prefix(report: &[u8], prefix: usize) -> Result<TouchFrame> {
    if report.len() < prefix {
        bail!(
            "short report: {} bytes, need {prefix} prefix bytes",
            report.len()
        );
    }
    let body = &report[prefix..];
    let n_full = body.len() / CONTACT_BLOCK_LEN;
    let trailing = body.len() % CONTACT_BLOCK_LEN;
    if n_full == 0 && trailing != 0 {
        bail!(
            "trailing bytes without full contact: body {} not a multiple of {CONTACT_BLOCK_LEN}",
            body.len()
        );
    }
    if n_full > MAX_CONTACTS {
        bail!("too many contacts: {n_full} > {MAX_CONTACTS}");
    }
    if trailing != 0 {
        log::debug!(
            "report {:#04x}: ignoring {trailing} trailing byte(s) after {n_full} contact(s) (body {} B)",
            report[0],
            body.len()
        );
    }
    let full = &body[..n_full * CONTACT_BLOCK_LEN];
    let fingers = full
        .as_chunks::<CONTACT_BLOCK_LEN>()
        .0
        .iter()
        .map(|c| parse_contact(&c[..]))
        .collect::<Result<Vec<_>>>()?;
    Ok(TouchFrame {
        fingers,
        clicks: report[1],
        timestamp_ms: 0,
    })
}

/// Split a `0xf7` double report into its two frames.
///
/// Strict variant: both halves must decode (trailing bytes inside each half
/// are tolerated per [`parse_frame`]); any malformed half is an error and
/// the caller drops the whole double. For the lossy kernel-like behavior
/// (process the good half even when its sibling is malformed), use
/// [`parse_double_report_partial`].
pub fn parse_double_report(report: &[u8]) -> Result<(TouchFrame, TouchFrame)> {
    if report.first() != Some(&DOUBLE_REPORT_ID) {
        bail!("not a double report (missing 0xf7 id)");
    }
    if report.len() < 2 {
        bail!("short double report: no length byte");
    }
    let first_len = usize::from(report[1]);
    if report.len() < 2 + first_len {
        bail!(
            "short double report: claims first_len {first_len}, have {}",
            report.len() - 2
        );
    }
    let first = parse_frame(&report[2..2 + first_len])?;
    let second = parse_frame(&report[2 + first_len..])?;
    Ok((first, second))
}

/// Lossy split of a `0xf7` double report: decode each half independently
/// and return every good half (0–2 frames), mirroring the kernel
/// `hid-magicmouse.c` path which processes each sub-report on its own.
///
/// A malformed half is skipped with a `debug!` log instead of discarding
/// its good sibling. Outer framing errors (missing id/length, `first_len`
/// overshoot) yield an empty vec. An empty second half (single-subreport
/// double) is treated as absent, not malformed.
///
/// NOTE: the live pump still calls [`parse_double_report`]; it should
/// migrate to this helper so one bad half can no longer drop good contacts.
pub fn parse_double_report_partial(report: &[u8]) -> Vec<TouchFrame> {
    if report.first() != Some(&DOUBLE_REPORT_ID) {
        log::debug!("double report: missing 0xf7 id, dropping");
        return Vec::new();
    }
    if report.len() < 2 {
        log::debug!("double report: short, no length byte");
        return Vec::new();
    }
    let first_len = usize::from(report[1]);
    if report.len() < 2 + first_len {
        log::debug!(
            "double report: claims first_len {first_len}, have {}; dropping",
            report.len() - 2
        );
        return Vec::new();
    }
    let mut out = Vec::with_capacity(2);
    let first_slice = &report[2..2 + first_len];
    match parse_frame(first_slice) {
        Ok(f) => out.push(f),
        Err(e) => log::debug!(
            "double report: dropping malformed first half ({} B): {e:#}",
            first_slice.len()
        ),
    }
    let second_slice = &report[2 + first_len..];
    if second_slice.is_empty() {
        return out;
    }
    match parse_frame(second_slice) {
        Ok(f) => out.push(f),
        Err(e) => log::debug!(
            "double report: dropping malformed second half ({} B): {e:#}",
            second_slice.len()
        ),
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn contact_bytes(patch: &[(usize, u8)]) -> [u8; 9] {
        let mut b = [0u8; 9];
        for &(i, v) in patch {
            b[i] = v;
        }
        b
    }

    #[test]
    fn tier_contract_is_l2p_arbitrary_not_grams() {
        let dev = AppleHidraw;
        assert_eq!(dev.tier(), CapabilityTier::L2p);
        assert_eq!(dev.pressure_units(), PressureUnits::Arbitrary);
        assert_ne!(dev.pressure_units(), PressureUnits::Grams);
    }

    #[test]
    fn poll_frame_errors_without_io() {
        let mut dev = AppleHidraw;
        let err = dev.poll_frame().unwrap_err().to_string();
        assert!(err.contains("uinput-proxy"), "unexpected: {err}");
    }

    #[test]
    fn contact_decode_known_vector() {
        // Hand-verified against 32-bit wrap + arithmetic shift.
        let b = [0x34, 0x12, 0x78, 0x85, 0x10, 0x20, 0x30, 0x64, 0x45];
        let c = parse_contact(&b).unwrap();
        assert_eq!(c.id, 0x5);
        assert_eq!((c.x, c.y), (-3532, -3008));
        assert_eq!(c.touch_major, 0x10);
        assert_eq!(c.touch_minor, 0x20);
        assert_eq!(c.size, 0x30);
        assert_eq!(c.pressure, 0x64);
        assert_eq!(c.orientation, -2); // (0x45 >> 5) - 4 = 2 - 4
        assert!(c.contact); // 0x85 & 0xC0 == 0x80
    }

    #[test]
    fn contact_negative_x_edge() {
        let b = [0xFF, 0x1F, 0x00, 0x80, 0x01, 0x02, 0x7F, 0xFD, 0x27];
        let c = parse_contact(&b).unwrap();
        assert_eq!((c.x, c.y), (-1, 0));
        assert_eq!(c.pressure, 253);
        assert!(c.contact);
    }

    #[test]
    fn contact_liftoff_state() {
        // state 0x00 (and 0x40) count as no-contact; only 0x80 is down.
        for state in [0x00u8, 0x40, 0xC0] {
            let b = contact_bytes(&[(3, state), (7, 100)]);
            assert!(!parse_contact(&b).unwrap().contact, "state {state:#04x}");
        }
        let b = contact_bytes(&[(3, 0x80), (7, 100)]);
        assert!(parse_contact(&b).unwrap().contact);
    }

    #[test]
    fn pressure_boundaries_pass_through() {
        for p in [0u8, 1, 253] {
            let c = parse_contact(&contact_bytes(&[(3, 0x80), (7, p)])).unwrap();
            assert_eq!(c.pressure, p);
        }
        assert_eq!(PRESSURE_VALID_MAX, 253);
        // Unobserved/reserved bytes pass through (kernel reports unclamped);
        // the uinput stage MUST clamp to PRESSURE_VALID_MAX.
        for p in [254u8, 255] {
            let c = parse_contact(&contact_bytes(&[(3, 0x80), (7, p)])).unwrap();
            assert_eq!(c.pressure, p);
        }
    }

    #[test]
    fn short_block_rejected() {
        assert!(parse_contact(&[0u8; 8]).is_err());
        assert!(parse_contact(&[]).is_err());
        assert!(parse_contact(&[0u8; 9]).is_ok());
        // Longer slices consume the first 9 bytes.
        assert!(parse_contact(&[0u8; 18]).is_ok());
    }

    #[test]
    fn usb_frame_two_contacts_ids_tracked() {
        let mut report = vec![0x02, 0x01, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0];
        report.extend_from_slice(&contact_bytes(&[(3, 0x80), (7, 10), (8, 0x02)]));
        report.extend_from_slice(&contact_bytes(&[(3, 0x80), (7, 200), (8, 0x09)]));
        let frame = parse_frame(&report).unwrap();
        assert_eq!(frame.clicks, 0x01);
        assert_eq!(frame.timestamp_ms, 0);
        assert_eq!(frame.fingers.len(), 2);
        assert_eq!(frame.fingers[0].id, 2);
        assert_eq!(frame.fingers[0].pressure, 10);
        assert_eq!(frame.fingers[1].id, 9);
        assert_eq!(frame.fingers[1].pressure, 200);
    }

    #[test]
    fn ids_stable_across_frames() {
        // Same tracking ids in consecutive frames = same fingers.
        let mk = |p0: u8, p1: u8| {
            let mut r = vec![0x31, 0x00, 0, 0];
            r.extend_from_slice(&contact_bytes(&[(3, 0x80), (7, p0), (8, 0x03)]));
            r.extend_from_slice(&contact_bytes(&[(3, 0x80), (7, p1), (8, 0x07)]));
            r
        };
        let a = parse_frame(&mk(50, 60)).unwrap();
        let b = parse_frame(&mk(55, 62)).unwrap();
        let ids_a: Vec<u8> = a.fingers.iter().map(|f| f.id).collect();
        let ids_b: Vec<u8> = b.fingers.iter().map(|f| f.id).collect();
        assert_eq!(ids_a, [3, 7]);
        assert_eq!(ids_a, ids_b);
        assert_eq!(b.fingers[0].pressure, 55);
    }

    #[test]
    fn bt_frame_prefix4() {
        let mut report = vec![0x31, 0x00, 0, 0];
        report.extend_from_slice(&contact_bytes(&[(3, 0x80), (7, 253), (8, 0x01)]));
        let frame = parse_frame(&report).unwrap();
        assert_eq!(frame.fingers.len(), 1);
        assert_eq!(frame.fingers[0].pressure, 253);
    }

    #[test]
    fn empty_contact_frame_ok() {
        let usb = vec![0x02, 0x00, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0];
        let frame = parse_frame(&usb).unwrap();
        assert!(frame.fingers.is_empty());
    }

    #[test]
    fn max_contacts_ok_16th_rejected() {
        let mut report = vec![0x02, 0x00, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0];
        for i in 0..15u8 {
            let mut c = contact_bytes(&[(3, 0x80), (7, 100)]);
            c[8] = i & 0x0f;
            report.extend_from_slice(&c);
        }
        assert_eq!(parse_frame(&report).unwrap().fingers.len(), 15);
        let mut c = contact_bytes(&[(3, 0x80), (7, 100)]);
        c[8] = 0x0F;
        report.extend_from_slice(&c);
        assert!(parse_frame(&report).is_err());
    }

    #[test]
    fn malformed_frames_rejected() {
        assert!(parse_frame(&[]).is_err()); // empty
        assert!(parse_frame(&[0x02]).is_err()); // shorter than prefix
        assert!(parse_frame(&[0x02, 0, 0]).is_err()); // shorter than prefix
                                                      // Trailing bytes with zero full contacts are still an error
                                                      // (nothing good to save; pump relies on this for pure garbage).
        let mut r = vec![0x02, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0];
        r.extend_from_slice(&[0u8; 8]);
        assert!(parse_frame(&r).is_err());
        let mut bt = vec![0x31, 0, 0, 0];
        bt.extend_from_slice(&[0u8; 8]);
        assert!(parse_frame(&bt).is_err());
        assert!(parse_frame(&[0xFF, 0, 0, 0]).is_err()); // unknown id
                                                         // 0x28 is a BT alias now (see legacy_0x28_* tests), not an error.
                                                         // 0xf7 must go through parse_double_report, not parse_frame.
        assert!(parse_frame(&[0xF7, 0]).is_err());
    }

    #[test]
    fn double_report_splits() {
        let sub1 = {
            let mut r = vec![0x31, 0x01, 0, 0];
            r.extend_from_slice(&contact_bytes(&[(3, 0x80), (7, 11), (8, 0x00)]));
            r
        };
        let sub2 = {
            let mut r = vec![0x31, 0x00, 0, 0];
            r.extend_from_slice(&contact_bytes(&[(3, 0x80), (7, 22), (8, 0x01)]));
            r
        };
        let mut dbl = vec![0xF7, sub1.len() as u8];
        dbl.extend_from_slice(&sub1);
        dbl.extend_from_slice(&sub2);
        let (a, b) = parse_double_report(&dbl).unwrap();
        assert_eq!(a.fingers[0].pressure, 11);
        assert_eq!(b.fingers[0].pressure, 22);
        assert_eq!(a.clicks, 1);
        assert!(parse_double_report(&sub1).is_err());
        assert!(parse_double_report(&[0xF7]).is_err());
    }

    #[test]
    fn byte1_high_bits_are_y_not_x() {
        // b[1] bits 5..7 are Y-low; they must not leak into x.
        let a = parse_contact(&[0x00, 0x00, 0x00, 0x80, 0, 0, 0, 50, 0]).unwrap();
        let b = parse_contact(&[0x00, 0xE0, 0x00, 0x80, 0, 0, 0, 50, 0]).unwrap();
        assert_eq!((a.x, a.y), (0, 0));
        assert_eq!(b.x, a.x);
        assert_eq!(b.y, -7);
    }

    #[test]
    fn double_report_rejects_malformed() {
        let good_sub = |clicks: u8, pressure: u8| {
            let mut r = vec![0x31, clicks, 0, 0];
            r.extend_from_slice(&contact_bytes(&[(3, 0x80), (7, pressure), (8, 0x01)]));
            r
        };
        // Overshoot: claims 255 first bytes, far fewer present.
        let mut overshoot = vec![0xF7, 255];
        overshoot.extend_from_slice(&good_sub(0, 10));
        assert!(parse_double_report(&overshoot).is_err());
        // Misaligned split: first_len lands mid-9B-block (body 8, remainder).
        let mut misaligned = vec![0xF7, 12];
        misaligned.extend_from_slice(&[0x31, 0, 0, 0, 1, 2, 3, 4, 5, 6, 7, 8]);
        misaligned.extend_from_slice(&good_sub(0, 10));
        assert!(parse_double_report(&misaligned).is_err());
        // Nested 0xf7 as second frame.
        let s1 = good_sub(0, 10);
        let mut nested = vec![0xF7, s1.len() as u8];
        nested.extend_from_slice(&s1);
        nested.extend_from_slice(&[0xF7, 0x00]);
        assert!(parse_double_report(&nested).is_err());
        // Trailing garbage after a full contact inside the second half is
        // tolerated now (floor-parse): strict still succeeds.
        let mut s2 = good_sub(0, 20);
        s2.push(0xFF);
        let mut trailing = vec![0xF7, s1.len() as u8];
        trailing.extend_from_slice(&s1);
        trailing.extend_from_slice(&s2);
        let (t1, t2) = parse_double_report(&trailing).unwrap();
        assert_eq!(t1.fingers.len(), 1);
        assert_eq!(t2.fingers.len(), 1);
        assert_eq!(t2.fingers[0].pressure, 20);
    }

    #[test]
    fn legacy_0x28_alias_of_0x31_one_and_two_contacts() {
        // 0x28 decodes identically to 0x31: same 4-byte prefix, same 9B blocks.
        let mk = |id: u8, pressures: &[u8]| {
            let mut r = vec![id, 0x01, 0, 0];
            for (i, &p) in pressures.iter().enumerate() {
                let mut c = contact_bytes(&[(3, 0x80), (7, p)]);
                c[8] = (i as u8) & 0x0f;
                r.extend_from_slice(&c);
            }
            r
        };
        for pressures in [vec![42u8], vec![10u8, 200u8]] {
            let bt = parse_frame(&mk(0x31, &pressures)).unwrap();
            let legacy = parse_frame(&mk(0x28, &pressures)).unwrap();
            assert_eq!(legacy, bt, "0x28 must match 0x31 for {pressures:?}");
            assert_eq!(legacy.fingers.len(), pressures.len());
            assert_eq!(legacy.clicks, 0x01);
            for (f, &p) in legacy.fingers.iter().zip(&pressures) {
                assert_eq!(f.pressure, p);
                assert!(f.contact);
            }
        }
        // Bare 0x28 prefix with no contacts is an empty frame like 0x31.
        assert!(parse_frame(&[0x28, 0, 0, 0]).unwrap().fingers.is_empty());
    }

    #[test]
    fn trailing_bytes_tolerated_bt_floor_parse() {
        // BT frame: 4-byte prefix + N*9 contacts + k trailing bytes (k=1..8).
        // Must return the N good contacts, ignoring the tail.
        for n in [1usize, 2] {
            for k in 1..CONTACT_BLOCK_LEN {
                let mut report = vec![0x31, 0x00, 0, 0];
                for i in 0..n {
                    let mut c = contact_bytes(&[(3, 0x80), (7, 50 + i as u8), (8, i as u8)]);
                    c[8] = i as u8 & 0x0f;
                    report.extend_from_slice(&c);
                }
                report.extend_from_slice(&vec![0xAA; k]);
                let frame = parse_frame(&report)
                    .unwrap_or_else(|e| panic!("N={n} k={k} should tolerate trailing: {e:#}"));
                assert_eq!(frame.fingers.len(), n, "N={n} k={k}");
                for (i, f) in frame.fingers.iter().enumerate() {
                    assert_eq!(f.pressure, 50 + i as u8);
                    assert!(f.contact);
                }
            }
        }
    }

    #[test]
    fn trailing_bytes_tolerated_usb_with_contacts() {
        // USB decode itself is unchanged for well-formed frames; trailing
        // bytes after >=1 full contact are ignored the same way as BT.
        // Sweep k=1..8 like the BT test (every non-block-aligned tail).
        for k in 1..CONTACT_BLOCK_LEN {
            let mut report = vec![0x02, 0x01, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0];
            report.extend_from_slice(&contact_bytes(&[(3, 0x80), (7, 77), (8, 0x04)]));
            report.extend_from_slice(&vec![0x55; k]);
            let frame = parse_frame(&report)
                .unwrap_or_else(|e| panic!("USB k={k} should tolerate trailing: {e:#}"));
            assert_eq!(frame.fingers.len(), 1, "USB k={k}");
            assert_eq!(frame.fingers[0].pressure, 77);
            assert_eq!(frame.fingers[0].id, 4);
            assert_eq!(frame.clicks, 0x01);
        }
    }

    #[test]
    fn double_partial_returns_good_half_when_other_bad() {
        let good_sub = |clicks: u8, pressure: u8| {
            let mut r = vec![0x31, clicks, 0, 0];
            r.extend_from_slice(&contact_bytes(&[(3, 0x80), (7, pressure), (8, 0x01)]));
            r
        };
        // Bad second half (unknown id): strict fails, partial keeps first.
        let s1 = good_sub(1, 10);
        let mut dbl_bad_second = vec![0xF7, s1.len() as u8];
        dbl_bad_second.extend_from_slice(&s1);
        dbl_bad_second.extend_from_slice(&[0xFF, 0x00, 0x00, 0x00]);
        assert!(parse_double_report(&dbl_bad_second).is_err());
        let partial = parse_double_report_partial(&dbl_bad_second);
        assert_eq!(partial.len(), 1);
        assert_eq!(partial[0].fingers[0].pressure, 10);
        assert_eq!(partial[0].clicks, 1);
        // Bad first half (misaligned: body 8 with zero full contacts):
        // strict fails, partial keeps second.
        let s2 = good_sub(0, 20);
        let mut dbl_bad_first = vec![0xF7, 12u8];
        dbl_bad_first.extend_from_slice(&[0x31, 0, 0, 0, 1, 2, 3, 4, 5, 6, 7, 8]);
        dbl_bad_first.extend_from_slice(&s2);
        assert!(parse_double_report(&dbl_bad_first).is_err());
        let partial = parse_double_report_partial(&dbl_bad_first);
        assert_eq!(partial.len(), 1);
        assert_eq!(partial[0].fingers[0].pressure, 20);
        // Both good: partial returns both in order.
        let a = good_sub(1, 11);
        let b = good_sub(0, 22);
        let mut both = vec![0xF7, a.len() as u8];
        both.extend_from_slice(&a);
        both.extend_from_slice(&b);
        let partial = parse_double_report_partial(&both);
        assert_eq!(partial.len(), 2);
        assert_eq!(partial[0].fingers[0].pressure, 11);
        assert_eq!(partial[1].fingers[0].pressure, 22);
        // Outer framing broken: empty vec, no panic.
        assert!(parse_double_report_partial(&[0xF7, 255, 0x31]).is_empty());
        assert!(parse_double_report_partial(&[0x31, 0, 0, 0]).is_empty());
    }
}
