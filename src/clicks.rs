//! Pressure state machine + click mapping + haptic decision + uinput mapping.
//!
//! Pure logic for the `uinput-proxy` task. No I/O, no evdev/uinput handles:
//! transport lives in [`crate::proxy`]. Reimplemented from
//! `docs/ARCHITECTURE.md` (click state machine) and `docs/RESEARCH.md` §6
//! (uinput contract); no reference code copied.
//!
//! # Contract
//!
//! - Per-finger pressure vs `sensitivity` (press) / `sensitivity - margin`
//!   (release hysteresis). Thresholds operate on the RAW pressure byte
//!   (Apple nominal 0..=253, arbitrary units, `res == 0`).
//! - Finger-count at the press edge selects `click_1..=click_5` via the
//!   existing [`crate::config::parse_click_action`].
//! - Force level (`force_offset`, `force_*`) is PARSED by config but OWNED by
//!   the force-click task: this module only exposes the arming threshold via
//!   [`ClickMachine::force_threshold`]; it never fires force actions.
//! - Haptic: `haptic == 0` = silent ([`should_emit_haptic`]); otherwise the
//!   proxy sends the host-click report (`0x21 0x01`, see [`crate::init`])
//!   with strength/quiet carried for logging. Strength byte mapping is
//!   unverified on hardware (proprietary `0x53` actuator per RESEARCH §2).
//! - uinput mapping: pressure MUST clamp to [`crate::apple_parse::PRESSURE_VALID_MAX`]
//!   (253, kernel `absmax`); orientation emits `-(raw)` per the kernel
//!   `hid-magicmouse` negation; physical `clicks` byte contributes
//!   `BTN = clicks & 1` (bits 6..7 are timestamp, not buttons) OR-ed into
//!   the click state.

use anyhow::Result;

use crate::apple_parse::{FingerContact, PRESSURE_VALID_MAX};
use crate::config::{ActionKind, ClickAction, Config};

/// Scale a pointer delta by `speed` percent (`100` = 1:1). Proxy path only.
pub fn scale_delta(delta: i32, speed: u32) -> i32 {
    ((i64::from(delta) * i64::from(speed)) / 100) as i32
}

/// Clamp a raw pressure byte to the kernel `absmax` (253). Values 254..=255
/// are reserved passthrough from the parser and must never hit the wire.
pub fn clamp_pressure(raw: u8) -> u8 {
    raw.min(PRESSURE_VALID_MAX)
}

/// Negate raw orientation for `ABS_MT_ORIENTATION` (kernel parity).
pub fn negate_orientation(raw: i8) -> i8 {
    raw.wrapping_neg()
}

/// Whether haptic feedback fires (`haptic == 0` = silent).
pub fn should_emit_haptic(haptic: u16) -> bool {
    haptic > 0
}

/// Press threshold = `sensitivity` (1..=255).
pub fn press_threshold(cfg: &Config) -> u8 {
    cfg.sensitivity.clamp(1, 255) as u8
}

/// Release threshold = `sensitivity - margin`, saturating at 0.
pub fn release_threshold(cfg: &Config) -> u8 {
    let s = cfg.sensitivity.clamp(1, 255);
    let m = cfg.margin.min(255);
    s.saturating_sub(m) as u8
}

/// A fired click: which fingers caused it and what to do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClickEvent {
    /// Finger count at the press edge (1..=5, clamped).
    pub finger_count: u8,
    /// Parsed `click_N` action.
    pub action: ClickAction,
    /// Haptic strength to emit (0 = silent, decided by caller too).
    pub haptic: u16,
    /// Soft waveform flag.
    pub quiet: bool,
}

impl ClickEvent {
    /// True when no keys/buttons should be emitted.
    pub fn is_none(&self) -> bool {
        matches!(self.action.kind, ActionKind::None)
    }
    /// True when haptic fires but no keys/buttons are sent.
    pub fn is_empty(&self) -> bool {
        matches!(self.action.kind, ActionKind::Empty)
    }
}

/// Per-finger + global click state.
///
/// Tracks one pressed bit per tracking id. A press edge on ANY finger while
/// the global click is idle captures `finger_count` (contacting fingers in
/// that frame, clamped 1..=5) and resolves `click_N`. Release happens when
/// every pressed finger falls to/below the release threshold (or lifts).
/// Physical button (`clicks & 1`) is folded in by the caller via
/// [`ClickMachine::physical_button`].
#[derive(Debug, Default)]
pub struct ClickMachine {
    pressed: std::collections::HashMap<u8, bool>,
    active: Option<ClickEvent>,
}

