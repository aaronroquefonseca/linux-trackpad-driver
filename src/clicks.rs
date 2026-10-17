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
//! - Touch presence is the device flag OR pressure past
//!   [`TOUCH_PRESSURE_FLOOR`] (10, old-service parity): light touches
//!   drive cursor slots without latching clicks.
//! - Old-service debounce on the timed path ([`ClickMachine::update_at`]):
//!   press swallowed within [`PRESS_DEBOUNCE_MS`] (50 ms) of the last
//!   release, release held within [`RELEASE_DEBOUNCE_MS`] (30 ms) of the
//!   press edge. The force level is undebounced (old parity).
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
//!   both be held. Edges fire in press order (first then force), staged
//!   across frames old-exact: a deep jump from idle latches first this
//!   frame, force follows on the next frame while held deep; reporters
//!   surface the force edge (it supersedes while held past threshold).
//!   Pressure falling below the force release returns to the first-level hold; falling below the first-level
//!   release (`sensitivity - margin`) releases all.
//! - Force haptic: [`crate::config::Config::effective_force_haptic`] (explicit
//!   `force_haptic` override, else standard `haptic`; `Some(0)` is silent);
//!   waveform from `force_quiet_click`. Each edge fires its own pulse on
//!   its own frame, so a deep stab feels like staged arrival (first snap,
//!   then the force snap a report later).
//! - Reporters surface [`FrameOutcome::primary`]: force while held past
//!   threshold, except a force `none` never supersedes (it emits nothing).
//! - `click_N`/`force_N` parse before any latch bit mutates: a bad action
//!   leaves state untouched for retry instead of half-latching.
//! - Haptic: `haptic == 0` = silent ([`should_emit_haptic`]); otherwise the
//!   proxy sends the host-click report (`0x21 0x01`, see [`crate::init`])
//!   with strength/quiet carried for logging. Strength byte mapping is
//!   unverified on hardware (proprietary `0x53` actuator per RESEARCH §2).
//! - uinput mapping: every down contact emits the old-service normalized
//!   values (pressure 60, major/minor 800 — raw bytes are arbitrary units
//!   the classifier drops); orientation emits `-(raw)` per the kernel
//!   `hid-magicmouse` negation; physical `clicks` byte contributes
//!   `BTN = clicks & 1` (bits 6..7 are timestamp, not buttons) OR-ed into
//!   the click state.
//! - Slide third status (`slide_1..=slide_5`, default `none` = unmapped):
//!   fires/holds while N fingers touch with every pressure at/below the
//!   release threshold. Releases on liftoff, on count change (re-selects
//!   when the new count maps an action, else releases silently), or on a
//!   press-edge handoff. Handoff rule: the slide latch drops when a
//!   first/force edge fires; the proxy releases slide keys first UNLESS
//!   the slide and press actions carry identical key sets — then the hold
//!   continues with no release/re-press. Slide never pulses: touching is
//!   silent, so `slide_* = empty` is keys-nothing + haptic-nothing
//!   (valid-but-silent — prefer `none`; old configs keep working).
//!   `none` never latches, never pulses (default = current behavior).
//! - Finger-count spoofing (`[spoof.slide]` / `[spoof.click]` /
//!   `[spoof.force]` sparse tables, omitted = passthrough default): the
//!   uinput touch stream (slots, `BTN_TOOL_*`) reports the spoofed count
//!   via [`spoof_contacts`] (real centroid, requested contacts on a tiny
//!   fixed pattern: pair ±18 x, 3+ on a 24-unit ring from straight up).
//!   Action selection keeps the real count (default, recommended) unless
//!   `[spoof] actions_follow_spoof = true` (then selection uses the
//!   spoofed count too, and the edge's `finger_count` reports it).
//! - Level-transition mode (`transition_mode`, global): `"cut"`
//!   (default) releases ALL level-1/slide keys first (with
//!   SYN) then presses the force combo fresh — shared codes show
//!   up+SYN+down; `"continuous"` holds level-1 keys while force keys join
//!   (shared codes down once, up once). The machine latches identically either way; the mode
//!   only changes the proxy's wire sequence on the force edge.
//! - Combination semantics: slide hands off to press (same-action =
//!   seamless, else release-then-press, one SYN); spoof reshapes the wire
//!   under whichever status latch holds (force > first > slide > idle
//!   slide-map); cut releases slide + level-1 wire keys before the force
//!   press. All three compose: e.g. a spoofed 2→3 slide can hand off to a
//!   real-count click that a force edge then cuts into a fresh combo.

use std::time::Instant;

use anyhow::Result;

use crate::apple_parse::FingerContact;
use crate::config::{ActionKind, ClickAction, Config};

/// Touch-presence pressure floor (old-service parity): a contact counts as
/// down when the device flag says so OR its pressure exceeds 10. Below the
/// click threshold this still drives cursor slots; at/above `sensitivity`
/// it also latches clicks (old `max_pressure` semantics, per-finger here).
pub const TOUCH_PRESSURE_FLOOR: u8 = 10;

/// Press debounce: a fresh press within 50 ms of the last release is
/// swallowed (old-service `last_release_time + 50ms` gate).
pub const PRESS_DEBOUNCE_MS: u64 = 50;
/// Release debounce: a release within 30 ms of the press edge is held
/// (old-service `last_press_time + 30ms` gate).
pub const RELEASE_DEBOUNCE_MS: u64 = 30;

/// Whether a decoded contact counts as touching (device flag or pressure).
pub fn touch_present(f: &FingerContact) -> bool {
    f.contact || f.pressure > TOUCH_PRESSURE_FLOOR
}

/// Scale a pointer delta by `speed` percent (`100` = 1:1). Proxy path only.
pub fn scale_delta(delta: i32, speed: u32) -> i32 {
    ((i64::from(delta) * i64::from(speed)) / 100) as i32
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
    cfg.click.sensitivity.clamp(1, 255) as u8
}

/// Release threshold = `sensitivity - margin`, saturating at 0.
pub fn release_threshold(cfg: &Config) -> u8 {
    let s = cfg.click.sensitivity.clamp(1, 255);
    let m = cfg.click.margin.min(255);
    s.saturating_sub(m) as u8
}

/// Spoof pattern geometry (device units; density is 48/44 units/mm, so
/// both are sub-millimeter): a requested pair splits ±`SPOOF_PAIR_DX` on
/// x; 3+ contacts sit evenly on a `SPOOF_RING_RADIUS` circle starting at
/// straight up (-90°), exactly like drafting positions around the real
/// centroid. Fixed (not configurable): keeps the wire shape predictable
/// for tests and downstream classifiers.
pub const SPOOF_PAIR_DX: i32 = 18;
pub const SPOOF_RING_RADIUS: i32 = 24;
/// Axis clamps for synthesized contacts (pointer-node ranges, see
/// [`crate::proxy::uinput_abs_setup`]): the centroid ± pattern stays
/// inside the ranges except for pathological edge touches.
pub const SPOOF_X_MIN: i32 = -3678;
pub const SPOOF_X_MAX: i32 = 3934;
pub const SPOOF_Y_MIN: i32 = -2478;
pub const SPOOF_Y_MAX: i32 = 2587;

