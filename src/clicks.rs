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
//! - Force second level: `force_offset > 0` arms force press at
//!   `sensitivity + force_offset` (`u16`, up to 510 — never overflows; a
//!   threshold above 255 simply never fires since pressure is a `u8`).
//!   Finger-count at the force edge selects `force_1..=force_5` with the same
//!   latch semantics as the first level. `force_offset = 0` disables the
//!   force level entirely (no behavior change).
//! - Force release hysteresis mirrors the first level: force releases at
//!   `sensitivity + force_offset - margin` (saturating at 0). Since the force
//!   press sits above the first press by `force_offset`, the saturating
//!   subtraction preserves ordering: force release >= first release, so the
//!   force level always drops at or before the first level.
//! - Coexistence ordering: first and force latches are independent and can
//!   both be held. Edges fire in press order (first then force) when both
//!   latch on the same frame; reporters surface the force edge (it
//!   supersedes while held past threshold). Pressure falling below the force
//!   release returns to the first-level hold; falling below the first-level
//!   release (`sensitivity - margin`) releases all.
//! - Force haptic: [`crate::config::Config::effective_force_haptic`] (explicit
//!   `force_haptic` override, else standard `haptic`; `Some(0)` is silent);
//!   waveform from `force_quiet_click`. Each edge fires its own pulse: a
//!   one-frame jump through both levels emits both (first then force),
//!   identical to staged arrival.
//! - Reporters surface [`FrameOutcome::primary`]: force while held past
//!   threshold, except a force `none` never supersedes (it emits nothing).
//! - `click_N`/`force_N` parse before any latch bit mutates: a bad action
//!   leaves state untouched for retry instead of half-latching.
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

/// Which pressure level fired a [`ClickEvent`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ClickLevel {
    /// Standard level (`click_N`, `haptic`, `quiet_click`).
    #[default]
    First,
    /// Second level (`force_N`, `effective_force_haptic()`, `force_quiet_click`).
    Force,
}

/// A fired click: which fingers caused it and what to do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClickEvent {
    /// Finger count at the press edge (1..=5, clamped).
    pub finger_count: u8,
    /// Parsed `click_N` / `force_N` action.
    pub action: ClickAction,
    /// Haptic strength to emit (0 = silent, decided by caller too).
    pub haptic: u16,
    /// Soft waveform flag.
    pub quiet: bool,
    /// Which level fired.
    pub level: ClickLevel,
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
    /// True for the second (force) level.
    pub fn is_force(&self) -> bool {
        matches!(self.level, ClickLevel::Force)
    }
}

/// Per-frame press edges, in press order (first then force).
///
/// When a deep press latches both levels on the same frame both are `Some`;
/// reporters surface `force` (it supersedes while held past threshold) while
/// holding both key sets.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FrameOutcome {
    /// Fresh first-level press edge, if any.
    pub first: Option<ClickEvent>,
    /// Fresh force-level press edge, if any.
    pub force: Option<ClickEvent>,
}

impl FrameOutcome {
    /// Superseding edge for reporters: force while held past threshold,
    /// else the first-level edge. A force `none` (swallowed gesture, no
    /// haptic) never supersedes: reporters see the first-level edge that
    /// actually emitted keys/haptic.
    pub fn primary(&self) -> Option<ClickEvent> {
        match &self.force {
            Some(f) if !f.is_none() => self.force.clone(),
            _ => self.first.clone(),
        }
    }
    /// True when neither level fired this frame.
    pub fn is_empty(&self) -> bool {
        self.first.is_none() && self.force.is_none()
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
    force_pressed: std::collections::HashMap<u8, bool>,
    force_active: Option<ClickEvent>,
}

impl ClickMachine {
    pub fn new() -> Self {
        Self::default()
    }

    /// Currently latched first-level click, if any.
    pub fn active(&self) -> Option<&ClickEvent> {
        self.active.as_ref()
    }

    /// Currently latched force-level click, if any.
    pub fn force_active(&self) -> Option<&ClickEvent> {
        self.force_active.as_ref()
    }

    /// Whether tracking id `id` is currently pressed (first level).
    pub fn finger_pressed(&self, id: u8) -> bool {
        self.pressed.get(&id).copied().unwrap_or(false)
    }