impl ClickMachine {
    pub fn new() -> Self {
        Self::default()
    }

    /// Currently latched click, if any.
    pub fn active(&self) -> Option<&ClickEvent> {
        self.active.as_ref()
    }

    /// Whether tracking id `id` is currently pressed.
    pub fn finger_pressed(&self, id: u8) -> bool {
        self.pressed.get(&id).copied().unwrap_or(false)
    }

    /// Force-level arming threshold (`sensitivity + force_offset`), if the
    /// force task arms it. Exposed for that task; never fired here.
    pub fn force_threshold(cfg: &Config) -> Option<u16> {
        if cfg.force_offset > 0 {
            Some(cfg.sensitivity + cfg.force_offset)
        } else {
            None
        }
    }

    /// Physical button bit from the raw `clicks` byte (`clicks & 1`).
    pub fn physical_button(clicks: u8) -> bool {
        clicks & 1 == 1
    }

    /// Feed one decoded frame. Returns `Some(ClickEvent)` on a fresh press
    /// edge (including `none`/`empty` so the caller can decide haptic), or
    /// `None` when steady-state/released. Parse errors on `click_N` are
    /// returned as `Err` (caller logs + drops the frame edge, never aborts).
    pub fn update(
        &mut self,
        fingers: &[FingerContact],
        clicks: u8,
        cfg: &Config,
    ) -> Result<Option<ClickEvent>> {
        let press = press_threshold(cfg);
        let release = release_threshold(cfg);
        let _ = clicks;

        // Update per-finger hysteresis.
        let mut newly_pressed = Vec::new();
        for f in fingers {
            if !f.contact {
                self.pressed.insert(f.id, false);
                continue;
            }
            let was = self.finger_pressed(f.id);
            if was {
                if f.pressure <= release {
                    self.pressed.insert(f.id, false);
                }
            } else if f.pressure >= press {
                self.pressed.insert(f.id, true);
                newly_pressed.push(f.id);
            }
        }
        // Fingers absent from this frame keep their bit unless the frame is
        // empty (all lifted).
        if fingers.is_empty() {
            for v in self.pressed.values_mut() {
                *v = false;
            }
        }

        // Release the global click when nothing remains pressed.
        if self.active.is_some() && !self.pressed.values().any(|&p| p) {
            self.active = None;
            return Ok(None);
        }

        // Press edge: first newly-pressed finger while idle latches.
        if self.active.is_none() && !newly_pressed.is_empty() {
            let contacting = fingers.iter().filter(|f| f.contact).count();
            let n = (contacting.clamp(1, 5)) as u8;
            let action = cfg.click_action(n)?;
            let ev = ClickEvent {
                finger_count: n,
                action,
                haptic: cfg.haptic,
                quiet: cfg.quiet_click,
            };
            // `none` still latches (swallows the gesture); caller emits
            // nothing and no haptic for it.
            self.active = Some(ev.clone());
            return Ok(Some(ev));
        }
        Ok(None)
    }

