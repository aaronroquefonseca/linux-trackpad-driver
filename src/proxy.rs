//! evdev grab + uinput click proxy (opt-in).
//!
//! Reimplemented from `docs/ARCHITECTURE.md` layering + `docs/RESEARCH.md`
//! §6 (uinput contract). No reference code copied, no new dependencies:
//! grab (`EVIOCGRAB`) and uinput emit go through `libc` ioctls/writes so the
//! build stays hermetic. `evdev`/`uinput` wrapper crates were considered and
//! rejected: they add review surface for what is here a ~30-line ioctl plus
//! pure mapping already covered by tests.
//!
//! # Rules
//!
//! - Grab ONLY when `enhanced_clicks=true` AND the pump is ready (a real
//!   `TouchSource` plus a draining pump loop plus a created uinput node).
//!   Default false → zero behavior change, pure passthrough: no grab, no
//!   uinput traffic. Grabbing without an emit path bricks the device, so
//!   `attach` degrades to passthrough until readiness holds.
//! - The physical evdev node is never guessed: `LTD_EVDEV_NODE` must name it
//!   (hidraw→eventN auto-resolution belongs to the transport task). Unset →
//!   refuse + passthrough, never `event0` roulette.
//! - Proxy node: Type-B MT slots + tracking IDs, `res == 0` arbitrary
//!   0..253 range, `BTN_LEFT`/`BTN_TOUCH` + `SYN`. NEVER advertises
//!   `EV_FF`/`FF_HAPTIC` (no 2026 libinput/Wayland consumer).
//! - `BTN_LEFT` derives from held codes (present only when the latched action
//!   contains `left`) OR-ed with the physical `clicks & 1` bit — never from
//!   latch state alone (a right/combo/empty hold must not chord left).
//! - Slots are positional per frame (see [`ClickProxy::emit_touch_frame`]
//!   slot-map docs); vacated slots are terminated with `TRACKING_ID=-1`.
//! - Orientation emits `-(raw)`; pressure clamps to 253 (kernel `absmax`).
//! - `poll_frame` errors → log + drop that frame, never abort the loop.
//! - Grab failure → log + passthrough continues. Clicks NEVER blocks init:
//!   hub callbacks do bounded work only (no loops), and panics are already
//!   isolated by [`crate::init::DeviceReadyHub`].
//! - `speed` scales pointer deltas in the proxy path only; init untouched.
//! - Force second level: [`crate::clicks::ClickMachine`] latches an
//!   independent force edge (`force_1..=force_5`, `effective_force_haptic`,
//!   `force_quiet_click`) while the first-level hold continues. The proxy
//!   holds both key sets; force supersedes in reporting while past threshold.
//!   Dropping below the force release (`sensitivity + offset - margin`)
//!   releases force keys only back to the first-level hold; dropping below
//!   the first release clears all. `force_offset = 0` disables force.
//! - Shared codes across levels (`ctrl` in both chords, `left` in both maps)
//!   are reference-held: down once, retained while either level holds, a
//!   single up on final release — no phantom release+press mid-hold.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use anyhow::Result;

use crate::apple_parse::{TouchFrame, TouchSource};
use crate::clicks::{self, ClickEvent, ClickMachine};
use crate::config::Config;
use crate::init::{DeviceReadyEvent, DeviceReadyHub, ReadyKind};

// ---- evdev/uinput constants (linux/input-event-codes.h) ----

pub const EV_SYN: u16 = 0x00;
pub const EV_KEY: u16 = 0x01;
pub const EV_ABS: u16 = 0x03;
/// Deliberately never used: no 2026 consumer (RESEARCH §6). Pinned by test.
pub const EV_FF: u16 = 0x15;
pub const SYN_REPORT: u16 = 0;
pub const ABS_MT_SLOT: u16 = 47;
pub const ABS_MT_ORIENTATION: u16 = 52;
pub const ABS_MT_POSITION_X: u16 = 53;
pub const ABS_MT_POSITION_Y: u16 = 54;
pub const ABS_MT_TRACKING_ID: u16 = 57;
pub const ABS_MT_PRESSURE: u16 = 58;
pub const BTN_LEFT: u16 = 272;
pub const BTN_RIGHT: u16 = 273;
pub const BTN_MIDDLE: u16 = 274;
pub const BTN_SIDE: u16 = 275;
pub const BTN_EXTRA: u16 = 276;
pub const BTN_TOUCH: u16 = 330;

/// `EVIOCGRAB` = `_IOW('E', 0x90, int)`.
pub fn eviocgrab_request() -> u64 {
    (1u64 << 30) | ((4u64) << 16) | ((u64::from(b'E')) << 8) | 0x90
}

/// Event-bit disposition for the virtual node (honest: the setup ioctls
/// `UI_SET_EVBIT`/`UI_ABS_SETUP`/`UI_DEV_CREATE` land with the transport
/// task, which must set exactly these bits). `EV_FF` must never appear —
/// no 2026 libinput/Wayland consumer (RESEARCH §6). Pinned by test.
pub fn uinput_evbits() -> Vec<u16> {
    vec![EV_SYN, EV_KEY, EV_ABS]
}

// ---- injectable hardware traits (mocks behind these in tests) ----

/// Exclusive-grab handle for the PHYSICAL node.
pub trait GrabDevice: Send {
    fn grab(&self) -> Result<()>;
    fn ungrab(&self);
    fn is_grabbed(&self) -> bool;
}

/// Emit handle for the VIRTUAL uinput node.
pub trait UinputSink: Send {
    fn emit(&self, ev_type: u16, code: u16, value: i32);
    fn sync(&self) {
        self.emit(EV_SYN, SYN_REPORT, 0);
    }
}

/// Haptic feedback sender (host-click report path).
pub trait HapticSender: Send {
    fn click_feedback(&self, strength: u8, quiet: bool) -> Result<()>;
}

/// Log-only haptic sender (default when no hardware wired).
#[derive(Debug, Default)]
pub struct LogHaptic;

impl HapticSender for LogHaptic {
    fn click_feedback(&self, strength: u8, quiet: bool) -> Result<()> {
        if strength == 0 {
            return Ok(());
        }
        log::info!("haptic feedback: strength={strength} quiet={quiet} (report 21 01)");
        Ok(())
    }
}

/// Map a config key/button token (lowercased) to `(ev_type, code)`.
/// Mouse buttons map to `BTN_*`; keyboard tokens to `EV_KEY` codes.
/// Returns `None` for unmapped tokens (caller logs + skips).
pub fn key_token_to_code(token: &str) -> Option<(u16, u16)> {
    match token {
        "left" => Some((EV_KEY, BTN_LEFT)),
        "right" => Some((EV_KEY, BTN_RIGHT)),
        "middle" => Some((EV_KEY, BTN_MIDDLE)),
        "back" => Some((EV_KEY, BTN_SIDE)),
        "forward" => Some((EV_KEY, BTN_EXTRA)),
        "ctrl" | "control" | "lctrl" => Some((EV_KEY, 29)),
        "rctrl" => Some((EV_KEY, 97)),
        "shift" | "lshift" => Some((EV_KEY, 42)),
        "rshift" => Some((EV_KEY, 54)),
        "alt" | "lalt" => Some((EV_KEY, 56)),
        "ralt" => Some((EV_KEY, 100)),
        "meta" | "super" | "cmd" | "command" | "win" | "lmeta" => Some((EV_KEY, 125)),
        "rmeta" => Some((EV_KEY, 126)),
        "esc" | "escape" => Some((EV_KEY, 1)),
        "enter" | "return" => Some((EV_KEY, 28)),
        "space" => Some((EV_KEY, 57)),
        "tab" => Some((EV_KEY, 15)),
        "backspace" => Some((EV_KEY, 14)),
        "delete" => Some((EV_KEY, 111)),
        "home" => Some((EV_KEY, 102)),
        "end" => Some((EV_KEY, 107)),
        "pageup" | "pgup" => Some((EV_KEY, 104)),
        "pagedown" | "pgdn" => Some((EV_KEY, 109)),
        "up" | "arrow_up" => Some((EV_KEY, 103)),
        "down" | "arrow_down" => Some((EV_KEY, 108)),
        "left_arrow" => Some((EV_KEY, 105)),
        "right_arrow" => Some((EV_KEY, 106)),
        "volup" => Some((EV_KEY, 115)),
        "voldown" => Some((EV_KEY, 114)),
        "mute" => Some((EV_KEY, 113)),
        "play" => Some((EV_KEY, 164)),
        "pause" => Some((EV_KEY, 119)),
        s if s.len() == 1 => keycode_letter_digit(s),
        s if s.starts_with('f') => keycode_fn(s),
        _ => None,
    }
}

fn keycode_letter_digit(s: &str) -> Option<(u16, u16)> {
    let c = s.chars().next()?;
    let code = match c {
        'a' => 30,
        'b' => 48,
        'c' => 46,
        'd' => 32,
        'e' => 18,
        'f' => 33,
        'g' => 34,
        'h' => 35,
        'i' => 23,
        'j' => 36,
        'k' => 37,
        'l' => 38,
        'm' => 50,
        'n' => 49,
        'o' => 24,
        'p' => 25,
        'q' => 16,
        'r' => 19,
        's' => 31,
        't' => 20,
        'u' => 22,
        'v' => 47,
        'w' => 17,
        'x' => 45,
        'y' => 21,
        'z' => 44,
        '1' => 2,
        '2' => 3,
        '3' => 4,
        '4' => 5,
        '5' => 6,
        '6' => 7,
        '7' => 8,
        '8' => 9,
        '9' => 10,
        '0' => 11,
        _ => return None,
    };
    Some((EV_KEY, code))
}