/// Build the virtual contact list for a spoofed count: keeps the real
/// centroid (mean of touching x/y), projects the requested number of
/// contacts on the fixed small pattern around it ([`SPOOF_PAIR_DX`] /
/// [`SPOOF_RING_RADIUS`]), copying size/orientation/pressure from the
/// first touching contact with fresh ids `1..=N` and `contact = true`.
///
/// Identity fast path: when the requested count equals the real touching
/// count the input slice is returned unchanged (ids and positions
/// preserved — no synthetic jitter). Empty input (or `virtual_n == 0`)
/// yields no contacts.
pub fn spoof_contacts(real: &[FingerContact], virtual_n: u8) -> Vec<FingerContact> {
    let touching: Vec<&FingerContact> = real.iter().filter(|f| touch_present(f)).collect();
    let want = virtual_n.clamp(1, 5) as usize;
    if touching.is_empty() || virtual_n == 0 {
        return Vec::new();
    }
    if touching.len() == want {
        return real.to_vec();
    }
    let cx = touching.iter().map(|f| i64::from(f.x)).sum::<i64>() / touching.len() as i64;
    let cy = touching.iter().map(|f| i64::from(f.y)).sum::<i64>() / touching.len() as i64;
    let template = *touching[0];
    let mut pts = Vec::with_capacity(want);
    if want == 1 {
        pts.push((cx, cy));
    } else if want == 2 {
        pts.push((cx - i64::from(SPOOF_PAIR_DX), cy));
        pts.push((cx + i64::from(SPOOF_PAIR_DX), cy));
    } else {
        for k in 0..want {
            let ang =
                -std::f64::consts::FRAC_PI_2 + 2.0 * std::f64::consts::PI * k as f64 / want as f64;
            let dx = (f64::from(SPOOF_RING_RADIUS) * ang.cos()).round() as i64;
            let dy = (f64::from(SPOOF_RING_RADIUS) * ang.sin()).round() as i64;
            pts.push((cx + dx, cy + dy));
        }
    }
    pts.into_iter()
        .enumerate()
        .map(|(i, (x, y))| FingerContact {
            id: (i + 1) as u8,
            x: x.clamp(i64::from(SPOOF_X_MIN), i64::from(SPOOF_X_MAX)) as i32,
            y: y.clamp(i64::from(SPOOF_Y_MIN), i64::from(SPOOF_Y_MAX)) as i32,
            contact: true,
            ..template
        })
        .collect()
}

/// Which pressure level fired a [`ClickEvent`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ClickLevel {
    /// Standard level (`click_N`, `haptic`, `quiet_click`).
    #[default]
    First,
    /// Second level (`force_N`, `effective_force_haptic()`, `force_quiet_click`).
    Force,
    /// Touch-but-not-pressed (`slide_N`): never pulses (touch is silent —
    /// only click/force buzz). Keys still flow.
    Slide,
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
    /// True for the touch-but-not-pressed (slide) level.
    pub fn is_slide(&self) -> bool {
        matches!(self.level, ClickLevel::Slide)
    }
}

/// Per-frame press edges, in press order (first then force, staged across
/// frames old-exact: a frame carries at most one fresh edge from idle).
///
/// Reporters surface `force` (it supersedes while held past threshold)
/// while holding both key sets. `slide` fires only when neither press level edges
/// (see [`ClickMachine`] handoff): reporters surface it when no press edge
/// fired.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FrameOutcome {
    /// Fresh first-level press edge, if any.
    pub first: Option<ClickEvent>,
    /// Fresh force-level press edge, if any.
    pub force: Option<ClickEvent>,
    /// Fresh slide-level edge, if any (touch-but-not-pressed onset or
    /// finger-count re-select while sliding).
    pub slide: Option<ClickEvent>,
}