    /// Whether tracking id `id` is currently past the force threshold.
    pub fn finger_force_pressed(&self, id: u8) -> bool {
        self.force_pressed.get(&id).copied().unwrap_or(false)
    }

    /// Force-level arming threshold (`sensitivity + force_offset`, `u16` so
    /// `255 + offset` never overflows; thresholds above 255 simply never
    /// fire). Clamps mirror [`press_threshold`] (`1..=255`) and
    /// [`release_threshold`] (`margin` capped at 255). `None` when
    /// `force_offset == 0` (force disabled entirely).
    pub fn force_threshold(cfg: &Config) -> Option<u16> {
        if cfg.force_offset > 0 {
            Some(
                cfg.sensitivity
                    .clamp(1, 255)
                    .saturating_add(cfg.force_offset.min(255)),
            )
        } else {
            None
        }
    }

    /// Force release point: `sensitivity + force_offset - margin`, saturating
    /// at 0 (mirrors [`release_threshold`]). `None` when disabled.
    pub fn force_release_threshold(cfg: &Config) -> Option<u16> {
        Self::force_threshold(cfg).map(|press| press.saturating_sub(cfg.margin.min(255)))
    }

    /// Physical button bit from the raw `clicks` byte (`clicks & 1`).
    pub fn physical_button(clicks: u8) -> bool {
        clicks & 1 == 1
    }

