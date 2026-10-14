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
//! - Grab ONLY when `enhanced_clicks=true` (default false → zero behavior
//!   change, pure passthrough: no grab, no uinput traffic).
//! - Proxy node: Type-B MT slots + tracking IDs, `res == 0` arbitrary
//!   0..253 range, `BTN_LEFT`/`BTN_TOUCH` + `SYN`. NEVER advertises
//!   `EV_FF`/`FF_HAPTIC` (no 2026 libinput/Wayland consumer).
//! - Orientation emits `-(raw)`; pressure clamps to 253 (kernel `absmax`).
//! - `poll_frame` errors → log + drop that frame, never abort the loop.
//! - Grab failure → log + passthrough continues. Clicks NEVER blocks init:
//!   hub callbacks do bounded work only (no loops), and panics are already
//!   isolated by [`crate::init::DeviceReadyHub`].
//! - `speed` scales pointer deltas in the proxy path only; init untouched.
//! - Force behavior belongs to the force-click task: only the arming
//!   threshold is exposed ([`crate::clicks::ClickMachine::force_threshold`]).

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use anyhow::Result;

use crate::apple_parse::{TouchFrame, TouchSource, PRESSURE_VALID_MAX};
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

/// Capability allowlist for the virtual node. `EV_FF` must never appear.
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
        "ctrl" | "control" | "lctrl" | "rctrl" => Some((EV_KEY, 29)),
        "shift" | "lshift" | "rshift" => Some((EV_KEY, 42)),
        "alt" | "lalt" | "ralt" => Some((EV_KEY, 56)),
        "meta" | "super" | "cmd" | "command" | "win" | "lmeta" | "rmeta" => Some((EV_KEY, 125)),
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

fn keycode_fn(s: &str) -> Option<(u16, u16)> {
    let n: u8 = s[1..].parse().ok()?;
    if (1..=12).contains(&n) {
        Some((EV_KEY, 58 + u16::from(n)))
    } else {
        None
    }
}

// ---- proxy ----

/// Snapshot of the settings this proxy acts on.
#[derive(Debug, Clone)]
pub struct ProxySettings {
    pub enhanced: bool,
    pub haptic: u16,
    pub quiet: bool,
    pub speed: u32,
}

impl From<&Config> for ProxySettings {
    fn from(c: &Config) -> Self {
        Self {
            enhanced: c.enhanced_clicks,
            haptic: c.haptic,
            quiet: c.quiet_click,
            speed: c.speed,
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
    held: Vec<(u16, u16)>,
    last_pos: HashMap<u8, (i32, i32)>,
    phys_node: String,
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
            held: Vec::new(),
            last_pos: HashMap::new(),
            phys_node: String::new(),
        }
    }

    pub fn is_attached(&self) -> bool {
        self.attached
    }

    /// True when events flow untouched (gate off, or grab failed).
    pub fn is_passthrough(&self) -> bool {
        self.passthrough
    }

    /// Attach on `Ready`: grab the physical node (opt-in only). Failure →
    /// log + passthrough, never `Err` (clicks must not fail init).
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
        self.phys_node = event.devnode.clone();
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

    /// Detach on `Detached`: release grab, drop latch + held keys.
    pub fn detach(&mut self, event: &DeviceReadyEvent) {
        if event.kind != ReadyKind::Detached {
            return;
        }
        if self.attached {
            log::info!("proxy: releasing grab ({})", event.devnode);
            self.grab.ungrab();
        }
        self.release_held();
        self.machine.reset();
        self.last_pos.clear();
        self.attached = false;
        self.passthrough = true;
    }