impl FrameOutcome {
    /// Superseding edge for reporters: force while held past threshold,
    /// except a force `none` never supersedes (it emits nothing); else the
    /// first-level edge; else the slide edge.
    pub fn primary(&self) -> Option<ClickEvent> {
        match &self.force {
            Some(f) if !f.is_none() => self.force.clone(),
            _ => match &self.first {
                Some(_) => self.first.clone(),
                None => self.slide.clone(),
            },
        }
    }
    /// True when no level fired this frame.
    pub fn is_empty(&self) -> bool {
        self.first.is_none() && self.force.is_none() && self.slide.is_none()
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
///
/// Slide level: while idle (no first/force latch) with N fingers touching
/// at/below the release threshold, `slide_N` latches instead of clicks.
/// Count changes re-select (old releases, new fires); liftoff releases; a
/// press edge hands off (the proxy releases slide keys first unless the
/// slide and press actions are identical — then the hold continues with no
/// wire flutter). A slide `none` (the default) never latches and never
/// emits: zero behavior change unless the user opts in. Entry requires all
/// touching pressures at/below release; once held it sticks (no flap on
/// noisy slides) until lift/press/count-change. Slide edges are
/// undebounced; a debounce-swallowed press neither fires nor hands off
/// (the slide holds until the retried press lands).
#[derive(Debug, Default)]
pub struct ClickMachine {
    pressed: std::collections::HashMap<u8, bool>,
    active: Option<ClickEvent>,
    force_pressed: std::collections::HashMap<u8, bool>,
    force_active: Option<ClickEvent>,
    slide_active: Option<ClickEvent>,
    /// Real touching count (clamped 1..=5) the slide latch selected on.
    /// Compared against the real count (not the spoof-aware select count)
    /// so an `[spoof] actions_follow_spoof` remap never re-fires every frame.
    slide_real: Option<u8>,
    last_press_at: Option<Instant>,
    last_release_at: Option<Instant>,
    /// Last frame clock that still held any force bit. A bit-less frame
    /// inside [`RELEASE_DEBOUNCE_MS`] of this stamp keeps the force latch
    /// (hold hysteresis: pressure noise must not end an intentional hold).
    last_force_hold_at: Option<Instant>,
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

    /// Currently latched slide-level action, if any.
    pub fn slide_active(&self) -> Option<&ClickEvent> {
        self.slide_active.as_ref()
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
        if cfg.force.force_offset > 0 {
            Some(
                cfg.click
                    .sensitivity
                    .clamp(1, 255)
                    .saturating_add(cfg.force.force_offset.min(255)),
            )
        } else {
            None
        }
    }

    /// Force release point: `sensitivity + force_offset - margin`, saturating
    /// at 0 (mirrors [`release_threshold`]). `None` when disabled.
    pub fn force_release_threshold(cfg: &Config) -> Option<u16> {
        Self::force_threshold(cfg).map(|press| press.saturating_sub(cfg.click.margin.min(255)))
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
    /// Timeless core: no debounce is applied and no timestamps are
    /// recorded. Unit tests use this; the production proxy path uses
    /// [`ClickMachine::update_at`] so the old-service 50 ms press / 30 ms
    /// release debounce applies on hardware.
    ///
    /// Force fires only when `force_offset > 0` and some finger's pressure
    /// (`u16`-compared, no overflow) reaches `sensitivity + force_offset`;
    /// finger-count is captured at the force edge like the first level.
    /// Old-exact staging: a deep jump from idle latches the first level
    /// this frame and force follows on the next frame while held deep
    /// (the old service makes one transition per report: Released admits
    /// Pressed only). An already-latched first level takes the force edge
    /// immediately.
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
        self.update_inner(fingers, cfg, None)
    }

    /// Timed feed: [`ClickMachine::update`] plus the old-service press /
    /// release debounce. A press within [`PRESS_DEBOUNCE_MS`] of the last
    /// release is swallowed (bits reverted so the next frame retries); a
    /// release within [`RELEASE_DEBOUNCE_MS`] of the press edge holds the
    /// latch (first level only: a force-held frame releases immediately,
    /// old parity). The force level is undebounced (old parity: time gates only
    /// guard the Released↔Pressed transitions), except a suppressed
    /// first-level press also suppresses the same-frame force edge (old
    /// `Released` admits no `ForcePressed` directly). Likewise a
    /// same-frame force edge behind a fresh first-level edge defers one
    /// frame (old-exact staging, no time gate involved).
    pub fn update_at(
        &mut self,
        fingers: &[FingerContact],
        _clicks: u8,
        cfg: &Config,
        now: Instant,
    ) -> Result<FrameOutcome> {
        self.update_inner(fingers, cfg, Some(now))
    }

    fn update_inner(
        &mut self,
        fingers: &[FingerContact],
        cfg: &Config,
        now: Option<Instant>,
    ) -> Result<FrameOutcome> {
        let press = press_threshold(cfg);
        let release = release_threshold(cfg);
        let force_press = Self::force_threshold(cfg);
        let force_release = Self::force_release_threshold(cfg);
        let armed = force_press.is_some();
        // Entry force latch: the old service debounces the release hold
        // only out of Pressed. Out of ForcePressed a full release drops
        // immediately, so a force-held frame never takes the 30 ms hold.
        let force_latched_at_entry = self.force_active.is_some();

        // Phase 1 (read-only): decide per-finger bit writes. Nothing mutates
        // here so a parse failure below leaves state untouched.
        // Presence is the device flag OR pressure past the touch floor
        // (old `is_down || pressure > 10`): light touches drive slots
        // without latching clicks.
        let mut first_write = Vec::new();
        let mut newly_pressed = Vec::new();
        for f in fingers {
            if !touch_present(f) {
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
                if !touch_present(f) {
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

        // Phase 2: parse before mutating. `n` is the real touching count
        // of the edge frame, clamped 1..=5; each level selects on its
        // spoof-aware count (`[spoof] actions_follow_spoof` swaps
        // real→virtual, default real — zero behavior change unless opted in).
        let contacting = fingers.iter().filter(|f| touch_present(f)).count();
        let n = (contacting.clamp(1, 5)) as u8;
        let first_select = cfg.select_count("click", n);
        let force_select = cfg.select_count("force", n);
        let slide_select = cfg.select_count("slide", n);
        let first_action = if self.active.is_none() && !newly_pressed.is_empty() {
            Some(cfg.click_action(first_select)?)
        } else {
            None
        };
        let force_action = if armed && self.force_active.is_none() && !newly_force.is_empty() {
            Some(cfg.force_action(force_select)?)
        } else {
            None
        };
        // Slide entry/re-select: idle, no press edge this frame, someone
        // touching, and not already holding this count. Initial entry
        // needs every touching pressure at/below release (the hysteresis
        // band is a dead zone: no slide, no press); a held slide sticks
        // without re-parsing so noisy slides never flap. Re-select (count
        // change while a slide holds) skips the entry predicate: a new
        // finger landing in the band re-selects instead of keeping a stale
        // action (a press edge still hands off — band only, never a press).
        let idle_no_edge = self.active.is_none()
            && self.force_active.is_none()
            && first_action.is_none()
            && force_action.is_none()
            && contacting > 0
            && self.slide_real != Some(n);
        let slide_entry = self.slide_active.is_none()
            && fingers
                .iter()
                .filter(|f| touch_present(f))
                .all(|f| f.pressure <= release);
        let slide_wanted =
            cfg.any_slide_mapped() && idle_no_edge && (self.slide_active.is_some() || slide_entry);
        let slide_action = if slide_wanted {
            Some(cfg.slide_action(slide_select)?)
        } else {
            None
        };

        // Debounce gates (timed path only; `None` = timeless unit-test
        // core, never gated). Press: within 50 ms of the last release the
        // edge is swallowed and the fresh bits reverted so the next frame
        // retries instead of half-latching. Release: within 30 ms of the
        // press edge the latch holds.
        let press_suppressed = first_action.is_some()
            && now.is_some_and(|t| {
                self.last_release_at.is_some_and(|r| {
                    t.saturating_duration_since(r).as_millis() < u128::from(PRESS_DEBOUNCE_MS)
                })
            });

        // Phase 3: commit bit writes.
        for (id, bit) in first_write {
            self.pressed.insert(id, bit);
        }
        for (id, bit) in force_write {
            self.force_pressed.insert(id, bit);
        }
        if press_suppressed {
            // Revert the fresh bits: the swallowed press retries next frame
            // (a committed `true` bit would never re-edge). The same-frame
            // force edge is swallowed too (old `Released` admits no direct
            // `ForcePressed`).
            for id in newly_pressed.iter().chain(newly_force.iter()) {
                self.pressed.insert(*id, false);
                self.force_pressed.insert(*id, false);
            }
        }
        // Old-exact staging: the old service makes one transition per
        // report, so Released admits Pressed only — a deep stab lands the
        // first level this frame and force follows on the next report.
        // When this frame fires the first-level edge from idle, defer a
        // same-frame force edge: revert its fresh bits so the next frame
        // re-edges (identical to staged arrival). An already-latched
        // first level still takes the force edge immediately.
        let first_fired = first_action.is_some() && !press_suppressed;
        let force_deferred = first_fired && force_action.is_some();
        if force_deferred {
            for id in newly_force.iter() {
                self.force_pressed.insert(*id, false);
            }
        }
        // Evict ids absent from this frame (a lifted finger reports no
        // block): without this a lifted finger's bit stays set and holds
        // the latch while a remaining finger moves lightly (stale hold →
        // stuck button across motion). Old-service parity: release follows
        // the current frame's max pressure, so an absent finger can never
        // hold it. An empty frame evicts every bit (full liftoff).
        {
            let present: std::collections::HashSet<u8> = fingers.iter().map(|f| f.id).collect();
            for (id, bit) in self.pressed.iter_mut() {
                if !present.contains(id) {
                    *bit = false;
                }
            }
            for (id, bit) in self.force_pressed.iter_mut() {
                if !present.contains(id) {
                    *bit = false;
                }
            }
        }
        if !armed {
            // Disarmed mid-hold: drop the force latch + bits outright.
            self.force_active = None;
            for v in self.force_pressed.values_mut() {
                *v = false;
            }
        }
        // Hold anchor AFTER every bit mutation above (commits, suppress and
        // staging reverts, eviction, disarm): only a frame that truly still
        // holds force refreshes the hysteresis window.
        if self.force_pressed.values().any(|&p| p) {
            if let Some(t) = now {
                self.last_force_hold_at = Some(t);
            }
        }

        // Release latches with no remaining bits (force first so a full
        // release clears both; first-level release implies force is gone too
        // since force release >= first release). The first level holds
        // while inside the release debounce window, unless force was
        // latched at entry (old releases ForcePressed immediately, no
        // 30 ms hold). The force level holds inside the same window anchored
        // at its last held frame instead (DEVIATION: old drops it
        // undebounced — a single noisy frame then ends an intentional hold
        // and its emitted keys, e.g. super in a drag chord, flap; clean
        // liftoff still releases outright).
        let force_hold_debounced = self.force_active.is_some()
            && !self.force_pressed.values().any(|&p| p)
            && now.is_some_and(|t| {
                self.last_force_hold_at.is_some_and(|h| {
                    t.saturating_duration_since(h).as_millis() < u128::from(RELEASE_DEBOUNCE_MS)
                })
            });
        if self.force_active.is_some()
            && !self.force_pressed.values().any(|&p| p)
            && !force_hold_debounced
        {
            self.force_active = None;
        }
        let release_debounced = !force_latched_at_entry
            && self.active.is_some()
            && now.is_some_and(|t| {
                self.last_press_at.is_some_and(|p| {
                    t.saturating_duration_since(p).as_millis() < u128::from(RELEASE_DEBOUNCE_MS)
                })
            });
        if self.active.is_some() && !self.pressed.values().any(|&p| p) && !release_debounced {
            self.active = None;
            self.force_active = None;
            for v in self.force_pressed.values_mut() {
                *v = false;
            }
            if let Some(t) = now {
                self.last_release_at = Some(t);
            }
        }

        // Slide releases: liftoff clears outright; a press edge that
        // actually fires hands off (the proxy releases slide keys first
        // unless the slide and press actions are identical — then the hold
        // continues with no wire flutter). A debounce-swallowed press
        // hands off nothing: the slide holds until the retried press lands.
        let press_fired = (first_action.is_some() || force_action.is_some()) && !press_suppressed;
        if contacting == 0 || press_fired {
            self.slide_active = None;
            self.slide_real = None;
        }

        let mut out = FrameOutcome::default();

        // Press edge: first newly-pressed finger while idle latches.
        if let Some(action) = first_action {
            if press_suppressed {
                // Swallowed: no latch, no edge; the reverted bits retry.
            } else {
                let ev = ClickEvent {
                    finger_count: first_select,
                    action,
                    haptic: cfg.haptics.haptic,
                    quiet: cfg.haptics.quiet_click,
                    level: ClickLevel::First,
                };
                // `none` still latches (swallows the gesture); caller emits
                // nothing and no haptic for it.
                self.active = Some(ev.clone());
                out.first = Some(ev);
                if let Some(t) = now {
                    self.last_press_at = Some(t);
                }
            }
        }
        // Force edge: same latch shape, disabled entirely when offset is 0.
        if let Some(action) = force_action {
            if press_suppressed || force_deferred {
                // Swallowed with the first-level press (see above), or
                // deferred a frame for old-exact staging (bits reverted
                // above, so the next frame re-edges while held deep).
            } else {
                let ev = ClickEvent {
                    finger_count: force_select,
                    action,
                    haptic: cfg.effective_force_haptic(),
                    quiet: cfg.haptics.force_quiet_click,
                    level: ClickLevel::Force,
                };
                self.force_active = Some(ev.clone());
                out.force = Some(ev);
            }
        }
        // Slide edge: unlike `none` clicks (which latch to swallow), a
        // slide `none` — the default — latches nothing and emits nothing
        // (zero behavior change unless opted in). A re-select onto an
        // unmapped count remembers the count silently (`slide_real` set,
        // latch empty) so later frames hold without re-parsing; the proxy
        // sees the `slide_active` transition and releases keys.
        if let Some(action) = slide_action {
            if matches!(action.kind, ActionKind::None) {
                self.slide_active = None;
                self.slide_real = Some(n);
            } else {
                let ev = ClickEvent {
                    finger_count: slide_select,
                    action,
                    haptic: cfg.haptics.haptic,
                    quiet: cfg.haptics.quiet_click,
                    level: ClickLevel::Slide,
                };
                self.slide_active = Some(ev.clone());
                self.slide_real = Some(n);
                out.slide = Some(ev);
            }
        }
        Ok(out)
    }

    /// End a staged press cancelled by the proxy on genuine liftoff.
    /// Its wire hold is gone, so the release-hold debounce must not retain
    /// a latch that would swallow the next contact's edge. Keep the normal
    /// press debounce measured from this release (or the prior release if
    /// update already ended the press); detach's reset alone would lose it.
    pub(crate) fn cancel_pending_press_at(&mut self, now: Instant) {
        let released_at = if self.active.is_some() {
            Some(now)
        } else {
            self.last_release_at
        };
        self.reset();
        self.last_release_at = released_at;
    }

    /// Latch reset (detach path): clears both levels and debounce clocks
    /// so a re-attach starts undebounced.
    pub fn reset(&mut self) {
        self.pressed.clear();
        self.active = None;
        self.force_pressed.clear();
        self.force_active = None;
        self.slide_active = None;
        self.slide_real = None;
        self.last_press_at = None;
        self.last_release_at = None;
        self.last_force_hold_at = None;
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
            click: crate::config::ClickConfig {
                sensitivity: 64,
                margin: 5,
                ..Config::default().click
            },
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
            click: crate::config::ClickConfig {
                sensitivity: 3,
                margin: 10,
                ..Config::default().click
            },
            ..Config::default()
        };
        assert_eq!(release_threshold(&cfg), 0);
    }

    #[test]
    fn finger_count_selects_action() {
        let cfg = Config {
            click: crate::config::ClickConfig {
                click_1: "left".into(),
                click_2: "right".into(),
                click_3: "middle".into(),
                ..Config::default().click
            },
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
            click: crate::config::ClickConfig {
                click_1: "none".into(),
                ..Config::default().click
            },
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
            click: crate::config::ClickConfig {
                click_1: "empty".into(),
                ..Config::default().click
            },
            haptics: crate::config::HapticsConfig {
                haptic: 7,
                ..Config::default().haptics
            },
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
    fn negation_mapping() {
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
            click: crate::config::ClickConfig {
                click_1: "left".into(),
                click_2: "right".into(),
                ..Config::default().click
            },
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
            click: crate::config::ClickConfig {
                sensitivity: 64,
                margin: 0,
                ..Config::default().click
            },
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
            click: crate::config::ClickConfig {
                sensitivity: 64,
                ..Config::default().click
            },
            force: crate::config::ForceConfig {
                force_offset: 20,
                ..Config::default().force
            },
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
            click: crate::config::ClickConfig {
                sensitivity: 64,
                margin: 5,
                click_1: "left".into(),
                click_2: "right".into(),
                ..Config::default().click
            },
            force: crate::config::ForceConfig {
                force_offset: 20,
                force_1: "forward".into(),
                force_2: "ctrl+c".into(),
                ..Config::default().force
            },
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
            click: crate::config::ClickConfig {
                sensitivity: 64,
                margin: 5,
                ..Config::default().click
            },
            haptics: crate::config::HapticsConfig {
                haptic: 5,
                quiet_click: false,
                force_quiet_click: true,
                ..Config::default().haptics
            },
            force: crate::config::ForceConfig {
                force_offset: 20,
                force_1: "forward".into(),
                ..Config::default().force
            },
            ..Config::default()
        };
        let mut m = ClickMachine::new();
        m.update(&[finger(1, 70)], 0, &cfg).unwrap();
        let out = m.update(&[finger(1, 100)], 0, &cfg).unwrap();
        let fev = out.force.unwrap();
        assert_eq!(fev.haptic, 5);
        assert!(fev.quiet);
        // Override wins.
        let mut cfg = cfg;
        cfg.haptics.force_haptic = Some(9);
        cfg.haptics.force_quiet_click = false;
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
            click: crate::config::ClickConfig {
                sensitivity: 64,
                margin: 5,
                ..Config::default().click
            },
            force: crate::config::ForceConfig {
                force_offset: 20,
                ..Config::default().force
            },
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
    fn force_stages_behind_first_edge() {
        // Old-exact: a deep jump from idle latches first this frame; force
        // follows on the next frame while held deep (Released admits
        // Pressed only). Reporters see first, then force supersedes.
        let cfg = Config {
            click: crate::config::ClickConfig {
                sensitivity: 64,
                margin: 5,
                ..Config::default().click
            },
            force: crate::config::ForceConfig {
                force_offset: 20,
                force_1: "forward".into(),
                ..Config::default().force
            },
            ..Config::default()
        };
        let mut m = ClickMachine::new();
        let out = m.update(&[finger(1, 250)], 0, &cfg).unwrap();
        assert!(out.first.is_some());
        assert!(out.force.is_none());
        assert!(!out.primary().unwrap().is_force());
        assert!(m.active().is_some());
        assert!(m.force_active().is_none());
        let out = m.update(&[finger(1, 250)], 0, &cfg).unwrap();
        assert!(out.first.is_none());
        assert!(out.force.is_some());
        let primary = out.primary().unwrap();
        assert!(primary.is_force());
        assert!(m.active().is_some());
        assert!(m.force_active().is_some());
    }

    #[test]
    fn force_no_overflow_at_max_sensitivity() {
        let cfg = Config {
            click: crate::config::ClickConfig {
                sensitivity: 255,
                margin: 5,
                ..Config::default().click
            },
            force: crate::config::ForceConfig {
                force_offset: 255,
                force_1: "forward".into(),
                ..Config::default().force
            },
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
            click: crate::config::ClickConfig {
                sensitivity: 64,
                margin: 5,
                click_1: "left".into(),
                ..Config::default().click
            },
            force: crate::config::ForceConfig {
                force_offset: 20,
                force_1: "none".into(),
                ..Config::default().force
            },
            ..Config::default()
        };
        let mut m = ClickMachine::new();
        // Staged: first edge frame one, force `none` frame two.
        let out = m.update(&[finger(1, 250)], 0, &cfg).unwrap();
        assert!(out.first.is_some());
        assert!(out.force.is_none());
        let out = m.update(&[finger(1, 250)], 0, &cfg).unwrap();
        assert!(out.first.is_none());
        let force = out.force.as_ref().unwrap();
        assert!(force.is_force() && force.is_none());
        // Force still latches (swallows), but reporters see nothing new:
        // the force `none` emits no keys/haptic and the first edge already
        // reported on its own frame.
        assert!(m.force_active().is_some());
        assert!(out.primary().is_none());
    }

    #[test]
    fn force_disarm_mid_hold_clears_latch() {
        let armed = Config {
            click: crate::config::ClickConfig {
                sensitivity: 64,
                margin: 5,
                ..Config::default().click
            },
            force: crate::config::ForceConfig {
                force_offset: 20,
                force_1: "forward".into(),
                ..Config::default().force
            },
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
        let mut off = armed;
        off.force.force_offset = 0;
        let out = m.update(&[finger(1, 100)], 0, &off).unwrap();
        assert!(out.is_empty());
        assert!(m.force_active().is_none());
        assert!(!m.finger_force_pressed(1));
        assert!(m.active().is_some());
    }

    #[test]
    fn six_finger_clamp_both_levels() {
        let cfg = Config {
            click: crate::config::ClickConfig {
                sensitivity: 64,
                margin: 5,
                ..Config::default().click
            },
            force: crate::config::ForceConfig {
                force_offset: 20,
                ..Config::default().force
            },
            ..Config::default()
        };
        let mut m = ClickMachine::new();
        let fingers: Vec<FingerContact> = (1..=6).map(|id| finger(id, 250)).collect();
        // Staged: frame one latches first (count 5), frame two force.
        let out = m.update(&fingers, 0, &cfg).unwrap();
        assert_eq!(out.first.as_ref().unwrap().finger_count, 5);
        assert!(out.force.is_none());
        let out = m.update(&fingers, 0, &cfg).unwrap();
        assert_eq!(out.force.as_ref().unwrap().finger_count, 5);
    }

    #[test]
    fn force_margin_zero_mirrors_first() {
        let cfg = Config {
            click: crate::config::ClickConfig {
                sensitivity: 64,
                margin: 0,
                ..Config::default().click
            },
            force: crate::config::ForceConfig {
                force_offset: 20,
                force_1: "forward".into(),
                ..Config::default().force
            },
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
            click: crate::config::ClickConfig {
                sensitivity: 0,
                margin: 300,
                ..Config::default().click
            },
            force: crate::config::ForceConfig {
                force_offset: 300,
                ..Config::default().force
            },
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
            click: crate::config::ClickConfig {
                sensitivity: 64,
                margin: 5,
                ..Config::default().click
            },
            force: crate::config::ForceConfig {
                force_offset: 20,
                force_1: "forward".into(),
                force_2: "forward".into(),
                ..Config::default().force
            },
            ..Config::default()
        };
        let mut m = ClickMachine::new();
        // A deep + B medium: first latches frame one (count 2), force
        // follows frame two while A stays deep (old-exact staging).
        let out = m.update(&[finger(1, 100), finger(2, 70)], 0, &cfg).unwrap();
        assert!(out.first.is_some() && out.force.is_none());
        let out = m.update(&[finger(1, 100), finger(2, 70)], 0, &cfg).unwrap();
        assert!(out.first.is_none() && out.force.is_some());
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
            click: crate::config::ClickConfig {
                click_1: "bogus-key".into(),
                ..Config::default().click
            },
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

    fn at(base: Instant, ms: u64) -> Instant {
        base + std::time::Duration::from_millis(ms)
    }

    /// Owner's tune (6 months on old driver): sensitivity 30, haptic 2
    /// quiet, force_offset 130, force_haptic 1. Margin/defaults otherwise.
    fn owner_cfg() -> Config {
        Config {
            click: crate::config::ClickConfig {
                sensitivity: 30,
                margin: 5,
                ..Config::default().click
            },
            haptics: crate::config::HapticsConfig {
                haptic: 2,
                quiet_click: true,
                force_haptic: Some(1),
                force_quiet_click: false,
            },
            force: crate::config::ForceConfig {
                force_offset: 130,
                force_1: "forward".into(),
                ..Config::default().force
            },
            ..Config::default()
        }
    }

    #[test]
    fn owner_tune_thresholds_match_old() {
        // Old: force press at 30 + 130 = 160, force release at 160 - 5.
        let cfg = owner_cfg();
        assert_eq!(press_threshold(&cfg), 30);
        assert_eq!(release_threshold(&cfg), 25);
        assert_eq!(ClickMachine::force_threshold(&cfg), Some(160));
        assert_eq!(ClickMachine::force_release_threshold(&cfg), Some(155));
    }

    #[test]
    fn owner_tune_staged_edges_carry_old_haptic_bytes() {
        // Staged arrival: medium press latches first (intensity 2, quiet),
        // deepening past 160 latches force (intensity 1, loud).
        let cfg = owner_cfg();
        let mut m = ClickMachine::new();
        let out = m.update(&[finger(1, 50)], 0, &cfg).unwrap();
        let first = out.first.unwrap();
        assert!(!first.is_force());
        assert_eq!(first.haptic, 2);
        assert!(first.quiet);
        assert!(out.force.is_none());
        let out = m.update(&[finger(1, 200)], 0, &cfg).unwrap();
        assert!(out.first.is_none());
        let force = out.force.unwrap();
        assert!(force.is_force());
        assert_eq!(force.haptic, 1);
        assert!(!force.quiet);
    }

    #[test]
    fn owner_tune_deep_jump_stages_force_one_frame_later() {
        // Old-exact: Released admits Pressed only (one transition per
        // report), so a deep stab latches first this frame and force
        // follows on the next report — never both on the same frame.
        let cfg = owner_cfg();
        let mut m = ClickMachine::new();
        let out = m.update(&[finger(1, 200)], 0, &cfg).unwrap();
        assert!(out.first.is_some());
        assert!(
            out.force.is_none(),
            "old stages force a frame later, no same-frame double edge"
        );
        assert!(m.force_active().is_none());
        assert_eq!(out.primary().unwrap().level, ClickLevel::First);
        // Still deep next frame: force edge arrives with old-exact bytes.
        let out = m.update(&[finger(1, 200)], 0, &cfg).unwrap();
        assert!(out.first.is_none());
        assert!(out.primary().unwrap().is_force());
        let force = out.force.unwrap();
        assert!(force.is_force());
        assert_eq!(force.haptic, 1);
        assert!(!force.quiet);
    }

    #[test]
    fn owner_tune_force_release_returns_to_first_hold() {
        // 100 < 155 (force release) but > 25 (first release): force drops,
        // first holds. 10 drops all.
        let cfg = owner_cfg();
        let mut m = ClickMachine::new();
        m.update(&[finger(1, 50)], 0, &cfg).unwrap();
        m.update(&[finger(1, 200)], 0, &cfg).unwrap();
        assert!(m.force_active().is_some());
        assert!(m.update(&[finger(1, 100)], 0, &cfg).unwrap().is_empty());
        assert!(m.force_active().is_none());
        assert!(m.active().is_some());
        assert!(m.update(&[finger(1, 10)], 0, &cfg).unwrap().is_empty());
        assert!(m.active().is_none());
        assert!(m.force_active().is_none());
    }

    #[test]
    fn force_hold_hysteresis_survives_dip_then_liftoff_releases() {
        // Tune 30/130/5: press 30, force 190, force-release 155. A dip to
        // 150 (noise just under release, still firmly pressed) holds the
        // latch inside the 30 ms hold window; past the window it drops;
        // liftoff always drops. (Old parity dropped undebounced.)
        let cfg = Config {
            click: crate::config::ClickConfig {
                sensitivity: 30,
                margin: 5,
                ..Config::default().click
            },
            force: crate::config::ForceConfig {
                force_offset: 130,
                ..Config::default().force
            },
            ..Config::default()
        };
        let mut m = ClickMachine::new();
        let t0 = Instant::now();
        assert!(m
            .update_at(&[finger(1, 40)], 0, &cfg, at(t0, 0))
            .unwrap()
            .first
            .is_some());
        // Deep stab: first already latched, force edges immediately.
        let t1 = at(t0, 60);
        assert!(m
            .update_at(&[finger(1, 200)], 0, &cfg, t1)
            .unwrap()
            .force
            .is_some());
        let t2 = at(t1, 60);
        assert!(m
            .update_at(&[finger(1, 200)], 0, &cfg, t2)
            .unwrap()
            .is_empty());
        assert!(m.force_active().is_some());
        // Dip 10 ms after the last held frame: no edge, latch holds.
        let t3 = at(t2, 10);
        assert!(m
            .update_at(&[finger(1, 150)], 0, &cfg, t3)
            .unwrap()
            .is_empty());
        assert!(m.force_active().is_some(), "dip holds the latch");
        // Back deep, then a dip 40 ms after the last held frame: drops.
        let t4 = at(t3, 10);
        assert!(m
            .update_at(&[finger(1, 200)], 0, &cfg, t4)
            .unwrap()
            .is_empty());
        let t5 = at(t4, 40);
        assert!(m
            .update_at(&[finger(1, 150)], 0, &cfg, t5)
            .unwrap()
            .is_empty());
        assert!(m.force_active().is_none(), "window expiry drops");
        // Latch re-edges on the next deep frame (not stuck released).
        let t6 = at(t5, 60);
        assert!(m
            .update_at(&[finger(1, 200)], 0, &cfg, t6)
            .unwrap()
            .force
            .is_some());
        // Clean liftoff releases everything.
        assert!(m.update_at(&[], 0, &cfg, at(t6, 60)).unwrap().is_empty());
        assert!(m.force_active().is_none());
        assert!(m.active().is_none());
    }

    #[test]
    fn press_debounce_swallows_fast_repress_then_retries() {
        // Old 50 ms press gate: release at T, re-press at T+20 swallowed
        // (bits reverted, no latch), re-press at T+60 fires.
        let cfg = Config::default();
        let mut m = ClickMachine::new();
        let t0 = Instant::now();
        assert!(m
            .update_at(&[finger(1, 200)], 0, &cfg, at(t0, 0))
            .unwrap()
            .first
            .is_some());
        assert!(m
            .update_at(&[finger(1, 0)], 0, &cfg, at(t0, 100))
            .unwrap()
            .is_empty());
        assert!(m.active().is_none());
        // 20 ms after release: swallowed, latch stays idle, bits reverted.
        let out = m
            .update_at(&[finger(1, 200)], 0, &cfg, at(t0, 120))
            .unwrap();
        assert!(out.is_empty());
        assert!(m.active().is_none());
        assert!(!m.finger_pressed(1));
        // 60 ms after release: fires.
        let out = m
            .update_at(&[finger(1, 200)], 0, &cfg, at(t0, 160))
            .unwrap();
        assert!(out.first.is_some());
    }

    #[test]
    fn release_debounce_holds_quick_tap_then_drops() {
        // Old 30 ms release gate: press at T, pressure drop at T+10 holds
        // the latch; drop at T+40 releases.
        let cfg = Config::default();
        let mut m = ClickMachine::new();
        let t0 = Instant::now();
        assert!(m
            .update_at(&[finger(1, 200)], 0, &cfg, at(t0, 0))
            .unwrap()
            .first
            .is_some());
        assert!(m
            .update_at(&[finger(1, 10)], 0, &cfg, at(t0, 10))
            .unwrap()
            .is_empty());
        assert!(m.active().is_some(), "quick release must hold the latch");
        assert!(m
            .update_at(&[finger(1, 10)], 0, &cfg, at(t0, 40))
            .unwrap()
            .is_empty());
        assert!(m.active().is_none());
    }

    #[test]
    fn force_held_release_ignores_release_debounce() {
        // Old releases ForcePressed immediately (no 30 ms hold): staged
        // latch (first at T, force at T+10), full liftoff at T+20 still
        // drops both latches on that frame.
        let cfg = Config {
            click: crate::config::ClickConfig {
                sensitivity: 64,
                margin: 5,
                ..Config::default().click
            },
            force: crate::config::ForceConfig {
                force_offset: 20,
                force_1: "forward".into(),
                ..Config::default().force
            },
            ..Config::default()
        };
        let mut m = ClickMachine::new();
        let t0 = Instant::now();
        assert!(m
            .update_at(&[finger(1, 70)], 0, &cfg, at(t0, 0))
            .unwrap()
            .first
            .is_some());
        assert!(m
            .update_at(&[finger(1, 100)], 0, &cfg, at(t0, 10))
            .unwrap()
            .force
            .is_some());
        assert!(m.update_at(&[], 0, &cfg, at(t0, 20)).unwrap().is_empty());
        assert!(
            m.active().is_none(),
            "force-held release must not debounce-hold the latch"
        );
        assert!(m.force_active().is_none());
    }

    #[test]
    fn timeless_update_ignores_debounce_clocks() {
        // The timeless core never gates: rapid press/release via update()
        // behaves exactly as before (existing hysteresis tests rely on it).
        let cfg = Config::default();
        let mut m = ClickMachine::new();
        let t0 = Instant::now();
        assert!(m
            .update_at(&[finger(1, 200)], 0, &cfg, t0)
            .unwrap()
            .first
            .is_some());
        // Timeless release drops immediately even inside the window.
        assert!(m.update(&[finger(1, 10)], 0, &cfg).unwrap().is_empty());
        assert!(m.active().is_none());
        // Timeless press fires immediately even inside the window.
        assert!(m
            .update(&[finger(1, 200)], 0, &cfg)
            .unwrap()
            .first
            .is_some());
    }

    #[test]
    fn touch_floor_drives_presence_below_click_threshold() {
        // contact=false but pressure 20 (> 10): present for slots and
        // finger-count, still below the click threshold (no latch).
        let cfg = Config::default();
        let hover = FingerContact {
            contact: false,
            ..finger(1, 20)
        };
        assert!(touch_present(&hover));
        let mut m = ClickMachine::new();
        assert!(m.update(&[hover], 0, &cfg).unwrap().is_empty());
        assert!(m.active().is_none());
        // contact=false but pressure past sensitivity: old max_pressure
        // parity — the click still latches.
        let deep = FingerContact {
            contact: false,
            ..finger(1, 200)
        };
        let ev = m.update(&[deep], 0, &cfg).unwrap().first.unwrap();
        assert_eq!(ev.finger_count, 1);
    }

    #[test]
    fn absent_finger_does_not_hold_latch() {
        // Old-service parity: release follows the CURRENT frame's max
        // pressure, so a finger that lifted (absent from the frame) must
        // not hold the latch while the remaining finger moves lightly.
        // Stale-bit hold = BTN stuck down across motion = ghostty drag.
        let cfg = Config {
            click: crate::config::ClickConfig {
                click_2: "right".into(),
                ..Config::default().click
            },
            ..Config::default()
        };
        let mut m = ClickMachine::new();
        let ev = m
            .update(&[finger(1, 200), finger(2, 200)], 0, &cfg)
            .unwrap()
            .first
            .unwrap();
        assert_eq!(ev.finger_count, 2);
        // Finger 2 lifts (omitted, as the device reports it); finger 1
        // stays, light and moving (pressure 10 <= release 59).
        assert!(m.update(&[finger(1, 10)], 0, &cfg).unwrap().is_empty());
        assert!(
            m.active().is_none(),
            "lifted finger holds stale latch (old: max_pressure 10 < 59 releases)"
        );
    }

    #[test]
    fn force_parse_error_does_not_swallow_first_edge() {
        let bad = Config {
            click: crate::config::ClickConfig {
                sensitivity: 64,
                margin: 5,
                ..Config::default().click
            },
            force: crate::config::ForceConfig {
                force_offset: 20,
                force_1: "bogus-key".into(),
                ..Config::default().force
            },
            ..Config::default()
        };
        let mut m = ClickMachine::new();
        assert!(m.update(&[finger(1, 250)], 0, &bad).is_err());
        // Neither level latched: the good first edge is retried, not lost.
        assert!(m.active().is_none());
        assert!(m.force_active().is_none());
        let mut good = bad;
        good.force.force_1 = "forward".into();
        // Staged retry: frame one latches first only, frame two force.
        let out = m.update(&[finger(1, 250)], 0, &good).unwrap();
        assert!(out.first.is_some() && out.force.is_none());
        let out = m.update(&[finger(1, 250)], 0, &good).unwrap();
        assert!(out.first.is_none() && out.force.is_some());
    }

    fn light(id: u8, pressure: u8) -> FingerContact {
        finger(id, pressure)
    }

    #[test]
    fn slide_default_none_never_latches() {
        // Default slide_N = none: light touches move the cursor (slots)
        // with zero latch state — current behavior exactly.
        let cfg = Config::default();
        let mut m = ClickMachine::new();
        let out = m.update(&[light(1, 20)], 0, &cfg).unwrap();
        assert!(out.is_empty());
        assert!(m.slide_active().is_none());
        let out = m.update(&[light(1, 20), light(2, 20)], 0, &cfg).unwrap();
        assert!(out.is_empty());
        assert!(m.slide_active().is_none());
    }

    #[test]
    fn slide_fires_holds_releases() {
        let cfg = Config {
            slide: crate::config::SlideConfig {
                slide_1: "left".into(),
                ..Config::default().slide
            },
            ..Config::default()
        };
        let mut m = ClickMachine::new();
        let out = m.update(&[light(1, 20)], 0, &cfg).unwrap();
        assert!(out.first.is_none() && out.force.is_none());
        let sev = out.slide.unwrap();
        assert!(sev.is_slide());
        assert_eq!(sev.finger_count, 1);
        assert_eq!(sev.haptic, cfg.haptics.haptic);
        // Steady slide: holds, no re-fire.
        assert!(m.update(&[light(1, 25)], 0, &cfg).unwrap().is_empty());
        assert!(m.slide_active().is_some());
        // Liftoff releases.
        assert!(m.update(&[], 0, &cfg).unwrap().is_empty());
        assert!(m.slide_active().is_none());
    }

    #[test]
    fn slide_entry_needs_below_release_band_is_dead() {
        // sensitivity 64 / margin 5: release 59, press 64.
        let cfg = Config {
            click: crate::config::ClickConfig {
                sensitivity: 64,
                margin: 5,
                ..Config::default().click
            },
            slide: crate::config::SlideConfig {
                slide_1: "left".into(),
                ..Config::default().slide
            },
            ..Config::default()
        };
        let mut m = ClickMachine::new();
        // Hysteresis band (62): neither slide nor press.
        assert!(m.update(&[light(1, 62)], 0, &cfg).unwrap().is_empty());
        assert!(m.slide_active().is_none());
        assert!(m.active().is_none());
        // At release (59): slide fires.
        assert!(m.update(&[light(1, 59)], 0, &cfg).unwrap().slide.is_some());
        // Press edge hands off: click fires, slide drops.
        let mut m = ClickMachine::new();
        m.update(&[light(1, 20)], 0, &cfg).unwrap();
        assert!(m.slide_active().is_some());
        let out = m.update(&[light(1, 200)], 0, &cfg).unwrap();
        assert!(out.first.is_some());
        assert!(out.slide.is_none());
        assert!(m.slide_active().is_none());
        assert!(m.active().is_some());
    }

    #[test]
    fn slide_count_change_reselects() {
        let cfg = Config {
            slide: crate::config::SlideConfig {
                slide_1: "left".into(),
                slide_2: "right".into(),
                ..Config::default().slide
            },
            ..Config::default()
        };
        let mut m = ClickMachine::new();
        assert!(m.update(&[light(1, 20)], 0, &cfg).unwrap().slide.is_some());
        // Second finger down light: re-select fires slide_2.
        let out = m.update(&[light(1, 20), light(2, 20)], 0, &cfg).unwrap();
        let sev = out.slide.unwrap();
        assert_eq!(sev.finger_count, 2);
        assert_eq!(sev.action.kind, ActionKind::Keys(vec!["right".to_string()]));
        // Re-select onto an unmapped count releases silently and remembers
        // the count: later same-count frames hold without re-parsing.
        let cfg = Config {
            slide: crate::config::SlideConfig {
                slide_1: "left".into(),
                ..Config::default().slide
            },
            ..Config::default()
        };
        let mut m = ClickMachine::new();
        m.update(&[light(1, 20)], 0, &cfg).unwrap();
        let out = m.update(&[light(1, 20), light(2, 20)], 0, &cfg).unwrap();
        assert!(out.is_empty());
        assert!(m.slide_active().is_none());
        let out = m.update(&[light(1, 20), light(2, 20)], 0, &cfg).unwrap();
        assert!(out.is_empty(), "remembered unmapped count never re-fires");
        assert!(m.slide_active().is_none());
    }

    #[test]
    fn slide_reselect_skips_band_predicate() {
        // sensitivity 64 / margin 5: band is 60..=63. A held slide whose
        // new finger lands in the band re-selects (no stale action); the
        // band still blocks *initial* entry (dead zone, no press either).
        let cfg = Config {
            click: crate::config::ClickConfig {
                sensitivity: 64,
                margin: 5,
                ..Config::default().click
            },
            slide: crate::config::SlideConfig {
                slide_1: "left".into(),
                slide_2: "right".into(),
                ..Config::default().slide
            },
            ..Config::default()
        };
        let mut m = ClickMachine::new();
        m.update(&[light(1, 20)], 0, &cfg).unwrap();
        let out = m.update(&[light(1, 20), light(2, 62)], 0, &cfg).unwrap();
        let sev = out
            .slide
            .expect("band finger must re-select, not hold stale");
        assert_eq!(sev.finger_count, 2);
        // Fresh entry with a band finger: neither slide nor press.
        let mut m = ClickMachine::new();
        assert!(m.update(&[light(1, 62)], 0, &cfg).unwrap().is_empty());
        assert!(m.slide_active().is_none());
        assert!(m.active().is_none());
    }

    #[test]
    fn slide_none_holds_until_press_then_fires_next_frame() {
        // Click releases under still-touching light fingers: the release
        // frame drops the latch, the NEXT light frame selects slide.
        let cfg = Config {
            slide: crate::config::SlideConfig {
                slide_1: "left".into(),
                ..Config::default().slide
            },
            ..Config::default()
        };
        let mut m = ClickMachine::new();
        m.update(&[finger(1, 200)], 0, &cfg).unwrap();
        let out = m.update(&[light(1, 20)], 0, &cfg).unwrap();
        assert!(m.active().is_none());
        assert!(out.slide.is_none(), "slide arms on the following frame");
        let out = m.update(&[light(1, 20)], 0, &cfg).unwrap();
        assert!(out.slide.is_some());
    }

    #[test]
    fn slide_parse_error_leaves_state_untouched() {
        let bad = Config {
            slide: crate::config::SlideConfig {
                slide_1: "bogus-key".into(),
                ..Config::default().slide
            },
            ..Config::default()
        };
        let mut m = ClickMachine::new();
        assert!(m.update(&[light(1, 20)], 0, &bad).is_err());
        assert!(m.slide_active().is_none());
        let out = m.update(&[light(1, 20)], 0, &Config::default()).unwrap();
        assert!(out.is_empty(), "slide none latches nothing on retry");
    }

    #[test]
    fn slide_empty_is_haptic_only() {
        let cfg = Config {
            haptics: crate::config::HapticsConfig {
                haptic: 7,
                ..Config::default().haptics
            },
            slide: crate::config::SlideConfig {
                slide_1: "empty".into(),
                ..Config::default().slide
            },
            ..Config::default()
        };
        let mut m = ClickMachine::new();
        let sev = m.update(&[light(1, 20)], 0, &cfg).unwrap().slide.unwrap();
        assert!(sev.is_empty());
        assert!(should_emit_haptic(sev.haptic));
    }

    #[test]
    fn slide_primary_fallback() {
        // A lone slide edge is what reporters see; a press edge wins.
        let cfg = Config {
            slide: crate::config::SlideConfig {
                slide_1: "left".into(),
                ..Config::default().slide
            },
            ..Config::default()
        };
        let mut m = ClickMachine::new();
        let out = m.update(&[light(1, 20)], 0, &cfg).unwrap();
        assert!(out.primary().unwrap().is_slide());
    }

    #[test]
    fn spoof_actions_follow_spoof_switches_action_lookup() {
        let mut click = std::collections::BTreeMap::new();
        click.insert(2u8, 3u16);
        let base = Config {
            spoof: crate::config::SpoofConfig {
                click,
                ..Default::default()
            },
            click: crate::config::ClickConfig {
                click_2: "right".into(),
                click_3: "middle".into(),
                ..Config::default().click
            },
            ..Config::default()
        };
        // Default: selection uses the real count (2 → right), even though
        // the wire reports 3.
        let mut m = ClickMachine::new();
        let ev = m
            .update(&[finger(1, 200), finger(2, 200)], 0, &base)
            .unwrap()
            .first
            .unwrap();
        assert_eq!(ev.finger_count, 2);
        assert_eq!(ev.action.kind, ActionKind::Keys(vec!["right".to_string()]));
        // Opt-in: selection uses the spoofed count (3 → middle).
        let mut cfg = base.clone();
        cfg.spoof.actions_follow_spoof = true;
        let mut m = ClickMachine::new();
        let ev = m
            .update(&[finger(1, 200), finger(2, 200)], 0, &cfg)
            .unwrap()
            .first
            .unwrap();
        assert_eq!(ev.finger_count, 3);
        assert_eq!(ev.action.kind, ActionKind::Keys(vec!["middle".to_string()]));
    }

    #[test]
    fn spoof_actions_follow_spoof_slide_uses_virtual_count() {
        let mut slide = std::collections::BTreeMap::new();
        slide.insert(1u8, 3u16);
        let cfg = Config {
            spoof: crate::config::SpoofConfig {
                slide,
                actions_follow_spoof: true,
                ..Default::default()
            },
            slide: crate::config::SlideConfig {
                slide_1: "left".into(),
                slide_3: "middle".into(),
                ..Config::default().slide
            },
            ..Config::default()
        };
        let mut m = ClickMachine::new();
        // One real finger spoofs to 3: slide_3 fires, reported count 3.
        let sev = m.update(&[light(1, 20)], 0, &cfg).unwrap().slide.unwrap();
        assert_eq!(sev.finger_count, 3);
        assert_eq!(
            sev.action.kind,
            ActionKind::Keys(vec!["middle".to_string()])
        );
        // …and the hold sticks on the real count (no per-frame re-fire).
        assert!(m.update(&[light(1, 20)], 0, &cfg).unwrap().is_empty());
        assert!(m.slide_active().is_some());
    }

    #[test]
    fn spoof_contacts_identity_on_equal_count() {
        let real = vec![finger(7, 200), finger(3, 200)];
        let out = spoof_contacts(&real, 2);
        assert_eq!(out, real, "equal counts preserve ids/positions");
    }

    #[test]
    fn spoof_contacts_empty_and_zero() {
        assert!(spoof_contacts(&[], 3).is_empty());
        assert!(spoof_contacts(&[finger(1, 200)], 0).is_empty());
    }

    #[test]
    fn spoof_contacts_pair_splits_on_centroid() {
        let mut a = finger(1, 200);
        a.x = 1000;
        a.y = 500;
        let out = spoof_contacts(&[a], 2);
        assert_eq!(out.len(), 2);
        let cx = (out[0].x as i64 + out[1].x as i64) / 2;
        let cy = (out[0].y as i64 + out[1].y as i64) / 2;
        assert_eq!((cx, cy), (1000, 500));
        assert_eq!(out[1].x - out[0].x, 2 * SPOOF_PAIR_DX);
        assert_eq!(out[0].y, out[1].y);
        assert!(out.iter().all(|f| f.contact));
    }

    #[test]
    fn spoof_contacts_ring_centroid_and_spacing() {
        let mut a = finger(1, 200);
        a.x = 0;
        a.y = 0;
        for want in [3u8, 4, 5] {
            let out = spoof_contacts(&[a], want);
            assert_eq!(out.len(), want as usize);
            let cx = out.iter().map(|f| i64::from(f.x)).sum::<i64>() / want as i64;
            let cy = out.iter().map(|f| i64::from(f.y)).sum::<i64>() / want as i64;
            // Even ring around the centroid: mean ≈ center (±1 rounding).
            assert!(
                (cx, cy).0.abs() <= 1 && (cy).abs() <= 1,
                "want {want}: ({cx},{cy})"
            );
            for f in &out {
                let dx = f.x as i64;
                let dy = f.y as i64;
                let r = ((dx * dx + dy * dy) as f64).sqrt().round() as i64;
                assert_eq!(r, i64::from(SPOOF_RING_RADIUS), "want {want}");
            }
        }
        // Single: exactly the centroid.
        let out = spoof_contacts(&[a], 1);
        assert_eq!((out[0].x, out[0].y), (0, 0));
    }

    #[test]
    fn spoof_contacts_clamp_to_axes() {
        let mut a = finger(1, 200);
        a.x = SPOOF_X_MAX;
        a.y = SPOOF_Y_MAX;
        let out = spoof_contacts(&[a], 3);
        for f in &out {
            assert!((SPOOF_X_MIN..=SPOOF_X_MAX).contains(&f.x));
            assert!((SPOOF_Y_MIN..=SPOOF_Y_MAX).contains(&f.y));
        }
    }
}
