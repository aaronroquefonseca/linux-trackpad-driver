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
    /// True when the uinput node exists (UI_DEV_CREATE done).
    /// Attach refuses to grab until this holds: grabbing without an
    /// emit path would swallow the physical device.
    fn is_ready(&self) -> bool {
        true
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
            attached_node: None,
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
    pub fn handle_frame(&mut self, cfg: &Config) -> Result<Option<ClickEvent>> {
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

/// Virtual device identity (proxy node, not a hardware claim).
pub const UINPUT_NAME: &str = "Linux Trackpad Driver";
pub const UINPUT_BUSTYPE: u16 = 0x03;
pub const UINPUT_VENDOR: u16 = 0x05ac;
pub const UINPUT_PRODUCT: u16 = 0x0324;
pub const UINPUT_VERSION: u16 = 1;

/// Absolute axis ranges for the proxy node (`res == 0` everywhere:
/// arbitrary units, never grams/newtons). Pressure clamps to 253
/// (kernel `absmax`); orientation emits `-(raw)` so the range is the
/// negated device range widened to ±32 (matches old-service tolerance).
/// Positions use the old-service Trackpad2 dimensions.
pub fn uinput_abs_setup() -> Vec<(u16, i32, i32, i32)> {
    vec![
        (ABS_MT_SLOT, 0, 15, 0),
        (ABS_MT_TRACKING_ID, 0, 65535, 0),
        (ABS_MT_POSITION_X, -3678, 3934, 0),
        (ABS_MT_POSITION_Y, -2478, 2587, 0),
        (ABS_MT_PRESSURE, 0, 253, 0),
        (ABS_MT_ORIENTATION, -32, 32, 0),
    ]
}

/// Key bits for the proxy node: buttons + every keyboard code the
/// click maps can emit. `EV_FF` is never set (no 2026 consumer).
pub fn uinput_keybits() -> Vec<u16> {
    let mut keys: Vec<u16> = (1u16..=255).collect();
    for extra in [
        BTN_LEFT, BTN_RIGHT, BTN_MIDDLE, BTN_SIDE, BTN_EXTRA, BTN_TOUCH, 364, 387,
    ] {
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
/// Created BEFORE grab; [`UinputSink::is_ready`] gates attach.
/// Never sets `EV_FF` (see [`uinput_evbits`]). Emits are `input_event`
/// writes once the fd exists; pre-create emits are debug-logged only.
pub struct RealUinput {
    node: String,
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
        Self {
            node: node.to_string(),
            fd: Mutex::new(None),
            created: std::sync::atomic::AtomicBool::new(false),
            staging: Mutex::new(Vec::new()),
        }
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

    fn ioctl(fd: std::os::unix::io::RawFd, req: u64, arg: *mut libc::c_void) -> Result<()> {
        let ret = unsafe { libc::ioctl(fd, req as libc::c_ulong, arg) };
        if ret < 0 {
            return Err(anyhow::anyhow!(
                "uinput ioctl {req:#x}: {}",
                std::io::Error::last_os_error()
            ));
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
        let r = Self::create_on(fd);
        if r.is_err() {
            *guard = None;
        } else {
            self.created
                .store(true, std::sync::atomic::Ordering::SeqCst);
            log::info!("proxy: uinput node {} created ({UINPUT_NAME})", self.node);
        }
        r
    }

    #[cfg(test)]
    pub(crate) fn fd_open(&self) -> bool {
        self.fd.lock().unwrap_or_else(|e| e.into_inner()).is_some()
    }

    fn create_on(fd: std::os::unix::io::RawFd) -> Result<()> {
        for ev in uinput_evbits() {
            let mut v = ev as libc::c_int;
            Self::ioctl(fd, ui_set_evbit(), (&mut v as *mut libc::c_int).cast())?;
        }
        for key in uinput_keybits() {
            let mut v = key as libc::c_int;
            Self::ioctl(fd, ui_set_keybit(), (&mut v as *mut libc::c_int).cast())?;
        }
        for (code, mn, mx, res) in uinput_abs_setup() {
            let mut v = code as libc::c_int;
            Self::ioctl(fd, ui_set_absbit(), (&mut v as *mut libc::c_int).cast())?;
            let setup = UinputAbsSetup {
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
            Self::ioctl(
                fd,
                ui_abs_setup(),
                (&setup as *const UinputAbsSetup).cast_mut().cast(),
            )?;
        }
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
        let name = UINPUT_NAME.as_bytes();
        let n = name.len().min(79);
        setup.name[..n].copy_from_slice(&name[..n]);
        Self::ioctl(
            fd,
            ui_dev_setup(),
            (&setup as *const UinputSetup).cast_mut().cast(),
        )?;
        Self::ioctl(fd, ui_dev_create(), std::ptr::null_mut())?;
        Ok(())
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

/// Actuator report builder (reimplemented from observed old-service
/// behavior + kernel RE notes, no code copied).
///
/// 15-byte vibration report:
/// `[0xF2, 0x53, 0x01, pressure, amplitude, 0x02, intensity,
///   0x24, 0x30, 0x06, 0x01, pattern, 0x18, 0x48, 0x12]`
/// - `intensity` = click strength (`haptic`, 0 = silent, never sent).
/// - `amplitude` = `quiet ? 0xFF : 0x78`, `pattern` = `quiet ? 0 : 6`.
/// - `pressure` echoes the strength here.
///
/// UNCERTAINTY (pending owner HW verify):
/// - Old service drove `pressure` from the press threshold (sensitivity)
///   with up-pulse at `sensitivity*3/5`; we only emit the down pulse per
///   press edge and echo strength as pressure. Waveform feel may differ.
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
pub fn vibration_report(strength: u8, quiet: bool) -> [u8; 15] {
    let amplitude = if quiet { 0xFF } else { 0x78 };
    let pattern = if quiet { 0x00 } else { 0x06 };
    [
        0xF2, 0x53, 0x01, strength, amplitude, 0x02, strength, 0x24, 0x30, 0x06, 0x01, pattern,
        0x18, 0x48, 0x12,
    ]
}

/// Real haptic sender: fires the `0x53` actuator report per press edge.
/// `strength == 0` is silent (no bytes on the wire). Empty node = log-only
/// (daemon has not attached yet); never fails attach.
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
    fn click_feedback(&self, strength: u8, quiet: bool) -> Result<()> {
        if strength == 0 {
            return Ok(());
        }
        let node = self.node.lock().unwrap_or_else(|e| e.into_inner()).clone();
        let bt = self.bt.load(std::sync::atomic::Ordering::SeqCst);
        let report = vibration_report(strength, quiet);
        log::info!(
            "haptic feedback: strength={strength} quiet={quiet} (report 53, node={node} bt={bt})"
        );
        if node.is_empty() {
            return Ok(());
        }
        let payload: &[u8] = if bt { &report } else { &report[1..] };
        // Strict send: EPIPE/EIO (device NAK) must propagate so the
        // fallback below runs. The lenient init variant would swallow
        // the NAK as success → silent no-haptic with no fallback.
        if let Err(e) = crate::init::send_feature_report_hidraw_strict(&node, payload) {
            log::warn!("haptic feature report failed ({e:#}); trying plain write fallback");
            if let Err(e2) = plain_write(&node, payload) {
                log::error!(
                    "haptic FAILED on {node} (feature: {e:#}; write: {e2:#}); clicks stay silent until re-attach"
                );
            }
        }
        Ok(())
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
    fn disconnect_self_releases_once_with_bounded_polls() {
        // Same-name replug: the fd is dead (0-byte/ENODEV) but no
        // Detached will ever bound the errors. The proxy must release
        // exactly once (grab, keys, SYN, latch, pump flag) and then
        // stop polling the dead source — bounded logs, session
        // revivable on fresh wiring.
        use anyhow::Error as AnyhowError;
        let grab = Arc::new(FakeGrab::ok());
        let uinput = Arc::new(FakeUinput::new());
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
            SharedUinput(Arc::clone(&uinput)),
            FakeHaptic::new(),
            ScriptSource::new(frames),
        );
        px.set_pump_ready(true);
        px.attach(&usb_ready("/dev/hidraw8"));
        let cfg = Config::default();
        px.handle_frame(&cfg).unwrap(); // press → BTN_LEFT down
        assert_eq!(key_values(&uinput, BTN_LEFT), vec![1]);
        px.handle_frame(&cfg).unwrap(); // disconnect → self-release
        assert!(!grab.is_grabbed(), "dead fd must release the grab");
        assert!(!px.is_attached());
        assert!(px.is_passthrough());
        assert_eq!(
            key_values(&uinput, BTN_LEFT),
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
        let uinput = Arc::new(FakeUinput::new());
        let mut px = ClickProxy::new(
            settings(true),
            SharedGrab(Arc::clone(&grab)),
            SharedUinput(Arc::clone(&uinput)),
            FakeHaptic::new(),
            ScriptSource::new(vec![]),
        );
        px.set_pump_ready(true);
        px.attach(&usb_ready("/dev/hidraw8"));
        assert!(px.is_attached());
        assert!(grab.is_grabbed());
        let events_before = uinput.events.lock().unwrap().len();
        px.detach(&detached("/dev/hidraw10", Transport::Usb));
        assert!(px.is_attached(), "sibling Detached released the live grab");
        assert!(!px.is_passthrough());
        assert!(grab.is_grabbed(), "sibling Detached dropped the evdev grab");
        assert_eq!(
            uinput.events.lock().unwrap().len(),
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
    fn uinput_ioctls_pin_kernel_layout() {
        assert_eq!(ui_set_evbit(), 0x4004_5564);
        assert_eq!(ui_set_keybit(), 0x4004_5565);
        assert_eq!(ui_set_absbit(), 0x4004_5567);
        assert_eq!(ui_dev_create(), 0x5501);
        assert_eq!(ui_dev_destroy(), 0x5502);
        // SETUP sizes ride the struct layouts (92 + 28 on 64-bit Linux).
        assert_eq!(ui_dev_setup() & 0xffff, 0x5503);
        assert_eq!(ui_abs_setup() & 0xffff, 0x5504);
    }

    #[test]
    fn uinput_setup_is_type_b_res0_no_ff() {
        let abs = uinput_abs_setup();
        let get = |code: u16| abs.iter().find(|(c, _, _, _)| *c == code).unwrap();
        assert_eq!(get(ABS_MT_SLOT), &(ABS_MT_SLOT, 0, 15, 0));
        assert_eq!(get(ABS_MT_PRESSURE), &(ABS_MT_PRESSURE, 0, 253, 0));
        assert!(get(ABS_MT_ORIENTATION).2 >= 3);
        assert!(abs.iter().all(|(_, _, _, res)| *res == 0));
        let keys = uinput_keybits();
        for must in [BTN_LEFT, BTN_RIGHT, BTN_MIDDLE, BTN_TOUCH] {
            assert!(keys.contains(&must), "missing keybit {must}");
        }
        assert!(!uinput_evbits().contains(&EV_FF));
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
        assert_eq!(
            vibration_report(5, false),
            [0xF2, 0x53, 0x01, 5, 0x78, 0x02, 5, 0x24, 0x30, 0x06, 0x01, 0x06, 0x18, 0x48, 0x12]
        );
        assert_eq!(
            vibration_report(9, true),
            [0xF2, 0x53, 0x01, 9, 0xFF, 0x02, 9, 0x24, 0x30, 0x06, 0x01, 0x00, 0x18, 0x48, 0x12]
        );
    }

    #[test]
    fn silent_haptic_sends_nothing() {
        let h = RealHaptic::new();
        h.set_node("/nonexistent-haptic-xyz");
        // strength 0: silent, Ok, no I/O attempted (would err otherwise).
        assert!(h.click_feedback(0, false).is_ok());
        assert!(h.click_feedback(0, true).is_ok());
    }

    #[test]
    fn haptic_without_node_is_log_only_ok() {
        let h = RealHaptic::new();
        assert!(h.click_feedback(5, false).is_ok());
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
        assert!(h.click_feedback(5, false).is_ok());
        assert_eq!(
            std::fs::read(&usb).unwrap(),
            &vibration_report(5, false)[1..]
        );
        h.set_node(bt.to_str().unwrap());
        h.set_bluetooth(true);
        assert!(h.click_feedback(9, true).is_ok());
        assert_eq!(std::fs::read(&bt).unwrap(), &vibration_report(9, true));
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

    // ---- Click-emission wire-purity regression net ----
    //
    // Wire hygiene for the proxy: these tests pin the COMPLETE wire event
    // set per scenario — press+hold+release must carry ONLY the action's
    // own codes plus `BTN_TOUCH`/`SYN` and the six MT `ABS` codes, with
    // zero other `EV_KEY` of any value — so any future unexpected emission
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
    const ALLOWED_ABS: [u16; 6] = [
        ABS_MT_SLOT,
        ABS_MT_TRACKING_ID,
        ABS_MT_POSITION_X,
        ABS_MT_POSITION_Y,
        ABS_MT_PRESSURE,
        ABS_MT_ORIENTATION,
    ];

    fn wire_events(uinput: &FakeUinput) -> Vec<(u16, u16, i32)> {
        uinput.events.lock().unwrap().clone()
    }

    /// Assert the COMPLETE wire stream uses only expected codes:
    /// - `EV_SYN` is exactly `(SYN_REPORT, 0)`;
    /// - `EV_ABS` is one of the six MT codes;
    /// - `EV_KEY` is in `allowed_keys` with value 0/1 (this is the primary
    ///   guard; the extra `KEY_MINUS` assert below is a redundant pin);
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
                        allowed_keys.contains(&code),
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
    /// `cfg`, returning the complete wire log.
    fn drive_click_sequence(
        frames: Vec<Result<Option<TouchFrame>>>,
        cfg: &Config,
    ) -> (Arc<FakeUinput>, Arc<FakeHaptic>) {
        let uinput = Arc::new(FakeUinput::new());
        let haptic = Arc::new(FakeHaptic::new());
        let mut px = drive_ready_proxy(&uinput, &haptic, frames);
        // Four frames per sequence; extra `None`s are harmless no-ops.
        for _ in 0..4 {
            px.handle_frame(cfg).unwrap();
        }
        (uinput, haptic)
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
        let (uinput, _) = drive_click_sequence(press_hold_release_liftoff(), &cfg);
        let got = wire_events(&uinput);
        let want: Vec<(u16, u16, i32)> = vec![
            // Press frame: touch block + BTN_TOUCH + one BTN_LEFT down + SYN.
            // Edge-triggered: no duplicate re-assert, no stray releases.
            (EV_ABS, ABS_MT_SLOT, 0),
            (EV_ABS, ABS_MT_TRACKING_ID, 1),
            (EV_ABS, ABS_MT_POSITION_X, 10),
            (EV_ABS, ABS_MT_POSITION_Y, 20),
            (EV_ABS, ABS_MT_PRESSURE, 200),
            (EV_ABS, ABS_MT_ORIENTATION, -2),
            (EV_KEY, BTN_TOUCH, 1),
            (EV_KEY, BTN_LEFT, 1),
            (EV_SYN, SYN_REPORT, 0),
            // Hold frame: moved +5/+5 at 100% speed; buttons unchanged, so
            // no BTN_LEFT traffic at all — only touch + BTN_TOUCH + SYN.
            (EV_ABS, ABS_MT_SLOT, 0),
            (EV_ABS, ABS_MT_TRACKING_ID, 1),
            (EV_ABS, ABS_MT_POSITION_X, 15),
            (EV_ABS, ABS_MT_POSITION_Y, 25),
            (EV_ABS, ABS_MT_PRESSURE, 200),
            (EV_ABS, ABS_MT_ORIENTATION, -2),
            (EV_KEY, BTN_TOUCH, 1),
            (EV_SYN, SYN_REPORT, 0),
            // Release frame: pressure 10 <= release 59, latch drops; the
            // contact is still present so touch stays up until liftoff.
            (EV_ABS, ABS_MT_SLOT, 0),
            (EV_ABS, ABS_MT_TRACKING_ID, 1),
            (EV_ABS, ABS_MT_POSITION_X, 15),
            (EV_ABS, ABS_MT_POSITION_Y, 25),
            (EV_ABS, ABS_MT_PRESSURE, 10),
            (EV_ABS, ABS_MT_ORIENTATION, -2),
            (EV_KEY, BTN_TOUCH, 1),
            (EV_KEY, BTN_LEFT, 0),
            (EV_SYN, SYN_REPORT, 0),
            // Liftoff: slot terminated, BTN_TOUCH up, trailing SYN. BTN_LEFT
            // is already up, so it stays silent (no stray release).
            (EV_ABS, ABS_MT_SLOT, 0),
            (EV_ABS, ABS_MT_TRACKING_ID, -1),
            (EV_KEY, BTN_TOUCH, 0),
            (EV_SYN, SYN_REPORT, 0),
        ];
        assert_eq!(got, want, "wire sequence drifted");
        assert_wire_pure(&got, &[BTN_LEFT, BTN_TOUCH]);
        // Edge-triggered pin: exactly one down/up pair, no re-asserts.
        let left = key_values(&uinput, BTN_LEFT);
        assert_eq!(left, vec![1, 0], "{left:?}");
        assert_strict_alternation(&left, "BTN_LEFT");
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
        let uinput = Arc::new(FakeUinput::new());
        let haptic = Arc::new(FakeHaptic::new());
        let source = crate::pump::HidrawSource::new(
            "/dev/hidraw1",
            crate::pump::ScriptReader::bytes(reports),
        );
        let mut px = ClickProxy::new(
            settings(true),
            FakeGrab::ok(),
            SharedUinput(Arc::clone(&uinput)),
            SharedHaptic(Arc::clone(&haptic)),
            source,
        );
        px.set_pump_ready(true);
        px.attach(&usb_ready("/dev/hidraw1"));
        let cfg = Config::default();
        for _ in 0..4 {
            px.handle_frame(&cfg).unwrap();
        }
        let got = wire_events(&uinput);
        let want: Vec<(u16, u16, i32)> = vec![
            (EV_ABS, ABS_MT_SLOT, 0),
            (EV_ABS, ABS_MT_TRACKING_ID, 1),
            (EV_ABS, ABS_MT_POSITION_X, 0),
            (EV_ABS, ABS_MT_POSITION_Y, 0),
            (EV_ABS, ABS_MT_PRESSURE, 200),
            (EV_ABS, ABS_MT_ORIENTATION, -2),
            (EV_KEY, BTN_TOUCH, 1),
            (EV_KEY, BTN_LEFT, 1),
            (EV_SYN, SYN_REPORT, 0),
            (EV_ABS, ABS_MT_SLOT, 0),
            (EV_ABS, ABS_MT_TRACKING_ID, 1),
            (EV_ABS, ABS_MT_POSITION_X, 0),
            (EV_ABS, ABS_MT_POSITION_Y, 0),
            (EV_ABS, ABS_MT_PRESSURE, 200),
            (EV_ABS, ABS_MT_ORIENTATION, -2),
            (EV_KEY, BTN_TOUCH, 1),
            (EV_SYN, SYN_REPORT, 0),
            (EV_ABS, ABS_MT_SLOT, 0),
            (EV_ABS, ABS_MT_TRACKING_ID, 1),
            (EV_ABS, ABS_MT_POSITION_X, 0),
            (EV_ABS, ABS_MT_POSITION_Y, 0),
            (EV_ABS, ABS_MT_PRESSURE, 10),
            (EV_ABS, ABS_MT_ORIENTATION, -2),
            (EV_KEY, BTN_TOUCH, 1),
            (EV_KEY, BTN_LEFT, 0),
            (EV_SYN, SYN_REPORT, 0),
            (EV_ABS, ABS_MT_SLOT, 0),
            (EV_ABS, ABS_MT_TRACKING_ID, -1),
            (EV_KEY, BTN_TOUCH, 0),
            (EV_SYN, SYN_REPORT, 0),
        ];
        assert_eq!(got, want, "hidraw-bytes wire sequence drifted");
        assert_wire_pure(&got, &[BTN_LEFT, BTN_TOUCH]);
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
        let (uinput, _) = drive_click_sequence(frames, &cfg);
        let got = wire_events(&uinput);
        assert_wire_pure(&got, &[BTN_RIGHT, BTN_TOUCH]);
        assert!(!got.contains(&(EV_KEY, BTN_LEFT, 1)), "{got:?}");
        // Exact down/up pair: strengthens first/last to a full pin, and
        // strict alternation rules out duplicate re-asserts.
        let right = key_values(&uinput, BTN_RIGHT);
        assert_eq!(right, vec![1, 0], "{right:?}");
        assert_strict_alternation(&right, "BTN_RIGHT");
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
        let (uinput, _) = drive_click_sequence(frames, &cfg);
        let got = wire_events(&uinput);
        assert_wire_pure(&got, &[BTN_MIDDLE, BTN_TOUCH]);
        assert!(!got.contains(&(EV_KEY, BTN_LEFT, 1)), "{got:?}");
        let middle = key_values(&uinput, BTN_MIDDLE);
        assert_eq!(middle, vec![1, 0], "{middle:?}");
        assert_strict_alternation(&middle, "BTN_MIDDLE");
    }

    #[test]
    fn empty_click_wire_pure() {
        let cfg = Config {
            click_1: "empty".into(),
            ..Config::default()
        };
        let (uinput, haptic) = drive_click_sequence(press_hold_release_liftoff(), &cfg);
        let got = wire_events(&uinput);
        // Haptic only: BTN_TOUCH is the sole EV_KEY allowed on the wire.
        assert_wire_pure(&got, &[BTN_TOUCH]);
        assert!(!haptic.calls.lock().unwrap().is_empty());
    }

    #[test]
    fn none_click_wire_pure() {
        let cfg = Config {
            click_1: "none".into(),
            ..Config::default()
        };
        let (uinput, haptic) = drive_click_sequence(press_hold_release_liftoff(), &cfg);
        let got = wire_events(&uinput);
        // Swallowed gesture: BTN_TOUCH only, no haptic, no buttons.
        assert_wire_pure(&got, &[BTN_TOUCH]);
        assert!(haptic.calls.lock().unwrap().is_empty());
    }

    #[test]
    fn combo_click_wire_pure() {
        // ctrl+c: exactly the combo codes + BTN_TOUCH; each key goes
        // down once and up once (a missing key-up would auto-repeat
        // downstream, turning one press into a character run).
        let cfg = Config {
            click_1: "ctrl+c".into(),
            ..Config::default()
        };
        let (uinput, _) = drive_click_sequence(press_hold_release_liftoff(), &cfg);
        let got = wire_events(&uinput);
        assert_wire_pure(&got, &[29, 46, BTN_TOUCH]);
        assert!(!got.contains(&(EV_KEY, BTN_LEFT, 1)), "{got:?}");
        let ctrl = key_values(&uinput, 29);
        assert_eq!(ctrl, vec![1, 0], "{ctrl:?}");
        assert_strict_alternation(&ctrl, "ctrl");
        let c = key_values(&uinput, 46);
        assert_eq!(c, vec![1, 0], "{c:?}");
        assert_strict_alternation(&c, "c");
    }

    #[test]
    fn new_keys_work_in_combos_wire_pure() {
        // `+` combos accept the new keys too: ctrl+refresh emits exactly
        // the combo codes + BTN_TOUCH, each paired down/up once.
        let cfg = Config {
            click_1: "ctrl+refresh".into(),
            ..Config::default()
        };
        let (uinput, _) = drive_click_sequence(press_hold_release_liftoff(), &cfg);
        let got = wire_events(&uinput);
        assert_wire_pure(&got, &[29, 173, BTN_TOUCH]);
        assert!(!got.contains(&(EV_KEY, BTN_LEFT, 1)), "{got:?}");
        let ctrl = key_values(&uinput, 29);
        assert_eq!(ctrl, vec![1, 0], "{ctrl:?}");
        assert_strict_alternation(&ctrl, "ctrl");
        let refresh = key_values(&uinput, 173);
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
        let (uinput, _) = drive_click_sequence(frames, &cfg);
        let got = wire_events(&uinput);
        assert_wire_pure(&got, &[BTN_LEFT, BTN_TOUCH]);
        let left = key_values(&uinput, BTN_LEFT);
        assert_eq!(left, vec![1, 0], "{left:?}");
        assert_strict_alternation(&left, "BTN_LEFT");
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
        let (uinput, _) = drive_click_sequence(frames, &Config::default());
        let got = wire_events(&uinput);
        assert_wire_pure(&got, &[BTN_LEFT, BTN_TOUCH]);
        let left = key_values(&uinput, BTN_LEFT);
        assert_eq!(left, vec![1, 0], "{left:?}");
        assert_strict_alternation(&left, "BTN_LEFT");
    }

    #[test]
    fn force_cycle_wire_pure() {
        // First hold (left) -> force edge (forward) -> force release back
        // to first hold -> full release: only the two action codes plus
        // BTN_TOUCH on EV_KEY, each paired down/up exactly once.
        let cfg = force_cfg();
        let frames = vec![
            Ok(Some(frame(vec![finger(1, 70, 0, 0)], 0))),
            Ok(Some(frame(vec![finger(1, 100, 0, 0)], 0))),
            Ok(Some(frame(vec![finger(1, 70, 0, 0)], 0))),
            Ok(Some(frame(vec![finger(1, 10, 0, 0)], 0))),
        ];
        let (uinput, _) = drive_click_sequence(frames, &cfg);
        let got = wire_events(&uinput);
        assert_wire_pure(&got, &[BTN_LEFT, BTN_EXTRA, BTN_TOUCH]);
        let left = key_values(&uinput, BTN_LEFT);
        assert_eq!(left, vec![1, 0], "{left:?}");
        assert_strict_alternation(&left, "BTN_LEFT");
        let extra = key_values(&uinput, BTN_EXTRA);
        assert_eq!(extra, vec![1, 0], "{extra:?}");
        assert_strict_alternation(&extra, "BTN_EXTRA");
    }

    #[test]
    fn detach_mid_hold_wire_pure() {
        // Grab released mid-hold: every held code gets exactly one key-up
        // (no stuck keys downstream) and the wire stays pure.
        use crate::init::{detached, Transport};
        let uinput = Arc::new(FakeUinput::new());
        let haptic = Arc::new(FakeHaptic::new());
        let frames = vec![Ok(Some(frame(vec![finger(1, 200, 0, 0)], 0)))];
        let mut px = drive_ready_proxy(&uinput, &haptic, frames);
        let cfg = Config::default();
        px.handle_frame(&cfg).unwrap();
        assert!(uinput.has(EV_KEY, BTN_LEFT, 1));
        px.detach(&detached("/dev/hidraw1", Transport::Usb));
        let got = wire_events(&uinput);
        assert_wire_pure(&got, &[BTN_LEFT, BTN_TOUCH]);
        // Exactly one down/up pair: no duplicate releases, no stuck key.
        let left = key_values(&uinput, BTN_LEFT);
        assert_eq!(left, vec![1, 0], "{left:?}");
        assert_strict_alternation(&left, "BTN_LEFT");
    }

    #[test]
    fn force_disarm_mid_hold_wire_pure() {
        // `force_offset` back to 0 mid-hold: force keys go up once, the
        // first-level hold continues silently, full release ends at zero.
        let uinput = Arc::new(FakeUinput::new());
        let haptic = Arc::new(FakeHaptic::new());
        let frames = vec![
            Ok(Some(frame(vec![finger(1, 70, 0, 0)], 0))),
            Ok(Some(frame(vec![finger(1, 100, 0, 0)], 0))),
            Ok(Some(frame(vec![finger(1, 100, 0, 0)], 0))),
            Ok(Some(frame(vec![finger(1, 10, 0, 0)], 0))),
        ];
        let mut px = drive_ready_proxy(&uinput, &haptic, frames);
        let armed = force_cfg();
        px.handle_frame(&armed).unwrap();
        px.handle_frame(&armed).unwrap();
        assert!(uinput.has(EV_KEY, BTN_EXTRA, 1));
        let off = Config {
            force_offset: 0,
            ..armed
        };
        px.handle_frame(&off).unwrap();
        px.handle_frame(&off).unwrap();
        let got = wire_events(&uinput);
        assert_wire_pure(&got, &[BTN_LEFT, BTN_EXTRA, BTN_TOUCH]);
        // Forward: exactly one down/up pair, released at disarm (frame 3),
        // never re-pressed. Left: down once, up once, at full release.
        let extra = key_values(&uinput, BTN_EXTRA);
        assert_eq!(extra, vec![1, 0], "{extra:?}");
        assert_strict_alternation(&extra, "BTN_EXTRA");
        let left = key_values(&uinput, BTN_LEFT);
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
}