/// `KEY_F1..F10` are contiguous (`59..=68`); `F11`/`F12` jump to 87/88
/// (`69`/`70` are `NUMLOCK`/`SCROLLLOCK` — a prior rev mapped them wrong).
/// `KEY_F13..F24` are contiguous (`183..=194`); each value verified
/// individually against `linux/input-event-codes.h`, never extrapolated.
fn keycode_fn(s: &str) -> Option<(u16, u16)> {
    let n: u8 = s[1..].parse().ok()?;
    match n {
        1..=10 => Some((EV_KEY, 58 + u16::from(n))),
        11 => Some((EV_KEY, 87)),
        12 => Some((EV_KEY, 88)),
        13 => Some((EV_KEY, 183)),
        14 => Some((EV_KEY, 184)),
        15 => Some((EV_KEY, 185)),
        16 => Some((EV_KEY, 186)),
        17 => Some((EV_KEY, 187)),
        18 => Some((EV_KEY, 188)),
        19 => Some((EV_KEY, 189)),
        20 => Some((EV_KEY, 190)),
        21 => Some((EV_KEY, 191)),
        22 => Some((EV_KEY, 192)),
        23 => Some((EV_KEY, 193)),
        24 => Some((EV_KEY, 194)),
        _ => None,
    }
}

// ---- proxy ----

/// Gate snapshot for the proxy. Single source is [`Config`]: this carries
/// only the opt-in bit. Haptic strength/quiet ride on each [`ClickEvent`];
/// pointer `speed` is read from the `&Config` passed per frame, so a restart
/// with new tuning applies without touching this struct.
#[derive(Debug, Clone)]
pub struct ProxySettings {
    pub enhanced: bool,
}

impl From<&Config> for ProxySettings {
    fn from(c: &Config) -> Self {
        Self {
            enhanced: c.enhanced_clicks,
        }
    }
}

/// evdev-grab + uinput click proxy. Generic over injectable backends so unit
/// tests run without hardware.
pub struct ClickProxy<G, U, H, S> {
    settings: ProxySettings,
    grab: G,
    uinput: U,
    haptic: H,
    source: S,
    machine: ClickMachine,
    attached: bool,
    passthrough: bool,
    /// Pump readiness: true only when a real `TouchSource` plus a draining
    /// pump loop plus a created uinput node exist. Until the transport task
    /// wires those, `attach` degrades to passthrough — grabbing without an
    /// emit path would swallow the physical device with nothing replacing it.
    pump_ready: bool,
    held_first: Vec<(u16, u16)>,
    held_force: Vec<(u16, u16)>,
    last_pos: HashMap<u8, (i32, i32)>,
    /// Highest slot index written so far; vacated slots are terminated.
    max_slot: usize,
    /// Last `BTN_LEFT` value on the wire. `BTN_LEFT` is edge-triggered:
    /// it emits only on state change, so non-left clicks carry zero
    /// `BTN_LEFT` traffic and steady holds stay silent. Pure wire
    /// hygiene; no claim about any downstream symptom.
    btn_left_down: bool,
}