    /// Feed one decoded frame. Returns fresh press edges (including
    /// `none`/`empty` so the caller can decide haptic), in press order:
    /// `first` then `force`. Steady-state/released frames yield `None`s.
    /// `click_N`/`force_N` are parsed BEFORE any latch bit mutates, so a
    /// parse error returns `Err` with state untouched (caller logs + drops
    /// the frame, never aborts; the next frame retries the edge instead of
    /// swallowing a half-latched click).
    ///
    /// Force fires only when `force_offset > 0` and some finger's pressure
    /// (`u16`-compared, no overflow) reaches `sensitivity + force_offset`;
    /// finger-count is captured at the force edge like the first level.
    /// Releases use hysteresis: force drops when no force bit remains (every
    /// deep finger fell to/below `sensitivity + force_offset - margin` or
    /// lifted) while the first-level hold continues; the first level drops
    /// when nothing remains above `sensitivity - margin`.
    ///
    /// If `force_offset` returns to 0 mid-hold the force latch + bits clear
    /// on that frame (the proxy sees the `force_active` transition and
    /// releases force-held keys, retaining codes the first level still
    /// holds). No re-attach needed.
    ///
    /// `_clicks` (raw `report[1]`) is intentionally unused here: the physical
    /// button bit (`clicks & 1`) is folded in by the proxy via
    /// [`ClickMachine::physical_button`], keeping pressure latching and the
    /// hardware button on separate, testable paths.
    pub fn update(
        &mut self,
        fingers: &[FingerContact],
        _clicks: u8,
        cfg: &Config,
    ) -> Result<FrameOutcome> {
        let press = press_threshold(cfg);
        let release = release_threshold(cfg);
        let force_press = Self::force_threshold(cfg);
        let force_release = Self::force_release_threshold(cfg);
        let armed = force_press.is_some();

        // Phase 1 (read-only): decide per-finger bit writes. Nothing mutates
        // here so a parse failure below leaves state untouched.
        let mut first_write = Vec::new();
        let mut newly_pressed = Vec::new();
        for f in fingers {
            if !f.contact {
                first_write.push((f.id, false));
                continue;
            }
            let was = self.finger_pressed(f.id);
            if was {
                if f.pressure <= release {
                    first_write.push((f.id, false));
                }
            } else if f.pressure >= press {
                first_write.push((f.id, true));
                newly_pressed.push(f.id);
            }
        }
        // Force level: same shape, u16 compare so `sensitivity + offset`
        // above 255 simply never fires.
        let mut force_write = Vec::new();
        let mut newly_force = Vec::new();
        if let (Some(fp), Some(fr)) = (force_press, force_release) {
            for f in fingers {
                if !f.contact {
                    force_write.push((f.id, false));
                    continue;
                }
                let p = u16::from(f.pressure);
                let was = self.finger_force_pressed(f.id);
                if was {
                    if p <= fr {
                        force_write.push((f.id, false));
                    }
                } else if p >= fp {
                    force_write.push((f.id, true));
                    newly_force.push(f.id);
                }
            }
        }

        // Phase 2: parse before mutating. `n` is shared: both edges capture
        // the contacting count of their edge frame, clamped 1..=5.
        let contacting = fingers.iter().filter(|f| f.contact).count();
        let n = (contacting.clamp(1, 5)) as u8;
        let first_action = if self.active.is_none() && !newly_pressed.is_empty() {
            Some(cfg.click_action(n)?)
        } else {
            None
        };
        let force_action = if armed && self.force_active.is_none() && !newly_force.is_empty() {
            Some(cfg.force_action(n)?)
        } else {
            None
        };

        // Phase 3: commit bit writes.
        for (id, bit) in first_write {
            self.pressed.insert(id, bit);
        }
        for (id, bit) in force_write {
            self.force_pressed.insert(id, bit);
        }
        // Fingers absent from this frame keep their bits unless the frame is
        // empty (all lifted).
        if fingers.is_empty() {
            for v in self.pressed.values_mut() {
                *v = false;
            }
            for v in self.force_pressed.values_mut() {
                *v = false;
            }
        }
        if !armed {
            // Disarmed mid-hold: drop the force latch + bits outright.
            self.force_active = None;
            for v in self.force_pressed.values_mut() {
                *v = false;
            }
        }

        // Release latches with no remaining bits (force first so a full
        // release clears both; first-level release implies force is gone too
        // since force release >= first release).
        if self.force_active.is_some() && !self.force_pressed.values().any(|&p| p) {
            self.force_active = None;
        }
        if self.active.is_some() && !self.pressed.values().any(|&p| p) {
            self.active = None;
            self.force_active = None;
            for v in self.force_pressed.values_mut() {
                *v = false;
            }
        }

        let mut out = FrameOutcome::default();

        // Press edge: first newly-pressed finger while idle latches.
        if let Some(action) = first_action {
            let ev = ClickEvent {
                finger_count: n,
                action,
                haptic: cfg.haptic,
                quiet: cfg.quiet_click,
                level: ClickLevel::First,
            };
            // `none` still latches (swallows the gesture); caller emits
            // nothing and no haptic for it.
            self.active = Some(ev.clone());
            out.first = Some(ev);
        }
        // Force edge: same latch shape, disabled entirely when offset is 0.
        if let Some(action) = force_action {
            let ev = ClickEvent {
                finger_count: n,
                action,
                haptic: cfg.effective_force_haptic(),
                quiet: cfg.force_quiet_click,
                level: ClickLevel::Force,
            };
            self.force_active = Some(ev.clone());
            out.force = Some(ev);
        }
        Ok(out)
    }