    /// One poll step: `poll_frame` errors are logged + dropped (`Ok(None)`),
    /// never propagated (the read loop must not abort on a bad frame).
    /// Gate-off or passthrough short-circuits without touching hardware.
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
        self.emit_touch_frame(&frame);
        let was_active = self.machine.active().is_some();
        let edge = match self.machine.update(&frame.fingers, frame.clicks, cfg) {
            Ok(e) => e,
            Err(e) => {
                log::warn!("proxy: click_N parse failed, dropping edge: {e:#}");
                return Ok(None);
            }
        };
        if let Some(ref ev) = edge {
            self.press_edge(ev);
            return Ok(edge);
        }
        if was_active && self.machine.active().is_none() {
            self.release_held();
            self.uinput.sync();
        }
        // Physical button held without a latched click still shows BTN_LEFT.
        if !was_active && ClickMachine::physical_button(frame.clicks) {
            self.uinput.emit(EV_KEY, BTN_LEFT, 1);
            self.uinput.sync();
        }
        Ok(None)
    }

    fn press_edge(&mut self, ev: &ClickEvent) {
        use crate::config::ActionKind;
        match &ev.action.kind {
            ActionKind::None => {
                log::debug!("proxy: click_{} = none (swallowed)", ev.finger_count);
            }
            ActionKind::Empty => {
                self.fire_haptic(ev);
                self.uinput.sync();
            }
            ActionKind::Keys(keys) => {
                for k in keys {
                    match key_token_to_code(k) {
                        Some((ty, code)) => {
                            self.uinput.emit(ty, code, 1);
                            self.held.push((ty, code));
                        }
                        None => log::warn!("proxy: no keycode for '{k}', skipped"),
                    }
                }
                self.fire_haptic(ev);
                self.uinput.sync();
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

    fn release_held(&mut self) {
        for (ty, code) in self.held.drain(..) {
            self.uinput.emit(ty, code, 0);
        }
    }

    /// Emit Type-B MT slots for one frame (res==0 arbitrary units).
    fn emit_touch_frame(&mut self, frame: &TouchFrame) {
        let any_touch = frame.fingers.iter().any(|f| f.contact);
        for (slot, f) in frame.fingers.iter().enumerate() {
            let slot = slot.min(15) as i32;
            self.uinput.emit(EV_ABS, ABS_MT_SLOT, slot);
            if f.contact {
                self.uinput
                    .emit(EV_ABS, ABS_MT_TRACKING_ID, i32::from(f.id));
                let (sx, sy) = self.scaled_pos(f.id, f.x, f.y);
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
            }
        }
        if frame.fingers.is_empty() {
            self.last_pos.clear();
        }
        self.uinput.emit(
            EV_KEY,
            BTN_TOUCH,
            i32::from(any_touch || ClickMachine::physical_button(frame.clicks)),
        );
        let btn_left = self.machine.active().is_some_and(|e| !e.is_none())
            || ClickMachine::physical_button(frame.clicks);
        self.uinput.emit(EV_KEY, BTN_LEFT, i32::from(btn_left));
        // PRESSURE_VALID_MAX documents the clamp duty; keep the import live.
        debug_assert!(u32::from(PRESSURE_VALID_MAX) == 253);
        self.uinput.sync();
    }

    /// Speed-scaled position: `prev + (cur - prev) * speed/100` per finger.
    fn scaled_pos(&mut self, id: u8, x: i32, y: i32) -> (i32, i32) {
        let speed = self.settings.speed;
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
    node: String,
    fd: Mutex<Option<std::fs::File>>,
    grabbed: std::sync::atomic::AtomicBool,
}

impl RealGrab {
    pub fn new(node: &str) -> Self {
        Self {
            node: node.to_string(),
            fd: Mutex::new(None),
            grabbed: std::sync::atomic::AtomicBool::new(false),
        }
    }
}

impl GrabDevice for RealGrab {
    fn grab(&self) -> Result<()> {
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&self.node)
            .map_err(|e| anyhow::anyhow!("opening evdev '{}': {e}", self.node))?;
        let ret = unsafe { libc::ioctl(file.as_raw_fd(), eviocgrab_request() as libc::c_ulong, 1) };
        if ret < 0 {
            return Err(anyhow::anyhow!(
                "EVIOCGRAB on '{}': {}",
                self.node,
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
                libc::write(f.as_raw_fd(), bytes.as_ptr().cast(), bytes.len());
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
        grabbed: RefCell<bool>,
    }

    impl FakeGrab {
        fn ok() -> Self {
            Self {
                fail: false,
                grabbed: RefCell::new(false),
            }
        }
        fn failing() -> Self {
            Self {
                fail: true,
                grabbed: RefCell::new(false),
            }
        }
    }

    impl GrabDevice for FakeGrab {
        fn grab(&self) -> Result<()> {
            if self.fail {
                anyhow::bail!("fake grab: EACCES");
            }
            *self.grabbed.borrow_mut() = true;
            Ok(())
        }
        fn ungrab(&self) {
            *self.grabbed.borrow_mut() = false;
        }
        fn is_grabbed(&self) -> bool {
            *self.grabbed.borrow()
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
            orientation: 3,
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
        let grab = FakeGrab::ok();
        let mut px = ClickProxy::new(
            settings(false),
            FakeGrab::ok(),
            FakeUinput::new(),
            FakeHaptic::new(),
            ScriptSource::new(vec![]),
        );
        px.attach(&usb_ready("/dev/hidraw1"));
        assert!(!px.is_attached());
        assert!(px.is_passthrough());
        assert!(!grab.is_grabbed());
        // handle_frame short-circuits even with a failing source.
        let cfg = Config::default();
        assert!(px.handle_frame(&cfg).unwrap().is_none());
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
        px.attach(&usb_ready("/dev/hidraw1"));
        let cfg = Config::default();
        let ev = px.handle_frame(&cfg).unwrap().unwrap();
        assert_eq!(ev.finger_count, 1);
        // 255 clamps to 253; orientation 3 negates to -3.
        assert!(uinput.has(EV_ABS, ABS_MT_PRESSURE, 253));
        assert!(uinput.has(EV_ABS, ABS_MT_ORIENTATION, -3));
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
        let mut st = settings(true);
        st.speed = 200;
        let mut px = ClickProxy::new(
            st,
            FakeGrab::ok(),
            SharedUinput(Arc::clone(&uinput)),
            SharedHaptic(Arc::clone(&haptic)),
            src,
        );
        px.attach(&usb_ready("/dev/hidraw1"));
        let cfg = Config {
            enhanced_clicks: true,
            sensitivity: 64,
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
    fn subscribe_attaches_on_ready() {
        let hub = DeviceReadyHub::new();
        let grab_ok = Arc::new(FakeUinput::new());
        let _ = grab_ok;
        let proxy = Arc::new(Mutex::new(ClickProxy::new(
            settings(true),
            FakeGrab::ok(),
            FakeUinput::new(),
            FakeHaptic::new(),
            ScriptSource::new(vec![]),
        )));
        subscribe_to_hub(&hub, Arc::clone(&proxy));
        hub.emit(&usb_ready("/dev/hidraw1"));
        assert!(proxy.lock().unwrap().is_attached());
        hub.emit(&detached("/dev/hidraw1", Transport::Usb));
        assert!(!proxy.lock().unwrap().is_attached());
    }
}