    /// Force-latch reset (detach path).
    pub fn reset(&mut self) {
        self.pressed.clear();
        self.active = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::apple_parse::FingerContact;

    fn finger(id: u8, pressure: u8) -> FingerContact {
        FingerContact {
            id,
            x: 0,
            y: 0,
            size: 10,
            orientation: 2,
            touch_major: 8,
            touch_minor: 6,
            pressure,
            contact: true,
        }
    }

    fn lifted(id: u8) -> FingerContact {
        FingerContact {
            contact: false,
            ..finger(id, 0)
        }
    }

    #[test]
    fn hysteresis_press_release_edges() {
        let cfg = Config {
            sensitivity: 64,
            margin: 5,
            ..Config::default()
        };
        assert_eq!(press_threshold(&cfg), 64);
        assert_eq!(release_threshold(&cfg), 59);
        let mut m = ClickMachine::new();
        // Below press: nothing.
        assert!(m.update(&[finger(1, 63)], 0, &cfg).unwrap().is_none());
        assert!(m.active().is_none());
        // At press: fires.
        let ev = m.update(&[finger(1, 64)], 0, &cfg).unwrap().unwrap();
        assert_eq!(ev.finger_count, 1);
        // Still pressed at 60 (above release 59): no re-fire.
        assert!(m.update(&[finger(1, 60)], 0, &cfg).unwrap().is_none());
        assert!(m.active().is_some());
        // At release threshold: drops.
        assert!(m.update(&[finger(1, 59)], 0, &cfg).unwrap().is_none());
        assert!(m.active().is_none());
    }

    #[test]
    fn release_saturates_at_zero() {
        let cfg = Config {
            sensitivity: 3,
            margin: 10,
            ..Config::default()
        };
        assert_eq!(release_threshold(&cfg), 0);
    }

    #[test]
    fn finger_count_selects_action() {
        let cfg = Config {
            click_1: "left".into(),
            click_2: "right".into(),
            click_3: "middle".into(),
            ..Config::default()
        };
        let mut m = ClickMachine::new();
        let fingers = vec![finger(1, 100), finger(2, 100)];
        let ev = m.update(&fingers, 0, &cfg).unwrap().unwrap();
        assert_eq!(ev.finger_count, 2);
        assert_eq!(ev.action.kind, ActionKind::Keys(vec!["right".to_string()]));
    }

    #[test]
    fn none_latches_but_marks_is_none() {
        let cfg = Config {
            click_1: "none".into(),
            ..Config::default()
        };
        let mut m = ClickMachine::new();
        let ev = m.update(&[finger(1, 200)], 0, &cfg).unwrap().unwrap();
        assert!(ev.is_none());
        assert!(!ev.is_empty());
        assert!(m.active().is_some());
    }

    #[test]
    fn empty_marks_haptic_only() {
        let cfg = Config {
            click_1: "empty".into(),
            haptic: 7,
            ..Config::default()
        };
        let mut m = ClickMachine::new();
        let ev = m.update(&[finger(1, 200)], 0, &cfg).unwrap().unwrap();
        assert!(ev.is_empty());
        assert!(!ev.is_none());
        assert!(should_emit_haptic(ev.haptic));
    }

    #[test]
    fn silent_haptic_when_zero() {
        assert!(!should_emit_haptic(0));
        assert!(should_emit_haptic(1));
    }

    #[test]
    fn clamp_and_negation_mapping() {
        assert_eq!(clamp_pressure(0), 0);
        assert_eq!(clamp_pressure(253), 253);
        assert_eq!(clamp_pressure(254), 253);
        assert_eq!(clamp_pressure(255), 253);
        assert_eq!(negate_orientation(3), -3);
        assert_eq!(negate_orientation(-4), 4);
    }

    #[test]
    fn physical_button_masks_bit0_only() {
        // Bits 6..7 are timestamp, not buttons.
        assert!(ClickMachine::physical_button(0x01));
        assert!(ClickMachine::physical_button(0xC1));
        assert!(!ClickMachine::physical_button(0xC0));
        assert!(!ClickMachine::physical_button(0x00));
    }

    #[test]
    fn scale_delta_speed() {
        assert_eq!(scale_delta(100, 100), 100);
        assert_eq!(scale_delta(100, 200), 200);
        assert_eq!(scale_delta(-40, 50), -20);
        assert_eq!(scale_delta(0, 1000), 0);
    }

    #[test]
    fn liftoff_releases_global_click() {
        let cfg = Config::default();
        let mut m = ClickMachine::new();
        m.update(&[finger(1, 200)], 0, &cfg).unwrap();
        assert!(m.active().is_some());
        m.update(&[lifted(1)], 0, &cfg).unwrap();
        // Contact=false clears the bit but the latch releases when no bits set.
        m.update(&[], 0, &cfg).unwrap();
        assert!(m.active().is_none());
    }

    #[test]
    fn force_threshold_exposed_not_fired() {
        let off = Config {
            force_offset: 20,
            sensitivity: 64,
            ..Config::default()
        };
        assert_eq!(ClickMachine::force_threshold(&off), Some(84));
        assert_eq!(ClickMachine::force_threshold(&Config::default()), None);
        // Even deep pressure only fires the standard level here.
        let mut m = ClickMachine::new();
        let ev = m.update(&[finger(1, 250)], 0, &off).unwrap().unwrap();
        assert_eq!(ev.finger_count, 1);
    }
}
