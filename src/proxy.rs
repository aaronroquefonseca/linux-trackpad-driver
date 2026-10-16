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
//! - The physical evdev node is auto-resolved per hidraw node
//!   ([`crate::evdev`]: `$LTD_EVDEV_NODE` override wins, else the sysfs
//!   parentage walk, else a name+MT-caps scan). The name scan is UNTRUSTED
//!   and never backs a grab; unresolvable → passthrough, never `event0`
//!   roulette.
//! - TWO virtual nodes (old-service parity, hardware-proven 2026-10-16):
//!   a single node advertising keys `1..=255` is classified KEYBOARD by
//!   the kernel (`Handlers=sysrq kbd eventN`) and libinput never drives a
//!   pointer from it. So the pointer node carries EXACTLY the 9
//!   touchpad key bits (`BTN_LEFT/RIGHT/MIDDLE/TOUCH/TOOL_FINGER..QUINTTAP`)
//!   with `SYN`/`ABS` plus `INPUT_PROP_POINTER` and the 8 Type-B MT axes,
//!   while a separate `Extras` node carries the keyboard click-map codes
//!   (`SYN`/`KEY` only, no `ABS`, no pointer prop). Touch frames,
//!   `BTN_LEFT`, and mouse buttons route to the pointer node; key-combo
//!   codes route to extras ([`DualSink`] owns the split).
//! - Pointer-node positions carry the old-service physical density
//!   (`ABS_MT_POSITION_X/Y` resolution 48/44); every down contact carries
//!   the old-service normalized values (`ABS_MT_PRESSURE` 60,
//!   `TOUCH_MAJOR/MINOR` 800) — raw bytes are arbitrary units the
//!   classifier drops (cursor stays dead on light slides otherwise).
//!   NEVER advertises `EV_FF`/`FF_HAPTIC` (no 2026
//!   libinput/Wayland consumer). The `BTN_TOOL_*` + `TOUCH_MAJOR/MINOR` +
//!   `POINTER` + nonzero-resolution set is what libinput needs to classify
//!   the node as a touchpad and move the cursor from slot motion
//!   (hardware-proven against the old driver; without them — or with
//!   keyboard codes polluting the node — the pointer stays dead).
//! - `BTN_LEFT` derives from held codes (present only when the latched action
//!   contains `left`) OR-ed with the physical `clicks & 1` bit only when the
//!   frame carries no contacts — never from latch state alone, and never
//!   from the physical bit under fingers (a right/combo/empty hold must not
//!   chord left; old-service parity).
//! - Slots are positional per frame (see [`ClickProxy::emit_touch_frame`]
//!   slot-map docs); vacated slots are terminated with `TRACKING_ID=-1`.
//! - Orientation emits `-(raw)`; pressure/major/minor emit the
//!   old-service normalized 60/800/800 (raw bytes are arbitrary units
//!   the classifier drops).
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
//! - Level-transition mode (`transition_mode`): `"continuous"` (default)
//!   holds level-1 keys while force keys join (shared codes down once, up
//!   once — see the reference-held rule below). `"cut"` releases ALL
//!   level-1/slide wire keys first (with SYN) then presses the force combo
//!   fresh, so shared codes show up+SYN+down; easing back below the force
//!   release re-presses the still-latched level-1 keys (one SYN frame, so
//!   the restart reads as up/down pairs, never a stuck key). Skipped for
//!   `none`/`empty` force actions, when nothing is held, and when the
//!   edge absorbed a slide handoff (seamless merge wins for that frame).
//!   Cut ALWAYS emits the release — even under a physically-held
//!   `BTN_LEFT` (the visible break is the entire point of cut; the
//!   physical carve-out stays continuous-only). The release lands on the
//!   edge frame and the fresh press on a later frame once a 5 ms floor
//!   elapses (clock-gated, not frame-counted): a same-frame up+SYN+down
//!   reads as one gesture downstream and the drag never restarts (owner
//!   HW). The fresh press itself stages modifiers-first (super down + SYN,
//!   then left down `combo_stagger_ms` later — human-scale; only
//!   super-first-then-left starts a drag on some compositors). A force
//!   latch that vanishes before the press lands drops the pending press
//!   cleanly. Continuous
//!   skips the cut while the physical button holds `BTN_LEFT` (wire
//!   stays down, no 0-then-1 flutter).
//! - Slide level (`slide_1..=slide_5`, default `none`): the proxy holds a
//!   third key set while fingers slide below the press threshold. Touch is
//!   silent: slide edges never fire a haptic pulse (down or release), not
//!   even for keys/`empty` — only the button levels (click/force) buzz.
//!   A press edge hands off: identical slide/press key sets merge with no
//!   wire flutter, differing sets release slide keys first (one SYN).
//! - Finger-count spoofing (`spoof_<slide|click|force>_<1..5>`, `0` =
//!   passthrough): the touch frame (slots, `BTN_TOOL_*`, `BTN_TOUCH`) is
//!   built from virtual contacts ([`crate::clicks::spoof_contacts`]) under
//!   whichever status holds (force > first > slide-or-idle-touch). Key
//!   selection still uses the real count unless `spoof_select = true`.
//!   Defaults are all passthrough: the wire is byte-identical unless opted
//!   in.
//! - Shared codes across levels (`ctrl` in both chords, `left` in both maps)
//!   are reference-held: down once, retained while either level holds, a
//!   single up on final release — no phantom release+press mid-hold.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use anyhow::Result;

use crate::apple_parse::{FingerContact, TouchFrame, TouchSource};
use crate::clicks::{self, ClickEvent, ClickLevel, ClickMachine, FrameOutcome};
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
pub const ABS_MT_TOUCH_MAJOR: u16 = 48;
pub const ABS_MT_TOUCH_MINOR: u16 = 49;
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
/// Touchpad classification bits (old-service parity): libinput only moves
/// the cursor from MT slots when these are advertised + emitted per count.
pub const BTN_TOOL_FINGER: u16 = 325;
pub const BTN_TOOL_DOUBLETAP: u16 = 333;
pub const BTN_TOOL_TRIPLETAP: u16 = 334;
pub const BTN_TOOL_QUADTAP: u16 = 335;
pub const BTN_TOOL_QUINTTAP: u16 = 328;
pub const BTN_TOUCH: u16 = 330;
/// `INPUT_PROP_POINTER` (`linux/input.h`): marks the virtual node as a
/// pointer device (old-service `PropType::POINTER` parity).
pub const INPUT_PROP_POINTER: u16 = 0;
/// Physical position density, units/mm at 1:1 (old-service
/// `(48 * 100) / speed` at the default `speed = 100`; Y scaled by
/// 44/48). Fixed here: pointer `speed` is applied in software
/// ([`crate::clicks::scale_delta`]), so the advertised density stays
/// truthful while speed still scales motion exactly once.
pub const POSITION_RES_X: i32 = 48;
pub const POSITION_RES_Y: i32 = 44;
/// Normalized per-contact values (old-service parity, hardware-proven):
/// every down contact emits `PRESSURE = 60` + `MAJOR/MINOR = 800`
/// (`200 << 2`). Raw bytes are arbitrary units (`res == 0`); small
/// light-slide values get dropped by the downstream classifier (owner
/// MT2 2026-10-15: real frames flowing, cursor dead until normalized).
pub const TOUCH_PRESSURE_NORM: i32 = 60;
pub const TOUCH_SIZE_NORM: i32 = 800;

/// `EVIOCGRAB` = `_IOW('E', 0x90, int)`.
pub fn eviocgrab_request() -> u64 {
    (1u64 << 30) | ((4u64) << 16) | ((u64::from(b'E')) << 8) | 0x90
}

/// Event-bit disposition for the POINTER virtual node (honest: the setup
/// ioctls `UI_SET_EVBIT`/`UI_ABS_SETUP`/`UI_DEV_CREATE` land with the
/// transport task, which must set exactly these bits). `EV_FF` must never
/// appear — no 2026 libinput/Wayland consumer (RESEARCH §6). Pinned by test.
pub fn pointer_evbits() -> Vec<u16> {
    vec![EV_SYN, EV_KEY, EV_ABS]
}

/// Event bits for the EXTRAS virtual node: keyboard codes only — no `ABS`
/// (it carries no axes), no pointer prop. `EV_FF` never set either.
pub fn extras_evbits() -> Vec<u16> {
    vec![EV_SYN, EV_KEY]
}

// ---- injectable hardware traits (mocks behind these in tests) ----

/// Exclusive-grab handle for the PHYSICAL node.
pub trait GrabDevice: Send {
    fn grab(&self) -> Result<()>;
    fn ungrab(&self);
    fn is_grabbed(&self) -> bool;
}

/// Emit handle for a VIRTUAL uinput node.
pub trait UinputSink: Send {
    fn emit(&self, ev_type: u16, code: u16, value: i32);
    fn sync(&self) {
        self.emit(EV_SYN, SYN_REPORT, 0);
    }
    /// True when the uinput node exists (UI_DEV_CREATE done).
    /// Attach refuses to grab until this holds: grabbing without an
    /// emit path would swallow the physical device.
    fn is_ready(&self) -> bool {
        true
    }
    /// Destroy the virtual node (detach path). Idempotent: safe on a
    /// never-created node. [`DualSink`] overrides this to destroy BOTH
    /// nodes so teardown never leaks the sibling.
    fn destroy(&self) {}
}

/// Haptic feedback sender (host-click report path).
///
/// `pressure` is the actuator pressure byte (the press threshold —
/// old-service parity), `intensity` the click strength (`0` = silent,
/// never sent); `quiet` selects the soft waveform.
/// `release_feedback` is the release up-pulse (`down_pressure` = the same
/// threshold byte the down pulse carried; the sender scales it to
/// `threshold*3/5` with intensity 0 — old-service parity). Unlike the
/// down pulse it always sends: intensity 0 is the payload, not silence.
pub trait HapticSender: Send {
    fn click_feedback(&self, pressure: u8, intensity: u8, quiet: bool) -> Result<()>;
    fn release_feedback(&self, down_pressure: u8, quiet: bool) -> Result<()>;
}

/// Log-only haptic sender (default when no hardware wired).
#[derive(Debug, Default)]
pub struct LogHaptic;

impl HapticSender for LogHaptic {
    fn click_feedback(&self, pressure: u8, intensity: u8, quiet: bool) -> Result<()> {
        if intensity == 0 {
            return Ok(());
        }
        log::info!(
            "haptic feedback: pressure={pressure} intensity={intensity} quiet={quiet} (report 21 01)"
        );
        Ok(())
    }
    fn release_feedback(&self, down_pressure: u8, quiet: bool) -> Result<()> {
        log::info!("haptic release: down_pressure={down_pressure} quiet={quiet} (report 21 01)");
        Ok(())
    }
}

/// Map a config key/button token (lowercased) to `(ev_type, code)`.
/// Mouse buttons map to `BTN_*`; keyboard tokens to `EV_KEY` codes.
/// Returns `None` for unmapped tokens (caller logs + skips).
///
/// Every code below was verified individually against
/// `/usr/include/linux/input-event-codes.h`, never extrapolated.
///
/// Token collisions: `back`/`forward` stay mouse buttons (`BTN_SIDE` /
/// `BTN_EXTRA`, the long-standing mapping `click_4`/`click_5` rely on), so
/// the browser AC Back/Forward keys live under `browserback` /
/// `browserforward` (plus `_`-spelled aliases). `play` stays an alias of
/// `KEY_PLAYPAUSE` (164); the CD-specific variants have their own tokens.
///
/// Deliberately excluded (would confuse consumers on a uinput trackpad):
/// - `KEY_FN` (464): the Fn modifier itself — no standalone meaning.
/// - `KEY_FN_ESC` (465), `KEY_FN_F1..F12` (466..477), `KEY_FN_1/2/D/E/F/S/B`
///   (478..484), `KEY_FN_RIGHT_SHIFT` (485): driver-internal Fn-row reports
///   consumed inside keyboard firmware/drivers, not meaningful output keys.
/// - `KEY_PLAY` (207), `KEY_STOP` (128, AC Stop), `KEY_PROPS` (130),
///   `KEY_STOP_RECORD`/`KEY_PAUSE_RECORD` (609/610): obscure duplicates of
///   covered keys (`playpause`/`stopcd`) or record-specific keys no
///   gesture user plausibly wants; emitting them risks confusing mappings.
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
        "insert" => Some((EV_KEY, 110)),
        "print" => Some((EV_KEY, 210)),
        "home" => Some((EV_KEY, 102)),
        "end" => Some((EV_KEY, 107)),
        "pageup" | "pgup" => Some((EV_KEY, 104)),
        "pagedown" | "pgdn" => Some((EV_KEY, 109)),
        "up" | "arrow_up" => Some((EV_KEY, 103)),
        "down" | "arrow_down" => Some((EV_KEY, 108)),
        "left_arrow" => Some((EV_KEY, 105)),
        "right_arrow" => Some((EV_KEY, 106)),
        "volup" | "volumeup" => Some((EV_KEY, 115)),
        "voldown" | "volumedown" => Some((EV_KEY, 114)),
        "mute" => Some((EV_KEY, 113)),
        "micmute" => Some((EV_KEY, 248)),
        "play" | "playpause" => Some((EV_KEY, 164)),
        "playcd" => Some((EV_KEY, 200)),
        "pausecd" => Some((EV_KEY, 201)),
        "stop" | "stopcd" => Some((EV_KEY, 166)),
        "next" | "nextsong" => Some((EV_KEY, 163)),
        "prev" | "previous" | "previoussong" => Some((EV_KEY, 165)),
        "pause" => Some((EV_KEY, 119)),
        "browserback" | "browser_back" => Some((EV_KEY, 158)),
        "browserforward" | "browser_forward" => Some((EV_KEY, 159)),
        "refresh" => Some((EV_KEY, 173)),
        "homepage" => Some((EV_KEY, 172)),
        "search" => Some((EV_KEY, 217)),
        "favorites" => Some((EV_KEY, 364)),
        "bookmarks" => Some((EV_KEY, 156)),
        "www" => Some((EV_KEY, 150)),
        "calc" => Some((EV_KEY, 140)),
        "mail" => Some((EV_KEY, 155)),
        "computer" => Some((EV_KEY, 157)),
        "player" => Some((EV_KEY, 387)),
        "screenlock" | "lock" => Some((EV_KEY, 152)),
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

/// Minimum gap between a cut-mode release SYN and the deferred fresh press
/// (see [`ClickProxy::pending_force`]). Owner HW: an 11 us-apart up+down
/// coalesces into the still-running drag (Mutter never starts drag #2);
/// 5 ms is imperceptible and safely two gestures. Enforced on the
/// monotonic frame clock, not frame counts — a fast next frame must not
/// collapse the gap.
const CUT_REPRESS_GAP: std::time::Duration = std::time::Duration::from_millis(5);

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
    /// Which hidraw devnode owns the live grab. A `Detached` for any other
    /// node is ignored: without this a sibling interface detaching would
    /// release the active grab and brick the session (grab gone,
    /// `attached=false`, while the owner still thinks it holds the pump).
    attached_node: Option<String>,
    passthrough: bool,
    /// Pump readiness: true only when a real `TouchSource` plus a draining
    /// pump loop plus a created uinput node exist. Until the transport task
    /// wires those, `attach` degrades to passthrough — grabbing without an
    /// emit path would swallow the physical device with nothing replacing it.
    pump_ready: bool,
    held_first: Vec<(u16, u16)>,
    held_force: Vec<(u16, u16)>,
    held_slide: Vec<(u16, u16)>,
    last_pos: HashMap<u8, (i32, i32)>,
    /// Highest slot index written so far; vacated slots are terminated.
    max_slot: usize,
    /// Last `BTN_LEFT` value on the wire. `BTN_LEFT` is edge-triggered:
    /// it emits only on state change, so non-left clicks carry zero
    /// `BTN_LEFT` traffic and steady holds stay silent. Pure wire
    /// hygiene; no claim about any downstream symptom.
    btn_left_down: bool,
    /// A down pulse fired for the live first-level latch (old-service
    /// `pressed_haptic_triggered` parity): the release edge owes the
    /// actuator its up-pulse. Silent presses (`haptic == 0`, `none`
    /// action) leave this false so the release stays silent too.
    pressed_haptic: bool,
    /// Same for the live force-level latch (`force_haptic_triggered`).
    /// The slide level never pulses (touch is silent): no flag, no debt.
    force_haptic: bool,
    force_haptic: bool,
    /// Cut-mode deferred force press: the force edge (frame N) emits the
    /// level-1/slide release + SYN, the fresh force-combo modifiers land
    /// once `now - released_at >= CUT_REPRESS_GAP` (5 ms floor on the
    /// monotonic clock, not frame counts), and the buttons follow once
    /// `now - mods_at >= combo_stagger_ms` (human-scale — only
    /// super-first-then-left starts a drag on some compositors). The
    /// release, modifiers and buttons must read as distinct gestures
    /// downstream. If the force latch vanishes before the press lands
    /// (liftoff/ease-off), the pending press is dropped: its keys never
    /// went down, so there is nothing to release and nothing dangles.
    pending_force: Option<PendingForcePress>,
}

/// One staged cut-press key group (modifiers or buttons): wire pairs in
/// emit order.
type KeyGroup = Vec<(u16, u16)>;

