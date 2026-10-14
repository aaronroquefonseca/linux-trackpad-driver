//! Generic (non-Apple) evdev trackpad source — Tier-0/1 scaffold.
//!
//! Phase-2 groundwork only: declares the tier/`pressure_units` contract,
//! probes device capability (pressure axis present?), and stubs frame
//! decoding with a clear error. Apple parser, clicks, proxy, init, config,
//! udev, and CI are untouched.
//!
//! # Tier mapping (`docs/RESEARCH.md` §4/§6)
//!
//! | Tier | Pressure | Haptic | Members |
//! |------|----------|--------|---------|
//! | L0 | none (binary click, area-only) | none | Framework 13/16-shipping, ASUS ErgoSense |
//! | L1a | area-proxy (`res == 0`) | none | most PTP I2C-HID pads |
//! | L1b | true force (grams/N, `res != 0`) | none | calibrated pads, Sensel `0x09` path |
//! | L2s | `Tip Pressure` + standard `0x0E` | host-controlled | Elan 2703 / kernel 6.18 `hid-haptic` |
//! | L2p | proprietary pressure | device-autonomous | Apple MT2 (`0x22/0x23+0x53`), Sensel shipping FW |
//!
//! This stub only ever declares [`CapabilityTier::L0`] or
//! [`CapabilityTier::L1a`]: pressure axis absent → L0, present → L1a
//! (area-proxy assumption). L1b/L2s/L2p require Phase-2 verification
//! (units calibration, `0x0E` waveforms, proprietary decode) and are never
//! claimed here.
//!
//! # `pressure_units` contract
//!
//! [`PressureUnits::Arbitrary`] means `res == 0`: a size-proxy byte with no
//! gram mapping — thresholds must not treat it as force. [`PressureUnits::Grams`]
//! means calibrated force (`res != 0`, grams/newtons). The L1a/L1b split is
//! machine-checked by this pair: `tier() == L1a` always pairs with
//! `Arbitrary` here; a future L1b claim must pair with `Grams` plus a
//! calibration source. The stub never returns `Grams` (no `ABS_MT_PRESSURE`
//! resolution probe is wired yet). Downstream keeps `res == 0` on any virtual
//! device and never advertises `EV_FF`/`FF_HAPTIC` (no 2026 libinput/Wayland
//! consumer — see RESEARCH §6).
//!
//! # Capability probe rule
//!
//! Real Phase-2 probe: `ABS_MT_PRESSURE` (or `ABS_PRESSURE`) advertised by the
//! evdev node → L1a candidate; absent → L0. Until the evdev transport lands,
//! the caller declares the outcome via [`EvdevGeneric::l0`] /
//! [`EvdevGeneric::l1a`] / [`EvdevGeneric::from_probe`]. `poll_frame` stays a
//! stub until Phase-2 wires MT-slot decode.

use anyhow::{bail, Result};

use crate::apple_parse::{CapabilityTier, PressureUnits, TouchFrame, TouchSource};

/// Device capability probe outcome (Phase-2 will fill this from evdev
/// `ABS_MT_PRESSURE` / `ABS_PRESSURE` advertisement + `res`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EvdevCaps {
    /// True when the node advertises a pressure axis. Assumed area-proxy
    /// (`res == 0`, L1a) until Phase-2 verifies resolution/units.
    pub has_pressure_axis: bool,
}

impl EvdevCaps {
    /// Probe outcome: no pressure axis → L0 device.
    pub fn without_pressure() -> Self {
        Self {
            has_pressure_axis: false,
        }
    }

    /// Probe outcome: pressure axis present → L1a candidate (area-proxy).
    pub fn with_pressure() -> Self {
        Self {
            has_pressure_axis: true,
        }
    }
}

/// Generic evdev trackpad source, Tier-0/1 scaffold (Phase-1 stub).
///
/// Attach/detach is pure state (no fd, no ioctl): Phase-2 owns transport.
/// `poll_frame` always errors naming Phase-2 so a stubbed source can never
/// be mistaken for live input.
#[derive(Debug)]
pub struct EvdevGeneric {
    has_pressure_axis: bool,
    attached: bool,
}

impl EvdevGeneric {
    /// Declare an L0 device (no pressure axis).
    pub fn l0() -> Self {
        Self::from_probe(false)
    }

    /// Declare an L1a device (pressure axis present, assumed area-proxy).
    pub fn l1a() -> Self {
        Self::from_probe(true)
    }

    /// Declare from a capability-probe outcome. `true` = pressure axis
    /// present → L1a; `false` → L0.
    pub fn from_probe(has_pressure_axis: bool) -> Self {
        Self {
            has_pressure_axis,
            attached: false,
        }
    }

    /// Declare from an [`EvdevCaps`] probe struct.
    pub fn from_caps(caps: EvdevCaps) -> Self {
        Self::from_probe(caps.has_pressure_axis)
    }

    /// Whether the probe saw a pressure axis.
    pub fn has_pressure_axis(&self) -> bool {
        self.has_pressure_axis
    }

    /// Attach to the (future) evdev node. Idempotent, no I/O.
    pub fn attach(&mut self) {
        self.attached = true;
    }

    /// Detach from the node. Idempotent, no I/O.
    pub fn detach(&mut self) {
        self.attached = false;
    }