impl<G, U, H, S> ClickProxy<G, U, H, S>
where
    G: GrabDevice,
    U: UinputSink,
    H: HapticSender,
    S: TouchSource,
{
    pub fn new(settings: ProxySettings, grab: G, uinput: U, haptic: H, source: S) -> Self {
        Self {
            settings,
            grab,
            uinput,
            haptic,
            source,
            machine: ClickMachine::new(),
            attached: false,
            passthrough: true,
            pump_ready: false,
            held_first: Vec::new(),
            held_force: Vec::new(),
            last_pos: HashMap::new(),
            max_slot: 0,
            btn_left_down: false,
        }
    }

    pub fn is_attached(&self) -> bool {
        self.attached
    }

    /// True when events flow untouched (gate off, pump unready, grab failed).
    pub fn is_passthrough(&self) -> bool {
        self.passthrough
    }

    /// Mark the pump path ready/unready (transport-task wiring). Clearing
    /// while attached releases the grab so the device is never swallowed
    /// without an emit path.
    pub fn set_pump_ready(&mut self, ready: bool) {
        self.pump_ready = ready;
        if !ready && self.attached {
            log::info!("proxy: pump lost, releasing grab (passthrough)");
            self.grab.ungrab();
            self.release_held(false);
            self.uinput.sync();
            self.machine.reset();
            self.last_pos.clear();
            self.attached = false;
            self.passthrough = true;
        }
    }

    /// Attach on `Ready`: grab the physical node only when the gate is on
    /// AND the pump is ready. Anything else → log + passthrough, never `Err`
    /// (clicks must not fail init, and must never swallow input it cannot
    /// re-emit).
    pub fn attach(&mut self, event: &DeviceReadyEvent) {
        if event.kind != ReadyKind::Ready {
            return;
        }
        if !self.settings.enhanced {
            log::debug!("proxy: gate off, passthrough for {}", event.devnode);
            self.attached = false;
            self.passthrough = true;
            return;
        }
        if !self.pump_ready {
            log::info!(
                "proxy: pump not ready for {} (no TouchSource/loop/uinput yet); passthrough",
                event.devnode
            );
            self.attached = false;
            self.passthrough = true;
            return;
        }
        match self.grab.grab() {
            Ok(()) => {
                log::info!("proxy: grabbed {} (uinput virtual node up)", event.devnode);
                self.attached = true;
                self.passthrough = false;
            }
            Err(e) => {
                log::warn!(
                    "proxy: grab failed on {}: {e:#}; passthrough continues",
                    event.devnode
                );
                self.attached = false;
                self.passthrough = true;
            }
        }
    }

    /// Detach on `Detached`: release grab, drop latch + held keys, trailing
    /// `SYN` so no key-up is lost (stuck keys otherwise).
    pub fn detach(&mut self, event: &DeviceReadyEvent) {
        if event.kind != ReadyKind::Detached {
            return;
        }
        if self.attached {
            log::info!("proxy: releasing grab ({})", event.devnode);
            self.grab.ungrab();
        }
        self.release_held(false);
        self.uinput.sync();
        self.machine.reset();
        self.last_pos.clear();
        self.max_slot = 0;
        self.attached = false;
        self.passthrough = true;
    }

    /// One poll step: `poll_frame` errors are logged + dropped (`Ok(None)`),
    /// never propagated (the read loop must not abort on a bad frame).
    /// Gate-off, pump-unready, or passthrough short-circuits without touching
    /// hardware. Exactly one `SYN` is emitted per handled frame.
    pub fn handle_frame(&mut self, cfg: &Config) -> Result<Option<ClickEvent>> {
        if !self.settings.enhanced || !self.attached || self.passthrough {
            return Ok(None);
        }
        let frame = match self.source.poll_frame() {
            Ok(Some(f)) => f,
            Ok(None) => return Ok(None),
            Err(e) => {
                log::warn!("proxy: poll_frame failed, dropping frame: {e:#}");
                return Ok(None);
            }
        };
        let was_first = self.machine.active().is_some();
        let was_force = self.machine.force_active().is_some();
        let edges = match self.machine.update(&frame.fingers, frame.clicks, cfg) {
            Ok(e) => e,
            Err(e) => {
                log::warn!("proxy: click/force parse failed, dropping edge: {e:#}");
                return Ok(None);
            }
        };
        // Touch slots first (post-update latch is visible to buttons below).
        self.emit_touch_frame(&frame, cfg.speed);
        // Press order: first then force; reporters see force (supersedes,
        // unless force is `none` — see `FrameOutcome::primary`).
        if let Some(ref ev) = edges.first {
            self.press_level(ev, false);
        }
        if let Some(ref ev) = edges.force {
            self.press_level(ev, true);
        }
        if edges.first.is_some() || edges.force.is_some() {
            // Edge frames reconcile `BTN_LEFT` through the wire tracker
            // (emits only on change — `press_level` already put fresh
            // downs on the wire). Exactly one SYN per handled frame
            // either way: `press_level` never syncs itself.
            self.emit_btn_left(frame.clicks);
            self.uinput.sync();
            return Ok(edges.primary());
        }
        let first_gone = was_first && self.machine.active().is_none();
        let force_gone = was_force && self.machine.force_active().is_none();
        if first_gone {
            // Full release clears both levels. The physical bit rides
            // along so a still-held hardware button suppresses the
            // latch's left-up (no 0-then-1 flutter in one frame).
            self.release_held(ClickMachine::physical_button(frame.clicks));
        } else if force_gone {
            // Back to the first-level hold: force keys up, first keys stay.
            self.release_force_held();
        }
        self.emit_btn_left(frame.clicks);
        self.uinput.sync();
        Ok(None)
    }

    /// `BTN_LEFT` from held codes only (either latched action contains `left`)
    /// or the physical button bit — never from latch state alone, so a
    /// right/middle/combo/empty hold cannot chord a phantom left. Edge
    /// triggered: emits only on state change, so frames where left is and
    /// was down stay silent (no `BTN_LEFT=0` releases for buttons never
    /// pressed, no duplicate re-asserts).
    fn emit_btn_left(&mut self, clicks: u8) {
        let held_left = self
            .held_first
            .iter()
            .chain(self.held_force.iter())
            .any(|&(ty, code)| ty == EV_KEY && code == BTN_LEFT);
        let down = held_left || ClickMachine::physical_button(clicks);
        if down != self.btn_left_down {
            self.uinput.emit(EV_KEY, BTN_LEFT, i32::from(down));
            self.btn_left_down = down;
        }
    }

    /// True when `(ty, code)` is already down on the wire (held by either
    /// level). Shared codes (e.g. `ctrl` in `ctrl+c` + `ctrl+v`, or `left`
    /// in both levels) go down once and stay down until every holding level
    /// releases them.
    fn is_down(&self, pair: (u16, u16)) -> bool {
        self.held_first.contains(&pair) || self.held_force.contains(&pair)
    }

    /// Press one level's edge: keys down (deduplicated across levels, see
    /// [`ClickProxy::is_down`]) plus that level's own haptic pulse. A
    /// one-frame jump through both levels therefore emits both pulses in
    /// press order (first then force) — identical to staged arrival. Never
    /// syncs: the caller emits buttons + a single `SYN` per frame.
    fn press_level(&mut self, ev: &ClickEvent, force: bool) {
        use crate::config::ActionKind;
        let name = if force { "force" } else { "click" };
        match &ev.action.kind {
            ActionKind::None => {
                log::debug!("proxy: {name}_{} = none (swallowed)", ev.finger_count);
            }
            ActionKind::Empty => {
                self.fire_haptic(ev);
            }
            ActionKind::Keys(keys) => {
                for k in keys {
                    match key_token_to_code(k) {
                        Some((ty, code)) => {
                            let pair = (ty, code);
                            // Skip the re-emit when the other level already
                            // holds the code down, but still record it in
                            // this level's set so release accounting stays
                            // per-level. `BTN_LEFT` goes through the wire
                            // tracker instead: it is already down when the
                            // physical bit holds it, and a duplicate down is
                            // redundant traffic the tracker exists to remove.
                            if pair == (EV_KEY, BTN_LEFT) {
                                if !self.btn_left_down {
                                    self.uinput.emit(ty, code, 1);
                                    self.btn_left_down = true;
                                }
                            } else if !self.is_down(pair) {
                                self.uinput.emit(ty, code, 1);
                            }
                            if force {
                                self.held_force.push(pair);
                            } else {
                                self.held_first.push(pair);
                            }
                        }
                        None => log::warn!("proxy: no keycode for '{k}', skipped"),
                    }
                }
                self.fire_haptic(ev);
            }
        }
    }

    fn fire_haptic(&self, ev: &ClickEvent) {
        if !clicks::should_emit_haptic(ev.haptic) {
            return;
        }
        let strength = ev.haptic.min(255) as u8;
        if let Err(e) = self.haptic.click_feedback(strength, ev.quiet) {
            log::warn!("proxy: haptic send failed: {e:#}");
        }
    }

    /// Full release: every latched code goes up on the wire, except
    /// `BTN_LEFT` while the physical button still holds it (`phys_down`) —
    /// emitting 0-then-1 in the same frame would flutter a phantom extra
    /// click. The caller reconciles via [`ClickProxy::emit_btn_left`]
    /// right after, so the wire stays at exactly one down/up pair.
    /// Detach/pump-loss pass `false` (no frame in hand): every up is
    /// forced there so no key-up is ever lost.
    fn release_held(&mut self, phys_down: bool) {
        let first = std::mem::take(&mut self.held_first);
        let mut left_up = false;
        for &(ty, code) in &first {
            if phys_down && (ty, code) == (EV_KEY, BTN_LEFT) {
                continue; // physical still holds it; wire stays down
            }
            self.uinput.emit(ty, code, 0);
            if (ty, code) == (EV_KEY, BTN_LEFT) {
                left_up = true;
            }
        }
        for (ty, code) in self.held_force.drain(..) {
            if first.contains(&(ty, code)) {
                continue; // already released above; one up per code
            }
            if phys_down && (ty, code) == (EV_KEY, BTN_LEFT) {
                continue; // physical still holds it; wire stays down
            }
            self.uinput.emit(ty, code, 0);
            if (ty, code) == (EV_KEY, BTN_LEFT) {
                left_up = true;
            }
        }
        // Full release: every latched code is up on the wire. If `BTN_LEFT`
        // was wire-down from the physical bit alone (no latch held it, e.g.
        // a bare hardware click), no loop above emitted its key-up — do it
        // here so no consumer ever sees a stuck left (a missing key-up
        // would auto-repeat downstream). Skipped while physical still
        // holds: the wire correctly stays down.
        if self.btn_left_down && !left_up && !phys_down {
            self.uinput.emit(EV_KEY, BTN_LEFT, 0);
        }
        self.btn_left_down = phys_down && self.btn_left_down;
    }

    /// Release force-held keys, retaining codes the first level still holds
    /// (no up-emit for those — the wire stays down until full release).
    fn release_force_held(&mut self) {
        let mut dropped_left = false;
        for (ty, code) in self.held_force.drain(..) {
            if self.held_first.contains(&(ty, code)) {
                continue;
            }
            self.uinput.emit(ty, code, 0);
            if (ty, code) == (EV_KEY, BTN_LEFT) {
                dropped_left = true;
            }
        }
        // The caller reconciles a still-held physical bit via
        // `emit_btn_left` right after; only a force-only left actually
        // went up on the wire here.
        if dropped_left {
            self.btn_left_down = false;
        }
    }

    /// Emit Type-B MT slots for one frame (`res == 0` arbitrary units).
    /// No trailing `SYN`: the caller syncs once per frame after keys.
    ///
    /// Slot-map semantics (transport-task contract): slots are POSITIONAL —
    /// slot N is the Nth contact in the frame, reassigned every frame, and
    /// carries no identity across frames. Identity lives in `TRACKING_ID`
    /// (the device id); libinput pairs slot↔id per `SYN`. A slot with no
    /// contact this frame gets `TRACKING_ID = -1`, including slots that
    /// existed only in earlier frames (`max_slot` termination) — without
    /// that, a 2→1 finger transition leaves a ghost contact alive downstream.
    fn emit_touch_frame(&mut self, frame: &TouchFrame, speed: u32) {
        use std::collections::HashSet;
        let any_touch = frame.fingers.iter().any(|f| f.contact);
        let n = frame.fingers.len().min(16);
        for (slot, f) in frame.fingers.iter().take(16).enumerate() {
            self.uinput.emit(EV_ABS, ABS_MT_SLOT, slot as i32);
            if f.contact {
                self.uinput
                    .emit(EV_ABS, ABS_MT_TRACKING_ID, i32::from(f.id));
                let (sx, sy) = self.scaled_pos(f.id, f.x, f.y, speed);
                self.uinput.emit(EV_ABS, ABS_MT_POSITION_X, sx);
                self.uinput.emit(EV_ABS, ABS_MT_POSITION_Y, sy);
                self.uinput.emit(
                    EV_ABS,
                    ABS_MT_PRESSURE,
                    i32::from(clicks::clamp_pressure(f.pressure)),
                );
                self.uinput.emit(
                    EV_ABS,
                    ABS_MT_ORIENTATION,
                    i32::from(clicks::negate_orientation(f.orientation)),
                );
            } else {
                self.uinput.emit(EV_ABS, ABS_MT_TRACKING_ID, -1);
                self.last_pos.remove(&f.id);
            }
        }
        // Terminate slots that existed in earlier frames but not this one.
        if n <= self.max_slot {
            for slot in n..=self.max_slot {
                self.uinput.emit(EV_ABS, ABS_MT_SLOT, slot as i32);
                self.uinput.emit(EV_ABS, ABS_MT_TRACKING_ID, -1);
            }
        }
        self.max_slot = n.saturating_sub(1);
        // Drop stale motion anchors: ids absent from this frame must not
        // anchor a future delta (stale-anchor jump on id reuse). Evict on
        // every frame, not only the empty one.
        let present: HashSet<u8> = frame.fingers.iter().map(|f| f.id).collect();
        if frame.fingers.is_empty() {
            self.last_pos.clear();
        } else {
            self.last_pos.retain(|id, _| present.contains(id));
        }
        self.uinput.emit(
            EV_KEY,
            BTN_TOUCH,
            i32::from(any_touch || ClickMachine::physical_button(frame.clicks)),
        );
    }

    /// Speed-scaled position: `prev + (cur - prev) * speed/100` per finger.
    /// `speed` comes from the per-frame `&Config` (single source), never a
    /// frozen snapshot.
    fn scaled_pos(&mut self, id: u8, x: i32, y: i32, speed: u32) -> (i32, i32) {
        match self.last_pos.get(&id).copied() {
            None => {
                self.last_pos.insert(id, (x, y));
                (x, y)
            }
            Some((px, py)) => {
                let nx = px + clicks::scale_delta(x - px, speed);
                let ny = py + clicks::scale_delta(y - py, speed);
                self.last_pos.insert(id, (x, y));
                (nx, ny)
            }
        }
    }
}

/// Subscribe a shared proxy to the hub. Callbacks do bounded attach/detach
/// only, so init is never blocked.
pub fn subscribe_to_hub<G, U, H, S>(
    hub: &DeviceReadyHub,
    proxy: Arc<Mutex<ClickProxy<G, U, H, S>>>,
) -> usize
where
    G: GrabDevice + Send + 'static,
    U: UinputSink + Send + 'static,
    H: HapticSender + Send + 'static,
    S: TouchSource + Send + 'static,
{
    let p = Arc::clone(&proxy);
    hub.subscribe(move |event: &DeviceReadyEvent| {
        let mut px = p.lock().unwrap_or_else(|e| e.into_inner());
        match event.kind {
            ReadyKind::Ready => px.attach(event),
            ReadyKind::Detached => px.detach(event),
        }
    })
}

// ---- minimal real backends (best-effort, fail open to passthrough) ----

use std::os::unix::io::AsRawFd;

/// `EVIOCGRAB` grabber for a physical evdev node. Opens lazily on `grab()`.
pub struct RealGrab {
    node: Mutex<String>,
    fd: Mutex<Option<std::fs::File>>,
    grabbed: std::sync::atomic::AtomicBool,
}

impl RealGrab {
    pub fn new(node: &str) -> Self {
        Self {
            node: Mutex::new(node.to_string()),
            fd: Mutex::new(None),
            grabbed: std::sync::atomic::AtomicBool::new(false),
        }
    }

    /// Point at a (re-)resolved node. A live grab keeps its fd: re-grab
    /// while grabbed is a no-op success, never a close+reopen (fd corrupt).
    pub fn set_node(&self, node: &str) {
        *self.node.lock().unwrap_or_else(|e| e.into_inner()) = node.to_string();
    }
}