    /// Latch reset (detach path): clears both levels.
    pub fn reset(&mut self) {
        self.pressed.clear();
        self.active = None;
        self.force_pressed.clear();
        self.force_active = None;
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
        assert!(m.update(&[finger(1, 63)], 0, &cfg).unwrap().is_empty());
        assert!(m.active().is_none());
        // At press: fires.
        let ev = m.update(&[finger(1, 64)], 0, &cfg).unwrap().first.unwrap();
        assert_eq!(ev.finger_count, 1);
        // Still pressed at 60 (above release 59): no re-fire.
        assert!(m.update(&[finger(1, 60)], 0, &cfg).unwrap().is_empty());
        assert!(m.active().is_some());
        // At release threshold: drops.
        assert!(m.update(&[finger(1, 59)], 0, &cfg).unwrap().is_empty());
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
        let ev = m.update(&fingers, 0, &cfg).unwrap().first.unwrap();
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
        let ev = m.update(&[finger(1, 200)], 0, &cfg).unwrap().first.unwrap();
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
        let ev = m.update(&[finger(1, 200)], 0, &cfg).unwrap().first.unwrap();
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
    fn second_finger_mid_latch_does_not_refire() {
        let cfg = Config {
            click_1: "left".into(),
            click_2: "right".into(),
            ..Config::default()
        };
        let mut m = ClickMachine::new();
        let ev = m.update(&[finger(1, 200)], 0, &cfg).unwrap().first.unwrap();
        assert_eq!(ev.finger_count, 1);
        // 2nd finger presses while latched: no edge, latch keeps count 1.
        assert!(m
            .update(&[finger(1, 200), finger(2, 200)], 0, &cfg)
            .unwrap()
            .is_empty());
        assert_eq!(m.active().unwrap().finger_count, 1);
    }

    #[test]
    fn margin_zero_releases_at_threshold() {
        let cfg = Config {
            sensitivity: 64,
            margin: 0,
            ..Config::default()
        };
        assert_eq!(release_threshold(&cfg), 64);
        let mut m = ClickMachine::new();
        m.update(&[finger(1, 200)], 0, &cfg).unwrap();
        assert!(m.active().is_some());
        // At exactly the threshold with margin 0 the finger lets go.
        m.update(&[finger(1, 64)], 0, &cfg).unwrap();
        assert!(m.active().is_none());
    }

    #[test]
    fn force_arming_and_disabled() {
        let off = Config {
            force_offset: 20,
            sensitivity: 64,
            ..Config::default()
        };
        assert_eq!(ClickMachine::force_threshold(&off), Some(84));
        assert_eq!(ClickMachine::force_release_threshold(&off), Some(79));
        assert_eq!(ClickMachine::force_threshold(&Config::default()), None);
        assert_eq!(
            ClickMachine::force_release_threshold(&Config::default()),
            None
        );
        // Disabled: deep pressure fires first level only, never force.
        let mut m = ClickMachine::new();
        let out = m.update(&[finger(1, 250)], 0, &Config::default()).unwrap();
        assert!(out.first.is_some());
        assert!(out.force.is_none());
        assert!(m.force_active().is_none());
    }

    #[test]
    fn force_edge_and_finger_map() {
        let cfg = Config {
            sensitivity: 64,
            margin: 5,
            force_offset: 20,
            click_1: "left".into(),
            click_2: "right".into(),
            force_1: "forward".into(),
            force_2: "ctrl+c".into(),
            ..Config::default()
        };
        let mut m = ClickMachine::new();
        // Medium press: first level only.
        let out = m.update(&[finger(1, 70)], 0, &cfg).unwrap();
        assert!(out.first.is_some());
        assert!(out.force.is_none());
        // Deepen with a 2nd finger: force edge captures count 2.
        let out = m
            .update(&[finger(1, 100), finger(2, 100)], 0, &cfg)
            .unwrap();
        assert!(out.first.is_none());
        let fev = out.force.unwrap();
        assert!(fev.is_force());
        assert_eq!(fev.finger_count, 2);
        assert_eq!(
            fev.action.kind,
            ActionKind::Keys(vec!["ctrl".to_string(), "c".to_string()])
        );
        // 3rd finger mid-force-latch does not refire.
        let out = m
            .update(&[finger(1, 100), finger(2, 100), finger(3, 100)], 0, &cfg)
            .unwrap();
        assert!(out.is_empty());
        assert_eq!(m.force_active().unwrap().finger_count, 2);
    }

    #[test]
    fn force_haptic_override_and_fallback() {
        // Fallback: no override -> standard haptic; quiet follows force flag.
        let cfg = Config {
            sensitivity: 64,
            margin: 5,
            force_offset: 20,
            haptic: 5,
            quiet_click: false,
            force_quiet_click: true,
            force_1: "forward".into(),
            ..Config::default()
        };
        let mut m = ClickMachine::new();
        m.update(&[finger(1, 70)], 0, &cfg).unwrap();
        let out = m.update(&[finger(1, 100)], 0, &cfg).unwrap();
        let fev = out.force.unwrap();
        assert_eq!(fev.haptic, 5);
        assert!(fev.quiet);
        // Override wins.
        let cfg = Config {
            force_haptic: Some(9),
            force_quiet_click: false,
            ..cfg
        };
        let mut m = ClickMachine::new();
        m.update(&[finger(1, 70)], 0, &cfg).unwrap();
        let out = m.update(&[finger(1, 100)], 0, &cfg).unwrap();
        let fev = out.force.unwrap();
        assert_eq!(fev.haptic, 9);
        assert!(!fev.quiet);
    }

    #[test]
    fn force_hysteresis_returns_to_first_hold() {
        let cfg = Config {
            sensitivity: 64,
            margin: 5,
            force_offset: 20,
            ..Config::default()
        };
        // Release point: 64 + 20 - 5 = 79.
        assert_eq!(ClickMachine::force_release_threshold(&cfg), Some(79));
        let mut m = ClickMachine::new();
        m.update(&[finger(1, 70)], 0, &cfg).unwrap();
        let out = m.update(&[finger(1, 100)], 0, &cfg).unwrap();
        assert!(out.force.is_some());
        // Hold above release: no refire, both latched.
        assert!(m.update(&[finger(1, 80)], 0, &cfg).unwrap().is_empty());
        assert!(m.force_active().is_some());
        assert!(m.active().is_some());
        // At release point: force drops, first holds (80 -> 79 boundary).
        assert!(m.update(&[finger(1, 79)], 0, &cfg).unwrap().is_empty());
        assert!(m.force_active().is_none());
        assert!(m.active().is_some());
        // Below first release (59): all drop.
        assert!(m.update(&[finger(1, 59)], 0, &cfg).unwrap().is_empty());
        assert!(m.active().is_none());
        assert!(m.force_active().is_none());
    }

    #[test]
    fn force_coexistence_ordering_same_frame() {
        let cfg = Config {
            sensitivity: 64,
            margin: 5,
            force_offset: 20,
            force_1: "forward".into(),
            ..Config::default()
        };
        let mut m = ClickMachine::new();
        // One-frame jump latches both in press order; force supersedes.
        let out = m.update(&[finger(1, 250)], 0, &cfg).unwrap();
        assert!(out.first.is_some());
        assert!(out.force.is_some());
        let primary = out.primary().unwrap();
        assert!(primary.is_force());
        assert!(m.active().is_some());
        assert!(m.force_active().is_some());
    }

    #[test]
    fn force_no_overflow_at_max_sensitivity() {
        let cfg = Config {
            sensitivity: 255,
            margin: 5,
            force_offset: 255,
            force_1: "forward".into(),
            ..Config::default()
        };
        // 255 + 255 = 510 in u16: representable, but unreachable by a u8.
        assert_eq!(ClickMachine::force_threshold(&cfg), Some(510));
        assert_eq!(ClickMachine::force_release_threshold(&cfg), Some(505));
        let mut m = ClickMachine::new();
        let out = m.update(&[finger(1, 255)], 0, &cfg).unwrap();
        assert!(out.first.is_some());
        assert!(out.force.is_none());
        assert!(m.force_active().is_none());
    }

    #[test]
    fn primary_skips_force_none() {
        let cfg = Config {
            sensitivity: 64,
            margin: 5,
            force_offset: 20,
            click_1: "left".into(),
            force_1: "none".into(),
            ..Config::default()
        };
        let mut m = ClickMachine::new();
        let out = m.update(&[finger(1, 250)], 0, &cfg).unwrap();
        assert!(out.first.is_some());
        let force = out.force.as_ref().unwrap();
        assert!(force.is_force() && force.is_none());
        // Force still latches (swallows), but reporters see the first edge.
        assert!(m.force_active().is_some());
        let primary = out.primary().unwrap();
        assert!(!primary.is_force());
    }

    #[test]
    fn force_disarm_mid_hold_clears_latch() {
        let armed = Config {
            sensitivity: 64,
            margin: 5,
            force_offset: 20,
            force_1: "forward".into(),
            ..Config::default()
        };
        let mut m = ClickMachine::new();
        m.update(&[finger(1, 70)], 0, &armed).unwrap();
        assert!(m
            .update(&[finger(1, 100)], 0, &armed)
            .unwrap()
            .force
            .is_some());
        // Offset back to 0 mid-hold: force drops, first holds, no re-attach.
        let off = Config {
            force_offset: 0,
            ..armed
        };
        let out = m.update(&[finger(1, 100)], 0, &off).unwrap();
        assert!(out.is_empty());
        assert!(m.force_active().is_none());
        assert!(!m.finger_force_pressed(1));
        assert!(m.active().is_some());
    }

    #[test]
    fn six_finger_clamp_both_levels() {
        let cfg = Config {
            sensitivity: 64,
            margin: 5,
            force_offset: 20,
            ..Config::default()
        };
        let mut m = ClickMachine::new();
        let fingers: Vec<FingerContact> = (1..=6).map(|id| finger(id, 250)).collect();
        let out = m.update(&fingers, 0, &cfg).unwrap();
        assert_eq!(out.first.as_ref().unwrap().finger_count, 5);
        assert_eq!(out.force.as_ref().unwrap().finger_count, 5);
    }

    #[test]
    fn force_margin_zero_mirrors_first() {
        let cfg = Config {
            sensitivity: 64,
            margin: 0,
            force_offset: 20,
            force_1: "forward".into(),
            ..Config::default()
        };
        // margin 0: force release == force press (84).
        assert_eq!(ClickMachine::force_release_threshold(&cfg), Some(84));
        let mut m = ClickMachine::new();
        m.update(&[finger(1, 70)], 0, &cfg).unwrap();
        assert!(m
            .update(&[finger(1, 100)], 0, &cfg)
            .unwrap()
            .force
            .is_some());
        // Exactly at the threshold the force finger lets go; first holds.
        let out = m.update(&[finger(1, 84)], 0, &cfg).unwrap();
        assert!(out.is_empty());
        assert!(m.force_active().is_none());
        assert!(m.active().is_some());
    }

    #[test]
    fn force_threshold_clamp_symmetry() {
        // Unvalidated extremes clamp exactly like press/release_threshold.
        let cfg = Config {
            sensitivity: 0,
            margin: 300,
            force_offset: 300,
            ..Config::default()
        };
        assert_eq!(press_threshold(&cfg), 1);
        assert_eq!(release_threshold(&cfg), 0);
        assert_eq!(ClickMachine::force_threshold(&cfg), Some(1 + 255));
        assert_eq!(ClickMachine::force_release_threshold(&cfg), Some(1));
    }

    #[test]
    fn partial_finger_lift_releases_force_holds_first() {
        let cfg = Config {
            sensitivity: 64,
            margin: 5,
            force_offset: 20,
            force_1: "forward".into(),
            force_2: "forward".into(),
            ..Config::default()
        };
        let mut m = ClickMachine::new();
        // A deep + B medium: both levels latch with count 2.
        let out = m.update(&[finger(1, 100), finger(2, 70)], 0, &cfg).unwrap();
        assert!(out.first.is_some() && out.force.is_some());
        // A lifts, B stays medium: force drops (no deep finger left), first
        // holds on B.
        let out = m.update(&[lifted(1), finger(2, 70)], 0, &cfg).unwrap();
        assert!(out.is_empty());
        assert!(m.force_active().is_none());
        assert!(m.active().is_some());
    }

    #[test]
    fn parse_error_leaves_state_untouched() {
        let bad = Config {
            click_1: "bogus-key".into(),
            ..Config::default()
        };
        let mut m = ClickMachine::new();
        assert!(m.update(&[finger(1, 200)], 0, &bad).is_err());
        assert!(m.active().is_none());
        assert!(!m.finger_pressed(1));
        // Retry with a fixed config fires on the same pressure.
        let out = m.update(&[finger(1, 200)], 0, &Config::default()).unwrap();
        assert!(out.first.is_some());
    }

    #[test]
    fn force_parse_error_does_not_swallow_first_edge() {
        let bad = Config {
            sensitivity: 64,
            margin: 5,
            force_offset: 20,
            force_1: "bogus-key".into(),
            ..Config::default()
        };
        let mut m = ClickMachine::new();
        assert!(m.update(&[finger(1, 250)], 0, &bad).is_err());
        // Neither level latched: the good first edge is retried, not lost.
        assert!(m.active().is_none());
        assert!(m.force_active().is_none());
        let good = Config {
            force_1: "forward".into(),
            ..bad
        };
        let out = m.update(&[finger(1, 250)], 0, &good).unwrap();
        assert!(out.first.is_some() && out.force.is_some());
    }
}