    /// True between [`EvdevGeneric::attach`] and [`EvdevGeneric::detach`].
    pub fn is_attached(&self) -> bool {
        self.attached
    }
}

impl TouchSource for EvdevGeneric {
    fn tier(&self) -> CapabilityTier {
        if self.has_pressure_axis {
            CapabilityTier::L1a
        } else {
            CapabilityTier::L0
        }
    }

    fn pressure_units(&self) -> PressureUnits {
        // Stub never claims grams: Grams needs res != 0 calibration, which
        // only Phase-2 can verify. Both L0 (no channel) and L1a (area-proxy)
        // report Arbitrary so thresholds never treat the byte as force.
        PressureUnits::Arbitrary
    }

    fn poll_frame(&mut self) -> Result<Option<TouchFrame>> {
        bail!("EvdevGeneric poll_frame unimplemented (Phase-2: evdev MT-slot decode not wired yet)")
    }
}

/// In-memory fake backend: exercises attach/detach + idle poll paths in
/// tests without hardware. Tier declaration mirrors [`EvdevGeneric`]; poll
/// returns idle (`Ok(None)`) instead of the stub error so the attach flow can
/// be driven end to end.
#[derive(Debug)]
pub struct MockBackend {
    inner: EvdevGeneric,
    attach_calls: u32,
    detach_calls: u32,
}

impl MockBackend {
    /// Fake L0 device (no pressure axis).
    pub fn l0() -> Self {
        Self::for_probe(false)
    }

    /// Fake L1a device (pressure axis present, area-proxy).
    pub fn l1a() -> Self {
        Self::for_probe(true)
    }

    fn for_probe(has_pressure_axis: bool) -> Self {
        Self {
            inner: EvdevGeneric::from_probe(has_pressure_axis),
            attach_calls: 0,
            detach_calls: 0,
        }
    }

    /// Fake open: records the call, flips attached state.
    pub fn attach(&mut self) {
        self.attach_calls += 1;
        self.inner.attach();
    }

    /// Fake close: records the call, flips attached state.
    pub fn detach(&mut self) {
        self.detach_calls += 1;
        self.inner.detach();
    }

    /// True between [`MockBackend::attach`] and [`MockBackend::detach`].
    pub fn is_attached(&self) -> bool {
        self.inner.is_attached()
    }

    /// Number of [`MockBackend::attach`] calls (re-attach safe).
    pub fn attach_calls(&self) -> u32 {
        self.attach_calls
    }

    /// Number of [`MockBackend::detach`] calls.
    pub fn detach_calls(&self) -> u32 {
        self.detach_calls
    }
}

impl TouchSource for MockBackend {
    fn tier(&self) -> CapabilityTier {
        self.inner.tier()
    }

    fn pressure_units(&self) -> PressureUnits {
        self.inner.pressure_units()
    }

    fn poll_frame(&mut self) -> Result<Option<TouchFrame>> {
        // Idle fake: no frames queued, device quiet.
        Ok(None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tier_follows_pressure_probe() {
        let l0 = EvdevGeneric::l0();
        assert_eq!(l0.tier(), CapabilityTier::L0);
        assert_eq!(l0.pressure_units(), PressureUnits::Arbitrary);
        assert!(!l0.has_pressure_axis());

        let l1a = EvdevGeneric::l1a();
        assert_eq!(l1a.tier(), CapabilityTier::L1a);
        assert_eq!(l1a.pressure_units(), PressureUnits::Arbitrary);
        assert!(l1a.has_pressure_axis());

        // from_probe / from_caps agree.
        assert_eq!(
            EvdevGeneric::from_probe(false).tier(),
            EvdevGeneric::from_caps(EvdevCaps::without_pressure()).tier()
        );
        assert_eq!(
            EvdevGeneric::from_probe(true).tier(),
            EvdevGeneric::from_caps(EvdevCaps::with_pressure()).tier()
        );
        // Stub never claims calibrated force.
        assert_ne!(l1a.pressure_units(), PressureUnits::Grams);
    }

    #[test]
    fn poll_frame_is_phase2_stub_error() {
        let mut dev = EvdevGeneric::l1a();
        let err = dev.poll_frame().unwrap_err().to_string();
        assert!(err.contains("Phase-2"), "unexpected: {err}");
        assert!(err.contains("poll_frame"), "unexpected: {err}");
    }

    #[test]
    fn mock_attach_detach_flow_without_hardware() {
        let mut mock = MockBackend::l1a();
        assert_eq!(mock.tier(), CapabilityTier::L1a);
        assert!(!mock.is_attached());

        mock.attach();
        assert!(mock.is_attached());
        assert_eq!(mock.attach_calls(), 1);
        // Idle poll while attached: no error, no frame.
        assert!(mock.poll_frame().unwrap().is_none());

        mock.attach(); // idempotent re-attach
        assert!(mock.is_attached());

        mock.detach();
        assert!(!mock.is_attached());
        assert_eq!(mock.detach_calls(), 1);

        let mut l0 = MockBackend::l0();
        assert_eq!(l0.tier(), CapabilityTier::L0);
        l0.attach();
        l0.detach();
        assert!(!l0.is_attached());
        assert_eq!((l0.attach_calls(), l0.detach_calls()), (1, 1));
    }
}