impl GrabDevice for RealGrab {
    fn grab(&self) -> Result<()> {
        if self.is_grabbed() {
            log::debug!("proxy: already grabbed, keeping fd");
            return Ok(());
        }
        let node = self.node.lock().unwrap_or_else(|e| e.into_inner()).clone();
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&node)
            .map_err(|e| anyhow::anyhow!("opening evdev '{node}': {e}"))?;
        let ret = unsafe { libc::ioctl(file.as_raw_fd(), eviocgrab_request() as libc::c_ulong, 1) };
        if ret < 0 {
            return Err(anyhow::anyhow!(
                "EVIOCGRAB on '{node}': {}",
                std::io::Error::last_os_error()
            ));
        }
        *self.fd.lock().unwrap_or_else(|e| e.into_inner()) = Some(file);
        self.grabbed
            .store(true, std::sync::atomic::Ordering::SeqCst);
        Ok(())
    }

    fn ungrab(&self) {
        let mut guard = self.fd.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(f) = guard.take() {
            unsafe {
                libc::ioctl(f.as_raw_fd(), eviocgrab_request() as libc::c_ulong, 0);
            }
        }
        self.grabbed
            .store(false, std::sync::atomic::Ordering::SeqCst);
    }

    fn is_grabbed(&self) -> bool {
        self.grabbed.load(std::sync::atomic::Ordering::SeqCst)
    }
}

/// Grabber that always refuses: used when no evdev node is configured.
/// The physical node is never guessed (`event0` roulette could swallow a
/// keyboard), so `attach` degrades to passthrough with a log instead.
#[derive(Debug, Default)]
pub struct RefusingGrab;

impl GrabDevice for RefusingGrab {
    fn grab(&self) -> Result<()> {
        anyhow::bail!(
            "no evdev node configured (LTD_EVDEV_NODE unset); refusing to guess — passthrough"
        )
    }
    fn ungrab(&self) {}
    fn is_grabbed(&self) -> bool {
        false
    }
}

/// Daemon grabber: real node when resolved, refusing otherwise. Single type
/// so the daemon hookup stays monomorphic.
pub enum DaemonGrab {
    Real(RealGrab),
    Refusing(RefusingGrab),
}

impl GrabDevice for DaemonGrab {
    fn grab(&self) -> Result<()> {
        match self {
            DaemonGrab::Real(g) => g.grab(),
            DaemonGrab::Refusing(g) => g.grab(),
        }
    }
    fn ungrab(&self) {
        match self {
            DaemonGrab::Real(g) => g.ungrab(),
            DaemonGrab::Refusing(g) => g.ungrab(),
        }
    }
    fn is_grabbed(&self) -> bool {
        match self {
            DaemonGrab::Real(g) => g.is_grabbed(),
            DaemonGrab::Refusing(g) => g.is_grabbed(),
        }
    }
}

/// Best-effort uinput sink. Opens `/dev/uinput` lazily; full device setup
/// (`UI_DEV_SETUP`, `UI_ABS_SETUP`, `UI_DEV_CREATE`) lands with hardware
/// verification. Until then emits are logged and best-effort written as
/// `input_event` structs when the fd is open. Never sets `EV_FF`
/// (see [`uinput_evbits`]).
pub struct RealUinput {
    node: String,
    fd: Mutex<Option<std::fs::File>>,
}

impl RealUinput {
    pub fn new(node: &str) -> Self {
        Self {
            node: node.to_string(),
            fd: Mutex::new(None),
        }
    }

    pub fn ensure_open(&self) {
        let mut guard = self.fd.lock().unwrap_or_else(|e| e.into_inner());
        if guard.is_some() {
            return;
        }
        match std::fs::OpenOptions::new().write(true).open(&self.node) {
            Ok(f) => {
                log::info!(
                    "proxy: uinput node {} open (setup verified on hardware)",
                    self.node
                );
                *guard = Some(f);
            }
            Err(e) => log::warn!(
                "proxy: uinput open {} failed: {e}; events logged only",
                self.node
            ),
        }
    }
}

impl UinputSink for RealUinput {
    fn emit(&self, ev_type: u16, code: u16, value: i32) {
        log::debug!("uinput emit: type={ev_type} code={code} value={value}");
        let guard = self.fd.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(f) = guard.as_ref() {
            use std::os::unix::io::AsRawFd;
            #[repr(C)]
            struct InputEvent {
                sec: libc::time_t,
                usec: libc::suseconds_t,
                ty: u16,
                code: u16,
                value: i32,
            }
            let ev = InputEvent {
                sec: 0,
                usec: 0,
                ty: ev_type,
                code,
                value,
            };
            let bytes: &[u8] = unsafe {
                std::slice::from_raw_parts(
                    (&raw const ev).cast::<u8>(),
                    std::mem::size_of::<InputEvent>(),
                )
            };
            unsafe {
                let ret = libc::write(f.as_raw_fd(), bytes.as_ptr().cast(), bytes.len());
                if ret < 0 {
                    log::warn!(
                        "proxy: uinput write failed (type={ev_type} code={code}): {}",
                        std::io::Error::last_os_error()
                    );
                }
            }
        }
    }
}

/// Host-click haptic sender: re-sends `0x21 0x01` (init-owned report) as
/// feedback; strength/quiet carried for logging until the proprietary `0x53`
/// actuator mapping is verified on hardware. `haptic == 0` never sends.
pub struct RealHaptic {
    node: Mutex<String>,
}

impl RealHaptic {
    pub fn new() -> Self {
        Self {
            node: Mutex::new(String::new()),
        }
    }

    pub fn set_node(&self, node: &str) {
        *self.node.lock().unwrap_or_else(|e| e.into_inner()) = node.to_string();
    }
}

impl Default for RealHaptic {
    fn default() -> Self {
        Self::new()
    }
}

impl HapticSender for RealHaptic {
    fn click_feedback(&self, strength: u8, quiet: bool) -> Result<()> {
        if strength == 0 {
            return Ok(());
        }
        let node = self.node.lock().unwrap_or_else(|e| e.into_inner()).clone();
        log::info!(
            "haptic feedback: strength={strength} quiet={quiet} (report 21 01, node={node})"
        );
        if node.is_empty() {
            return Ok(());
        }
        crate::init::send_feature_report_hidraw(&node, crate::init::HOST_CLICK_USB_ON)?;
        Ok(())
    }
}

/// Idle touch source for the daemon hookup: hidraw transport lands with the
/// transport task, so the daemon subscribes a proxy that polls to no-op
/// until then. Reports the same tier/units contract as the real source.
#[derive(Debug, Default)]
pub struct NullSource;