/// A cut-mode force press waiting for its gap-gated frames (see
/// [`ClickProxy::pending_force`]). `released_at` is the edge frame's
/// clock: the modifier group lands once `now - released_at >=
/// CUT_REPRESS_GAP`. `mods_at` is the modifiers' clock once they land:
/// the button group follows once `now - mods_at >= combo_stagger_ms`
/// (human-scale gap).
#[derive(Debug, Clone)]
struct PendingForcePress {
    ev: ClickEvent,
    released_at: std::time::Instant,
    mods_at: Option<std::time::Instant>,
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
            attached_node: None,
            passthrough: true,
            pump_ready: false,
            held_first: Vec::new(),
            held_force: Vec::new(),
            held_slide: Vec::new(),
            last_pos: HashMap::new(),
            max_slot: 0,
            btn_left_down: false,
            pressed_haptic: false,
            force_haptic: false,
            pending_force: None,
        }
    }

    pub fn is_attached(&self) -> bool {
        self.attached
    }

    /// True when events flow untouched (gate off, pump unready, grab failed).
    pub fn is_passthrough(&self) -> bool {
        self.passthrough
    }

    /// Borrow the touch source (manager swaps hidraw nodes per attach).
    pub fn source_mut(&mut self) -> &mut S {
        &mut self.source
    }

    /// Replace the touch source, returning the old one.
    pub fn replace_source(&mut self, src: S) -> S {
        std::mem::replace(&mut self.source, src)
    }

    /// Borrow the uinput sink (manager creates/destroys the node).
    pub fn uinput(&self) -> &U {
        &self.uinput
    }

    /// Borrow the grab handle (manager points it at the resolved evdev).
    pub fn grab(&self) -> &G {
        &self.grab
    }

    /// Borrow the haptic sender mutably (manager points it at the hidraw node).
    pub fn haptic_mut(&mut self) -> &mut H {
        &mut self.haptic
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
            self.pressed_haptic = false;
            self.force_haptic = false;
            self.pending_force = None;
            self.last_pos.clear();
            self.attached = false;
            self.attached_node = None;
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
            self.attached_node = None;
            self.passthrough = true;
            return;
        }
        if !self.pump_ready {
            log::info!(
                "proxy: pump not ready for {} (no TouchSource/loop/uinput yet); passthrough",
                event.devnode
            );
            self.attached = false;
            self.attached_node = None;
            self.passthrough = true;
            return;
        }
        if !self.uinput.is_ready() {
            log::info!(
                "proxy: uinput not created for {} yet; passthrough (create before grab)",
                event.devnode
            );
            self.attached = false;
            self.attached_node = None;
            self.passthrough = true;
            return;
        }
        match self.grab.grab() {
            Ok(()) => {
                log::info!("proxy: grabbed {} (uinput virtual node up)", event.devnode);
                self.attached = true;
                self.attached_node = Some(event.devnode.clone());
                self.passthrough = false;
            }
            Err(e) => {
                log::warn!(
                    "proxy: grab failed on {}: {e:#}; passthrough continues",
                    event.devnode
                );
                self.attached = false;
                self.attached_node = None;
                self.passthrough = true;
            }
        }
    }

    /// Detach on `Detached`: release grab, drop latch + held keys, trailing
    /// `SYN` so no key-up is lost (stuck keys otherwise). Ownership-gated:
    /// a `Detached` for any node other than the grabbed one is ignored, so
    /// a sibling interface vanishing can never release the live grab.
    pub fn detach(&mut self, event: &DeviceReadyEvent) {
        if event.kind != ReadyKind::Detached {
            return;
        }
        if self.attached_node.as_deref() != Some(event.devnode.as_str()) {
            log::debug!(
                "proxy: ignoring Detached for {} (grab owned by {:?})",
                event.devnode,
                self.attached_node
            );
            return;
        }
        if self.attached {
            log::info!("proxy: releasing grab ({})", event.devnode);
            self.grab.ungrab();
        }
        self.release_held(false);
        self.uinput.sync();
        self.machine.reset();
        self.pressed_haptic = false;
        self.force_haptic = false;
        self.pending_force = None;
        self.last_pos.clear();
        self.max_slot = 0;
        self.attached = false;
        self.attached_node = None;
        self.passthrough = true;
    }

    /// One poll step: `poll_frame` errors are logged + dropped (`Ok(None)`),
    /// never propagated (the read loop must not abort on a bad frame).
    /// Gate-off, pump-unready, or passthrough short-circuits without touching
    /// hardware. Exactly one `SYN` is emitted per handled frame.
    /// Timed with `Instant::now` (old-service 50 ms press / 30 ms release
    /// debounce); tests needing deterministic edges use
    /// [`ClickProxy::handle_frame_at`].
    pub fn handle_frame(&mut self, cfg: &Config) -> Result<Option<ClickEvent>> {
        self.handle_frame_at(cfg, std::time::Instant::now())
    }

    /// [`ClickProxy::handle_frame`] with an explicit clock (deterministic
    /// debounce in tests; production passes `Instant::now`).
    pub fn handle_frame_at(
        &mut self,
        cfg: &Config,
        now: std::time::Instant,
    ) -> Result<Option<ClickEvent>> {
        if !self.settings.enhanced || !self.attached || self.passthrough {
            return Ok(None);
        }
        let frame = match self.source.poll_frame() {
            Ok(Some(f)) => f,
            Ok(None) => return Ok(None),
            Err(e) => {
                // Persistent disconnect (unplug/replug reusing the same
                // hidraw name: the fd is dead but no Detached will ever
                // bound the errors) self-releases the session exactly
                // once: ungrab, held keys up, trailing SYN, latch reset,
                // back to passthrough with the pump marked unready. After
                // the release the idle short-circuit above stops polling
                // the dead fd, so logs stay bounded (one warn per state
                // change, never ~500/s). Re-attach rebinds fresh fds.
                if crate::pump::is_disconnect(&e) {
                    if self.attached {
                        log::warn!("proxy: disconnect detected, self-releasing: {e:#}");
                        self.set_pump_ready(false);
                    } else {
                        log::debug!("proxy: disconnect poll while idle: {e:#}");
                    }
                    return Ok(None);
                }
                log::warn!("proxy: poll_frame failed, dropping frame: {e:#}");
                return Ok(None);
            }
        };
        let was_first = self.machine.active().is_some();
        let was_force = self.machine.force_active().is_some();
        let was_slide = self.machine.slide_active().cloned();
        let edges = match self
            .machine
            .update_at(&frame.fingers, frame.clicks, cfg, now)
        {
            Ok(e) => e,
            Err(e) => {
                log::warn!("proxy: click/force parse failed, dropping edge: {e:#}");
                return Ok(None);
            }
        };
        // Haptic pressure byte = the press threshold (old-service parity:
        // `pressure=sensitivity`, `intensity=haptic`; never the strength
        // echoed as pressure).
        let haptic_pressure = cfg.sensitivity.clamp(1, 255) as u8;
        // Touch slots first (post-update latch is visible to buttons below;
        // the spoof map follows the live status: force > first > slide).
        self.emit_touch_frame(&frame, cfg);
        // Slide re-select: a count change onto a different action ends the
        // old session (keys up, never a pulse — slide is silent) before
        // the new count presses. An identical key set merges instead
        // (latch already swapped by the machine; wire carries over
        // untouched — no mid-slide flutter). A lone slide edge just
        // presses keys.
        if let Some(ref ev) = edges.slide {
            let merged = was_slide
                .as_ref()
                .is_some_and(|old| old.action.kind.is_same_action(&ev.action.kind));
            if !merged {
                if was_slide.is_some() {
                    self.release_slide_held(ClickMachine::physical_button(frame.clicks));
                }
                self.press_level(ev, haptic_pressure);
            }
        }
        // Press order: first then force; reporters see force (supersedes,
        // unless force is `none` — see `FrameOutcome::primary`). A press
        // edge hands off a held slide: identical key sets merge with no
        // wire flutter, differing sets release slide keys first (keys
        // only — slide never pulsed, so the firing press always emits
        // its own down-pulse).
        let absorbed = if edges.first.is_some() || edges.force.is_some() {
            self.handoff_slide(
                &edges,
                was_slide.clone(),
                ClickMachine::physical_button(frame.clicks),
            )
        } else {
            None
        };
        // Cut-mode deferred press (two stages, chronological: the deferred
        // edge is older than this frame's own presses).
        self.flush_cut_pending(cfg, haptic_pressure, now);
        if let Some(ref ev) = edges.first {
            self.press_level(ev, haptic_pressure);
        }
        if let Some(ref ev) = edges.force {
            // Cut mode: release ALL level-1/slide wire keys first (with
            // SYN) on the edge frame; the fresh force combo follows in two
            // clock-gated stages — modifiers once the 5 ms floor elapses,
            // buttons `combo_stagger_ms` after the modifiers (human-scale:
            // only super-first-then-left starts a drag on some
            // compositors). The split is the point: a same-frame
            // up+SYN+down lands ~11 us apart and the compositor merges it
            // into the still-running drag (owner HW: overview opens, drag
            // never starts). If the force latch vanishes before the press
            // lands, the pending press drops cleanly (its keys never went
            // down). Skipped for `none` / `empty` (no keys to restart into:
            // releasing the hold for a haptic-only pulse would flutter),
            // when nothing is held, and when this frame absorbed a slide
            // (the seamless handoff just merged those keys: cutting them
            // would reintroduce the flutter the handoff avoided).
            if cfg.is_cut_transition()
                && !ev.is_none()
                && !ev.is_empty()
                && !self.held_first.is_empty()
                && absorbed.is_none()
            {
                // No-op after a handoff (slide already absorbed/released),
                // belt-and-braces so cut never leaves a slide hold behind.
                // Cut bypasses the physical carve-out (`false`): the visible
                // break is the entire point of cut, and the user
                // physically holds the click essentially always.
                self.release_slide_held(false);
                self.cut_first_wire();
                self.uinput.sync();
                self.pending_force = Some(PendingForcePress {
                    ev: ev.clone(),
                    released_at: now,
                    mods_at: None,
                });
            } else {
                self.press_level(ev, haptic_pressure);
            }
        }
        }
        if edges.first.is_some() || edges.force.is_some() || edges.slide.is_some() {
            // Edge frames reconcile `BTN_LEFT` through the wire tracker
            // (emits only on change — fresh downs already went out above,
            // except a cut-mode force edge whose press is deferred to the
            // next frame: the release went out with its SYN, and the
            // reconcile below is a no-op, never a flutter).
            // `press_level` never syncs itself; exactly one trailing SYN
            // per handled frame — except a cut-mode force edge, which
            // already synced once after the level-1 release.
            self.emit_btn_left(frame.clicks, !frame.fingers.is_empty());
            self.uinput.sync();
            return Ok(edges.primary());
        }
        let first_gone = was_first && self.machine.active().is_none();
        let force_gone = was_force && self.machine.force_active().is_none();
        // A held slide that vanished with no edge and no handoff (liftoff,
        // or a re-select onto an unmapped count) releases its keys.
        let slide_gone = was_slide.is_some()
            && self.machine.slide_active().is_none()
            && edges.slide.is_none()
            && edges.first.is_none()
            && edges.force.is_none();
        if first_gone {
            // Full release clears both levels. The physical bit rides
            // along so a still-held hardware button suppresses the
            // latch's left-up (no 0-then-1 flutter in one frame).
            self.release_held(ClickMachine::physical_button(frame.clicks));
            // Old-service release snap: force up-pulse first (when its
            // down pulse fired), then the first-level up-pulse. No 15 ms
            // staging sleep (old inserts one between the two): the pump
            // thread must not stall on a release frame.
            if was_force {
                self.fire_release(
                    clicks::ClickLevel::Force,
                    haptic_pressure,
                    cfg.force_quiet_click,
                );
            }
            self.fire_release(clicks::ClickLevel::First, haptic_pressure, cfg.quiet_click);
        } else if force_gone {
            // Back to the first-level hold: force keys up, first keys stay
            // (continuous). In cut mode the first keys were released at the
            // force edge, so re-press the still-latched level-1 combo (keys
            // only, no haptic: no new edge fired).
            self.release_force_held();
            self.fire_release(
                clicks::ClickLevel::Force,
                haptic_pressure,
                cfg.force_quiet_click,
            );
            if cfg.is_cut_transition() {
                if let Some(ev) = self.machine.active().cloned() {
                    self.repress_keys(&ev);
                }
            }
        } else if slide_gone {
            self.release_slide_held(ClickMachine::physical_button(frame.clicks));
        }
        self.emit_btn_left(frame.clicks, !frame.fingers.is_empty());
        self.uinput.sync();
        Ok(None)
    }

    /// `BTN_LEFT` from held codes only (either latched action contains `left`)
    /// or the physical button bit with no contacts on the frame — never from
    /// latch state alone, and never from the physical bit while fingers are
    /// present, so a right/middle/combo/empty hold cannot chord a phantom
    /// left (old-service parity: buttons derive purely from the pressure
    /// latch + finger count; the `clicks & 1` bit only covers the bare
    /// no-contact button path). Edge triggered: emits only on state change,
    /// so frames where left is and was down stay silent (no `BTN_LEFT=0`
    /// releases for buttons never pressed, no duplicate re-asserts).
    fn emit_btn_left(&mut self, clicks: u8, has_fingers: bool) {
        let held_left = self
            .held_first
            .iter()
            .chain(self.held_force.iter())
            .chain(self.held_slide.iter())
            .any(|&(ty, code)| ty == EV_KEY && code == BTN_LEFT);
        let down = held_left || (!has_fingers && ClickMachine::physical_button(clicks));
        if down != self.btn_left_down {
            self.uinput.emit(EV_KEY, BTN_LEFT, i32::from(down));
            self.btn_left_down = down;
        }
    }

    /// True when `(ty, code)` is already down on the wire (held by any
    /// level). Shared codes (e.g. `ctrl` in `ctrl+c` + `ctrl+v`, or `left`
    /// in both levels, or a slide set a press edge absorbs) go down once
    /// and stay down until every holding level releases them.
    fn is_down(&self, pair: (u16, u16)) -> bool {
        self.held_first.contains(&pair)
            || self.held_force.contains(&pair)
            || self.held_slide.contains(&pair)
    }

    /// Press one level's edge: keys down (deduplicated across levels, see
    /// [`ClickProxy::is_down`]) plus that level's own haptic pulse. The
    /// slide level never pulses (touch is silent): keys (or nothing for
    /// `empty`/`none`) go down, no down-pulse, no release debt. Edges
    /// arrive staged old-exact (first frame one, force frame two), so each
    /// button pulse lands on its own frame. Never syncs: the caller emits
    /// buttons + a single `SYN` per frame. `pressure` is the actuator
    /// pressure byte (the press threshold, old-service parity); the event
    /// carries the intensity + waveform.
    fn press_level(&mut self, ev: &ClickEvent, pressure: u8) {
        use crate::config::ActionKind;
        let name = match ev.level {
            ClickLevel::First => "click",
            ClickLevel::Force => "force",
            ClickLevel::Slide => "slide",
        };
        // Slide = just touching: keys still flow, pulses never do.
        if matches!(ev.level, ClickLevel::Slide) {
            if let ActionKind::Keys(keys) = &ev.action.kind {
                self.down_keys(keys, ev.level);
            } else {
                log::debug!(
                    "proxy: {name}_{} silent (slide never pulses)",
                    ev.finger_count
                );
            }
            return;
        }
        match &ev.action.kind {
            ActionKind::None => {
                log::debug!("proxy: {name}_{} = none (swallowed)", ev.finger_count);
            }
            ActionKind::Empty => {
                let fired = self.fire_haptic(ev, pressure);
                self.set_haptic_flag(ev.level, fired);
            }
            ActionKind::Keys(keys) => {
                self.down_keys(keys, ev.level);
                let fired = self.fire_haptic(ev, pressure);
                self.set_haptic_flag(ev.level, fired);
            }
        }
    }

    /// Key-down half of [`ClickProxy::press_level`]: emits fresh downs
    /// (deduplicated via [`ClickProxy::is_down`]) and records them on the
    /// level's held set for release accounting. `BTN_LEFT` goes through
    /// the wire tracker (see [`ClickProxy::emit_btn_left`]). Extras-routed
    /// codes (modifiers, combos) emit before pointer-routed buttons, stable
    /// within each route — the compositor must see e.g. super held when
    /// left lands (window-drag), regardless of config order (`left+super`
    /// vs `super+left`). Simultaneous within one frame (continuous mode and
    /// all non-cut presses keep this); the cut fresh-press staggers instead
    /// (see [`ClickProxy::down_keys_staggered`]).
    fn down_keys(&mut self, keys: &[String], level: ClickLevel) {
        let mut pairs = Self::resolve_keys(keys);
        // Stable: extras first, pointer buttons second.
        pairs.sort_by_key(|&(ty, code)| {
            u8::from(matches!(split_route(ty, code), SplitRoute::Pointer))
        });
        for pair in pairs {
            self.emit_key_down(pair, level);
        }
    }

    /// Cut fresh-press key-downs for one stage: modifiers first as SEPARATE
    /// steps — the extras-routed group goes down + `SYN`, then the
    /// pointer-routed group goes down. Same-frame simultaneous combo downs
    /// read as an extension of the old drag downstream (owner HW: drag never
    /// restarts); the framed modifier-first order gives Mutter the modifier
    /// established before the button that starts the move-drag.
    /// Single-group actions behave exactly like [`ClickProxy::down_keys`]
    /// (no stray `SYN`). Only the cut fresh-press stages this way (see
    /// [`ClickProxy::flush_cut_pending`); continuous and all other presses
    /// stay simultaneous.
    fn down_keys_staggered(&mut self, keys: &[String], level: ClickLevel) {
        let pairs = Self::resolve_keys(keys);
        let (mods, btns): (Vec<_>, Vec<_>) = pairs
            .into_iter()
            .partition(|&(ty, code)| !matches!(split_route(ty, code), SplitRoute::Pointer));
        for pair in &mods {
            self.emit_key_down(*pair, level);
        }
        if !mods.is_empty() && !btns.is_empty() {
            self.uinput.sync();
        }
        for pair in &btns {
            self.emit_key_down(*pair, level);
        }
    }

    /// Resolve action tokens to wire pairs (unknown tokens log + skip).
    fn resolve_keys(keys: &[String]) -> Vec<(u16, u16)> {
        let mut pairs = Vec::with_capacity(keys.len());
        for k in keys {
            match key_token_to_code(k) {
                Some(pair) => pairs.push(pair),
                None => log::warn!("proxy: no keycode for '{k}', skipped"),
            }
        }
        pairs
    }

    /// One key-down: emit unless already down elsewhere (per-level record
    /// always, so release accounting stays per-level). `BTN_LEFT` goes
    /// through the wire tracker instead of a duplicate down.
    fn emit_key_down(&mut self, pair: (u16, u16), level: ClickLevel) {
        let (ty, code) = pair;
        // Skip the re-emit when another level already holds
        // the code down, but still record it in this level's
        // set so release accounting stays per-level.
        // `BTN_LEFT` goes through the wire tracker instead: it
        // is already down when the physical bit holds it, and
        // a duplicate down is redundant traffic the tracker
        // exists to remove.
        if pair == (EV_KEY, BTN_LEFT) {
            if !self.btn_left_down {
                self.uinput.emit(ty, code, 1);
                self.btn_left_down = true;
            }
        } else if !self.is_down(pair) {
            self.uinput.emit(ty, code, 1);
        }
        let held = match level {
            ClickLevel::First => &mut self.held_first,
            ClickLevel::Force => &mut self.held_force,
            ClickLevel::Slide => &mut self.held_slide,
        };
        // Same-level dedup: a handoff-absorbed code is already
        // recorded (one up per code on release); cross-level
        // sharing still records per level.
        if !held.contains(&pair) {
            held.push(pair);
        }
    }

    /// Cut fresh-press for single-group actions (modifiers-only or
    /// buttons-only): one stage, simultaneous, framed by the caller's syncs
    /// exactly like [`ClickProxy::down_keys`] — plus the level's own haptic
    /// pulse (as in [`ClickProxy::press_level`]). Mixed actions never come
    /// here (they stage through [`ClickProxy::flush_cut_pending`]).
    fn press_cut_combo(&mut self, ev: &ClickEvent, pressure: u8) {
        use crate::config::ActionKind;
        match &ev.action.kind {
            ActionKind::None => {
                log::debug!("proxy: force_{} = none (swallowed)", ev.finger_count);
            }
            ActionKind::Empty => {
                self.pulse_cut_haptic(ev, pressure);
            }
            ActionKind::Keys(keys) => {
                self.down_keys_staggered(keys, ev.level);
                self.pulse_cut_haptic(ev, pressure);
            }
        }
    }

    /// The level's own haptic pulse for a cut staged press.
    fn pulse_cut_haptic(&mut self, ev: &ClickEvent, pressure: u8) {
        let fired = self.fire_haptic(ev, pressure);
        self.set_haptic_flag(ev.level, fired);
    }

    /// Split a force action's keys into (modifiers, buttons): extras-routed
    /// first group, pointer-routed second. Non-key actions yield two empty
    /// groups (cut never stores those; defensive).
    fn partition_cut_keys(ev: &ClickEvent) -> (KeyGroup, KeyGroup) {
        use crate::config::ActionKind;
        match &ev.action.kind {
            ActionKind::Keys(keys) => Self::resolve_keys(keys)
                .into_iter()
                .partition(|&(ty, code)| !matches!(split_route(ty, code), SplitRoute::Pointer)),
            _ => (Vec::new(), Vec::new()),
        }
    }

    /// Land a pending cut fresh-press in clock-gated stages. Drops the
    /// pending press when the force latch already vanished (liftoff/ease-off
    /// before the press lands): its keys never went down, so there is
    /// nothing to release. Lands before this frame's own presses
    /// (chronological: the deferred edge is older).
    ///
    /// Stage 1 (modifiers): once `now - released_at >= CUT_REPRESS_GAP`
    /// (5 ms floor, clock-gated not frame-counted). Single-group actions
    /// finish here via [`ClickProxy::press_cut_combo`].
    ///
    /// Stage 2 (buttons): once `now - mods_at >= combo_stagger_ms`
    /// (human-scale; `0` lands both stages in one frame, still ≥5 ms after
    /// the release). The haptic pulse fires with stage 1 (earliest feedback
    /// for the threshold crossing).
    fn flush_cut_pending(&mut self, cfg: &Config, pressure: u8, now: std::time::Instant) {
        if self.machine.force_active().is_none() {
            // Traced at debug with the survived time — a press that never
            // survives the floor shows here, which distinguishes
            // hold-instability from node/routing loss.
            if let Some(pending) = self.pending_force.take() {
                log::debug!(
                    "proxy: cut pending force press dropped (latch gone after {}ms, floor {}ms)",
                    now.saturating_duration_since(pending.released_at)
                        .as_millis(),
                    CUT_REPRESS_GAP.as_millis()
                );
            }
            return;
        }
        let Some((ev, released_at)) = self
            .pending_force
            .as_ref()
            .map(|p| (p.ev.clone(), p.released_at))
        else {
            return;
        };
        let mods_done = self
            .pending_force
            .as_ref()
            .is_some_and(|p| p.mods_at.is_some());
        // Stage 1 (modifiers): once the 5 ms floor elapses. Single-group
        // actions finish here via [`ClickProxy::press_cut_combo`].
        if !mods_done {
            if now.saturating_duration_since(released_at) < CUT_REPRESS_GAP {
                return;
            }
            let (mods, btns) = Self::partition_cut_keys(&ev);
            if mods.is_empty() || btns.is_empty() {
                self.press_cut_combo(&ev, pressure);
                self.pending_force = None;
                return;
            }
            for pair in &mods {
                self.emit_key_down(*pair, ev.level);
            }
            self.uinput.sync();
            self.pulse_cut_haptic(&ev, pressure);
            if let Some(pending) = self.pending_force.as_mut() {
                pending.mods_at = Some(now);
            }
        }
        // Stage 2 (buttons): once `combo_stagger_ms` elapsed since the
        // modifiers — same frame when the stagger is 0.
        let mods_at = self.pending_force.as_ref().and_then(|p| p.mods_at);
        if let Some(t) = mods_at {
            if now.saturating_duration_since(t) >= Self::cut_stagger(cfg) {
                let (_, btns) = Self::partition_cut_keys(&ev);
                for pair in &btns {
                    self.emit_key_down(*pair, ev.level);
                }
                self.pending_force = None;
            }
        }
        // Else: stagger not elapsed — the buttons wait for a later frame.
    }

    /// Effective button stagger for the cut fresh-press: the configured
    /// `combo_stagger_ms` (the 5 ms release→modifiers floor already applied
    /// upstream, so `0` keeps release→press ≥5 ms).
    fn cut_stagger(cfg: &Config) -> std::time::Duration {
        std::time::Duration::from_millis(u64::from(cfg.combo_stagger_ms))
    }

    /// Re-press the still-latched level-1 combo after a cut-mode force
    /// release (keys only, no haptic: no new edge fired). Records into
    /// `held_first` so the eventual full release owns the key-ups.
    fn repress_keys(&mut self, ev: &ClickEvent) {
        use crate::config::ActionKind;
        if let ActionKind::Keys(keys) = &ev.action.kind {
            debug_assert!(matches!(ev.level, ClickLevel::First));
            self.down_keys(keys, ClickLevel::First);
        }
    }

    /// Record which button level fired its down pulse (so the release owes
    /// the up-pulse exactly then). Slide never calls this (silent).
    fn set_haptic_flag(&mut self, level: ClickLevel, fired: bool) {
        match level {
            ClickLevel::First => self.pressed_haptic = fired,
            ClickLevel::Force => self.force_haptic = fired,
            ClickLevel::Slide => {}
        }
    }

    /// Slide handoff on a press edge: identical slide/press key sets merge
    /// (slide wire keys move into the firing level with no ups — the hold
    /// continues seamlessly, and the firing press emits its own down-pulse:
    /// slide never pulsed), differing sets release slide keys first (keys
    /// only, never a pulse). Force is checked before first: when both
    /// edges fire on one frame the superseding force level owns the merge.
    /// Returns the absorb target, if any. `was_slide` is the pre-update
    /// latch (the machine already dropped it on handoff); callers pass it
    /// because post-update state cannot tell a handoff from a liftoff.
    fn handoff_slide(
        &mut self,
        edges: &FrameOutcome,
        was_slide: Option<ClickEvent>,
        phys_down: bool,
    ) -> Option<ClickLevel> {
        let slide = was_slide?;
        let slide_kind = &slide.action.kind;
        // Set compare (order-/dup-insensitive wire identity): `ctrl+left`
        // merges a `left+ctrl` hold with no flutter.
        let force_same = edges
            .force
            .as_ref()
            .is_some_and(|e| e.action.kind.is_same_action(slide_kind));
        let first_same = edges
            .first
            .as_ref()
            .is_some_and(|e| e.action.kind.is_same_action(slide_kind));
        if force_same {
            self.absorb_slide_into(ClickLevel::Force);
            Some(ClickLevel::Force)
        } else if first_same {
            self.absorb_slide_into(ClickLevel::First);
            Some(ClickLevel::First)
        } else {
            self.release_slide_held(phys_down);
            None
        }
    }

    /// Merge held slide wire keys into the firing level with no wire
    /// traffic (same-action handoff): codes the target already recorded
    /// are dropped to keep release accounting duplicate-free. No haptic
    /// moves (slide never pulsed): the firing press emits its own pulse.
    fn absorb_slide_into(&mut self, level: ClickLevel) {
        if matches!(level, ClickLevel::Slide) {
            return;
        }
        let moving: Vec<(u16, u16)> = std::mem::take(&mut self.held_slide);
        let target = match level {
            ClickLevel::First => &mut self.held_first,
            ClickLevel::Force => &mut self.held_force,
            ClickLevel::Slide => unreachable!("absorb target is First/Force (guarded above)"),
        };
        for pair in moving {
            if !target.contains(&pair) {
                target.push(pair);
            }
        }
    }

    /// Down pulse for one level's press edge. Returns whether the pulse
    /// fired (false = silent config, so the release owes no up-pulse).
    fn fire_haptic(&self, ev: &ClickEvent, pressure: u8) -> bool {
        if !clicks::should_emit_haptic(ev.haptic) {
            return false;
        }
        let intensity = ev.haptic.min(255) as u8;
        if let Err(e) = self.haptic.click_feedback(pressure, intensity, ev.quiet) {
            log::warn!("proxy: haptic send failed: {e:#}");
        }
        true
    }

    /// Release up-pulse for one button level (old-service `vibration_up`
    /// parity). Fires only when that level's down pulse fired; clears
    /// the flag either way so a release never snaps twice. Slide never
    /// owes a pulse (it never fires one).
    fn fire_release(&mut self, level: ClickLevel, down_pressure: u8, quiet: bool) {
        let fired = match level {
            ClickLevel::First => std::mem::replace(&mut self.pressed_haptic, false),
            ClickLevel::Force => std::mem::replace(&mut self.force_haptic, false),
            ClickLevel::Slide => false,
        };
        if !fired {
            return;
        }
        if let Err(e) = self.haptic.release_feedback(down_pressure, quiet) {
            log::warn!("proxy: haptic release send failed: {e:#}");
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
        let slide = std::mem::take(&mut self.held_slide);
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
        for &(ty, code) in &slide {
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
        for (ty, code) in self.held_force.drain(..) {
            if first.contains(&(ty, code)) || slide.contains(&(ty, code)) {
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

    /// Release slide-held keys, retaining codes another level still holds
    /// (no up-emit for those — the wire stays down until full release).
    /// Like [`ClickProxy::release_held`], `BTN_LEFT` stays down while the
    /// physical button holds it (`phys_down`): emitting 0-then-1 in the
    /// same frame would flutter a phantom extra click. The caller
    /// reconciles via [`ClickProxy::emit_btn_left`] right after.
    fn release_slide_held(&mut self, phys_down: bool) {
        let mut dropped_left = false;
        for (ty, code) in self.held_slide.drain(..) {
            if self.held_first.contains(&(ty, code)) || self.held_force.contains(&(ty, code)) {
                continue;
            }
            if phys_down && (ty, code) == (EV_KEY, BTN_LEFT) {
                continue; // physical still holds it; wire stays down
            }
            self.uinput.emit(ty, code, 0);
            if (ty, code) == (EV_KEY, BTN_LEFT) {
                dropped_left = true;
            }
        }
        // Only a slide-only left actually went up on the wire here.
        if dropped_left {
            self.btn_left_down = false;
        }
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

    /// Cut-mode release of level-1 wire keys on a force edge: every
    /// `held_first` code goes up and the vec clears, so the force press
    /// re-downs shared codes fresh (up+SYN+down on the wire). ALWAYS emits
    /// — even under a physically-held `BTN_LEFT`: the visible break is the
    /// entire point of cut (continuous keeps the carve-out in
    /// [`ClickProxy::release_held`]). The first-level latch stays latched
    /// in the machine — only the wire restarts. The caller syncs between
    /// this and the force press; the edge-frame [`ClickProxy::emit_btn_left`]
    /// reconciliation after the fresh press is then a no-op (wire already
    /// down again), never a flutter.
    fn cut_first_wire(&mut self) {
        for (ty, code) in std::mem::take(&mut self.held_first) {
            self.uinput.emit(ty, code, 0);
            if (ty, code) == (EV_KEY, BTN_LEFT) {
                self.btn_left_down = false;
            }
        }
    }

    /// Emit Type-B MT slots for one frame. No trailing `SYN`: the caller
    /// syncs once per frame after keys.
    ///
    /// Slot-map semantics (transport-task contract): slots are POSITIONAL —
    /// slot N is the Nth contact in the frame, reassigned every frame, and
    /// carries no identity across frames. Identity lives in `TRACKING_ID`
    /// (the device id); libinput pairs slot↔id per `SYN`. A slot with no
    /// contact this frame gets `TRACKING_ID = -1`, including slots that
    /// existed only in earlier frames (`max_slot` termination) — without
    /// that, a 2→1 finger transition leaves a ghost contact alive downstream.
    ///
    /// Presence follows [`clicks::touch_present`] (device flag OR pressure
    /// past the touch floor, old-service parity). Per-contact axis order
    /// mirrors the old driver (slot, id, x, y, pressure, major, minor,
    /// orientation); the trailing `BTN_TOOL_*` row carries the touching
    /// count (1/2/3/4/5+, exactly like old) so libinput classifies the
    /// node as a touchpad and moves the cursor. Pressure/major/minor emit
    /// the old-service normalized values (60/800/800): raw bytes are
    /// arbitrary units (`res == 0`) and small light-slide values get
    /// dropped by the classifier (cursor dead); the fixed values sit
    /// inside the 0..253 / 0..1020 axis ranges.
    ///
    /// Spoofing: the emitted contacts follow the live status map (force
    /// held → `spoof_force_*`, first held → `spoof_click_*`, otherwise
    /// `spoof_slide_*`) via [`clicks::spoof_contacts`]. All maps default
    /// to passthrough, so the frame below is the real one unless opted in.
    fn emit_touch_frame(&mut self, frame: &TouchFrame, cfg: &Config) {
        use std::borrow::Cow;
        use std::collections::HashSet;
        let real_touching = frame
            .fingers
            .iter()
            .filter(|f| clicks::touch_present(f))
            .count();
        let status = if self.machine.force_active().is_some() {
            "force"
        } else if self.machine.active().is_some() {
            "click"
        } else {
            "slide"
        };
        let mapped = cfg.spoofed_count(status, real_touching.min(255) as u8);
        // Passthrough (or an unindexed real count > 5) borrows the real
        // fingers untouched: no per-frame clone, no synthetic jitter, no
        // dropped slots. Only a live spoof allocates.
        let fingers: Cow<'_, [FingerContact]> = if mapped == real_touching.min(255) as u8 {
            Cow::Borrowed(&frame.fingers)
        } else {
            Cow::Owned(clicks::spoof_contacts(&frame.fingers, mapped))
        };
        let present_count = fingers.iter().filter(|f| clicks::touch_present(f)).count();
        let any_touch = present_count > 0;
        let n = fingers.len().min(16);
        for (slot, f) in fingers.iter().take(16).enumerate() {
            self.uinput.emit(EV_ABS, ABS_MT_SLOT, slot as i32);
            if clicks::touch_present(f) {
                self.uinput
                    .emit(EV_ABS, ABS_MT_TRACKING_ID, i32::from(f.id));
                let (sx, sy) = self.scaled_pos(f.id, f.x, f.y, cfg.speed);
                self.uinput.emit(EV_ABS, ABS_MT_POSITION_X, sx);
                self.uinput.emit(EV_ABS, ABS_MT_POSITION_Y, sy);
                self.uinput
                    .emit(EV_ABS, ABS_MT_PRESSURE, TOUCH_PRESSURE_NORM);
                self.uinput
                    .emit(EV_ABS, ABS_MT_TOUCH_MAJOR, TOUCH_SIZE_NORM);
                self.uinput
                    .emit(EV_ABS, ABS_MT_TOUCH_MINOR, TOUCH_SIZE_NORM);
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
        // every frame, not only the empty one. Anchors key on the EMITTED
        // (possibly spoofed) ids so virtual contacts track smoothly.
        let present: HashSet<u8> = fingers.iter().map(|f| f.id).collect();
        if fingers.is_empty() {
            self.last_pos.clear();
        } else {
            self.last_pos.retain(|id, _| present.contains(id));
        }
        self.uinput.emit(
            EV_KEY,
            BTN_TOUCH,
            i32::from(any_touch || ClickMachine::physical_button(frame.clicks)),
        );
        self.emit_tool_bits(present_count);
    }

    /// `BTN_TOOL_*` classification row from the touching count (old-service
    /// parity: exactly one of FINGER/DOUBLE/TRIPLE/QUAD/QUIN is down for
    /// counts 1/2/3/4/5+; all up when nothing touches). Emitted every
    /// frame — libinput keys touchpad classification off these bits.
    fn emit_tool_bits(&mut self, present_count: usize) {
        self.uinput
            .emit(EV_KEY, BTN_TOOL_FINGER, i32::from(present_count == 1));
        self.uinput
            .emit(EV_KEY, BTN_TOOL_DOUBLETAP, i32::from(present_count == 2));
        self.uinput
            .emit(EV_KEY, BTN_TOOL_TRIPLETAP, i32::from(present_count == 3));
        self.uinput
            .emit(EV_KEY, BTN_TOOL_QUADTAP, i32::from(present_count == 4));
        self.uinput
            .emit(EV_KEY, BTN_TOOL_QUINTTAP, i32::from(present_count >= 5));
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

    /// Point at a (re-)resolved node. Never retargets a live grab: if
    /// grabbed, the grab is released first (with a log) so the old fd
    /// can never keep swallowing the previous node after the switch.
    pub fn set_node(&self, node: &str) {
        if self.is_grabbed() {
            log::warn!("grab target moved to '{node}' while grabbed; releasing grab first");
            self.ungrab();
        }
        *self.node.lock().unwrap_or_else(|e| e.into_inner()) = node.to_string();
    }

    /// Test hook: install an open fd as a live grab.
    #[cfg(test)]
    fn inject_open(&self, f: std::fs::File) {
        *self.fd.lock().unwrap_or_else(|e| e.into_inner()) = Some(f);
        self.grabbed
            .store(true, std::sync::atomic::Ordering::SeqCst);
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

/// uinput ioctl numbers (linux/uinput.h, `_IOW('U', nr, int)` etc).
/// Computed here so tests pin the exact kernel layout without headers.
pub const UINPUT_IOCTL_BASE: u8 = b'U';
const fn iow(nr: u8, size: usize) -> u64 {
    (1u64 << 30) | ((size as u64) << 16) | ((UINPUT_IOCTL_BASE as u64) << 8) | (nr as u64)
}
const fn io(nr: u8) -> u64 {
    ((UINPUT_IOCTL_BASE as u64) << 8) | (nr as u64)
}
pub fn ui_set_evbit() -> u64 {
    iow(100, 4)
}
pub fn ui_set_keybit() -> u64 {
    iow(101, 4)
}
pub fn ui_set_absbit() -> u64 {
    iow(103, 4)
}
/// `UI_SET_PROPBIT` = `_IOW('U', 110, int)` (carries `INPUT_PROP_POINTER`).
pub fn ui_set_propbit() -> u64 {
    iow(110, 4)
}
pub fn ui_dev_setup() -> u64 {
    iow(3, std::mem::size_of::<UinputSetup>())
}
pub fn ui_abs_setup() -> u64 {
    iow(4, std::mem::size_of::<UinputAbsSetup>())
}
pub fn ui_dev_create() -> u64 {
    io(1)
}
pub fn ui_dev_destroy() -> u64 {
    io(2)
}

/// Virtual device identities (proxy nodes, not hardware claims). The
/// pointer node keeps the established name; extras rides alongside it
/// (old-service `Magic Trackpad Extras` parity).
pub const UINPUT_NAME: &str = "Linux Trackpad Driver";
pub const UINPUT_EXTRAS_NAME: &str = "Linux Trackpad Driver Extras";
pub const UINPUT_BUSTYPE: u16 = 0x03;
pub const UINPUT_VENDOR: u16 = 0x05ac;
pub const UINPUT_PRODUCT: u16 = 0x0324;
pub const UINPUT_VERSION: u16 = 1;

/// Which of the two virtual nodes a [`RealUinput`] drives. The pointer
/// node is the touchpad (MT axes + 9 key bits + `POINTER` prop); extras
/// is the keyboard outlet (combo codes only, no axes, no prop).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UinputRole {
    Pointer,
    Extras,
}

/// Absolute axis ranges for the pointer node. Pressure/major/minor carry
/// the old-service normalized per-contact values (60/800/800, see
/// [`TOUCH_PRESSURE_NORM`]/[`TOUCH_SIZE_NORM`]) with `res == 0`
/// (arbitrary units, never grams/newtons); orientation emits `-(raw)`
/// so the range is the negated device range widened to ±32 (matches
/// old-service tolerance).
/// Positions use the old-service Trackpad2 dimensions WITH the
/// old-service physical density ([`POSITION_RES_X`]/[`POSITION_RES_Y`]):
/// libinput needs nonzero resolution to scale slot motion into cursor
/// motion. Touch major/minor mirror the old-service 0..1020 axes.
/// Deviations from old, documented: pressure range 0..253 (not 0..255,
/// the kernel `absmax`) and resolution fixed at the 1:1 density (old
/// varies it with speed; speed here applies once, in software).
pub fn uinput_abs_setup() -> Vec<(u16, i32, i32, i32)> {
    vec![
        (ABS_MT_SLOT, 0, 15, 0),
        (ABS_MT_TRACKING_ID, 0, 65535, 0),
        (ABS_MT_POSITION_X, -3678, 3934, POSITION_RES_X),
        (ABS_MT_POSITION_Y, -2478, 2587, POSITION_RES_Y),
        (ABS_MT_PRESSURE, 0, 253, 0),
        (ABS_MT_TOUCH_MAJOR, 0, 1020, 0),
        (ABS_MT_TOUCH_MINOR, 0, 1020, 0),
        (ABS_MT_ORIENTATION, -32, 32, 0),
    ]
}

/// Which virtual node an event belongs on. [`split_route`] is the single
/// routing decision for the two-node layout (old-service parity: mouse
/// buttons ride the pointer node, keyboard codes ride extras).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SplitRoute {
    Pointer,
    Extras,
    Both,
}

/// Key codes emitted on the POINTER node: exactly the old-service 9
/// (buttons + touch classification). No keyboard code may ever appear
/// here — the kernel classifies a node advertising `1..=255` as KEYBOARD
/// and libinput never drives a pointer from it (owner hardware 2026-10-16).
/// Pinned exact by test, including the regression pin (no `KEY_A..Z`).
pub const POINTER_KEYBITS: [u16; 9] = [
    BTN_LEFT,
    BTN_RIGHT,
    BTN_MIDDLE,
    BTN_TOOL_FINGER,
    BTN_TOOL_DOUBLETAP,
    BTN_TOOL_TRIPLETAP,
    BTN_TOOL_QUADTAP,
    BTN_TOOL_QUINTTAP,
    BTN_TOUCH,
];

/// Route one event to its node: `ABS` axes + the 9 pointer key codes go to
/// the pointer node, every other `KEY` code (combos, `back`/`forward`,
/// media, app keys) goes to extras, `SYN` fans out to both (each node is
/// independently framed). Event types other than these three are never
/// emitted; they default to extras.
pub fn split_route(ev_type: u16, code: u16) -> SplitRoute {
    match ev_type {
        EV_SYN => SplitRoute::Both,
        EV_ABS => SplitRoute::Pointer,
        EV_KEY if POINTER_KEYBITS.contains(&code) => SplitRoute::Pointer,
        _ => SplitRoute::Extras,
    }
}

/// Two-node emit handle: owns the pointer sink plus the extras sink and
/// routes every event via [`split_route`]. `is_ready` reflects the POINTER
/// node only — the touch path is the emit path grab safety depends on; an
/// extras-only failure degrades combos (dropped with a log at create time)
/// while motion/buttons keep working. `destroy` tears down BOTH nodes.
pub struct DualSink<P, E> {
    pointer: P,
    extras: E,
}

impl<P, E> DualSink<P, E> {
    pub fn new(pointer: P, extras: E) -> Self {
        Self { pointer, extras }
    }

    pub fn pointer(&self) -> &P {
        &self.pointer
    }

    pub fn extras(&self) -> &E {
        &self.extras
    }
}

impl<P: UinputSink, E: UinputSink> UinputSink for DualSink<P, E> {
    fn is_ready(&self) -> bool {
        self.pointer.is_ready()
    }

    fn emit(&self, ev_type: u16, code: u16, value: i32) {
        match split_route(ev_type, code) {
            SplitRoute::Pointer => self.pointer.emit(ev_type, code, value),
            SplitRoute::Extras => self.extras.emit(ev_type, code, value),
            SplitRoute::Both => {
                self.pointer.emit(ev_type, code, value);
                self.extras.emit(ev_type, code, value);
            }
        }
    }

    fn sync(&self) {
        self.pointer.sync();
        self.extras.sync();
    }

    fn destroy(&self) {
        self.pointer.destroy();
        self.extras.destroy();
    }
}

/// Key bits for the POINTER node: exactly the 9 classification-safe codes
/// ([`POINTER_KEYBITS`], sorted). Pinned exact by test.
pub fn pointer_keybits() -> Vec<u16> {
    let mut keys = POINTER_KEYBITS.to_vec();
    keys.sort_unstable();
    keys
}

/// Key bits for the EXTRAS node: every keyboard code the click maps can
/// emit (`1..=255` covers letters/digits/modifiers/nav/media/app/F-keys;
/// the four above-255 codes cover `back`/`forward`/`favorites`/`player`).
/// Pointer-routed buttons (`BTN_LEFT/RIGHT/MIDDLE`) and the touch
/// classification bits are deliberately absent — extras carries no
/// pointer traffic. `EV_FF` is never set (no 2026 consumer).
pub fn extras_keybits() -> Vec<u16> {
    let mut keys: Vec<u16> = (1u16..=255).collect();
    for extra in [BTN_SIDE, BTN_EXTRA, 364, 387] {
        if !keys.contains(&extra) {
            keys.push(extra);
        }
    }
    keys.sort_unstable();
    keys
}

#[repr(C)]
struct InputId {
    bustype: u16,
    vendor: u16,
    product: u16,
    version: u16,
}

#[repr(C)]
pub struct UinputSetup {
    id: InputId,
    name: [u8; 80],
    ff_effects_max: u32,
}

#[repr(C)]
struct InputAbsinfo {
    value: i32,
    minimum: i32,
    maximum: i32,
    fuzz: i32,
    flat: i32,
    resolution: i32,
}

#[repr(C)]
pub struct UinputAbsSetup {
    code: u16,
    filler: u16,
    absinfo: InputAbsinfo,
}

/// Best-effort uinput sink with real device creation (`UI_DEV_SETUP` +
/// `UI_SET_EVBIT/KEYBIT/ABSBIT` + `UI_ABS_SETUP` + `UI_DEV_CREATE`).
/// One instance drives ONE virtual node ([`UinputRole`]); the daemon owns
/// a pointer instance plus an extras instance (see [`DualSink`]).
/// The pointer instance is created BEFORE grab; [`UinputSink::is_ready`]
/// gates attach on it. Never sets `EV_FF` (see [`pointer_evbits`]).
/// Emits are `input_event` writes once the fd exists; pre-create emits
/// are debug-logged only.
pub struct RealUinput {
    node: String,
    role: UinputRole,
    fd: Mutex<Option<std::fs::File>>,
    created: std::sync::atomic::AtomicBool,
    /// Staged `input_event` bytes, flushed as one `write(2)` per `sync()`
    /// (one frame = one syscall). Pre-create emits are dropped with a
    /// debug log, never buffered: without a node there is no consumer.
    staging: Mutex<Vec<u8>>,
}

/// Serialize one `input_event` (zeroed timestamps). Layout pinned by
/// test: `sec(8) usec(8) type(2) code(2) value(4)`, native-endian.
fn encode_event(ev_type: u16, code: u16, value: i32) -> [u8; 24] {
    let mut b = [0u8; 24];
    b[16..18].copy_from_slice(&ev_type.to_ne_bytes());
    b[18..20].copy_from_slice(&code.to_ne_bytes());
    b[20..24].copy_from_slice(&value.to_ne_bytes());
    b
}

impl RealUinput {
    pub fn new(node: &str) -> Self {
        Self::with_role(node, UinputRole::Pointer)
    }

    /// Extras-node instance: keyboard outlet, no axes, no pointer prop.
    /// Shares the `/dev/uinput` path (one fd per virtual node).
    pub fn new_extras(node: &str) -> Self {
        Self::with_role(node, UinputRole::Extras)
    }

    fn with_role(node: &str, role: UinputRole) -> Self {
        Self {
            node: node.to_string(),
            role,
            fd: Mutex::new(None),
            created: std::sync::atomic::AtomicBool::new(false),
            staging: Mutex::new(Vec::new()),
        }
    }

    pub fn role(&self) -> UinputRole {
        self.role
    }

    pub fn is_created(&self) -> bool {
        self.created.load(std::sync::atomic::Ordering::SeqCst)
    }

    fn open_locked(
        guard: &mut Option<std::fs::File>,
        node: &str,
    ) -> Result<std::os::unix::io::RawFd> {
        use std::os::unix::io::AsRawFd;
        if guard.is_none() {
            let f = std::fs::OpenOptions::new()
                .write(true)
                .open(node)
                .map_err(|e| anyhow::anyhow!("opening uinput '{node}': {e}"))?;
            *guard = Some(f);
        }
        Ok(guard.as_ref().expect("just opened").as_raw_fd())
    }

    fn ioctl_ptr(fd: std::os::unix::io::RawFd, req: u64, arg: *mut libc::c_void) -> Result<()> {
        let ret = unsafe { libc::ioctl(fd, req as libc::c_ulong, arg) };
        if ret < 0 {
            let io = std::io::Error::last_os_error();
            return Err(anyhow::anyhow!(
                "uinput ioctl {req:#x} (struct ptr): {io}{}",
                Self::einval_hint(io.raw_os_error(), "UI_DEV_SETUP/UI_ABS_SETUP take a struct pointer; check struct size/layout vs linux/uinput.h")
            ));
        }
        Ok(())
    }

    /// `UI_SET_*BIT` take the bit number as an int VALUE (like the
    /// `evdev` crate's `ioctl_write_int!`), not a pointer. Passing
    /// `&val` makes the kernel read the pointer address as the bit
    /// number and fail the first ioctl (`UI_SET_EVBIT`) with `EINVAL`.
    fn ioctl_int(fd: std::os::unix::io::RawFd, req: u64, val: libc::c_int) -> Result<()> {
        let ret = unsafe { libc::ioctl(fd, req as libc::c_ulong, val) };
        if ret < 0 {
            let io = std::io::Error::last_os_error();
            return Err(anyhow::anyhow!(
                "uinput ioctl {req:#x} (int value {val}): {io}{}",
                Self::einval_hint(io.raw_os_error(), "UI_SET_*BIT take the bit number as int value, not a pointer; out-of-range bit or restricted /dev/uinput also give EINVAL")
            ));
        }
        Ok(())
    }

    fn einval_hint(errno: Option<i32>, likely: &str) -> String {
        if errno == Some(libc::EINVAL) {
            format!("; likely cause: {likely}")
        } else {
            String::new()
        }
    }
}

/// One step of the virtual-node create sequence, in order.
/// `Bit` steps must go through the int-value ioctl; struct steps
/// through the pointer ioctl (pinned by test).
#[derive(Debug, PartialEq)]
enum CreateOp {
    Bit(u64, libc::c_int),
    Abs(u16, i32, i32, i32),
    DevSetup,
    DevCreate,
}

impl RealUinput {
    /// Ordered create plan per role. Pointer: evbits → keybits →
    /// (absbit + abs-setup per axis) → propbit → dev-setup → dev-create.
    /// Matches the working `evdev` crate `VirtualDeviceBuilder` sequence
    /// (hardware-verified), plus `UI_SET_PROPBIT(INPUT_PROP_POINTER)` for
    /// the old-service pointer classification. Extras: evbits → keybits →
    /// dev-setup → dev-create (no axes, no prop — a pure keyboard outlet).
    /// A single-node failure must not break the other: the daemon creates
    /// each role independently and continues pointer-only when extras
    /// fails (and vice versa on teardown, where both destroys always run).
    fn create_plan_for(role: UinputRole) -> Vec<CreateOp> {
        let mut plan = Vec::new();
        let evbits = match role {
            UinputRole::Pointer => pointer_evbits(),
            UinputRole::Extras => extras_evbits(),
        };
        for ev in evbits {
            plan.push(CreateOp::Bit(ui_set_evbit(), ev as libc::c_int));
        }
        let keybits = match role {
            UinputRole::Pointer => pointer_keybits(),
            UinputRole::Extras => extras_keybits(),
        };
        for key in keybits {
            plan.push(CreateOp::Bit(ui_set_keybit(), key as libc::c_int));
        }
        if role == UinputRole::Pointer {
            for (code, mn, mx, res) in uinput_abs_setup() {
                plan.push(CreateOp::Bit(ui_set_absbit(), code as libc::c_int));
                plan.push(CreateOp::Abs(code, mn, mx, res));
            }
            plan.push(CreateOp::Bit(
                ui_set_propbit(),
                INPUT_PROP_POINTER as libc::c_int,
            ));
        }
        plan.push(CreateOp::DevSetup);
        plan.push(CreateOp::DevCreate);
        plan
    }

    fn run_plan(
        fd: std::os::unix::io::RawFd,
        plan: &[CreateOp],
        setup: &UinputSetup,
    ) -> Result<()> {
        for op in plan {
            match *op {
                CreateOp::Bit(req, val) => Self::ioctl_int(fd, req, val)?,
                CreateOp::Abs(code, mn, mx, res) => {
                    let abs = UinputAbsSetup {
                        code,
                        filler: 0,
                        absinfo: InputAbsinfo {
                            value: 0,
                            minimum: mn,
                            maximum: mx,
                            fuzz: 0,
                            flat: 0,
                            resolution: res,
                        },
                    };
                    Self::ioctl_ptr(
                        fd,
                        ui_abs_setup(),
                        (&abs as *const UinputAbsSetup).cast_mut().cast(),
                    )?;
                }
                CreateOp::DevSetup => Self::ioctl_ptr(
                    fd,
                    ui_dev_setup(),
                    (setup as *const UinputSetup).cast_mut().cast(),
                )?,
                CreateOp::DevCreate => Self::ioctl_ptr(fd, ui_dev_create(), std::ptr::null_mut())?,
            }
        }
        Ok(())
    }

    /// Create the virtual node. Idempotent; fails gracefully without
    /// hardware/`/dev/uinput` (caller logs + stays passthrough). A
    /// mid-sequence failure closes the fd instead of leaking a
    /// half-configured node: the next `create()` starts clean.
    pub fn create(&self) -> Result<()> {
        if self.is_created() {
            return Ok(());
        }
        let mut guard = self.fd.lock().unwrap_or_else(|e| e.into_inner());
        let fd = Self::open_locked(&mut guard, &self.node)?;
        let r = Self::create_on(fd, self.role);
        if r.is_err() {
            *guard = None;
        } else {
            self.created
                .store(true, std::sync::atomic::Ordering::SeqCst);
            let dev_name = match self.role {
                UinputRole::Pointer => UINPUT_NAME,
                UinputRole::Extras => UINPUT_EXTRAS_NAME,
            };
            log::info!("proxy: uinput node {} created ({dev_name})", self.node);
        }
        r
    }

    #[cfg(test)]
    pub(crate) fn fd_open(&self) -> bool {
        self.fd.lock().unwrap_or_else(|e| e.into_inner()).is_some()
    }

    fn create_on(fd: std::os::unix::io::RawFd, role: UinputRole) -> Result<()> {
        let mut setup = UinputSetup {
            id: InputId {
                bustype: UINPUT_BUSTYPE,
                vendor: UINPUT_VENDOR,
                product: UINPUT_PRODUCT,
                version: UINPUT_VERSION,
            },
            name: [0u8; 80],
            ff_effects_max: 0,
        };
        let dev_name = match role {
            UinputRole::Pointer => UINPUT_NAME,
            UinputRole::Extras => UINPUT_EXTRAS_NAME,
        };
        let name = dev_name.as_bytes();
        let n = name.len().min(79);
        setup.name[..n].copy_from_slice(&name[..n]);
        Self::run_plan(fd, &Self::create_plan_for(role), &setup)
    }

    /// Flush one staged buffer in a single write loop. A short actual
    /// (`ret != remaining`) without error keeps writing; any error or a
    /// zero return drops the remainder with a loud log instead of
    /// looping forever on a dead fd.
    fn flush_locked(fd: std::os::unix::io::RawFd, buf: &mut Vec<u8>) {
        let mut written = 0usize;
        while written < buf.len() {
            let ret =
                unsafe { libc::write(fd, buf[written..].as_ptr().cast(), buf.len() - written) };
            if ret < 0 {
                log::warn!(
                    "proxy: uinput write failed after {written}/{} bytes: {}",
                    buf.len(),
                    std::io::Error::last_os_error()
                );
                break;
            }
            if ret == 0 {
                log::warn!(
                    "proxy: uinput write returned 0 with {} bytes left; dropping",
                    buf.len() - written
                );
                break;
            }
            written += ret as usize;
        }
        if written != buf.len() {
            log::warn!(
                "proxy: uinput short write ({written}/{} bytes); remainder dropped",
                buf.len()
            );
        }
        buf.clear();
    }

    /// Lock order is always fd → staging (matches [`UinputSink::emit`]);
    /// never the reverse.
    fn flush(&self) {
        let guard = self.fd.lock().unwrap_or_else(|e| e.into_inner());
        let mut staging = self.staging.lock().unwrap_or_else(|e| e.into_inner());
        if staging.is_empty() {
            return;
        }
        let Some(f) = guard.as_ref() else {
            log::debug!("uinput flush with no fd; dropping {} bytes", staging.len());
            staging.clear();
            return;
        };
        use std::os::unix::io::AsRawFd;
        Self::flush_locked(f.as_raw_fd(), &mut staging);
    }

    /// Destroy the virtual node (detach path). Flushes staged events
    /// first so no key-up is lost, then releases grab-side state via caller.
    pub fn destroy(&self) {
        use std::os::unix::io::AsRawFd;
        self.flush();
        let mut guard = self.fd.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(f) = guard.as_ref() {
            unsafe {
                libc::ioctl(f.as_raw_fd(), ui_dev_destroy() as libc::c_ulong);
            }
        }
        *guard = None;
        self.created
            .store(false, std::sync::atomic::Ordering::SeqCst);
        log::info!("proxy: uinput node {} destroyed", self.node);
    }

    #[cfg(test)]
    pub(crate) fn staged_len(&self) -> usize {
        self.staging.lock().unwrap_or_else(|e| e.into_inner()).len()
    }
}

impl UinputSink for RealUinput {
    fn is_ready(&self) -> bool {
        self.is_created()
    }
    fn destroy(&self) {
        RealUinput::destroy(self);
    }
    fn emit(&self, ev_type: u16, code: u16, value: i32) {
        log::debug!("uinput emit: type={ev_type} code={code} value={value}");
        let guard = self.fd.lock().unwrap_or_else(|e| e.into_inner());
        if guard.is_none() {
            log::debug!("uinput emit pre-create; dropping event");
            return;
        }
        drop(guard);
        self.staging
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .extend_from_slice(&encode_event(ev_type, code, value));
    }

    /// Frame boundary: stage the trailing `SYN` and flush the whole
    /// frame in one write.
    fn sync(&self) {
        self.emit(EV_SYN, SYN_REPORT, 0);
        self.flush();
    }
}

/// Daemon-side create helpers for the two real nodes behind a
/// [`DualSink`]: each role is created (and may fail) independently, so a
/// single-node failure never breaks the other — the caller continues
/// pointer-only when extras fails, and teardown destroys both.
impl DualSink<RealUinput, RealUinput> {
    pub fn create_pointer(&self) -> Result<()> {
        self.pointer.create()
    }

    pub fn create_extras(&self) -> Result<()> {
        self.extras.create()
    }
}

/// Actuator report builder (reimplemented from observed old-service
/// behavior + kernel RE notes, no code copied).
///
/// 15-byte vibration report:
/// `[0xF2, 0x53, 0x01, pressure, amplitude, 0x02, intensity,
///   0x24, 0x30, 0x06, 0x01, pattern, 0x18, 0x48, 0x12]`
/// - `pressure` = the press threshold (`sensitivity`, old-service parity).
/// - `intensity` = click strength (`haptic` / force override, 0 = silent,
///   never sent).
/// - `amplitude` = `quiet ? 0xFF : 0x78`, `pattern` = `quiet ? 0 : 6`
///   (old-service parity: `120` / `6` normal, `255` / `0` quiet).
/// - Release up-pulse ([`vibration_up_report`]): same template at
///   `pressure = sensitivity*3/5`, `intensity = 0`, emitted on every
///   release whose down pulse fired (old-service parity: the actuator
///   needs the down+up pair for the click snap).
///
/// - Transport framing: old service sent full 15 B via hidapi feature
///   report on BT but stripped the leading `0xF2` on USB (14 B starting
///   `0x53`). We mirror that split over `HIDIOCSFEATURE`; whether the
///   kernel expects feature vs output (`write`) for `0x53` on this
///   firmware is unconfirmed — failures log + fall back to a plain
///   `write(2)` of the stripped bytes.
/// - Haptic node routing: the daemon resolves the interface-2 sibling
///   via [`crate::evdev::haptic_node_for`] (old-service actuator
///   interface); BT and unknown topologies fall back to the touch node
///   with a loud log — If2 placement is pending owner HW verify.
pub fn vibration_report(pressure: u8, intensity: u8, quiet: bool) -> [u8; 15] {
    let amplitude = if quiet { 0xFF } else { 0x78 };
    let pattern = if quiet { 0x00 } else { 0x06 };
    [
        0xF2, 0x53, 0x01, pressure, amplitude, 0x02, intensity, 0x24, 0x30, 0x06, 0x01, pattern,
        0x18, 0x48, 0x12,
    ]
}

/// Release up-pulse pressure (old-service parity): the down-pulse
/// threshold byte scaled lighter (`sensitivity*3/5`, `u16` math, no
/// overflow). Intensity is always 0 in the up report.
pub fn up_pressure(down_pressure: u8) -> u8 {
    (u16::from(down_pressure) * 3 / 5) as u8
}

/// Release up-pulse report (old-service `vibration_up` parity):
/// same template at `pressure = down*3/5`, `intensity = 0`, waveform
/// from `quiet` exactly like the down pulse.
pub fn vibration_up_report(down_pressure: u8, quiet: bool) -> [u8; 15] {
    vibration_report(up_pressure(down_pressure), 0, quiet)
}

/// Real haptic sender: fires the `0x53` actuator report per press edge
/// plus the release up-pulse per release edge (old-service down+up
/// pair). Down `intensity == 0` is silent (no bytes on the wire); the
/// up-pulse always sends (intensity 0 is its payload, not silence).
/// Empty node = log-only (daemon has not attached yet); never fails attach.
pub struct RealHaptic {
    node: Mutex<String>,
    bt: std::sync::atomic::AtomicBool,
}

impl RealHaptic {
    pub fn new() -> Self {
        Self {
            node: Mutex::new(String::new()),
            bt: std::sync::atomic::AtomicBool::new(false),
        }
    }

    pub fn set_node(&self, node: &str) {
        *self.node.lock().unwrap_or_else(|e| e.into_inner()) = node.to_string();
    }

    pub fn set_bluetooth(&self, bt: bool) {
        self.bt.store(bt, std::sync::atomic::Ordering::SeqCst);
    }
}

impl Default for RealHaptic {
    fn default() -> Self {
        Self::new()
    }
}

impl HapticSender for RealHaptic {
    fn click_feedback(&self, pressure: u8, intensity: u8, quiet: bool) -> Result<()> {
        if intensity == 0 {
            return Ok(());
        }
        let node = self.node.lock().unwrap_or_else(|e| e.into_inner()).clone();
        let bt = self.bt.load(std::sync::atomic::Ordering::SeqCst);
        let report = vibration_report(pressure, intensity, quiet);
        log::info!(
            "haptic feedback: pressure={pressure} intensity={intensity} quiet={quiet} (report 53, node={node} bt={bt})"
        );
        if node.is_empty() {
            return Ok(());
        }
        self.send_report(&node, bt, &report);
        Ok(())
    }
    fn release_feedback(&self, down_pressure: u8, quiet: bool) -> Result<()> {
        let node = self.node.lock().unwrap_or_else(|e| e.into_inner()).clone();
        let bt = self.bt.load(std::sync::atomic::Ordering::SeqCst);
        let report = vibration_up_report(down_pressure, quiet);
        log::info!(
            "haptic release: down_pressure={down_pressure} quiet={quiet} (report 53, node={node} bt={bt})"
        );
        if node.is_empty() {
            return Ok(());
        }
        self.send_report(&node, bt, &report);
        Ok(())
    }
}

impl RealHaptic {
    fn send_report(&self, node: &str, bt: bool, report: &[u8; 15]) {
        let payload: &[u8] = if bt { &report[..] } else { &report[1..] };
        // Strict send: EPIPE/EIO (device NAK) must propagate so the
        // fallback below runs. The lenient init variant would swallow
        // the NAK as success → silent no-haptic with no fallback.
        if let Err(e) = crate::init::send_feature_report_hidraw_strict(node, payload) {
            log::warn!("haptic feature report failed ({e:#}); trying plain write fallback");
            if let Err(e2) = plain_write(node, payload) {
                log::error!(
                    "haptic FAILED on {node} (feature: {e:#}; write: {e2:#}); clicks stay silent until re-attach"
                );
            }
        }
    }
}

fn plain_write(node: &str, bytes: &[u8]) -> Result<()> {
    use std::io::Write;
    let mut f = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(node)
        .map_err(|e| anyhow::anyhow!("opening haptic '{node}': {e}"))?;
    f.write_all(bytes)
        .map_err(|e| anyhow::anyhow!("haptic write to '{node}': {e}"))?;
    Ok(())
}

/// One actuator command for the haptic worker queue.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HapticCmd {
    Down {
        pressure: u8,
        intensity: u8,
        quiet: bool,
    },
    Up {
        down_pressure: u8,
        quiet: bool,
    },
}

/// Async actuator sender: the pump thread enqueues down/up pulses and
/// never waits for the firmware round-trip.
///
/// Why: every [`HapticSender`] call opens the hidraw node and issues a
/// blocking `HIDIOCSFEATURE` (+ a fallback open+write on NAK). On the
/// pump thread that steals frame time per click edge; during
/// release/re-press chatter (noisy pressure hovering at the release
/// point mid-drag) dozens of edges × ~10 ms round-trips stall motion in
/// 150-800 ms bursts. The worker drains a FIFO channel instead, so input
/// latency never depends on actuator timing. Order is preserved
/// (down/up pairs stay paired); bytes, node selection, and silent rules
/// are unchanged (the inner sender is untouched).
pub struct AsyncHaptic<H> {
    inner: std::sync::Arc<H>,
    tx: Option<std::sync::mpsc::Sender<HapticCmd>>,
    worker: Option<std::thread::JoinHandle<()>>,
}

impl<H> AsyncHaptic<H>
where
    H: HapticSender + Send + Sync + 'static,
{
    pub fn new(inner: H) -> Self {
        let inner = std::sync::Arc::new(inner);
        let (tx, rx) = std::sync::mpsc::channel::<HapticCmd>();
        let worker_inner = std::sync::Arc::clone(&inner);
        let worker = std::thread::Builder::new()
            .name("ltd-haptic".into())
            .spawn(move || {
                for cmd in rx {
                    match cmd {
                        HapticCmd::Down {
                            pressure,
                            intensity,
                            quiet,
                        } => {
                            if let Err(e) = worker_inner.click_feedback(pressure, intensity, quiet)
                            {
                                log::warn!("proxy: async haptic down failed: {e:#}");
                            }
                        }
                        HapticCmd::Up {
                            down_pressure,
                            quiet,
                        } => {
                            if let Err(e) = worker_inner.release_feedback(down_pressure, quiet) {
                                log::warn!("proxy: async haptic up failed: {e:#}");
                            }
                        }
                    }
                }
                log::debug!("proxy: haptic worker drained, exiting");
            })
            .expect("haptic worker thread spawn");
        Self {
            inner,
            tx: Some(tx),
            worker: Some(worker),
        }
    }

    /// Borrow the inner sender (daemon points [`RealHaptic`] at its node).
    pub fn inner(&self) -> &H {
        &self.inner
    }
}

impl<H> Drop for AsyncHaptic<H> {
    fn drop(&mut self) {
        // Disconnect first so the worker drains the queued trailing
        // up-pulses, then join (bounded: the queue only holds click edges).
        self.tx.take();
        if let Some(w) = self.worker.take() {
            let _ = w.join();
        }
    }
}

impl<H> HapticSender for AsyncHaptic<H>
where
    H: HapticSender + Send + Sync,
{
    fn click_feedback(&self, pressure: u8, intensity: u8, quiet: bool) -> Result<()> {
        let cmd = HapticCmd::Down {
            pressure,
            intensity,
            quiet,
        };
        match self.tx.as_ref().map(|tx| tx.send(cmd)) {
            Some(Ok(())) => Ok(()),
            _ => {
                // Worker gone (only via a failed spawn, which already
                // panicked, or drop racing a call): degrade to a direct
                // synchronous send rather than losing the pulse.
                log::warn!("proxy: haptic worker gone, sending directly");
                self.inner.click_feedback(pressure, intensity, quiet)
            }
        }
    }
    fn release_feedback(&self, down_pressure: u8, quiet: bool) -> Result<()> {
        let cmd = HapticCmd::Up {
            down_pressure,
            quiet,
        };
        match self.tx.as_ref().map(|tx| tx.send(cmd)) {
            Some(Ok(())) => Ok(()),
            _ => {
                log::warn!("proxy: haptic worker gone, sending directly");
                self.inner.release_feedback(down_pressure, quiet)
            }
        }
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
        destroyed: Mutex<bool>,
    }

    impl FakeUinput {
        fn new() -> Self {
            Self {
                events: Mutex::new(Vec::new()),
                destroyed: Mutex::new(false),
            }
        }
        fn has(&self, ty: u16, code: u16, val: i32) -> bool {
            self.events.lock().unwrap().contains(&(ty, code, val))
        }
        fn was_destroyed(&self) -> bool {
            *self.destroyed.lock().unwrap()
        }
    }

    impl UinputSink for FakeUinput {
        fn emit(&self, ev_type: u16, code: u16, value: i32) {
            self.events.lock().unwrap().push((ev_type, code, value));
        }
        fn destroy(&self) {
            *self.destroyed.lock().unwrap() = true;
        }
    }

    struct FakeHaptic {
        calls: Mutex<Vec<(u8, u8, bool)>>,
        releases: Mutex<Vec<(u8, bool)>>,
    }

    impl FakeHaptic {
        fn new() -> Self {
            Self {
                calls: Mutex::new(Vec::new()),
                releases: Mutex::new(Vec::new()),
            }
        }
    }

    impl HapticSender for FakeHaptic {
        fn click_feedback(&self, pressure: u8, intensity: u8, quiet: bool) -> Result<()> {
            self.calls
                .lock()
                .unwrap()
                .push((pressure, intensity, quiet));
            Ok(())
        }
        fn release_feedback(&self, down_pressure: u8, quiet: bool) -> Result<()> {
            self.releases.lock().unwrap().push((down_pressure, quiet));
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
    fn disconnect_self_releases_once_with_bounded_polls() {
        // Same-name replug: the fd is dead (0-byte/ENODEV) but no
        // Detached will ever bound the errors. The proxy must release
        // exactly once (grab, keys, SYN, latch, pump flag) and then
        // stop polling the dead source — bounded logs, session
        // revivable on fresh wiring.
        use anyhow::Error as AnyhowError;
        let grab = Arc::new(FakeGrab::ok());
        let sinks = TestSinks::new();
        let disc = || {
            Err::<Option<TouchFrame>, AnyhowError>(
                crate::pump::DisconnectError("test unplug".into()).into(),
            )
        };
        let frames = vec![
            Ok(Some(frame(vec![finger(1, 200, 0, 0)], 0))),
            disc(),
            disc(),
            disc(),
        ];
        let mut px = ClickProxy::new(
            settings(true),
            SharedGrab(Arc::clone(&grab)),
            sinks.dual(),
            FakeHaptic::new(),
            ScriptSource::new(frames),
        );
        px.set_pump_ready(true);
        px.attach(&usb_ready("/dev/hidraw8"));
        let cfg = Config::default();
        px.handle_frame(&cfg).unwrap(); // press → BTN_LEFT down
        assert_eq!(key_values(&sinks.pointer, BTN_LEFT), vec![1]);
        px.handle_frame(&cfg).unwrap(); // disconnect → self-release
        assert!(!grab.is_grabbed(), "dead fd must release the grab");
        assert!(!px.is_attached());
        assert!(px.is_passthrough());
        assert_eq!(
            key_values(&sinks.pointer, BTN_LEFT),
            vec![1, 0],
            "held keys go up exactly once"
        );
        // Pump marked unready: a bare re-attach stays passthrough.
        px.attach(&usb_ready("/dev/hidraw8"));
        assert!(!px.is_attached());
        // Dead source never polled again (logs bounded by construction).
        let remaining = px.source.frames.borrow().len();
        px.handle_frame(&cfg).unwrap();
        px.handle_frame(&cfg).unwrap();
        assert_eq!(px.source.frames.borrow().len(), remaining);
        // Fresh wiring revives the session on the same node.
        px.set_pump_ready(true);
        px.attach(&usb_ready("/dev/hidraw8"));
        assert!(px.is_attached());
        assert!(grab.is_grabbed());
    }

    #[test]
    fn sibling_detached_never_kills_live_grab() {
        // Live session on hidraw8; a sibling interface (hidraw10) detaching
        // must not release the grab, flip passthrough, or emit key-ups.
        let grab = Arc::new(FakeGrab::ok());
        let sinks = TestSinks::new();
        let mut px = ClickProxy::new(
            settings(true),
            SharedGrab(Arc::clone(&grab)),
            sinks.dual(),
            FakeHaptic::new(),
            ScriptSource::new(vec![]),
        );
        px.set_pump_ready(true);
        px.attach(&usb_ready("/dev/hidraw8"));
        assert!(px.is_attached());
        assert!(grab.is_grabbed());
        let events_before =
            sinks.pointer.events.lock().unwrap().len() + sinks.extras.events.lock().unwrap().len();
        px.detach(&detached("/dev/hidraw10", Transport::Usb));
        assert!(px.is_attached(), "sibling Detached released the live grab");
        assert!(!px.is_passthrough());
        assert!(grab.is_grabbed(), "sibling Detached dropped the evdev grab");
        assert_eq!(
            sinks.pointer.events.lock().unwrap().len() + sinks.extras.events.lock().unwrap().len(),
            events_before,
            "sibling Detached emitted teardown traffic"
        );
        // The owning node detaching still tears down.
        px.detach(&detached("/dev/hidraw8", Transport::Usb));
        assert!(!px.is_attached());
        assert!(px.is_passthrough());
        assert!(!grab.is_grabbed());
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
        fn destroy(&self) {
            self.0.destroy();
        }
    }

    struct SharedHaptic(Arc<FakeHaptic>);

    impl HapticSender for SharedHaptic {
        fn click_feedback(&self, pressure: u8, intensity: u8, quiet: bool) -> Result<()> {
            self.0.click_feedback(pressure, intensity, quiet)
        }
        fn release_feedback(&self, down_pressure: u8, quiet: bool) -> Result<()> {
            self.0.release_feedback(down_pressure, quiet)
        }
    }

    #[test]
    fn pressure_normalized_orientation_negated_on_wire() {
        assert_eq!(crate::apple_parse::PRESSURE_VALID_MAX, 253);
        let sinks = TestSinks::new();
        let haptic = Arc::new(FakeHaptic::new());
        let frames = vec![Ok(Some(frame(vec![finger(5, 255, 10, 20)], 0)))];
        let mut px = drive_ready_proxy(&sinks, &haptic, frames);
        let cfg = Config::default();
        let ev = px.handle_frame(&cfg).unwrap().unwrap();
        assert_eq!(ev.finger_count, 1);
        // Raw 255 still wires old-parity 60 (normalized, never raw);
        // raw orientation 2 negates to -2.
        assert!(sinks.pointer.has(EV_ABS, ABS_MT_PRESSURE, 60));
        assert!(sinks.pointer.has(EV_ABS, ABS_MT_ORIENTATION, -2));
        assert!(sinks.pointer.has(EV_ABS, ABS_MT_TRACKING_ID, 5));
        assert!(sinks.pointer.has(EV_KEY, BTN_TOUCH, 1));
        // click_1 = left → BTN_LEFT down; haptic fired (default 5).
        assert!(sinks.pointer.has(EV_KEY, BTN_LEFT, 1));
        assert_eq!(haptic.calls.lock().unwrap().as_slice(), &[(64, 5, false)]);
    }

    #[test]
    fn silent_haptic_sends_no_feedback() {
        let sinks = TestSinks::new();
        let haptic = Arc::new(FakeHaptic::new());
        let frames = vec![Ok(Some(frame(vec![finger(1, 200, 0, 0)], 0)))];
        let mut px = drive_ready_proxy(&sinks, &haptic, frames);
        let cfg = Config {
            haptic: 0,
            ..Config::default()
        };
        px.handle_frame(&cfg).unwrap();
        assert!(haptic.calls.lock().unwrap().is_empty());
        // Button still emits; only the haptic is silent.
        assert!(sinks.pointer.has(EV_KEY, BTN_LEFT, 1));
    }

    #[test]
    fn none_action_no_haptic_no_extra_keys() {
        let sinks = TestSinks::new();
        let haptic = Arc::new(FakeHaptic::new());
        let frames = vec![Ok(Some(frame(vec![finger(1, 200, 0, 0)], 0)))];
        let mut px = drive_ready_proxy(&sinks, &haptic, frames);
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
        let sinks = TestSinks::new();
        let haptic = Arc::new(FakeHaptic::new());
        let frames = vec![
            Ok(Some(frame(vec![finger(1, 10, 0, 0)], 0))),
            Ok(Some(frame(vec![finger(1, 10, 100, 0)], 0))),
        ];
        let mut px = drive_ready_proxy(&sinks, &haptic, frames);
        // Speed is a single source: the per-frame Config, not a snapshot.
        let cfg = Config {
            speed: 200,
            ..Config::default()
        };
        px.handle_frame(&cfg).unwrap();
        px.handle_frame(&cfg).unwrap();
        // Second frame: dx=100 @200% → emitted x=200.
        assert!(sinks.pointer.has(EV_ABS, ABS_MT_POSITION_X, 200));
    }

    #[test]
    fn no_ff_in_capabilities() {
        assert!(!pointer_evbits().contains(&EV_FF));
        assert!(!extras_evbits().contains(&EV_FF));
    }

    #[test]
    fn eviocgrab_request_pins_linux_layout() {
        // _IOW('E', 0x90, int): 1<<30 | sizeof(int)<<16 | 'E'<<8 | 0x90.
        assert_eq!(eviocgrab_request(), 0x4004_4590);
    }

    #[test]
    fn uinput_ioctls_pin_kernel_layout() {
        assert_eq!(ui_set_evbit(), 0x4004_5564);
        assert_eq!(ui_set_keybit(), 0x4004_5565);
        assert_eq!(ui_set_absbit(), 0x4004_5567);
        assert_eq!(ui_set_propbit(), 0x4004_556e);
        assert_eq!(ui_dev_create(), 0x5501);
        assert_eq!(ui_dev_destroy(), 0x5502);
        // SETUP sizes ride the struct layouts (92 + 28 on 64-bit Linux).
        assert_eq!(ui_dev_setup() & 0xffff, 0x5503);
        assert_eq!(ui_abs_setup() & 0xffff, 0x5504);
    }

    #[test]
    fn uinput_setup_is_type_b_pointer_no_ff() {
        // Cursor-motion contract (old-service parity): the classification
        // bits + touch axes + pointer density libinput needs. `EV_FF`
        // stays absent (no 2026 consumer) — extended, never weakened.
        let abs = uinput_abs_setup();
        let get = |code: u16| abs.iter().find(|(c, _, _, _)| *c == code).unwrap();
        assert_eq!(get(ABS_MT_SLOT), &(ABS_MT_SLOT, 0, 15, 0));
        assert_eq!(get(ABS_MT_PRESSURE), &(ABS_MT_PRESSURE, 0, 253, 0));
        assert_eq!(get(ABS_MT_TOUCH_MAJOR), &(ABS_MT_TOUCH_MAJOR, 0, 1020, 0));
        assert_eq!(get(ABS_MT_TOUCH_MINOR), &(ABS_MT_TOUCH_MINOR, 0, 1020, 0));
        // Physical density: libinput scales slot motion by this.
        assert_eq!(
            get(ABS_MT_POSITION_X),
            &(ABS_MT_POSITION_X, -3678, 3934, POSITION_RES_X)
        );
        assert_eq!(
            get(ABS_MT_POSITION_Y),
            &(ABS_MT_POSITION_Y, -2478, 2587, POSITION_RES_Y)
        );
        assert!(get(ABS_MT_ORIENTATION).2 >= 3);
        // Pointer node: EXACTLY the 9 classification-safe key bits
        // (old-service parity). Any keyboard code here reclassifies the
        // node as KEYBOARD and kills the cursor (owner hardware 2026-10-16).
        let mut want = vec![
            BTN_LEFT,
            BTN_RIGHT,
            BTN_MIDDLE,
            BTN_TOUCH,
            BTN_TOOL_FINGER,
            BTN_TOOL_DOUBLETAP,
            BTN_TOOL_TRIPLETAP,
            BTN_TOOL_QUADTAP,
            BTN_TOOL_QUINTTAP,
        ];
        want.sort_unstable();
        assert_eq!(pointer_keybits(), want);
        // Regression pin: no keyboard-range code on the pointer node —
        // not letters, not digits, not modifiers, not the `back`/`forward`
        // mouse buttons (those ride extras like the old driver).
        for code in 0u16..=255 {
            assert!(
                !pointer_keybits().contains(&code),
                "keyboard code {code} on pointer node (KEYBOARD reclassification)"
            );
        }
        for spot in [
            KEY_A, KEY_Z, KEY_0, KEY_SPACE, KEY_ENTER, BTN_SIDE, BTN_EXTRA,
        ] {
            assert!(
                !pointer_keybits().contains(&spot),
                "code {spot} must not be on the pointer node"
            );
        }
        assert!(!pointer_evbits().contains(&EV_FF));
        // Extras node: keyboard outlet — SYN/KEY only, no ABS evbit, and
        // its key set carries every combo code while excluding the pointer
        // node's touch classification bits.
        assert_eq!(extras_evbits(), vec![EV_SYN, EV_KEY]);
        let extras = extras_keybits();
        for code in [BTN_TOUCH, BTN_TOOL_FINGER, BTN_TOOL_QUINTTAP] {
            assert!(
                !extras.contains(&code),
                "pointer code {code} must not be on the extras node"
            );
        }
        for code in [BTN_SIDE, BTN_EXTRA, 364, 387] {
            assert!(extras.contains(&code), "missing extras keybit {code}");
        }
    }

    /// Spot key codes for the regression pin above (linux/input-event-codes.h).
    const KEY_A: u16 = 30;
    const KEY_Z: u16 = 44;
    const KEY_0: u16 = 11;
    const KEY_SPACE: u16 = 57;
    const KEY_ENTER: u16 = 28;

    #[test]
    fn extras_keybits_cover_every_combo_code() {
        // Every code `key_token_to_code` can produce must be advertisable
        // on exactly one node: pointer-routed buttons on the pointer node,
        // everything else on extras. Candidate universe mirrors the KEYS.md
        // drift test (config tokens + printable singles + f0..=f99) so a
        // newly mapped token breaks here until its node covers it.
        const CONFIG_SRC: &str = include_str!("config.rs");
        let marker = "fn is_known_key";
        let start = CONFIG_SRC.find(marker).expect("is_known_key missing");
        let tail = &CONFIG_SRC[start..];
        let end = tail.find("\n}\n").expect("is_known_key end missing");
        let mut candidates: Vec<String> = Vec::new();
        for chunk in tail[..end].split('"').skip(1).step_by(2) {
            if !chunk.is_empty() {
                candidates.push(chunk.to_string());
            }
        }
        for b in 0x20u8..=0x7Eu8 {
            candidates.push((b as char).to_string());
        }
        for n in 0..=99 {
            candidates.push(format!("f{n}"));
        }
        let pointer = pointer_keybits();
        let extras = extras_keybits();
        let mut covered = 0;
        for cand in &candidates {
            let Some((ty, code)) = key_token_to_code(cand) else {
                continue;
            };
            assert_eq!(ty, EV_KEY, "non-EV_KEY mapping for '{cand}'");
            assert!(
                pointer.contains(&code) || extras.contains(&code),
                "token '{cand}' maps to {code}, advertised on neither node"
            );
            covered += 1;
        }
        assert!(covered > 100, "sweep too small to mean anything: {covered}");
    }

    struct GatedUinput {
        inner: FakeUinput,
        ready: std::sync::atomic::AtomicBool,
    }

    impl GatedUinput {
        fn new(ready: bool) -> Self {
            Self {
                inner: FakeUinput::new(),
                ready: std::sync::atomic::AtomicBool::new(ready),
            }
        }
    }

    impl UinputSink for GatedUinput {
        fn is_ready(&self) -> bool {
            self.ready.load(std::sync::atomic::Ordering::SeqCst)
        }
        fn emit(&self, ty: u16, code: u16, val: i32) {
            self.inner.emit(ty, code, val);
        }
    }

    #[test]
    fn grab_waits_for_sink_ready() {
        let grab = Arc::new(FakeGrab::ok());
        let sink = GatedUinput::new(false);
        let mut px = ClickProxy::new(
            settings(true),
            SharedGrab(Arc::clone(&grab)),
            sink,
            FakeHaptic::new(),
            ScriptSource::new(vec![]),
        );
        px.set_pump_ready(true);
        // Sink not created yet: no grab even though pump is ready.
        px.attach(&usb_ready("/dev/hidraw1"));
        assert!(!px.is_attached());
        assert!(px.is_passthrough());
        assert!(!grab.is_grabbed());
    }

    #[test]
    fn real_uinput_create_fails_gracefully_without_node() {
        let u = RealUinput::new("/nonexistent-uinput-xyz");
        assert!(!u.is_created());
        assert!(!u.is_ready());
        assert!(u.create().is_err());
        assert!(!u.is_created());
    }

    #[test]
    fn uinput_create_plan_pins_working_sequence() {
        // Hardware-verified on kernel 7.2.9-arch1-1: UI_SET_*BIT take
        // the bit as int VALUE (evdev crate `ioctl_write_int!`); a
        // pointer arg makes the first ioctl fail with EINVAL.
        assert_eq!(ui_dev_setup(), 0x405c_5503); // _IOW('U',3,uinput_setup=92)
        assert_eq!(ui_abs_setup(), 0x401c_5504); // _IOW('U',4,uinput_abs_setup=28)
        let plan = RealUinput::create_plan_for(UinputRole::Pointer);
        let mut i = 0;
        for ev in pointer_evbits() {
            assert_eq!(
                plan[i],
                CreateOp::Bit(ui_set_evbit(), ev as libc::c_int),
                "evbit #{i}"
            );
            i += 1;
        }
        for key in pointer_keybits() {
            assert_eq!(
                plan[i],
                CreateOp::Bit(ui_set_keybit(), key as libc::c_int),
                "keybit #{i}"
            );
            i += 1;
        }
        for (code, mn, mx, res) in uinput_abs_setup() {
            assert_eq!(
                plan[i],
                CreateOp::Bit(ui_set_absbit(), code as libc::c_int),
                "absbit #{i}"
            );
            i += 1;
            assert_eq!(plan[i], CreateOp::Abs(code, mn, mx, res), "abs-setup #{i}");
            i += 1;
        }
        // Pointer classification rides between the axes and dev-setup
        // (evdev VirtualDeviceBuilder order).
        assert_eq!(
            plan[i],
            CreateOp::Bit(ui_set_propbit(), INPUT_PROP_POINTER as libc::c_int),
            "propbit INPUT_PROP_POINTER"
        );
        i += 1;
        assert_eq!(plan[i], CreateOp::DevSetup);
        assert_eq!(plan[i + 1], CreateOp::DevCreate);
        assert_eq!(plan.len(), i + 2);
    }

    #[test]
    fn extras_create_plan_has_no_abs_no_prop() {
        // The extras node is a pure keyboard outlet: evbits → keybits →
        // dev-setup → dev-create. No ABS bit/setup ops (it has no axes)
        // and no pointer propbit — pinned so a future axis/prop addition
        // fails here first.
        let plan = RealUinput::create_plan_for(UinputRole::Extras);
        let mut i = 0;
        for ev in extras_evbits() {
            assert_eq!(
                plan[i],
                CreateOp::Bit(ui_set_evbit(), ev as libc::c_int),
                "evbit #{i}"
            );
            i += 1;
        }
        for key in extras_keybits() {
            assert_eq!(
                plan[i],
                CreateOp::Bit(ui_set_keybit(), key as libc::c_int),
                "keybit #{i}"
            );
            i += 1;
        }
        assert_eq!(plan[i], CreateOp::DevSetup, "extras must skip ABS setup");
        assert_eq!(plan[i + 1], CreateOp::DevCreate);
        assert_eq!(plan.len(), i + 2);
        assert!(
            !plan.iter().any(|op| matches!(op, CreateOp::Abs(..)))
                && !plan.iter().any(|op| matches!(
                    op,
                    CreateOp::Bit(req, _) if *req == ui_set_absbit() || *req == ui_set_propbit()
                )),
            "extras plan must contain no ABS/prop ops: {plan:?}"
        );
    }

    #[test]
    fn split_route_pins_node_assignment() {
        // MT axes + the 9 pointer codes → pointer node; SYN → both;
        // every other KEY (letters, modifiers, media, back/forward, app
        // keys) → extras node.
        assert_eq!(split_route(EV_ABS, ABS_MT_SLOT), SplitRoute::Pointer);
        assert_eq!(split_route(EV_ABS, ABS_MT_POSITION_X), SplitRoute::Pointer);
        assert_eq!(split_route(EV_ABS, ABS_MT_TRACKING_ID), SplitRoute::Pointer);
        for code in POINTER_KEYBITS {
            assert_eq!(
                split_route(EV_KEY, code),
                SplitRoute::Pointer,
                "code {code}"
            );
        }
        assert_eq!(split_route(EV_SYN, SYN_REPORT), SplitRoute::Both);
        for code in [
            30u16, // KEY_A
            46,    // KEY_C
            29,    // KEY_LEFTCTRL
            125,   // KEY_LEFTMETA
            158, 159, // browser back/forward
            163, 173, // media/refresh
            275, 276, // back/forward mouse buttons (old-driver parity: extras)
            364, 387, // favorites/player
        ] {
            assert_eq!(split_route(EV_KEY, code), SplitRoute::Extras, "code {code}");
        }
    }

    #[test]
    fn dual_sink_fans_out_per_node() {
        let ptr = Arc::new(FakeUinput::new());
        let ext = Arc::new(FakeUinput::new());
        let sink = DualSink::new(
            SharedUinput(Arc::clone(&ptr)),
            SharedUinput(Arc::clone(&ext)),
        );
        // Touch axis + button → pointer only; combo key → extras only.
        sink.emit(EV_ABS, ABS_MT_SLOT, 0);
        sink.emit(EV_ABS, ABS_MT_TRACKING_ID, 1);
        sink.emit(EV_KEY, BTN_LEFT, 1);
        sink.emit(EV_KEY, 46, 1); // KEY_C
        sink.emit(EV_KEY, 29, 1); // KEY_LEFTCTRL
        sink.sync(); // trailing SYN lands on BOTH nodes (each framed)
        let p = ptr.events.lock().unwrap().clone();
        let e = ext.events.lock().unwrap().clone();
        assert_eq!(
            p,
            vec![
                (EV_ABS, ABS_MT_SLOT, 0),
                (EV_ABS, ABS_MT_TRACKING_ID, 1),
                (EV_KEY, BTN_LEFT, 1),
                (EV_SYN, SYN_REPORT, 0),
            ],
            "pointer stream"
        );
        assert_eq!(
            e,
            vec![(EV_KEY, 46, 1), (EV_KEY, 29, 1), (EV_SYN, SYN_REPORT, 0),],
            "extras stream"
        );
        // Extras never carries axes or touch classification bits.
        assert!(!e.iter().any(|&(t, _, _)| t == EV_ABS), "{e:?}");
        for tool in [
            BTN_TOUCH,
            BTN_TOOL_FINGER,
            BTN_TOOL_DOUBLETAP,
            BTN_TOOL_TRIPLETAP,
            BTN_TOOL_QUADTAP,
            BTN_TOOL_QUINTTAP,
        ] {
            assert!(
                !e.contains(&(EV_KEY, tool, 0)) && !e.contains(&(EV_KEY, tool, 1)),
                "{e:?}"
            );
        }
    }

    #[test]
    fn dual_sink_teardown_destroys_both() {
        // Teardown must destroy BOTH nodes (no leaks): even the sibling
        // of a failed create is always attempted (destroy is idempotent).
        let ptr = Arc::new(FakeUinput::new());
        let ext = Arc::new(FakeUinput::new());
        let sink = DualSink::new(
            SharedUinput(Arc::clone(&ptr)),
            SharedUinput(Arc::clone(&ext)),
        );
        sink.destroy();
        assert!(ptr.was_destroyed(), "pointer node leaked");
        assert!(ext.was_destroyed(), "extras node leaked");
    }

    #[test]
    fn dual_sink_ready_reflects_pointer_only() {
        // Grab safety depends on the touch emit path: pointer-unready
        // blocks attach even with extras ready; extras-unready never
        // blocks a working pointer session (single-node failure isolation).
        let gated_off = GatedUinput::new(false);
        let sink = DualSink::new(gated_off, FakeUinput::new());
        assert!(!sink.is_ready());
        let sink = DualSink::new(GatedUinput::new(true), GatedUinput::new(false));
        assert!(sink.is_ready());
    }

    #[test]
    fn uinput_einval_error_names_likely_cause() {
        let hint = RealUinput::einval_hint(Some(libc::EINVAL), "int value, not a pointer");
        assert!(hint.contains("likely cause"), "hint was: {hint}");
        assert!(RealUinput::einval_hint(Some(libc::ENOMEM), "x").is_empty());
        assert!(RealUinput::einval_hint(None, "x").is_empty());
    }

    #[test]
    fn real_uinput_create_e2e_on_hardware() {
        // Live check: skipped (Ok) where /dev/uinput is absent (CI).
        if std::fs::OpenOptions::new()
            .write(true)
            .open("/dev/uinput")
            .is_err()
        {
            return;
        }
        let u = RealUinput::new("/dev/uinput");
        u.create().expect("create on /dev/uinput");
        assert!(u.is_created());
        u.destroy();
        assert!(!u.is_created());
    }

    #[test]
    fn failed_create_leaves_no_half_configured_fd() {
        // A regular file opens fine but NAKs the first ioctl: the fd
        // must be closed again, not leaked as a half-configured node.
        let dir = tempfile::tempdir().unwrap();
        let fake = dir.path().join("uinput");
        std::fs::write(&fake, []).unwrap();
        let u = RealUinput::new(fake.to_str().unwrap());
        assert!(u.create().is_err());
        assert!(!u.is_created());
        assert!(!u.fd_open());
    }

    #[test]
    fn set_node_releases_live_grab_first() {
        let g = RealGrab::new("/dev/null");
        let f = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open("/dev/null")
            .unwrap();
        g.inject_open(f);
        assert!(g.is_grabbed());
        g.set_node("/dev/input/event13");
        assert!(!g.is_grabbed(), "retarget must release the old grab");
    }

    #[test]
    fn encode_event_pins_input_event_layout() {
        // sec(8) usec(8) type(2) code(2) value(4), native-endian.
        let b = encode_event(EV_KEY, BTN_LEFT, 1);
        assert_eq!(b.len(), 24);
        assert_eq!(&b[..16], &[0u8; 16]);
        assert_eq!(u16::from_ne_bytes([b[16], b[17]]), EV_KEY);
        assert_eq!(u16::from_ne_bytes([b[18], b[19]]), BTN_LEFT);
        assert_eq!(i32::from_ne_bytes([b[20], b[21], b[22], b[23]]), 1);
    }

    #[test]
    fn pre_create_emits_are_dropped_never_buffered() {
        let u = RealUinput::new("/nonexistent-uinput-xyz");
        u.emit(EV_KEY, BTN_LEFT, 1);
        u.emit(EV_ABS, ABS_MT_SLOT, 0);
        u.sync();
        assert_eq!(u.staged_len(), 0);
    }

    #[test]
    fn vibration_report_pins_actuator_bytes() {
        // Old-service mapping: pressure = press threshold, intensity =
        // click strength (decoupled — never the strength echoed as
        // pressure). Amplitude/pattern carry the quiet waveform.
        assert_eq!(
            vibration_report(64, 5, false),
            [0xF2, 0x53, 0x01, 64, 0x78, 0x02, 5, 0x24, 0x30, 0x06, 0x01, 0x06, 0x18, 0x48, 0x12]
        );
        assert_eq!(
            vibration_report(64, 9, true),
            [0xF2, 0x53, 0x01, 64, 0xFF, 0x02, 9, 0x24, 0x30, 0x06, 0x01, 0x00, 0x18, 0x48, 0x12]
        );
    }

    #[test]
    fn silent_haptic_sends_nothing() {
        let h = RealHaptic::new();
        h.set_node("/nonexistent-haptic-xyz");
        // intensity 0: silent, Ok, no I/O attempted (would err otherwise).
        assert!(h.click_feedback(64, 0, false).is_ok());
        assert!(h.click_feedback(64, 0, true).is_ok());
    }

    #[test]
    fn haptic_without_node_is_log_only_ok() {
        let h = RealHaptic::new();
        assert!(h.click_feedback(64, 5, false).is_ok());
    }

    #[test]
    fn haptic_nak_runs_write_fallback_with_exact_bytes() {
        // A regular file stands in for the hidraw node: the strict
        // feature ioctl NAKs (ENOTTY — and a strict send must NOT
        // swallow it), so the plain-write fallback runs and lands the
        // exact payload bytes. USB strips the leading 0xF2; BT sends
        // the full 15 B report.
        let dir = tempfile::tempdir().unwrap();
        let usb = dir.path().join("haptic-usb");
        let bt = dir.path().join("haptic-bt");
        std::fs::write(&usb, []).unwrap();
        std::fs::write(&bt, []).unwrap();
        let h = RealHaptic::new();
        h.set_node(usb.to_str().unwrap());
        h.set_bluetooth(false);
        assert!(h.click_feedback(64, 5, false).is_ok());
        assert_eq!(
            std::fs::read(&usb).unwrap(),
            &vibration_report(64, 5, false)[1..]
        );
        h.set_node(bt.to_str().unwrap());
        h.set_bluetooth(true);
        assert!(h.click_feedback(64, 9, true).is_ok());
        assert_eq!(std::fs::read(&bt).unwrap(), &vibration_report(64, 9, true));
        // The up-pulse rides the same transport: USB strips F2, BT keeps it.
        h.set_node(usb.to_str().unwrap());
        h.set_bluetooth(false);
        assert!(h.release_feedback(64, false).is_ok());
        assert_eq!(
            std::fs::read(&usb).unwrap(),
            &vibration_up_report(64, false)[1..]
        );
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
            ("volumeup", 115),
            ("voldown", 114),
            ("volumedown", 114),
            ("mute", 113),
            ("micmute", 248),
            ("play", 164),
            ("playpause", 164),
            ("playcd", 200),
            ("pausecd", 201),
            ("stop", 166),
            ("stopcd", 166),
            ("next", 163),
            ("nextsong", 163),
            ("prev", 165),
            ("previous", 165),
            ("previoussong", 165),
            ("pause", 119),
            // Browser (back/forward stay mouse buttons above).
            ("browserback", 158),
            ("browser_back", 158),
            ("browserforward", 159),
            ("browser_forward", 159),
            ("refresh", 173),
            ("homepage", 172),
            ("search", 217),
            ("favorites", 364),
            ("bookmarks", 156),
            ("www", 150),
            // App launchers / system.
            ("calc", 140),
            ("mail", 155),
            ("computer", 157),
            ("player", 387),
            ("screenlock", 152),
            ("lock", 152),
            // Basics audit: insert/print were missing; pause already mapped.
            ("insert", 110),
            ("print", 210),
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
        sinks: &TestSinks,
        haptic: &Arc<FakeHaptic>,
        frames: Vec<Result<Option<TouchFrame>>>,
    ) -> DualTestProxy {
        let mut px = ClickProxy::new(
            settings(true),
            SharedGrab(Arc::new(FakeGrab::ok())),
            sinks.dual(),
            SharedHaptic(Arc::clone(haptic)),
            ScriptSource::new(frames),
        );
        px.set_pump_ready(true);
        px.attach(&usb_ready("/dev/hidraw1"));
        assert!(px.is_attached());
        px
    }

    /// Both wire logs behind the production two-node split: touch frames
    /// and buttons land on `pointer`, key-combo codes on `extras`
    /// ([`DualSink`] routing). Tests assert per node.
    struct TestSinks {
        pointer: Arc<FakeUinput>,
        extras: Arc<FakeUinput>,
    }

    impl TestSinks {
        fn new() -> Self {
            Self {
                pointer: Arc::new(FakeUinput::new()),
                extras: Arc::new(FakeUinput::new()),
            }
        }

        fn dual(&self) -> DualSink<SharedUinput, SharedUinput> {
            DualSink::new(
                SharedUinput(Arc::clone(&self.pointer)),
                SharedUinput(Arc::clone(&self.extras)),
            )
        }
    }

    /// Ready proxy type behind the production split (touch/buttons on the
    /// pointer sink, combos on extras).
    type DualTestProxy =
        ClickProxy<SharedGrab, DualSink<SharedUinput, SharedUinput>, SharedHaptic, ScriptSource>;

    /// Deterministic frame clock: 60 ms steps clear both the 50 ms press
    /// and 30 ms release debounce, so multi-frame sequences exercise edge
    /// logic undebounced (rapid `handle_frame` calls would trip the
    /// production debounce instead).
    fn spaced_at(t0: std::time::Instant, i: u64) -> std::time::Instant {
        t0 + std::time::Duration::from_millis(i * 60)
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
        let sinks = TestSinks::new();
        let haptic = Arc::new(FakeHaptic::new());
        let frames = vec![
            Ok(Some(frame(vec![finger(1, 200, 0, 0)], 0))),
            Ok(Some(frame(vec![finger(1, 200, 5, 5)], 0))),
        ];
        let mut px = drive_ready_proxy(&sinks, &haptic, frames);
        let cfg = Config {
            click_1: "ctrl+c".into(),
            ..Config::default()
        };
        px.handle_frame(&cfg).unwrap();
        px.handle_frame(&cfg).unwrap();
        assert!(!sinks.pointer.has(EV_KEY, BTN_LEFT, 1));
        // Combo codes ride the extras node, never the pointer node.
        assert!(sinks.extras.has(EV_KEY, 29, 1)); // ctrl down
        assert!(sinks.extras.has(EV_KEY, 46, 1)); // c down
        assert!(!sinks.pointer.has(EV_KEY, 29, 1));
        assert!(!sinks.pointer.has(EV_KEY, 46, 1));
    }

    #[test]
    fn right_hold_emits_no_btn_left() {
        let sinks = TestSinks::new();
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
        let mut px = drive_ready_proxy(&sinks, &haptic, frames);
        let cfg = Config {
            click_2: "right".into(),
            ..Config::default()
        };
        let ev = px.handle_frame(&cfg).unwrap().unwrap();
        assert_eq!(ev.finger_count, 2);
        px.handle_frame(&cfg).unwrap();
        assert!(!sinks.pointer.has(EV_KEY, BTN_LEFT, 1));
        assert!(sinks.pointer.has(EV_KEY, BTN_RIGHT, 1));
    }

    #[test]
    fn empty_hold_emits_no_key_downs() {
        let sinks = TestSinks::new();
        let haptic = Arc::new(FakeHaptic::new());
        let frames = vec![
            Ok(Some(frame(vec![finger(1, 200, 0, 0)], 0))),
            Ok(Some(frame(vec![finger(1, 200, 5, 5)], 0))),
        ];
        let mut px = drive_ready_proxy(&sinks, &haptic, frames);
        let cfg = Config {
            click_1: "empty".into(),
            ..Config::default()
        };
        px.handle_frame(&cfg).unwrap();
        px.handle_frame(&cfg).unwrap();
        // Only BTN_TOUCH=1 plus the BTN_TOOL_FINGER=1 classification bit
        // may be down (once per frame each); no buttons, no keys, no
        // BTN_LEFT.
        let downs = key_downs(&sinks.pointer);
        assert_eq!(downs.len(), 4, "{downs:?}");
        assert!(
            downs
                .iter()
                .all(|&e| e == (EV_KEY, BTN_TOUCH, 1) || e == (EV_KEY, BTN_TOOL_FINGER, 1)),
            "{downs:?}"
        );
        // Extras node carries framing only: zero KEY events there.
        assert!(
            sinks
                .extras
                .events
                .lock()
                .unwrap()
                .iter()
                .all(|&(t, _, _)| t != EV_KEY),
            "{:?}",
            sinks.extras.events.lock().unwrap()
        );
        assert!(!haptic.calls.lock().unwrap().is_empty());
    }

    #[test]
    fn ghost_slot_terminated_on_contact_drop() {
        // 2 contacts then 1: slot 1 must get TRACKING_ID=-1, or libinput
        // keeps a ghost contact alive.
        let sinks = TestSinks::new();
        let haptic = Arc::new(FakeHaptic::new());
        let frames = vec![
            Ok(Some(frame(
                vec![finger(1, 10, 0, 0), finger(2, 10, 9, 9)],
                0,
            ))),
            Ok(Some(frame(vec![finger(1, 10, 0, 0)], 0))),
        ];
        let mut px = drive_ready_proxy(&sinks, &haptic, frames);
        let cfg = Config::default();
        px.handle_frame(&cfg).unwrap();
        px.handle_frame(&cfg).unwrap();
        let evs = sinks.pointer.events.lock().unwrap().clone();
        let term1 = evs
            .windows(2)
            .any(|w| w[0] == (EV_ABS, ABS_MT_SLOT, 1) && w[1] == (EV_ABS, ABS_MT_TRACKING_ID, -1));
        assert!(term1, "slot 1 never terminated: {evs:?}");
    }

    #[test]
    fn id_reuse_after_lift_has_no_jump() {
        // Lift id 7 at x=100, then a NEW id-7 contact at x=0: without
        // eviction the scaler would emit prev + delta (a jump).
        let sinks = TestSinks::new();
        let haptic = Arc::new(FakeHaptic::new());
        let mut lift = finger(7, 0, 100, 0);
        lift.contact = false;
        let frames = vec![
            Ok(Some(frame(vec![finger(7, 10, 100, 0)], 0))),
            Ok(Some(frame(vec![lift], 0))),
            Ok(Some(frame(vec![finger(7, 10, 0, 0)], 0))),
        ];
        let mut px = drive_ready_proxy(&sinks, &haptic, frames);
        let cfg = Config::default();
        px.handle_frame(&cfg).unwrap();
        px.handle_frame(&cfg).unwrap();
        px.handle_frame(&cfg).unwrap();
        // Fresh anchor: emitted x == raw x on first sighting.
        let xs: Vec<i32> = sinks
            .pointer
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
        let sinks = TestSinks::new();
        let haptic = Arc::new(FakeHaptic::new());
        let frames = vec![Ok(Some(frame(vec![], 0xC0))), Ok(Some(frame(vec![], 0xC1)))];
        let mut px = drive_ready_proxy(&sinks, &haptic, frames);
        let cfg = Config::default();
        px.handle_frame(&cfg).unwrap();
        px.handle_frame(&cfg).unwrap();
        let evs = sinks.pointer.events.lock().unwrap().clone();
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
        let sinks = TestSinks::new();
        let haptic = Arc::new(FakeHaptic::new());
        let frames = vec![Ok(Some(frame(vec![finger(1, 200, 0, 0)], 0)))];
        let mut px = drive_ready_proxy(&sinks, &haptic, frames);
        let cfg = Config::default();
        px.handle_frame(&cfg).unwrap();
        assert!(sinks.pointer.has(EV_KEY, BTN_LEFT, 1));
        px.detach(&detached("/dev/hidraw1", Transport::Usb));
        assert!(sinks.pointer.has(EV_KEY, BTN_LEFT, 0));
        // Trailing SYN flushes the key-up; without it keys stick.
        let evs = sinks.pointer.events.lock().unwrap().clone();
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
        let sinks = TestSinks::new();
        let haptic = Arc::new(FakeHaptic::new());
        let frames = vec![Ok(Some(frame(vec![finger(1, 250, 0, 0)], 0)))];
        let mut px = drive_ready_proxy(&sinks, &haptic, frames);
        let cfg = Config::default(); // force_offset = 0
        let ev = px.handle_frame(&cfg).unwrap().unwrap();
        assert!(!ev.is_force());
        assert_eq!(ev.finger_count, 1);
        // First-level keys only; no forward key from the force map.
        assert!(sinks.pointer.has(EV_KEY, BTN_LEFT, 1));
        assert!(!sinks.pointer.has(EV_KEY, BTN_EXTRA, 1));
    }

    #[test]
    fn force_edge_holds_both_key_sets() {
        let sinks = TestSinks::new();
        let haptic = Arc::new(FakeHaptic::new());
        let frames = vec![
            Ok(Some(frame(vec![finger(1, 70, 0, 0)], 0))),
            Ok(Some(frame(vec![finger(1, 100, 0, 0)], 0))),
        ];
        let mut px = drive_ready_proxy(&sinks, &haptic, frames);
        let cfg = force_cfg();
        let first = px.handle_frame(&cfg).unwrap().unwrap();
        assert!(!first.is_force());
        let second = px.handle_frame(&cfg).unwrap().unwrap();
        assert!(second.is_force());
        assert_eq!(second.finger_count, 1);
        // Coexist: left (first) still down, forward (force) now down too.
        // `forward` (BTN_EXTRA) rides the extras node (old-driver parity:
        // only LEFT/RIGHT/MIDDLE are pointer buttons).
        assert!(sinks.pointer.has(EV_KEY, BTN_LEFT, 1));
        assert!(sinks.extras.has(EV_KEY, BTN_EXTRA, 1));
        assert!(!sinks.pointer.has(EV_KEY, BTN_EXTRA, 1));
        // Force supersedes in reporting; haptic falls back to standard 5.
        assert_eq!(
            haptic.calls.lock().unwrap().as_slice(),
            &[(64, 5, false), (64, 5, false)]
        );
    }

    #[test]
    fn force_haptic_override_and_quiet_waveform() {
        let sinks = TestSinks::new();
        let haptic = Arc::new(FakeHaptic::new());
        let frames = vec![
            Ok(Some(frame(vec![finger(1, 70, 0, 0)], 0))),
            Ok(Some(frame(vec![finger(1, 100, 0, 0)], 0))),
        ];
        let mut px = drive_ready_proxy(&sinks, &haptic, frames);
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
            &[(64, 5, false), (64, 9, true)]
        );
    }

    #[test]
    fn force_release_returns_to_first_hold_then_full_release() {
        let sinks = TestSinks::new();
        let haptic = Arc::new(FakeHaptic::new());
        // 70 = first hold; 100 = past force press (84); 70 = below force
        // release (79) but above first release (59); 10 = below all.
        let frames = vec![
            Ok(Some(frame(vec![finger(1, 70, 0, 0)], 0))),
            Ok(Some(frame(vec![finger(1, 100, 0, 0)], 0))),
            Ok(Some(frame(vec![finger(1, 70, 0, 0)], 0))),
            Ok(Some(frame(vec![finger(1, 10, 0, 0)], 0))),
        ];
        let mut px = drive_ready_proxy(&sinks, &haptic, frames);
        let cfg = force_cfg();
        // Spaced: the full-release frame must clear the release debounce.
        let t0 = std::time::Instant::now();
        px.handle_frame_at(&cfg, spaced_at(t0, 0)).unwrap();
        px.handle_frame_at(&cfg, spaced_at(t0, 1)).unwrap();
        assert!(sinks.extras.has(EV_KEY, BTN_EXTRA, 1));
        // Force release: forward up, left still down (BTN_LEFT re-asserted).
        px.handle_frame_at(&cfg, spaced_at(t0, 2)).unwrap();
        assert!(sinks.extras.has(EV_KEY, BTN_EXTRA, 0));
        assert!(sinks.pointer.has(EV_KEY, BTN_LEFT, 1));
        // Full release: left up.
        px.handle_frame_at(&cfg, spaced_at(t0, 3)).unwrap();
        assert!(sinks.pointer.has(EV_KEY, BTN_LEFT, 0));
    }

    #[test]
    fn force_two_finger_map_selects_force_2() {
        let sinks = TestSinks::new();
        let haptic = Arc::new(FakeHaptic::new());
        let frames = vec![
            Ok(Some(frame(vec![finger(1, 70, 0, 0)], 0))),
            Ok(Some(frame(
                vec![finger(1, 100, 0, 0), finger(2, 100, 9, 9)],
                0,
            ))),
        ];
        let mut px = drive_ready_proxy(&sinks, &haptic, frames);
        let cfg = Config {
            force_2: "right".into(),
            ..force_cfg()
        };
        px.handle_frame(&cfg).unwrap();
        let ev = px.handle_frame(&cfg).unwrap().unwrap();
        assert!(ev.is_force());
        assert_eq!(ev.finger_count, 2);
        assert!(sinks.pointer.has(EV_KEY, BTN_RIGHT, 1));
    }

    #[test]
    fn force_never_fires_past_u8_range() {
        let sinks = TestSinks::new();
        let haptic = Arc::new(FakeHaptic::new());
        let frames = vec![Ok(Some(frame(vec![finger(1, 255, 0, 0)], 0)))];
        let mut px = drive_ready_proxy(&sinks, &haptic, frames);
        let cfg = Config {
            sensitivity: 255,
            margin: 5,
            force_offset: 255,
            force_1: "forward".into(),
            ..Config::default()
        };
        let ev = px.handle_frame(&cfg).unwrap().unwrap();
        assert!(!ev.is_force());
        assert!(sinks.pointer.has(EV_KEY, BTN_LEFT, 1));
        // Force never fired: no forward key on EITHER node.
        assert!(!sinks.extras.has(EV_KEY, BTN_EXTRA, 1));
        assert!(!sinks.pointer.has(EV_KEY, BTN_EXTRA, 1));
    }

    #[test]
    fn shared_modifier_survives_force_release() {
        // `ctrl` in both chords: force release must not drop it mid-hold.
        // Tail-asserted: the single ctrl up lands in the full-release frame.
        let sinks = TestSinks::new();
        let haptic = Arc::new(FakeHaptic::new());
        let frames = vec![
            Ok(Some(frame(vec![finger(1, 70, 0, 0)], 0))),
            Ok(Some(frame(vec![finger(1, 100, 0, 0)], 0))),
            Ok(Some(frame(vec![finger(1, 70, 0, 0)], 0))),
            Ok(Some(frame(vec![finger(1, 10, 0, 0)], 0))),
        ];
        let mut px = drive_ready_proxy(&sinks, &haptic, frames);
        let cfg = Config {
            sensitivity: 64,
            margin: 5,
            force_offset: 20,
            click_1: "ctrl+c".into(),
            force_1: "ctrl+v".into(),
            ..Config::default()
        };
        // Spaced: the full-release frame must clear the release debounce.
        let t0 = std::time::Instant::now();
        px.handle_frame_at(&cfg, spaced_at(t0, 0)).unwrap();
        px.handle_frame_at(&cfg, spaced_at(t0, 1)).unwrap();
        px.handle_frame_at(&cfg, spaced_at(t0, 2)).unwrap();
        // Combo codes ride the extras node: pin the shared-modifier
        // accounting there (pointer carries no combo traffic at all).
        let mark = sinks.extras.events.lock().unwrap().len();
        px.handle_frame_at(&cfg, spaced_at(t0, 3)).unwrap();
        let evs = sinks.extras.events.lock().unwrap().clone();
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
        // Staged frames (250, 250): first latches frame one, force frame
        // two — the second 250 proves the force path latches (without it
        // the deferred edge never re-fires and the test is dead).
        let sinks = TestSinks::new();
        let haptic = Arc::new(FakeHaptic::new());
        let frames = vec![
            Ok(Some(frame(vec![finger(1, 250, 0, 0)], 0))),
            Ok(Some(frame(vec![finger(1, 250, 0, 0)], 0))),
            Ok(Some(frame(vec![finger(1, 70, 0, 0)], 0))),
            Ok(Some(frame(vec![finger(1, 10, 0, 0)], 0))),
        ];
        let mut px = drive_ready_proxy(&sinks, &haptic, frames);
        let cfg = Config {
            sensitivity: 64,
            margin: 5,
            force_offset: 20,
            force_1: "left".into(),
            ..Config::default()
        };
        // Spaced: the release frame must clear the release debounce.
        let t0 = std::time::Instant::now();
        px.handle_frame_at(&cfg, spaced_at(t0, 0)).unwrap();
        px.handle_frame_at(&cfg, spaced_at(t0, 1)).unwrap();
        px.handle_frame_at(&cfg, spaced_at(t0, 2)).unwrap();
        px.handle_frame_at(&cfg, spaced_at(t0, 3)).unwrap();
        let left = key_values(&sinks.pointer, BTN_LEFT);
        assert_eq!(left.first(), Some(&1));
        assert_eq!(left.last(), Some(&0));
        assert_eq!(transitions(&left), 1, "phantom BTN_LEFT release: {left:?}");
        // Both levels pulsed: first DOWN then force DOWN (force path live).
        assert_eq!(
            haptic.calls.lock().unwrap().as_slice(),
            &[(64, 5, false), (64, 5, false)]
        );
    }

    #[test]
    fn staged_edges_hold_then_release() {
        // Old-exact staging: deep jump latches first frame one, force
        // frame two; dropping below all thresholds releases both.
        let sinks = TestSinks::new();
        let haptic = Arc::new(FakeHaptic::new());
        let frames = vec![
            Ok(Some(frame(vec![finger(1, 250, 0, 0)], 0))),
            Ok(Some(frame(vec![finger(1, 250, 0, 0)], 0))),
            Ok(Some(frame(vec![finger(1, 10, 0, 0)], 0))),
        ];
        let mut px = drive_ready_proxy(&sinks, &haptic, frames);
        // Split waveforms: force UP loud, first UP quiet — pins both the
        // release order (force UP before first UP) and each level's flag.
        let cfg = Config {
            quiet_click: true,
            ..force_cfg()
        };
        // Spaced: the release frame must clear the release debounce.
        let t0 = std::time::Instant::now();
        let first = px.handle_frame_at(&cfg, spaced_at(t0, 0)).unwrap().unwrap();
        assert!(!first.is_force());
        let ev = px.handle_frame_at(&cfg, spaced_at(t0, 1)).unwrap().unwrap();
        assert!(ev.is_force());
        assert!(sinks.pointer.has(EV_KEY, BTN_LEFT, 1));
        assert!(sinks.extras.has(EV_KEY, BTN_EXTRA, 1));
        let mark = sinks.pointer.events.lock().unwrap().len();
        let emark = sinks.extras.events.lock().unwrap().len();
        px.handle_frame_at(&cfg, spaced_at(t0, 2)).unwrap();
        let tail = sinks.pointer.events.lock().unwrap()[mark..].to_vec();
        assert!(
            tail.contains(&(EV_KEY, BTN_LEFT, 0)),
            "left never released: {tail:?}"
        );
        let etail = sinks.extras.events.lock().unwrap()[emark..].to_vec();
        assert!(
            etail.contains(&(EV_KEY, BTN_EXTRA, 0)),
            "forward never released: {etail:?}"
        );
        // UP order + waveforms: force UP (loud) before first UP (quiet).
        assert_eq!(
            haptic.releases.lock().unwrap().as_slice(),
            &[(64, false), (64, true)]
        );
    }

    #[test]
    fn force_silent_override_emits_no_force_haptic() {
        // force_haptic = Some(0): force edge silent, first-level pulse kept.
        let sinks = TestSinks::new();
        let haptic = Arc::new(FakeHaptic::new());
        let frames = vec![
            Ok(Some(frame(vec![finger(1, 70, 0, 0)], 0))),
            Ok(Some(frame(vec![finger(1, 100, 0, 0)], 0))),
        ];
        let mut px = drive_ready_proxy(&sinks, &haptic, frames);
        let cfg = Config {
            force_haptic: Some(0),
            ..force_cfg()
        };
        px.handle_frame(&cfg).unwrap();
        let ev = px.handle_frame(&cfg).unwrap().unwrap();
        assert!(ev.is_force());
        assert_eq!(ev.haptic, 0);
        assert_eq!(haptic.calls.lock().unwrap().as_slice(), &[(64, 5, false)]);
        // Keys still held on both levels despite the silent force edge.
        assert!(sinks.pointer.has(EV_KEY, BTN_LEFT, 1));
        assert!(sinks.extras.has(EV_KEY, BTN_EXTRA, 1));
    }

    #[test]
    fn staged_jump_emits_both_haptic_pulses_in_order() {
        // Old-exact staging: one deep jump from idle emits the
        // first-level pulse frame one, the force pulse frame two
        // (never both on the same frame).
        let sinks = TestSinks::new();
        let haptic = Arc::new(FakeHaptic::new());
        let frames = vec![
            Ok(Some(frame(vec![finger(1, 250, 0, 0)], 0))),
            Ok(Some(frame(vec![finger(1, 250, 0, 0)], 0))),
        ];
        let mut px = drive_ready_proxy(&sinks, &haptic, frames);
        let cfg = Config {
            force_haptic: Some(9),
            force_quiet_click: true,
            ..force_cfg()
        };
        let t0 = std::time::Instant::now();
        let first = px.handle_frame_at(&cfg, spaced_at(t0, 0)).unwrap().unwrap();
        assert!(!first.is_force());
        assert_eq!(haptic.calls.lock().unwrap().as_slice(), &[(64, 5, false)]);
        let ev = px.handle_frame_at(&cfg, spaced_at(t0, 1)).unwrap().unwrap();
        assert!(ev.is_force());
        assert_eq!(
            haptic.calls.lock().unwrap().as_slice(),
            &[(64, 5, false), (64, 9, true)]
        );
    }

    /// Owner's tune (6 months on old driver): sensitivity 30, haptic 2
    /// quiet, force_offset 130, force_haptic 1. `forward` stands in for
    /// the owner's force map (action identity is not under test; the
    /// haptic pulse bytes + order are).
    fn owner_cfg() -> Config {
        Config {
            sensitivity: 30,
            margin: 5,
            haptic: 2,
            quiet_click: true,
            force_offset: 130,
            force_haptic: Some(1),
            force_quiet_click: false,
            force_1: "forward".into(),
            ..Config::default()
        }
    }

    #[test]
    fn owner_tune_force_reports_pin_old_bytes() {
        // Old-exact DOWN: pressure = sensitivity (30), amplitude 0x78
        // (loud), intensity = force_haptic (1), pattern 6. UP: pressure =
        // 30*3/5 (18), intensity 0, same waveform.
        assert_eq!(
            vibration_report(30, 1, false),
            [0xF2, 0x53, 0x01, 30, 0x78, 0x02, 1, 0x24, 0x30, 0x06, 0x01, 0x06, 0x18, 0x48, 0x12]
        );
        assert_eq!(up_pressure(30), 18);
        assert_eq!(
            vibration_up_report(30, false),
            [0xF2, 0x53, 0x01, 18, 0x78, 0x02, 0, 0x24, 0x30, 0x06, 0x01, 0x06, 0x18, 0x48, 0x12]
        );
    }

    #[test]
    fn owner_tune_staged_force_down_up_pulse_sequence() {
        // Staged gesture at owner tune: 50 (first hold) -> 200 (force
        // edge) -> 100 (force step-down) -> 10 (full release). DOWN
        // pulses carry (pressure 30, intensity, quiet); UP pulses echo
        // the down pressure + waveform with intensity 0 on the wire.
        let sinks = TestSinks::new();
        let haptic = Arc::new(FakeHaptic::new());
        let frames = vec![
            Ok(Some(frame(vec![finger(1, 50, 0, 0)], 0))),
            Ok(Some(frame(vec![finger(1, 200, 0, 0)], 0))),
            Ok(Some(frame(vec![finger(1, 100, 0, 0)], 0))),
            Ok(Some(frame(vec![finger(1, 10, 0, 0)], 0))),
        ];
        let mut px = drive_ready_proxy(&sinks, &haptic, frames);
        let cfg = owner_cfg();
        let t0 = std::time::Instant::now();
        let first = px.handle_frame_at(&cfg, spaced_at(t0, 0)).unwrap().unwrap();
        assert!(!first.is_force());
        let second = px.handle_frame_at(&cfg, spaced_at(t0, 1)).unwrap().unwrap();
        assert!(second.is_force());
        px.handle_frame_at(&cfg, spaced_at(t0, 2)).unwrap();
        px.handle_frame_at(&cfg, spaced_at(t0, 3)).unwrap();
        assert_eq!(
            haptic.calls.lock().unwrap().as_slice(),
            &[(30, 2, true), (30, 1, false)]
        );
        assert_eq!(
            haptic.releases.lock().unwrap().as_slice(),
            &[(30, false), (30, true)]
        );
    }

    #[test]
    fn owner_tune_deep_jump_stages_force_pulse() {
        // Old-exact: a deep stab from idle emits the first-level DOWN
        // only; the force DOWN follows on the next frame while held deep.
        let sinks = TestSinks::new();
        let haptic = Arc::new(FakeHaptic::new());
        let frames = vec![
            Ok(Some(frame(vec![finger(1, 200, 0, 0)], 0))),
            Ok(Some(frame(vec![finger(1, 200, 0, 0)], 0))),
        ];
        let mut px = drive_ready_proxy(&sinks, &haptic, frames);
        let cfg = owner_cfg();
        let t0 = std::time::Instant::now();
        let ev = px.handle_frame_at(&cfg, spaced_at(t0, 0)).unwrap().unwrap();
        assert!(!ev.is_force());
        assert_eq!(
            haptic.calls.lock().unwrap().as_slice(),
            &[(30, 2, true)],
            "same-frame force pulse diverges from old (staged) feel"
        );
        let ev = px.handle_frame_at(&cfg, spaced_at(t0, 1)).unwrap().unwrap();
        assert!(ev.is_force());
        assert_eq!(
            haptic.calls.lock().unwrap().as_slice(),
            &[(30, 2, true), (30, 1, false)]
        );
        assert!(sinks.extras.has(EV_KEY, BTN_EXTRA, 1));
    }

    #[test]
    fn btn_left_reasserted_after_force_release() {
        // click left + force forward: the force-release frame tail still
        // carries BTN_LEFT=1 (first hold); the full-release tail ends at 0.
        let sinks = TestSinks::new();
        let haptic = Arc::new(FakeHaptic::new());
        let frames = vec![
            Ok(Some(frame(vec![finger(1, 70, 0, 0)], 0))),
            Ok(Some(frame(vec![finger(1, 100, 0, 0)], 0))),
            Ok(Some(frame(vec![finger(1, 70, 0, 0)], 0))),
            Ok(Some(frame(vec![finger(1, 10, 0, 0)], 0))),
        ];
        let mut px = drive_ready_proxy(&sinks, &haptic, frames);
        let cfg = force_cfg();
        // Spaced: the full-release frame must clear the release debounce.
        let t0 = std::time::Instant::now();
        px.handle_frame_at(&cfg, spaced_at(t0, 0)).unwrap();
        px.handle_frame_at(&cfg, spaced_at(t0, 1)).unwrap();
        px.handle_frame_at(&cfg, spaced_at(t0, 2)).unwrap();
        let left = key_values(&sinks.pointer, BTN_LEFT);
        assert_eq!(left.last(), Some(&1), "first hold lost: {left:?}");
        let mark = sinks.pointer.events.lock().unwrap().len();
        px.handle_frame_at(&cfg, spaced_at(t0, 3)).unwrap();
        let tail: Vec<(u16, u16, i32)> = sinks.pointer.events.lock().unwrap()[mark..].to_vec();
        assert!(
            tail.contains(&(EV_KEY, BTN_LEFT, 0)),
            "left never released: {tail:?}"
        );
        assert_eq!(key_values(&sinks.pointer, BTN_LEFT).last(), Some(&0));
    }

    // ---- Cursor-motion + feel-parity contract ----
    //
    // Hardware-proven against the old driver: without the classification
    // bits + touch axes + pointer density the cursor stays dead. These
    // tests pin the motion stream libinput consumes.

    #[test]
    fn motion_stream_carries_sane_position_deltas() {
        // A slow diagonal drag must surface as changing ABS_MT_POSITION_X/Y
        // with sane per-frame deltas, framed by BTN_TOOL_FINGER + BTN_TOUCH
        // + SYN — the evtest-visible motion stream libinput turns into
        // cursor motion. Owner check: `sudo evtest` on the virtual node
        // shows the same `Event: type 3 (EV_ABS)` rows dumped below.
        let sinks = TestSinks::new();
        let haptic = Arc::new(FakeHaptic::new());
        let frames = vec![
            Ok(Some(frame(vec![finger(1, 10, 100, 100)], 0))),
            Ok(Some(frame(vec![finger(1, 10, 110, 105)], 0))),
            Ok(Some(frame(vec![finger(1, 10, 125, 112)], 0))),
        ];
        let mut px = drive_ready_proxy(&sinks, &haptic, frames);
        let cfg = Config::default();
        let t0 = std::time::Instant::now();
        for i in 0..3 {
            px.handle_frame_at(&cfg, spaced_at(t0, i)).unwrap();
        }
        let evs = wire_events(&sinks.pointer);
        let mut dump = String::new();
        for &(ty, code, val) in &evs {
            if ty == EV_ABS && (code == ABS_MT_POSITION_X || code == ABS_MT_POSITION_Y) {
                let name = if code == ABS_MT_POSITION_X {
                    "ABS_MT_POSITION_X"
                } else {
                    "ABS_MT_POSITION_Y"
                };
                dump.push_str(&format!(
                    "Event: type 3 (EV_ABS), code {code} ({name}), value {val}\n"
                ));
            }
        }
        assert!(!dump.is_empty(), "no motion rows on the wire");
        let axis = |code: u16| {
            evs.iter()
                .copied()
                .filter(|&(t, c, _)| t == EV_ABS && c == code)
                .map(|(_, _, v)| v)
                .collect::<Vec<i32>>()
        };
        let xs = axis(ABS_MT_POSITION_X);
        let ys = axis(ABS_MT_POSITION_Y);
        assert_eq!(xs, vec![100, 110, 125], "motion X stream:\n{dump}");
        assert_eq!(ys, vec![100, 105, 112], "motion Y stream:\n{dump}");
        // Sane deltas: nonzero forward motion, bounded (no jumps).
        for (series, name) in [(&xs, "X"), (&ys, "Y")] {
            for w in series.windows(2) {
                let d = w[1] - w[0];
                assert!(
                    d > 0 && d <= 50,
                    "{name} delta out of sane range: {d} ({series:?})"
                );
            }
        }
        // Classification rows ride every contact frame (cursor lives here).
        assert_eq!(
            key_values(&sinks.pointer, BTN_TOOL_FINGER),
            vec![1, 1, 1],
            "BTN_TOOL_FINGER must mark every contact frame"
        );
        assert!(sinks.pointer.has(EV_ABS, ABS_MT_TOUCH_MAJOR, 800));
        assert!(sinks.pointer.has(EV_ABS, ABS_MT_TOUCH_MINOR, 800));
        assert!(sinks.pointer.has(EV_KEY, BTN_TOUCH, 1));
        // Light touch (pressure 10 < sensitivity): motion, no click.
        assert!(!sinks.pointer.has(EV_KEY, BTN_LEFT, 1));
        // One SYN per handled frame on EACH node (both independently framed).
        let syns = evs
            .iter()
            .filter(|&&(t, c, v)| t == EV_SYN && c == SYN_REPORT && v == 0)
            .count();
        assert_eq!(syns, 3, "exactly one SYN per frame: {evs:?}");
        let xsyns = wire_events(&sinks.extras)
            .iter()
            .filter(|&&(t, c, v)| t == EV_SYN && c == SYN_REPORT && v == 0)
            .count();
        assert_eq!(xsyns, 3, "extras must frame every handled frame");
    }

    #[test]
    fn light_slide_emits_old_parity_motion_values() {
        // Hardware failure mode (owner MT2 USB-C, 2026-10-15): real frames
        // flowing, cursor dead on our node. Frame-by-frame diff against the
        // old emit path (trackpad_handler.rs:193-236) shows one
        // motion-relevant divergence: old normalizes every down contact to
        // PRESSURE=60 + MAJOR/MINOR=800 (`200 << 2`); we put the raw
        // arbitrary-unit bytes on the wire (small for a light slide), which
        // the downstream classifier drops. Slots/TOOL/SYN/resolution already
        // match, so this test pins the normalized values per contact frame.
        fn raw_slide(id: u8, pressure: u8, x: i32, y: i32) -> FingerContact {
            let mut f = finger(id, pressure, x, y);
            f.touch_major = 30;
            f.touch_minor = 20;
            f
        }
        let sinks = TestSinks::new();
        let haptic = Arc::new(FakeHaptic::new());
        let frames = vec![
            Ok(Some(frame(vec![raw_slide(3, 20, 100, 100)], 0))),
            Ok(Some(frame(vec![raw_slide(3, 22, 110, 105)], 0))),
            Ok(Some(frame(vec![raw_slide(3, 19, 125, 112)], 0))),
        ];
        let mut px = drive_ready_proxy(&sinks, &haptic, frames);
        let cfg = Config::default();
        let t0 = std::time::Instant::now();
        for i in 0..3 {
            px.handle_frame_at(&cfg, spaced_at(t0, i)).unwrap();
        }
        let evs = wire_events(&sinks.pointer);
        let axis = |code: u16| {
            evs.iter()
                .copied()
                .filter(|&(t, c, _)| t == EV_ABS && c == code)
                .map(|(_, _, v)| v)
                .collect::<Vec<i32>>()
        };
        // Raw bytes (20/30/20) must never reach the wire: old-parity
        // normalized values on every contact frame.
        assert_eq!(axis(ABS_MT_PRESSURE), vec![60, 60, 60]);
        assert_eq!(axis(ABS_MT_TOUCH_MAJOR), vec![800, 800, 800]);
        assert_eq!(axis(ABS_MT_TOUCH_MINOR), vec![800, 800, 800]);
        // Motion itself still tracks 1:1 at default speed.
        assert_eq!(axis(ABS_MT_POSITION_X), vec![100, 110, 125]);
        assert_eq!(axis(ABS_MT_POSITION_Y), vec![100, 105, 112]);
        assert_eq!(
            key_values(&sinks.pointer, BTN_TOOL_FINGER),
            vec![1, 1, 1],
            "BTN_TOOL_FINGER must mark every contact frame"
        );
        assert!(sinks.pointer.has(EV_KEY, BTN_TOUCH, 1));
        // Light slide: motion, no click, no haptic.
        assert!(!sinks.pointer.has(EV_KEY, BTN_LEFT, 1));
        assert!(haptic.calls.lock().unwrap().is_empty());
        let syns = evs
            .iter()
            .filter(|&&(t, c, v)| t == EV_SYN && c == SYN_REPORT && v == 0)
            .count();
        assert_eq!(syns, 3, "exactly one SYN per frame: {evs:?}");
        let xsyns = wire_events(&sinks.extras)
            .iter()
            .filter(|&&(t, c, v)| t == EV_SYN && c == SYN_REPORT && v == 0)
            .count();
        assert_eq!(xsyns, 3, "extras must frame every handled frame");
    }

    #[test]
    fn tool_bits_follow_finger_count() {
        // 2 fingers → DOUBLETAP down (FINGER up); liftoff clears all.
        let sinks = TestSinks::new();
        let haptic = Arc::new(FakeHaptic::new());
        let frames = vec![
            Ok(Some(frame(
                vec![finger(1, 10, 0, 0), finger(2, 10, 9, 9)],
                0,
            ))),
            Ok(Some(frame(vec![], 0))),
        ];
        let mut px = drive_ready_proxy(&sinks, &haptic, frames);
        let cfg = Config::default();
        let t0 = std::time::Instant::now();
        px.handle_frame_at(&cfg, spaced_at(t0, 0)).unwrap();
        px.handle_frame_at(&cfg, spaced_at(t0, 1)).unwrap();
        assert_eq!(key_values(&sinks.pointer, BTN_TOOL_FINGER), vec![0, 0]);
        assert_eq!(key_values(&sinks.pointer, BTN_TOOL_DOUBLETAP), vec![1, 0]);
        assert_eq!(key_values(&sinks.pointer, BTN_TOOL_TRIPLETAP), vec![0, 0]);
        assert_eq!(key_values(&sinks.pointer, BTN_TOOL_QUINTTAP), vec![0, 0]);
        // Five fingers → QUINTTAP.
        let sinks = TestSinks::new();
        let haptic = Arc::new(FakeHaptic::new());
        let fingers: Vec<_> = (1..=5).map(|id| finger(id, 10, 0, 0)).collect();
        let frames = vec![Ok(Some(frame(fingers, 0)))];
        let mut px = drive_ready_proxy(&sinks, &haptic, frames);
        px.handle_frame_at(&cfg, spaced_at(t0, 0)).unwrap();
        assert_eq!(key_values(&sinks.pointer, BTN_TOOL_QUINTTAP), vec![1]);
        assert_eq!(key_values(&sinks.pointer, BTN_TOOL_QUADTAP), vec![0]);
    }

    #[test]
    fn tool_keycodes_match_kernel_header() {
        // Owner evtest 2026-10-16: the wire carried BTN_TOOL_MOUSE (326) /
        // BTN_TOOL_LENS (327) on a touchpad — hand-rolled codes ran 325..=329
        // sequentially, but the kernel header interleaves MOUSE / LENS /
        // QUINTTAP / STYLUS3 between FINGER and DOUBLETAP. Pin exact numeric
        // values so a wrong constant fails here first.
        assert_eq!(BTN_TOOL_FINGER, 325); // 0x145
        assert_eq!(BTN_TOOL_QUINTTAP, 328); // 0x148
        assert_eq!(BTN_TOUCH, 330); // 0x14a
        assert_eq!(BTN_TOOL_DOUBLETAP, 333); // 0x14d
        assert_eq!(BTN_TOOL_TRIPLETAP, 334); // 0x14e
        assert_eq!(BTN_TOOL_QUADTAP, 335); // 0x14f
                                           // The mouse/lens codes must be nowhere near the touchpad tool row.
        assert_ne!(BTN_TOOL_DOUBLETAP, 326);
        assert_ne!(BTN_TOOL_TRIPLETAP, 327);
    }

    #[test]
    fn rapid_release_holds_latch_per_debounce() {
        // Production timing: back-to-back handle_frame calls land
        // microseconds apart, inside the 30 ms release window — the latch
        // holds and BTN_LEFT stays down (old-service quick-tap debounce).
        let sinks = TestSinks::new();
        let haptic = Arc::new(FakeHaptic::new());
        let frames = vec![
            Ok(Some(frame(vec![finger(1, 200, 0, 0)], 0))),
            Ok(Some(frame(vec![finger(1, 10, 0, 0)], 0))),
        ];
        let mut px = drive_ready_proxy(&sinks, &haptic, frames);
        let cfg = Config::default();
        assert!(px.handle_frame(&cfg).unwrap().is_some());
        assert!(px.handle_frame(&cfg).unwrap().is_none());
        assert_eq!(
            key_values(&sinks.pointer, BTN_LEFT),
            vec![1],
            "quick release must hold BTN_LEFT down"
        );
    }

    #[test]
    fn hover_pressure_drives_slot_without_click() {
        // contact=false but pressure 50 (> touch floor 10): the slot is
        // emitted (cursor tracks) while the click latch stays idle.
        let sinks = TestSinks::new();
        let haptic = Arc::new(FakeHaptic::new());
        let mut hover = finger(1, 50, 30, 40);
        hover.contact = false;
        let frames = vec![Ok(Some(frame(vec![hover], 0)))];
        let mut px = drive_ready_proxy(&sinks, &haptic, frames);
        let cfg = Config::default();
        assert!(px.handle_frame(&cfg).unwrap().is_none());
        assert!(sinks.pointer.has(EV_ABS, ABS_MT_TRACKING_ID, 1));
        assert!(sinks.pointer.has(EV_ABS, ABS_MT_POSITION_X, 30));
        assert!(sinks.pointer.has(EV_KEY, BTN_TOUCH, 1));
        assert_eq!(key_values(&sinks.pointer, BTN_TOOL_FINGER), vec![1]);
        assert!(!sinks.pointer.has(EV_KEY, BTN_LEFT, 1));
    }

    #[test]
    fn haptic_pressure_is_threshold_not_strength() {
        // Old-service mapping through the full proxy path: pressure byte =
        // sensitivity (threshold), intensity = configured haptic strength.
        let sinks = TestSinks::new();
        let haptic = Arc::new(FakeHaptic::new());
        let frames = vec![Ok(Some(frame(vec![finger(1, 200, 0, 0)], 0)))];
        let mut px = drive_ready_proxy(&sinks, &haptic, frames);
        let cfg = Config {
            sensitivity: 80,
            haptic: 7,
            ..Config::default()
        };
        px.handle_frame(&cfg).unwrap();
        assert_eq!(haptic.calls.lock().unwrap().as_slice(), &[(80, 7, false)]);
    }

    #[test]
    fn haptic_down_up_pair_matches_old_service_bytes() {
        // Hardware-proven regression (owner MT2 USB-C 2026-10-15): the
        // actuator needs the old-service down+up pair — DOWN at
        // pressure=sensitivity with intensity=haptic, UP at
        // pressure=sensitivity*3/5 with intensity 0 (both edges). A
        // down-only stream leaves the click feel dead.
        let sinks = TestSinks::new();
        let haptic = Arc::new(FakeHaptic::new());
        let frames = vec![
            Ok(Some(frame(vec![finger(1, 200, 0, 0)], 0))),
            Ok(Some(frame(vec![finger(1, 10, 0, 0)], 0))),
        ];
        let mut px = drive_ready_proxy(&sinks, &haptic, frames);
        let cfg = Config::default();
        let t0 = std::time::Instant::now();
        px.handle_frame_at(&cfg, spaced_at(t0, 0)).unwrap();
        assert_eq!(haptic.calls.lock().unwrap().as_slice(), &[(64, 5, false)]);
        px.handle_frame_at(&cfg, spaced_at(t0, 1)).unwrap();
        assert_eq!(haptic.releases.lock().unwrap().as_slice(), &[(64, false)]);
        // Byte-exact pair on the wire (15 B template, USB strips F2).
        assert_eq!(
            vibration_report(64, 5, false),
            [0xF2, 0x53, 0x01, 64, 0x78, 0x02, 5, 0x24, 0x30, 0x06, 0x01, 0x06, 0x18, 0x48, 0x12]
        );
        assert_eq!(up_pressure(64), 38);
        assert_eq!(
            vibration_up_report(64, false),
            [0xF2, 0x53, 0x01, 38, 0x78, 0x02, 0, 0x24, 0x30, 0x06, 0x01, 0x06, 0x18, 0x48, 0x12]
        );
        assert_eq!(
            vibration_up_report(64, true),
            [0xF2, 0x53, 0x01, 38, 0xFF, 0x02, 0, 0x24, 0x30, 0x06, 0x01, 0x00, 0x18, 0x48, 0x12]
        );
    }

    #[test]
    fn silent_haptic_sends_no_down_or_up() {
        // haptic=0: no down pulse, so no release up-pulse either
        // (old-service triggered-flag parity — a silent press leaves
        // nothing for the release to snap back from).
        let sinks = TestSinks::new();
        let haptic = Arc::new(FakeHaptic::new());
        let frames = vec![
            Ok(Some(frame(vec![finger(1, 200, 0, 0)], 0))),
            Ok(Some(frame(vec![finger(1, 10, 0, 0)], 0))),
        ];
        let mut px = drive_ready_proxy(&sinks, &haptic, frames);
        let cfg = Config {
            haptic: 0,
            ..Config::default()
        };
        let t0 = std::time::Instant::now();
        px.handle_frame_at(&cfg, spaced_at(t0, 0)).unwrap();
        px.handle_frame_at(&cfg, spaced_at(t0, 1)).unwrap();
        assert!(sinks.pointer.has(EV_KEY, BTN_LEFT, 1));
        assert!(haptic.calls.lock().unwrap().is_empty());
        assert!(haptic.releases.lock().unwrap().is_empty());
    }

    // ---- Click-emission wire-purity regression net ----
    //
    // Wire hygiene for the proxy: these tests pin the COMPLETE wire event
    // set per scenario — press+hold+release must carry ONLY the action's
    // own codes plus `BTN_TOUCH`/`BTN_TOOL_*` classification/`SYN` and the
    // eight MT `ABS` codes, with zero other `EV_KEY` of any value — so any
    // future unexpected emission
    // fails here first. This is hygiene, not a symptom fix: the owner bug
    // (clicks typing '-' in LibreOffice, old version) stays OPEN pending
    // a hardware capture (see TESTING.md); no causation is claimed here.
    // (`BTN_LEFT` = 272 can never be the `KEY_MINUS` = 12 the symptom
    // would need — that capture decides the real cause.)

    /// `KEY_MINUS` (`linux/input-event-codes.h`): explicit redundant pin
    /// for the owner-reported '-' symptom. The per-scenario `allowed_keys`
    /// set is the primary guard (it already excludes code 12 everywhere);
    /// this assert only names the symptom so a future failure message
    /// points at the open bug instead of a bare code number.
    const KEY_MINUS: u16 = 12;

    /// Every `EV_ABS` code the proxy may ever emit (MT slot block only).
    const ALLOWED_ABS: [u16; 8] = [
        ABS_MT_SLOT,
        ABS_MT_TRACKING_ID,
        ABS_MT_POSITION_X,
        ABS_MT_POSITION_Y,
        ABS_MT_PRESSURE,
        ABS_MT_TOUCH_MAJOR,
        ABS_MT_TOUCH_MINOR,
        ABS_MT_ORIENTATION,
    ];

    /// Touchpad classification bits: emitted every frame from the touching
    /// count (not action codes), so the purity net permits them
    /// unconditionally — the exact-sequence tests pin their values.
    const TOOL_KEYS: [u16; 5] = [
        BTN_TOOL_FINGER,
        BTN_TOOL_DOUBLETAP,
        BTN_TOOL_TRIPLETAP,
        BTN_TOOL_QUADTAP,
        BTN_TOOL_QUINTTAP,
    ];

    fn wire_events(uinput: &FakeUinput) -> Vec<(u16, u16, i32)> {
        uinput.events.lock().unwrap().clone()
    }

    /// Assert the COMPLETE wire stream uses only expected codes:
    /// - `EV_SYN` is exactly `(SYN_REPORT, 0)`;
    /// - `EV_ABS` is one of the eight MT codes;
    /// - `EV_KEY` is in `allowed_keys` or a `BTN_TOOL_*` classification
    ///   bit, with value 0/1 (the action-code guard is the primary net;
    ///   the extra `KEY_MINUS` assert below is a redundant pin);
    /// - no other event type exists on the wire.
    fn assert_wire_pure(events: &[(u16, u16, i32)], allowed_keys: &[u16]) {
        for &(ty, code, val) in events {
            match ty {
                EV_SYN => assert_eq!(
                    (code, val),
                    (SYN_REPORT, 0),
                    "stray SYN event {ty}/{code}/{val}"
                ),
                EV_ABS => assert!(
                    ALLOWED_ABS.contains(&code),
                    "stray EV_ABS code {code}: {events:?}"
                ),
                EV_KEY => {
                    assert!(
                        val == 0 || val == 1,
                        "key event with bad value {ty}/{code}/{val}"
                    );
                    assert!(
                        allowed_keys.contains(&code) || TOOL_KEYS.contains(&code),
                        "stray EV_KEY code {code}: {events:?}"
                    );
                    assert_ne!(code, KEY_MINUS, "explicit KEY_MINUS pin fired: {events:?}");
                }
                _ => panic!("unexpected event type {ty}: {events:?}"),
            }
        }
        assert!(
            !events
                .iter()
                .any(|&(t, c, _)| t == EV_KEY && c == KEY_MINUS),
            "explicit KEY_MINUS pin fired: {events:?}"
        );
    }

    /// Button codes on an edge-triggered wire strictly alternate (exact
    /// down/up pairs): no stuck keys, no duplicate same-value re-asserts.
    fn assert_strict_alternation(vals: &[i32], what: &str) {
        assert!(
            !vals.is_empty(),
            "{what}: expected a down/up pair, got none"
        );
        assert!(
            vals.windows(2).all(|w| w[0] != w[1]),
            "{what}: duplicate re-assert, expected strict alternation: {vals:?}"
        );
    }

    /// Drive press + hold + release + liftoff through a ready proxy with
    /// `cfg`, returning the per-node wire logs (pointer + extras).
    fn drive_click_sequence(
        frames: Vec<Result<Option<TouchFrame>>>,
        cfg: &Config,
    ) -> (TestSinks, Arc<FakeHaptic>) {
        let sinks = TestSinks::new();
        let haptic = Arc::new(FakeHaptic::new());
        let mut px = drive_ready_proxy(&sinks, &haptic, frames);
        // Four frames per sequence; extra `None`s are harmless no-ops.
        // Spaced 60 ms: release-bearing sequences must clear the
        // production debounce (rapid calls would hold the latch).
        let t0 = std::time::Instant::now();
        for i in 0..4 {
            px.handle_frame_at(cfg, spaced_at(t0, i)).unwrap();
        }
        (sinks, haptic)
    }

    fn press_hold_release_liftoff() -> Vec<Result<Option<TouchFrame>>> {
        vec![
            Ok(Some(frame(vec![finger(1, 200, 10, 20)], 0))),
            Ok(Some(frame(vec![finger(1, 200, 15, 25)], 0))),
            Ok(Some(frame(vec![finger(1, 10, 15, 25)], 0))),
            Ok(Some(frame(vec![], 0))),
        ]
    }

    #[test]
    fn left_click_exact_wire_sequence() {
        // Canonical 1-finger left click: press, hold (moved), release
        // (pressure below hysteresis), liftoff. Pins the COMPLETE stream.
        let cfg = Config::default();
        let (sinks, _) = drive_click_sequence(press_hold_release_liftoff(), &cfg);
        let got = wire_events(&sinks.pointer);
        let want: Vec<(u16, u16, i32)> = vec![
            // Press frame: touch block + BTN_TOUCH + TOOL row (1 finger) +
            // one BTN_LEFT down + SYN. Edge-triggered: no duplicate
            // re-assert, no stray releases.
            (EV_ABS, ABS_MT_SLOT, 0),
            (EV_ABS, ABS_MT_TRACKING_ID, 1),
            (EV_ABS, ABS_MT_POSITION_X, 10),
            (EV_ABS, ABS_MT_POSITION_Y, 20),
            (EV_ABS, ABS_MT_PRESSURE, 60),
            (EV_ABS, ABS_MT_TOUCH_MAJOR, 800),
            (EV_ABS, ABS_MT_TOUCH_MINOR, 800),
            (EV_ABS, ABS_MT_ORIENTATION, -2),
            (EV_KEY, BTN_TOUCH, 1),
            (EV_KEY, BTN_TOOL_FINGER, 1),
            (EV_KEY, BTN_TOOL_DOUBLETAP, 0),
            (EV_KEY, BTN_TOOL_TRIPLETAP, 0),
            (EV_KEY, BTN_TOOL_QUADTAP, 0),
            (EV_KEY, BTN_TOOL_QUINTTAP, 0),
            (EV_KEY, BTN_LEFT, 1),
            (EV_SYN, SYN_REPORT, 0),
            // Hold frame: moved +5/+5 at 100% speed; buttons unchanged, so
            // no BTN_LEFT traffic at all — only touch + BTN_TOUCH + TOOL + SYN.
            (EV_ABS, ABS_MT_SLOT, 0),
            (EV_ABS, ABS_MT_TRACKING_ID, 1),
            (EV_ABS, ABS_MT_POSITION_X, 15),
            (EV_ABS, ABS_MT_POSITION_Y, 25),
            (EV_ABS, ABS_MT_PRESSURE, 60),
            (EV_ABS, ABS_MT_TOUCH_MAJOR, 800),
            (EV_ABS, ABS_MT_TOUCH_MINOR, 800),
            (EV_ABS, ABS_MT_ORIENTATION, -2),
            (EV_KEY, BTN_TOUCH, 1),
            (EV_KEY, BTN_TOOL_FINGER, 1),
            (EV_KEY, BTN_TOOL_DOUBLETAP, 0),
            (EV_KEY, BTN_TOOL_TRIPLETAP, 0),
            (EV_KEY, BTN_TOOL_QUADTAP, 0),
            (EV_KEY, BTN_TOOL_QUINTTAP, 0),
            (EV_SYN, SYN_REPORT, 0),
            // Release frame: pressure 10 <= release 59, latch drops; the
            // contact is still present so touch stays up until liftoff.
            (EV_ABS, ABS_MT_SLOT, 0),
            (EV_ABS, ABS_MT_TRACKING_ID, 1),
            (EV_ABS, ABS_MT_POSITION_X, 15),
            (EV_ABS, ABS_MT_POSITION_Y, 25),
            (EV_ABS, ABS_MT_PRESSURE, 60),
            (EV_ABS, ABS_MT_TOUCH_MAJOR, 800),
            (EV_ABS, ABS_MT_TOUCH_MINOR, 800),
            (EV_ABS, ABS_MT_ORIENTATION, -2),
            (EV_KEY, BTN_TOUCH, 1),
            (EV_KEY, BTN_TOOL_FINGER, 1),
            (EV_KEY, BTN_TOOL_DOUBLETAP, 0),
            (EV_KEY, BTN_TOOL_TRIPLETAP, 0),
            (EV_KEY, BTN_TOOL_QUADTAP, 0),
            (EV_KEY, BTN_TOOL_QUINTTAP, 0),
            (EV_KEY, BTN_LEFT, 0),
            (EV_SYN, SYN_REPORT, 0),
            // Liftoff: slot terminated, BTN_TOUCH up, TOOL row all up,
            // trailing SYN. BTN_LEFT is already up, so it stays silent
            // (no stray release).
            (EV_ABS, ABS_MT_SLOT, 0),
            (EV_ABS, ABS_MT_TRACKING_ID, -1),
            (EV_KEY, BTN_TOUCH, 0),
            (EV_KEY, BTN_TOOL_FINGER, 0),
            (EV_KEY, BTN_TOOL_DOUBLETAP, 0),
            (EV_KEY, BTN_TOOL_TRIPLETAP, 0),
            (EV_KEY, BTN_TOOL_QUADTAP, 0),
            (EV_KEY, BTN_TOOL_QUINTTAP, 0),
            (EV_SYN, SYN_REPORT, 0),
        ];
        assert_eq!(got, want, "wire sequence drifted");
        assert_wire_pure(&got, &[BTN_LEFT, BTN_TOUCH]);
        // Edge-triggered pin: exactly one down/up pair, no re-asserts.
        let left = key_values(&sinks.pointer, BTN_LEFT);
        assert_eq!(left, vec![1, 0], "{left:?}");
        assert_strict_alternation(&left, "BTN_LEFT");
        // Extras node: framing only (one SYN per handled frame), zero keys.
        let xgot = wire_events(&sinks.extras);
        assert_eq!(
            xgot,
            vec![(EV_SYN, SYN_REPORT, 0); 4],
            "extras framing drifted"
        );
    }

    #[test]
    fn hidraw_bytes_to_exact_wire_sequence() {
        // Scripted hidraw bytes → HidrawSource decode → proxy → exact
        // uinput events. No hardware: the fd layer is the scripted reader.
        // Block layout: x=0,y=0 (b0..b3 zero except contact bit),
        // b8 = 0xC0|id → raw orientation 2 → emitted -2.
        fn raw_usb(clicks: u8, blocks: &[&[u8; 9]]) -> Vec<u8> {
            let mut r = vec![0x02, clicks, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0];
            for b in blocks {
                r.extend_from_slice(&**b);
            }
            r
        }
        let press = [0u8, 0, 0, 0x80, 10, 6, 8, 200, 0xC1];
        let hold = [0u8, 0, 0, 0x80, 10, 6, 8, 200, 0xC1];
        let release = [0u8, 0, 0, 0x80, 10, 6, 8, 10, 0xC1];
        let reports = vec![
            raw_usb(0, &[&press]),
            raw_usb(0, &[&hold]),
            raw_usb(0, &[&release]),
            raw_usb(0, &[]),
        ];
        let sinks = TestSinks::new();
        let haptic = Arc::new(FakeHaptic::new());
        let source = crate::pump::HidrawSource::new(
            "/dev/hidraw1",
            crate::pump::ScriptReader::bytes(reports),
        );
        let mut px = ClickProxy::new(
            settings(true),
            FakeGrab::ok(),
            sinks.dual(),
            SharedHaptic(Arc::clone(&haptic)),
            source,
        );
        px.set_pump_ready(true);
        px.attach(&usb_ready("/dev/hidraw1"));
        let cfg = Config::default();
        // Spaced 60 ms so the release frame clears the production
        // debounce (rapid calls would hold the latch instead).
        let t0 = std::time::Instant::now();
        for i in 0..4 {
            px.handle_frame_at(&cfg, spaced_at(t0, i)).unwrap();
        }
        let got = wire_events(&sinks.pointer);
        // Raw hidraw blocks normalize on the wire (pressure 60,
        // major/minor 800 — old-service parity, never the raw bytes).
        let want: Vec<(u16, u16, i32)> = vec![
            (EV_ABS, ABS_MT_SLOT, 0),
            (EV_ABS, ABS_MT_TRACKING_ID, 1),
            (EV_ABS, ABS_MT_POSITION_X, 0),
            (EV_ABS, ABS_MT_POSITION_Y, 0),
            (EV_ABS, ABS_MT_PRESSURE, 60),
            (EV_ABS, ABS_MT_TOUCH_MAJOR, 800),
            (EV_ABS, ABS_MT_TOUCH_MINOR, 800),
            (EV_ABS, ABS_MT_ORIENTATION, -2),
            (EV_KEY, BTN_TOUCH, 1),
            (EV_KEY, BTN_TOOL_FINGER, 1),
            (EV_KEY, BTN_TOOL_DOUBLETAP, 0),
            (EV_KEY, BTN_TOOL_TRIPLETAP, 0),
            (EV_KEY, BTN_TOOL_QUADTAP, 0),
            (EV_KEY, BTN_TOOL_QUINTTAP, 0),
            (EV_KEY, BTN_LEFT, 1),
            (EV_SYN, SYN_REPORT, 0),
            (EV_ABS, ABS_MT_SLOT, 0),
            (EV_ABS, ABS_MT_TRACKING_ID, 1),
            (EV_ABS, ABS_MT_POSITION_X, 0),
            (EV_ABS, ABS_MT_POSITION_Y, 0),
            (EV_ABS, ABS_MT_PRESSURE, 60),
            (EV_ABS, ABS_MT_TOUCH_MAJOR, 800),
            (EV_ABS, ABS_MT_TOUCH_MINOR, 800),
            (EV_ABS, ABS_MT_ORIENTATION, -2),
            (EV_KEY, BTN_TOUCH, 1),
            (EV_KEY, BTN_TOOL_FINGER, 1),
            (EV_KEY, BTN_TOOL_DOUBLETAP, 0),
            (EV_KEY, BTN_TOOL_TRIPLETAP, 0),
            (EV_KEY, BTN_TOOL_QUADTAP, 0),
            (EV_KEY, BTN_TOOL_QUINTTAP, 0),
            (EV_SYN, SYN_REPORT, 0),
            (EV_ABS, ABS_MT_SLOT, 0),
            (EV_ABS, ABS_MT_TRACKING_ID, 1),
            (EV_ABS, ABS_MT_POSITION_X, 0),
            (EV_ABS, ABS_MT_POSITION_Y, 0),
            (EV_ABS, ABS_MT_PRESSURE, 60),
            (EV_ABS, ABS_MT_TOUCH_MAJOR, 800),
            (EV_ABS, ABS_MT_TOUCH_MINOR, 800),
            (EV_ABS, ABS_MT_ORIENTATION, -2),
            (EV_KEY, BTN_TOUCH, 1),
            (EV_KEY, BTN_TOOL_FINGER, 1),
            (EV_KEY, BTN_TOOL_DOUBLETAP, 0),
            (EV_KEY, BTN_TOOL_TRIPLETAP, 0),
            (EV_KEY, BTN_TOOL_QUADTAP, 0),
            (EV_KEY, BTN_TOOL_QUINTTAP, 0),
            (EV_KEY, BTN_LEFT, 0),
            (EV_SYN, SYN_REPORT, 0),
            (EV_ABS, ABS_MT_SLOT, 0),
            (EV_ABS, ABS_MT_TRACKING_ID, -1),
            (EV_KEY, BTN_TOUCH, 0),
            (EV_KEY, BTN_TOOL_FINGER, 0),
            (EV_KEY, BTN_TOOL_DOUBLETAP, 0),
            (EV_KEY, BTN_TOOL_TRIPLETAP, 0),
            (EV_KEY, BTN_TOOL_QUADTAP, 0),
            (EV_KEY, BTN_TOOL_QUINTTAP, 0),
            (EV_SYN, SYN_REPORT, 0),
        ];
        assert_eq!(got, want, "hidraw-bytes wire sequence drifted");
        assert_wire_pure(&got, &[BTN_LEFT, BTN_TOUCH]);
        // Extras node: framing only (one SYN per handled frame), zero keys.
        let xgot = wire_events(&sinks.extras);
        assert_eq!(
            xgot,
            vec![(EV_SYN, SYN_REPORT, 0); 4],
            "extras framing drifted"
        );
    }

    #[test]
    fn right_click_wire_pure() {
        let cfg = Config::default(); // click_2 = right
        let frames = vec![
            Ok(Some(frame(
                vec![finger(1, 200, 0, 0), finger(2, 200, 9, 9)],
                0,
            ))),
            Ok(Some(frame(
                vec![finger(1, 200, 1, 1), finger(2, 200, 9, 9)],
                0,
            ))),
            Ok(Some(frame(
                vec![finger(1, 10, 1, 1), finger(2, 10, 9, 9)],
                0,
            ))),
            Ok(Some(frame(vec![], 0))),
        ];
        let (sinks, _) = drive_click_sequence(frames, &cfg);
        let got = wire_events(&sinks.pointer);
        assert_wire_pure(&got, &[BTN_RIGHT, BTN_TOUCH]);
        assert!(!got.contains(&(EV_KEY, BTN_LEFT, 1)), "{got:?}");
        // Exact down/up pair: strengthens first/last to a full pin, and
        // strict alternation rules out duplicate re-asserts.
        let right = key_values(&sinks.pointer, BTN_RIGHT);
        assert_eq!(right, vec![1, 0], "{right:?}");
        assert_strict_alternation(&right, "BTN_RIGHT");
        // Extras node: framing only (one SYN per handled frame), zero keys.
        assert_eq!(
            wire_events(&sinks.extras),
            vec![(EV_SYN, SYN_REPORT, 0); 4],
            "extras framing drifted"
        );
    }

    #[test]
    fn right_partial_lift_releases_without_stale_hold() {
        // Ghostty right-click stick: 2-finger press, then ONE finger lifts
        // (omitted from the frame, as the device reports it) while the
        // other keeps sliding lightly. The release (BTN_RIGHT 0 + SYN)
        // must land on the pressure-drop frame — exactly one 1→0 pair —
        // so no motion frame carries a stale held button (drag in ghostty).
        let cfg = Config::default(); // click_2 = right
        let frames = vec![
            Ok(Some(frame(
                vec![finger(1, 200, 0, 0), finger(2, 200, 9, 9)],
                0,
            ))),
            Ok(Some(frame(vec![finger(1, 10, 4, 4)], 0))),
            Ok(Some(frame(vec![finger(1, 10, 8, 8)], 0))),
            Ok(Some(frame(vec![], 0))),
        ];
        let uinput = TestSinks::new();
        let haptic = Arc::new(FakeHaptic::new());
        let mut px = drive_ready_proxy(&uinput, &haptic, frames);
        let t0 = std::time::Instant::now();
        px.handle_frame_at(&cfg, spaced_at(t0, 0)).unwrap();
        px.handle_frame_at(&cfg, spaced_at(t0, 1)).unwrap();
        // Release lands on the pressure-drop frame, BEFORE the motion and
        // liftoff frames: no stale hold across motion.
        assert_eq!(
            key_values(&uinput.pointer, BTN_RIGHT),
            vec![1, 0],
            "BTN_RIGHT up delayed past the lift (stale hold across motion)"
        );
        px.handle_frame_at(&cfg, spaced_at(t0, 2)).unwrap();
        px.handle_frame_at(&cfg, spaced_at(t0, 3)).unwrap();
        let got = wire_events(&uinput.pointer);
        assert_wire_pure(&got, &[BTN_RIGHT, BTN_TOUCH]);
        let right = key_values(&uinput.pointer, BTN_RIGHT);
        assert_eq!(right, vec![1, 0], "{right:?}");
        assert_strict_alternation(&right, "BTN_RIGHT");
        // Extras node: framing only (one SYN per handled frame), zero keys.
        assert_eq!(
            wire_events(&uinput.extras),
            vec![(EV_SYN, SYN_REPORT, 0); 4],
            "extras framing drifted"
        );
        assert_strict_alternation(&right, "BTN_RIGHT");
        // Motion flowed (cursor tracked the sliding finger) across the
        // partial-lift frames: press frame carries both fingers (0 and 9),
        // then slot 0 walks 0→4→8.
        let xs: Vec<i32> = got
            .iter()
            .copied()
            .filter(|&(t, c, _)| t == EV_ABS && c == ABS_MT_POSITION_X)
            .map(|(_, _, v)| v)
            .collect();
        assert_eq!(xs, vec![0, 9, 4, 8], "motion stalled across lift: {xs:?}");
        // Trailing SYN after the key-up: exactly one SYN per handled frame
        // on the pointer node.
        let syns = got
            .iter()
            .filter(|&&(t, c, v)| t == EV_SYN && c == SYN_REPORT && v == 0)
            .count();
        assert_eq!(syns, 4, "exactly one SYN per frame: {got:?}");
    }

    /// 2 s press+move at device cadence (220 frames × 9 ms ≈ 110 Hz):
    /// firm press, continuous +2 px deltas, then liftoff.
    fn two_second_drag_frames() -> Vec<Result<Option<TouchFrame>>> {
        let mut frames: Vec<Result<Option<TouchFrame>>> = (0..220)
            .map(|i| Ok(Some(frame(vec![finger(1, 200, i * 2, 0)], 0))))
            .collect();
        frames.push(Ok(Some(frame(vec![finger(1, 10, 440, 0)], 0))));
        frames.push(Ok(Some(frame(vec![], 0))));
        frames
    }

    #[test]
    fn press_and_move_two_seconds_uninterrupted() {
        // BUG-2 contract: a firm 2 s press+drag must stream motion on every
        // frame with exactly one BTN_LEFT 1→0 pair (down at press, up at
        // liftoff) — no stalls, no spurious release/re-press pairs — plus
        // exactly one SYN per handled frame.
        let cfg = Config::default();
        let frames = two_second_drag_frames();
        let n_frames = frames.len();
        let sinks = TestSinks::new();
        let haptic = Arc::new(FakeHaptic::new());
        let mut px = drive_ready_proxy(&sinks, &haptic, frames);
        // Device cadence: 9 ms logical steps (≈110 Hz), no wall sleeps.
        let t0 = std::time::Instant::now();
        for i in 0..n_frames as u64 {
            px.handle_frame_at(&cfg, t0 + std::time::Duration::from_millis(i * 9))
                .unwrap();
        }
        let got = wire_events(&sinks.pointer);
        assert_wire_pure(&got, &[BTN_LEFT, BTN_TOUCH]);
        let left = key_values(&sinks.pointer, BTN_LEFT);
        assert_eq!(left, vec![1, 0], "{left:?}");
        assert_strict_alternation(&left, "BTN_LEFT");
        let xs: Vec<i32> = got
            .iter()
            .copied()
            .filter(|&(t, c, _)| t == EV_ABS && c == ABS_MT_POSITION_X)
            .map(|(_, _, v)| v)
            .collect();
        assert_eq!(xs.len(), 221, "motion stalled mid-drag: {xs:?}");
        for w in xs.windows(2) {
            assert_eq!(w[1] - w[0], 2, "motion gap/jump mid-drag: {xs:?}");
        }
        let syns = got
            .iter()
            .filter(|&&(t, c, v)| t == EV_SYN && c == SYN_REPORT && v == 0)
            .count();
        assert_eq!(syns, n_frames, "exactly one SYN per frame: {got:?}");
        // Extras node: framing only (one SYN per handled frame), zero keys.
        assert_eq!(
            wire_events(&sinks.extras),
            vec![(EV_SYN, SYN_REPORT, 0); n_frames],
            "extras framing drifted"
        );
    }

    /// Haptic sender with a blocking actuator round-trip per edge (slow
    /// firmware / wrong-interface NAK + fallback, ~10 ms each). Buffers
    /// are shared so tests can read the stream after the async worker
    /// drains (drop joins the worker).
    struct SlowHaptic {
        calls: Arc<Mutex<Vec<(u8, u8, bool)>>>,
        releases: Arc<Mutex<Vec<(u8, bool)>>>,
    }

    impl HapticSender for SlowHaptic {
        fn click_feedback(&self, pressure: u8, intensity: u8, quiet: bool) -> Result<()> {
            std::thread::sleep(std::time::Duration::from_millis(10));
            self.calls
                .lock()
                .unwrap()
                .push((pressure, intensity, quiet));
            Ok(())
        }
        fn release_feedback(&self, down_pressure: u8, quiet: bool) -> Result<()> {
            std::thread::sleep(std::time::Duration::from_millis(10));
            self.releases.lock().unwrap().push((down_pressure, quiet));
            Ok(())
        }
    }

    /// Hovering pressure profile: firm press decaying into a hover around
    /// the release point with dips below it (noisy slide). Per the
    /// threshold spec each dip IS a release, so the wire carries
    /// release/re-press pairs — but motion must never stall on them, even
    /// when every actuator round-trip blocks.
    fn hover_drag_frames() -> Vec<Result<Option<TouchFrame>>> {
        (0..220)
            .map(|i| {
                let p = if i < 30 {
                    200 - (i as u8) * 4
                } else {
                    [66u8, 62, 68, 58, 64, 60, 66, 57, 63, 61][i % 10]
                };
                Ok(Some(frame(vec![finger(1, p, (i as i32) * 2, 0)], 0)))
            })
            .collect()
    }

    #[test]
    fn slow_actuator_never_stalls_motion_frames() {
        // BUG-2 stall: with a 10 ms blocking actuator round-trip per edge,
        // a 2 s hover-drag (~38 edges) must still retire all 220 motion
        // frames promptly. Synchronous haptic on the pump thread costs
        // ~380 ms here and stalls motion in bursts (owner: 150-800 ms
        // freezes); the async actuator path must stay far under budget.
        let cfg = Config::default();
        let sinks = TestSinks::new();
        let slow = SlowHaptic {
            calls: Arc::new(Mutex::new(Vec::new())),
            releases: Arc::new(Mutex::new(Vec::new())),
        };
        let mut px = ClickProxy::new(
            settings(true),
            FakeGrab::ok(),
            sinks.dual(),
            AsyncHaptic::new(slow),
            ScriptSource::new(hover_drag_frames()),
        );
        px.set_pump_ready(true);
        px.attach(&usb_ready("/dev/hidraw1"));
        let t0 = std::time::Instant::now();
        for i in 0..220u64 {
            px.handle_frame_at(&cfg, t0 + std::time::Duration::from_millis(i * 9))
                .unwrap();
        }
        let elapsed = t0.elapsed();
        let xs: Vec<i32> = wire_events(&sinks.pointer)
            .iter()
            .copied()
            .filter(|&(t, c, _)| t == EV_ABS && c == ABS_MT_POSITION_X)
            .map(|(_, _, v)| v)
            .collect();
        assert_eq!(xs.len(), 220, "motion stalled mid-drag");
        assert!(
            elapsed < std::time::Duration::from_millis(250),
            "pump stalled on actuator I/O: {elapsed:?} for 220 frames"
        );
        // The worker still fires every pulse, in order, off the hot path
        // (drop joins the drain; assertions below see the full stream).
    }

    #[test]
    fn async_haptic_preserves_down_up_order() {
        // FIFO: the worker must fire pulses in enqueue order so down/up
        // pairs stay paired even though the pump never waits.
        let rec = Arc::new(FakeHaptic::new());
        let ah = AsyncHaptic::new(SharedHaptic(Arc::clone(&rec)));
        ah.click_feedback(64, 5, false).unwrap();
        ah.release_feedback(64, false).unwrap();
        ah.click_feedback(64, 5, true).unwrap();
        ah.release_feedback(64, true).unwrap();
        drop(ah); // join: queue fully drained before return
        assert_eq!(
            rec.calls.lock().unwrap().as_slice(),
            &[(64, 5, false), (64, 5, true)]
        );
        assert_eq!(
            rec.releases.lock().unwrap().as_slice(),
            &[(64, false), (64, true)]
        );
    }

    #[test]
    fn async_haptic_enqueue_never_waits() {
        // Even with the worker stuck inside a 10 ms round-trip, enqueueing
        // four pulses must return ~instantly (a synchronous sender would
        // cost 40 ms here).
        let calls = Arc::new(Mutex::new(Vec::new()));
        let releases = Arc::new(Mutex::new(Vec::new()));
        let ah = AsyncHaptic::new(SlowHaptic {
            calls: Arc::clone(&calls),
            releases: Arc::clone(&releases),
        });
        let t0 = std::time::Instant::now();
        ah.click_feedback(64, 5, false).unwrap();
        ah.click_feedback(64, 5, false).unwrap();
        ah.release_feedback(64, false).unwrap();
        ah.release_feedback(64, false).unwrap();
        assert!(
            t0.elapsed() < std::time::Duration::from_millis(5),
            "enqueue blocked on actuator I/O: {:?}",
            t0.elapsed()
        );
        drop(ah); // drain + join before asserting the full stream landed
        assert_eq!(calls.lock().unwrap().len(), 2);
        assert_eq!(releases.lock().unwrap().len(), 2);
    }

    #[test]
    fn middle_click_wire_pure() {
        let cfg = Config::default(); // click_3 = middle
        let fingers = || {
            vec![
                finger(1, 200, 0, 0),
                finger(2, 200, 9, 9),
                finger(3, 200, 5, 5),
            ]
        };
        let frames = vec![
            Ok(Some(frame(fingers(), 0))),
            Ok(Some(frame(fingers(), 0))),
            Ok(Some(frame(
                vec![
                    finger(1, 10, 0, 0),
                    finger(2, 10, 9, 9),
                    finger(3, 10, 5, 5),
                ],
                0,
            ))),
            Ok(Some(frame(vec![], 0))),
        ];
        let (sinks, _) = drive_click_sequence(frames, &cfg);
        let got = wire_events(&sinks.pointer);
        assert_wire_pure(&got, &[BTN_MIDDLE, BTN_TOUCH]);
        assert!(!got.contains(&(EV_KEY, BTN_LEFT, 1)), "{got:?}");
        let middle = key_values(&sinks.pointer, BTN_MIDDLE);
        assert_eq!(middle, vec![1, 0], "{middle:?}");
        assert_strict_alternation(&middle, "BTN_MIDDLE");
        // Extras node: framing only (one SYN per handled frame), zero keys.
        assert_eq!(
            wire_events(&sinks.extras),
            vec![(EV_SYN, SYN_REPORT, 0); 4],
            "extras framing drifted"
        );
    }

    #[test]
    fn single_finger_click_with_physical_bit_exact() {
        // 1-finger left click with the hardware button bit set (real press):
        // the latch holds left so BTN_LEFT still fires exactly once — the
        // physical gate only suppresses LEFT under fingers with no left
        // latch, never a genuine left press. Guards the multi-finger chord
        // fix against over-suppression. Pins the COMPLETE stream.
        let cfg = Config::default(); // click_1 = left
        let frames = vec![
            Ok(Some(frame(vec![finger(1, 200, 10, 20)], 0x01))),
            Ok(Some(frame(vec![finger(1, 200, 10, 20)], 0x01))),
            Ok(Some(frame(vec![finger(1, 10, 10, 20)], 0x01))),
            Ok(Some(frame(vec![], 0))),
        ];
        let (sinks, _) = drive_click_sequence(frames, &cfg);
        let got = wire_events(&sinks.pointer);
        let touch_block = || -> Vec<(u16, u16, i32)> {
            vec![
                (EV_ABS, ABS_MT_SLOT, 0),
                (EV_ABS, ABS_MT_TRACKING_ID, 1),
                (EV_ABS, ABS_MT_POSITION_X, 10),
                (EV_ABS, ABS_MT_POSITION_Y, 20),
                (EV_ABS, ABS_MT_PRESSURE, 60),
                (EV_ABS, ABS_MT_TOUCH_MAJOR, 800),
                (EV_ABS, ABS_MT_TOUCH_MINOR, 800),
                (EV_ABS, ABS_MT_ORIENTATION, -2),
            ]
        };
        let tool_finger = || -> Vec<(u16, u16, i32)> {
            vec![
                (EV_KEY, BTN_TOUCH, 1),
                (EV_KEY, BTN_TOOL_FINGER, 1),
                (EV_KEY, BTN_TOOL_DOUBLETAP, 0),
                (EV_KEY, BTN_TOOL_TRIPLETAP, 0),
                (EV_KEY, BTN_TOOL_QUADTAP, 0),
                (EV_KEY, BTN_TOOL_QUINTTAP, 0),
            ]
        };
        let mut want: Vec<(u16, u16, i32)> = Vec::new();
        // Press: touch + TOOL row + LEFT down + SYN.
        want.extend(touch_block());
        want.extend(tool_finger());
        want.push((EV_KEY, BTN_LEFT, 1));
        want.push((EV_SYN, SYN_REPORT, 0));
        // Hold: touch + TOOL + SYN only.
        want.extend(touch_block());
        want.extend(tool_finger());
        want.push((EV_SYN, SYN_REPORT, 0));
        // Release: LEFT up + SYN (no flutter from the still-set button bit).
        want.extend(touch_block());
        want.extend(tool_finger());
        want.push((EV_KEY, BTN_LEFT, 0));
        want.push((EV_SYN, SYN_REPORT, 0));
        // Liftoff: slot terminated, TOUCH up, TOOL row clear, SYN.
        want.extend(vec![
            (EV_ABS, ABS_MT_SLOT, 0),
            (EV_ABS, ABS_MT_TRACKING_ID, -1),
            (EV_KEY, BTN_TOUCH, 0),
            (EV_KEY, BTN_TOOL_FINGER, 0),
            (EV_KEY, BTN_TOOL_DOUBLETAP, 0),
            (EV_KEY, BTN_TOOL_TRIPLETAP, 0),
            (EV_KEY, BTN_TOOL_QUADTAP, 0),
            (EV_KEY, BTN_TOOL_QUINTTAP, 0),
            (EV_SYN, SYN_REPORT, 0),
        ]);
        assert_eq!(got, want, "1-finger physical wire sequence drifted");
        assert_wire_pure(&got, &[BTN_LEFT, BTN_TOUCH]);
        let left = key_values(&sinks.pointer, BTN_LEFT);
        assert_eq!(left, vec![1, 0], "{left:?}");
        assert_strict_alternation(&left, "BTN_LEFT");
        assert_eq!(
            wire_events(&sinks.extras),
            vec![(EV_SYN, SYN_REPORT, 0); 4],
            "extras framing drifted"
        );
    }

    #[test]
    fn two_finger_click_exact_wire_sequence() {
        // Old-exact 2-finger right click with the hardware button bit set
        // (real press: `clicks & 1 == 1` while deflecting the pad). The old
        // driver derives buttons purely from the pressure latch + finger
        // count and never ORs the physical bit into BTN_LEFT, so the wire
        // carries RIGHT only — a LEFT+RIGHT chord reads as hold/drag in
        // strict apps (owner ghostty 2026-10-16). Pins the COMPLETE stream.
        let cfg = Config::default(); // click_2 = right
        let frames = vec![
            Ok(Some(frame(
                vec![finger(1, 200, 0, 0), finger(2, 200, 9, 9)],
                0x01,
            ))),
            Ok(Some(frame(
                vec![finger(1, 200, 0, 0), finger(2, 200, 9, 9)],
                0x01,
            ))),
            Ok(Some(frame(
                vec![finger(1, 10, 0, 0), finger(2, 10, 9, 9)],
                0x01,
            ))),
            Ok(Some(frame(vec![], 0))),
        ];
        let (sinks, _) = drive_click_sequence(frames, &cfg);
        let got = wire_events(&sinks.pointer);
        let touch_block = |x0: i32, y0: i32, x1: i32, y1: i32| -> Vec<(u16, u16, i32)> {
            vec![
                (EV_ABS, ABS_MT_SLOT, 0),
                (EV_ABS, ABS_MT_TRACKING_ID, 1),
                (EV_ABS, ABS_MT_POSITION_X, x0),
                (EV_ABS, ABS_MT_POSITION_Y, y0),
                (EV_ABS, ABS_MT_PRESSURE, 60),
                (EV_ABS, ABS_MT_TOUCH_MAJOR, 800),
                (EV_ABS, ABS_MT_TOUCH_MINOR, 800),
                (EV_ABS, ABS_MT_ORIENTATION, -2),
                (EV_ABS, ABS_MT_SLOT, 1),
                (EV_ABS, ABS_MT_TRACKING_ID, 2),
                (EV_ABS, ABS_MT_POSITION_X, x1),
                (EV_ABS, ABS_MT_POSITION_Y, y1),
                (EV_ABS, ABS_MT_PRESSURE, 60),
                (EV_ABS, ABS_MT_TOUCH_MAJOR, 800),
                (EV_ABS, ABS_MT_TOUCH_MINOR, 800),
                (EV_ABS, ABS_MT_ORIENTATION, -2),
            ]
        };
        let tool_double = || -> Vec<(u16, u16, i32)> {
            vec![
                (EV_KEY, BTN_TOUCH, 1),
                (EV_KEY, BTN_TOOL_FINGER, 0),
                (EV_KEY, BTN_TOOL_DOUBLETAP, 1),
                (EV_KEY, BTN_TOOL_TRIPLETAP, 0),
                (EV_KEY, BTN_TOOL_QUADTAP, 0),
                (EV_KEY, BTN_TOOL_QUINTTAP, 0),
            ]
        };
        let mut want: Vec<(u16, u16, i32)> = Vec::new();
        // Press: touch + TOOL row + RIGHT down + SYN. No LEFT despite the
        // physical bit — the latch holds right, not left.
        want.extend(touch_block(0, 0, 9, 9));
        want.extend(tool_double());
        want.push((EV_KEY, BTN_RIGHT, 1));
        want.push((EV_SYN, SYN_REPORT, 0));
        // Hold: touch + TOOL + SYN only, zero button traffic.
        want.extend(touch_block(0, 0, 9, 9));
        want.extend(tool_double());
        want.push((EV_SYN, SYN_REPORT, 0));
        // Release: latch drops, RIGHT up + SYN. Still no LEFT.
        want.extend(touch_block(0, 0, 9, 9));
        want.extend(tool_double());
        want.push((EV_KEY, BTN_RIGHT, 0));
        want.push((EV_SYN, SYN_REPORT, 0));
        // Liftoff: both slots terminated, TOUCH up, TOOL row all up, SYN.
        want.extend(vec![
            (EV_ABS, ABS_MT_SLOT, 0),
            (EV_ABS, ABS_MT_TRACKING_ID, -1),
            (EV_ABS, ABS_MT_SLOT, 1),
            (EV_ABS, ABS_MT_TRACKING_ID, -1),
            (EV_KEY, BTN_TOUCH, 0),
            (EV_KEY, BTN_TOOL_FINGER, 0),
            (EV_KEY, BTN_TOOL_DOUBLETAP, 0),
            (EV_KEY, BTN_TOOL_TRIPLETAP, 0),
            (EV_KEY, BTN_TOOL_QUADTAP, 0),
            (EV_KEY, BTN_TOOL_QUINTTAP, 0),
            (EV_SYN, SYN_REPORT, 0),
        ]);
        assert_eq!(got, want, "2-finger wire sequence drifted");
        assert_wire_pure(&got, &[BTN_RIGHT, BTN_TOUCH]);
        // No BTN_LEFT traffic of any value on the whole stream.
        assert!(
            !got.iter().any(|&(t, c, _)| t == EV_KEY && c == BTN_LEFT),
            "phantom BTN_LEFT alongside RIGHT: {got:?}"
        );
        let right = key_values(&sinks.pointer, BTN_RIGHT);
        assert_eq!(right, vec![1, 0], "{right:?}");
        assert_strict_alternation(&right, "BTN_RIGHT");
        // Extras node: framing only (one SYN per handled frame), zero keys.
        assert_eq!(
            wire_events(&sinks.extras),
            vec![(EV_SYN, SYN_REPORT, 0); 4],
            "extras framing drifted"
        );
    }

    #[test]
    fn three_finger_click_exact_wire_sequence() {
        // Old-exact 3-finger middle click with the hardware button bit set
        // (same physical-chord hazard as the 2-finger test: LEFT must never
        // ride alongside MIDDLE). Pins the COMPLETE stream.
        let cfg = Config::default(); // click_3 = middle
        let fingers = || {
            vec![
                finger(1, 200, 0, 0),
                finger(2, 200, 9, 9),
                finger(3, 200, 5, 5),
            ]
        };
        let release = || {
            vec![
                finger(1, 10, 0, 0),
                finger(2, 10, 9, 9),
                finger(3, 10, 5, 5),
            ]
        };
        let frames = vec![
            Ok(Some(frame(fingers(), 0x01))),
            Ok(Some(frame(fingers(), 0x01))),
            Ok(Some(frame(release(), 0x01))),
            Ok(Some(frame(vec![], 0))),
        ];
        let (sinks, _) = drive_click_sequence(frames, &cfg);
        let got = wire_events(&sinks.pointer);
        let touch_block = || -> Vec<(u16, u16, i32)> {
            vec![
                (EV_ABS, ABS_MT_SLOT, 0),
                (EV_ABS, ABS_MT_TRACKING_ID, 1),
                (EV_ABS, ABS_MT_POSITION_X, 0),
                (EV_ABS, ABS_MT_POSITION_Y, 0),
                (EV_ABS, ABS_MT_PRESSURE, 60),
                (EV_ABS, ABS_MT_TOUCH_MAJOR, 800),
                (EV_ABS, ABS_MT_TOUCH_MINOR, 800),
                (EV_ABS, ABS_MT_ORIENTATION, -2),
                (EV_ABS, ABS_MT_SLOT, 1),
                (EV_ABS, ABS_MT_TRACKING_ID, 2),
                (EV_ABS, ABS_MT_POSITION_X, 9),
                (EV_ABS, ABS_MT_POSITION_Y, 9),
                (EV_ABS, ABS_MT_PRESSURE, 60),
                (EV_ABS, ABS_MT_TOUCH_MAJOR, 800),
                (EV_ABS, ABS_MT_TOUCH_MINOR, 800),
                (EV_ABS, ABS_MT_ORIENTATION, -2),
                (EV_ABS, ABS_MT_SLOT, 2),
                (EV_ABS, ABS_MT_TRACKING_ID, 3),
                (EV_ABS, ABS_MT_POSITION_X, 5),
                (EV_ABS, ABS_MT_POSITION_Y, 5),
                (EV_ABS, ABS_MT_PRESSURE, 60),
                (EV_ABS, ABS_MT_TOUCH_MAJOR, 800),
                (EV_ABS, ABS_MT_TOUCH_MINOR, 800),
                (EV_ABS, ABS_MT_ORIENTATION, -2),
            ]
        };
        let tool_triple = || -> Vec<(u16, u16, i32)> {
            vec![
                (EV_KEY, BTN_TOUCH, 1),
                (EV_KEY, BTN_TOOL_FINGER, 0),
                (EV_KEY, BTN_TOOL_DOUBLETAP, 0),
                (EV_KEY, BTN_TOOL_TRIPLETAP, 1),
                (EV_KEY, BTN_TOOL_QUADTAP, 0),
                (EV_KEY, BTN_TOOL_QUINTTAP, 0),
            ]
        };
        let mut want: Vec<(u16, u16, i32)> = Vec::new();
        // Press: touch + TOOL row + MIDDLE down + SYN. No LEFT.
        want.extend(touch_block());
        want.extend(tool_triple());
        want.push((EV_KEY, BTN_MIDDLE, 1));
        want.push((EV_SYN, SYN_REPORT, 0));
        // Hold: touch + TOOL + SYN only.
        want.extend(touch_block());
        want.extend(tool_triple());
        want.push((EV_SYN, SYN_REPORT, 0));
        // Release: MIDDLE up + SYN. Still no LEFT.
        want.extend(touch_block());
        want.extend(tool_triple());
        want.push((EV_KEY, BTN_MIDDLE, 0));
        want.push((EV_SYN, SYN_REPORT, 0));
        // Liftoff: all three slots terminated, TOUCH up, TOOL row clear, SYN.
        want.extend(vec![
            (EV_ABS, ABS_MT_SLOT, 0),
            (EV_ABS, ABS_MT_TRACKING_ID, -1),
            (EV_ABS, ABS_MT_SLOT, 1),
            (EV_ABS, ABS_MT_TRACKING_ID, -1),
            (EV_ABS, ABS_MT_SLOT, 2),
            (EV_ABS, ABS_MT_TRACKING_ID, -1),
            (EV_KEY, BTN_TOUCH, 0),
            (EV_KEY, BTN_TOOL_FINGER, 0),
            (EV_KEY, BTN_TOOL_DOUBLETAP, 0),
            (EV_KEY, BTN_TOOL_TRIPLETAP, 0),
            (EV_KEY, BTN_TOOL_QUADTAP, 0),
            (EV_KEY, BTN_TOOL_QUINTTAP, 0),
            (EV_SYN, SYN_REPORT, 0),
        ]);
        assert_eq!(got, want, "3-finger wire sequence drifted");
        assert_wire_pure(&got, &[BTN_MIDDLE, BTN_TOUCH]);
        assert!(
            !got.iter().any(|&(t, c, _)| t == EV_KEY && c == BTN_LEFT),
            "phantom BTN_LEFT alongside MIDDLE: {got:?}"
        );
        let middle = key_values(&sinks.pointer, BTN_MIDDLE);
        assert_eq!(middle, vec![1, 0], "{middle:?}");
        assert_strict_alternation(&middle, "BTN_MIDDLE");
        // Extras node: framing only (one SYN per handled frame), zero keys.
        assert_eq!(
            wire_events(&sinks.extras),
            vec![(EV_SYN, SYN_REPORT, 0); 4],
            "extras framing drifted"
        );
    }

    #[test]
    fn empty_click_wire_pure() {
        let cfg = Config {
            click_1: "empty".into(),
            ..Config::default()
        };
        let (sinks, haptic) = drive_click_sequence(press_hold_release_liftoff(), &cfg);
        let got = wire_events(&sinks.pointer);
        // Haptic only: BTN_TOUCH is the sole EV_KEY allowed on the wire.
        assert_wire_pure(&got, &[BTN_TOUCH]);
        assert!(!haptic.calls.lock().unwrap().is_empty());
        // Extras node: framing only (one SYN per handled frame), zero keys.
        assert_eq!(
            wire_events(&sinks.extras),
            vec![(EV_SYN, SYN_REPORT, 0); 4],
            "extras framing drifted"
        );
    }

    #[test]
    fn none_click_wire_pure() {
        let cfg = Config {
            click_1: "none".into(),
            ..Config::default()
        };
        let (sinks, haptic) = drive_click_sequence(press_hold_release_liftoff(), &cfg);
        let got = wire_events(&sinks.pointer);
        // Swallowed gesture: BTN_TOUCH only, no haptic, no buttons.
        assert_wire_pure(&got, &[BTN_TOUCH]);
        assert!(haptic.calls.lock().unwrap().is_empty());
        // Extras node: framing only (one SYN per handled frame), zero keys.
        assert_eq!(
            wire_events(&sinks.extras),
            vec![(EV_SYN, SYN_REPORT, 0); 4],
            "extras framing drifted"
        );
    }

    #[test]
    fn combo_click_wire_pure() {
        // ctrl+c: the combo codes ride the EXTRAS node (each down once
        // and up once — a missing key-up would auto-repeat downstream,
        // turning one press into a character run) while the pointer node
        // carries touch framing only, never the combo codes.
        let cfg = Config {
            click_1: "ctrl+c".into(),
            ..Config::default()
        };
        let (sinks, _) = drive_click_sequence(press_hold_release_liftoff(), &cfg);
        let got = wire_events(&sinks.pointer);
        assert_wire_pure(&got, &[BTN_TOUCH]);
        assert!(!got.contains(&(EV_KEY, BTN_LEFT, 1)), "{got:?}");
        assert!(
            !got.iter()
                .any(|&(t, c, _)| t == EV_KEY && (c == 29 || c == 46)),
            "combo codes leaked onto the pointer node: {got:?}"
        );
        let xgot = wire_events(&sinks.extras);
        assert_eq!(
            xgot,
            vec![
                (EV_KEY, 29, 1),
                (EV_KEY, 46, 1),
                (EV_SYN, SYN_REPORT, 0),
                (EV_SYN, SYN_REPORT, 0),
                (EV_KEY, 29, 0),
                (EV_KEY, 46, 0),
                (EV_SYN, SYN_REPORT, 0),
                (EV_SYN, SYN_REPORT, 0),
            ],
            "extras combo sequence drifted"
        );
        assert_wire_pure(&xgot, &[29, 46]);
        let ctrl = key_values(&sinks.extras, 29);
        assert_eq!(ctrl, vec![1, 0], "{ctrl:?}");
        assert_strict_alternation(&ctrl, "ctrl");
        let c = key_values(&sinks.extras, 46);
        assert_eq!(c, vec![1, 0], "{c:?}");
        assert_strict_alternation(&c, "c");
    }

    #[test]
    fn new_keys_work_in_combos_wire_pure() {
        // `+` combos accept the new keys too: ctrl+refresh emits exactly
        // the combo codes on extras (+ touch framing on the pointer node),
        // each paired down/up once.
        let cfg = Config {
            click_1: "ctrl+refresh".into(),
            ..Config::default()
        };
        let (sinks, _) = drive_click_sequence(press_hold_release_liftoff(), &cfg);
        let got = wire_events(&sinks.pointer);
        assert_wire_pure(&got, &[BTN_TOUCH]);
        assert!(!got.contains(&(EV_KEY, BTN_LEFT, 1)), "{got:?}");
        let xgot = wire_events(&sinks.extras);
        assert_wire_pure(&xgot, &[29, 173]);
        let ctrl = key_values(&sinks.extras, 29);
        assert_eq!(ctrl, vec![1, 0], "{ctrl:?}");
        assert_strict_alternation(&ctrl, "ctrl");
        let refresh = key_values(&sinks.extras, 173);
        assert_eq!(refresh, vec![1, 0], "{refresh:?}");
        assert_strict_alternation(&refresh, "refresh");
    }

    #[test]
    fn excluded_fn_and_obscure_keys_stay_unmapped() {
        // Driver-internal / confusing keys must not emit (see
        // `key_token_to_code` docs for reasons).
        for tok in [
            "fn",
            "fn_esc",
            "fn_f1",
            "fn_f12",
            "fn_1",
            "fn_right_shift",
            "acstop",
            "props",
        ] {
            assert_eq!(key_token_to_code(tok), None, "{tok}");
        }
        // `back`/`forward` stay mouse buttons, never browser AC codes.
        assert_eq!(key_token_to_code("back"), Some((EV_KEY, BTN_SIDE)));
        assert_eq!(key_token_to_code("forward"), Some((EV_KEY, BTN_EXTRA)));
    }

    #[test]
    fn physical_button_wire_pure() {
        // No fingers, hardware button bit only: BTN_LEFT follows the
        // `clicks & 1` bit, BTN_TOUCH mirrors it, nothing else on EV_KEY.
        let cfg = Config::default();
        let frames = vec![
            Ok(Some(frame(vec![], 0x01))),
            Ok(Some(frame(vec![], 0x01))),
            Ok(Some(frame(vec![], 0x00))),
            Ok(Some(frame(vec![], 0x00))),
        ];
        let (sinks, _) = drive_click_sequence(frames, &cfg);
        let got = wire_events(&sinks.pointer);
        assert_wire_pure(&got, &[BTN_LEFT, BTN_TOUCH]);
        let left = key_values(&sinks.pointer, BTN_LEFT);
        assert_eq!(left, vec![1, 0], "{left:?}");
        assert_strict_alternation(&left, "BTN_LEFT");
        // Extras node: framing only (one SYN per handled frame), zero keys.
        assert_eq!(
            wire_events(&sinks.extras),
            vec![(EV_SYN, SYN_REPORT, 0); 4],
            "extras framing drifted"
        );
    }

    #[test]
    fn physical_hold_suppresses_latch_release_flutter() {
        // Latch releases while the hardware button still holds: no
        // BTN_LEFT 0-then-1 flutter in one frame, the wire simply stays
        // down until the physical bit drops — still exactly one pair.
        let frames = vec![
            Ok(Some(frame(vec![finger(1, 200, 0, 0)], 0x01))),
            Ok(Some(frame(vec![finger(1, 10, 0, 0)], 0x01))),
            Ok(Some(frame(vec![finger(1, 10, 0, 0)], 0x00))),
            Ok(Some(frame(vec![], 0x00))),
        ];
        let (sinks, _) = drive_click_sequence(frames, &Config::default());
        let got = wire_events(&sinks.pointer);
        assert_wire_pure(&got, &[BTN_LEFT, BTN_TOUCH]);
        let left = key_values(&sinks.pointer, BTN_LEFT);
        assert_eq!(left, vec![1, 0], "{left:?}");
        assert_strict_alternation(&left, "BTN_LEFT");
        // Extras node: framing only (one SYN per handled frame), zero keys.
        assert_eq!(
            wire_events(&sinks.extras),
            vec![(EV_SYN, SYN_REPORT, 0); 4],
            "extras framing drifted"
        );
    }

    #[test]
    fn force_cycle_wire_pure() {
        // First hold (left) -> force edge (forward) -> force release back
        // to first hold -> full release: BTN_LEFT rides the pointer node,
        // BTN_EXTRA the extras node, each paired down/up exactly once.
        let cfg = force_cfg();
        let frames = vec![
            Ok(Some(frame(vec![finger(1, 70, 0, 0)], 0))),
            Ok(Some(frame(vec![finger(1, 100, 0, 0)], 0))),
            Ok(Some(frame(vec![finger(1, 70, 0, 0)], 0))),
            Ok(Some(frame(vec![finger(1, 10, 0, 0)], 0))),
        ];
        let (sinks, _) = drive_click_sequence(frames, &cfg);
        let got = wire_events(&sinks.pointer);
        assert_wire_pure(&got, &[BTN_LEFT, BTN_TOUCH]);
        let left = key_values(&sinks.pointer, BTN_LEFT);
        assert_eq!(left, vec![1, 0], "{left:?}");
        assert_strict_alternation(&left, "BTN_LEFT");
        let xgot = wire_events(&sinks.extras);
        assert_eq!(
            xgot,
            vec![
                (EV_SYN, SYN_REPORT, 0),
                (EV_KEY, BTN_EXTRA, 1),
                (EV_SYN, SYN_REPORT, 0),
                (EV_KEY, BTN_EXTRA, 0),
                (EV_SYN, SYN_REPORT, 0),
                (EV_SYN, SYN_REPORT, 0),
            ],
            "extras force sequence drifted"
        );
        assert_wire_pure(&xgot, &[BTN_EXTRA]);
        let extra = key_values(&sinks.extras, BTN_EXTRA);
        assert_eq!(extra, vec![1, 0], "{extra:?}");
        assert_strict_alternation(&extra, "BTN_EXTRA");
    }

    #[test]
    fn detach_mid_hold_wire_pure() {
        // Grab released mid-hold: every held code gets exactly one key-up
        // (no stuck keys downstream) and the wire stays pure.
        use crate::init::{detached, Transport};
        let sinks = TestSinks::new();
        let haptic = Arc::new(FakeHaptic::new());
        let frames = vec![Ok(Some(frame(vec![finger(1, 200, 0, 0)], 0)))];
        let mut px = drive_ready_proxy(&sinks, &haptic, frames);
        let cfg = Config::default();
        px.handle_frame(&cfg).unwrap();
        assert!(sinks.pointer.has(EV_KEY, BTN_LEFT, 1));
        px.detach(&detached("/dev/hidraw1", Transport::Usb));
        let got = wire_events(&sinks.pointer);
        assert_wire_pure(&got, &[BTN_LEFT, BTN_TOUCH]);
        // Exactly one down/up pair: no duplicate releases, no stuck key.
        let left = key_values(&sinks.pointer, BTN_LEFT);
        assert_eq!(left, vec![1, 0], "{left:?}");
        assert_strict_alternation(&left, "BTN_LEFT");
        // Teardown hits BOTH nodes: extras gets the trailing SYN too, with
        // zero keys (teardown must never leak the sibling node).
        let xgot = wire_events(&sinks.extras);
        assert_eq!(
            xgot,
            vec![(EV_SYN, SYN_REPORT, 0); 2],
            "extras teardown drifted"
        );
    }

    #[test]
    fn force_disarm_mid_hold_wire_pure() {
        // `force_offset` back to 0 mid-hold: force keys go up once, the
        // first-level hold continues silently, full release ends at zero.
        let sinks = TestSinks::new();
        let haptic = Arc::new(FakeHaptic::new());
        let frames = vec![
            Ok(Some(frame(vec![finger(1, 70, 0, 0)], 0))),
            Ok(Some(frame(vec![finger(1, 100, 0, 0)], 0))),
            Ok(Some(frame(vec![finger(1, 100, 0, 0)], 0))),
            Ok(Some(frame(vec![finger(1, 10, 0, 0)], 0))),
        ];
        let mut px = drive_ready_proxy(&sinks, &haptic, frames);
        let armed = force_cfg();
        // Spaced: the full-release frame must clear the release debounce.
        let t0 = std::time::Instant::now();
        px.handle_frame_at(&armed, spaced_at(t0, 0)).unwrap();
        px.handle_frame_at(&armed, spaced_at(t0, 1)).unwrap();
        assert!(sinks.extras.has(EV_KEY, BTN_EXTRA, 1));
        let off = Config {
            force_offset: 0,
            ..armed
        };
        px.handle_frame_at(&off, spaced_at(t0, 2)).unwrap();
        px.handle_frame_at(&off, spaced_at(t0, 3)).unwrap();
        let got = wire_events(&sinks.pointer);
        assert_wire_pure(&got, &[BTN_LEFT, BTN_TOUCH]);
        // Forward: exactly one down/up pair on extras, released at disarm
        // (frame 3), never re-pressed. Left: down once, up once, at full
        // release on the pointer node.
        let extra = key_values(&sinks.extras, BTN_EXTRA);
        assert_eq!(extra, vec![1, 0], "{extra:?}");
        assert_strict_alternation(&extra, "BTN_EXTRA");
        let left = key_values(&sinks.pointer, BTN_LEFT);
        assert_eq!(left, vec![1, 0], "{left:?}");
        assert_strict_alternation(&left, "BTN_LEFT");
    }

    #[test]
    fn keys_md_pins_generated_table() {
        // `docs/KEYS.md` is generated by `tools/gen_keys_md.py` from the
        // live `key_token_to_code` map + `is_known_key`. This test fails on
        // any drift: every doc row must match the live map, and the live
        // mapped set (over config-source tokens + broad sweeps) must equal
        // the doc set, so additions on either side fail here first.
        const DOC: &str = include_str!("../docs/KEYS.md");
        let mut doc_rows: Vec<(&str, u16)> = Vec::new();
        for line in DOC.lines() {
            let line = line.trim();
            if !line.starts_with("| `") {
                continue;
            }
            let cells: Vec<&str> = line.split('|').map(str::trim).collect();
            assert!(cells.len() >= 6, "malformed KEYS.md row: {line}");
            let name = cells[1]
                .strip_prefix('`')
                .and_then(|s| s.strip_suffix('`'))
                .expect("name cell must be backticked");
            let code: u16 = cells[2].parse().expect("code cell must be decimal");
            doc_rows.push((name, code));
        }
        assert!(!doc_rows.is_empty(), "no rows parsed from docs/KEYS.md");
        // Every doc row matches the live map and parses as a click action.
        for (name, code) in &doc_rows {
            assert_eq!(
                key_token_to_code(name),
                Some((EV_KEY, *code)),
                "doc row '{name}' drifts from key_token_to_code"
            );
            assert!(
                crate::config::parse_click_action(name).is_ok(),
                "doc row '{name}' rejected by parse_click_action"
            );
        }
        let mut seen = std::collections::HashSet::new();
        for (name, _) in &doc_rows {
            assert!(seen.insert(*name), "duplicate doc row '{name}'");
        }
        // Candidate universe: every token named in `is_known_key` source
        // (parsed, not hand-listed) + all printable single chars + f0..=f99.
        // A newly mapped token on either side lands here and breaks the
        // set equality below until the doc is regenerated.
        const CONFIG_SRC: &str = include_str!("config.rs");
        let marker = "fn is_known_key";
        let start = CONFIG_SRC.find(marker).expect("is_known_key missing");
        let tail = &CONFIG_SRC[start..];
        let end = tail.find("\n}\n").expect("is_known_key end missing");
        let mut candidates: Vec<String> = Vec::new();
        for chunk in tail[..end].split('"').skip(1).step_by(2) {
            if !chunk.is_empty() {
                candidates.push(chunk.to_string());
            }
        }
        for b in 0x20u8..=0x7Eu8 {
            candidates.push((b as char).to_string());
        }
        for n in 0..=99 {
            candidates.push(format!("f{n}"));
        }
        let mut live = std::collections::HashSet::new();
        for cand in &candidates {
            if let Some((ty, code)) = key_token_to_code(cand) {
                assert_eq!(ty, EV_KEY, "non-EV_KEY mapping for '{cand}'");
                live.insert((cand.as_str(), code));
            }
        }
        let doc_set: std::collections::HashSet<(&str, u16)> = doc_rows.iter().copied().collect();
        assert_eq!(
            live, doc_set,
            "KEYS.md drifts from the live map (regenerate: tools/gen_keys_md.py)"
        );
        // Spot entries: mouse buttons vs browser AC codes, aliases, edges.
        for (name, code) in [
            ("back", BTN_SIDE),
            ("forward", BTN_EXTRA),
            ("browserback", 158),
            ("browserforward", 159),
            ("play", 164),
            ("playpause", 164),
            ("f11", 87),
            ("f12", 88),
            ("f13", 183),
            ("f24", 194),
            ("print", 210),
            ("favorites", 364),
            ("player", 387),
            ("micmute", 248),
            ("screenlock", 152),
        ] {
            assert!(
                doc_set.contains(&(name, code)),
                "spot entry {name}={code} missing from KEYS.md"
            );
        }
    }

    fn slide_cfg() -> Config {
        Config {
            slide_1: "left".into(),
            ..Config::default()
        }
    }

    #[test]
    fn slide_wire_down_hold_up_silent() {
        let sinks = TestSinks::new();
        let haptic = Arc::new(FakeHaptic::new());
        let frames = vec![
            Ok(Some(frame(vec![finger(1, 20, 0, 0)], 0))),
            Ok(Some(frame(vec![finger(1, 22, 5, 5)], 0))),
            Ok(Some(frame(vec![], 0))),
        ];
        let mut px = drive_ready_proxy(&sinks, &haptic, frames);
        let cfg = slide_cfg();
        let t0 = std::time::Instant::now();
        // Slide edge: left down, never a pulse (touch is silent).
        let ev = px.handle_frame_at(&cfg, spaced_at(t0, 0)).unwrap().unwrap();
        assert!(ev.is_slide());
        assert!(sinks.pointer.has(EV_KEY, BTN_LEFT, 1));
        assert!(haptic.calls.lock().unwrap().is_empty());
        // Steady slide: no re-fire, no duplicate down.
        assert!(px
            .handle_frame_at(&cfg, spaced_at(t0, 1))
            .unwrap()
            .is_none());
        assert_eq!(key_values(&sinks.pointer, BTN_LEFT), vec![1]);
        // Liftoff: left up, still no pulse.
        assert!(px
            .handle_frame_at(&cfg, spaced_at(t0, 2))
            .unwrap()
            .is_none());
        assert!(sinks.pointer.has(EV_KEY, BTN_LEFT, 0));
        assert!(haptic.releases.lock().unwrap().is_empty());
    }

    #[test]
    fn slide_release_holds_left_under_physical_button() {
        // T15 shape: slide holds left, fingers lift while the hardware
        // button stays down (`clicks & 1`). The slide release must NOT
        // emit left-up (no phantom 0-then-1 in one frame); the wire stays
        // down until the physical bit itself releases.
        let sinks = TestSinks::new();
        let haptic = Arc::new(FakeHaptic::new());
        let frames = vec![
            Ok(Some(frame(vec![finger(1, 20, 0, 0)], 0))),
            Ok(Some(frame(vec![], 1))),
            Ok(Some(frame(vec![], 0))),
        ];
        let mut px = drive_ready_proxy(&sinks, &haptic, frames);
        let cfg = slide_cfg();
        let t0 = std::time::Instant::now();
        assert!(px
            .handle_frame_at(&cfg, spaced_at(t0, 0))
            .unwrap()
            .unwrap()
            .is_slide());
        assert!(px
            .handle_frame_at(&cfg, spaced_at(t0, 1))
            .unwrap()
            .is_none());
        let left = key_values(&sinks.pointer, BTN_LEFT);
        assert_eq!(left, vec![1], "no left-up under physical hold: {left:?}");
        px.handle_frame_at(&cfg, spaced_at(t0, 2)).unwrap();
        assert_eq!(key_values(&sinks.pointer, BTN_LEFT), vec![1, 0]);
    }

    #[test]
    fn slide_default_none_is_wire_silent() {
        // Default slide_N = none: light frames emit motion slots only —
        // no keys, no haptic, current behavior exactly.
        let sinks = TestSinks::new();
        let haptic = Arc::new(FakeHaptic::new());
        let frames = vec![
            Ok(Some(frame(vec![finger(1, 20, 0, 0)], 0))),
            Ok(Some(frame(vec![], 0))),
        ];
        let mut px = drive_ready_proxy(&sinks, &haptic, frames);
        let cfg = Config::default();
        assert!(px.handle_frame(&cfg).unwrap().is_none());
        assert!(px.handle_frame(&cfg).unwrap().is_none());
        assert!(!sinks.pointer.has(EV_KEY, BTN_LEFT, 1));
        assert!(haptic.calls.lock().unwrap().is_empty());
        assert!(haptic.releases.lock().unwrap().is_empty());
    }

    #[test]
    fn slide_handoff_different_action_releases_first() {
        // slide_1 = left, click_1 = right: the press edge releases left
        // before pressing right, all inside one SYN frame.
        let sinks = TestSinks::new();
        let haptic = Arc::new(FakeHaptic::new());
        let frames = vec![
            Ok(Some(frame(vec![finger(1, 20, 0, 0)], 0))),
            Ok(Some(frame(vec![finger(1, 200, 0, 0)], 0))),
        ];
        let mut px = drive_ready_proxy(&sinks, &haptic, frames);
        let cfg = Config {
            slide_1: "left".into(),
            click_1: "right".into(),
            ..Config::default()
        };
        px.handle_frame(&cfg).unwrap();
        assert!(sinks.pointer.has(EV_KEY, BTN_LEFT, 1));
        let mark = sinks.pointer.events.lock().unwrap().len();
        let ev = px.handle_frame(&cfg).unwrap().unwrap();
        assert!(!ev.is_slide());
        let tail: Vec<(u16, u16, i32)> = sinks.pointer.events.lock().unwrap()[mark..].to_vec();
        let lefts: Vec<i32> = tail
            .iter()
            .copied()
            .filter(|&(ty, c, _)| ty == EV_KEY && c == BTN_LEFT)
            .map(|(_, _, v)| v)
            .collect();
        let rights: Vec<i32> = tail
            .iter()
            .copied()
            .filter(|&(ty, c, _)| ty == EV_KEY && c == BTN_RIGHT)
            .map(|(_, _, v)| v)
            .collect();
        assert_eq!(lefts, vec![0], "slide left must go up first: {tail:?}");
        assert_eq!(rights, vec![1], "click right presses after: {tail:?}");
        assert!(tail.contains(&(EV_SYN, SYN_REPORT, 0)));
    }

    #[test]
    fn slide_handoff_same_action_is_seamless() {
        // slide_1 = click_1 = left: no release/re-press — exactly one
        // down→up pair across slide, press, and liftoff.
        let sinks = TestSinks::new();
        let haptic = Arc::new(FakeHaptic::new());
        let frames = vec![
            Ok(Some(frame(vec![finger(1, 20, 0, 0)], 0))),
            Ok(Some(frame(vec![finger(1, 200, 0, 0)], 0))),
            Ok(Some(frame(vec![finger(1, 200, 1, 1)], 0))),
            Ok(Some(frame(vec![], 0))),
        ];
        let mut px = drive_ready_proxy(&sinks, &haptic, frames);
        let cfg = slide_cfg(); // click_1 defaults to left: same action
        let t0 = std::time::Instant::now();
        assert!(px
            .handle_frame_at(&cfg, spaced_at(t0, 0))
            .unwrap()
            .unwrap()
            .is_slide());
        assert!(!px
            .handle_frame_at(&cfg, spaced_at(t0, 1))
            .unwrap()
            .unwrap()
            .is_slide());
        px.handle_frame_at(&cfg, spaced_at(t0, 2)).unwrap();
        px.handle_frame_at(&cfg, spaced_at(t0, 3)).unwrap();
        let left = key_values(&sinks.pointer, BTN_LEFT);
        assert_eq!(
            left,
            vec![1, 0],
            "seamless handoff: single pair, got {left:?}"
        );
        assert_eq!(transitions(&left), 1);
        // One down-pulse / one up-pulse, both from the absorbed press (the
        // slide edge stays silent; the handoff press fires its own pulse).
        assert_eq!(haptic.calls.lock().unwrap().as_slice(), &[(64, 5, false)]);
        assert_eq!(haptic.releases.lock().unwrap().as_slice(), &[(64, false)]);
    }

    #[test]
    fn slide_reselect_identical_keyset_merges() {
        // slide_1 = slide_2 = left: a count change mid-slide keeps the
        // wire down (latch swaps to the new count, no release-then-press).
        let sinks = TestSinks::new();
        let haptic = Arc::new(FakeHaptic::new());
        let frames = vec![
            Ok(Some(frame(vec![finger(1, 20, 0, 0)], 0))),
            Ok(Some(frame(
                vec![finger(1, 20, 0, 0), finger(2, 20, 9, 9)],
                0,
            ))),
            Ok(Some(frame(vec![], 0))),
        ];
        let mut px = drive_ready_proxy(&sinks, &haptic, frames);
        let cfg = Config {
            slide_1: "left".into(),
            slide_2: "left".into(),
            ..Config::default()
        };
        let t0 = std::time::Instant::now();
        let first = px.handle_frame_at(&cfg, spaced_at(t0, 0)).unwrap().unwrap();
        assert!(first.is_slide() && first.finger_count == 1);
        let second = px.handle_frame_at(&cfg, spaced_at(t0, 1)).unwrap().unwrap();
        assert!(second.is_slide() && second.finger_count == 2);
        assert_eq!(
            key_values(&sinks.pointer, BTN_LEFT),
            vec![1],
            "identical re-select must not flutter"
        );
        assert!(haptic.calls.lock().unwrap().is_empty());
        px.handle_frame_at(&cfg, spaced_at(t0, 2)).unwrap();
        assert_eq!(key_values(&sinks.pointer, BTN_LEFT), vec![1, 0]);
        assert!(haptic.releases.lock().unwrap().is_empty());
    }

    #[test]
    fn slide_handoff_order_swapped_keyset_merges() {
        // slide_1 = left+ctrl, click_1 = ctrl+left: same set, different
        // order — seamless merge, single pair per code.
        let sinks = TestSinks::new();
        let haptic = Arc::new(FakeHaptic::new());
        let frames = vec![
            Ok(Some(frame(vec![finger(1, 20, 0, 0)], 0))),
            Ok(Some(frame(vec![finger(1, 200, 0, 0)], 0))),
            Ok(Some(frame(vec![], 0))),
        ];
        let mut px = drive_ready_proxy(&sinks, &haptic, frames);
        let cfg = Config {
            slide_1: "left+ctrl".into(),
            click_1: "ctrl+left".into(),
            ..Config::default()
        };
        let t0 = std::time::Instant::now();
        assert!(px
            .handle_frame_at(&cfg, spaced_at(t0, 0))
            .unwrap()
            .unwrap()
            .is_slide());
        assert!(!px
            .handle_frame_at(&cfg, spaced_at(t0, 1))
            .unwrap()
            .unwrap()
            .is_slide());
        px.handle_frame_at(&cfg, spaced_at(t0, 2)).unwrap();
        assert_eq!(key_values(&sinks.pointer, BTN_LEFT), vec![1, 0]);
        assert_eq!(key_values(&sinks.extras, 29), vec![1, 0]);
        assert_eq!(haptic.calls.lock().unwrap().len(), 1);
        assert_eq!(haptic.releases.lock().unwrap().len(), 1);
    }

    #[test]
    fn slide_handoff_absorbs_into_matching_press_level() {
        // Staged composition: slide == first merges seamlessly on the stab
        // frame (no pulse, no wire traffic — left never ups); force follows
        // on the next frame with its own pulse while the hold continues.
        // Left: exactly one down/up pair. Right presses alongside on frame 3.
        let sinks = TestSinks::new();
        let haptic = Arc::new(FakeHaptic::new());
        let frames = vec![
            Ok(Some(frame(vec![finger(1, 20, 0, 0)], 0))),
            Ok(Some(frame(vec![finger(1, 250, 0, 0)], 0))),
            Ok(Some(frame(vec![finger(1, 250, 0, 0)], 0))),
            Ok(Some(frame(vec![], 0))),
        ];
        let mut px = drive_ready_proxy(&sinks, &haptic, frames);
        let cfg = Config {
            slide_1: "left".into(),
            click_1: "left".into(),
            force_1: "right".into(),
            sensitivity: 64,
            margin: 5,
            force_offset: 20,
            ..Config::default()
        };
        let t0 = std::time::Instant::now();
        assert!(px
            .handle_frame_at(&cfg, spaced_at(t0, 0))
            .unwrap()
            .unwrap()
            .is_slide());
        let mark = sinks.pointer.events.lock().unwrap().len();
        let first = px.handle_frame_at(&cfg, spaced_at(t0, 1)).unwrap().unwrap();
        assert!(!first.is_force(), "staged: first edge lands alone");
        let tail: Vec<(u16, u16, i32)> = sinks.pointer.events.lock().unwrap()[mark..].to_vec();
        let lefts: Vec<i32> = tail
            .iter()
            .copied()
            .filter(|&(ty, c, _)| ty == EV_KEY && c == BTN_LEFT)
            .map(|(_, _, v)| v)
            .collect();
        assert!(
            lefts.iter().all(|&v| v == 1),
            "no left up mid-hold: {tail:?}"
        );
        assert!(sinks.pointer.has(EV_KEY, BTN_LEFT, 1));
        let ev = px.handle_frame_at(&cfg, spaced_at(t0, 2)).unwrap().unwrap();
        assert!(ev.is_force(), "force supersedes in reporting");
        assert!(sinks.pointer.has(EV_KEY, BTN_RIGHT, 1));
        // Two downs (first + force; the slide edge stays silent) / two ups.
        assert_eq!(haptic.calls.lock().unwrap().len(), 2);
        px.handle_frame_at(&cfg, spaced_at(t0, 3)).unwrap();
        assert_eq!(haptic.releases.lock().unwrap().len(), 2);
        assert!(sinks.pointer.has(EV_KEY, BTN_LEFT, 0));
        assert!(sinks.pointer.has(EV_KEY, BTN_RIGHT, 0));
    }

    #[test]
    fn transition_cut_skips_empty_force() {
        // force_1 = empty in cut mode: haptic-only pulse must not release
        // the held level-1 keys (nothing to restart into) and must not
        // re-press later — no flutter.
        let sinks = TestSinks::new();
        let haptic = Arc::new(FakeHaptic::new());
        let frames = vec![
            Ok(Some(frame(vec![finger(1, 70, 0, 0)], 0))),
            Ok(Some(frame(vec![finger(1, 100, 0, 0)], 0))),
            Ok(Some(frame(vec![finger(1, 70, 0, 0)], 0))),
            Ok(Some(frame(vec![], 0))),
        ];
        let mut px = drive_ready_proxy(&sinks, &haptic, frames);
        let cfg = Config {
            click_1: "left".into(),
            force_1: "empty".into(),
            sensitivity: 64,
            margin: 5,
            force_offset: 20,
            transition_mode: "cut".into(),
            ..Config::default()
        };
        let t0 = std::time::Instant::now();
        px.handle_frame_at(&cfg, spaced_at(t0, 0)).unwrap();
        let ev = px.handle_frame_at(&cfg, spaced_at(t0, 1)).unwrap().unwrap();
        assert!(ev.is_force() && ev.is_empty());
        // Left never ups at the force edge; the empty pulse still fires.
        assert_eq!(key_values(&sinks.pointer, BTN_LEFT), vec![1]);
        assert_eq!(
            haptic.calls.lock().unwrap().as_slice(),
            &[(64, 5, false), (64, 5, false)]
        );
        px.handle_frame_at(&cfg, spaced_at(t0, 2)).unwrap();
        assert_eq!(key_values(&sinks.pointer, BTN_LEFT), vec![1]);
        px.handle_frame_at(&cfg, spaced_at(t0, 3)).unwrap();
        assert_eq!(key_values(&sinks.pointer, BTN_LEFT), vec![1, 0]);
    }

    #[test]
    fn transition_continuous_holds_first_under_force() {
        // Default mode: force joins, left never ups mid-hold.
        let sinks = TestSinks::new();
        let haptic = Arc::new(FakeHaptic::new());
        let frames = vec![
            Ok(Some(frame(vec![finger(1, 70, 0, 0)], 0))),
            Ok(Some(frame(vec![finger(1, 100, 0, 0)], 0))),
            Ok(Some(frame(vec![finger(1, 70, 0, 0)], 0))),
            Ok(Some(frame(vec![], 0))),
        ];
        let mut px = drive_ready_proxy(&sinks, &haptic, frames);
        let cfg = Config {
            click_1: "left".into(),
            force_1: "left+super".into(),
            sensitivity: 64,
            margin: 5,
            force_offset: 20,
            ..Config::default()
        };
        assert!(!cfg.is_cut_transition());
        let t0 = std::time::Instant::now();
        px.handle_frame_at(&cfg, spaced_at(t0, 0)).unwrap();
        px.handle_frame_at(&cfg, spaced_at(t0, 1)).unwrap();
        assert!(sinks.pointer.has(EV_KEY, BTN_LEFT, 1));
        assert!(sinks.extras.has(EV_KEY, 125, 1)); // super joins
                                                   // Force release: shared left stays down (one pair overall so far).
        px.handle_frame_at(&cfg, spaced_at(t0, 2)).unwrap();
        assert_eq!(key_values(&sinks.pointer, BTN_LEFT), vec![1]);
        px.handle_frame_at(&cfg, spaced_at(t0, 3)).unwrap();
        assert_eq!(key_values(&sinks.pointer, BTN_LEFT), vec![1, 0]);
    }

    #[test]
    fn transition_cut_restarts_force_combo() {
        // Cut mode, staged frames: the force edge (frame N) releases left
        // (up+SYN); the modifiers land once the 5 ms floor elapses; the
        // buttons follow `combo_stagger_ms` after the modifiers. The
        // staging is the point: same-frame up+down merges into the running
        // drag downstream.
        let sinks = TestSinks::new();
        let haptic = Arc::new(FakeHaptic::new());
        let frames = vec![
            Ok(Some(frame(vec![finger(1, 70, 0, 0)], 0))),
            Ok(Some(frame(vec![finger(1, 100, 0, 0)], 0))),
            Ok(Some(frame(vec![finger(1, 100, 5, 5)], 0))),
            Ok(Some(frame(vec![finger(1, 100, 9, 9)], 0))),
        ];
        let mut px = drive_ready_proxy(&sinks, &haptic, frames);
        let cfg = Config {
            click_1: "left".into(),
            force_1: "left+super".into(),
            sensitivity: 64,
            margin: 5,
            force_offset: 20,
            transition_mode: "cut".into(),
            ..Config::default()
        };
        let t0 = std::time::Instant::now();
        px.handle_frame_at(&cfg, spaced_at(t0, 0)).unwrap();
        assert!(sinks.pointer.has(EV_KEY, BTN_LEFT, 1));
        // Edge frame: release only, press deferred.
        let mark = sinks.pointer.events.lock().unwrap().len();
        let emark = sinks.extras.events.lock().unwrap().len();
        let ev = px.handle_frame_at(&cfg, spaced_at(t0, 1)).unwrap().unwrap();
        assert!(ev.is_force());
        let tail: Vec<(u16, u16, i32)> = sinks.pointer.events.lock().unwrap()[mark..].to_vec();
        let lefts: Vec<i32> = tail
            .iter()
            .copied()
            .filter(|&(ty, c, _)| ty == EV_KEY && c == BTN_LEFT)
            .map(|(_, _, v)| v)
            .collect();
        assert_eq!(lefts, vec![0], "edge frame releases only: {tail:?}");
        assert!(
            tail.contains(&(EV_SYN, SYN_REPORT, 0)),
            "release carries SYN: {tail:?}"
        );
        let etail: Vec<(u16, u16, i32)> = sinks.extras.events.lock().unwrap()[emark..].to_vec();
        assert!(
            !etail.contains(&(EV_KEY, 125, 1)),
            "press deferred past the edge frame: {etail:?}"
        );
        // Next steady frame (60 ms later, floor elapsed): modifiers land,
        // buttons still wait out the stagger.
        assert!(px
            .handle_frame_at(&cfg, spaced_at(t0, 2))
            .unwrap()
            .is_none());
        assert_eq!(
            key_values(&sinks.pointer, BTN_LEFT),
            vec![1, 0],
            "buttons wait for the stagger"
        );
        assert!(sinks.extras.has(EV_KEY, 125, 1));
        assert!(px.pending_force.is_some(), "buttons still pending");
        // Following frame (stagger elapsed): buttons land.
        let mark3 = sinks.pointer.events.lock().unwrap().len();
        assert!(px
            .handle_frame_at(&cfg, spaced_at(t0, 3))
            .unwrap()
            .is_none());
        let tail3: Vec<(u16, u16, i32)> = sinks.pointer.events.lock().unwrap()[mark3..].to_vec();
        let lefts3: Vec<i32> = tail3
            .iter()
            .copied()
            .filter(|&(ty, c, _)| ty == EV_KEY && c == BTN_LEFT)
            .map(|(_, _, v)| v)
            .collect();
        assert_eq!(lefts3, vec![1], "buttons land after stagger: {tail3:?}");
        assert!(px.pending_force.is_none(), "press flushed");
        // Overall: exactly down/up/down, strict.
        let all = key_values(&sinks.pointer, BTN_LEFT);
        assert_eq!(all, vec![1, 0, 1], "staged cut: {all:?}");
        assert_strict_alternation(&all, "BTN_LEFT");
    }

    #[test]
    fn transition_cut_represses_first_on_force_release() {
        // Easing back below the force release before the deferred press
        // lands drops it (its keys never went down); the still-latched
        // level-1 keys re-press and the full release owns the final key-ups.
        let sinks = TestSinks::new();
        let haptic = Arc::new(FakeHaptic::new());
        let frames = vec![
            Ok(Some(frame(vec![finger(1, 70, 0, 0)], 0))),
            Ok(Some(frame(vec![finger(1, 100, 0, 0)], 0))),
            Ok(Some(frame(vec![finger(1, 70, 0, 0)], 0))),
            Ok(Some(frame(vec![], 0))),
        ];
        let mut px = drive_ready_proxy(&sinks, &haptic, frames);
        let cfg = Config {
            click_1: "left".into(),
            force_1: "forward".into(),
            sensitivity: 64,
            margin: 5,
            force_offset: 20,
            transition_mode: "cut".into(),
            ..Config::default()
        };
        let t0 = std::time::Instant::now();
        px.handle_frame_at(&cfg, spaced_at(t0, 0)).unwrap();
        px.handle_frame_at(&cfg, spaced_at(t0, 1)).unwrap();
        // Cut dropped left at the force edge; force release re-presses it.
        // The dropped force combo never touches the wire at all.
        px.handle_frame_at(&cfg, spaced_at(t0, 2)).unwrap();
        let left = key_values(&sinks.pointer, BTN_LEFT);
        assert_eq!(left, vec![1, 0, 1], "cut, re-press on return: {left:?}");
        assert!(
            key_values(&sinks.extras, BTN_EXTRA).is_empty(),
            "dropped press leaves no forward traffic: {:?}",
            sinks.extras.events.lock().unwrap()
        );
        px.handle_frame_at(&cfg, spaced_at(t0, 3)).unwrap();
        assert_eq!(key_values(&sinks.pointer, BTN_LEFT), vec![1, 0, 1, 0]);
    }

    #[test]
    fn transition_cut_liftoff_before_press_drops_cleanly() {
        // Liftoff before the deferred press lands: the pending press drops,
        // the wire ends at exactly one down/up pair, no force keys ever
        // appear, and the force haptic never fires (nothing to release).
        let sinks = TestSinks::new();
        let haptic = Arc::new(FakeHaptic::new());
        let frames = vec![
            Ok(Some(frame(vec![finger(1, 70, 0, 0)], 0))),
            Ok(Some(frame(vec![finger(1, 100, 0, 0)], 0))),
            Ok(Some(frame(vec![], 0))),
        ];
        let mut px = drive_ready_proxy(&sinks, &haptic, frames);
        let cfg = Config {
            click_1: "left".into(),
            force_1: "left+super".into(),
            sensitivity: 64,
            margin: 5,
            force_offset: 20,
            transition_mode: "cut".into(),
            ..Config::default()
        };
        let t0 = std::time::Instant::now();
        px.handle_frame_at(&cfg, spaced_at(t0, 0)).unwrap();
        let ev = px.handle_frame_at(&cfg, spaced_at(t0, 1)).unwrap().unwrap();
        assert!(ev.is_force());
        assert_eq!(key_values(&sinks.pointer, BTN_LEFT), vec![1, 0]);
        px.handle_frame_at(&cfg, spaced_at(t0, 2)).unwrap();
        let left = key_values(&sinks.pointer, BTN_LEFT);
        assert_eq!(left, vec![1, 0], "clean drop, no dangling: {left:?}");
        assert_strict_alternation(&left, "BTN_LEFT");
        assert!(
            sinks
                .extras
                .events
                .lock()
                .unwrap()
                .iter()
                .all(|&(t, _, _)| t != EV_KEY),
            "force combo never hits the wire: {:?}",
            sinks.extras.events.lock().unwrap()
        );
        assert_eq!(haptic.calls.lock().unwrap().len(), 1, "first press only");
        assert_eq!(
            haptic.releases.lock().unwrap().len(),
            1,
            "first release only"
        );
    }

    #[test]
    fn transition_cut_noisy_dip_holds_super_clean_liftoff_releases() {
        // Owner NIRI (tune 130/30/5: press 130, force 160, first-release
        // 125, force-release 155): a hold dipping to 150 (just under force
        // release, still firmly pressed) must keep super down — otherwise
        // niri sees super-tap (overview on release) plus a separate click,
        // never the chord, and no drag starts. Only a clean liftoff
        // releases. `combo_stagger_ms = 0` keeps the staging compact here;
        // the stagger itself is pinned elsewhere.
        let sinks = TestSinks::new();
        let haptic = Arc::new(FakeHaptic::new());
        let frames = vec![
            Ok(Some(frame(vec![finger(1, 140, 0, 0)], 0))),
            Ok(Some(frame(vec![finger(1, 175, 0, 0)], 0))),
            Ok(Some(frame(vec![finger(1, 175, 5, 5)], 0))),
            Ok(Some(frame(vec![finger(1, 150, 5, 5)], 0))),
            Ok(Some(frame(vec![finger(1, 175, 9, 9)], 0))),
            Ok(Some(frame(vec![], 0))),
        ];
        let mut px = drive_ready_proxy(&sinks, &haptic, frames);
        let cfg = Config {
            click_1: "left".into(),
            force_1: "left+super".into(),
            sensitivity: 130,
            margin: 5,
            force_offset: 30,
            transition_mode: "cut".into(),
            combo_stagger_ms: 0,
            ..Config::default()
        };
        let t0 = std::time::Instant::now();
        px.handle_frame_at(&cfg, spaced_at(t0, 0)).unwrap();
        let t1 = spaced_at(t0, 1);
        px.handle_frame_at(&cfg, t1).unwrap().unwrap();
        let t2 = t1 + std::time::Duration::from_millis(60);
        px.handle_frame_at(&cfg, t2).unwrap();
        assert_eq!(key_values(&sinks.pointer, BTN_LEFT), vec![1, 0, 1]);
        assert_eq!(key_values(&sinks.extras, 125), vec![1]);
        // Dip to 150 for 10 ms: hold survives, wire silent.
        let t3 = t2 + std::time::Duration::from_millis(10);
        assert!(
            px.handle_frame_at(&cfg, t3).unwrap().is_none(),
            "dip must not edge"
        );
        assert_eq!(
            key_values(&sinks.extras, 125),
            vec![1],
            "dip just under release keeps super down"
        );
        assert_eq!(
            key_values(&sinks.pointer, BTN_LEFT),
            vec![1, 0, 1],
            "dip adds no button traffic"
        );
        // Back deep, then a clean liftoff releases everything once.
        let t4 = t3 + std::time::Duration::from_millis(10);
        px.handle_frame_at(&cfg, t4).unwrap();
        assert_eq!(key_values(&sinks.extras, 125), vec![1]);
        px.handle_frame_at(&cfg, t4 + std::time::Duration::from_millis(60))
            .unwrap();
        assert_eq!(key_values(&sinks.extras, 125), vec![1, 0]);
        assert_eq!(
            key_values(&sinks.pointer, BTN_LEFT),
            vec![1, 0, 1, 0],
            "liftoff owns the final ups"
        );
        assert_strict_alternation(&key_values(&sinks.pointer, BTN_LEFT), "BTN_LEFT");
        assert_strict_alternation(&key_values(&sinks.extras, 125), "super");
    }

    #[test]
    fn transition_cut_press_waits_for_gap_floor() {
        // The 5 ms floor is clock-gated, not frame-counted: a next frame 2 ms
        // after the release holds the whole press; the modifiers land once
        // 5 ms have elapsed, the buttons `combo_stagger_ms` after that.
        // Controlled timestamps pin both gaps.
        let sinks = TestSinks::new();
        let haptic = Arc::new(FakeHaptic::new());
        let frames = vec![
            Ok(Some(frame(vec![finger(1, 70, 0, 0)], 0))),
            Ok(Some(frame(vec![finger(1, 100, 0, 0)], 0))),
            Ok(Some(frame(vec![finger(1, 100, 5, 5)], 0))),
            Ok(Some(frame(vec![finger(1, 100, 9, 9)], 0))),
            Ok(Some(frame(vec![finger(1, 100, 13, 13)], 0))),
        ];
        let mut px = drive_ready_proxy(&sinks, &haptic, frames);
        let cfg = Config {
            click_1: "left".into(),
            force_1: "left+super".into(),
            sensitivity: 64,
            margin: 5,
            force_offset: 20,
            transition_mode: "cut".into(),
            ..Config::default()
        };
        let t0 = std::time::Instant::now();
        px.handle_frame_at(&cfg, spaced_at(t0, 0)).unwrap();
        let t1 = spaced_at(t0, 1);
        let ev = px.handle_frame_at(&cfg, t1).unwrap().unwrap();
        assert!(ev.is_force());
        assert_eq!(key_values(&sinks.pointer, BTN_LEFT), vec![1, 0]);
        // +2 ms: floor not elapsed — press held, wire silent.
        px.handle_frame_at(&cfg, t1 + std::time::Duration::from_millis(2))
            .unwrap();
        assert_eq!(
            key_values(&sinks.pointer, BTN_LEFT),
            vec![1, 0],
            "gap floor holds the press"
        );
        assert!(
            !sinks.extras.has(EV_KEY, 125, 1),
            "super waits for the gap too"
        );
        assert!(
            px.pending_force.is_some(),
            "pending survives the fast frame"
        );
        // +5 ms: floor elapsed — modifiers land, buttons wait the stagger.
        let tmods = t1 + std::time::Duration::from_millis(5);
        px.handle_frame_at(&cfg, tmods).unwrap();
        assert_eq!(
            key_values(&sinks.pointer, BTN_LEFT),
            vec![1, 0],
            "buttons wait out the stagger"
        );
        assert!(sinks.extras.has(EV_KEY, 125, 1));
        assert!(px.pending_force.is_some(), "buttons still pending");
        // +55 ms (50 ms stagger after the modifiers): buttons land.
        px.handle_frame_at(&cfg, tmods + std::time::Duration::from_millis(50))
            .unwrap();
        assert_eq!(key_values(&sinks.pointer, BTN_LEFT), vec![1, 0, 1]);
        assert!(px.pending_force.is_none(), "press flushed");
    }

    #[test]
    fn transition_cut_zero_stagger_presses_both_stages_at_floor() {
        // `combo_stagger_ms = 0`: modifiers and buttons land in one frame
        // once the 5 ms floor elapses (still ≥5 ms after the release).
        let sinks = TestSinks::new();
        let haptic = Arc::new(FakeHaptic::new());
        let frames = vec![
            Ok(Some(frame(vec![finger(1, 70, 0, 0)], 0))),
            Ok(Some(frame(vec![finger(1, 100, 0, 0)], 0))),
            Ok(Some(frame(vec![finger(1, 100, 5, 5)], 0))),
        ];
        let mut px = drive_ready_proxy(&sinks, &haptic, frames);
        let cfg = Config {
            click_1: "left".into(),
            force_1: "left+super".into(),
            sensitivity: 64,
            margin: 5,
            force_offset: 20,
            transition_mode: "cut".into(),
            combo_stagger_ms: 0,
            ..Config::default()
        };
        let t0 = std::time::Instant::now();
        px.handle_frame_at(&cfg, spaced_at(t0, 0)).unwrap();
        let t1 = spaced_at(t0, 1);
        px.handle_frame_at(&cfg, t1).unwrap().unwrap();
        px.handle_frame_at(&cfg, t1 + std::time::Duration::from_millis(5))
            .unwrap();
        assert_eq!(key_values(&sinks.pointer, BTN_LEFT), vec![1, 0, 1]);
        assert!(sinks.extras.has(EV_KEY, 125, 1));
        assert!(px.pending_force.is_none(), "press flushed");
    }

    #[test]
    fn transition_cut_custom_stagger_delays_buttons() {
        // `combo_stagger_ms = 120`: modifiers at the floor, buttons 120 ms
        // after the modifiers — not a millisecond sooner.
        let sinks = TestSinks::new();
        let haptic = Arc::new(FakeHaptic::new());
        let frames = vec![
            Ok(Some(frame(vec![finger(1, 70, 0, 0)], 0))),
            Ok(Some(frame(vec![finger(1, 100, 0, 0)], 0))),
            Ok(Some(frame(vec![finger(1, 100, 5, 5)], 0))),
            Ok(Some(frame(vec![finger(1, 100, 9, 9)], 0))),
            Ok(Some(frame(vec![finger(1, 100, 13, 13)], 0))),
        ];
        let mut px = drive_ready_proxy(&sinks, &haptic, frames);
        let cfg = Config {
            click_1: "left".into(),
            force_1: "left+super".into(),
            sensitivity: 64,
            margin: 5,
            force_offset: 20,
            transition_mode: "cut".into(),
            combo_stagger_ms: 120,
            ..Config::default()
        };
        let t0 = std::time::Instant::now();
        px.handle_frame_at(&cfg, spaced_at(t0, 0)).unwrap();
        let t1 = spaced_at(t0, 1);
        px.handle_frame_at(&cfg, t1).unwrap().unwrap();
        let tmods = t1 + std::time::Duration::from_millis(5);
        px.handle_frame_at(&cfg, tmods).unwrap();
        assert!(sinks.extras.has(EV_KEY, 125, 1));
        assert_eq!(key_values(&sinks.pointer, BTN_LEFT), vec![1, 0]);
        // +119 ms after the modifiers: buttons still held.
        px.handle_frame_at(&cfg, tmods + std::time::Duration::from_millis(119))
            .unwrap();
        assert_eq!(
            key_values(&sinks.pointer, BTN_LEFT),
            vec![1, 0],
            "buttons wait the full custom stagger"
        );
        assert!(px.pending_force.is_some());
        // +120 ms: buttons land.
        px.handle_frame_at(&cfg, tmods + std::time::Duration::from_millis(120))
            .unwrap();
        assert_eq!(key_values(&sinks.pointer, BTN_LEFT), vec![1, 0, 1]);
        assert!(px.pending_force.is_none(), "press flushed");
    }

    #[test]
    fn transition_cut_full_cycle_pairs_super_and_hides_taps_under_hold() {
        // Niri overview audit: super must pair exactly (one down/up) across
        // a full cut cycle under physical hold, and taps made while the
        // hardware button stays held must emit ZERO button traffic — the
        // wire already reads held-down, so there is no fresh click for a
        // compositor to dismiss on (same as a held physical mouse button).
        let sinks = TestSinks::new();
        let haptic = Arc::new(FakeHaptic::new());
        let frames = vec![
            Ok(Some(frame(vec![finger(1, 70, 0, 0)], 1))),
            Ok(Some(frame(vec![finger(1, 100, 0, 0)], 1))),
            Ok(Some(frame(vec![finger(1, 100, 5, 5)], 1))),
            Ok(Some(frame(vec![finger(1, 100, 9, 9)], 1))),
            Ok(Some(frame(vec![finger(1, 70, 0, 0)], 1))),
            Ok(Some(frame(vec![], 1))),
            Ok(Some(frame(vec![], 0))),
            Ok(Some(frame(vec![], 1))),
            Ok(Some(frame(vec![finger(1, 200, 0, 0)], 1))),
            Ok(Some(frame(vec![], 1))),
        ];
        let mut px = drive_ready_proxy(&sinks, &haptic, frames);
        let cfg = Config {
            click_1: "left".into(),
            force_1: "left+super".into(),
            sensitivity: 64,
            margin: 5,
            force_offset: 20,
            transition_mode: "cut".into(),
            ..Config::default()
        };
        let t0 = std::time::Instant::now();
        for i in 0..7 {
            px.handle_frame_at(&cfg, spaced_at(t0, i)).unwrap();
        }
        // Full cycle: press / cut-break / deferred press / ease-repress /
        // liftoff-under-hold / physical release. Super pairs exactly once.
        assert_eq!(
            key_values(&sinks.pointer, BTN_LEFT),
            vec![1, 0, 1, 0, 1, 0],
            "full cycle under hold: {:?}",
            sinks.pointer.events.lock().unwrap()
        );
        assert_eq!(
            key_values(&sinks.extras, 125),
            vec![1, 0],
            "super pairs exactly once: {:?}",
            sinks.extras.events.lock().unwrap()
        );
        // Taps under a held physical button: physical re-press owns its own
        // down, but the pad tap itself adds no button traffic — no fresh
        // click exists on the wire to dismiss on.
        let mark = sinks.pointer.events.lock().unwrap().len();
        px.handle_frame_at(&cfg, spaced_at(t0, 7)).unwrap();
        px.handle_frame_at(&cfg, spaced_at(t0, 8)).unwrap();
        px.handle_frame_at(&cfg, spaced_at(t0, 9)).unwrap();
        let tail: Vec<(u16, u16, i32)> = sinks.pointer.events.lock().unwrap()[mark..].to_vec();
        let lefts: Vec<i32> = tail
            .iter()
            .copied()
            .filter(|&(ty, c, _)| ty == EV_KEY && c == BTN_LEFT)
            .map(|(_, _, v)| v)
            .collect();
        assert_eq!(
            lefts,
            vec![1],
            "only the physical re-press itself; tap adds nothing: {tail:?}"
        );
    }

    #[test]
    fn extras_key_down_up_each_syn_closed_like_pointer() {
        // Owner NIRI: a super-down that lands WITHOUT its SYN is held by
        // libinput and flushed with its up as a tap (overview) instead of a
        // hold (drag modifier). Every extras key down/up must be SYN-closed
        // in its own frame batch, exactly the pointer discipline (one shared
        // fan-out sync — pinned here so any emit-without-sync regresses).
        // Drive press / cut-edge / mods / buttons / ease / liftoff.
        let sinks = TestSinks::new();
        let haptic = Arc::new(FakeHaptic::new());
        let frames = vec![
            Ok(Some(frame(vec![finger(1, 70, 0, 0)], 0))),
            Ok(Some(frame(vec![finger(1, 100, 0, 0)], 0))),
            Ok(Some(frame(vec![finger(1, 100, 5, 5)], 0))),
            Ok(Some(frame(vec![finger(1, 100, 9, 9)], 0))),
            Ok(Some(frame(vec![finger(1, 70, 0, 0)], 0))),
            Ok(Some(frame(vec![], 0))),
        ];
        let mut px = drive_ready_proxy(&sinks, &haptic, frames);
        let cfg = Config {
            click_1: "left".into(),
            force_1: "left+super".into(),
            sensitivity: 64,
            margin: 5,
            force_offset: 20,
            transition_mode: "cut".into(),
            ..Config::default()
        };
        let t0 = std::time::Instant::now();
        for i in 0..6 {
            px.handle_frame_at(&cfg, spaced_at(t0, i)).unwrap();
        }
        // Extras: super down alone in the flush frame, up alone in the ease
        // frame — each immediately SYN-closed (a hold, never a batched tap).
        let x: Vec<(u16, u16, i32)> = sinks.extras.events.lock().unwrap().clone();
        let down = x
            .iter()
            .position(|&e| e == (EV_KEY, 125, 1))
            .expect("super down lands");
        assert_eq!(
            x[down + 1],
            (EV_SYN, SYN_REPORT, 0),
            "super-down SYN-closed: {x:?}"
        );
        let up = x
            .iter()
            .position(|&e| e == (EV_KEY, 125, 0))
            .expect("super up lands");
        assert!(up > down, "up after down (a hold span, not a batch): {x:?}");
        assert_eq!(
            x[up + 1],
            (EV_SYN, SYN_REPORT, 0),
            "super-up SYN-closed: {x:?}"
        );
        // Pointer discipline, same bar: the flush-frame left re-press is
        // SYN-closed, and the whole log ends batch-closed.
        let p: Vec<(u16, u16, i32)> = sinks.pointer.events.lock().unwrap().clone();
        assert_eq!(
            key_values(&sinks.pointer, BTN_LEFT),
            vec![1, 0, 1, 0, 1, 0],
            "left cycle: {p:?}"
        );
        assert_eq!(
            p.last(),
            Some(&(EV_SYN, SYN_REPORT, 0)),
            "pointer log batch-closed: {p:?}"
        );
        assert_eq!(
            x.last(),
            Some(&(EV_SYN, SYN_REPORT, 0)),
            "extras log batch-closed: {x:?}"
        );
    }

    #[test]
    fn cut_force_press_staggers_super_then_left_on_one_wire() {
        // Owner HW: Mutter needs super held (framed) BEFORE left lands —
        // same-frame simultaneous downs read as one gesture. A single sink
        // pins GLOBAL order: super down + SYN strictly before the left
        // re-press. (Audit: the deferred path replays the full force key
        // set through the same `press_level`/`DualSink` routing — per-node
        // tests already show extras 125 alongside the pointer re-press, and
        // `is_down` has no stale holder here. This test pins the order.)
        let wire = Arc::new(FakeUinput::new());
        let haptic = Arc::new(FakeHaptic::new());
        let frames = vec![
            Ok(Some(frame(vec![finger(1, 70, 0, 0)], 0))),
            Ok(Some(frame(vec![finger(1, 100, 0, 0)], 0))),
            Ok(Some(frame(vec![finger(1, 100, 5, 5)], 0))),
            Ok(Some(frame(vec![finger(1, 100, 9, 9)], 0))),
        ];
        let mut px = ClickProxy::new(
            settings(true),
            SharedGrab(Arc::new(FakeGrab::ok())),
            SharedUinput(Arc::clone(&wire)),
            SharedHaptic(Arc::clone(&haptic)),
            ScriptSource::new(frames),
        );
        px.set_pump_ready(true);
        px.attach(&usb_ready("/dev/hidraw1"));
        let cfg = Config {
            click_1: "left".into(),
            force_1: "super+left".into(),
            sensitivity: 64,
            margin: 5,
            force_offset: 20,
            transition_mode: "cut".into(),
            ..Config::default()
        };
        let t0 = std::time::Instant::now();
        px.handle_frame_at(&cfg, spaced_at(t0, 0)).unwrap();
        px.handle_frame_at(&cfg, spaced_at(t0, 1)).unwrap().unwrap();
        // Modifiers stage (floor elapsed), buttons still pending.
        px.handle_frame_at(&cfg, spaced_at(t0, 2)).unwrap();
        // Buttons stage (stagger elapsed).
        px.handle_frame_at(&cfg, spaced_at(t0, 3)).unwrap();
        let evs = wire.events.lock().unwrap().clone();
        let super_down = evs
            .iter()
            .position(|&e| e == (EV_KEY, 125, 1))
            .expect("super must go down");
        let left_redown = evs
            .iter()
            .rposition(|&e| e == (EV_KEY, BTN_LEFT, 1))
            .expect("left must re-press");
        assert!(super_down < left_redown, "super before left: {evs:?}");
        assert!(
            evs[super_down..=left_redown].contains(&(EV_SYN, SYN_REPORT, 0)),
            "a SYN must frame super before left lands: {evs:?}"
        );
    }

    #[test]
    fn transition_cut_breaks_wire_under_physical_hold() {
        // Owner hardware: transition_mode=cut with force_1=super+left must
        // break the wire even while the user physically holds the click
        // (`clicks & 1`). The cut bypasses the physical carve-out; release
        // lands on the edge frame, modifiers on a later frame (5 ms floor),
        // buttons `combo_stagger_ms` after that — with no same-frame
        // re-flutter.
        let sinks = TestSinks::new();
        let haptic = Arc::new(FakeHaptic::new());
        let frames = vec![
            Ok(Some(frame(vec![finger(1, 70, 0, 0)], 1))),
            Ok(Some(frame(vec![finger(1, 100, 0, 0)], 1))),
            Ok(Some(frame(vec![finger(1, 100, 5, 5)], 1))),
            Ok(Some(frame(vec![finger(1, 100, 9, 9)], 1))),
        ];
        let mut px = drive_ready_proxy(&sinks, &haptic, frames);
        let cfg = Config {
            click_1: "left".into(),
            force_1: "left+super".into(),
            sensitivity: 64,
            margin: 5,
            force_offset: 20,
            transition_mode: "cut".into(),
            ..Config::default()
        };
        let t0 = std::time::Instant::now();
        px.handle_frame_at(&cfg, spaced_at(t0, 0)).unwrap();
        assert_eq!(key_values(&sinks.pointer, BTN_LEFT), vec![1]);
        // Edge frame: release only, even under hold.
        let mark = sinks.pointer.events.lock().unwrap().len();
        let ev = px.handle_frame_at(&cfg, spaced_at(t0, 1)).unwrap().unwrap();
        assert!(ev.is_force());
        let tail: Vec<(u16, u16, i32)> = sinks.pointer.events.lock().unwrap()[mark..].to_vec();
        let lefts: Vec<i32> = tail
            .iter()
            .copied()
            .filter(|&(ty, c, _)| ty == EV_KEY && c == BTN_LEFT)
            .map(|(_, _, v)| v)
            .collect();
        assert_eq!(lefts, vec![0], "cut must break under hold: {tail:?}");
        assert!(
            tail.contains(&(EV_SYN, SYN_REPORT, 0)),
            "release carries SYN: {tail:?}"
        );
        // Modifiers land next (floor elapsed), buttons wait the stagger.
        px.handle_frame_at(&cfg, spaced_at(t0, 2)).unwrap();
        assert_eq!(
            key_values(&sinks.pointer, BTN_LEFT),
            vec![1, 0],
            "buttons wait for the stagger"
        );
        assert!(sinks.extras.has(EV_KEY, 125, 1));
        // Stagger elapsed: buttons land.
        assert!(px
            .handle_frame_at(&cfg, spaced_at(t0, 3))
            .unwrap()
            .is_none());
        // Next steady frame (gap elapsed): fresh press lands.
        assert!(px
            .handle_frame_at(&cfg, spaced_at(t0, 2))
            .unwrap()
            .is_none());
        // No same-frame re-flutter: exactly down/up/down, strict.
        let all = key_values(&sinks.pointer, BTN_LEFT);
        assert_eq!(all, vec![1, 0, 1], "no extra flutter: {all:?}");
        assert_strict_alternation(&all, "BTN_LEFT");
        assert_eq!(transitions(&all), 2);
        // Super joins on the extras node.
        assert!(sinks.extras.has(EV_KEY, 125, 1));
    }

    #[test]
    fn transition_continuous_holds_wire_under_physical_hold() {
        // Continuous mode unchanged: the physical carve-out stays, so the
        // force edge never breaks the held left under `clicks & 1`.
        let sinks = TestSinks::new();
        let haptic = Arc::new(FakeHaptic::new());
        let frames = vec![
            Ok(Some(frame(vec![finger(1, 70, 0, 0)], 1))),
            Ok(Some(frame(vec![finger(1, 100, 0, 0)], 1))),
        ];
        let mut px = drive_ready_proxy(&sinks, &haptic, frames);
        let cfg = Config {
            click_1: "left".into(),
            force_1: "left+super".into(),
            sensitivity: 64,
            margin: 5,
            force_offset: 20,
            ..Config::default()
        };
        assert!(!cfg.is_cut_transition());
        let t0 = std::time::Instant::now();
        px.handle_frame_at(&cfg, spaced_at(t0, 0)).unwrap();
        px.handle_frame_at(&cfg, spaced_at(t0, 1)).unwrap();
        assert_eq!(
            key_values(&sinks.pointer, BTN_LEFT),
            vec![1],
            "continuous must not cut under hold"
        );
        assert!(sinks.extras.has(EV_KEY, 125, 1));
    }

    #[test]
    fn force_combo_emits_super_before_left_regardless_of_config_order() {
        // Window-drag needs super held when left lands. `down_keys` emits
        // extras-routed codes before pointer buttons, so `left+super`
        // still presses super first. Pinned via `held_force` order (push
        // follows emit order).
        for force in ["left+super", "super+left"] {
            let sinks = TestSinks::new();
            let haptic = Arc::new(FakeHaptic::new());
            let frames = vec![
                Ok(Some(frame(vec![finger(1, 70, 0, 0)], 0))),
                Ok(Some(frame(vec![finger(1, 100, 0, 0)], 0))),
                Ok(Some(frame(vec![finger(1, 100, 5, 5)], 0))),
                Ok(Some(frame(vec![finger(1, 100, 9, 9)], 0))),
            ];
            let mut px = drive_ready_proxy(&sinks, &haptic, frames);
            let cfg = Config {
                click_1: "left".into(),
                force_1: force.into(),
                sensitivity: 64,
                margin: 5,
                force_offset: 20,
                transition_mode: "cut".into(),
                ..Config::default()
            };
            let t0 = std::time::Instant::now();
            px.handle_frame_at(&cfg, spaced_at(t0, 0)).unwrap();
            px.handle_frame_at(&cfg, spaced_at(t0, 1)).unwrap();
            // Deferred: nothing held yet on the edge frame.
            assert!(px.held_force.is_empty(), "press deferred: {force:?}");
            // Modifiers stage first, buttons follow the stagger.
            px.handle_frame_at(&cfg, spaced_at(t0, 2)).unwrap();
            assert_eq!(
                px.held_force,
                vec![(EV_KEY, 125)],
                "modifiers first for force_1={force:?}"
            );
            px.handle_frame_at(&cfg, spaced_at(t0, 3)).unwrap();
            assert_eq!(
                px.held_force,
                vec![(EV_KEY, 125), (EV_KEY, BTN_LEFT)],
                "super-first for force_1={force:?}"
            );
        }
    }

    #[test]
    fn spoof_wire_reports_virtual_count_selection_stays_real() {
        // spoof_click_2 = 3: two real fingers press click_2 (right), but
        // the wire carries three contacts (TRIPLETAP, three slots).
        let sinks = TestSinks::new();
        let haptic = Arc::new(FakeHaptic::new());
        let frames = vec![Ok(Some(frame(
            vec![finger(1, 200, 0, 0), finger(2, 200, 9, 9)],
            0,
        )))];
        let mut px = drive_ready_proxy(&sinks, &haptic, frames);
        let cfg = Config {
            click_2: "right".into(),
            spoof_click_2: 3,
            ..Config::default()
        };
        let ev = px.handle_frame(&cfg).unwrap().unwrap();
        assert_eq!(ev.finger_count, 2, "selection uses the real count");
        assert!(sinks.pointer.has(EV_KEY, BTN_RIGHT, 1));
        // Wire: three tracking ids, tripletap classification.
        let evs = wire_events(&sinks.pointer);
        let ids: Vec<i32> = evs
            .iter()
            .copied()
            .filter(|&(ty, c, _)| ty == EV_ABS && c == ABS_MT_TRACKING_ID)
            .map(|(_, _, v)| v)
            .collect();
        assert_eq!(ids, vec![1, 2, 3], "virtual contacts: {ids:?}");
        assert!(sinks.pointer.has(EV_KEY, BTN_TOOL_TRIPLETAP, 1));
        assert!(!sinks.pointer.has(EV_KEY, BTN_TOOL_DOUBLETAP, 1));
    }

    #[test]
    fn spoof_select_switches_wire_and_action_together() {
        // spoof_select = true: the spoofed count also selects click_3.
        let sinks = TestSinks::new();
        let haptic = Arc::new(FakeHaptic::new());
        let frames = vec![Ok(Some(frame(
            vec![finger(1, 200, 0, 0), finger(2, 200, 9, 9)],
            0,
        )))];
        let mut px = drive_ready_proxy(&sinks, &haptic, frames);
        let cfg = Config {
            click_2: "right".into(),
            click_3: "middle".into(),
            spoof_click_2: 3,
            spoof_select: true,
            ..Config::default()
        };
        let ev = px.handle_frame(&cfg).unwrap().unwrap();
        assert_eq!(ev.finger_count, 3);
        assert!(sinks.pointer.has(EV_KEY, BTN_MIDDLE, 1));
        assert!(!sinks.pointer.has(EV_KEY, BTN_RIGHT, 1));
    }

    #[test]
    fn spoof_slide_composes_with_slide_action() {
        // One light finger: slide_1 fires (real-count selection) while the
        // wire reports the spoofed pair.
        let sinks = TestSinks::new();
        let haptic = Arc::new(FakeHaptic::new());
        let frames = vec![Ok(Some(frame(vec![finger(1, 20, 0, 0)], 0)))];
        let mut px = drive_ready_proxy(&sinks, &haptic, frames);
        let cfg = Config {
            slide_1: "left".into(),
            spoof_slide_1: 2,
            ..Config::default()
        };
        let ev = px.handle_frame(&cfg).unwrap().unwrap();
        assert!(ev.is_slide());
        assert_eq!(ev.finger_count, 1);
        assert!(sinks.pointer.has(EV_KEY, BTN_LEFT, 1));
        assert!(sinks.pointer.has(EV_KEY, BTN_TOOL_DOUBLETAP, 1));
        assert!(!sinks.pointer.has(EV_KEY, BTN_TOOL_FINGER, 1));
    }
}