impl TouchSource for NullSource {
    fn tier(&self) -> crate::apple_parse::CapabilityTier {
        crate::apple_parse::CapabilityTier::L2p
    }
    fn pressure_units(&self) -> crate::apple_parse::PressureUnits {
        crate::apple_parse::PressureUnits::Arbitrary
    }
    fn poll_frame(&mut self) -> Result<Option<TouchFrame>> {
        Ok(None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::apple_parse::{CapabilityTier, FingerContact, PressureUnits, TouchFrame};
    use crate::config::Config;
    use crate::init::{detached, usb_ready, Transport};
    use std::cell::RefCell;

    struct FakeGrab {
        fail: bool,
        grabbed: Mutex<bool>,
    }

    impl FakeGrab {
        fn ok() -> Self {
            Self {
                fail: false,
                grabbed: Mutex::new(false),
            }
        }
        fn failing() -> Self {
            Self {
                fail: true,
                grabbed: Mutex::new(false),
            }
        }
    }

    impl GrabDevice for FakeGrab {
        fn grab(&self) -> Result<()> {
            if self.fail {
                anyhow::bail!("fake grab: EACCES");
            }
            *self.grabbed.lock().unwrap() = true;
            Ok(())
        }
        fn ungrab(&self) {
            *self.grabbed.lock().unwrap() = false;
        }
        fn is_grabbed(&self) -> bool {
            *self.grabbed.lock().unwrap()
        }
    }

    /// Shared grab handle so tests assert on the same object the proxy owns
    /// (a detached handle would make grab assertions vacuous).
    struct SharedGrab(Arc<FakeGrab>);

    impl GrabDevice for SharedGrab {
        fn grab(&self) -> Result<()> {
            self.0.grab()
        }
        fn ungrab(&self) {
            self.0.ungrab()
        }
        fn is_grabbed(&self) -> bool {
            self.0.is_grabbed()
        }
    }

    struct FakeUinput {
        events: Mutex<Vec<(u16, u16, i32)>>,
    }

    impl FakeUinput {
        fn new() -> Self {
            Self {
                events: Mutex::new(Vec::new()),
            }
        }
        fn has(&self, ty: u16, code: u16, val: i32) -> bool {
            self.events.lock().unwrap().contains(&(ty, code, val))
        }
    }

    impl UinputSink for FakeUinput {
        fn emit(&self, ev_type: u16, code: u16, value: i32) {
            self.events.lock().unwrap().push((ev_type, code, value));
        }
    }

    struct FakeHaptic {
        calls: Mutex<Vec<(u8, bool)>>,
    }

    impl FakeHaptic {
        fn new() -> Self {
            Self {
                calls: Mutex::new(Vec::new()),
            }
        }
    }

    impl HapticSender for FakeHaptic {
        fn click_feedback(&self, strength: u8, quiet: bool) -> Result<()> {
            self.calls.lock().unwrap().push((strength, quiet));
            Ok(())
        }
    }

    struct ScriptSource {
        frames: RefCell<Vec<Result<Option<TouchFrame>>>>,
    }

    impl ScriptSource {
        fn new(frames: Vec<Result<Option<TouchFrame>>>) -> Self {
            Self {
                frames: RefCell::new(frames),
            }
        }
    }

    impl TouchSource for ScriptSource {
        fn tier(&self) -> CapabilityTier {
            CapabilityTier::L2p
        }
        fn pressure_units(&self) -> PressureUnits {
            PressureUnits::Arbitrary
        }
        fn poll_frame(&mut self) -> Result<Option<TouchFrame>> {
            let mut f = self.frames.borrow_mut();
            if f.is_empty() {
                return Ok(None);
            }
            f.remove(0)
        }
    }

    fn finger(id: u8, pressure: u8, x: i32, y: i32) -> FingerContact {
        FingerContact {
            id,
            x,
            y,
            size: 10,
            orientation: 2,
            touch_major: 8,
            touch_minor: 6,
            pressure,
            contact: true,
        }
    }

    fn frame(fingers: Vec<FingerContact>, clicks: u8) -> TouchFrame {
        TouchFrame {
            fingers,
            clicks,
            timestamp_ms: 0,
        }
    }

    fn settings(enhanced: bool) -> ProxySettings {
        let cfg = Config {
            enhanced_clicks: enhanced,
            ..Config::default()
        };
        ProxySettings::from(&cfg)
    }

    #[test]
    fn gate_off_never_grabs_passthrough() {
        let grab = Arc::new(FakeGrab::ok());
        let mut px = ClickProxy::new(
            settings(false),
            SharedGrab(Arc::clone(&grab)),
            FakeUinput::new(),
            FakeHaptic::new(),
            ScriptSource::new(vec![]),
        );
        px.set_pump_ready(true);
        px.attach(&usb_ready("/dev/hidraw1"));
        assert!(!px.is_attached());
        assert!(px.is_passthrough());
        assert!(!grab.is_grabbed());
        // handle_frame short-circuits even with a failing source.
        let cfg = Config::default();
        assert!(px.handle_frame(&cfg).unwrap().is_none());
    }

    #[test]
    fn pump_unready_never_grabs() {
        // Gate on but no pump loop/uinput yet → passthrough, device untouched.
        let grab = Arc::new(FakeGrab::ok());
        let mut px = ClickProxy::new(
            settings(true),
            SharedGrab(Arc::clone(&grab)),
            FakeUinput::new(),
            FakeHaptic::new(),
            ScriptSource::new(vec![]),
        );
        assert!(!px.is_attached());
        px.attach(&usb_ready("/dev/hidraw1"));
        assert!(!px.is_attached());
        assert!(px.is_passthrough());
        assert!(!grab.is_grabbed());
    }

    #[test]
    fn pump_loss_releases_grab() {
        let grab = Arc::new(FakeGrab::ok());
        let mut px = ClickProxy::new(
            settings(true),
            SharedGrab(Arc::clone(&grab)),
            FakeUinput::new(),
            FakeHaptic::new(),
            ScriptSource::new(vec![]),
        );
        px.set_pump_ready(true);
        px.attach(&usb_ready("/dev/hidraw1"));
        assert!(px.is_attached());
        assert!(grab.is_grabbed());
        px.set_pump_ready(false);
        assert!(!px.is_attached());
        assert!(px.is_passthrough());
        assert!(!grab.is_grabbed());
    }

    #[test]
    fn attach_detach_hub_cycle() {
        let mut px = ClickProxy::new(
            settings(true),
            FakeGrab::ok(),
            FakeUinput::new(),
            FakeHaptic::new(),
            ScriptSource::new(vec![]),
        );
        px.set_pump_ready(true);
        px.attach(&usb_ready("/dev/hidraw1"));
        assert!(px.is_attached());
        assert!(!px.is_passthrough());
        px.detach(&detached("/dev/hidraw1", Transport::Usb));
        assert!(!px.is_attached());
        assert!(px.is_passthrough());
    }

    #[test]
    fn grab_failure_falls_back_to_passthrough() {
        let mut px = ClickProxy::new(
            settings(true),
            FakeGrab::failing(),
            FakeUinput::new(),
            FakeHaptic::new(),
            ScriptSource::new(vec![]),
        );
        px.set_pump_ready(true);
        px.attach(&usb_ready("/dev/hidraw1"));
        assert!(!px.is_attached());
        assert!(px.is_passthrough());
        // Clicks path short-circuits; init never sees an error.
        let cfg = Config::default();
        assert!(px.handle_frame(&cfg).unwrap().is_none());
    }

    #[test]
    fn poll_error_drops_frame_never_aborts() {
        let src = ScriptSource::new(vec![
            Err(anyhow::anyhow!("fake hidraw EIO")),
            Ok(Some(frame(vec![finger(1, 200, 0, 0)], 0))),
        ]);
        let mut px = ClickProxy::new(
            settings(true),
            FakeGrab::ok(),
            FakeUinput::new(),
            FakeHaptic::new(),
            src,
        );
        px.set_pump_ready(true);
        px.attach(&usb_ready("/dev/hidraw1"));
        let cfg = Config::default();
        assert!(px.handle_frame(&cfg).unwrap().is_none()); // dropped
        let ev = px.handle_frame(&cfg).unwrap().unwrap(); // next frame fires
        assert_eq!(ev.finger_count, 1);
    }

    struct SharedUinput(Arc<FakeUinput>);

    impl UinputSink for SharedUinput {
        fn emit(&self, ev_type: u16, code: u16, value: i32) {
            self.0.emit(ev_type, code, value);
        }
    }

    struct SharedHaptic(Arc<FakeHaptic>);

    impl HapticSender for SharedHaptic {
        fn click_feedback(&self, strength: u8, quiet: bool) -> Result<()> {
            self.0.click_feedback(strength, quiet)
        }
    }

    #[test]
    fn clamp_negation_and_buttons_on_wire() {
        assert_eq!(crate::apple_parse::PRESSURE_VALID_MAX, 253);
        let uinput = Arc::new(FakeUinput::new());
        let haptic = Arc::new(FakeHaptic::new());
        let src = ScriptSource::new(vec![Ok(Some(frame(vec![finger(5, 255, 10, 20)], 0)))]);
        let mut px = ClickProxy::new(
            settings(true),
            FakeGrab::ok(),
            SharedUinput(Arc::clone(&uinput)),
            SharedHaptic(Arc::clone(&haptic)),
            src,
        );
        px.set_pump_ready(true);
        px.attach(&usb_ready("/dev/hidraw1"));
        let cfg = Config::default();
        let ev = px.handle_frame(&cfg).unwrap().unwrap();
        assert_eq!(ev.finger_count, 1);
        // 255 clamps to 253; raw orientation 2 negates to -2.
        assert!(uinput.has(EV_ABS, ABS_MT_PRESSURE, 253));
        assert!(uinput.has(EV_ABS, ABS_MT_ORIENTATION, -2));
        assert!(uinput.has(EV_ABS, ABS_MT_TRACKING_ID, 5));
        assert!(uinput.has(EV_KEY, BTN_TOUCH, 1));
        // click_1 = left → BTN_LEFT down; haptic fired (default 5).
        assert!(uinput.has(EV_KEY, BTN_LEFT, 1));
        assert_eq!(haptic.calls.lock().unwrap().as_slice(), &[(5, false)]);
    }

    #[test]
    fn silent_haptic_sends_no_feedback() {
        let uinput = Arc::new(FakeUinput::new());
        let haptic = Arc::new(FakeHaptic::new());
        let src = ScriptSource::new(vec![Ok(Some(frame(vec![finger(1, 200, 0, 0)], 0)))]);
        let mut px = ClickProxy::new(
            settings(true),
            FakeGrab::ok(),
            SharedUinput(Arc::clone(&uinput)),
            SharedHaptic(Arc::clone(&haptic)),
            src,
        );
        px.set_pump_ready(true);
        px.attach(&usb_ready("/dev/hidraw1"));
        let cfg = Config {
            haptic: 0,
            ..Config::default()
        };
        px.handle_frame(&cfg).unwrap();
        assert!(haptic.calls.lock().unwrap().is_empty());
        // Button still emits; only the haptic is silent.
        assert!(uinput.has(EV_KEY, BTN_LEFT, 1));
    }

    #[test]
    fn none_action_no_haptic_no_extra_keys() {
        let uinput = Arc::new(FakeUinput::new());
        let haptic = Arc::new(FakeHaptic::new());
        let src = ScriptSource::new(vec![Ok(Some(frame(vec![finger(1, 200, 0, 0)], 0)))]);
        let mut px = ClickProxy::new(
            settings(true),
            FakeGrab::ok(),
            SharedUinput(Arc::clone(&uinput)),
            SharedHaptic(Arc::clone(&haptic)),
            src,
        );
        px.set_pump_ready(true);
        px.attach(&usb_ready("/dev/hidraw1"));
        let cfg = Config {
            click_1: "none".into(),
            ..Config::default()
        };
        let ev = px.handle_frame(&cfg).unwrap().unwrap();
        assert!(ev.is_none());
        assert!(haptic.calls.lock().unwrap().is_empty());
    }

    #[test]
    fn speed_scales_motion_deltas() {
        let uinput = Arc::new(FakeUinput::new());
        let haptic = Arc::new(FakeHaptic::new());
        let src = ScriptSource::new(vec![
            Ok(Some(frame(vec![finger(1, 10, 0, 0)], 0))),
            Ok(Some(frame(vec![finger(1, 10, 100, 0)], 0))),
        ]);
        let mut px = ClickProxy::new(
            settings(true),
            FakeGrab::ok(),
            SharedUinput(Arc::clone(&uinput)),
            SharedHaptic(Arc::clone(&haptic)),
            src,
        );
        px.set_pump_ready(true);
        px.attach(&usb_ready("/dev/hidraw1"));
        // Speed is a single source: the per-frame Config, not a snapshot.
        let cfg = Config {
            speed: 200,
            ..Config::default()
        };
        px.handle_frame(&cfg).unwrap();
        px.handle_frame(&cfg).unwrap();
        // Second frame: dx=100 @200% → emitted x=200.
        assert!(uinput.has(EV_ABS, ABS_MT_POSITION_X, 200));
    }

    #[test]
    fn no_ff_in_capabilities() {
        assert!(!uinput_evbits().contains(&EV_FF));
    }

    #[test]
    fn eviocgrab_request_pins_linux_layout() {
        // _IOW('E', 0x90, int): 1<<30 | sizeof(int)<<16 | 'E'<<8 | 0x90.
        assert_eq!(eviocgrab_request(), 0x4004_4590);
    }

    #[test]
    fn fn_keycodes_cover_all_twenty_four() {
        let table: &[(&str, u16)] = &[
            ("f1", 59),
            ("f2", 60),
            ("f3", 61),
            ("f4", 62),
            ("f5", 63),
            ("f6", 64),
            ("f7", 65),
            ("f8", 66),
            ("f9", 67),
            ("f10", 68),
            ("f11", 87),
            ("f12", 88),
            ("f13", 183),
            ("f14", 184),
            ("f15", 185),
            ("f16", 186),
            ("f17", 187),
            ("f18", 188),
            ("f19", 189),
            ("f20", 190),
            ("f21", 191),
            ("f22", 192),
            ("f23", 193),
            ("f24", 194),
        ];
        for (name, code) in table {
            assert_eq!(key_token_to_code(name), Some((EV_KEY, *code)), "{name}");
        }
        assert_eq!(key_token_to_code("f0"), None);
        assert_eq!(key_token_to_code("f25"), None);
    }

    #[test]
    fn all_mapped_keycodes_match_kernel_header() {
        // Full audit against linux/input-event-codes.h: every token in
        // `key_token_to_code` pinned here so a wrong-code regression fails.
        let table: &[(&str, u16)] = &[
            // Mouse buttons.
            ("left", BTN_LEFT),
            ("right", BTN_RIGHT),
            ("middle", BTN_MIDDLE),
            ("back", BTN_SIDE),
            ("forward", BTN_EXTRA),
            // Modifiers (left + right variants).
            ("ctrl", 29),
            ("control", 29),
            ("lctrl", 29),
            ("rctrl", 97),
            ("shift", 42),
            ("lshift", 42),
            ("rshift", 54),
            ("alt", 56),
            ("lalt", 56),
            ("ralt", 100),
            ("meta", 125),
            ("super", 125),
            ("cmd", 125),
            ("command", 125),
            ("win", 125),
            ("lmeta", 125),
            ("rmeta", 126),
            // Navigation / editing.
            ("esc", 1),
            ("escape", 1),
            ("enter", 28),
            ("return", 28),
            ("space", 57),
            ("tab", 15),
            ("backspace", 14),
            ("delete", 111),
            ("home", 102),
            ("end", 107),
            ("pageup", 104),
            ("pgup", 104),
            ("pagedown", 109),
            ("pgdn", 109),
            ("up", 103),
            ("arrow_up", 103),
            ("down", 108),
            ("arrow_down", 108),
            ("left_arrow", 105),
            ("right_arrow", 106),
            // Media.
            ("volup", 115),
            ("voldown", 114),
            ("mute", 113),
            ("play", 164),
            ("pause", 119),
            // Letters.
            ("a", 30),
            ("b", 48),
            ("c", 46),
            ("d", 32),
            ("e", 18),
            ("f", 33),
            ("g", 34),
            ("h", 35),
            ("i", 23),
            ("j", 36),
            ("k", 37),
            ("l", 38),
            ("m", 50),
            ("n", 49),
            ("o", 24),
            ("p", 25),
            ("q", 16),
            ("r", 19),
            ("s", 31),
            ("t", 20),
            ("u", 22),
            ("v", 47),
            ("w", 17),
            ("x", 45),
            ("y", 21),
            ("z", 44),
            // Digits.
            ("1", 2),
            ("2", 3),
            ("3", 4),
            ("4", 5),
            ("5", 6),
            ("6", 7),
            ("7", 8),
            ("8", 9),
            ("9", 10),
            ("0", 11),
            // Function row (F13..F24 = 183..=194).
            ("f1", 59),
            ("f2", 60),
            ("f3", 61),
            ("f4", 62),
            ("f5", 63),
            ("f6", 64),
            ("f7", 65),
            ("f8", 66),
            ("f9", 67),
            ("f10", 68),
            ("f11", 87),
            ("f12", 88),
            ("f13", 183),
            ("f14", 184),
            ("f15", 185),
            ("f16", 186),
            ("f17", 187),
            ("f18", 188),
            ("f19", 189),
            ("f20", 190),
            ("f21", 191),
            ("f22", 192),
            ("f23", 193),
            ("f24", 194),
        ];
        for (name, code) in table {
            assert_eq!(key_token_to_code(name), Some((EV_KEY, *code)), "{name}");
        }
    }

    #[test]
    fn right_modifiers_map_to_right_codes() {
        assert_eq!(key_token_to_code("rctrl"), Some((EV_KEY, 97)));
        assert_eq!(key_token_to_code("rshift"), Some((EV_KEY, 54)));
        assert_eq!(key_token_to_code("ralt"), Some((EV_KEY, 100)));
        assert_eq!(key_token_to_code("rmeta"), Some((EV_KEY, 126)));
        assert_eq!(key_token_to_code("lctrl"), Some((EV_KEY, 29)));
        assert_eq!(key_token_to_code("lshift"), Some((EV_KEY, 42)));
        assert_eq!(key_token_to_code("lalt"), Some((EV_KEY, 56)));
        assert_eq!(key_token_to_code("lmeta"), Some((EV_KEY, 125)));
    }

    fn drive_ready_proxy(
        uinput: &Arc<FakeUinput>,
        haptic: &Arc<FakeHaptic>,
        frames: Vec<Result<Option<TouchFrame>>>,
    ) -> ClickProxy<SharedGrab, SharedUinput, SharedHaptic, ScriptSource> {
        let mut px = ClickProxy::new(
            settings(true),
            SharedGrab(Arc::new(FakeGrab::ok())),
            SharedUinput(Arc::clone(uinput)),
            SharedHaptic(Arc::clone(haptic)),
            ScriptSource::new(frames),
        );
        px.set_pump_ready(true);
        px.attach(&usb_ready("/dev/hidraw1"));
        assert!(px.is_attached());
        px
    }

    fn key_downs(uinput: &FakeUinput) -> Vec<(u16, u16, i32)> {
        uinput
            .events
            .lock()
            .unwrap()
            .iter()
            .copied()
            .filter(|&(ty, _, v)| ty == EV_KEY && v == 1)
            .collect()
    }

    /// Ordered values for one `(EV_KEY, code)` on the wire (duplicates kept).
    fn key_values(uinput: &FakeUinput, code: u16) -> Vec<i32> {
        uinput
            .events
            .lock()
            .unwrap()
            .iter()
            .copied()
            .filter(|&(ty, c, _)| ty == EV_KEY && c == code)
            .map(|(_, _, v)| v)
            .collect()
    }

    /// Number of value transitions (e.g. `1` = exactly one down→up pair
    /// modulo idempotent same-value re-emits).
    fn transitions(vals: &[i32]) -> usize {
        vals.windows(2).filter(|w| w[0] != w[1]).count()
    }

    #[test]
    fn combo_hold_emits_no_btn_left() {
        // click_1 = ctrl+c must not chord a phantom BTN_LEFT — press frame
        // AND the 2nd hold frame (single-frame tests masked the hold path).
        let uinput = Arc::new(FakeUinput::new());
        let haptic = Arc::new(FakeHaptic::new());
        let frames = vec![
            Ok(Some(frame(vec![finger(1, 200, 0, 0)], 0))),
            Ok(Some(frame(vec![finger(1, 200, 5, 5)], 0))),
        ];
        let mut px = drive_ready_proxy(&uinput, &haptic, frames);
        let cfg = Config {
            click_1: "ctrl+c".into(),
            ..Config::default()
        };
        px.handle_frame(&cfg).unwrap();
        px.handle_frame(&cfg).unwrap();
        assert!(!uinput.has(EV_KEY, BTN_LEFT, 1));
        assert!(uinput.has(EV_KEY, 29, 1)); // ctrl down
        assert!(uinput.has(EV_KEY, 46, 1)); // c down
    }

    #[test]
    fn right_hold_emits_no_btn_left() {
        let uinput = Arc::new(FakeUinput::new());
        let haptic = Arc::new(FakeHaptic::new());
        let frames = vec![
            Ok(Some(frame(
                vec![finger(1, 200, 0, 0), finger(2, 200, 9, 9)],
                0,
            ))),
            Ok(Some(frame(
                vec![finger(1, 200, 1, 1), finger(2, 200, 9, 9)],
                0,
            ))),
        ];
        let mut px = drive_ready_proxy(&uinput, &haptic, frames);
        let cfg = Config {
            click_2: "right".into(),
            ..Config::default()
        };
        let ev = px.handle_frame(&cfg).unwrap().unwrap();
        assert_eq!(ev.finger_count, 2);
        px.handle_frame(&cfg).unwrap();
        assert!(!uinput.has(EV_KEY, BTN_LEFT, 1));
        assert!(uinput.has(EV_KEY, BTN_RIGHT, 1));
    }

    #[test]
    fn empty_hold_emits_no_key_downs() {
        let uinput = Arc::new(FakeUinput::new());
        let haptic = Arc::new(FakeHaptic::new());
        let frames = vec![
            Ok(Some(frame(vec![finger(1, 200, 0, 0)], 0))),
            Ok(Some(frame(vec![finger(1, 200, 5, 5)], 0))),
        ];
        let mut px = drive_ready_proxy(&uinput, &haptic, frames);
        let cfg = Config {
            click_1: "empty".into(),
            ..Config::default()
        };
        px.handle_frame(&cfg).unwrap();
        px.handle_frame(&cfg).unwrap();
        // Only BTN_TOUCH=1 may be down (once per frame); no buttons, no
        // keys, no BTN_LEFT.
        let downs = key_downs(&uinput);
        assert_eq!(downs.len(), 2, "{downs:?}");
        assert!(
            downs.iter().all(|&e| e == (EV_KEY, BTN_TOUCH, 1)),
            "{downs:?}"
        );
        assert!(!haptic.calls.lock().unwrap().is_empty());
    }

    #[test]
    fn ghost_slot_terminated_on_contact_drop() {
        // 2 contacts then 1: slot 1 must get TRACKING_ID=-1, or libinput
        // keeps a ghost contact alive.
        let uinput = Arc::new(FakeUinput::new());
        let haptic = Arc::new(FakeHaptic::new());
        let frames = vec![
            Ok(Some(frame(
                vec![finger(1, 10, 0, 0), finger(2, 10, 9, 9)],
                0,
            ))),
            Ok(Some(frame(vec![finger(1, 10, 0, 0)], 0))),
        ];
        let mut px = drive_ready_proxy(&uinput, &haptic, frames);
        let cfg = Config::default();
        px.handle_frame(&cfg).unwrap();
        px.handle_frame(&cfg).unwrap();
        let evs = uinput.events.lock().unwrap().clone();
        let term1 = evs
            .windows(2)
            .any(|w| w[0] == (EV_ABS, ABS_MT_SLOT, 1) && w[1] == (EV_ABS, ABS_MT_TRACKING_ID, -1));
        assert!(term1, "slot 1 never terminated: {evs:?}");
    }

    #[test]
    fn id_reuse_after_lift_has_no_jump() {
        // Lift id 7 at x=100, then a NEW id-7 contact at x=0: without
        // eviction the scaler would emit prev + delta (a jump).
        let uinput = Arc::new(FakeUinput::new());
        let haptic = Arc::new(FakeHaptic::new());
        let mut lift = finger(7, 0, 100, 0);
        lift.contact = false;
        let frames = vec![
            Ok(Some(frame(vec![finger(7, 10, 100, 0)], 0))),
            Ok(Some(frame(vec![lift], 0))),
            Ok(Some(frame(vec![finger(7, 10, 0, 0)], 0))),
        ];
        let mut px = drive_ready_proxy(&uinput, &haptic, frames);
        let cfg = Config::default();
        px.handle_frame(&cfg).unwrap();
        px.handle_frame(&cfg).unwrap();
        px.handle_frame(&cfg).unwrap();
        // Fresh anchor: emitted x == raw x on first sighting.
        let xs: Vec<i32> = uinput
            .events
            .lock()
            .unwrap()
            .iter()
            .copied()
            .filter(|&(t, c, _)| t == EV_ABS && c == ABS_MT_POSITION_X)
            .map(|(_, _, v)| v)
            .collect();
        assert_eq!(xs.first(), Some(&100));
        assert_eq!(xs.last(), Some(&0));
    }

    #[test]
    fn clicks_high_bits_are_timestamp_not_buttons() {
        // bits 6..7 are device-timestamp bits: 0xC0 must stay quiet, while
        // 0xC1 still shows the button.
        let uinput = Arc::new(FakeUinput::new());
        let haptic = Arc::new(FakeHaptic::new());
        let frames = vec![Ok(Some(frame(vec![], 0xC0))), Ok(Some(frame(vec![], 0xC1)))];
        let mut px = drive_ready_proxy(&uinput, &haptic, frames);
        let cfg = Config::default();
        px.handle_frame(&cfg).unwrap();
        px.handle_frame(&cfg).unwrap();
        let evs = uinput.events.lock().unwrap().clone();
        let left_ones = evs
            .iter()
            .filter(|&&(t, c, v)| t == EV_KEY && c == BTN_LEFT && v == 1)
            .count();
        assert_eq!(left_ones, 1, "exactly the 0xC1 frame: {evs:?}");
        let touch_ones = evs
            .iter()
            .filter(|&&(t, c, v)| t == EV_KEY && c == BTN_TOUCH && v == 1)
            .count();
        assert_eq!(touch_ones, 1, "exactly the 0xC1 frame: {evs:?}");
    }

    #[test]
    fn detach_releases_held_keys_with_sync() {
        let uinput = Arc::new(FakeUinput::new());
        let haptic = Arc::new(FakeHaptic::new());
        let frames = vec![Ok(Some(frame(vec![finger(1, 200, 0, 0)], 0)))];
        let mut px = drive_ready_proxy(&uinput, &haptic, frames);
        let cfg = Config::default();
        px.handle_frame(&cfg).unwrap();
        assert!(uinput.has(EV_KEY, BTN_LEFT, 1));
        px.detach(&detached("/dev/hidraw1", Transport::Usb));
        assert!(uinput.has(EV_KEY, BTN_LEFT, 0));
        // Trailing SYN flushes the key-up; without it keys stick.
        let evs = uinput.events.lock().unwrap().clone();
        let up_at = evs
            .iter()
            .position(|&e| e == (EV_KEY, BTN_LEFT, 0))
            .expect("key-up missing");
        assert!(
            evs[up_at + 1..].contains(&(EV_SYN, SYN_REPORT, 0)),
            "no SYN after key-up: {evs:?}"
        );
    }

    #[test]
    fn subscribe_attaches_on_ready() {
        let hub = DeviceReadyHub::new();
        let proxy = Arc::new(Mutex::new(ClickProxy::new(
            settings(true),
            FakeGrab::ok(),
            FakeUinput::new(),
            FakeHaptic::new(),
            ScriptSource::new(vec![]),
        )));
        subscribe_to_hub(&hub, Arc::clone(&proxy));
        // Pump unready → Ready must NOT grab (device stays usable).
        hub.emit(&usb_ready("/dev/hidraw1"));
        assert!(!proxy.lock().unwrap().is_attached());
        proxy.lock().unwrap().set_pump_ready(true);
        hub.emit(&usb_ready("/dev/hidraw1"));
        assert!(proxy.lock().unwrap().is_attached());
        hub.emit(&detached("/dev/hidraw1", Transport::Usb));
        assert!(!proxy.lock().unwrap().is_attached());
    }

    fn force_cfg() -> Config {
        Config {
            sensitivity: 64,
            margin: 5,
            force_offset: 20,
            force_1: "forward".into(),
            ..Config::default()
        }
    }

    #[test]
    fn force_disabled_no_behavior_change() {
        let uinput = Arc::new(FakeUinput::new());
        let haptic = Arc::new(FakeHaptic::new());
        let frames = vec![Ok(Some(frame(vec![finger(1, 250, 0, 0)], 0)))];
        let mut px = drive_ready_proxy(&uinput, &haptic, frames);
        let cfg = Config::default(); // force_offset = 0
        let ev = px.handle_frame(&cfg).unwrap().unwrap();
        assert!(!ev.is_force());
        assert_eq!(ev.finger_count, 1);
        // First-level keys only; no forward key from the force map.
        assert!(uinput.has(EV_KEY, BTN_LEFT, 1));
        assert!(!uinput.has(EV_KEY, BTN_EXTRA, 1));
    }

    #[test]
    fn force_edge_holds_both_key_sets() {
        let uinput = Arc::new(FakeUinput::new());
        let haptic = Arc::new(FakeHaptic::new());
        let frames = vec![
            Ok(Some(frame(vec![finger(1, 70, 0, 0)], 0))),
            Ok(Some(frame(vec![finger(1, 100, 0, 0)], 0))),
        ];
        let mut px = drive_ready_proxy(&uinput, &haptic, frames);
        let cfg = force_cfg();
        let first = px.handle_frame(&cfg).unwrap().unwrap();
        assert!(!first.is_force());
        let second = px.handle_frame(&cfg).unwrap().unwrap();
        assert!(second.is_force());
        assert_eq!(second.finger_count, 1);
        // Coexist: left (first) still down, forward (force) now down too.
        assert!(uinput.has(EV_KEY, BTN_LEFT, 1));
        assert!(uinput.has(EV_KEY, BTN_EXTRA, 1));
        // Force supersedes in reporting; haptic falls back to standard 5.
        assert_eq!(
            haptic.calls.lock().unwrap().as_slice(),
            &[(5, false), (5, false)]
        );
    }

    #[test]
    fn force_haptic_override_and_quiet_waveform() {
        let uinput = Arc::new(FakeUinput::new());
        let haptic = Arc::new(FakeHaptic::new());
        let frames = vec![
            Ok(Some(frame(vec![finger(1, 70, 0, 0)], 0))),
            Ok(Some(frame(vec![finger(1, 100, 0, 0)], 0))),
        ];
        let mut px = drive_ready_proxy(&uinput, &haptic, frames);
        let cfg = Config {
            force_haptic: Some(9),
            force_quiet_click: true,
            ..force_cfg()
        };
        px.handle_frame(&cfg).unwrap();
        let ev = px.handle_frame(&cfg).unwrap().unwrap();
        assert!(ev.is_force());
        assert_eq!(ev.haptic, 9);
        assert!(ev.quiet);
        assert_eq!(
            haptic.calls.lock().unwrap().as_slice(),
            &[(5, false), (9, true)]
        );
    }

    #[test]
    fn force_release_returns_to_first_hold_then_full_release() {
        let uinput = Arc::new(FakeUinput::new());
        let haptic = Arc::new(FakeHaptic::new());
        // 70 = first hold; 100 = past force press (84); 70 = below force
        // release (79) but above first release (59); 10 = below all.
        let frames = vec![
            Ok(Some(frame(vec![finger(1, 70, 0, 0)], 0))),
            Ok(Some(frame(vec![finger(1, 100, 0, 0)], 0))),
            Ok(Some(frame(vec![finger(1, 70, 0, 0)], 0))),
            Ok(Some(frame(vec![finger(1, 10, 0, 0)], 0))),
        ];
        let mut px = drive_ready_proxy(&uinput, &haptic, frames);
        let cfg = force_cfg();
        px.handle_frame(&cfg).unwrap();
        px.handle_frame(&cfg).unwrap();
        assert!(uinput.has(EV_KEY, BTN_EXTRA, 1));
        // Force release: forward up, left still down (BTN_LEFT re-asserted).
        px.handle_frame(&cfg).unwrap();
        assert!(uinput.has(EV_KEY, BTN_EXTRA, 0));
        assert!(uinput.has(EV_KEY, BTN_LEFT, 1));
        // Full release: left up.
        px.handle_frame(&cfg).unwrap();
        assert!(uinput.has(EV_KEY, BTN_LEFT, 0));
    }

    #[test]
    fn force_two_finger_map_selects_force_2() {
        let uinput = Arc::new(FakeUinput::new());
        let haptic = Arc::new(FakeHaptic::new());
        let frames = vec![
            Ok(Some(frame(vec![finger(1, 70, 0, 0)], 0))),
            Ok(Some(frame(
                vec![finger(1, 100, 0, 0), finger(2, 100, 9, 9)],
                0,
            ))),
        ];
        let mut px = drive_ready_proxy(&uinput, &haptic, frames);
        let cfg = Config {
            force_2: "right".into(),
            ..force_cfg()
        };
        px.handle_frame(&cfg).unwrap();
        let ev = px.handle_frame(&cfg).unwrap().unwrap();
        assert!(ev.is_force());
        assert_eq!(ev.finger_count, 2);
        assert!(uinput.has(EV_KEY, BTN_RIGHT, 1));
    }

    #[test]
    fn force_never_fires_past_u8_range() {
        let uinput = Arc::new(FakeUinput::new());
        let haptic = Arc::new(FakeHaptic::new());
        let frames = vec![Ok(Some(frame(vec![finger(1, 255, 0, 0)], 0)))];
        let mut px = drive_ready_proxy(&uinput, &haptic, frames);
        let cfg = Config {
            sensitivity: 255,
            margin: 5,
            force_offset: 255,
            force_1: "forward".into(),
            ..Config::default()
        };
        let ev = px.handle_frame(&cfg).unwrap().unwrap();
        assert!(!ev.is_force());
        assert!(uinput.has(EV_KEY, BTN_LEFT, 1));
        assert!(!uinput.has(EV_KEY, BTN_EXTRA, 1));
    }

    #[test]
    fn shared_modifier_survives_force_release() {
        // `ctrl` in both chords: force release must not drop it mid-hold.
        // Tail-asserted: the single ctrl up lands in the full-release frame.
        let uinput = Arc::new(FakeUinput::new());
        let haptic = Arc::new(FakeHaptic::new());
        let frames = vec![
            Ok(Some(frame(vec![finger(1, 70, 0, 0)], 0))),
            Ok(Some(frame(vec![finger(1, 100, 0, 0)], 0))),
            Ok(Some(frame(vec![finger(1, 70, 0, 0)], 0))),
            Ok(Some(frame(vec![finger(1, 10, 0, 0)], 0))),
        ];
        let mut px = drive_ready_proxy(&uinput, &haptic, frames);
        let cfg = Config {
            sensitivity: 64,
            margin: 5,
            force_offset: 20,
            click_1: "ctrl+c".into(),
            force_1: "ctrl+v".into(),
            ..Config::default()
        };
        px.handle_frame(&cfg).unwrap();
        px.handle_frame(&cfg).unwrap();
        px.handle_frame(&cfg).unwrap();
        let mark = uinput.events.lock().unwrap().len();
        px.handle_frame(&cfg).unwrap();
        let evs = uinput.events.lock().unwrap().clone();
        // ctrl (29): down once, up once, single transition — never 0 mid-hold.
        let ctrl: Vec<i32> = evs
            .iter()
            .copied()
            .filter(|&(t, c, _)| t == EV_KEY && c == 29)
            .map(|(_, _, v)| v)
            .collect();
        assert_eq!(ctrl.first(), Some(&1));
        assert_eq!(ctrl.last(), Some(&0));
        assert_eq!(transitions(&ctrl), 1, "ctrl dropped mid-hold: {ctrl:?}");
        let first_up = evs
            .iter()
            .position(|&e| e == (EV_KEY, 29, 0))
            .expect("ctrl up missing");
        assert!(
            first_up >= mark,
            "ctrl released before full release: {evs:?}"
        );
        // `v` (47) belongs to force only: exactly one down→up pair.
        let v: Vec<i32> = evs
            .iter()
            .copied()
            .filter(|&(t, c, _)| t == EV_KEY && c == 47)
            .map(|(_, _, v)| v)
            .collect();
        assert_eq!(v, vec![1, 0]);
    }

    #[test]
    fn shared_left_has_no_phantom_release() {
        // click_1 == force_1 == left: one down, one up, no 0 while held.
        let uinput = Arc::new(FakeUinput::new());
        let haptic = Arc::new(FakeHaptic::new());
        let frames = vec![
            Ok(Some(frame(vec![finger(1, 250, 0, 0)], 0))),
            Ok(Some(frame(vec![finger(1, 70, 0, 0)], 0))),
            Ok(Some(frame(vec![finger(1, 10, 0, 0)], 0))),
        ];
        let mut px = drive_ready_proxy(&uinput, &haptic, frames);
        let cfg = Config {
            sensitivity: 64,
            margin: 5,
            force_offset: 20,
            force_1: "left".into(),
            ..Config::default()
        };
        px.handle_frame(&cfg).unwrap();
        px.handle_frame(&cfg).unwrap();
        px.handle_frame(&cfg).unwrap();
        let left = key_values(&uinput, BTN_LEFT);
        assert_eq!(left.first(), Some(&1));
        assert_eq!(left.last(), Some(&0));
        assert_eq!(transitions(&left), 1, "phantom BTN_LEFT release: {left:?}");
    }

    #[test]
    fn same_frame_both_edges_hold_then_release() {
        // One-frame jump latches both: both key sets down, force reported;
        // dropping below all thresholds releases both.
        let uinput = Arc::new(FakeUinput::new());
        let haptic = Arc::new(FakeHaptic::new());
        let frames = vec![
            Ok(Some(frame(vec![finger(1, 250, 0, 0)], 0))),
            Ok(Some(frame(vec![finger(1, 10, 0, 0)], 0))),
        ];
        let mut px = drive_ready_proxy(&uinput, &haptic, frames);
        let cfg = force_cfg();
        let ev = px.handle_frame(&cfg).unwrap().unwrap();
        assert!(ev.is_force());
        assert!(uinput.has(EV_KEY, BTN_LEFT, 1));
        assert!(uinput.has(EV_KEY, BTN_EXTRA, 1));
        let mark = uinput.events.lock().unwrap().len();
        px.handle_frame(&cfg).unwrap();
        let tail = uinput.events.lock().unwrap()[mark..].to_vec();
        assert!(
            tail.contains(&(EV_KEY, BTN_LEFT, 0)),
            "left never released: {tail:?}"
        );
        assert!(
            tail.contains(&(EV_KEY, BTN_EXTRA, 0)),
            "forward never released: {tail:?}"
        );
    }

    #[test]
    fn force_silent_override_emits_no_force_haptic() {
        // force_haptic = Some(0): force edge silent, first-level pulse kept.
        let uinput = Arc::new(FakeUinput::new());
        let haptic = Arc::new(FakeHaptic::new());
        let frames = vec![
            Ok(Some(frame(vec![finger(1, 70, 0, 0)], 0))),
            Ok(Some(frame(vec![finger(1, 100, 0, 0)], 0))),
        ];
        let mut px = drive_ready_proxy(&uinput, &haptic, frames);
        let cfg = Config {
            force_haptic: Some(0),
            ..force_cfg()
        };
        px.handle_frame(&cfg).unwrap();
        let ev = px.handle_frame(&cfg).unwrap().unwrap();
        assert!(ev.is_force());
        assert_eq!(ev.haptic, 0);
        assert_eq!(haptic.calls.lock().unwrap().as_slice(), &[(5, false)]);
        // Keys still held on both levels despite the silent force edge.
        assert!(uinput.has(EV_KEY, BTN_LEFT, 1));
        assert!(uinput.has(EV_KEY, BTN_EXTRA, 1));
    }

    #[test]
    fn one_frame_jump_emits_both_haptic_pulses() {
        // Pins the single-vs-double decision: one frame through both levels
        // emits both pulses in press order (first then force), identical to
        // staged arrival.
        let uinput = Arc::new(FakeUinput::new());
        let haptic = Arc::new(FakeHaptic::new());
        let frames = vec![Ok(Some(frame(vec![finger(1, 250, 0, 0)], 0)))];
        let mut px = drive_ready_proxy(&uinput, &haptic, frames);
        let cfg = Config {
            force_haptic: Some(9),
            force_quiet_click: true,
            ..force_cfg()
        };
        let ev = px.handle_frame(&cfg).unwrap().unwrap();
        assert!(ev.is_force());
        assert_eq!(
            haptic.calls.lock().unwrap().as_slice(),
            &[(5, false), (9, true)]
        );
    }

    #[test]
    fn btn_left_reasserted_after_force_release() {
        // click left + force forward: the force-release frame tail still
        // carries BTN_LEFT=1 (first hold); the full-release tail ends at 0.
        let uinput = Arc::new(FakeUinput::new());
        let haptic = Arc::new(FakeHaptic::new());
        let frames = vec![
            Ok(Some(frame(vec![finger(1, 70, 0, 0)], 0))),
            Ok(Some(frame(vec![finger(1, 100, 0, 0)], 0))),
            Ok(Some(frame(vec![finger(1, 70, 0, 0)], 0))),
            Ok(Some(frame(vec![finger(1, 10, 0, 0)], 0))),
        ];
        let mut px = drive_ready_proxy(&uinput, &haptic, frames);
        let cfg = force_cfg();
        px.handle_frame(&cfg).unwrap();
        px.handle_frame(&cfg).unwrap();
        px.handle_frame(&cfg).unwrap();
        let left = key_values(&uinput, BTN_LEFT);
        assert_eq!(left.last(), Some(&1), "first hold lost: {left:?}");
        let mark = uinput.events.lock().unwrap().len();
        px.handle_frame(&cfg).unwrap();
        let tail: Vec<(u16, u16, i32)> = uinput.events.lock().unwrap()[mark..].to_vec();
        assert!(
            tail.contains(&(EV_KEY, BTN_LEFT, 0)),
            "left never released: {tail:?}"
        );
        assert_eq!(key_values(&uinput, BTN_LEFT).last(), Some(&0));
    }
}
