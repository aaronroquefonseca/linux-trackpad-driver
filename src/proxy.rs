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
//! - Level-transition mode (`transition_mode`): `"cut"` (default)
//!   releases ALL level-1/slide wire keys first (with SYN) then presses
//!   the force combo fresh, so shared codes show up+SYN+down; easing back
//!   below the force release re-presses the still-latched level-1 keys
//!   (one SYN frame, so the restart reads as up/down pairs, never a stuck
//!   key). `"continuous"` holds level-1 keys while force keys join (shared
//!   codes down once, up once — see the reference-held rule below). Skipped for
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
//!   Fresh mixed slides emit extras + SYN first, then pointer buttons
//!   after the selected binding gap (global fallback `combo_stagger_ms`).
//!   Explicit per-binding gaps also work on ordinary clicks/continuous
//!   force; omission preserves their immediate behavior.
//!   A press edge hands off: identical slide/press key sets merge with no
//!   wire flutter, differing sets release slide keys first (one SYN).
//! - Finger-count spoofing (`[spoof.slide]` / `[spoof.click]` /
//!   `[spoof.force]` sparse tables, omitted = passthrough): the touch
//!   frame (slots, `BTN_TOOL_*`, `BTN_TOUCH`) is built from virtual
//!   contacts ([`crate::clicks::spoof_contacts`]) under whichever status
//!   holds (force > first > slide-or-idle-touch). Key selection still
//!   uses the real count (default, recommended) unless
//!   `[spoof] actions_follow_spoof = true`. Defaults are
//!   all passthrough: the wire is byte-identical unless opted in.
//! - Shared codes across levels (`ctrl` in both chords, `left` in both maps)
//!   are reference-held: down once, retained while either level holds, a
//!   single up on final release — no phantom release+press mid-hold.

use std::collections::{hash_map::Entry, HashMap, VecDeque};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, OnceLock};

use anyhow::{Context, Result};

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
/// Single-touch absolute position (owner sysfs 2026-10-17: the physical
/// Apple nodes advertise `ABS_X`/`ABS_Y` alongside the MT slots, and
/// libinput requires them for pointer/touchpad classification — without
/// them button presses never reach compositor pointer bindings while
/// motion still leaks through). Same ranges/density as the MT positions.
pub const ABS_X: u16 = 0x00;
pub const ABS_Y: u16 = 0x01;
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
/// (per-contact raw/emitted/remainder anchor in `scaled_pos`), so the
/// advertised density stays truthful while speed still scales motion
/// exactly once.
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
    fn emit(&self, ev_type: u16, code: u16, value: i32) -> Result<()>;
    fn sync(&self) -> Result<()> {
        self.emit(EV_SYN, SYN_REPORT, 0)
    }
    /// True when the uinput node exists and its output path is healthy.
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

/// Bound on simultaneously outstanding haptic endpoint files.
///
/// Production needs at most a couple (current session plus an in-flight
/// pulse on the retired fd); the margin covers parallel test churn. One
/// background closer and one bounded channel serve the whole process, so
/// this is also the bound on queued disposal work and on fds retained if
/// the closer ever stalls.
const HAPTIC_ENDPOINT_CAP: usize = 32;

/// Bounded background disposal pool (F3).
///
/// One worker thread drops items; handoff is `try_send` and never blocks.
/// Admission reserves a slot *before* acquisition, so live plus queued
/// items never exceed `cap` and the handoff can never find a full
/// channel. Setup failure yields `Err` and callers must degrade
/// explicitly (no acquisition, no synchronous close on the caller).
struct DisposalPool<T: Send + 'static> {
    tx: std::sync::mpsc::SyncSender<T>,
    live: Arc<AtomicUsize>,
    cap: usize,
}

impl<T: Send + 'static> std::fmt::Debug for DisposalPool<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DisposalPool")
            .field("cap", &self.cap)
            .field("live", &self.live.load(Ordering::SeqCst))
            .finish_non_exhaustive()
    }
}

impl<T: Send + 'static> DisposalPool<T> {
    /// Start one daemon-lifetime worker. `spawner` runs the close loop;
    /// tests inject a failing spawner for a deterministic setup-failure
    /// path without touching host resource limits.
    fn start<S>(cap: usize, spawner: S) -> std::io::Result<Arc<Self>>
    where
        S: FnOnce(
            Box<dyn FnOnce() + Send + 'static>,
        ) -> std::io::Result<std::thread::JoinHandle<()>>,
    {
        let (tx, rx) = std::sync::mpsc::sync_channel::<T>(cap);
        let live = Arc::new(AtomicUsize::new(0));
        let live_worker = Arc::clone(&live);
        let handle = spawner(Box::new(move || {
            while let Ok(item) = rx.recv() {
                drop(item);
                live_worker.fetch_sub(1, Ordering::SeqCst);
            }
            // Senders gone: queue already drained by `recv`, so nothing
            // is stranded. Forgotten items (handoff bug path) stay
            // retained until exit by design, bounded by `cap`.
        }))?;
        // Detached daemon lifetime: teardown never joins the closer, so
        // a stalled HID release cannot escape into the shutdown budget.
        // `drop` on the handle detaches; no extra thread is created here.
        drop(handle);
        Ok(Arc::new(Self { tx, live, cap }))
    }

    /// Reserve a slot before acquisition. `None` means at capacity: the
    /// caller must degrade explicitly instead of opening.
    fn reserve(&self) -> Option<Reservation<'_, T>> {
        let mut current = self.live.load(Ordering::SeqCst);
        loop {
            if current >= self.cap {
                return None;
            }
            match self.live.compare_exchange_weak(
                current,
                current + 1,
                Ordering::SeqCst,
                Ordering::SeqCst,
            ) {
                Ok(_) => {
                    return Some(Reservation {
                        pool: self,
                        armed: true,
                    })
                }
                Err(observed) => current = observed,
            }
        }
    }

    /// Nonblocking handoff of a reserved item. Unreachable-by-construction
    /// when every acquisition reserved first (queued never exceeds live,
    /// live never exceeds `cap`, channel bound equals `cap`). The bug-path
    /// fallback retains until exit instead of closing on the caller, still
    /// bounded by `cap` because the slot stays consumed.
    fn handoff(&self, item: T) {
        if let Err(e) = self.tx.try_send(item) {
            log::error!("haptic disposal queue full; retaining endpoint until exit (bounded)");
            match e {
                std::sync::mpsc::TrySendError::Full(item)
                | std::sync::mpsc::TrySendError::Disconnected(item) => {
                    std::mem::forget(item);
                }
            }
        }
    }
}

/// Outstanding-slot guard: releases back to the pool unless disarmed.
/// Prepare disarms once the opened file is owned by an endpoint (the
/// slot is then released by the closer worker after the close).
struct Reservation<'a, T: Send + 'static> {
    pool: &'a DisposalPool<T>,
    armed: bool,
}

impl<T: Send + 'static> Reservation<'_, T> {
    fn disarm(mut self) {
        self.armed = false;
    }
}

impl<T: Send + 'static> Drop for Reservation<'_, T> {
    fn drop(&mut self) {
        if self.armed {
            self.pool.live.fetch_sub(1, Ordering::SeqCst);
        }
    }
}

/// Process-wide endpoint closer, established before any endpoint is
/// admitted. `None` (worker setup failed) means haptics degrade to
/// log-only explicitly: prepare acquires nothing.
static ENDPOINT_POOL: OnceLock<Option<Arc<DisposalPool<std::fs::File>>>> = OnceLock::new();

/// Borrow the process closer, starting it once. Never blocks on HID.
fn endpoint_pool() -> Option<Arc<DisposalPool<std::fs::File>>> {
    ENDPOINT_POOL
        .get_or_init(|| {
            match DisposalPool::start(HAPTIC_ENDPOINT_CAP, |job| {
                std::thread::Builder::new()
                    .name("ltd-haptic-close".into())
                    .spawn(job)
            }) {
                Ok(pool) => Some(pool),
                Err(e) => {
                    log::error!("haptic disposal worker failed to start ({e:#}); haptics log-only");
                    None
                }
            }
        })
        .clone()
}

/// Pinned kernel endpoint for one haptic session target.
///
/// A frozen pathname is NOT a device identity: the kernel may reuse a
/// hidraw pathname for a replacement device after reconnect. The endpoint
/// pins the session's device by holding the open [`std::fs::File`]
/// acquired at attach/route time; both the feature ioctl and the
/// plain-write fallback operate on that same fd, never a later pathname
/// reopen. Clones share the same underlying file description, so queued
/// pulses from one session share one identity and an in-flight pulse
/// keeps the old fd even after retirement installs a replacement.
///
/// The final close never runs on the caller: the last drop hands the
/// file to the bounded process closer, so retirement/teardown never
/// block under the queue mutex, on the input pump thread, or inside the
/// bounded worker join.
#[derive(Debug)]
struct HapticEndpoint {
    file: Mutex<Option<std::fs::File>>,
    pool: Option<Arc<DisposalPool<std::fs::File>>>,
}

impl HapticEndpoint {
    fn empty() -> Arc<Self> {
        Arc::new(Self {
            file: Mutex::new(None),
            pool: None,
        })
    }

    fn with_file(file: std::fs::File, pool: Arc<DisposalPool<std::fs::File>>) -> Arc<Self> {
        Arc::new(Self {
            file: Mutex::new(Some(file)),
            pool: Some(pool),
        })
    }
}

impl Drop for HapticEndpoint {
    fn drop(&mut self) {
        let file = self.file.lock().unwrap_or_else(|e| e.into_inner()).take();
        let Some(f) = file else { return };
        let Some(pool) = self.pool.as_ref() else {
            // Unreachable: a live file always carries its pool (prepare
            // only opens after admission). Retain boundedly rather than
            // closing on the caller if this ever breaks.
            log::error!("haptic endpoint without pool; retaining until exit (bounded)");
            std::mem::forget(f);
            return;
        };
        pool.handoff(f);
    }
}

/// Session snapshot for one queued actuator pulse.
///
/// The pump thread snapshots the current session target at enqueue time;
/// the worker executes against the snapshot, never a live lookup. A stale
/// pulse (older generation) is dropped without touching hardware. A pulse
/// already inside the firmware send at retire completes only against its
/// own pinned endpoint (the old fd), never a replacement device reusing
/// the same pathname: both the feature ioctl and the plain-write fallback
/// run on the snapshot's fd with no later pathname reopen.
///
/// Equality is routing equality (`node` + `bluetooth`) for queue
/// bookkeeping; fd identity is intentionally not compared.
#[derive(Debug, Clone)]
pub struct HapticTarget {
    pub node: String,
    pub bluetooth: bool,
    endpoint: Arc<HapticEndpoint>,
}

impl HapticTarget {
    /// Prepare (open) a session endpoint. Performs filesystem I/O and must
    /// be called outside the proxy mutex (live input path) and outside the
    /// queue mutex; commit with [`AsyncHaptic::retarget_prepared`] or
    /// [`AsyncHaptic::update_route_prepared`].
    ///
    /// Admission is reserved *before* opening against the bounded process
    /// closer: at capacity, or when the closer failed to start, this
    /// degrades to a file-less target explicitly (log-only sends, never a
    /// later pathname reopen) instead of acquiring unbounded work or
    /// closing synchronously on the caller.
    pub fn prepare(node: &str, bluetooth: bool) -> Self {
        if node.is_empty() {
            return Self::empty();
        }
        let Some(pool) = endpoint_pool() else {
            log::warn!("haptic disposal unavailable; degrading '{node}' to log-only");
            return Self::degraded(node, bluetooth);
        };
        let Some(reservation) = pool.reserve() else {
            log::warn!("haptic endpoint capacity reached; degrading '{node}' to log-only");
            return Self::degraded(node, bluetooth);
        };
        match std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(node)
        {
            Ok(file) => {
                reservation.disarm();
                Self {
                    node: node.to_string(),
                    bluetooth,
                    endpoint: HapticEndpoint::with_file(file, pool),
                }
            }
            Err(e) => {
                log::warn!("haptic endpoint unavailable for '{node}': {e:#}");
                Self::degraded(node, bluetooth)
            }
        }
    }

    /// File-less target that keeps its routing identity but never touches
    /// hardware: sends degrade to log-only and must not reopen the
    /// pathname later. Used for empty routing, failed opens, full
    /// disposal capacity, or a failed closer setup.
    fn degraded(node: &str, bluetooth: bool) -> Self {
        Self {
            node: node.to_string(),
            bluetooth,
            endpoint: HapticEndpoint::empty(),
        }
    }

    fn empty() -> Self {
        Self {
            node: String::new(),
            bluetooth: false,
            endpoint: HapticEndpoint::empty(),
        }
    }

    /// Clone the pinned fd for one firmware send. The queue/endpoint
    /// mutexes are held only for the clone; the blocking ioctl/write run
    /// on the clone with no lock held. `None` means log-only (empty
    /// routing or attach-time open failed): the caller must not reopen
    /// the pathname.
    fn try_clone_file(&self) -> Option<std::fs::File> {
        self.endpoint
            .file
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
            .and_then(|f| f.try_clone().ok())
    }
}

impl PartialEq for HapticTarget {
    fn eq(&self, other: &Self) -> bool {
        self.node == other.node && self.bluetooth == other.bluetooth
    }
}

impl Eq for HapticTarget {}

/// Haptic feedback sender (host-click report path).
///
/// `pressure` is the actuator pressure byte (the press threshold —
/// old-service parity), `intensity` the click strength (`0` = silent,
/// never sent); `quiet` selects the soft waveform.
/// `release_feedback` is the release up-pulse (`down_pressure` = the same
/// threshold byte the down pulse carried; the sender scales it to
/// `threshold*3/5` with intensity 0 — old-service parity). Unlike the
/// down pulse it always sends: intensity 0 is the payload, not silence.
///
/// Session scope: [`AsyncHaptic`] tags every queued pulse with a session
/// generation plus a [`HapticTarget`] snapshot and executes via
/// `execute_down`/`execute_up` against that snapshot. Senders without
/// hardware routing use the defaults (snapshot ignored); [`RealHaptic`]
/// pins the report to the snapshot so stale work can never retarget.
/// `retire_session` drops a detached session's queued work; synchronous
/// senders use the default no-op.
pub trait HapticSender: Send {
    fn click_feedback(&self, pressure: u8, intensity: u8, quiet: bool) -> Result<()>;
    fn release_feedback(&self, down_pressure: u8, quiet: bool) -> Result<()>;

    /// Execute one queued down-pulse against its session snapshot.
    fn execute_down(
        &self,
        _target: &HapticTarget,
        pressure: u8,
        intensity: u8,
        quiet: bool,
    ) -> Result<()> {
        self.click_feedback(pressure, intensity, quiet)
    }
    /// Execute one queued up-pulse against its session snapshot.
    fn execute_up(&self, _target: &HapticTarget, down_pressure: u8, quiet: bool) -> Result<()> {
        self.release_feedback(down_pressure, quiet)
    }
    /// Retire this session's queued actuator work (owned detach / pump
    /// loss). No-op for synchronous senders.
    fn retire_session(&self) {}
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
/// Single source is [`crate::action_tokens::token_code`] (codes verified
/// individually against `/usr/include/linux/input-event-codes.h`, never
/// extrapolated; see that module for token collisions and the
/// deliberately-excluded list). This wrapper only pairs the shared code
/// with `EV_KEY`.
pub fn key_token_to_code(token: &str) -> Option<(u16, u16)> {
    crate::action_tokens::token_code(token).map(|code| (EV_KEY, code))
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

/// What one pump step did. Distinguishes hidraw traffic from idle so the
/// pump can wait on input readiness instead of polling continuously:
/// `Frame` (traffic, drain immediately) vs `Idle` (nothing to do, block
/// until input/control/deadline). `Deadline` is idle work that still
/// emitted at a timing deadline (release/stagger/cut); `Disconnect` retires
/// a dead session; `Transient` is a dropped frame needing a short backoff
/// so a persistently failing fd cannot busy-spin.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PumpActivity {
    Idle,
    Frame,
    Deadline,
    Disconnect,
    Transient,
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
    held_slide: Vec<(u16, u16)>,
    last_pos: HashMap<u8, PointerAnchor>,
    /// Highest slot index written so far; vacated slots are terminated.
    max_slot: usize,
    /// Last `BTN_LEFT` value on the wire. `BTN_LEFT` is edge-triggered:
    /// it emits only on state change, so non-left clicks carry zero
    /// `BTN_LEFT` traffic and steady holds stay silent. Pure wire
    /// hygiene; no claim about any downstream symptom.
    btn_left_down: bool,
    /// Bare physical left hold from the last accepted report (no contacts).
    /// Idle release processing preserves it without replaying touch data.
    physical_left_down: bool,
    /// A down pulse fired for the live first-level latch (old-service
    /// `pressed_haptic_triggered` parity): the release edge owes the
    /// actuator its up-pulse. Silent presses (`haptic == 0`, `none`
    /// action) leave this false so the release stays silent too.
    pressed_haptic: bool,
    /// Same for the live force-level latch (`force_haptic_triggered`).
    /// The slide level never pulses (touch is silent): no flag, no debt.
    force_haptic: bool,
    /// Cut-mode deferred force press: the force edge (frame N) emits the
    /// level-1/slide release + SYN, the fresh force-combo modifiers land
    /// once `now - released_at >= CUT_REPRESS_GAP` (5 ms floor on the
    /// monotonic clock, not frame counts), and the buttons follow once
    /// `now - mods_at >= captured_stagger` (human-scale — only
    /// super-first-then-left starts a drag on some compositors). The
    /// release, modifiers and buttons must read as distinct gestures
    /// downstream. If the force latch vanishes before the press lands
    /// (liftoff/ease-off), the pending press is dropped: its keys never
    /// went down, so there is nothing to release and nothing dangles.
    pending_force: Option<PendingForcePress>,
    /// Buttons from fresh mixed click/force/slide actions. Only emitted
    /// keys live in `held_*`; pending keys carry separate ownership so cancellation
    /// cannot release a never-pressed button or revive an old action.
    pending_buttons: Option<PendingButtons>,
}

struct PendingButtons {
    /// A same-action handoff transfers ownership; continuous levels can
    /// also share a pending button without bypassing the original gap.
    keys: Vec<PendingButton>,
}

struct PendingButton {
    deadline: std::time::Instant,
    pair: (u16, u16),
    owners: Vec<ClickLevel>,
}

/// One staged cut-press key group (modifiers or buttons): wire pairs in
/// emit order.
type KeyGroup = Vec<(u16, u16)>;

/// A cut-mode force press waiting for its gap-gated frames (see
/// [`ClickProxy::pending_force`]). `released_at` is the edge frame's
/// clock: the modifier group lands once `now - released_at >=
/// CUT_REPRESS_GAP`. `mods_at` is the modifiers' clock once they land:
/// the button group follows once `now - mods_at >= captured_stagger`
/// (human-scale gap).
#[derive(Debug, Clone)]
struct PendingForcePress {
    ev: ClickEvent,
    released_at: std::time::Instant,
    stagger: std::time::Duration,
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
            physical_left_down: false,
            pressed_haptic: false,
            force_haptic: false,
            pending_force: None,
            pending_buttons: None,
        }
    }

    pub fn is_attached(&self) -> bool {
        self.attached && self.uinput.is_ready()
    }

    /// True when events flow untouched (gate off, pump unready, grab failed).
    pub fn is_passthrough(&self) -> bool {
        self.passthrough
    }

    /// Hidraw readiness fd for the pump's `poll(2)` wait. `None` while idle
    /// (no device, gate-off, passthrough): the pump then blocks on
    /// control/deadline notifications only, producing (near-)zero wakeups.
    pub fn poll_fd(&self) -> Option<std::os::unix::io::RawFd> {
        if !self.settings.enhanced || !self.attached || self.passthrough {
            return None;
        }
        self.source.poll_fd()
    }

    /// True when a decoded frame is queued without touching the fd (double
    /// report second half). The pump drains these immediately.
    pub fn has_pending_frame(&self) -> bool {
        if !self.settings.enhanced || !self.attached || self.passthrough {
            return false;
        }
        self.source.has_pending_frame()
    }

    /// Earliest idle instant needing pump work. `None` means idle polls do
    /// nothing: no latch can release and no staged key-down is due, so the
    /// pump blocks until input/control instead of polling. Covers the 30 ms
    /// release hold, every `pending_buttons` stagger deadline, and both
    /// cut stages (`released_at + 5 ms`, then `mods_at + stagger`).
    pub fn next_deadline(&self) -> Option<std::time::Instant> {
        if !self.settings.enhanced || !self.attached || self.passthrough {
            return None;
        }
        let mut best: Option<std::time::Instant> = None;
        let mut consider = |d: Option<std::time::Instant>| {
            if let Some(d) = d {
                best = Some(best.map_or(d, |b| b.min(d)));
            }
        };
        consider(self.machine.next_release_deadline());
        if let Some(pending) = &self.pending_buttons {
            for key in &pending.keys {
                consider(Some(key.deadline));
            }
        }
        if let Some(p) = &self.pending_force {
            if self.machine.force_active().is_some() {
                match p.mods_at {
                    Some(t) => consider(Some(t + p.stagger)),
                    None => consider(Some(p.released_at + CUT_REPRESS_GAP)),
                }
            }
        }
        best
    }

    /// One pump step with an explicit clock, reporting traffic vs idle.
    /// Same edge/output semantics as [`ClickProxy::handle_frame_at`]; the
    /// activity lets the event-driven pump wait instead of polling.
    pub fn pump_step_at(
        &mut self,
        cfg: &Config,
        now: std::time::Instant,
    ) -> (Result<Option<ClickEvent>>, PumpActivity) {
        let result = self.handle_frame_inner(cfg, now);
        match result {
            Ok((ev, activity)) => (Ok(ev), activity),
            Err(e) => {
                // Single owner for output-failure diagnostics: `output_failed`
                // logs and retires the session; the pump layer propagates
                // silently to avoid duplicate warnings per failure.
                self.output_failed(&e);
                (Err(e), PumpActivity::Disconnect)
            }
        }
    }

    /// Test hook: fake a live attached session (grabbed + ready) without
    /// hardware, so shutdown-teardown tests can drive a surviving live
    /// session through the real release paths.
    #[cfg(test)]
    pub(crate) fn test_claim_live(&mut self, node: &str) {
        self.pump_ready = true;
        self.attached = true;
        self.attached_node = Some(node.to_string());
        self.passthrough = false;
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
    /// without an emit path. Clearing always runs the single idempotent
    /// session teardown, so live virtual contacts are terminated on the
    /// wire (Type-B is stateful: stopping emission alone leaves them
    /// active downstream).
    pub fn set_pump_ready(&mut self, ready: bool) {
        self.pump_ready = ready;
        if !ready {
            self.teardown_session("pump lost");
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

    /// Detach on `Detached`: the single idempotent session teardown
    /// (grab release, held keys up, every virtual contact terminated,
    /// tool bits cleared, trailing `SYN`). Ownership-gated: a `Detached`
    /// for any node other than the grabbed one is ignored, so a sibling
    /// interface vanishing can never release the live grab.
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
        self.teardown_session(&format!("detached {}", event.devnode));
    }

    /// One idempotent teardown for every source-loss path: disconnect
    /// (`set_pump_ready(false)`), explicit detach, output failure,
    /// shutdown, and attachment rollback. Releases the grab first (native
    /// input must resume even when the virtual emit path is broken), then
    /// releases held keys, ends every virtual contact and tool bit on the
    /// wire (kernel Type-B is stateful: stopping emission does not
    /// communicate removal), syncs one trailing frame, and resets session
    /// state.
    /// Safe to call when already down (no duplicate wire traffic): only
    /// sessions with live wire state emit, and state is cleared so a
    /// second call is a no-op. On wire failure the virtual nodes are
    /// retired (`destroy`) so contacts end via device removal instead of
    /// lingering on a broken path. Node destruction itself stays with
    /// the owner (daemon destroys after teardown); this only destroys on
    /// the failure path where emits cannot land.
    pub fn teardown_session(&mut self, reason: &str) {
        let had_wire_state = self.attached
            || !self.held_first.is_empty()
            || !self.held_force.is_empty()
            || !self.held_slide.is_empty()
            || self.btn_left_down
            || self.physical_left_down
            || self.max_slot > 0
            || !self.last_pos.is_empty()
            || self.pending_force.is_some()
            || self.pending_buttons.is_some();
        // Always release the physical grab first: native input must resume
        // even when the virtual emit path is already broken. Idempotent.
        self.grab.ungrab();
        if had_wire_state && self.uinput.is_ready() {
            log::info!("proxy: tearing down session ({reason})");
            let wire = self
                .release_held(false)
                .and_then(|()| self.release_contacts())
                .and_then(|()| self.uinput.sync());
            if let Err(error) = wire {
                log::warn!(
                    "proxy: teardown wire failed ({reason}), retiring virtual nodes: {error:#}"
                );
                let _ = self.uinput.sync();
                self.uinput.destroy();
            }
        } else if had_wire_state {
            // Output already gone: device removal is the termination.
            self.uinput.destroy();
        }
        self.machine.reset();
        self.held_first.clear();
        self.held_force.clear();
        self.held_slide.clear();
        self.btn_left_down = false;
        self.physical_left_down = false;
        self.pressed_haptic = false;
        self.force_haptic = false;
        // Owned detach ends the session: retire queued pulses (tagged with
        // the old generation/target) so they never fire on the next node.
        self.haptic.retire_session();
        self.pending_force = None;
        self.pending_buttons = None;
        self.last_pos.clear();
        self.max_slot = 0;
        self.pump_ready = false;
        self.attached = false;
        self.attached_node = None;
        self.passthrough = true;
    }

    /// Terminate every virtual contact on the wire: each slot written so
    /// far gets `TRACKING_ID = -1`, `BTN_TOUCH` goes to 0, and the full
    /// `BTN_TOOL_*` row clears. No trailing `SYN`: the caller syncs once
    /// after keys + contacts. Pure emission: reset ownership stays in
    /// [`ClickProxy::teardown_session`] (which runs even when the wire
    /// fails), and physical-only `BTN_LEFT` is already released by
    /// [`ClickProxy::release_held`] with `phys_down = false`, so no
    /// fallback emit belongs here.
    fn release_contacts(&mut self) -> Result<()> {
        for slot in 0..=self.max_slot {
            self.uinput.emit(EV_ABS, ABS_MT_SLOT, slot as i32)?;
            self.uinput.emit(EV_ABS, ABS_MT_TRACKING_ID, -1)?;
        }
        self.uinput.emit(EV_KEY, BTN_TOUCH, 0)?;
        self.emit_tool_bits(0)?;
        Ok(())
    }

    /// One poll step: `poll_frame` errors are logged + dropped (`Ok(None)`),
    /// never propagated (the read loop must not abort on a bad frame).
    /// Gate-off, pump-unready, or passthrough short-circuits without touching
    /// hardware. Each handled frame ends with `SYN`; staged combos may
    /// also sync their modifier group or a deadline on an idle poll.
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
        let result = self.handle_frame_inner(cfg, now);
        match result {
            Ok((ev, _)) => Ok(ev),
            Err(error) => {
                self.output_failed(&error);
                Err(error)
            }
        }
    }

    /// Retire the entire failed pointer session. Runs the single
    /// idempotent teardown (contacts + keys terminated on the wire when
    /// still possible, queued haptic pulses retired with a generation
    /// bump), then destroys the virtual nodes so held
    /// keys/contacts end via device removal even when key-up writes
    /// cannot land. The teardown already destroys on its own wire-failure
    /// branch, so this second `destroy` is a deliberate idempotent
    /// re-assert: every [`UinputSink::destroy`] impl (real, dual, fake)
    /// is safe to call twice (pinned by test). `is_attached` is the
    /// health signal consumed by attachment decisions. Extras-only
    /// failures never reach here ([`DualSink`] degrades to pointer-only),
    /// so the live pointer session is preserved there.
    fn output_failed(&mut self, error: &anyhow::Error) {
        log::warn!("proxy: output failed, releasing grab and retiring session: {error:#}");
        let reason = format!("output failed: {error:#}");
        self.teardown_session(&reason);
        self.uinput.destroy();
    }

    fn handle_frame_inner(
        &mut self,
        cfg: &Config,
        now: std::time::Instant,
    ) -> Result<(Option<ClickEvent>, PumpActivity)> {
        if !self.settings.enhanced || !self.attached || self.passthrough {
            return Ok((None, PumpActivity::Idle));
        }
        anyhow::ensure!(self.uinput.is_ready(), "pointer output is unavailable");
        let frame = match self.source.poll_frame() {
            Ok(Some(f)) => f,
            Ok(None) => {
                // Event-driven idle: the pump blocks on input readiness +
                // control/deadline notifications and only wakes here when a
                // deadline expired (or input arrived but decoded empty).
                // Advance deadlines from the last live latch, after polling
                // so disconnect always wins.
                let was_first = self.machine.active().is_some();
                let was_force = self.machine.force_active().is_some();
                let released = self.machine.advance_releases_at(now);
                if released || self.pending_force.is_some() || self.pending_buttons.is_some() {
                    // Releases must win ties with staged key-down deadlines.
                    self.finish_pressure_releases(
                        was_first,
                        was_force,
                        self.physical_left_down,
                        cfg,
                        now,
                    )?;
                    let pressure = cfg.click.sensitivity.clamp(1, 255) as u8;
                    self.flush_cut_pending(pressure, now)?;
                    self.flush_pending_buttons(now)?;
                    self.emit_btn_left()?;
                    self.uinput.sync()?;
                    return Ok((None, PumpActivity::Deadline));
                }
                return Ok((None, PumpActivity::Idle));
            }
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
                        return Ok((None, PumpActivity::Disconnect));
                    }
                    log::debug!("proxy: disconnect poll while idle: {e:#}");
                    return Ok((None, PumpActivity::Idle));
                }
                log::warn!("proxy: poll_frame failed, dropping frame: {e:#}");
                return Ok((None, PumpActivity::Transient));
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
                return Ok((None, PumpActivity::Frame));
            }
        };
        self.physical_left_down =
            frame.fingers.is_empty() && ClickMachine::physical_button(frame.clicks);
        // Haptic pressure byte = the press threshold (old-service parity:
        // `pressure=sensitivity`, `intensity=haptic`; never the strength
        // echoed as pressure).
        let haptic_pressure = cfg.click.sensitivity.clamp(1, 255) as u8;
        // Touch slots first (post-update latch is visible to buttons below;
        // the spoof map follows the live status: force > first > slide).
        self.emit_touch_frame(&frame, cfg)?;
        // A slide handed to a press can still be inside that press's
        // release debounce on clean liftoff. Cancel staged work outright:
        // idle polls must never turn that stale latch into a late down.
        // End the matching latch too, so retouch can retry after the normal
        // press debounce instead of silently reviving an actionless hold.
        if (self.pending_buttons.is_some() || self.pending_force.is_some())
            && !frame.fingers.iter().any(clicks::touch_present)
        {
            self.machine.cancel_pending_press_at(now);
            self.release_held(ClickMachine::physical_button(frame.clicks))?;
        }
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
                    self.release_slide_held(ClickMachine::physical_button(frame.clicks))?;
                }
                self.press_slide(ev, cfg, now)?;
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
            )?
        } else {
            None
        };
        // Cut-mode deferred press (two stages, chronological: the deferred
        // edge is older than this frame's own presses).
        self.flush_cut_pending(haptic_pressure, now)?;
        if let Some(ref ev) = edges.first {
            self.press_level(ev, haptic_pressure, now)?;
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
                && (!self.held_first.is_empty()
                    || self.pending_buttons.as_ref().is_some_and(|p| {
                        p.keys.iter().any(|k| k.owners.contains(&ClickLevel::First))
                    }))
                && absorbed.is_none()
            {
                // No-op after a handoff (slide already absorbed/released),
                // belt-and-braces so cut never leaves a slide hold behind.
                // Cut bypasses the physical carve-out (`false`): the visible
                // break is the entire point of cut, and the user
                // physically holds the click essentially always.
                self.release_slide_held(false)?;
                self.cut_first_wire()?;
                self.uinput.sync()?;
                self.pending_force = Some(PendingForcePress {
                    ev: ev.clone(),
                    released_at: now,
                    stagger: Self::combo_stagger(ev, cfg.force.combo_stagger_ms),
                    mods_at: None,
                });
            } else {
                self.press_level(ev, haptic_pressure, now)?;
            }
        }
        if edges.first.is_some() || edges.force.is_some() || edges.slide.is_some() {
            // Edge frames reconcile `BTN_LEFT` through the wire tracker
            // (emits only on change — fresh downs already went out above,
            // except a cut-mode force edge whose press is deferred to the
            // next frame: the release went out with its SYN, and the
            // reconcile below is a no-op, never a flutter).
            // One trailing SYN per handled frame. Mixed staged presses
            // additionally sync extras before buttons; cut also syncs
            // the first-level release.
            self.flush_pending_buttons(now)?;
            self.emit_btn_left()?;
            self.uinput.sync()?;
            return Ok((edges.primary(), PumpActivity::Frame));
        }
        // A held slide that vanished without a handoff releases its keys.
        let slide_gone = was_slide.is_some() && self.machine.slide_active().is_none();
        if !self.finish_pressure_releases(
            was_first,
            was_force,
            ClickMachine::physical_button(frame.clicks),
            cfg,
            now,
        )? && slide_gone
        {
            self.release_slide_held(ClickMachine::physical_button(frame.clicks))?;
        }
        // Releases and replacements above must cancel before any deadline
        // is serviced, including a liftoff arriving exactly at the deadline.
        self.flush_pending_buttons(now)?;
        self.emit_btn_left()?;
        self.uinput.sync()?;
        Ok((None, PumpActivity::Frame))
    }

    /// Complete pressure-latch transitions on both reports and idle polls.
    /// Cancels pending presses before any of their deadlines are serviced.
    fn finish_pressure_releases(
        &mut self,
        was_first: bool,
        was_force: bool,
        phys_down: bool,
        cfg: &Config,
        now: std::time::Instant,
    ) -> Result<bool> {
        let first_gone = was_first && self.machine.active().is_none();
        let force_gone = was_force && self.machine.force_active().is_none();
        let haptic_pressure = cfg.click.sensitivity.clamp(1, 255) as u8;
        if first_gone {
            // Full release clears both levels. The physical bit rides
            // along so a still-held hardware button suppresses the
            // latch's left-up (no 0-then-1 flutter in one frame).
            self.release_held(phys_down)?;
            // Old-service release snap: force up-pulse first (when its
            // down pulse fired), then the first-level up-pulse. No 15 ms
            // staging sleep (old inserts one between the two): the pump
            // thread must not stall on a release frame.
            if was_force {
                self.fire_release(
                    clicks::ClickLevel::Force,
                    haptic_pressure,
                    cfg.haptics.force_quiet_click,
                );
            }
            self.fire_release(
                clicks::ClickLevel::First,
                haptic_pressure,
                cfg.haptics.quiet_click,
            );
        } else if force_gone {
            // Back to the first-level hold: force keys up, first keys stay
            // (continuous). In cut mode the first keys were released at the
            // force edge, so re-press the still-latched level-1 combo (keys
            // only, no haptic: no new edge fired).
            self.release_force_held()?;
            self.fire_release(
                clicks::ClickLevel::Force,
                haptic_pressure,
                cfg.haptics.force_quiet_click,
            );
            if cfg.is_cut_transition() {
                if let Some(ev) = self.machine.active().cloned() {
                    self.repress_keys(&ev, now)?;
                }
            }
        }
        Ok(first_gone || force_gone)
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
    fn emit_btn_left(&mut self) -> Result<()> {
        let held_left = self
            .held_first
            .iter()
            .chain(self.held_force.iter())
            .chain(self.held_slide.iter())
            .any(|&(ty, code)| ty == EV_KEY && code == BTN_LEFT);
        let down = held_left || self.physical_left_down;
        if down != self.btn_left_down {
            self.uinput.emit(EV_KEY, BTN_LEFT, i32::from(down))?;
            self.btn_left_down = down;
        }
        Ok(())
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

    /// Press a click/force edge: keys down (deduplicated across levels,
    /// see [`ClickProxy::is_down`]) plus that level's own haptic pulse.
    /// Slides use `press_slide` and never request haptics. Edges arrive
    /// staged old-exact (first frame one, force frame two), so each button
    /// pulse lands on its own frame. Timed mixed actions sync extras;
    /// the caller always emits the trailing SYN.
    /// `pressure` is the actuator pressure byte (the press threshold,
    /// old-service parity); the event carries the intensity + waveform.
    fn press_level(
        &mut self,
        ev: &ClickEvent,
        pressure: u8,
        now: std::time::Instant,
    ) -> Result<()> {
        use crate::config::ActionKind;
        debug_assert_ne!(ev.level, ClickLevel::Slide);
        let name = match ev.level {
            ClickLevel::First => "click",
            ClickLevel::Force => "force",
            ClickLevel::Slide => "slide",
        };
        match &ev.action.kind {
            ActionKind::None => {
                log::debug!("proxy: {name}_{} = none (swallowed)", ev.finger_count);
            }
            ActionKind::Empty => {
                let fired = self.fire_haptic(ev, pressure);
                self.set_haptic_flag(ev.level, fired);
            }
            ActionKind::Keys(_) => {
                self.down_keys_timed(ev, Self::combo_stagger(ev, 0), now)?;
                let fired = self.fire_haptic(ev, pressure);
                self.set_haptic_flag(ev.level, fired);
            }
        }
        Ok(())
    }

    /// Slides never pulse. Only fresh mixed actions have a timed division.
    fn press_slide(
        &mut self,
        ev: &ClickEvent,
        cfg: &Config,
        now: std::time::Instant,
    ) -> Result<()> {
        self.down_keys_timed(ev, Self::combo_stagger(ev, cfg.force.combo_stagger_ms), now)?;
        Ok(())
    }

    /// Emit extras first and capture each new button's deadline. Existing
    /// pending keys retain their deadline across shared ownership/handoffs;
    /// independent buttons can wait on different bindings at the same time.
    fn down_keys_timed(
        &mut self,
        ev: &ClickEvent,
        delay: std::time::Duration,
        now: std::time::Instant,
    ) -> Result<()> {
        let (mods, buttons) = Self::partition_combo_keys(ev);
        let mixed = !mods.is_empty() && !buttons.is_empty();
        for pair in mods {
            self.emit_key_down(pair, ev.level)?;
        }
        if mixed && (!delay.is_zero() || ev.level == ClickLevel::Slide) {
            self.uinput.sync()?;
        }
        for pair in buttons {
            if self.defer_pending_button(pair, ev.level) {
                continue;
            }
            if mixed && !delay.is_zero() && !self.is_down(pair) {
                self.pending_buttons
                    .get_or_insert_with(|| PendingButtons { keys: Vec::new() })
                    .keys
                    .push(PendingButton {
                        pair,
                        deadline: now + delay,
                        owners: vec![ev.level],
                    });
            } else {
                self.emit_key_down(pair, ev.level)?;
            }
        }
        Ok(())
    }

    /// Register shared ownership without letting a press handoff shortcut
    /// an existing modifier-to-button gap. Pending keys are not wire-held.
    fn defer_pending_button(&mut self, pair: (u16, u16), level: ClickLevel) -> bool {
        if let Some(pending) = &mut self.pending_buttons {
            if let Some(key) = pending.keys.iter_mut().find(|key| key.pair == pair) {
                if !key.owners.contains(&level) {
                    key.owners.push(level);
                }
                return true;
            }
        }
        false
    }

    fn cancel_pending_buttons(&mut self, level: ClickLevel) {
        if let Some(pending) = &mut self.pending_buttons {
            for key in &mut pending.keys {
                key.owners.retain(|owner| *owner != level);
            }
            pending.keys.retain(|key| !key.owners.is_empty());
            if pending.keys.is_empty() {
                self.pending_buttons = None;
            }
        }
    }

    fn flush_pending_buttons(&mut self, now: std::time::Instant) -> Result<()> {
        let Some(mut pending) = self.pending_buttons.take() else {
            return Ok(());
        };
        let mut waiting = Vec::new();
        for key in pending.keys.drain(..) {
            if now >= key.deadline {
                for level in key.owners {
                    self.emit_key_down(key.pair, level)?;
                }
            } else {
                waiting.push(key);
            }
        }
        if !waiting.is_empty() {
            pending.keys = waiting;
            self.pending_buttons = Some(pending);
        }
        Ok(())
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
    fn emit_key_down(&mut self, pair: (u16, u16), level: ClickLevel) -> Result<()> {
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
                self.uinput.emit(ty, code, 1)?;
                self.btn_left_down = true;
            }
        } else if !self.is_down(pair) {
            self.uinput.emit(ty, code, 1)?;
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
        Ok(())
    }

    /// The level's own haptic pulse for a cut staged press.
    fn pulse_cut_haptic(&mut self, ev: &ClickEvent, pressure: u8) {
        let fired = self.fire_haptic(ev, pressure);
        self.set_haptic_flag(ev.level, fired);
    }

    /// Split an action's keys into (modifiers, buttons): extras-routed
    /// first group, pointer-routed second. Non-key actions yield two empty
    /// groups (cut never stores those; defensive).
    fn partition_combo_keys(ev: &ClickEvent) -> (KeyGroup, KeyGroup) {
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
    /// finish here via [`ClickProxy::press_level`].
    ///
    /// Stage 2 (buttons): once `now - mods_at >= captured_stagger`
    /// (human-scale; `0` lands both stages in one frame, still ≥5 ms after
    /// the release). The haptic pulse fires with stage 1 (earliest feedback
    /// for the threshold crossing).
    fn flush_cut_pending(&mut self, pressure: u8, now: std::time::Instant) -> Result<()> {
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
            return Ok(());
        }
        let Some((ev, released_at, stagger)) = self
            .pending_force
            .as_ref()
            .map(|p| (p.ev.clone(), p.released_at, p.stagger))
        else {
            return Ok(());
        };
        let mods_done = self
            .pending_force
            .as_ref()
            .is_some_and(|p| p.mods_at.is_some());
        // Stage 1 (modifiers): once the 5 ms floor elapses. Single-group
        // actions finish here via [`ClickProxy::press_level`].
        if !mods_done {
            if now.saturating_duration_since(released_at) < CUT_REPRESS_GAP {
                return Ok(());
            }
            let (mods, btns) = Self::partition_combo_keys(&ev);
            if mods.is_empty() || btns.is_empty() {
                self.press_level(&ev, pressure, now)?;
                self.pending_force = None;
                return Ok(());
            }
            for pair in &mods {
                self.emit_key_down(*pair, ev.level)?;
            }
            self.uinput.sync()?;
            self.pulse_cut_haptic(&ev, pressure);
            if let Some(pending) = self.pending_force.as_mut() {
                pending.mods_at = Some(now);
            }
        }
        // Stage 2 (buttons): once the captured binding/path gap elapsed since the
        // modifiers — same frame when the stagger is 0.
        let mods_at = self.pending_force.as_ref().and_then(|p| p.mods_at);
        if let Some(t) = mods_at {
            if now.saturating_duration_since(t) >= stagger {
                let (_, btns) = Self::partition_combo_keys(&ev);
                for pair in &btns {
                    self.emit_key_down(*pair, ev.level)?;
                }
                self.pending_force = None;
            }
        }
        // Else: stagger not elapsed — the buttons wait for a later frame.
        Ok(())
    }

    /// Selected binding override, otherwise this path's legacy fallback.
    /// Cut's independent 5 ms release floor applies first.
    fn combo_stagger(ev: &ClickEvent, fallback_ms: u16) -> std::time::Duration {
        std::time::Duration::from_millis(u64::from(ev.action.stagger_ms.unwrap_or(fallback_ms)))
    }

    /// Re-press the still-latched level-1 combo after a cut-mode force
    /// release (keys only, no haptic: no new edge fired). Records into
    /// `held_first` so the eventual full release owns the key-ups.
    fn repress_keys(&mut self, ev: &ClickEvent, now: std::time::Instant) -> Result<()> {
        use crate::config::ActionKind;
        if let ActionKind::Keys(_) = &ev.action.kind {
            debug_assert!(matches!(ev.level, ClickLevel::First));
            self.down_keys_timed(ev, Self::combo_stagger(ev, 0), now)?;
        }
        Ok(())
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
    ) -> Result<Option<ClickLevel>> {
        let Some(slide) = was_slide else {
            return Ok(None);
        };
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
            Ok(Some(ClickLevel::Force))
        } else if first_same {
            self.absorb_slide_into(ClickLevel::First);
            Ok(Some(ClickLevel::First))
        } else {
            self.release_slide_held(phys_down)?;
            Ok(None)
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
        if let Some(pending) = &mut self.pending_buttons {
            for key in &mut pending.keys {
                if key.owners.contains(&ClickLevel::Slide) {
                    key.owners.retain(|owner| *owner != ClickLevel::Slide);
                    if !key.owners.contains(&level) {
                        key.owners.push(level);
                    }
                }
            }
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
    /// fired (false = silent config OR the pulse was dropped: a bounded
    /// queue overflow / shutdown reports `Err`, so the release owes no
    /// up-pulse and down/up pairing stays intact).
    fn fire_haptic(&self, ev: &ClickEvent, pressure: u8) -> bool {
        if !clicks::should_emit_haptic(ev.haptic) {
            return false;
        }
        let intensity = ev.haptic.min(255) as u8;
        if let Err(e) = self.haptic.click_feedback(pressure, intensity, ev.quiet) {
            log::warn!("proxy: haptic send failed: {e:#}");
            return false;
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
    fn release_held(&mut self, phys_down: bool) -> Result<()> {
        self.pending_buttons = None;
        self.pending_force = None;
        let first = std::mem::take(&mut self.held_first);
        let slide = std::mem::take(&mut self.held_slide);
        let mut left_up = false;
        for &(ty, code) in &first {
            if phys_down && (ty, code) == (EV_KEY, BTN_LEFT) {
                continue; // physical still holds it; wire stays down
            }
            self.uinput.emit(ty, code, 0)?;
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
            self.uinput.emit(ty, code, 0)?;
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
            self.uinput.emit(ty, code, 0)?;
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
            self.uinput.emit(EV_KEY, BTN_LEFT, 0)?;
        }
        self.btn_left_down = phys_down && self.btn_left_down;
        Ok(())
    }

    /// Release slide-held keys, retaining codes another level still holds
    /// (no up-emit for those — the wire stays down until full release).
    /// Like [`ClickProxy::release_held`], `BTN_LEFT` stays down while the
    /// physical button holds it (`phys_down`): emitting 0-then-1 in the
    /// same frame would flutter a phantom extra click. The caller
    /// reconciles via [`ClickProxy::emit_btn_left`] right after.
    fn release_slide_held(&mut self, phys_down: bool) -> Result<()> {
        self.cancel_pending_buttons(ClickLevel::Slide);
        let mut dropped_left = false;
        for (ty, code) in self.held_slide.drain(..) {
            if self.held_first.contains(&(ty, code)) || self.held_force.contains(&(ty, code)) {
                continue;
            }
            if phys_down && (ty, code) == (EV_KEY, BTN_LEFT) {
                continue; // physical still holds it; wire stays down
            }
            self.uinput.emit(ty, code, 0)?;
            if (ty, code) == (EV_KEY, BTN_LEFT) {
                dropped_left = true;
            }
        }
        // Only a slide-only left actually went up on the wire here.
        if dropped_left {
            self.btn_left_down = false;
        }
        Ok(())
    }

    /// Release force-held keys, retaining codes the first level still holds
    /// (no up-emit for those — the wire stays down until full release).
    fn release_force_held(&mut self) -> Result<()> {
        self.cancel_pending_buttons(ClickLevel::Force);
        self.pending_force = None;
        let mut dropped_left = false;
        for (ty, code) in self.held_force.drain(..) {
            if self.held_first.contains(&(ty, code)) {
                continue;
            }
            self.uinput.emit(ty, code, 0)?;
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
        Ok(())
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
    fn cut_first_wire(&mut self) -> Result<()> {
        self.cancel_pending_buttons(ClickLevel::First);
        for (ty, code) in std::mem::take(&mut self.held_first) {
            self.uinput.emit(ty, code, 0)?;
            if (ty, code) == (EV_KEY, BTN_LEFT) {
                self.btn_left_down = false;
            }
        }
        Ok(())
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
    /// held → `[spoof.force]`, first held → `[spoof.click]`, otherwise
    /// `[spoof.slide]`) via [`clicks::spoof_contacts`]. All maps default
    /// to passthrough, so the frame below is the real one unless opted in.
    fn emit_touch_frame(&mut self, frame: &TouchFrame, cfg: &Config) -> Result<()> {
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
        // Single-touch position mirrors the first touching contact (slot 0);
        // skipped with no contacts (last position stands, as on hardware).
        let mut first_xy: Option<(i32, i32)> = None;
        for (slot, f) in fingers.iter().take(16).enumerate() {
            self.uinput.emit(EV_ABS, ABS_MT_SLOT, slot as i32)?;
            if clicks::touch_present(f) {
                self.uinput
                    .emit(EV_ABS, ABS_MT_TRACKING_ID, i32::from(f.id))?;
                let (sx, sy) = self.scaled_pos(f.id, f.x, f.y, cfg.pointer.speed);
                if first_xy.is_none() {
                    first_xy = Some((sx, sy));
                }
                self.uinput.emit(EV_ABS, ABS_MT_POSITION_X, sx)?;
                self.uinput.emit(EV_ABS, ABS_MT_POSITION_Y, sy)?;
                self.uinput
                    .emit(EV_ABS, ABS_MT_PRESSURE, TOUCH_PRESSURE_NORM)?;
                self.uinput
                    .emit(EV_ABS, ABS_MT_TOUCH_MAJOR, TOUCH_SIZE_NORM)?;
                self.uinput
                    .emit(EV_ABS, ABS_MT_TOUCH_MINOR, TOUCH_SIZE_NORM)?;
                self.uinput.emit(
                    EV_ABS,
                    ABS_MT_ORIENTATION,
                    i32::from(clicks::negate_orientation(f.orientation)),
                )?;
            } else {
                self.uinput.emit(EV_ABS, ABS_MT_TRACKING_ID, -1)?;
                self.last_pos.remove(&f.id);
            }
        }
        // Terminate slots that existed in earlier frames but not this one.
        if n <= self.max_slot {
            for slot in n..=self.max_slot {
                self.uinput.emit(EV_ABS, ABS_MT_SLOT, slot as i32)?;
                self.uinput.emit(EV_ABS, ABS_MT_TRACKING_ID, -1)?;
            }
        }
        self.max_slot = n.saturating_sub(1);
        if let Some((sx, sy)) = first_xy {
            self.uinput.emit(EV_ABS, ABS_X, sx)?;
            self.uinput.emit(EV_ABS, ABS_Y, sy)?;
        }
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
        )?;
        self.emit_tool_bits(present_count)?;
        Ok(())
    }

    /// `BTN_TOOL_*` classification row from the touching count (old-service
    /// parity: exactly one of FINGER/DOUBLE/TRIPLE/QUAD/QUIN is down for
    /// counts 1/2/3/4/5+; all up when nothing touches). Emitted every
    /// frame — libinput keys touchpad classification off these bits.
    fn emit_tool_bits(&mut self, present_count: usize) -> Result<()> {
        self.uinput
            .emit(EV_KEY, BTN_TOOL_FINGER, i32::from(present_count == 1))?;
        self.uinput
            .emit(EV_KEY, BTN_TOOL_DOUBLETAP, i32::from(present_count == 2))?;
        self.uinput
            .emit(EV_KEY, BTN_TOOL_TRIPLETAP, i32::from(present_count == 3))?;
        self.uinput
            .emit(EV_KEY, BTN_TOOL_QUADTAP, i32::from(present_count == 4))?;
        self.uinput
            .emit(EV_KEY, BTN_TOOL_QUINTTAP, i32::from(present_count >= 5))?;
        Ok(())
    }

    /// Speed-scaled position: `emitted + scaled(raw - last_raw)` per finger,
    /// with fractional-remainder carry. `speed` comes from the per-frame
    /// `&Config` (single source), never a frozen snapshot.
    fn scaled_pos(&mut self, id: u8, x: i32, y: i32, speed: u32) -> (i32, i32) {
        match self.last_pos.entry(id) {
            Entry::Vacant(vacant) => {
                // Clamp the first EMITTED report to the advertised bounds so
                // a stationary out-of-range contact cannot step on its second
                // frame; raw stays unclamped as the delta origin.
                let ex = x.clamp(POSITION_MIN_X, POSITION_MAX_X);
                let ey = y.clamp(POSITION_MIN_Y, POSITION_MAX_Y);
                vacant.insert(PointerAnchor {
                    raw: (x, y),
                    emitted: (ex, ey),
                    rem: (0, 0),
                });
                (ex, ey)
            }
            Entry::Occupied(mut occupied) => {
                let anchor = occupied.get_mut();
                let speed = i64::from(speed);
                let dx = i64::from(x) - i64::from(anchor.raw.0);
                let dy = i64::from(y) - i64::from(anchor.raw.1);
                let num_x = dx * speed + anchor.rem.0;
                let num_y = dy * speed + anchor.rem.1;
                // Truncating division (toward zero); the remainder carries
                // the sub-unit fraction so slow motion accumulates instead
                // of sticking.
                let step_x = num_x / 100;
                let step_y = num_y / 100;
                anchor.rem = (num_x % 100, num_y % 100);
                anchor.raw = (x, y);
                let nx = (i64::from(anchor.emitted.0) + step_x)
                    .clamp(i64::from(POSITION_MIN_X), i64::from(POSITION_MAX_X))
                    as i32;
                let ny = (i64::from(anchor.emitted.1) + step_y)
                    .clamp(i64::from(POSITION_MIN_Y), i64::from(POSITION_MAX_Y))
                    as i32;
                anchor.emitted = (nx, ny);
                (nx, ny)
            }
        }
    }
}

/// Speed-scaled pointer state per contact id: the last RAW device position
/// (delta source), the last EMITTED virtual position (delta base), and the
/// fractional remainder (leftover `delta * speed % 100` numerator,
/// `(-100, 100)`) so sub-unit motion at low speeds accumulates instead of
/// truncating to zero every frame. Raw and emitted are distinct: the emitted
/// cursor is an accumulation of scaled raw deltas, never a rescaling of the
/// raw anchor.
///
/// Bounds: emitted positions clamp to the virtual `ABS_X`/`ABS_Y`
/// (`-3678..=3934`, `-2478..=2587`, see [`uinput_abs_setup`]). Overshoot is
/// dropped (raw still advances, so reversal responds immediately).
/// Reanchoring: a fresh id anchors `raw` unclamped (delta origin) and
/// `emitted = clamp(raw), rem = 0`; lift
/// (non-touching or absent id), detach, pump loss, or output failure evicts
/// the anchor so reuse never inherits a stale base.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct PointerAnchor {
    raw: (i32, i32),
    emitted: (i32, i32),
    rem: (i64, i64),
}

/// Virtual axis bounds for the emitted pointer position (must match
/// [`uinput_abs_setup`]).
pub const POSITION_MIN_X: i32 = -3678;
pub const POSITION_MAX_X: i32 = 3934;
pub const POSITION_MIN_Y: i32 = -2478;
pub const POSITION_MAX_Y: i32 = 2587;

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
/// motion. `ABS_X`/`ABS_Y` mirror the first touching contact (slot 0)
/// with the same ranges/density — the physical nodes advertise them and
/// libinput requires them for pointer classification. Touch major/minor
/// mirror the old-service 0..1020 axes.
/// Deviations from old, documented: pressure range 0..253 (not 0..255,
/// the kernel `absmax`) and resolution fixed at the 1:1 density (old
/// varies it with speed; speed here applies once, in software).
pub fn uinput_abs_setup() -> Vec<(u16, i32, i32, i32)> {
    vec![
        (ABS_X, -3678, 3934, POSITION_RES_X),
        (ABS_Y, -2478, 2587, POSITION_RES_Y),
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
/// extras-only failure retires that node and degrades to pointer-only output
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

    fn emit(&self, ev_type: u16, code: u16, value: i32) -> Result<()> {
        match split_route(ev_type, code) {
            SplitRoute::Pointer => self.pointer.emit(ev_type, code, value),
            SplitRoute::Extras => {
                self.emit_extras(ev_type, code, value);
                Ok(())
            }
            SplitRoute::Both => {
                let pointer = self.pointer.emit(ev_type, code, value);
                self.emit_extras(ev_type, code, value);
                pointer
            }
        }
    }

    fn sync(&self) -> Result<()> {
        let pointer = self.pointer.sync();
        // Flush the surviving extras node even if pointer delivery failed.
        if self.extras.is_ready() {
            self.handle_extras_result(self.extras.sync());
        }
        pointer
    }

    fn destroy(&self) {
        self.pointer.destroy();
        self.extras.destroy();
    }
}

impl<P, E: UinputSink> DualSink<P, E> {
    fn emit_extras(&self, ev_type: u16, code: u16, value: i32) {
        if self.extras.is_ready() {
            self.handle_extras_result(self.extras.emit(ev_type, code, value));
        }
    }

    fn handle_extras_result(&self, result: Result<()>) {
        if let Err(error) = result {
            log::warn!("proxy: extras output failed; continuing pointer-only: {error:#}");
            self.extras.destroy();
        }
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
/// Emits are `input_event` writes once created; unavailable output is an error.
pub struct RealUinput {
    node: String,
    role: UinputRole,
    fd: Mutex<Option<std::fs::File>>,
    created: std::sync::atomic::AtomicBool,
    /// Staged `input_event` bytes, flushed as one `write(2)` per `sync()`
    /// (one frame = one syscall). Pre-create emits are dropped with an
    /// error, never buffered: without a node there is no consumer.
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

    /// `write_all` completes partial writes, retries EINTR, and treats zero
    /// writes as WriteZero. A failed frame is never replayed: some events may
    /// already have reached the consumer, so its virtual node must be retired.
    fn flush_locked(writer: &mut impl std::io::Write, buf: &[u8]) -> std::io::Result<()> {
        writer.write_all(buf)
    }

    /// Lock order is always fd → staging (matches [`UinputSink::emit`]);
    /// never the reverse.
    fn flush(&self) -> Result<()> {
        let mut guard = self.fd.lock().unwrap_or_else(|e| e.into_inner());
        let mut staging = self.staging.lock().unwrap_or_else(|e| e.into_inner());
        let result = match guard.as_mut() {
            Some(f) if self.is_created() => Self::flush_locked(f, &staging)
                .with_context(|| format!("uinput {:?} write failed", self.role)),
            _ => Err(anyhow::anyhow!("uinput {:?} output unavailable", self.role)),
        };
        staging.clear();
        if result.is_err() {
            Self::retire_locked(&mut guard);
            self.created
                .store(false, std::sync::atomic::Ordering::SeqCst);
        }
        result
    }

    fn retire_locked(guard: &mut Option<std::fs::File>) {
        if let Some(f) = guard.as_ref() {
            unsafe {
                libc::ioctl(f.as_raw_fd(), ui_dev_destroy() as libc::c_ulong);
            }
        }
        // Closing the fd also removes the virtual node if UI_DEV_DESTROY fails.
        *guard = None;
    }

    /// Destroy the virtual node (detach path). Flushes staged events
    /// first so no key-up is lost, then releases grab-side state via caller.
    pub fn destroy(&self) {
        let _ = self.flush();
        let mut guard = self.fd.lock().unwrap_or_else(|e| e.into_inner());
        Self::retire_locked(&mut guard);
        self.created
            .store(false, std::sync::atomic::Ordering::SeqCst);
        log::info!("proxy: uinput node {} destroyed", self.node);
    }

    #[cfg(test)]
    pub(crate) fn staged_len(&self) -> usize {
        self.staging.lock().unwrap_or_else(|e| e.into_inner()).len()
    }

    /// Test hook: fake a created virtual node without opening /dev/uinput.
    /// `destroy` stays safe (flush errors, nothing to retire).
    #[cfg(test)]
    pub(crate) fn test_mark_created(&self) {
        self.created
            .store(true, std::sync::atomic::Ordering::SeqCst);
    }
}

impl UinputSink for RealUinput {
    fn is_ready(&self) -> bool {
        self.is_created()
    }
    fn destroy(&self) {
        RealUinput::destroy(self);
    }
    fn emit(&self, ev_type: u16, code: u16, value: i32) -> Result<()> {
        log::debug!("uinput emit: type={ev_type} code={code} value={value}");
        let guard = self.fd.lock().unwrap_or_else(|e| e.into_inner());
        anyhow::ensure!(
            guard.is_some() && self.is_created(),
            "uinput {:?} output unavailable",
            self.role
        );
        self.staging
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .extend_from_slice(&encode_event(ev_type, code, value));
        Ok(())
    }

    /// Frame boundary: stage the trailing `SYN` and flush the whole
    /// frame in one write.
    fn sync(&self) -> Result<()> {
        self.emit(EV_SYN, SYN_REPORT, 0)?;
        self.flush()
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
    target: Mutex<HapticTarget>,
}

impl RealHaptic {
    pub fn new() -> Self {
        Self {
            target: Mutex::new(HapticTarget::empty()),
        }
    }

    pub fn set_node(&self, node: &str) {
        // Read routing first, then prepare (open) with no lock held, so a
        // blocked open never stalls a concurrent sender on this mutex.
        let bluetooth = self
            .target
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .bluetooth;
        let target = HapticTarget::prepare(node, bluetooth);
        *self.target.lock().unwrap_or_else(|e| e.into_inner()) = target;
    }

    pub fn set_bluetooth(&self, bt: bool) {
        self.target
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .bluetooth = bt;
    }

    /// Live routing snapshot (direct-send path only; the daemon path
    /// snapshots per pulse inside [`AsyncHaptic`]). The snapshot shares
    /// the pinned endpoint, so the send below runs on the same fd.
    fn current_target(&self) -> HapticTarget {
        self.target
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }
}

impl Default for RealHaptic {
    fn default() -> Self {
        Self::new()
    }
}

impl HapticSender for RealHaptic {
    fn click_feedback(&self, pressure: u8, intensity: u8, quiet: bool) -> Result<()> {
        let target = self.current_target();
        self.execute_down(&target, pressure, intensity, quiet)
    }
    fn release_feedback(&self, down_pressure: u8, quiet: bool) -> Result<()> {
        let target = self.current_target();
        self.execute_up(&target, down_pressure, quiet)
    }
    fn execute_down(
        &self,
        target: &HapticTarget,
        pressure: u8,
        intensity: u8,
        quiet: bool,
    ) -> Result<()> {
        if intensity == 0 {
            return Ok(());
        }
        let report = vibration_report(pressure, intensity, quiet);
        log::info!(
            "haptic feedback: pressure={pressure} intensity={intensity} quiet={quiet} (report 53, node={} bt={})",
            target.node,
            target.bluetooth,
        );
        if target.node.is_empty() {
            return Ok(());
        }
        self.send_report(target, &report);
        Ok(())
    }
    fn execute_up(&self, target: &HapticTarget, down_pressure: u8, quiet: bool) -> Result<()> {
        let report = vibration_up_report(down_pressure, quiet);
        log::info!(
            "haptic release: down_pressure={down_pressure} quiet={quiet} (report 53, node={} bt={})",
            target.node,
            target.bluetooth,
        );
        if target.node.is_empty() {
            return Ok(());
        }
        self.send_report(target, &report);
        Ok(())
    }
}

impl RealHaptic {
    fn send_report(&self, target: &HapticTarget, report: &[u8; 15]) {
        let payload: &[u8] = if target.bluetooth {
            &report[..]
        } else {
            &report[1..]
        };
        let node = target.node.as_str();
        // Both sends run on the SAME pinned fd from the session snapshot.
        // No pathname reopen: a replacement device reusing this pathname
        // after reconnect can never receive this (old-session) pulse.
        let Some(mut file) = target.try_clone_file() else {
            if !target.node.is_empty() {
                log::error!(
                    "haptic FAILED on {node} (no pinned endpoint); clicks stay silent until re-attach"
                );
            }
            return;
        };
        // Strict send: EPIPE/EIO (device NAK) must fall through to the
        // plain-write fallback below. The lenient init variant would
        // swallow the NAK as success → silent no-haptic with no fallback.
        let req = crate::init::hidiocsfeature_request(payload.len());
        let ret = unsafe {
            libc::ioctl(
                std::os::unix::io::AsRawFd::as_raw_fd(&file),
                req as libc::c_ulong,
                payload.as_ptr() as *const libc::c_void,
            )
        };
        if ret >= 0 {
            log::debug!("feature report {:02X?} sent to {node}", payload);
            return;
        }
        let e = std::io::Error::last_os_error();
        log::warn!("haptic feature report failed ({e:#}); trying plain write fallback");
        if let Err(e2) = plain_write_pinned(&mut file, payload) {
            log::error!(
                "haptic FAILED on {node} (feature: {e:#}; write: {e2:#}); clicks stay silent until re-attach"
            );
        }
    }
}

/// Plain-write fallback on the already-pinned fd (no pathname reopen).
/// Rewinds regular files so each pulse overwrites from the start (same
/// bytes the old reopen-per-send path produced); hidraw ignores the
/// offset and the rewind error is ignored there.
fn plain_write_pinned(file: &mut std::fs::File, bytes: &[u8]) -> Result<()> {
    use std::io::{Seek, SeekFrom, Write};
    let rewound = file.seek(SeekFrom::Start(0)).is_ok();
    file.write_all(bytes)
        .map_err(|e| anyhow::anyhow!("haptic write: {e}"))?;
    if rewound {
        let _ = file.set_len(bytes.len() as u64);
    }
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

/// Bound on queued actuator pulses ([`AsyncHaptic`]).
///
/// Worst-case drain at ~10 ms per firmware round-trip is ~160 ms; hover
/// chatter bursts (dozens of edges per drag) fit without blocking the
/// pump, while a wedged actuator cannot accumulate unbounded stale work.
pub const HAPTIC_QUEUE_CAP: usize = 16;

/// Bounded shutdown join for the haptic worker at `Drop` ([`AsyncHaptic`]).
/// Covers a full-queue drain under a slow actuator with margin; a
/// hung firmware send detaches instead of stalling teardown. Orderly
/// shutdown drains longer via [`AsyncHaptic::shutdown`].
const HAPTIC_SHUTDOWN_JOIN_MS: u64 = 500;

/// One queued actuator pulse: pulse data plus the session generation and
/// routing snapshot taken at enqueue time.
#[derive(Debug, Clone)]
struct QueuedPulse {
    generation: u64,
    target: HapticTarget,
    cmd: HapticCmd,
}

struct HapticState {
    pulses: VecDeque<QueuedPulse>,
    target: HapticTarget,
    closed: bool,
    dropped_overflow: u64,
    dropped_stale: u64,
}

struct HapticShared {
    state: Mutex<HapticState>,
    cvar: Condvar,
    generation: AtomicU64,
}

impl HapticShared {
    /// End the current session: purge queued pulses and advance the
    /// generation so pulses already popped by the worker are dropped at
    /// execution instead of running. Optionally installs the next
    /// session's target (new attach) or clears routing (detach, so a
    /// stray enqueue degrades to log-only). Returns pulses purged.
    /// Enqueue/retarget serialize here. Replaced endpoints and cleared
    /// pulses drop their last references here, but the blocking device
    /// close itself runs on the bounded process closer (see
    /// [`DisposalPool`]), so this never stalls the queue lock owner or
    /// the input pump, and teardown's bounded join does not include
    /// endpoint close.
    fn retire(&self, target: Option<HapticTarget>) -> u64 {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let stale = state.pulses.len() as u64;
        state.pulses.clear();
        if stale > 0 {
            state.dropped_stale += stale;
            log::info!("proxy: haptic retired {stale} stale pulse(s) on session change");
        }
        self.generation.fetch_add(1, Ordering::SeqCst);
        if let Some(t) = target {
            state.target = t;
        }
        stale
    }
}

/// Async actuator sender: the pump thread enqueues down/up pulses and
/// never waits for the firmware round-trip.
///
/// Why: every [`HapticSender`] call issues a blocking `HIDIOCSFEATURE`
/// (+ a fallback write on NAK) on the worker thread. On the pump thread
/// that steals frame time per click edge; during release/re-press
/// chatter (noisy pressure hovering at the release point mid-drag)
/// dozens of edges × ~10 ms round-trips stall motion in 150-800 ms
/// bursts. The worker drains a bounded FIFO instead, so input latency
/// never depends on actuator timing. Order is preserved (down/up pairs
/// stay paired); bytes and silent rules are unchanged.
///
/// Session scope: each pulse carries the session generation plus a
/// [`HapticTarget`] snapshot pinning the kernel endpoint (open fd, not
/// just a pathname).
///
/// [`ClickProxy::detach`], pump loss, pointer-output failure, and
/// [`AsyncHaptic::retarget`] retire the old session (purge + generation
/// bump). The worker drops stale pulses without touching hardware, so a
/// backlog can never fire on a newly selected node after reconnect, even
/// when the kernel reuses the same hidraw pathname.
///
/// A pulse already inside the firmware send at retire completes only
/// against its own pinned (old) fd; it is never reopened or redirected.
/// A same-session route correction ([`AsyncHaptic::update_route`])
/// re-points pending pulses to a newly pinned endpoint without retiring
/// them; an already in-flight pulse keeps the old endpoint.
///
/// Policy (input responsiveness wins over click feel in every case):
/// - Overflow: the pump never blocks — the incoming pulse is dropped and
///   reported as `Err`. A dropped down-pulse reports failure so the proxy
///   owes no up-pulse (down/up pairing preserved); a dropped up-pulse is
///   a missing release snap, never stuck state (pulses are momentary).
/// - Error: a panicking/broken actuator send is caught per pulse and the
///   worker continues; a pulse that cannot run is dropped with a log.
///   The pump never degrades to synchronous sends.
/// - Shutdown: the session is retired first (queued pulses are dropped,
///   never drained against the dead target), then close is signaled and
///   `Drop` joins up to [`HAPTIC_SHUTDOWN_JOIN_MS`]; past that the thread
///   detaches and teardown returns instead of hanging.
pub struct AsyncHaptic<H> {
    shared: std::sync::Arc<HapticShared>,
    worker: Option<std::thread::JoinHandle<()>>,
    _sender: std::marker::PhantomData<H>,
}

impl<H> AsyncHaptic<H>
where
    H: HapticSender + Send + Sync + 'static,
{
    pub fn new(inner: H) -> Self {
        let inner = std::sync::Arc::new(inner);
        let shared = std::sync::Arc::new(HapticShared {
            state: Mutex::new(HapticState {
                pulses: VecDeque::new(),
                target: HapticTarget::empty(),
                closed: false,
                dropped_overflow: 0,
                dropped_stale: 0,
            }),
            cvar: Condvar::new(),
            generation: AtomicU64::new(0),
        });
        let worker_shared = std::sync::Arc::clone(&shared);
        let worker_inner = std::sync::Arc::clone(&inner);
        let worker = std::thread::Builder::new()
            .name("ltd-haptic".into())
            .spawn(move || haptic_worker(worker_shared, worker_inner))
            .expect("haptic worker thread spawn");
        Self {
            shared,
            worker: Some(worker),
            _sender: std::marker::PhantomData,
        }
    }

    fn enqueue(&self, cmd: HapticCmd) -> Result<()> {
        let mut state = self.shared.state.lock().unwrap_or_else(|e| e.into_inner());
        if state.closed {
            anyhow::bail!("haptic worker shutting down");
        }
        if state.pulses.len() >= HAPTIC_QUEUE_CAP {
            state.dropped_overflow += 1;
            let total = state.dropped_overflow;
            anyhow::bail!(
                "haptic queue full ({HAPTIC_QUEUE_CAP}); dropping pulse (overflow={total})"
            );
        }
        // Generation + snapshot read under the queue lock: a concurrent
        // retire either purged first (this pulse tags the new session) or
        // bumped after (this pulse is purged / dropped stale). No mix.
        let generation = self.shared.generation.load(Ordering::SeqCst);
        let target = state.target.clone();
        state.pulses.push_back(QueuedPulse {
            generation,
            target,
            cmd,
        });
        self.shared.cvar.notify_one();
        Ok(())
    }

    /// New session: retire queued work and pin the actuator target.
    /// Convenience wrapper that prepares (opens) outside the queue lock;
    /// callers holding the live proxy mutex must use
    /// [`AsyncHaptic::retarget_prepared`] with a target prepared before
    /// locking instead, so the open never stalls input processing.
    /// The retire then commits the new endpoint together with the
    /// generation bump. Final disposal of the replaced endpoint runs on
    /// the bounded process closer, never under the queue lock or on the
    /// caller.
    pub fn retarget(&self, node: &str, bluetooth: bool) {
        let target = HapticTarget::prepare(node, bluetooth);
        self.shared.retire(Some(target));
    }

    /// New session commit for a target prepared outside all live locks.
    /// No filesystem I/O under any mutex; replaced endpoints close on the
    /// bounded process closer.
    pub fn retarget_prepared(&self, target: HapticTarget) {
        self.shared.retire(Some(target));
    }

    /// Same-session route correction (late actuator sibling): re-point
    /// pending pulses at the corrected node without retiring them. Pins
    /// a new endpoint fd for the corrected node; pulses already popped
    /// by the worker keep the old endpoint. The generation is unchanged.
    /// Convenience wrapper; proxy-mutex holders must prepare outside and
    /// use [`AsyncHaptic::update_route_prepared`] instead.
    pub fn update_route(&self, node: &str) {
        let (generation, bluetooth, current) = {
            let state = self.shared.state.lock().unwrap_or_else(|e| e.into_inner());
            if state.target.node == node {
                return;
            }
            (
                self.shared.generation.load(Ordering::SeqCst),
                state.target.bluetooth,
                state.target.node.clone(),
            )
        };
        // Open outside the queue lock; commit below only if the session
        // has not retired meanwhile (otherwise the correction would clobber
        // the new session's endpoint).
        let corrected = HapticTarget::prepare(node, bluetooth);
        let mut state = self.shared.state.lock().unwrap_or_else(|e| e.into_inner());
        if self.shared.generation.load(Ordering::SeqCst) != generation {
            return;
        }
        if state.target.node != current {
            return;
        }
        state.target = corrected.clone();
        for pulse in state.pulses.iter_mut() {
            if pulse.generation == generation {
                pulse.target = corrected.clone();
            }
        }
    }

    /// Same-session route commit for a target prepared outside all live
    /// locks. Generation-fenced: returns false (discarding the prepared
    /// endpoint, which then closes on a background thread) when the
    /// session retired between prepare and commit, or when the route is
    /// already correct. No filesystem I/O under any mutex.
    pub fn update_route_prepared(&self, prepared: HapticTarget, baseline_generation: u64) -> bool {
        let mut state = self.shared.state.lock().unwrap_or_else(|e| e.into_inner());
        if self.shared.generation.load(Ordering::SeqCst) != baseline_generation {
            return false;
        }
        if state.target.node == prepared.node {
            return false;
        }
        state.target = prepared.clone();
        for pulse in state.pulses.iter_mut() {
            if pulse.generation == baseline_generation {
                pulse.target = prepared.clone();
            }
        }
        true
    }

    /// Current session generation (atomic load, no locks, no I/O).
    /// Daemon captures this before preparing a route correction outside
    /// the proxy mutex, then fences the commit with it.
    pub fn generation(&self) -> u64 {
        self.shared.generation.load(Ordering::SeqCst)
    }

    /// Current routing (node + bluetooth) under a brief queue lock; no I/O.
    /// Daemon reads this before preparing outside the proxy mutex.
    pub fn current_routing(&self) -> (String, bool) {
        let state = self.shared.state.lock().unwrap_or_else(|e| e.into_inner());
        (state.target.node.clone(), state.target.bluetooth)
    }

    #[cfg(test)]
    pub fn pending(&self) -> usize {
        self.shared
            .state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .pulses
            .len()
    }

    #[cfg(test)]
    pub fn dropped_overflow(&self) -> u64 {
        self.shared
            .state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .dropped_overflow
    }

    #[cfg(test)]
    pub fn dropped_stale(&self) -> u64 {
        self.shared
            .state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .dropped_stale
    }
}

/// Worker: pop in FIFO order, drop stale generations without touching
/// hardware, execute the rest against their snapshots. A panicking send
/// is caught per pulse so one bad report cannot kill the worker; close
/// exits once the queue is empty (teardown retires first, so close-time
/// drain only ever sees pulses enqueued after the retire — normally
/// none), then the thread exits.
fn haptic_worker<H>(shared: std::sync::Arc<HapticShared>, inner: std::sync::Arc<H>)
where
    H: HapticSender + Send + Sync + 'static,
{
    loop {
        let pulse = {
            let mut state = shared.state.lock().unwrap_or_else(|e| e.into_inner());
            while state.pulses.is_empty() && !state.closed {
                state = shared.cvar.wait(state).unwrap_or_else(|e| e.into_inner());
            }
            match state.pulses.pop_front() {
                Some(pulse) => pulse,
                None => {
                    if state.closed {
                        break;
                    }
                    continue;
                }
            }
        };
        if pulse.generation != shared.generation.load(Ordering::SeqCst) {
            shared
                .state
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .dropped_stale += 1;
            continue;
        }
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| match pulse.cmd {
            HapticCmd::Down {
                pressure,
                intensity,
                quiet,
            } => inner.execute_down(&pulse.target, pressure, intensity, quiet),
            HapticCmd::Up {
                down_pressure,
                quiet,
            } => inner.execute_up(&pulse.target, down_pressure, quiet),
        }));
        match outcome {
            Ok(Ok(())) => {}
            Ok(Err(e)) => log::warn!("proxy: async haptic pulse failed: {e:#}"),
            Err(_) => log::error!("proxy: async haptic pulse panicked; worker continues"),
        }
    }
    log::debug!("proxy: haptic worker drained, exiting");
}

impl<H> AsyncHaptic<H> {
    /// Bounded drain for orderly shutdown: disconnect the queue so the
    /// worker finishes the trailing pulses, then join with `timeout`.
    /// A stalled worker is detached (with a loud log) rather than hanging
    /// shutdown; a panicked worker is reported. Idempotent: safe to call
    /// after a previous shutdown or before drop.
    pub fn shutdown(&mut self, timeout: std::time::Duration) {
        // Disconnect first so the worker drains the queued trailing pulses
        // and exits once the queue is empty.
        {
            let mut state = self.shared.state.lock().unwrap_or_else(|e| e.into_inner());
            state.closed = true;
            self.shared.cvar.notify_all();
        }
        let Some(worker) = self.worker.take() else {
            return;
        };
        match crate::pump::join_bounded(worker, timeout) {
            crate::pump::WorkerJoin::Completed(()) => {}
            crate::pump::WorkerJoin::Panicked(msg) => {
                log::error!("proxy: haptic worker panicked: {msg}");
            }
            crate::pump::WorkerJoin::TimedOut => {
                log::error!("proxy: haptic worker did not drain within {timeout:?}; detaching");
            }
        }
    }
}

/// Bound for the actuator drain at shutdown/drop: queued pulses are click
/// edges only, each a ~10 ms firmware round-trip, so two seconds is
/// generous; a wedged actuator must never hang process exit.
pub const HAPTIC_SHUTDOWN_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(2);

impl<H> Drop for AsyncHaptic<H> {
    fn drop(&mut self) {
        // Retire first: queued pulses belong to a session that no longer
        // exists, so purge them (and invalidate the generation) instead of
        // draining them against the old endpoint during teardown. Then
        // close and join bounded: a wedged actuator detaches with a log
        // instead of hanging process exit forever. Drop-scale budget (see
        // HAPTIC_SHUTDOWN_JOIN_MS); orderly shutdown drains longer via
        // `shutdown`.
        self.shared.retire(Some(HapticTarget::empty()));
        self.shutdown(std::time::Duration::from_millis(HAPTIC_SHUTDOWN_JOIN_MS));
    }
}

impl<H> HapticSender for AsyncHaptic<H>
where
    H: HapticSender + Send + Sync + 'static,
{
    fn click_feedback(&self, pressure: u8, intensity: u8, quiet: bool) -> Result<()> {
        if intensity == 0 {
            return Ok(());
        }
        let cmd = HapticCmd::Down {
            pressure,
            intensity,
            quiet,
        };
        if let Err(e) = self.enqueue(cmd) {
            // Overflow: drop the pulse, never block the pump.
            // `Err` tells the proxy the down never fired, so no up-pulse
            // is owed and pairs stay paired.
            log::warn!("proxy: haptic down dropped ({e:#}); input path unaffected");
            return Err(e);
        }
        Ok(())
    }
    fn release_feedback(&self, down_pressure: u8, quiet: bool) -> Result<()> {
        let cmd = HapticCmd::Up {
            down_pressure,
            quiet,
        };
        if let Err(e) = self.enqueue(cmd) {
            // A dropped up-pulse is a missing release snap (best effort),
            // never stuck state: actuator pulses are momentary.
            log::warn!("proxy: haptic up dropped ({e:#}); input path unaffected");
            return Err(e);
        }
        Ok(())
    }
    fn retire_session(&self) {
        self.shared.retire(Some(HapticTarget::empty()));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::apple_parse::{CapabilityTier, FingerContact, PressureUnits, TouchFrame};
    use crate::config::Config;
    use crate::daemon::PumpManager;
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
        fn emit(&self, ev_type: u16, code: u16, value: i32) -> Result<()> {
            self.events.lock().unwrap().push((ev_type, code, value));
            Ok(())
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
        fn emit(&self, ev_type: u16, code: u16, value: i32) -> Result<()> {
            self.0.emit(ev_type, code, value)
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
    fn single_touch_xy_mirrors_slot_zero_and_skips_empty_frames() {
        // Owner sysfs: physical nodes advertise ABS_X/Y; libinput requires
        // them for pointer classification. Each touching frame carries
        // ABS_X/Y equal to slot 0's MT position; empty frames carry none
        // (last position stands, as on hardware).
        let sinks = TestSinks::new();
        let haptic = Arc::new(FakeHaptic::new());
        let frames = vec![
            Ok(Some(frame(vec![finger(1, 200, 100, 60)], 0))),
            Ok(Some(frame(vec![finger(1, 200, 110, 70)], 0))),
            Ok(Some(frame(vec![], 0))),
        ];
        let mut px = drive_ready_proxy(&sinks, &haptic, frames);
        let cfg = Config::default();
        let t0 = std::time::Instant::now();
        let m0 = sinks.pointer.events.lock().unwrap().len();
        px.handle_frame_at(&cfg, spaced_at(t0, 0)).unwrap();
        let f0: Vec<(u16, u16, i32)> = sinks.pointer.events.lock().unwrap()[m0..].to_vec();
        let mt_x = f0
            .iter()
            .find(|&&(t, c, _)| t == EV_ABS && c == ABS_MT_POSITION_X)
            .map(|&(_, _, v)| v)
            .expect("slot has MT X");
        let mt_y = f0
            .iter()
            .find(|&&(t, c, _)| t == EV_ABS && c == ABS_MT_POSITION_Y)
            .map(|&(_, _, v)| v)
            .expect("slot has MT Y");
        assert!(
            f0.contains(&(EV_ABS, ABS_X, mt_x)),
            "ABS_X mirrors slot 0: {f0:?}"
        );
        assert!(
            f0.contains(&(EV_ABS, ABS_Y, mt_y)),
            "ABS_Y mirrors slot 0: {f0:?}"
        );
        // Motion frame: X/Y track slot 0 again (changed values).
        let m1 = sinks.pointer.events.lock().unwrap().len();
        px.handle_frame_at(&cfg, spaced_at(t0, 1)).unwrap();
        let f1: Vec<(u16, u16, i32)> = sinks.pointer.events.lock().unwrap()[m1..].to_vec();
        assert!(
            f1.iter().any(|&(t, c, _)| t == EV_ABS && c == ABS_X),
            "motion frame carries ABS_X: {f1:?}"
        );
        assert!(
            f1.iter().any(|&(t, c, _)| t == EV_ABS && c == ABS_Y),
            "motion frame carries ABS_Y: {f1:?}"
        );
        // Empty frame: no X/Y (and no slots either).
        let m2 = sinks.pointer.events.lock().unwrap().len();
        px.handle_frame_at(&cfg, spaced_at(t0, 2)).unwrap();
        let f2: Vec<(u16, u16, i32)> = sinks.pointer.events.lock().unwrap()[m2..].to_vec();
        assert!(
            !f2.iter()
                .any(|&(t, c, _)| t == EV_ABS && (c == ABS_X || c == ABS_Y)),
            "empty frame carries no X/Y: {f2:?}"
        );
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
        // click_1 = left → BTN_LEFT down; haptic fired (default 2, quiet).
        assert!(sinks.pointer.has(EV_KEY, BTN_LEFT, 1));
        assert_eq!(haptic.calls.lock().unwrap().as_slice(), &[(30, 2, true)]);
    }

    #[test]
    fn silent_haptic_sends_no_feedback() {
        let sinks = TestSinks::new();
        let haptic = Arc::new(FakeHaptic::new());
        let frames = vec![Ok(Some(frame(vec![finger(1, 200, 0, 0)], 0)))];
        let mut px = drive_ready_proxy(&sinks, &haptic, frames);
        let cfg = Config {
            haptics: crate::config::HapticsConfig {
                haptic: 0,
                ..Config::default().haptics
            },
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
            click: crate::config::ClickConfig {
                click_1: "none".into(),
                ..Config::default().click
            },
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
            pointer: crate::config::PointerConfig { speed: 200 },
            ..Config::default()
        };
        px.handle_frame(&cfg).unwrap();
        px.handle_frame(&cfg).unwrap();
        // Second frame: dx=100 @200% → emitted x=200.
        assert!(sinks.pointer.has(EV_ABS, ABS_MT_POSITION_X, 200));
    }

    /// Emitted MT X values for one handled frame, in slot order.
    fn frame_mt_x(sinks: &TestSinks, before: usize) -> Vec<i32> {
        sinks.pointer.events.lock().unwrap()[before..]
            .iter()
            .filter(|&&(t, c, _)| t == EV_ABS && c == ABS_MT_POSITION_X)
            .map(|&(_, _, v)| v)
            .collect()
    }

    fn drive_speed_frames(raw_x: &[i32], speed: u32) -> Vec<i32> {
        let sinks = TestSinks::new();
        let haptic = Arc::new(FakeHaptic::new());
        let frames = raw_x
            .iter()
            .map(|&x| Ok(Some(frame(vec![finger(1, 200, x, 0)], 0))))
            .collect();
        let mut px = drive_ready_proxy(&sinks, &haptic, frames);
        let cfg = Config {
            pointer: crate::config::PointerConfig { speed },
            ..Config::default()
        };
        let t0 = std::time::Instant::now();
        let mut out = Vec::with_capacity(raw_x.len());
        for (i, _) in raw_x.iter().enumerate() {
            let before = sinks.pointer.events.lock().unwrap().len();
            px.handle_frame_at(&cfg, spaced_at(t0, i as u64)).unwrap();
            let xs = frame_mt_x(&sinks, before);
            assert_eq!(xs.len(), 1, "one touching contact per frame");
            out.push(xs[0]);
        }
        out
    }

    /// Emitted MT Y values for one handled frame, in slot order.
    fn frame_mt_y(sinks: &TestSinks, before: usize) -> Vec<i32> {
        sinks.pointer.events.lock().unwrap()[before..]
            .iter()
            .filter(|&&(t, c, _)| t == EV_ABS && c == ABS_MT_POSITION_Y)
            .map(|&(_, _, v)| v)
            .collect()
    }

    fn drive_speed_points(raw: &[(i32, i32)], speed: u32) -> Vec<(i32, i32)> {
        let sinks = TestSinks::new();
        let haptic = Arc::new(FakeHaptic::new());
        let frames = raw
            .iter()
            .map(|&(x, y)| Ok(Some(frame(vec![finger(1, 200, x, y)], 0))))
            .collect();
        let mut px = drive_ready_proxy(&sinks, &haptic, frames);
        let cfg = Config {
            pointer: crate::config::PointerConfig { speed },
            ..Config::default()
        };
        let t0 = std::time::Instant::now();
        let mut out = Vec::with_capacity(raw.len());
        for (i, _) in raw.iter().enumerate() {
            let before = sinks.pointer.events.lock().unwrap().len();
            px.handle_frame_at(&cfg, spaced_at(t0, i as u64)).unwrap();
            let xs = frame_mt_x(&sinks, before);
            let ys = frame_mt_y(&sinks, before);
            assert_eq!(xs.len(), 1, "one touching contact per frame");
            assert_eq!(ys.len(), 1, "one touching contact per frame");
            out.push((xs[0], ys[0]));
        }
        out
    }

    #[test]
    fn speed_accumulates_emitted_position_across_frames() {
        // Finding #6 repro: raw [0,10,20,30,30] @200% must emit
        // [0,20,40,60,60] (old code emitted [0,20,30,40,30]).
        assert_eq!(
            drive_speed_frames(&[0, 10, 20, 30, 30], 200),
            vec![0, 20, 40, 60, 60]
        );
    }

    #[test]
    fn speed_stop_holds_emitted_position() {
        // Final stopped frame (dx=0) holds; no backward step toward raw.
        let emitted = drive_speed_frames(&[0, 10, 20, 20, 20], 200);
        assert_eq!(emitted, vec![0, 20, 40, 40, 40]);
    }

    #[test]
    fn speed_reversal_is_continuous() {
        // Out @200% then back to origin: [0,10,0] -> [0,20,0].
        assert_eq!(drive_speed_frames(&[0, 10, 0], 200), vec![0, 20, 0]);
    }

    #[test]
    fn speed_100_is_exact_passthrough() {
        // Default speed must stay bit-exact with raw, including negatives.
        let raw = vec![5, 15, 7, 100, -50, -50];
        assert_eq!(drive_speed_frames(&raw, 100), raw);
    }

    #[test]
    fn speed_low_speed_accumulates_fraction() {
        // +1 raw steps @50%: per-frame truncation would stick at 0 forever;
        // remainder carry emits every second step.
        assert_eq!(
            drive_speed_frames(&[0, 1, 2, 3, 4], 50),
            vec![0, 0, 1, 1, 2]
        );
    }

    #[test]
    fn speed_lift_reanchors_fresh_touch() {
        let sinks = TestSinks::new();
        let haptic = Arc::new(FakeHaptic::new());
        let frames = vec![
            Ok(Some(frame(vec![finger(1, 200, 0, 0)], 0))),
            Ok(Some(frame(vec![finger(1, 200, 10, 0)], 0))),
            Ok(Some(frame(vec![], 0))), // lift: evicts the anchor
            Ok(Some(frame(vec![finger(1, 200, 500, 0)], 0))), // reuse: fresh anchor
            Ok(Some(frame(vec![finger(1, 200, 510, 0)], 0))),
        ];
        let mut px = drive_ready_proxy(&sinks, &haptic, frames);
        let cfg = Config {
            pointer: crate::config::PointerConfig { speed: 200 },
            ..Config::default()
        };
        let t0 = std::time::Instant::now();
        let mut out = Vec::new();
        for i in 0..5 {
            let before = sinks.pointer.events.lock().unwrap().len();
            px.handle_frame_at(&cfg, spaced_at(t0, i)).unwrap();
            out.push(frame_mt_x(&sinks, before));
        }
        assert_eq!(out[0], vec![0]);
        assert_eq!(out[1], vec![20]);
        assert!(out[2].is_empty(), "lift frame emits no MT X");
        // Retouch anchors at raw (no jump from the old emitted 20).
        assert_eq!(out[3], vec![500]);
        assert_eq!(out[4], vec![520]);
    }

    #[test]
    fn speed_tracks_contacts_independently() {
        let sinks = TestSinks::new();
        let haptic = Arc::new(FakeHaptic::new());
        let frames = vec![
            Ok(Some(frame(
                vec![finger(1, 200, 0, 0), finger(2, 200, 100, 0)],
                0,
            ))),
            Ok(Some(frame(
                vec![finger(1, 200, 10, 0), finger(2, 200, 110, 0)],
                0,
            ))),
            Ok(Some(frame(
                vec![finger(1, 200, 20, 0), finger(2, 200, 100, 0)],
                0,
            ))),
        ];
        let mut px = drive_ready_proxy(&sinks, &haptic, frames);
        let cfg = Config {
            pointer: crate::config::PointerConfig { speed: 200 },
            ..Config::default()
        };
        let t0 = std::time::Instant::now();
        let mut out = Vec::new();
        for i in 0..3u64 {
            let before = sinks.pointer.events.lock().unwrap().len();
            px.handle_frame_at(&cfg, spaced_at(t0, i)).unwrap();
            out.push(frame_mt_x(&sinks, before));
        }
        assert_eq!(out[0], vec![0, 100]);
        assert_eq!(out[1], vec![20, 120]);
        // Contact 1 advances, contact 2 reverses to its origin.
        assert_eq!(out[2], vec![40, 100]);
    }

    #[test]
    fn speed_clamps_at_axis_bounds_and_reverses_cleanly() {
        // Raw 3900 -> 3920 @200% wants 3940; virtual X caps at 3934.
        // Hold stays pinned; reversal responds immediately (no stick).
        let emitted = drive_speed_frames(&[3900, 3920, 3920, 3900], 200);
        assert_eq!(emitted, vec![3900, POSITION_MAX_X, POSITION_MAX_X, 3894]);
        // Lower bound pins symmetrically.
        let emitted = drive_speed_frames(&[-3600, -3650, -3650, -3600], 200);
        assert_eq!(emitted, vec![-3600, POSITION_MIN_X, POSITION_MIN_X, -3578]);
    }

    #[test]
    fn speed_fresh_out_of_range_anchor_clamps_and_holds() {
        // Fresh out-of-range contacts must emit the clamped bound on first
        // contact and stay put while raw is stationary (zero delta = no move).
        assert_eq!(
            drive_speed_frames(&[4000, 4000, 4000], 100),
            vec![POSITION_MAX_X, POSITION_MAX_X, POSITION_MAX_X]
        );
        assert_eq!(
            drive_speed_frames(&[-4000, -4000, -4000], 100),
            vec![POSITION_MIN_X, POSITION_MIN_X, POSITION_MIN_X]
        );
        assert_eq!(
            drive_speed_points(&[(0, 3000), (0, 3000), (0, 3000)], 100),
            vec![
                (0, POSITION_MAX_Y),
                (0, POSITION_MAX_Y),
                (0, POSITION_MAX_Y)
            ]
        );
        assert_eq!(
            drive_speed_points(&[(0, -3000), (0, -3000), (0, -3000)], 100),
            vec![
                (0, POSITION_MIN_Y),
                (0, POSITION_MIN_Y),
                (0, POSITION_MIN_Y)
            ]
        );
    }

    #[test]
    fn speed_fresh_out_of_range_first_move_uses_raw_delta() {
        // Raw stays the delta origin: inward motion from a clamped anchor
        // steps off the bound; outward overshoot stays pinned.
        assert_eq!(
            drive_speed_frames(&[4000, 4000, 3990], 100),
            vec![POSITION_MAX_X, POSITION_MAX_X, POSITION_MAX_X - 10]
        );
        assert_eq!(
            drive_speed_frames(&[-4000, -4000, -3990], 100),
            vec![POSITION_MIN_X, POSITION_MIN_X, POSITION_MIN_X + 10]
        );
        assert_eq!(
            drive_speed_frames(&[4000, 4000, 4010], 100),
            vec![POSITION_MAX_X, POSITION_MAX_X, POSITION_MAX_X]
        );
        // Y mirrors X, and scaling still applies to the raw delta.
        assert_eq!(
            drive_speed_points(&[(0, 3000), (0, 3000), (0, 2990)], 100),
            vec![
                (0, POSITION_MAX_Y),
                (0, POSITION_MAX_Y),
                (0, POSITION_MAX_Y - 10)
            ]
        );
        assert_eq!(
            drive_speed_points(&[(0, -3000), (0, -3000), (0, -2990)], 100),
            vec![
                (0, POSITION_MIN_Y),
                (0, POSITION_MIN_Y),
                (0, POSITION_MIN_Y + 10)
            ]
        );
        assert_eq!(
            drive_speed_frames(&[4000, 4000, 3990], 200),
            vec![POSITION_MAX_X, POSITION_MAX_X, POSITION_MAX_X - 20]
        );
    }

    #[test]
    fn position_bounds_match_virtual_axes() {
        let get = |code: u16| {
            uinput_abs_setup()
                .iter()
                .find(|(c, _, _, _)| *c == code)
                .copied()
                .unwrap()
        };
        assert_eq!(
            (get(ABS_X).1, get(ABS_X).2),
            (POSITION_MIN_X, POSITION_MAX_X)
        );
        assert_eq!(
            (get(ABS_Y).1, get(ABS_Y).2),
            (POSITION_MIN_Y, POSITION_MAX_Y)
        );
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
        // Single-touch position mirrors the MT ranges/density (owner sysfs:
        // the physical nodes advertise ABS_X/Y; libinput requires them for
        // pointer classification).
        assert_eq!(get(ABS_X), &(ABS_X, -3678, 3934, POSITION_RES_X));
        assert_eq!(get(ABS_Y), &(ABS_Y, -2478, 2587, POSITION_RES_Y));
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
        // Every code the shared token table can produce must be advertisable
        // on exactly one node: pointer-routed buttons on the pointer node,
        // everything else on extras. Candidate universe is the shared token
        // universe plus printable singles and f0..=f99 (unknowns must stay
        // unmapped), so a newly mapped token breaks here until its node
        // covers it.
        let mut candidates: Vec<String> = crate::action_tokens::all_known_tokens();
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
        fn emit(&self, ty: u16, code: u16, val: i32) -> Result<()> {
            self.inner.emit(ty, code, val)
        }
    }

    #[test]
    fn bt_ready_only_suppresses_with_a_ready_proxy_and_releases_on_detach() {
        // Run the daemon's real BT Ready fan-out into a fake proxy. A gate,
        // source/output readiness or grab failure must leave native input available;
        // a successful session suppresses only through its owned evdev grab.
        for (enhanced, pump_ready, sink_ready, grab_fails, should_grab) in [
            (false, true, true, false, false),
            (true, false, true, false, false),
            (true, true, false, false, false),
            (true, true, true, true, false),
            (true, true, true, false, true),
        ] {
            let grab = Arc::new(if grab_fails {
                FakeGrab::failing()
            } else {
                FakeGrab::ok()
            });
            let mut px = ClickProxy::new(
                settings(enhanced),
                SharedGrab(Arc::clone(&grab)),
                GatedUinput::new(sink_ready),
                FakeHaptic::new(),
                ScriptSource::new(vec![Ok(Some(frame(vec![finger(1, 10, 100, 200)], 0)))]),
            );
            px.set_pump_ready(pump_ready);
            let px = Arc::new(Mutex::new(px));
            let hub = crate::init::DeviceReadyHub::new();
            let capture = Arc::clone(&px);
            hub.subscribe(move |ev| match ev.kind {
                ReadyKind::Ready => capture.lock().unwrap().attach(ev),
                ReadyKind::Detached => capture.lock().unwrap().detach(ev),
            });
            let dev = crate::discovery::DiscoveredDevice {
                devnode: "/dev/fake-bt-trackpad".into(),
                vendor_id: 0x004c,
                product_id: 0x0324,
                transport: Transport::Bluetooth,
            };
            crate::daemon::init_node(&dev, 110, &hub).unwrap();
            assert_eq!(grab.is_grabbed(), should_grab);
            {
                let mut px = px.lock().unwrap();
                assert_eq!(px.is_attached(), should_grab);
                assert_eq!(px.is_passthrough(), !should_grab);
                px.handle_frame(&Config::default()).unwrap();
                assert_eq!(
                    px.uinput().inner.has(EV_ABS, ABS_MT_TRACKING_ID, 1),
                    should_grab
                );
                assert_eq!(px.uinput().inner.has(EV_KEY, BTN_TOUCH, 1), should_grab);
            }
            // A hardware-only helper must also leave an active grab alone.
            let node = tempfile::NamedTempFile::new().unwrap();
            assert_eq!(
                crate::resume::resume_bt_node(node.path().to_str().unwrap()),
                crate::resume::ResumeAction::UpToDate
            );
            assert_eq!(grab.is_grabbed(), should_grab);
            hub.emit(&detached(&dev.devnode, Transport::Bluetooth));
            assert!(!grab.is_grabbed());
            assert!(px.lock().unwrap().is_passthrough());
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
        assert_eq!(split_route(EV_ABS, ABS_X), SplitRoute::Pointer);
        assert_eq!(split_route(EV_ABS, ABS_Y), SplitRoute::Pointer);
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
        sink.emit(EV_ABS, ABS_MT_SLOT, 0).unwrap();
        sink.emit(EV_ABS, ABS_MT_TRACKING_ID, 1).unwrap();
        sink.emit(EV_KEY, BTN_LEFT, 1).unwrap();
        sink.emit(EV_KEY, 46, 1).unwrap(); // KEY_C
        sink.emit(EV_KEY, 29, 1).unwrap(); // KEY_LEFTCTRL
        sink.sync().unwrap(); // trailing SYN lands on BOTH nodes (each framed)
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
        assert!(u.emit(EV_KEY, BTN_LEFT, 1).is_err());
        assert!(u.emit(EV_ABS, ABS_MT_SLOT, 0).is_err());
        assert!(u.sync().is_err());
        assert_eq!(u.staged_len(), 0);
    }

    // Software-only output failure fixtures. No uinput or evdev device is opened.
    #[derive(Clone)]
    struct FaultUinput {
        inner: Arc<FakeUinput>,
        // 0 = healthy, 1 = fail emit, 2 = fail sync.
        failure: Arc<std::sync::atomic::AtomicU8>,
    }

    impl FaultUinput {
        fn new() -> Self {
            Self {
                inner: Arc::new(FakeUinput::new()),
                failure: Arc::new(std::sync::atomic::AtomicU8::new(0)),
            }
        }

        fn fail(&self, failure: u8) {
            self.failure
                .store(failure, std::sync::atomic::Ordering::SeqCst);
        }
    }

    impl UinputSink for FaultUinput {
        fn is_ready(&self) -> bool {
            !self.inner.was_destroyed()
        }

        fn emit(&self, ty: u16, code: u16, value: i32) -> Result<()> {
            anyhow::ensure!(self.is_ready(), "fake output retired");
            anyhow::ensure!(
                self.failure.load(std::sync::atomic::Ordering::SeqCst) != 1,
                "fake emit failure"
            );
            self.inner.emit(ty, code, value)
        }

        fn sync(&self) -> Result<()> {
            anyhow::ensure!(
                self.failure.load(std::sync::atomic::Ordering::SeqCst) != 2,
                "fake sync failure"
            );
            self.emit(EV_SYN, SYN_REPORT, 0)
        }

        fn destroy(&self) {
            self.inner.destroy();
        }
    }

    fn output_failure_config() -> Config {
        Config {
            click: crate::config::ClickConfig {
                click_1: "ctrl+left".into(),
                ..Config::default().click
            },
            ..Config::default()
        }
    }

    #[test]
    fn pointer_output_failure_releases_grab_and_retires_held_session() {
        for failure in [1, 2] {
            let pointer = FaultUinput::new();
            let extras = FaultUinput::new();
            let grab = Arc::new(FakeGrab::ok());
            let frames = (0..3)
                .map(|x| Ok(Some(frame(vec![finger(1, 200, x, 0)], 0))))
                .collect();
            let mut px = ClickProxy::new(
                settings(true),
                SharedGrab(Arc::clone(&grab)),
                DualSink::new(pointer.clone(), extras.clone()),
                FakeHaptic::new(),
                ScriptSource::new(frames),
            );
            px.set_pump_ready(true);
            px.attach(&usb_ready("/dev/hidraw1"));
            let cfg = output_failure_config();
            let now = std::time::Instant::now();
            px.handle_frame_at(&cfg, now).unwrap();
            assert!(grab.is_grabbed());
            assert!(pointer.inner.has(EV_ABS, ABS_MT_TRACKING_ID, 1));
            assert!(pointer.inner.has(EV_KEY, BTN_TOUCH, 1));
            assert!(pointer.inner.has(EV_KEY, BTN_LEFT, 1));
            assert!(extras.inner.has(EV_KEY, 29, 1));

            pointer.fail(failure);
            assert!(px.handle_frame_at(&cfg, spaced_at(now, 1)).is_err());
            assert!(!grab.is_grabbed());
            assert!(!px.is_attached());
            assert!(px.is_passthrough());
            assert!(!px.pump_ready);
            assert!(pointer.inner.was_destroyed());
            assert!(extras.inner.was_destroyed());
            assert!(px.machine.active().is_none());
            assert!(
                px.held_first.is_empty() && px.held_force.is_empty() && px.held_slide.is_empty()
            );
            assert!(!px.btn_left_down);
            assert!(px.pending_buttons.is_none() && px.pending_force.is_none());
            assert!(px.last_pos.is_empty());

            // A failed session stops consuming reports and cannot grab again
            // until its owner has supplied a fresh output/pump path.
            let pointer_events = wire_events(&pointer.inner);
            let extras_events = wire_events(&extras.inner);
            px.attach(&usb_ready("/dev/hidraw1"));
            px.handle_frame_at(&cfg, spaced_at(now, 2)).unwrap();
            assert!(!grab.is_grabbed());
            assert_eq!(px.source.frames.borrow().len(), 1);
            assert_eq!(wire_events(&pointer.inner), pointer_events);
            assert_eq!(wire_events(&extras.inner), extras_events);
        }
    }

    #[test]
    fn idle_staged_output_failure_releases_grab_and_cancels_pending_action() {
        let pointer = FaultUinput::new();
        let extras = FaultUinput::new();
        let mut cfg = output_failure_config();
        cfg.click.click_1.stagger_ms = Some(100);
        let mut px = ClickProxy::new(
            settings(true),
            FakeGrab::ok(),
            DualSink::new(pointer.clone(), extras.clone()),
            FakeHaptic::new(),
            ScriptSource::new(vec![Ok(Some(frame(vec![finger(1, 200, 0, 0)], 0)))]),
        );
        px.set_pump_ready(true);
        px.attach(&usb_ready("/dev/hidraw1"));
        let now = std::time::Instant::now();
        px.handle_frame_at(&cfg, now).unwrap();
        assert!(px.pending_buttons.is_some());
        assert!(extras.inner.has(EV_KEY, 29, 1));
        assert!(!pointer.inner.has(EV_KEY, BTN_LEFT, 1));
        pointer.fail(2);
        assert!(px.handle_frame_at(&cfg, spaced_at(now, 2)).is_err());
        assert!(!px.grab.is_grabbed());
        assert!(!px.is_attached());
        assert!(px.pending_buttons.is_none());
        assert!(pointer.inner.was_destroyed() && extras.inner.was_destroyed());
    }

    #[test]
    fn idle_pressure_release_preserves_pointer_failure_and_extras_degradation() {
        for pointer_fails in [false, true] {
            for failure in [1, 2] {
                for stagger in [0, 30] {
                    let pointer = FaultUinput::new();
                    let extras = FaultUinput::new();
                    let mut cfg = output_failure_config();
                    cfg.click.click_1.stagger_ms = Some(stagger);
                    let mut px = ClickProxy::new(
                        settings(true),
                        FakeGrab::ok(),
                        DualSink::new(pointer.clone(), extras.clone()),
                        FakeHaptic::new(),
                        // Pressure drops during release debounce. Keep the
                        // contact so idle must cancel any pending button.
                        ScriptSource::new(vec![
                            Ok(Some(frame(vec![finger(1, 100, 0, 0)], 0))),
                            Ok(Some(frame(vec![finger(1, 10, 0, 0)], 0))),
                        ]),
                    );
                    px.set_pump_ready(true);
                    px.attach(&usb_ready("/dev/hidraw1"));
                    let now = std::time::Instant::now();
                    px.handle_frame_at(&cfg, now).unwrap();
                    px.handle_frame_at(&cfg, ms_after(now, 10)).unwrap();
                    assert!(px.machine.active().is_some());
                    assert!(extras.inner.has(EV_KEY, 29, 1));
                    assert_eq!(px.pending_buttons.is_some(), stagger != 0);
                    if pointer_fails {
                        pointer.fail(failure);
                    } else {
                        extras.fail(failure);
                    }
                    let result = px.handle_frame_at(&cfg, ms_after(now, 30));
                    assert_eq!(result.is_err(), pointer_fails);
                    assert!(px.machine.active().is_none());
                    assert!(px.pending_buttons.is_none() && px.pending_force.is_none());
                    assert_eq!(px.grab.is_grabbed(), !pointer_fails);
                    assert_eq!(px.is_attached(), !pointer_fails);
                    assert_eq!(px.is_passthrough(), pointer_fails);
                    assert_eq!(pointer.inner.was_destroyed(), pointer_fails);
                    assert!(extras.inner.was_destroyed());
                    if stagger != 0 {
                        assert!(key_values(&pointer.inner, BTN_LEFT).is_empty());
                    } else if !pointer_fails {
                        assert_eq!(key_values(&pointer.inner, BTN_LEFT), vec![1, 0]);
                    }
                    let ended = (wire_events(&pointer.inner), wire_events(&extras.inner));
                    px.handle_frame_at(&cfg, ms_after(now, 1000)).unwrap();
                    assert_eq!(
                        (wire_events(&pointer.inner), wire_events(&extras.inner)),
                        ended
                    );
                }
            }
        }
    }

    #[test]
    fn idle_force_release_and_cut_restore_propagate_pointer_errors() {
        for force_on_pointer in [false, true] {
            for failure in [1, 2] {
                let pointer = FaultUinput::new();
                let extras = FaultUinput::new();
                let mut cfg = Config::default();
                cfg.click.click_1 = if force_on_pointer { "ctrl" } else { "left" }.into();
                cfg.force.force_offset = 100;
                cfg.force.transition_mode = "cut".into();
                cfg.force.force_1 = if force_on_pointer { "right" } else { "ctrl" }.into();
                let mut px = ClickProxy::new(
                    settings(true),
                    FakeGrab::ok(),
                    DualSink::new(pointer.clone(), extras.clone()),
                    FakeHaptic::new(),
                    ScriptSource::new(vec![slide_report(1, 100, 0), slide_report(1, 200, 0)]),
                );
                px.set_pump_ready(true);
                px.attach(&usb_ready("/dev/hidraw1"));
                let now = std::time::Instant::now();
                px.handle_frame_at(&cfg, now).unwrap();
                px.handle_frame_at(&cfg, ms_after(now, 10)).unwrap();
                px.handle_frame_at(&cfg, ms_after(now, 15)).unwrap();
                assert!(px.pending_force.is_none());
                assert_eq!(
                    key_values(
                        &pointer.inner,
                        if force_on_pointer {
                            BTN_RIGHT
                        } else {
                            BTN_LEFT
                        }
                    ),
                    if force_on_pointer {
                        vec![1]
                    } else {
                        vec![1, 0]
                    }
                );
                px.replace_source(ScriptSource::new(vec![slide_report(1, 100, 0)]));
                px.handle_frame_at(&cfg, ms_after(now, 16)).unwrap();
                assert!(px.machine.active().is_some() && px.machine.force_active().is_some());
                pointer.fail(failure);
                // At 40 ms idle expires the force latch. Depending on the
                // routing, pointer failure occurs on force key-up, the cut
                // first-level re-press, or the trailing frame flush.
                assert!(px.handle_frame_at(&cfg, ms_after(now, 40)).is_err());
                assert!(!px.is_attached() && !px.grab.is_grabbed());
                assert!(px.is_passthrough() && !px.pump_ready);
                assert!(pointer.inner.was_destroyed() && extras.inner.was_destroyed());
                assert!(px.machine.active().is_none() && px.machine.force_active().is_none());
                assert!(px.held_first.is_empty() && px.held_force.is_empty());
                assert!(!px.physical_left_down);
            }
        }
    }

    #[test]
    fn extras_output_failure_retires_only_extras_and_keeps_pointer_working() {
        for failure in [1, 2] {
            let pointer = FaultUinput::new();
            let extras = FaultUinput::new();
            let mut px = ClickProxy::new(
                settings(true),
                FakeGrab::ok(),
                DualSink::new(pointer.clone(), extras.clone()),
                FakeHaptic::new(),
                ScriptSource::new(vec![
                    Ok(Some(frame(vec![finger(1, 200, 0, 0)], 0))),
                    Ok(Some(frame(vec![finger(1, 200, 20, 0)], 0))),
                    Ok(Some(frame(vec![], 0))),
                    Ok(Some(frame(vec![finger(1, 200, 30, 0)], 0))),
                ]),
            );
            px.set_pump_ready(true);
            px.attach(&usb_ready("/dev/hidraw1"));
            let cfg = output_failure_config();
            let now = std::time::Instant::now();
            px.handle_frame_at(&cfg, now).unwrap();
            assert!(extras.inner.has(EV_KEY, 29, 1));
            extras.fail(failure);
            // Sync failure happens while holding the shortcut. Emit failure
            // happens on its key-up. Both must remove the keyboard node.
            px.handle_frame_at(&cfg, spaced_at(now, 1)).unwrap();
            px.handle_frame_at(&cfg, spaced_at(now, 2)).unwrap();
            assert!(extras.inner.was_destroyed());
            let retired_events = wire_events(&extras.inner);
            px.handle_frame_at(&cfg, spaced_at(now, 3)).unwrap();
            assert_eq!(wire_events(&extras.inner), retired_events);
            assert!(px.grab.is_grabbed() && px.is_attached());
            assert!(!px.is_passthrough());
            assert!(!pointer.inner.was_destroyed());
            assert!(pointer.inner.has(EV_ABS, ABS_MT_POSITION_X, 20));
            assert!(pointer.inner.has(EV_KEY, BTN_LEFT, 0));
            assert!(pointer.inner.has(EV_ABS, ABS_MT_TRACKING_ID, -1));
            assert_eq!(key_values(&pointer.inner, BTN_LEFT), vec![1, 0, 1]);
        }
    }

    #[test]
    fn unavailable_extras_drops_shortcuts_but_allows_pointer_attach_and_release() {
        let pointer = FaultUinput::new();
        let mut px = ClickProxy::new(
            settings(true),
            FakeGrab::ok(),
            DualSink::new(pointer.clone(), RealUinput::new_extras("/unused")),
            FakeHaptic::new(),
            ScriptSource::new(vec![
                Ok(Some(frame(vec![finger(1, 200, 0, 0)], 0))),
                Ok(Some(frame(vec![], 0))),
            ]),
        );
        px.set_pump_ready(true);
        px.attach(&usb_ready("/dev/hidraw1"));
        let cfg = output_failure_config();
        let now = std::time::Instant::now();
        px.handle_frame_at(&cfg, now).unwrap();
        px.handle_frame_at(&cfg, spaced_at(now, 1)).unwrap();
        assert!(px.is_attached() && px.grab.is_grabbed());
        assert_eq!(key_values(&pointer.inner, BTN_LEFT), vec![1, 0]);
        assert_eq!(px.uinput.extras().staged_len(), 0);
    }

    fn fixture_uinput(file: std::fs::File) -> RealUinput {
        let sink = RealUinput::new("/unused");
        *sink.fd.lock().unwrap() = Some(file);
        sink.created
            .store(true, std::sync::atomic::Ordering::SeqCst);
        sink
    }

    #[test]
    fn real_uinput_enospc_retires_fd_and_reports_unready() {
        let sink = fixture_uinput(
            std::fs::OpenOptions::new()
                .write(true)
                .open("/dev/full")
                .unwrap(),
        );
        sink.emit(EV_KEY, BTN_LEFT, 1).unwrap();
        assert_eq!(sink.staged_len(), 24);
        let error = sink.sync().unwrap_err();
        assert_eq!(
            error
                .downcast_ref::<std::io::Error>()
                .unwrap()
                .raw_os_error(),
            Some(libc::ENOSPC)
        );
        assert!(!sink.is_ready());
        assert!(!sink.fd_open());
        assert_eq!(sink.staged_len(), 0);
        assert!(sink.emit(EV_KEY, BTN_LEFT, 0).is_err());
        assert!(sink.sync().is_err());
        sink.destroy(); // idempotent after failure
        assert!(!sink.fd_open());
    }

    #[test]
    fn real_pointer_enospc_propagates_to_session_and_allows_fresh_attachment() {
        let pointer = fixture_uinput(tempfile::tempfile().unwrap());
        let extras = FaultUinput::new();
        let mut px = ClickProxy::new(
            settings(true),
            FakeGrab::ok(),
            DualSink::new(pointer, extras.clone()),
            FakeHaptic::new(),
            ScriptSource::new(vec![
                Ok(Some(frame(vec![finger(1, 200, 0, 0)], 0))),
                Ok(Some(frame(vec![finger(1, 200, 10, 0)], 0))),
            ]),
        );
        px.set_pump_ready(true);
        px.attach(&usb_ready("/dev/hidraw1"));
        let cfg = output_failure_config();
        let now = std::time::Instant::now();
        px.handle_frame_at(&cfg, now).unwrap();
        assert!(extras.inner.has(EV_KEY, 29, 1));
        assert!(px.btn_left_down && !px.last_pos.is_empty());
        *px.uinput.pointer().fd.lock().unwrap() = Some(
            std::fs::OpenOptions::new()
                .write(true)
                .open("/dev/full")
                .unwrap(),
        );
        let error = px.handle_frame_at(&cfg, spaced_at(now, 1)).unwrap_err();
        assert_eq!(
            error
                .downcast_ref::<std::io::Error>()
                .unwrap()
                .raw_os_error(),
            Some(libc::ENOSPC)
        );
        assert!(!px.is_attached() && !px.grab.is_grabbed());
        assert!(!px.uinput.pointer().fd_open());
        assert!(extras.inner.was_destroyed());

        // Session ownership can replace the failed outputs and attach again;
        // no manager retry/reconciliation policy is implemented here.
        px.uinput = DualSink::new(
            fixture_uinput(tempfile::tempfile().unwrap()),
            FaultUinput::new(),
        );
        px.replace_source(ScriptSource::new(vec![Ok(Some(frame(vec![], 0)))]));
        px.set_pump_ready(true);
        px.attach(&usb_ready("/dev/hidraw1"));
        px.handle_frame_at(&cfg, spaced_at(now, 2)).unwrap();
        assert!(px.is_attached() && px.grab.is_grabbed());
        assert!(px.machine.active().is_none());
        assert!(!px.btn_left_down && px.last_pos.is_empty());
    }

    #[test]
    fn teardown_write_failure_also_retires_both_outputs() {
        for pump_loss in [false, true] {
            let pointer = FaultUinput::new();
            let extras = FaultUinput::new();
            let mut px = ClickProxy::new(
                settings(true),
                FakeGrab::ok(),
                DualSink::new(pointer.clone(), extras.clone()),
                FakeHaptic::new(),
                ScriptSource::new(vec![Ok(Some(frame(vec![finger(1, 200, 0, 0)], 0)))]),
            );
            px.set_pump_ready(true);
            px.attach(&usb_ready("/dev/hidraw1"));
            px.handle_frame(&output_failure_config()).unwrap();
            pointer.fail(2);
            if pump_loss {
                px.set_pump_ready(false);
            } else {
                px.detach(&detached("/dev/hidraw1", Transport::Usb));
            }
            assert!(!px.is_attached() && !px.grab.is_grabbed());
            assert!(pointer.inner.was_destroyed() && extras.inner.was_destroyed());
        }
    }

    #[test]
    fn real_uinput_normal_frame_is_batched_and_stays_ready() {
        use std::io::{Read, Seek};
        let file = tempfile::tempfile().unwrap();
        let mut reader = file.try_clone().unwrap();
        let sink = fixture_uinput(file);
        sink.emit(EV_KEY, BTN_LEFT, 1).unwrap();
        assert_eq!(reader.metadata().unwrap().len(), 0);
        sink.sync().unwrap();
        reader.rewind().unwrap();
        let mut bytes = Vec::new();
        reader.read_to_end(&mut bytes).unwrap();
        let expected = [
            encode_event(EV_KEY, BTN_LEFT, 1),
            encode_event(EV_SYN, SYN_REPORT, 0),
        ]
        .concat();
        assert_eq!(bytes, expected);
        assert!(sink.is_ready());
        assert_eq!(sink.staged_len(), 0);
        sink.destroy();
        assert!(!sink.is_ready() && !sink.fd_open());
    }

    #[test]
    fn frame_writer_completes_short_writes_and_eintr_without_duplication() {
        use std::collections::VecDeque;
        use std::io::{self, Write};
        struct Writer {
            steps: VecDeque<io::Result<usize>>,
            bytes: Vec<u8>,
        }
        impl Write for Writer {
            fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
                let n = self.steps.pop_front().expect("unexpected write")?;
                self.bytes.extend_from_slice(&bytes[..n]);
                Ok(n)
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }
        let bytes = [
            encode_event(EV_KEY, BTN_LEFT, 1),
            encode_event(EV_SYN, SYN_REPORT, 0),
        ]
        .concat();
        let mut writer = Writer {
            steps: VecDeque::from([
                Err(io::Error::from(io::ErrorKind::Interrupted)),
                Ok(24),
                Err(io::Error::from(io::ErrorKind::Interrupted)),
                Ok(24),
            ]),
            bytes: Vec::new(),
        };
        RealUinput::flush_locked(&mut writer, &bytes).unwrap();
        assert_eq!(writer.bytes, bytes);
        for failure in [Ok(0), Err(io::Error::from_raw_os_error(libc::ENOSPC))] {
            writer.steps = VecDeque::from([Ok(24), failure]);
            writer.bytes.clear();
            let error = RealUinput::flush_locked(&mut writer, &bytes).unwrap_err();
            assert!(matches!(
                error.kind(),
                io::ErrorKind::WriteZero | io::ErrorKind::StorageFull
            ));
            assert_eq!(writer.bytes, bytes[..24]);
            assert!(writer.steps.is_empty(), "must stop on a failed write");
        }
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

    /// Exactly one trailing SYN: successful teardown emits key-ups +
    /// contact termination with no intermediate sync, then a single frame
    /// sync. Both the count and the trailing position are pinned.
    fn assert_single_trailing_syn(tail: &[(u16, u16, i32)], ctx: &str) {
        let syns = tail
            .iter()
            .filter(|&&(ty, code, _)| ty == EV_SYN && code == SYN_REPORT)
            .count();
        assert_eq!(
            syns, 1,
            "{ctx}: expected exactly one SYN, got {syns}: {tail:?}"
        );
        assert_eq!(
            tail.last(),
            Some(&(EV_SYN, SYN_REPORT, 0)),
            "{ctx}: SYN must trail the teardown wire: {tail:?}"
        );
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
            click: crate::config::ClickConfig {
                click_1: "ctrl+c".into(),
                ..Config::default().click
            },
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
            click: crate::config::ClickConfig {
                click_2: "right".into(),
                ..Config::default().click
            },
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
            click: crate::config::ClickConfig {
                click_1: "empty".into(),
                ..Config::default().click
            },
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
    fn teardown_with_live_contacts_terminates_slots_and_tool_bits() {
        // Finding #7: disconnect fallback left virtual contacts active
        // (only SYN on teardown, no slot termination / BTN_TOUCH=0).
        // Every source-loss path must end every contact + tool bit via
        // the single idempotent teardown.
        const TOOLS: [u16; 5] = [
            BTN_TOOL_FINGER,
            BTN_TOOL_DOUBLETAP,
            BTN_TOOL_TRIPLETAP,
            BTN_TOOL_QUADTAP,
            BTN_TOOL_QUINTTAP,
        ];
        for teardown in ["pump_loss", "detach"] {
            for (label, fingers) in [
                ("light-touch", vec![finger(1, 10, 100, 60)]),
                ("press", vec![finger(1, 200, 100, 60)]),
                ("two-finger", vec![finger(1, 10, 0, 0), finger(2, 10, 9, 9)]),
            ] {
                let slots = fingers.len();
                let sinks = TestSinks::new();
                let haptic = Arc::new(FakeHaptic::new());
                let frames = vec![Ok(Some(frame(fingers, 0)))];
                let mut px = drive_ready_proxy(&sinks, &haptic, frames);
                let cfg = Config::default();
                let t0 = std::time::Instant::now();
                px.handle_frame_at(&cfg, spaced_at(t0, 0)).unwrap();
                assert!(
                    sinks.pointer.has(EV_KEY, BTN_TOUCH, 1),
                    "{label}/{teardown}: live contact must drive BTN_TOUCH=1"
                );
                let base_len = sinks.pointer.events.lock().unwrap().len();
                match teardown {
                    "pump_loss" => px.set_pump_ready(false),
                    "detach" => px.detach(&detached("/dev/hidraw1", Transport::Usb)),
                    _ => unreachable!(),
                }
                assert!(!px.is_attached(), "{label}/{teardown}: still attached");
                assert!(px.is_passthrough(), "{label}/{teardown}: not passthrough");
                assert_eq!(px.max_slot, 0, "{label}/{teardown}: max_slot not reset");
                assert!(px.last_pos.is_empty(), "{label}/{teardown}: anchors leak");
                assert!(!px.btn_left_down, "{label}/{teardown}: BTN_LEFT flag stuck");
                let evs = sinks.pointer.events.lock().unwrap().clone();
                let tail = &evs[base_len..];
                assert_single_trailing_syn(tail, &format!("{label}/{teardown}"));
                for slot in 0..slots {
                    assert!(
                        tail.windows(2)
                            .any(|w| w[0] == (EV_ABS, ABS_MT_SLOT, slot as i32)
                                && w[1] == (EV_ABS, ABS_MT_TRACKING_ID, -1)),
                        "{label}/{teardown}: slot {slot} never terminated: {tail:?}"
                    );
                }
                let touch_vals = key_values(&sinks.pointer, BTN_TOUCH);
                assert_eq!(
                    touch_vals.last(),
                    Some(&0),
                    "{label}/{teardown}: BTN_TOUCH never cleared: {touch_vals:?}"
                );
                for tool in TOOLS {
                    let vals = key_values(&sinks.pointer, tool);
                    assert_eq!(
                        vals.last(),
                        Some(&0),
                        "{label}/{teardown}: tool {tool} never cleared: {vals:?}"
                    );
                }
                // Idempotent: a second teardown emits nothing.
                let len_after_first = sinks.pointer.events.lock().unwrap().len();
                match teardown {
                    "pump_loss" => px.set_pump_ready(false),
                    "detach" => px.detach(&detached("/dev/hidraw1", Transport::Usb)),
                    _ => unreachable!(),
                }
                assert_eq!(
                    sinks.pointer.events.lock().unwrap().len(),
                    len_after_first,
                    "{label}/{teardown}: second teardown emitted duplicate wire traffic"
                );
            }
        }
    }

    #[test]
    fn disconnect_with_live_contacts_terminates_slots() {
        // Same-name replug while a light-touch contact is live: the
        // disconnect self-release must terminate the slot, clear
        // BTN_TOUCH + tool bits, and stop polling the dead source.
        use anyhow::Error as AnyhowError;
        let sinks = TestSinks::new();
        let haptic = Arc::new(FakeHaptic::new());
        let disc = || {
            Err::<Option<TouchFrame>, AnyhowError>(
                crate::pump::DisconnectError("test unplug".into()).into(),
            )
        };
        let frames = vec![
            Ok(Some(frame(vec![finger(1, 10, 100, 60)], 0))),
            disc(),
            disc(),
        ];
        let mut px = ClickProxy::new(
            settings(true),
            SharedGrab(Arc::new(FakeGrab::ok())),
            sinks.dual(),
            SharedHaptic(Arc::clone(&haptic)),
            ScriptSource::new(frames),
        );
        px.set_pump_ready(true);
        px.attach(&usb_ready("/dev/hidraw8"));
        let cfg = Config::default();
        let t0 = std::time::Instant::now();
        px.handle_frame_at(&cfg, spaced_at(t0, 0)).unwrap();
        assert!(sinks.pointer.has(EV_KEY, BTN_TOUCH, 1));
        let base_len = sinks.pointer.events.lock().unwrap().len();
        px.handle_frame_at(&cfg, spaced_at(t0, 1)).unwrap();
        assert!(!px.is_attached());
        assert!(px.is_passthrough());
        let evs = sinks.pointer.events.lock().unwrap().clone();
        let tail = &evs[base_len..];
        assert!(
            tail.windows(2)
                .any(|w| w[0] == (EV_ABS, ABS_MT_SLOT, 0)
                    && w[1] == (EV_ABS, ABS_MT_TRACKING_ID, -1)),
            "disconnect left slot 0 active: {tail:?}"
        );
        assert_eq!(key_values(&sinks.pointer, BTN_TOUCH).last(), Some(&0));
        for tool in [
            BTN_TOOL_FINGER,
            BTN_TOOL_DOUBLETAP,
            BTN_TOOL_TRIPLETAP,
            BTN_TOOL_QUADTAP,
            BTN_TOOL_QUINTTAP,
        ] {
            assert_eq!(key_values(&sinks.pointer, tool).last(), Some(&0));
        }
        assert_single_trailing_syn(tail, "disconnect");
        // Dead source never polled again.
        let remaining = px.source.frames.borrow().len();
        px.handle_frame_at(&cfg, spaced_at(t0, 2)).unwrap();
        assert_eq!(px.source.frames.borrow().len(), remaining);
    }

    type TestManager =
        PumpManager<SharedGrab, DualSink<SharedUinput, SharedUinput>, SharedHaptic, ScriptSource>;

    /// Manager under test with fakes: the SAME `finish_attach` / `detach` /
    /// `Drop` dispatch production uses, with an empty script source as idle.
    fn test_manager(
        grab: &Arc<FakeGrab>,
        sinks: &TestSinks,
        haptic: &Arc<FakeHaptic>,
    ) -> TestManager {
        let proxy = std::sync::Arc::new(std::sync::Mutex::new(ClickProxy::new(
            settings(true),
            SharedGrab(Arc::clone(grab)),
            sinks.dual(),
            SharedHaptic(Arc::clone(haptic)),
            ScriptSource::new(vec![]),
        )));
        PumpManager::new(
            proxy,
            Config {
                enhanced_clicks: true,
                ..Config::default()
            },
            std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
            || ScriptSource::new(vec![]),
        )
    }

    /// Poison the manager's proxy mutex the way a panicked pump iteration
    /// would: lock, panic while holding, rejoin. The next manager op must
    /// recover the mutex and still tear down.
    fn poison_manager_proxy(manager: &TestManager) {
        let proxy = std::sync::Arc::clone(manager.proxy());
        let _ = std::thread::spawn(move || {
            let _guard = proxy.lock().unwrap();
            panic!("test poison");
        })
        .join();
    }

    /// True when `control` becomes readable within `timeout` (drains the
    /// byte so phases stay independent).
    fn control_wakes_within(
        control: &std::sync::Arc<crate::pump::PumpControl>,
        timeout: std::time::Duration,
    ) -> bool {
        let mut pfd = libc::pollfd {
            fd: control.read_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        let woke = unsafe { libc::poll(&mut pfd, 1, timeout.as_millis() as libc::c_int) } > 0;
        if woke {
            control.drain();
        }
        woke
    }

    #[test]
    fn manager_drop_wakes_blocked_pump_via_control() {
        // Actual manager `Drop` establishes stop AND wakes a poll-blocked
        // event-driven pump: the worker must observe shutdown without
        // waiting for the next deadline. The wake fires even though the
        // session teardown itself is silent here (drop-before-attach).
        let grab = Arc::new(FakeGrab::ok());
        let sinks = TestSinks::new();
        let haptic = Arc::new(FakeHaptic::new());
        let control = Arc::new(crate::pump::PumpControl::new().unwrap());
        let manager = test_manager(&grab, &sinks, &haptic).with_control(Arc::clone(&control));
        drop(manager);
        assert!(
            control_wakes_within(&control, std::time::Duration::from_secs(2)),
            "Drop did not notify a poll-blocked pump"
        );
    }

    #[test]
    fn attach_rollback_via_manager_idles_and_destroys() {
        // Grab-refused attach through the ACTUAL manager dispatch
        // (`PumpManager::finish_attach`, the same tail production calls
        // after opening the source). A fresh refused session holds no wire
        // state, so rollback emits nothing, then idles the source and
        // destroys both nodes; a repeat refused attach stays silent.
        let grab = Arc::new(FakeGrab::failing());
        let sinks = TestSinks::new();
        let haptic = Arc::new(FakeHaptic::new());
        let manager = test_manager(&grab, &sinks, &haptic);
        let live = || ScriptSource::new(vec![Ok(Some(frame(vec![finger(1, 10, 100, 60)], 0)))]);
        manager.finish_attach(&usb_ready("/dev/hidraw1"), live(), |_| {});
        {
            let px = manager.proxy().lock().unwrap();
            assert!(!px.is_attached());
            assert!(px.is_passthrough());
            assert!(
                px.source.frames.borrow().is_empty(),
                "rollback must idle the source"
            );
        }
        assert!(!grab.is_grabbed());
        assert!(sinks.pointer.was_destroyed() && sinks.extras.was_destroyed());
        assert!(
            sinks.pointer.events.lock().unwrap().is_empty(),
            "fresh rollback must emit no wire traffic"
        );
        // A repeat refused attach stays silent.
        manager.finish_attach(&usb_ready("/dev/hidraw1"), live(), |_| {});
        assert!(
            sinks.pointer.events.lock().unwrap().is_empty(),
            "second rollback emitted duplicate wire traffic"
        );
    }

    #[test]
    fn attach_rollback_terminates_stale_physical_hold() {
        // Stale wire state through the actual rollback dispatch: seed a
        // bare-hardware-click hold (`BTN_LEFT` wire-down with no latch, the
        // physical-only path [`ClickProxy::release_held`] must release),
        // refuse the grab, and require `BTN_LEFT = 0`, slot termination,
        // tool clear, exactly one trailing SYN, and silence on repeat.
        let grab = Arc::new(FakeGrab::failing());
        let sinks = TestSinks::new();
        let haptic = Arc::new(FakeHaptic::new());
        let manager = test_manager(&grab, &sinks, &haptic);
        manager.finish_attach(
            &usb_ready("/dev/hidraw1"),
            ScriptSource::new(vec![]),
            |_| {},
        );
        {
            let mut px = manager.proxy().lock().unwrap();
            px.btn_left_down = true;
            px.physical_left_down = true;
            px.last_pos.insert(
                1,
                PointerAnchor {
                    raw: (100, 60),
                    emitted: (100, 60),
                    rem: (0, 0),
                },
            );
        }
        let base_len = sinks.pointer.events.lock().unwrap().len();
        manager.finish_attach(
            &usb_ready("/dev/hidraw1"),
            ScriptSource::new(vec![]),
            |_| {},
        );
        {
            let px = manager.proxy().lock().unwrap();
            assert!(!px.is_attached());
            assert!(!px.btn_left_down);
            assert!(!px.physical_left_down);
            assert!(px.last_pos.is_empty());
            assert_eq!(px.max_slot, 0);
        }
        assert!(!grab.is_grabbed());
        assert!(sinks.pointer.was_destroyed() && sinks.extras.was_destroyed());
        let evs = sinks.pointer.events.lock().unwrap().clone();
        let tail = &evs[base_len..];
        assert!(
            tail.windows(2)
                .any(|w| w[0] == (EV_ABS, ABS_MT_SLOT, 0)
                    && w[1] == (EV_ABS, ABS_MT_TRACKING_ID, -1)),
            "rollback left slot 0 active: {tail:?}"
        );
        assert_eq!(
            key_values(&sinks.pointer, BTN_LEFT).last(),
            Some(&0),
            "stale physical hold survived rollback"
        );
        assert_eq!(key_values(&sinks.pointer, BTN_TOUCH).last(), Some(&0));
        for tool in [
            BTN_TOOL_FINGER,
            BTN_TOOL_DOUBLETAP,
            BTN_TOOL_TRIPLETAP,
            BTN_TOOL_QUADTAP,
            BTN_TOOL_QUINTTAP,
        ] {
            assert_eq!(key_values(&sinks.pointer, tool).last(), Some(&0));
        }
        assert_single_trailing_syn(tail, "stale rollback");
        // Idled source drains to no-op; a repeat rollback stays silent.
        let cfg = Config::default();
        let t0 = std::time::Instant::now();
        assert!(manager
            .proxy()
            .lock()
            .unwrap()
            .handle_frame_at(&cfg, spaced_at(t0, 0))
            .unwrap()
            .is_none());
        let len_after_first = sinks.pointer.events.lock().unwrap().len();
        manager.finish_attach(
            &usb_ready("/dev/hidraw1"),
            ScriptSource::new(vec![]),
            |_| {},
        );
        assert_eq!(
            sinks.pointer.events.lock().unwrap().len(),
            len_after_first,
            "second rollback emitted duplicate wire traffic"
        );
    }

    #[test]
    fn manager_drop_terminates_live_press() {
        // Actual `PumpManager` Drop with a live press (keys + contact +
        // BTN_LEFT): slot terminated, touch/tool/left cleared, exactly one
        // trailing SYN, grab released, source idled, both nodes destroyed.
        let grab = Arc::new(FakeGrab::ok());
        let sinks = TestSinks::new();
        let haptic = Arc::new(FakeHaptic::new());
        let manager = test_manager(&grab, &sinks, &haptic);
        manager.finish_attach(
            &usb_ready("/dev/hidraw1"),
            ScriptSource::new(vec![Ok(Some(frame(vec![finger(1, 200, 100, 60)], 0)))]),
            |_| {},
        );
        assert!(manager.proxy().lock().unwrap().is_attached());
        let cfg = Config::default();
        let t0 = std::time::Instant::now();
        manager
            .proxy()
            .lock()
            .unwrap()
            .handle_frame_at(&cfg, spaced_at(t0, 0))
            .unwrap();
        assert!(sinks.pointer.has(EV_KEY, BTN_TOUCH, 1));
        assert!(sinks.pointer.has(EV_KEY, BTN_LEFT, 1));
        let base_len = sinks.pointer.events.lock().unwrap().len();
        drop(manager);
        assert!(!grab.is_grabbed());
        assert!(sinks.pointer.was_destroyed() && sinks.extras.was_destroyed());
        let evs = sinks.pointer.events.lock().unwrap().clone();
        let tail = &evs[base_len..];
        assert!(
            tail.windows(2)
                .any(|w| w[0] == (EV_ABS, ABS_MT_SLOT, 0)
                    && w[1] == (EV_ABS, ABS_MT_TRACKING_ID, -1)),
            "shutdown left slot 0 active: {tail:?}"
        );
        assert_eq!(key_values(&sinks.pointer, BTN_TOUCH).last(), Some(&0));
        assert_eq!(key_values(&sinks.pointer, BTN_LEFT).last(), Some(&0));
        for tool in [
            BTN_TOOL_FINGER,
            BTN_TOOL_DOUBLETAP,
            BTN_TOOL_TRIPLETAP,
            BTN_TOOL_QUADTAP,
            BTN_TOOL_QUINTTAP,
        ] {
            assert_eq!(key_values(&sinks.pointer, tool).last(), Some(&0));
        }
        assert_single_trailing_syn(tail, "shutdown drop");
    }

    #[test]
    fn manager_drop_recovers_poisoned_proxy() {
        // A panicked holder poisons the proxy mutex; Drop must recover it
        // and still terminate the live session (same wire assertions as
        // the clean Drop above).
        let grab = Arc::new(FakeGrab::ok());
        let sinks = TestSinks::new();
        let haptic = Arc::new(FakeHaptic::new());
        let manager = test_manager(&grab, &sinks, &haptic);
        manager.finish_attach(
            &usb_ready("/dev/hidraw1"),
            ScriptSource::new(vec![Ok(Some(frame(vec![finger(1, 200, 100, 60)], 0)))]),
            |_| {},
        );
        let cfg = Config::default();
        let t0 = std::time::Instant::now();
        manager
            .proxy()
            .lock()
            .unwrap()
            .handle_frame_at(&cfg, spaced_at(t0, 0))
            .unwrap();
        assert!(sinks.pointer.has(EV_KEY, BTN_LEFT, 1));
        let base_len = sinks.pointer.events.lock().unwrap().len();
        poison_manager_proxy(&manager);
        drop(manager);
        assert!(!grab.is_grabbed());
        assert!(sinks.pointer.was_destroyed() && sinks.extras.was_destroyed());
        let evs = sinks.pointer.events.lock().unwrap().clone();
        let tail = &evs[base_len..];
        assert!(
            tail.windows(2)
                .any(|w| w[0] == (EV_ABS, ABS_MT_SLOT, 0)
                    && w[1] == (EV_ABS, ABS_MT_TRACKING_ID, -1)),
            "poisoned shutdown left slot 0 active: {tail:?}"
        );
        assert_eq!(key_values(&sinks.pointer, BTN_TOUCH).last(), Some(&0));
        assert_eq!(key_values(&sinks.pointer, BTN_LEFT).last(), Some(&0));
        assert_single_trailing_syn(tail, "poisoned shutdown drop");
    }

    #[test]
    fn manager_detach_then_drop_stays_silent() {
        // Drop-after-detach through the actual dispatch: `detach` owns the
        // live press (exactly one trailing SYN, source idled, both nodes
        // destroyed), and the later Drop finds no wire state, so it emits
        // nothing further.
        let grab = Arc::new(FakeGrab::ok());
        let sinks = TestSinks::new();
        let haptic = Arc::new(FakeHaptic::new());
        let manager = test_manager(&grab, &sinks, &haptic);
        manager.finish_attach(
            &usb_ready("/dev/hidraw1"),
            ScriptSource::new(vec![Ok(Some(frame(vec![finger(1, 200, 100, 60)], 0)))]),
            |_| {},
        );
        let cfg = Config::default();
        let t0 = std::time::Instant::now();
        manager
            .proxy()
            .lock()
            .unwrap()
            .handle_frame_at(&cfg, spaced_at(t0, 0))
            .unwrap();
        assert!(sinks.pointer.has(EV_KEY, BTN_LEFT, 1));
        let base_len = sinks.pointer.events.lock().unwrap().len();
        manager.detach(&detached("/dev/hidraw1", Transport::Usb));
        {
            let px = manager.proxy().lock().unwrap();
            assert!(!px.is_attached());
            assert!(px.is_passthrough());
        }
        assert!(sinks.pointer.was_destroyed() && sinks.extras.was_destroyed());
        let evs = sinks.pointer.events.lock().unwrap().clone();
        assert_single_trailing_syn(&evs[base_len..], "manager detach");
        // A sibling Detached must not touch the torn-down session, and
        // the later Drop must emit nothing further.
        manager.detach(&detached("/dev/hidraw9", Transport::Usb));
        let len_after_detach = sinks.pointer.events.lock().unwrap().len();
        drop(manager);
        assert_eq!(
            sinks.pointer.events.lock().unwrap().len(),
            len_after_detach,
            "Drop after detach emitted duplicate wire traffic"
        );
    }

    #[test]
    fn output_failure_double_destroy_is_safe() {
        // output_failed runs teardown (which destroys on its own
        // wire-failure branch) then destroys again: destroy must be
        // idempotent, and a repeat teardown must not re-destroy or
        // re-emit. Pinned with a destroy-counting sink.
        #[derive(Clone)]
        struct CountingSink {
            inner: FaultUinput,
            destroys: Arc<std::sync::atomic::AtomicUsize>,
        }
        impl UinputSink for CountingSink {
            fn is_ready(&self) -> bool {
                self.inner.is_ready()
            }
            fn emit(&self, ty: u16, code: u16, value: i32) -> Result<()> {
                self.inner.emit(ty, code, value)
            }
            fn sync(&self) -> Result<()> {
                self.inner.sync()
            }
            fn destroy(&self) {
                self.destroys
                    .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                self.inner.destroy();
            }
        }
        let pointer = CountingSink {
            inner: FaultUinput::new(),
            destroys: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        };
        let extras = CountingSink {
            inner: FaultUinput::new(),
            destroys: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        };
        let grab = Arc::new(FakeGrab::ok());
        let frames = vec![
            Ok(Some(frame(vec![finger(1, 200, 0, 0)], 0))),
            Ok(Some(frame(vec![finger(1, 200, 10, 0)], 0))),
        ];
        let mut px = ClickProxy::new(
            settings(true),
            SharedGrab(Arc::clone(&grab)),
            DualSink::new(pointer.clone(), extras.clone()),
            FakeHaptic::new(),
            ScriptSource::new(frames),
        );
        px.set_pump_ready(true);
        px.attach(&usb_ready("/dev/hidraw1"));
        let cfg = output_failure_config();
        let now = std::time::Instant::now();
        px.handle_frame_at(&cfg, now).unwrap();
        assert!(pointer.inner.inner.has(EV_KEY, BTN_TOUCH, 1));
        // Break the pointer path: teardown's wire fails, both nodes retire.
        pointer.inner.fail(2);
        assert!(px.handle_frame_at(&cfg, spaced_at(now, 1)).is_err());
        assert!(!px.is_attached());
        assert!(px.is_passthrough());
        assert!(!grab.is_grabbed());
        assert!(pointer.inner.inner.was_destroyed() && extras.inner.inner.was_destroyed());
        let pointer_destroys = pointer.destroys.load(std::sync::atomic::Ordering::SeqCst);
        let extras_destroys = extras.destroys.load(std::sync::atomic::Ordering::SeqCst);
        assert!(
            pointer_destroys >= 1 && extras_destroys >= 1,
            "both nodes must retire (pointer={pointer_destroys}, extras={extras_destroys})"
        );
        // Repeat teardown: no new wire traffic, no further destroys.
        let pointer_events = pointer.inner.inner.events.lock().unwrap().len();
        let extras_events = extras.inner.inner.events.lock().unwrap().len();
        px.teardown_session("repeat");
        assert_eq!(
            pointer.inner.inner.events.lock().unwrap().len(),
            pointer_events
        );
        assert_eq!(
            extras.inner.inner.events.lock().unwrap().len(),
            extras_events
        );
        assert_eq!(
            pointer.destroys.load(std::sync::atomic::Ordering::SeqCst),
            pointer_destroys
        );
        assert_eq!(
            extras.destroys.load(std::sync::atomic::Ordering::SeqCst),
            extras_destroys
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
            click: crate::config::ClickConfig {
                sensitivity: 64,
                margin: 5,
                ..Config::default().click
            },
            force: crate::config::ForceConfig {
                force_offset: 20,
                force_1: "forward".into(),
                transition_mode: "continuous".into(),
                ..Config::default().force
            },
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
        // Force supersedes in reporting; haptic falls back to standard 2, quiet.
        assert_eq!(
            haptic.calls.lock().unwrap().as_slice(),
            &[(64, 2, true), (64, 2, false)]
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
        let mut cfg = force_cfg();
        cfg.haptics.force_haptic = Some(9);
        cfg.haptics.force_quiet_click = true;
        px.handle_frame(&cfg).unwrap();
        let ev = px.handle_frame(&cfg).unwrap().unwrap();
        assert!(ev.is_force());
        assert_eq!(ev.haptic, 9);
        assert!(ev.quiet);
        assert_eq!(
            haptic.calls.lock().unwrap().as_slice(),
            &[(64, 2, true), (64, 9, true)]
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
        let mut cfg = force_cfg();
        cfg.force.force_2 = "right".into();
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
            click: crate::config::ClickConfig {
                sensitivity: 64,
                margin: 5,
                click_1: "ctrl+c".into(),
                ..Config::default().click
            },
            force: crate::config::ForceConfig {
                force_offset: 20,
                force_1: "ctrl+v".into(),
                transition_mode: "continuous".into(),
                ..Config::default().force
            },
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
            click: crate::config::ClickConfig {
                sensitivity: 64,
                margin: 5,
                ..Config::default().click
            },
            force: crate::config::ForceConfig {
                force_offset: 20,
                force_1: "left".into(),
                transition_mode: "continuous".into(),
                ..Config::default().force
            },
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
            &[(64, 2, true), (64, 2, false)]
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
        let mut cfg = force_cfg();
        cfg.haptics.quiet_click = true;
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
        let mut cfg = force_cfg();
        cfg.haptics.force_haptic = Some(0);
        px.handle_frame(&cfg).unwrap();
        let ev = px.handle_frame(&cfg).unwrap().unwrap();
        assert!(ev.is_force());
        assert_eq!(ev.haptic, 0);
        assert_eq!(haptic.calls.lock().unwrap().as_slice(), &[(64, 2, true)]);
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
        let mut cfg = force_cfg();
        cfg.haptics.force_haptic = Some(9);
        cfg.haptics.force_quiet_click = true;
        let t0 = std::time::Instant::now();
        let first = px.handle_frame_at(&cfg, spaced_at(t0, 0)).unwrap().unwrap();
        assert!(!first.is_force());
        assert_eq!(haptic.calls.lock().unwrap().as_slice(), &[(64, 2, true)]);
        let ev = px.handle_frame_at(&cfg, spaced_at(t0, 1)).unwrap().unwrap();
        assert!(ev.is_force());
        assert_eq!(
            haptic.calls.lock().unwrap().as_slice(),
            &[(64, 2, true), (64, 9, true)]
        );
    }

    /// Owner's tune (6 months on old driver): sensitivity 30, haptic 2
    /// quiet, force_offset 130, force_haptic 1. `forward` stands in for
    /// the owner's force map (action identity is not under test; the
    /// haptic pulse bytes + order are).
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
                transition_mode: "continuous".into(),
                ..Config::default().force
            },
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
        // contact=false but pressure 20 (> touch floor 10): the slot is
        // emitted (cursor tracks) while the click latch stays idle.
        let sinks = TestSinks::new();
        let haptic = Arc::new(FakeHaptic::new());
        let mut hover = finger(1, 20, 30, 40);
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
            click: crate::config::ClickConfig {
                sensitivity: 80,
                ..Config::default().click
            },
            haptics: crate::config::HapticsConfig {
                haptic: 7,
                ..Config::default().haptics
            },
            ..Config::default()
        };
        px.handle_frame(&cfg).unwrap();
        assert_eq!(haptic.calls.lock().unwrap().as_slice(), &[(80, 7, true)]);
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
        let cfg = Config {
            click: crate::config::ClickConfig {
                sensitivity: 64,
                ..Config::default().click
            },
            haptics: crate::config::HapticsConfig {
                haptic: 5,
                quiet_click: false,
                ..Config::default().haptics
            },
            ..Config::default()
        };
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
            haptics: crate::config::HapticsConfig {
                haptic: 0,
                ..Config::default().haptics
            },
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

    /// Every `EV_ABS` code the proxy may ever emit (single-touch position
    /// plus the MT slot block).
    const ALLOWED_ABS: [u16; 10] = [
        ABS_X,
        ABS_Y,
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
    /// - `EV_ABS` is one of the ten position/slot codes;
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
            (EV_ABS, ABS_X, 10),
            (EV_ABS, ABS_Y, 20),
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
            (EV_ABS, ABS_X, 15),
            (EV_ABS, ABS_Y, 25),
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
            (EV_ABS, ABS_X, 15),
            (EV_ABS, ABS_Y, 25),
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
            (EV_ABS, ABS_X, 0),
            (EV_ABS, ABS_Y, 0),
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
            (EV_ABS, ABS_X, 0),
            (EV_ABS, ABS_Y, 0),
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
            (EV_ABS, ABS_X, 0),
            (EV_ABS, ABS_Y, 0),
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
        // (test-end teardown retires the session, then drains post-retire;
        // nothing here depends on post-drop execution).
    }

    #[test]
    fn async_haptic_preserves_down_up_order() {
        // FIFO: the worker must fire pulses in enqueue order so down/up
        // pairs stay paired even though the pump never waits. (Teardown
        // retires the session, then drains post-retire with a bounded
        // join, so drain completion is awaited BEFORE drop.)
        let rec = Arc::new(FakeHaptic::new());
        let ah = AsyncHaptic::new(SharedHaptic(Arc::clone(&rec)));
        ah.click_feedback(64, 5, false).unwrap();
        ah.release_feedback(64, false).unwrap();
        ah.click_feedback(64, 5, true).unwrap();
        ah.release_feedback(64, true).unwrap();
        let t0 = std::time::Instant::now();
        while rec.calls.lock().unwrap().len() < 2 || rec.releases.lock().unwrap().len() < 2 {
            assert!(
                t0.elapsed() < std::time::Duration::from_secs(5),
                "haptic worker never drained the queue"
            );
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        assert_eq!(
            rec.calls.lock().unwrap().as_slice(),
            &[(64, 5, false), (64, 5, true)]
        );
        assert_eq!(
            rec.releases.lock().unwrap().as_slice(),
            &[(64, false), (64, true)]
        );
        drop(ah); // retire + bounded join
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
        // Await the drain before drop (teardown retires; it must not be
        // relied on to execute queued work).
        let t0 = std::time::Instant::now();
        while calls.lock().unwrap().len() < 2 || releases.lock().unwrap().len() < 2 {
            assert!(
                t0.elapsed() < std::time::Duration::from_secs(5),
                "haptic worker never drained the queue"
            );
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        drop(ah); // retire (empty) + bounded join
        assert_eq!(calls.lock().unwrap().len(), 2);
        assert_eq!(releases.lock().unwrap().len(), 2);
    }

    /// Target-recording, gate-blocked haptic for session tests. The worker
    /// blocks inside `execute_*` until the test opens the gate, so
    /// backlogs build deterministically; every execution records the
    /// session snapshot it ran against: `(node, bluetooth, is_down,
    /// pressure_or_down_pressure, intensity, quiet)`.
    #[derive(Clone)]
    struct GateHaptic {
        shared: Arc<GateShared>,
    }

    struct GateShared {
        state: Mutex<GateState>,
        cvar: Condvar,
    }

    struct GateState {
        open: bool,
        entered: usize,
        executed: Vec<(String, bool, bool, u8, u8, bool)>,
    }

    impl GateHaptic {
        fn new_closed() -> Self {
            Self {
                shared: Arc::new(GateShared {
                    state: Mutex::new(GateState {
                        open: false,
                        entered: 0,
                        executed: Vec::new(),
                    }),
                    cvar: Condvar::new(),
                }),
            }
        }
        fn open(&self) {
            let mut state = self.shared.state.lock().unwrap();
            state.open = true;
            self.shared.cvar.notify_all();
        }
        fn entered(&self) -> usize {
            self.shared.state.lock().unwrap().entered
        }
        fn executed(&self) -> Vec<(String, bool, bool, u8, u8, bool)> {
            self.shared.state.lock().unwrap().executed.clone()
        }
        fn wait_entered(&self, n: usize) {
            let t0 = std::time::Instant::now();
            while self.entered() < n {
                assert!(
                    t0.elapsed() < std::time::Duration::from_secs(5),
                    "haptic worker never picked up pulse"
                );
                std::thread::sleep(std::time::Duration::from_millis(1));
            }
        }
        /// Await drain completion BEFORE drop: teardown retires first
        /// (queued remainder purged), then drains post-retire with a
        /// bounded join, so tests asserting execution must wait here,
        /// not rely on `Drop`.
        fn wait_executed(&self, n: usize) {
            let t0 = std::time::Instant::now();
            while self.executed().len() < n {
                assert!(
                    t0.elapsed() < std::time::Duration::from_secs(5),
                    "haptic worker never executed {n} pulses"
                );
                std::thread::sleep(std::time::Duration::from_millis(1));
            }
        }
        fn record(&self, target: &HapticTarget, down: bool, a: u8, b: u8, quiet: bool) {
            let mut state = self.shared.state.lock().unwrap();
            state.entered += 1;
            while !state.open {
                state = self.shared.cvar.wait(state).unwrap();
            }
            state
                .executed
                .push((target.node.clone(), target.bluetooth, down, a, b, quiet));
        }
    }

    impl HapticSender for GateHaptic {
        fn click_feedback(&self, _pressure: u8, _intensity: u8, _quiet: bool) -> Result<()> {
            Ok(())
        }
        fn release_feedback(&self, _down_pressure: u8, _quiet: bool) -> Result<()> {
            Ok(())
        }
        fn execute_down(
            &self,
            target: &HapticTarget,
            pressure: u8,
            intensity: u8,
            quiet: bool,
        ) -> Result<()> {
            self.record(target, true, pressure, intensity, quiet);
            Ok(())
        }
        fn execute_up(&self, target: &HapticTarget, down_pressure: u8, quiet: bool) -> Result<()> {
            self.record(target, false, down_pressure, 0, quiet);
            Ok(())
        }
    }

    #[test]
    fn haptic_backlog_retires_across_sessions() {
        // Reconnect with a queued backlog: the retire purges queued pulses
        // and bumps the generation, so stale work never fires — in
        // particular never on the newly selected node. The pulse already
        // inside the firmware send at detach completes against its own
        // (old) snapshot; it is never redirected.
        let gate = GateHaptic::new_closed();
        let ah = AsyncHaptic::new(gate.clone());
        ah.retarget("/dev/hidraw-old", false);
        ah.click_feedback(64, 5, false).unwrap();
        gate.wait_entered(1); // worker holds the first pulse inside the gate
        for _ in 0..5 {
            ah.click_feedback(64, 5, false).unwrap();
        }
        for _ in 0..5 {
            ah.release_feedback(64, false).unwrap();
        }
        assert_eq!(ah.pending(), 10);
        let gen_old = ah.generation();
        ah.retarget("/dev/hidraw-new", true); // reconnect: new session
        assert_eq!(ah.pending(), 0);
        assert_eq!(ah.generation(), gen_old + 1);
        assert_eq!(ah.dropped_stale(), 10);
        gate.open(); // first pulse resumes against its old snapshot
        ah.click_feedback(70, 6, true).unwrap();
        ah.release_feedback(70, true).unwrap();
        gate.wait_executed(3); // drain BEFORE drop (drop retires, not drains)
        drop(ah); // retire (empty) + bounded join
        let got = gate.executed();
        assert_eq!(
            got,
            vec![
                ("/dev/hidraw-old".to_string(), false, true, 64, 5, false),
                ("/dev/hidraw-new".to_string(), true, true, 70, 6, true),
                ("/dev/hidraw-new".to_string(), true, false, 70, 0, true),
            ],
            "stale backlog fired: {got:?}"
        );
    }

    #[test]
    fn haptic_update_route_repoints_pending_pulses() {
        // Same-session route correction (late actuator sibling): pending
        // pulses move to the corrected node with the generation unchanged
        // — nothing retired, nothing dropped stale. The pulse already
        // inside the send keeps its own (old) snapshot.
        let gate = GateHaptic::new_closed();
        let ah = AsyncHaptic::new(gate.clone());
        ah.retarget("/dev/hidraw-if1", false);
        let gen = ah.generation();
        ah.click_feedback(64, 5, false).unwrap();
        gate.wait_entered(1); // worker holds the first pulse inside the gate
        ah.click_feedback(64, 7, false).unwrap();
        ah.release_feedback(64, false).unwrap();
        assert_eq!(ah.pending(), 2);
        ah.update_route("/dev/hidraw-if2");
        assert_eq!(ah.generation(), gen, "route correction must not retire");
        assert_eq!(ah.pending(), 2, "route correction must not drop");
        assert_eq!(ah.dropped_stale(), 0);
        gate.open();
        gate.wait_executed(3); // drain BEFORE drop (drop retires, not drains)
        drop(ah); // retire (empty) + bounded join
        let got = gate.executed();
        assert_eq!(
            got,
            vec![
                ("/dev/hidraw-if1".to_string(), false, true, 64, 5, false),
                ("/dev/hidraw-if2".to_string(), false, true, 64, 7, false),
                ("/dev/hidraw-if2".to_string(), false, false, 64, 0, false),
            ],
            "pending pulses missed the corrected node: {got:?}"
        );
    }

    #[test]
    fn haptic_overflow_drops_newest_without_blocking() {
        // A wedged actuator must not stall the pump: the bounded queue
        // reports overflow as `Err` instead of blocking, drops the
        // incoming pulse, and keeps already-queued pairs complete.
        let gate = GateHaptic::new_closed();
        let ah = AsyncHaptic::new(gate.clone());
        ah.retarget("/dev/hidraw9", false);
        ah.click_feedback(64, 5, false).unwrap();
        gate.wait_entered(1);
        for _ in 0..HAPTIC_QUEUE_CAP {
            ah.click_feedback(64, 5, false).unwrap();
        }
        assert_eq!(ah.pending(), HAPTIC_QUEUE_CAP);
        let t0 = std::time::Instant::now();
        let err = ah
            .click_feedback(64, 5, false)
            .expect_err("full queue must report, not block");
        assert!(
            t0.elapsed() < std::time::Duration::from_millis(250),
            "enqueue blocked on full queue: {:?}",
            t0.elapsed()
        );
        assert!(
            format!("{err:#}").contains("queue full"),
            "overflow error should say so: {err:#}"
        );
        assert_eq!(ah.dropped_overflow(), 1);
        ah.release_feedback(64, false)
            .expect_err("full queue must report on up-pulses too");
        assert_eq!(ah.dropped_overflow(), 2);
        gate.open();
        gate.wait_executed(HAPTIC_QUEUE_CAP + 1); // drain BEFORE drop
        drop(ah); // retire (empty) + bounded join
        let got = gate.executed();
        assert_eq!(
            got.len(),
            HAPTIC_QUEUE_CAP + 1,
            "queued pulses stay complete; only the incoming drops: {got:?}"
        );
        assert!(
            got.iter().all(|e| e.2),
            "no up-pulse was queued, none may fire: {got:?}"
        );
    }

    #[test]
    fn haptic_down_overflow_owes_no_up() {
        // Pairing across the drop: an overflowed down-pulse reports
        // failure, so the proxy records it unfired and the later release
        // owes no up-pulse (no phantom release snap for a click that
        // never buzzed).
        let gate = GateHaptic::new_closed();
        let haptic = AsyncHaptic::new(gate.clone());
        haptic.retarget("/dev/hidraw9", false);
        haptic.click_feedback(1, 1, false).unwrap();
        gate.wait_entered(1);
        for _ in 0..HAPTIC_QUEUE_CAP {
            haptic.click_feedback(1, 1, false).unwrap();
        }
        let sinks = TestSinks::new();
        let px = ClickProxy::new(
            settings(true),
            FakeGrab::ok(),
            sinks.dual(),
            haptic,
            ScriptSource::new(vec![]),
        );
        let ev = ClickEvent {
            finger_count: 1,
            action: crate::config::ClickAction {
                raw: "empty".into(),
                stagger_ms: None,
                kind: crate::config::ActionKind::Empty,
            },
            haptic: 120,
            quiet: false,
            level: ClickLevel::First,
        };
        assert!(
            !px.fire_haptic(&ev, 120),
            "overflowed down must report unfired"
        );
        assert!(!px.pressed_haptic, "no up-pulse owed for a dropped down");
        gate.open();
    }

    #[test]
    fn haptic_overflow_press_release_owes_no_up() {
        // Full proxy path across a full queue: the press edge's down-pulse
        // is rejected, so the proxy owes nothing and the later release
        // enqueues no up-pulse (no phantom release snap, no extra drop).
        let gate = GateHaptic::new_closed();
        let ah = AsyncHaptic::new(gate.clone());
        ah.click_feedback(1, 1, false).unwrap();
        gate.wait_entered(1);
        for _ in 0..HAPTIC_QUEUE_CAP {
            ah.click_feedback(1, 1, false).unwrap();
        }
        let mut px = ClickProxy::new(
            settings(true),
            FakeGrab::ok(),
            TestSinks::new().dual(),
            ah,
            ScriptSource::new(vec![
                Ok(Some(frame(vec![finger(1, 200, 0, 0)], 0))),
                Ok(Some(frame(vec![finger(1, 10, 0, 0)], 0))),
                Ok(Some(frame(vec![], 0))),
            ]),
        );
        px.set_pump_ready(true);
        px.attach(&usb_ready("fixture"));
        let now = std::time::Instant::now();
        let cfg = Config::default();
        px.handle_frame_at(&cfg, now).unwrap();
        assert!(!px.pressed_haptic);
        assert_eq!(px.haptic.dropped_overflow(), 1);
        px.handle_frame_at(&cfg, spaced_at(now, 2)).unwrap();
        px.handle_frame_at(&cfg, spaced_at(now, 3)).unwrap();
        assert!(!px.pressed_haptic);
        assert_eq!(
            px.haptic.dropped_overflow(),
            1,
            "release must not attempt up for rejected down"
        );
        gate.open();
        gate.wait_executed(HAPTIC_QUEUE_CAP + 1);
        assert!(gate.executed().iter().all(|pulse| pulse.2));
    }

    #[test]
    fn haptic_panic_then_next_pulse_progresses() {
        // Panic containment through the real queue: a panicking pulse is
        // caught per pulse and the worker still runs later work in order.
        struct PanicOnce {
            panicked: std::sync::atomic::AtomicBool,
            calls: Mutex<Vec<(u8, u8, bool)>>,
            releases: Mutex<Vec<(u8, bool)>>,
        }
        impl HapticSender for PanicOnce {
            fn click_feedback(&self, _p: u8, _i: u8, _q: bool) -> Result<()> {
                Ok(())
            }
            fn release_feedback(&self, _d: u8, _q: bool) -> Result<()> {
                Ok(())
            }
            fn execute_down(
                &self,
                _target: &HapticTarget,
                pressure: u8,
                intensity: u8,
                quiet: bool,
            ) -> Result<()> {
                if !self
                    .panicked
                    .swap(true, std::sync::atomic::Ordering::SeqCst)
                {
                    panic!("test panic in haptic pulse");
                }
                self.calls
                    .lock()
                    .unwrap()
                    .push((pressure, intensity, quiet));
                Ok(())
            }
            fn execute_up(
                &self,
                _target: &HapticTarget,
                down_pressure: u8,
                quiet: bool,
            ) -> Result<()> {
                self.releases.lock().unwrap().push((down_pressure, quiet));
                Ok(())
            }
        }
        let rec = Arc::new(PanicOnce {
            panicked: std::sync::atomic::AtomicBool::new(false),
            calls: Mutex::new(Vec::new()),
            releases: Mutex::new(Vec::new()),
        });
        struct PanicOnceSender(Arc<PanicOnce>);
        impl HapticSender for PanicOnceSender {
            fn click_feedback(&self, _p: u8, _i: u8, _q: bool) -> Result<()> {
                Ok(())
            }
            fn release_feedback(&self, _d: u8, _q: bool) -> Result<()> {
                Ok(())
            }
            fn execute_down(&self, target: &HapticTarget, p: u8, i: u8, q: bool) -> Result<()> {
                self.0.execute_down(target, p, i, q)
            }
            fn execute_up(&self, target: &HapticTarget, d: u8, q: bool) -> Result<()> {
                self.0.execute_up(target, d, q)
            }
        }
        let ah = AsyncHaptic::new(PanicOnceSender(Arc::clone(&rec)));
        ah.click_feedback(64, 5, false).unwrap(); // panics inside worker
        ah.click_feedback(70, 6, true).unwrap();
        ah.release_feedback(70, true).unwrap();
        let t0 = std::time::Instant::now();
        loop {
            {
                let calls = rec.calls.lock().unwrap();
                let releases = rec.releases.lock().unwrap();
                if !calls.is_empty() && !releases.is_empty() {
                    break;
                }
            }
            assert!(
                t0.elapsed() < std::time::Duration::from_secs(5),
                "worker died after panicking pulse"
            );
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        assert!(rec.panicked.load(std::sync::atomic::Ordering::SeqCst));
        assert_eq!(
            rec.calls.lock().unwrap().as_slice(),
            &[(70, 6, true)],
            "only post-panic pulses run, in order"
        );
        assert_eq!(
            rec.releases.lock().unwrap().as_slice(),
            &[(70, true)],
            "up-pulse after panic still fires"
        );
        drop(ah);
    }

    #[test]
    fn haptic_shutdown_bounded_on_hung_actuator() {
        // A wedged firmware send (never returns until released) must not
        // hang teardown: `Drop` joins up to 500 ms, then detaches. The
        // entered gate proves the worker is inside the send (not draining),
        // so the measured Drop time exercises the join budget itself.
        struct HungHaptic {
            state: Mutex<bool>,
            cvar: Condvar,
        }
        impl HungHaptic {
            fn enter(&self) {
                let mut entered = self.state.lock().unwrap();
                *entered = true;
                self.cvar.notify_all();
                while *entered {
                    entered = self.cvar.wait(entered).unwrap();
                }
            }
        }
        impl HapticSender for HungHaptic {
            fn click_feedback(&self, _pressure: u8, _intensity: u8, _quiet: bool) -> Result<()> {
                self.enter();
                Ok(())
            }
            fn release_feedback(&self, _down_pressure: u8, _quiet: bool) -> Result<()> {
                self.enter();
                Ok(())
            }
        }
        let hung = Arc::new(HungHaptic {
            state: Mutex::new(false),
            cvar: Condvar::new(),
        });
        struct HungSender(Arc<HungHaptic>);
        impl HapticSender for HungSender {
            fn click_feedback(&self, _p: u8, _i: u8, _q: bool) -> Result<()> {
                Ok(())
            }
            fn release_feedback(&self, _d: u8, _q: bool) -> Result<()> {
                Ok(())
            }
            fn execute_down(&self, _t: &HapticTarget, p: u8, i: u8, q: bool) -> Result<()> {
                self.0.click_feedback(p, i, q)
            }
            fn execute_up(&self, _t: &HapticTarget, d: u8, q: bool) -> Result<()> {
                self.0.release_feedback(d, q)
            }
        }
        let ah = AsyncHaptic::new(HungSender(Arc::clone(&hung)));
        ah.click_feedback(64, 5, false).unwrap();
        // Explicit entered signal: the worker is inside the hung send.
        let t0 = std::time::Instant::now();
        while !*hung.state.lock().unwrap() {
            assert!(
                t0.elapsed() < std::time::Duration::from_secs(5),
                "haptic worker never entered hung send"
            );
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        let t0 = std::time::Instant::now();
        drop(ah);
        let elapsed = t0.elapsed();
        assert!(
            elapsed >= std::time::Duration::from_millis(450),
            "Drop returned before the join budget engaged: {elapsed:?}"
        );
        assert!(
            elapsed < std::time::Duration::from_millis(2000),
            "shutdown hung on wedged actuator: {elapsed:?}"
        );
        // Release the fixture gate so the detached worker exits cleanly
        // instead of lingering parked until process exit.
        {
            let mut entered = hung.state.lock().unwrap();
            *entered = false;
            hung.cvar.notify_all();
        }
    }

    #[test]
    fn haptic_fallback_uses_pinned_endpoint_across_pathname_reuse() {
        // F1: a frozen pathname is not a device identity. The worker pauses
        // at the fallback boundary (feature ioctl failed, before plain
        // write); the session retires and the kernel reuses the same
        // pathname for a replacement. The fallback must write the old fd
        // (the renamed old file), never the replacement at the same path.
        struct GateLog {
            state: Mutex<(bool, bool)>,
            cv: Condvar,
            // Fixture worker identity: the process-global logger sees every
            // test thread's records, and other haptic tests emit the same
            // prefix. Only the fixture worker's own thread may trip the gate;
            // otherwise a foreign warning could signal entered before this
            // worker reaches the fallback, and the retire would purge its
            // pulse.
            worker: std::thread::ThreadId,
        }
        impl log::Log for GateLog {
            fn enabled(&self, _: &log::Metadata<'_>) -> bool {
                true
            }
            fn log(&self, record: &log::Record<'_>) {
                if std::thread::current().id() != self.worker {
                    return;
                }
                if record
                    .args()
                    .to_string()
                    .starts_with("haptic feature report failed")
                {
                    let mut state = self.state.lock().unwrap();
                    state.0 = true;
                    self.cv.notify_all();
                    while !state.1 {
                        state = self.cv.wait(state).unwrap();
                    }
                }
            }
            fn flush(&self) {}
        }
        let dir = tempfile::tempdir().unwrap();
        let node = dir.path().join("reused-node");
        let old_path = dir.path().join("old-node");
        std::fs::write(&node, []).unwrap();
        let ah = AsyncHaptic::new(RealHaptic::new());
        // The worker is idle before the first enqueue; pin its identity
        // before installing the process-global logger.
        let worker = ah.worker.as_ref().unwrap().thread().id();
        let logger = Box::leak(Box::new(GateLog {
            state: Mutex::new((false, false)),
            cv: Condvar::new(),
            worker,
        }));
        log::set_logger(logger).unwrap();
        log::set_max_level(log::LevelFilter::Info);
        ah.retarget(node.to_str().unwrap(), false);
        ah.click_feedback(64, 5, false).unwrap();
        let state = logger.state.lock().unwrap();
        let (state, timeout) = logger
            .cv
            .wait_timeout_while(state, std::time::Duration::from_secs(5), |s| !s.0)
            .unwrap();
        assert!(
            !timeout.timed_out(),
            "worker did not reach fallback boundary"
        );
        drop(state);
        // Feature ioctl on the original fixture failed (ENOTTY); retire,
        // then simulate the kernel reusing the same hidraw pathname.
        ah.retire_session();
        std::fs::rename(&node, &old_path).unwrap();
        std::fs::write(&node, []).unwrap();
        ah.retarget(node.to_str().unwrap(), false);
        {
            let mut state = logger.state.lock().unwrap();
            state.1 = true;
            logger.cv.notify_all();
        }
        drop(ah); // Bounded join waits for the already-entered send.
        let replacement = std::fs::read(&node).unwrap();
        assert!(
            replacement.is_empty(),
            "old-session fallback must not write the replacement node: {replacement:02x?}"
        );
        let old_bytes = std::fs::read(&old_path).unwrap();
        assert_eq!(
            old_bytes,
            &vibration_report(64, 5, false)[1..],
            "in-flight pulse must complete only against the old endpoint"
        );
    }

    #[test]
    fn haptic_first_send_uses_pinned_endpoint_after_retire() {
        // F1 check-to-first-send race: the worker is descheduled after the
        // generation check but before the first send; retire + pathname
        // reuse must not redirect the in-flight pulse. The wrapper gates
        // inside `execute_down` before delegating to the pinned fd, which
        // simulates the deschedule. No pathname reopen occurs at send time
        // by construction (the fd was pinned at retarget).
        struct GateReal {
            state: Mutex<(bool, bool)>,
            cvar: Condvar,
            inner: RealHaptic,
        }
        impl HapticSender for GateReal {
            fn click_feedback(&self, _p: u8, _i: u8, _q: bool) -> Result<()> {
                Ok(())
            }
            fn release_feedback(&self, _d: u8, _q: bool) -> Result<()> {
                Ok(())
            }
            fn execute_down(
                &self,
                target: &HapticTarget,
                pressure: u8,
                intensity: u8,
                quiet: bool,
            ) -> Result<()> {
                {
                    let mut state = self.state.lock().unwrap();
                    state.0 = true;
                    self.cvar.notify_all();
                    while !state.1 {
                        state = self.cvar.wait(state).unwrap();
                    }
                }
                self.inner.execute_down(target, pressure, intensity, quiet)
            }
        }
        let gate = std::sync::Arc::new(GateReal {
            state: Mutex::new((false, false)),
            cvar: Condvar::new(),
            inner: RealHaptic::new(),
        });
        struct GateRealSender(std::sync::Arc<GateReal>);
        impl HapticSender for GateRealSender {
            fn click_feedback(&self, _p: u8, _i: u8, _q: bool) -> Result<()> {
                Ok(())
            }
            fn release_feedback(&self, _d: u8, _q: bool) -> Result<()> {
                Ok(())
            }
            fn execute_down(&self, target: &HapticTarget, p: u8, i: u8, q: bool) -> Result<()> {
                self.0.execute_down(target, p, i, q)
            }
            fn execute_up(&self, target: &HapticTarget, d: u8, q: bool) -> Result<()> {
                self.0.inner.execute_up(target, d, q)
            }
        }
        let dir = tempfile::tempdir().unwrap();
        let node = dir.path().join("reused-first-open");
        let old_path = dir.path().join("old-first-open");
        std::fs::write(&node, []).unwrap();
        let ah = AsyncHaptic::new(GateRealSender(std::sync::Arc::clone(&gate)));
        ah.retarget(node.to_str().unwrap(), false);
        ah.click_feedback(64, 5, false).unwrap();
        let t0 = std::time::Instant::now();
        while !gate.state.lock().unwrap().0 {
            assert!(
                t0.elapsed() < std::time::Duration::from_secs(5),
                "worker never entered first send"
            );
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        // Worker holds the in-flight pulse past its generation check.
        ah.retire_session();
        std::fs::rename(&node, &old_path).unwrap();
        std::fs::write(&node, []).unwrap();
        ah.retarget(node.to_str().unwrap(), false);
        {
            let mut state = gate.state.lock().unwrap();
            state.1 = true;
            gate.cvar.notify_all();
        }
        drop(ah);
        let replacement = std::fs::read(&node).unwrap();
        assert!(
            replacement.is_empty(),
            "retired first send must not touch the reused pathname: {replacement:02x?}"
        );
        let old_bytes = std::fs::read(&old_path).unwrap();
        assert_eq!(
            old_bytes,
            &vibration_report(64, 5, false)[1..],
            "in-flight first send completes only on the old endpoint"
        );
    }

    #[test]
    fn haptic_output_failure_retires_backlog_frame() {
        // F2 (frame path): a pointer emit failure ends the proxy session;
        // queued pulses must be purged and the generation bumped before
        // ownership is cleared. Only the already in-flight pulse may
        // complete (against its old snapshot); queued work never runs.
        let gate = GateHaptic::new_closed();
        let ah = AsyncHaptic::new(gate.clone());
        ah.retarget("old-session", false);
        ah.click_feedback(64, 5, false).unwrap();
        gate.wait_entered(1);
        ah.click_feedback(64, 5, false).unwrap();
        ah.release_feedback(64, false).unwrap();
        let before = ah.generation();
        let pointer = FaultUinput::new();
        let mut px = ClickProxy::new(
            settings(true),
            FakeGrab::ok(),
            DualSink::new(pointer.clone(), FaultUinput::new()),
            ah,
            ScriptSource::new(vec![Ok(Some(frame(vec![finger(1, 10, 0, 0)], 0)))]),
        );
        px.set_pump_ready(true);
        px.attach(&usb_ready("old-session"));
        pointer.fail(1);
        assert!(px.handle_frame(&Config::default()).is_err());
        assert!(!px.is_attached());
        let pending = px.haptic.pending();
        let after = px.haptic.generation();
        assert_eq!(
            pending, 0,
            "failed pointer session must purge queued haptics"
        );
        assert!(
            after > before,
            "failed pointer session must retire generation"
        );
        gate.open();
        gate.wait_executed(1);
        std::thread::sleep(std::time::Duration::from_millis(50));
        let executed = gate.executed();
        assert_eq!(
            executed.len(),
            1,
            "only the in-flight pulse may complete after failure: {executed:?}"
        );
        assert_eq!(executed[0].0, "old-session");
        drop(px);
    }

    #[test]
    fn haptic_idle_output_failure_retires_backlog() {
        // F2 (idle path): a staged-action idle poll whose trailing SYN
        // fails must retire the same way as a frame failure. Both paths
        // converge on `output_failed`.
        let gate = GateHaptic::new_closed();
        let ah = AsyncHaptic::new(gate.clone());
        ah.retarget("old-session", false);
        ah.click_feedback(64, 5, false).unwrap();
        gate.wait_entered(1);
        ah.click_feedback(64, 5, false).unwrap();
        ah.release_feedback(64, false).unwrap();
        let before = ah.generation();
        let pointer = FaultUinput::new();
        let extras = FaultUinput::new();
        let mut cfg = output_failure_config();
        cfg.click.click_1.stagger_ms = Some(100);
        let mut px = ClickProxy::new(
            settings(true),
            FakeGrab::ok(),
            DualSink::new(pointer.clone(), extras.clone()),
            ah,
            ScriptSource::new(vec![Ok(Some(frame(vec![finger(1, 200, 0, 0)], 0)))]),
        );
        px.set_pump_ready(true);
        px.attach(&usb_ready("old-session"));
        let now = std::time::Instant::now();
        px.handle_frame_at(&cfg, now).unwrap();
        assert!(px.pending_buttons.is_some());
        pointer.fail(2);
        assert!(px.handle_frame_at(&cfg, spaced_at(now, 2)).is_err());
        assert!(!px.is_attached());
        let pending = px.haptic.pending();
        let after = px.haptic.generation();
        assert_eq!(pending, 0, "idle failure must purge queued haptics");
        assert!(after > before, "idle failure must retire generation");
        gate.open();
        gate.wait_executed(1);
        std::thread::sleep(std::time::Duration::from_millis(50));
        assert_eq!(
            gate.executed().len(),
            1,
            "only the in-flight pulse may complete after idle failure"
        );
        drop(px);
    }

    #[test]
    fn haptic_extras_only_failure_keeps_pointer_session() {
        // Extras-only degradation must not retire the live pointer session
        // or its queued haptics: the pointer path stays attached and the
        // backlog survives.
        let gate = GateHaptic::new_closed();
        let ah = AsyncHaptic::new(gate.clone());
        ah.retarget("/dev/hidraw9", false);
        ah.click_feedback(64, 5, false).unwrap();
        gate.wait_entered(1);
        ah.click_feedback(64, 5, false).unwrap();
        let gen = ah.generation();
        let pointer = FaultUinput::new();
        let extras = FaultUinput::new();
        let mut px = ClickProxy::new(
            settings(true),
            FakeGrab::ok(),
            DualSink::new(pointer.clone(), extras.clone()),
            ah,
            ScriptSource::new(vec![Ok(Some(frame(vec![finger(1, 200, 0, 0)], 0)))]),
        );
        px.set_pump_ready(true);
        px.attach(&usb_ready("/dev/hidraw1"));
        extras.fail(1);
        let cfg = output_failure_config();
        let now = std::time::Instant::now();
        px.handle_frame_at(&cfg, now).unwrap();
        assert!(
            px.is_attached(),
            "extras-only failure must keep pointer session"
        );
        assert!(!px.is_passthrough());
        // The press edge above enqueues its own down-pulse alongside the
        // pre-queued backlog (worker still holds the first): extras
        // degradation must preserve all of it, never purge or retire.
        assert_eq!(
            px.haptic.pending(),
            2,
            "extras failure must not purge haptics"
        );
        assert_eq!(
            px.haptic.generation(),
            gen,
            "extras failure must not retire"
        );
        gate.open();
        gate.wait_executed(3);
        drop(px);
    }

    #[test]
    fn haptic_route_prepared_commits_same_session() {
        // F3 prepared route path: prepare (open) outside all live locks,
        // then commit generation-fenced without I/O. Pending pulses move
        // to the corrected endpoint; generation unchanged; in-flight keeps
        // the old endpoint.
        let dir = tempfile::tempdir().unwrap();
        let old = dir.path().join("old");
        let corrected = dir.path().join("corrected");
        std::fs::write(&old, []).unwrap();
        std::fs::write(&corrected, []).unwrap();
        let gate = GateHaptic::new_closed();
        let ah = AsyncHaptic::new(gate.clone());
        ah.retarget(old.to_str().unwrap(), false);
        let gen = ah.generation();
        ah.click_feedback(64, 5, false).unwrap();
        gate.wait_entered(1);
        ah.click_feedback(64, 7, false).unwrap();
        ah.release_feedback(64, false).unwrap();
        assert_eq!(ah.pending(), 2);
        // Prepare outside any proxy/queue lock (fast here; gated slow-open
        // placement is covered by the io evidence run).
        let (_, bt) = ah.current_routing();
        let prepared = HapticTarget::prepare(corrected.to_str().unwrap(), bt);
        assert!(ah.update_route_prepared(prepared, gen));
        assert_eq!(ah.generation(), gen, "route commit must not retire");
        assert_eq!(ah.pending(), 2, "route commit must not drop");
        gate.open();
        gate.wait_executed(3);
        drop(ah);
        let got = gate.executed();
        assert_eq!(
            got,
            vec![
                (old.to_str().unwrap().to_string(), false, true, 64, 5, false),
                (
                    corrected.to_str().unwrap().to_string(),
                    false,
                    true,
                    64,
                    7,
                    false
                ),
                (
                    corrected.to_str().unwrap().to_string(),
                    false,
                    false,
                    64,
                    0,
                    false
                ),
            ],
            "prepared route missed: {got:?}"
        );
    }

    #[test]
    fn haptic_route_prepared_fence_discards_stale() {
        // F3 generation fence: a correction prepared before a retire must
        // not clobber the new session's endpoint, generation, or mode.
        let dir = tempfile::tempdir().unwrap();
        let old = dir.path().join("old");
        let late = dir.path().join("late");
        let fresh = dir.path().join("fresh");
        for node in [&old, &late, &fresh] {
            std::fs::write(node, []).unwrap();
        }
        let ah = AsyncHaptic::new(RealHaptic::new());
        ah.retarget(old.to_str().unwrap(), false);
        let baseline = ah.generation();
        let (_, bt) = ah.current_routing();
        assert!(!bt);
        let prepared = HapticTarget::prepare(late.to_str().unwrap(), bt);
        ah.retire_session();
        ah.retarget(fresh.to_str().unwrap(), true);
        let generation = ah.generation();
        assert!(!ah.update_route_prepared(prepared, baseline));
        assert_eq!(ah.generation(), generation);
        // Prepared endpoint discarded; new session intact with its own fd.
        let target = ah.shared.state.lock().unwrap().target.clone();
        assert_eq!(target.node, fresh.to_str().unwrap());
        assert!(target.bluetooth);
        use std::os::unix::fs::MetadataExt;
        assert_eq!(
            target.try_clone_file().unwrap().metadata().unwrap().ino(),
            std::fs::metadata(&fresh).unwrap().ino()
        );
        drop(ah);
    }

    #[test]
    fn disposal_pool_startup_failure_degrades_explicitly() {
        // F3 failure path: a cleanup worker that cannot start must deny
        // admission deterministically (mock spawner, no host limit
        // changes). Queue/pump/Drop work stays bounded on the degraded
        // file-less target; nothing closes synchronously on the caller.
        let failing =
            |_job: Box<dyn FnOnce() + Send + 'static>| -> std::io::Result<std::thread::JoinHandle<()>> {
                Err(std::io::Error::new(
                    std::io::ErrorKind::ResourceBusy,
                    "mock closer spawn failure",
                ))
            };
        assert!(
            DisposalPool::<std::fs::File>::start(2, failing).is_err(),
            "worker setup failure must be an explicit error, never silent"
        );
        // Degraded target keeps routing identity but acquires nothing.
        let target = HapticTarget::degraded("/dev/hidraw9", false);
        assert_eq!(target.node, "/dev/hidraw9");
        assert!(target.try_clone_file().is_none());
        // Queue lifecycle on file-less targets stays prompt and bounded.
        let gate = GateHaptic::new_closed();
        let ah = AsyncHaptic::new(gate.clone());
        ah.retarget_prepared(target);
        ah.click_feedback(64, 5, false).unwrap();
        gate.wait_entered(1);
        let t0 = std::time::Instant::now();
        ah.retire_session();
        assert_eq!(ah.pending(), 0);
        gate.open();
        gate.wait_executed(1);
        drop(ah);
        assert!(
            t0.elapsed() < std::time::Duration::from_secs(5),
            "degraded retire/Drop must stay bounded"
        );
    }

    #[test]
    fn disposal_pool_capacity_bounds_outstanding() {
        // F3 capacity: reserve-before-open bounds simultaneous items;
        // handoff never blocks even while the closer is stuck in a slow
        // close (mock slow Drop, no shim); slots recycle after the close.
        struct SlowClose {
            gate: Arc<(Mutex<bool>, Condvar)>,
        }
        impl Drop for SlowClose {
            fn drop(&mut self) {
                let mut open = self.gate.0.lock().unwrap();
                while !*open {
                    open = self.gate.1.wait(open).unwrap();
                }
            }
        }
        let pool = DisposalPool::start(2, |job| {
            std::thread::Builder::new()
                .name("ltd-test-closer".into())
                .spawn(job)
        })
        .unwrap();
        let gate = Arc::new((Mutex::new(false), Condvar::new()));
        let r1 = pool.reserve();
        let r2 = pool.reserve();
        assert!(r1.is_some() && r2.is_some());
        assert!(
            pool.reserve().is_none(),
            "third reservation past cap must deny, never block"
        );
        r1.unwrap().disarm();
        r2.unwrap().disarm();
        // Nonblocking handoff of both reserved items while closes stall.
        let t0 = std::time::Instant::now();
        pool.handoff(SlowClose {
            gate: Arc::clone(&gate),
        });
        pool.handoff(SlowClose {
            gate: Arc::clone(&gate),
        });
        assert!(
            t0.elapsed() < std::time::Duration::from_millis(250),
            "handoff must not wait for the stuck closer"
        );
        assert_eq!(pool.live.load(Ordering::SeqCst), 2);
        // Stuck closer holds both; capacity stays denied (bounded).
        std::thread::sleep(std::time::Duration::from_millis(50));
        assert!(pool.reserve().is_none());
        assert_eq!(pool.live.load(Ordering::SeqCst), 2);
        // Release: the single worker closes both, slots recycle.
        {
            let mut open = gate.0.lock().unwrap();
            *open = true;
            gate.1.notify_all();
        }
        let t0 = std::time::Instant::now();
        while pool.live.load(Ordering::SeqCst) != 0 {
            assert!(
                t0.elapsed() < std::time::Duration::from_secs(5),
                "slots must recycle after the close"
            );
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        assert!(pool.reserve().is_some());
    }

    #[test]
    fn async_haptic_explicit_shutdown_drains_and_is_idempotent() {
        // Orderly shutdown drains the queued trailing pulses before
        // returning, then tolerates repeats and drop without hanging.
        let rec = Arc::new(FakeHaptic::new());
        let mut ah = AsyncHaptic::new(SharedHaptic(Arc::clone(&rec)));
        ah.click_feedback(64, 5, false).unwrap();
        ah.release_feedback(64, false).unwrap();
        ah.shutdown(std::time::Duration::from_secs(5));
        assert_eq!(rec.calls.lock().unwrap().as_slice(), &[(64, 5, false)]);
        assert_eq!(rec.releases.lock().unwrap().as_slice(), &[(64, false)]);
        // Idempotent: worker already gone, returns immediately.
        ah.shutdown(std::time::Duration::from_millis(10));
        drop(ah);
    }

    #[test]
    fn async_haptic_shutdown_is_bounded_when_worker_stalls() {
        // A wedged actuator (firmware round-trip never returns) must not
        // hang shutdown: the worker detaches after the bound.
        struct StallHaptic {
            gate: Arc<(Mutex<bool>, std::sync::Condvar)>,
        }
        impl HapticSender for StallHaptic {
            fn click_feedback(&self, _: u8, _: u8, _: bool) -> Result<()> {
                let (lock, cvar) = &*self.gate;
                let mut open = lock.lock().unwrap();
                while !*open {
                    open = cvar.wait(open).unwrap();
                }
                Ok(())
            }
            fn release_feedback(&self, _: u8, _: bool) -> Result<()> {
                Ok(())
            }
        }
        let gate = Arc::new((Mutex::new(false), std::sync::Condvar::new()));
        let mut ah = AsyncHaptic::new(StallHaptic {
            gate: Arc::clone(&gate),
        });
        ah.click_feedback(64, 5, false).unwrap();
        let t0 = std::time::Instant::now();
        ah.shutdown(std::time::Duration::from_millis(50));
        assert!(
            t0.elapsed() < std::time::Duration::from_secs(5),
            "shutdown must give up on a stalled worker: {:?}",
            t0.elapsed()
        );
        // Release the detached worker so it exits on its own.
        let (lock, cvar) = &*gate;
        *lock.lock().unwrap() = true;
        cvar.notify_all();
    }

    #[test]
    fn async_haptic_post_shutdown_calls_fail_fast_without_actuator_work() {
        // Closed queue admits nothing: after shutdown, calls return promptly
        // with an error (the proxy owes no up-pulse) and no actuator send
        // runs — no synchronous fallback on the caller, no reopen. Input
        // callbacks must never run firmware I/O.
        let rec = Arc::new(FakeHaptic::new());
        let mut ah = AsyncHaptic::new(SharedHaptic(Arc::clone(&rec)));
        ah.click_feedback(64, 5, false).unwrap();
        ah.release_feedback(64, false).unwrap();
        ah.shutdown(std::time::Duration::from_secs(5));
        let calls = rec.calls.lock().unwrap().len();
        let releases = rec.releases.lock().unwrap().len();
        let t0 = std::time::Instant::now();
        assert!(ah.click_feedback(70, 6, true).is_err());
        assert!(ah.release_feedback(70, true).is_err());
        assert!(
            t0.elapsed() < std::time::Duration::from_secs(1),
            "post-shutdown calls must not block: {:?}",
            t0.elapsed()
        );
        assert_eq!(
            rec.calls.lock().unwrap().len(),
            calls,
            "no actuator down-pulse may run after shutdown"
        );
        assert_eq!(
            rec.releases.lock().unwrap().len(),
            releases,
            "no actuator release may run after shutdown"
        );
    }

    #[test]
    fn drop_retires_queued_backlog_before_close() {
        // Drop retires the session first: pulses already queued behind a
        // gated in-flight send never execute after Drop. Only the
        // already-admitted in-flight operation may finish, on its pinned
        // old endpoint.
        let gate = GateHaptic::new_closed();
        let ah = AsyncHaptic::new(gate.clone());
        ah.click_feedback(64, 5, false).unwrap(); // picked up, blocks in gate
        gate.wait_entered(1);
        ah.click_feedback(70, 6, true).unwrap(); // queued backlog
        ah.release_feedback(70, true).unwrap(); // queued backlog
        let t0 = std::time::Instant::now();
        drop(ah); // retire purges the backlog, then close + bounded join
        assert!(
            t0.elapsed() >= std::time::Duration::from_millis(400),
            "Drop returned before the join budget engaged: {:?}",
            t0.elapsed()
        );
        assert!(
            t0.elapsed() < std::time::Duration::from_secs(5),
            "Drop must not hang on a wedged actuator: {:?}",
            t0.elapsed()
        );
        // Release the gate: the detached worker may still finish the one
        // in-flight send, but the retired backlog must never run.
        gate.open();
        gate.wait_executed(1);
        std::thread::sleep(std::time::Duration::from_millis(100));
        let executed = gate.executed();
        assert_eq!(
            executed.len(),
            1,
            "only the admitted in-flight pulse may finish after Drop: {executed:?}"
        );
        assert_eq!(
            executed[0].3, 64,
            "wrong pulse survived retire: {executed:?}"
        );
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
                (EV_ABS, ABS_X, 10),
                (EV_ABS, ABS_Y, 20),
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
                (EV_ABS, ABS_X, x0),
                (EV_ABS, ABS_Y, y0),
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
                (EV_ABS, ABS_X, 0),
                (EV_ABS, ABS_Y, 0),
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
            click: crate::config::ClickConfig {
                click_1: "empty".into(),
                ..Config::default().click
            },
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
            click: crate::config::ClickConfig {
                click_1: "none".into(),
                ..Config::default().click
            },
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
            click: crate::config::ClickConfig {
                click_1: "ctrl+c".into(),
                ..Config::default().click
            },
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
            click: crate::config::ClickConfig {
                click_1: "ctrl+refresh".into(),
                ..Config::default().click
            },
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
        let mut off = armed;
        off.force.force_offset = 0;
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
        // shared token table. This test fails on any drift: every doc row
        // must match the live map, and the live mapped set (shared token
        // universe plus broad sweeps) must equal the doc set, so additions
        // on either side fail here first.
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
        // Candidate universe: the shared token universe (enumerated,
        // not hand-listed) + all printable single chars + f0..=f99.
        // A newly mapped token on either side lands here and breaks the
        // set equality below until the doc is regenerated.
        let mut candidates: Vec<String> = crate::action_tokens::all_known_tokens();
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
            slide: crate::config::SlideConfig {
                slide_1: "left".into(),
                ..Config::default().slide
            },
            ..Config::default()
        }
    }

    fn five_slide_cfg() -> Config {
        let mut slide = std::collections::BTreeMap::new();
        slide.insert(5u8, 1u16);
        Config {
            enhanced_clicks: true,
            spoof: crate::config::SpoofConfig {
                slide,
                ..Default::default()
            },
            device: crate::config::DeviceConfig { polling_rate: 110 },
            pointer: crate::config::PointerConfig { speed: 100 },
            click: crate::config::ClickConfig {
                sensitivity: 30,
                margin: 5,
                click_4: "none".into(),
                click_5: "none".into(),
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
                force_1: "super+left".into(),
                force_2: "back".into(),
                force_3: "forward".into(),
                force_4: "alt+f4".into(),
                force_5: "none".into(),
                transition_mode: "cut".into(),
                ..Config::default().force
            },
            slide: crate::config::SlideConfig {
                slide_5: "super+left".into(),
                ..Config::default().slide
            },
            ..Config::default()
        }
    }

    fn slide_report(n: u8, pressure: u8, x: i32) -> Result<Option<TouchFrame>> {
        Ok(Some(frame(
            (1..=n).map(|id| finger(id, pressure, x, 20)).collect(),
            u8::from(pressure >= 30),
        )))
    }

    fn ms_after(t0: std::time::Instant, ms: u64) -> std::time::Instant {
        t0 + std::time::Duration::from_millis(ms)
    }

    fn assert_no_haptics(haptic: &FakeHaptic) {
        assert!(haptic.calls.lock().unwrap().is_empty());
        assert!(haptic.releases.lock().unwrap().is_empty());
    }

    #[test]
    fn idle_release_finishes_quick_mouse_and_keyboard_actions() {
        for action in ["left", "right", "ctrl", "ctrl+c", "super+left"] {
            for lift in [
                vec![],
                vec![FingerContact {
                    contact: false,
                    ..finger(1, 0, 0, 0)
                }],
                vec![finger(1, 10, 0, 0)],
            ] {
                for deadline_poll in [30, 1000] {
                    let mut cfg = Config::default();
                    cfg.click.click_1 = action.into();
                    let sinks = TestSinks::new();
                    let haptic = Arc::new(FakeHaptic::new());
                    let mut px = drive_ready_proxy(
                        &sinks,
                        &haptic,
                        vec![
                            Ok(Some(frame(vec![finger(1, 100, 0, 0)], 0))),
                            Ok(Some(frame(lift.clone(), 0))),
                        ],
                    );
                    let t0 = std::time::Instant::now();
                    px.handle_frame_at(&cfg, t0).unwrap();
                    px.handle_frame_at(&cfg, ms_after(t0, 10)).unwrap();
                    let before_idle = wire_events(&sinks.pointer);
                    px.handle_frame_at(&cfg, ms_after(t0, 29)).unwrap();
                    assert!(
                        px.machine.active().is_some(),
                        "release before 30 ms: {action}"
                    );
                    assert_eq!(
                        wire_events(&sinks.pointer),
                        before_idle,
                        "idle must not replay touch slots"
                    );
                    assert!(px
                        .handle_frame_at(&cfg, ms_after(t0, deadline_poll))
                        .unwrap()
                        .is_none());
                    assert!(
                        px.machine.active().is_none(),
                        "stuck quick action: {action}"
                    );
                    for token in action.split('+') {
                        let (ty, code) = key_token_to_code(token).unwrap();
                        let sink = if matches!(split_route(ty, code), SplitRoute::Pointer) {
                            &sinks.pointer
                        } else {
                            &sinks.extras
                        };
                        assert_eq!(key_values(sink, code), vec![1, 0], "{action}, {token}");
                    }
                    let ended = (wire_events(&sinks.pointer), wire_events(&sinks.extras));
                    px.handle_frame_at(&cfg, ms_after(t0, 2000)).unwrap();
                    assert_eq!(
                        (wire_events(&sinks.pointer), wire_events(&sinks.extras)),
                        ended
                    );
                    assert_eq!(haptic.calls.lock().unwrap().len(), 1);
                    assert_eq!(haptic.releases.lock().unwrap().len(), 1);
                }
            }
        }
    }

    #[test]
    fn idle_release_never_invents_contact_or_press_edges() {
        // A single deep report cannot become a new force edge on an idle
        // poll; steady click/slide contacts also remain held indefinitely.
        for pressure in [10, 100, 200] {
            let mut cfg = Config::default();
            cfg.slide.slide_1 = "alt".into();
            cfg.click.click_1 = "ctrl".into();
            cfg.force.force_offset = 100;
            cfg.force.force_1 = "super".into();
            let sinks = TestSinks::new();
            let haptic = Arc::new(FakeHaptic::new());
            let mut px = drive_ready_proxy(
                &sinks,
                &haptic,
                vec![Ok(Some(frame(vec![finger(1, pressure, 0, 0)], 0)))],
            );
            let t0 = std::time::Instant::now();
            px.handle_frame_at(&cfg, t0).unwrap();
            let held = (wire_events(&sinks.pointer), wire_events(&sinks.extras));
            for ms in [30, 1000] {
                assert!(px
                    .handle_frame_at(&cfg, ms_after(t0, ms))
                    .unwrap()
                    .is_none());
            }
            assert_eq!(
                (wire_events(&sinks.pointer), wire_events(&sinks.extras)),
                held
            );
            assert!(px.machine.force_active().is_none());
            if pressure == 10 {
                assert!(px.machine.slide_active().is_some());
                assert_no_haptics(&haptic);
            } else {
                assert!(px.machine.active().is_some());
                assert_eq!(haptic.calls.lock().unwrap().len(), 1);
                assert!(haptic.releases.lock().unwrap().is_empty());
            }
        }
    }

    #[test]
    fn idle_release_keeps_press_debounce_and_requires_fresh_report_to_retry() {
        let cfg = Config::default();
        let sinks = TestSinks::new();
        let haptic = Arc::new(FakeHaptic::new());
        let mut px = drive_ready_proxy(
            &sinks,
            &haptic,
            vec![slide_report(1, 100, 0), slide_report(0, 0, 0)],
        );
        let t0 = std::time::Instant::now();
        px.handle_frame_at(&cfg, t0).unwrap();
        px.handle_frame_at(&cfg, ms_after(t0, 10)).unwrap();
        px.handle_frame_at(&cfg, ms_after(t0, 30)).unwrap();
        px.replace_source(ScriptSource::new(vec![slide_report(1, 100, 0)]));
        px.handle_frame_at(&cfg, ms_after(t0, 79)).unwrap();
        assert!(px.machine.active().is_none());
        for ms in [80, 1000] {
            px.handle_frame_at(&cfg, ms_after(t0, ms)).unwrap();
        }
        assert_eq!(key_values(&sinks.pointer, BTN_LEFT), vec![1, 0]);
        px.replace_source(ScriptSource::new(vec![slide_report(1, 100, 0)]));
        px.handle_frame_at(&cfg, ms_after(t0, 1001)).unwrap();
        assert_eq!(key_values(&sinks.pointer, BTN_LEFT), vec![1, 0, 1]);
    }

    #[test]
    fn idle_release_obeys_new_pressure_at_deadline_and_physical_hold() {
        for recovered in [false, true] {
            let cfg = Config::default();
            let sinks = TestSinks::new();
            let haptic = Arc::new(FakeHaptic::new());
            let mut px = drive_ready_proxy(
                &sinks,
                &haptic,
                vec![slide_report(1, 100, 0), Ok(Some(frame(vec![], 1)))],
            );
            let t0 = std::time::Instant::now();
            px.handle_frame_at(&cfg, t0).unwrap();
            px.handle_frame_at(&cfg, ms_after(t0, 10)).unwrap();
            if recovered {
                px.replace_source(ScriptSource::new(vec![slide_report(1, 100, 0)]));
            }
            px.handle_frame_at(&cfg, ms_after(t0, 30)).unwrap();
            px.handle_frame_at(&cfg, ms_after(t0, 1000)).unwrap();
            assert_eq!(px.machine.active().is_some(), recovered);
            assert_eq!(key_values(&sinks.pointer, BTN_LEFT), vec![1]);
            px.replace_source(ScriptSource::new(vec![slide_report(0, 0, 0)]));
            px.handle_frame_at(&cfg, ms_after(t0, 1001)).unwrap();
            assert_eq!(key_values(&sinks.pointer, BTN_LEFT), vec![1, 0]);
            assert_eq!(haptic.calls.lock().unwrap().len(), 1);
            assert_eq!(haptic.releases.lock().unwrap().len(), 1);
        }
    }

    #[test]
    fn idle_release_cancels_pending_button_before_equal_or_overdue_deadline() {
        for stagger in [30, 50] {
            for poll in [30, 1000] {
                let cfg: Config = toml::from_str(&format!(
                    r#"schema_version = 1
[click]
click_1 = {{ action = "super+left", stagger_ms = {stagger} }}
[slide]
slide_1 = "alt""#
                ))
                .unwrap();
                let sinks = TestSinks::new();
                let haptic = Arc::new(FakeHaptic::new());
                // Contact remains, pressure releases: the pending combo is
                // retained through normal debounce, then cancelled on idle.
                let mut px = drive_ready_proxy(
                    &sinks,
                    &haptic,
                    vec![slide_report(1, 100, 0), slide_report(1, 10, 0)],
                );
                let t0 = std::time::Instant::now();
                px.handle_frame_at(&cfg, t0).unwrap();
                px.handle_frame_at(&cfg, ms_after(t0, 10)).unwrap();
                px.handle_frame_at(&cfg, ms_after(t0, 29)).unwrap();
                assert!(px.pending_buttons.is_some());
                assert_eq!(key_values(&sinks.extras, 125), vec![1]);
                px.handle_frame_at(&cfg, ms_after(t0, poll)).unwrap();
                px.handle_frame_at(&cfg, ms_after(t0, 2000)).unwrap();
                assert_eq!(key_values(&sinks.extras, 125), vec![1, 0]);
                assert!(key_values(&sinks.pointer, BTN_LEFT).is_empty());
                assert!(
                    key_values(&sinks.extras, 56).is_empty(),
                    "idle cannot start slide from stale contact"
                );
                assert!(px.pending_buttons.is_none());
                assert!(px.machine.active().is_none());
                assert_eq!(haptic.releases.lock().unwrap().len(), 1);
            }
        }
    }

    #[test]
    fn idle_release_finishes_force_dip_before_pending_cut_or_combo_press() {
        for mode in ["cut", "continuous"] {
            for stage_modifiers in [false, true] {
                let stagger = if mode == "cut" { 25 } else { 30 };
                let cfg: Config = toml::from_str(&format!(
                    r#"schema_version = 1
[click]
click_1 = "ctrl"
[force]
force_offset = 100
transition_mode = "{mode}"
force_1 = {{ action = "super+left", stagger_ms = {stagger} }}"#
                ))
                .unwrap();
                let sinks = TestSinks::new();
                let haptic = Arc::new(FakeHaptic::new());
                let mut px = drive_ready_proxy(
                    &sinks,
                    &haptic,
                    vec![slide_report(1, 100, 0), slide_report(1, 200, 0)],
                );
                let t0 = std::time::Instant::now();
                px.handle_frame_at(&cfg, t0).unwrap();
                px.handle_frame_at(&cfg, ms_after(t0, 10)).unwrap();
                if stage_modifiers {
                    px.handle_frame_at(&cfg, ms_after(t0, 15)).unwrap();
                }
                px.replace_source(ScriptSource::new(vec![slide_report(1, 100, 0)]));
                px.handle_frame_at(&cfg, ms_after(t0, if stage_modifiers { 16 } else { 11 }))
                    .unwrap();
                assert!(px.machine.force_active().is_some());
                // The force hold window ends at 10 + 30. When staged, its
                // button deadline is also 40; release must win that tie.
                if stage_modifiers {
                    px.handle_frame_at(&cfg, ms_after(t0, 39)).unwrap();
                    assert!(px.machine.force_active().is_some());
                    assert!(key_values(&sinks.pointer, BTN_LEFT).is_empty());
                }
                px.handle_frame_at(&cfg, ms_after(t0, 40)).unwrap();
                assert!(px.machine.force_active().is_none());
                assert!(px.machine.active().is_some());
                assert!(px.pending_force.is_none() && px.pending_buttons.is_none());
                assert!(key_values(&sinks.pointer, BTN_LEFT).is_empty());
                let expected_ctrl = if mode == "cut" {
                    vec![1, 0, 1]
                } else {
                    vec![1]
                };
                assert_eq!(key_values(&sinks.extras, 29), expected_ctrl);
                let expected_super = if mode == "continuous" || stage_modifiers {
                    vec![1, 0]
                } else {
                    vec![]
                };
                assert_eq!(key_values(&sinks.extras, 125), expected_super);
                let held = (wire_events(&sinks.pointer), wire_events(&sinks.extras));
                px.handle_frame_at(&cfg, ms_after(t0, 1000)).unwrap();
                assert_eq!(
                    (wire_events(&sinks.pointer), wire_events(&sinks.extras)),
                    held
                );
                px.replace_source(ScriptSource::new(vec![slide_report(0, 0, 0)]));
                px.handle_frame_at(&cfg, ms_after(t0, 1001)).unwrap();
                assert_eq!(key_values(&sinks.extras, 29).last(), Some(&0));
                assert_eq!(
                    haptic.releases.lock().unwrap().len(),
                    haptic.calls.lock().unwrap().len()
                );
            }
        }
    }

    #[test]
    fn binding_timing_force_disarm_at_deadline_preserves_first_pending_owner() {
        for shared in [false, true] {
            let mut cfg: Config = toml::from_str(&format!(
                r#"
                schema_version = 1
                [click]
                sensitivity = 30
                click_1 = {{ action = "super+left", stagger_ms = 100 }}
                [force]
                force_offset = 130
                transition_mode = "continuous"
                force_1 = {{ action = "ctrl+{}", stagger_ms = 50 }}
            "#,
                if shared { "left" } else { "right" }
            ))
            .unwrap();
            let sinks = TestSinks::new();
            let haptic = Arc::new(FakeHaptic::new());
            let mut px = drive_ready_proxy(
                &sinks,
                &haptic,
                vec![
                    slide_report(1, 60, 0),
                    slide_report(1, 200, 0),
                    slide_report(1, 200, 10),
                ],
            );
            let t0 = std::time::Instant::now();
            px.handle_frame_at(&cfg, t0).unwrap();
            px.handle_frame_at(&cfg, ms_after(t0, 10)).unwrap();
            cfg.force.force_offset = 0;
            px.handle_frame_at(&cfg, ms_after(t0, 60)).unwrap();
            px.handle_frame_at(&cfg, ms_after(t0, 99)).unwrap();
            assert!(key_values(&sinks.pointer, BTN_LEFT).is_empty());
            assert!(key_values(&sinks.pointer, BTN_RIGHT).is_empty());
            assert_eq!(key_values(&sinks.extras, 29), vec![1, 0]);
            assert_eq!(key_values(&sinks.extras, 125), vec![1]);
            px.handle_frame_at(&cfg, ms_after(t0, 100)).unwrap();
            assert_eq!(key_values(&sinks.pointer, BTN_LEFT), vec![1]);
            assert!(key_values(&sinks.pointer, BTN_RIGHT).is_empty());
            px.replace_source(ScriptSource::new(vec![slide_report(0, 0, 0)]));
            px.handle_frame_at(&cfg, ms_after(t0, 110)).unwrap();
            assert_eq!(key_values(&sinks.extras, 125), vec![1, 0]);
            assert_eq!(key_values(&sinks.pointer, BTN_LEFT), vec![1, 0]);
        }
    }

    #[test]
    fn binding_timing_cut_cancels_first_pending_buttons_before_zero_override_floor() {
        let cfg: Config = toml::from_str(
            r#"
            schema_version = 1
            [click]
            sensitivity = 30
            click_1 = { action = "super+left", stagger_ms = 50 }
            [force]
            force_offset = 130
            transition_mode = "cut"
            force_1 = { action = "ctrl+right", stagger_ms = 0 }
        "#,
        )
        .unwrap();
        let sinks = TestSinks::new();
        let haptic = Arc::new(FakeHaptic::new());
        let mut px = drive_ready_proxy(
            &sinks,
            &haptic,
            vec![slide_report(1, 60, 0), slide_report(1, 200, 0)],
        );
        let t0 = std::time::Instant::now();
        for ms in [0, 10, 14] {
            px.handle_frame_at(&cfg, ms_after(t0, ms)).unwrap();
        }
        assert!(key_values(&sinks.pointer, BTN_RIGHT).is_empty());
        assert_eq!(key_values(&sinks.extras, 125), vec![1, 0]);
        px.handle_frame_at(&cfg, ms_after(t0, 15)).unwrap();
        assert_eq!(key_values(&sinks.pointer, BTN_RIGHT), vec![1]);
        assert_eq!(key_values(&sinks.extras, 29), vec![1]);
        px.handle_frame_at(&cfg, ms_after(t0, 50)).unwrap();
        assert!(
            key_values(&sinks.pointer, BTN_LEFT).is_empty(),
            "old first-level deadline was cancelled by cut"
        );
        px.replace_source(ScriptSource::new(vec![slide_report(1, 60, 0)]));
        px.handle_frame_at(&cfg, ms_after(t0, 60)).unwrap();
        px.handle_frame_at(&cfg, ms_after(t0, 109)).unwrap();
        assert!(key_values(&sinks.pointer, BTN_LEFT).is_empty());
        px.handle_frame_at(&cfg, ms_after(t0, 110)).unwrap();
        assert_eq!(
            key_values(&sinks.pointer, BTN_LEFT),
            vec![1],
            "restored first binding gets a fresh override deadline"
        );
        px.replace_source(ScriptSource::new(vec![slide_report(0, 0, 0)]));
        px.handle_frame_at(&cfg, ms_after(t0, 120)).unwrap();
        assert_eq!(key_values(&sinks.pointer, BTN_LEFT), vec![1, 0]);
        assert_eq!(key_values(&sinks.pointer, BTN_RIGHT), vec![1, 0]);
        assert_eq!(haptic.calls.lock().unwrap().len(), 2);
        assert_eq!(haptic.releases.lock().unwrap().len(), 2);
    }

    #[test]
    fn binding_timing_session_teardown_cancels_explicit_overrides() {
        for level in ["click", "force", "slide"] {
            for teardown in ["disarm", "detach", "disconnect"] {
                let mut cfg = Config::default();
                cfg.click.sensitivity = 30;
                cfg.force.force_offset = 130;
                let binding = crate::config::ActionBinding {
                    action: "super+left".into(),
                    stagger_ms: Some(50),
                };
                match level {
                    "click" => cfg.click.click_1 = binding,
                    "force" => {
                        cfg.click.click_1 = "none".into();
                        cfg.force.force_1 = binding;
                    }
                    _ => {
                        cfg.click.click_1 = "none".into();
                        cfg.slide.slide_1 = binding;
                    }
                }
                let sinks = TestSinks::new();
                let haptic = Arc::new(FakeHaptic::new());
                let mut px = drive_ready_proxy(
                    &sinks,
                    &haptic,
                    vec![
                        slide_report(1, if level == "slide" { 10 } else { 60 }, 0),
                        slide_report(1, 200, 0),
                    ],
                );
                let t0 = std::time::Instant::now();
                px.handle_frame_at(&cfg, t0).unwrap();
                if level == "force" {
                    px.handle_frame_at(&cfg, ms_after(t0, 10)).unwrap();
                }
                match teardown {
                    "disarm" => px.set_pump_ready(false),
                    "detach" => px.detach(&detached("/dev/hidraw1", Transport::Usb)),
                    _ => {
                        px.replace_source(ScriptSource::new(vec![Err(
                            crate::pump::DisconnectError("test unplug".into()).into(),
                        )]));
                        px.handle_frame_at(&cfg, ms_after(t0, 60)).unwrap();
                    }
                }
                assert!(px.is_passthrough());
                assert!(px.machine.active().is_none());
                assert!(px.pending_buttons.is_none());
                px.replace_source(ScriptSource::new(vec![]));
                px.set_pump_ready(true);
                px.attach(&usb_ready("/dev/hidraw1"));
                px.handle_frame_at(&cfg, ms_after(t0, 500)).unwrap();
                assert!(
                    key_values(&sinks.pointer, BTN_LEFT).is_empty(),
                    "{level}/{teardown}"
                );
                assert_eq!(key_values(&sinks.extras, 125), vec![1, 0]);
                if level == "slide" {
                    assert_no_haptics(&haptic);
                }
            }
        }
    }

    #[test]
    fn binding_timing_paths_preserve_omission_and_haptic_pairs() {
        // The same schedule exercises real frame selection and idle polling.
        // Cut keeps its first left click, independent 5 ms floor, then gap.
        for path in ["slide", "click", "continuous", "cut"] {
            let mut legacy_trace = None;
            for form in ["string", "omitted", "zero", "fifty"] {
                let binding = match form {
                    "string" => "\"super+left\"",
                    "omitted" => "{ action = \"super+left\" }",
                    "zero" => "{ action = \"super+left\", stagger_ms = 0 }",
                    _ => "{ action = \"super+left\", stagger_ms = 50 }",
                };
                let key = match path {
                    "slide" => "slide_1",
                    "click" => "click_1",
                    _ => "force_1",
                };
                let transition = if path == "cut" { "cut" } else { "continuous" };
                let click_part = if key == "click_1" {
                    format!("{key} = {binding}")
                } else if path != "cut" {
                    "click_1 = \"none\"".to_string()
                } else {
                    String::new()
                };
                let force_part = if key == "force_1" {
                    format!("{key} = {binding}")
                } else {
                    String::new()
                };
                let slide_part = if key == "slide_1" {
                    format!("{key} = {binding}")
                } else {
                    String::new()
                };
                let mut cfg: Config = toml::from_str(&format!(
                    "schema_version = 1\n[click]\nsensitivity = 30\n{click_part}\n[force]\nforce_offset = 130\ntransition_mode = \"{transition}\"\n{force_part}\n[slide]\n{slide_part}\n",
                )).unwrap();
                cfg.validate().unwrap();
                let sinks = TestSinks::new();
                let haptic = Arc::new(FakeHaptic::new());
                let first_pressure = if path == "slide" { 10 } else { 60 };
                let second_pressure = if key == "force_1" {
                    200
                } else {
                    first_pressure
                };
                let mut px = drive_ready_proxy(
                    &sinks,
                    &haptic,
                    vec![
                        slide_report(1, first_pressure, 0),
                        slide_report(1, second_pressure, 10),
                    ],
                );
                let t0 = std::time::Instant::now();
                let start = match path {
                    "cut" => 15,
                    "continuous" => 10,
                    _ => 0,
                };
                let delay = match form {
                    "zero" => 0,
                    "fifty" => 50,
                    _ if path == "slide" || path == "cut" => 50,
                    _ => 0,
                };
                for ms in [0, 10, 14, 15, 49, 50, 59, 60, 64, 65] {
                    px.handle_frame_at(&cfg, ms_after(t0, ms)).unwrap();
                    let expected = if path == "cut" && ms < 10 {
                        vec![1]
                    } else if path == "cut" && ms < start + delay {
                        vec![1, 0]
                    } else if ms >= start + delay {
                        if path == "cut" {
                            vec![1, 0, 1]
                        } else {
                            vec![1]
                        }
                    } else {
                        vec![]
                    };
                    assert_eq!(
                        key_values(&sinks.pointer, BTN_LEFT),
                        expected,
                        "{path}/{form} at {ms}"
                    );
                    if ms == 10 {
                        // A later config/edge cannot retime captured pending work.
                        cfg.force.combo_stagger_ms = 500;
                    }
                }
                px.replace_source(ScriptSource::new(vec![slide_report(0, 0, 0)]));
                px.handle_frame_at(&cfg, ms_after(t0, 100)).unwrap();
                assert_eq!(key_values(&sinks.extras, 125), vec![1, 0]);
                let pulses = match path {
                    "slide" => 0,
                    "cut" => 2,
                    _ => 1,
                };
                assert_eq!(haptic.calls.lock().unwrap().len(), pulses);
                assert_eq!(haptic.releases.lock().unwrap().len(), pulses);
                let trace = (wire_events(&sinks.pointer), wire_events(&sinks.extras));
                if form == "string" {
                    legacy_trace = Some(trace);
                } else if form == "omitted" {
                    assert_eq!(
                        Some(trace),
                        legacy_trace,
                        "{path}: object omission changed legacy wire framing"
                    );
                }
            }
        }
    }

    #[test]
    fn binding_timing_single_group_actions_have_no_timed_division() {
        for level in ["click", "force", "slide"] {
            for (action, code) in [
                ("alt+f4", 56),
                ("left", BTN_LEFT),
                ("back", BTN_SIDE),
                ("super+back", 125),
            ] {
                let binding = format!("{{ action = \"{action}\", stagger_ms = 500 }}");
                let (click_part, force_part, slide_part) = match level {
                    "click" => (format!("click_1 = {binding}"), String::new(), String::new()),
                    "force" => (
                        "click_1 = \"none\"".to_string(),
                        format!("force_1 = {binding}"),
                        String::new(),
                    ),
                    _ => (
                        "click_1 = \"none\"".to_string(),
                        String::new(),
                        format!("slide_1 = {binding}"),
                    ),
                };
                let cfg: Config = toml::from_str(&format!(
                    "schema_version = 1\n[click]\nsensitivity = 30\n{click_part}\n[force]\nforce_offset = 130\n{force_part}\n[slide]\n{slide_part}\n",
                )).unwrap();
                let sinks = TestSinks::new();
                let haptic = Arc::new(FakeHaptic::new());
                let mut px = drive_ready_proxy(
                    &sinks,
                    &haptic,
                    vec![
                        slide_report(1, if level == "slide" { 10 } else { 60 }, 0),
                        slide_report(1, 200, 0),
                    ],
                );
                let t0 = std::time::Instant::now();
                px.handle_frame_at(&cfg, t0).unwrap();
                if level == "force" {
                    px.handle_frame_at(&cfg, ms_after(t0, 10)).unwrap();
                }
                let sink = if code == BTN_LEFT {
                    &sinks.pointer
                } else {
                    &sinks.extras
                };
                assert_eq!(key_values(sink, code), vec![1], "{level}/{action}");
                assert!(px.pending_buttons.is_none());
                if level == "slide" {
                    assert_no_haptics(&haptic);
                }
            }
        }
    }

    #[test]
    fn binding_timing_independent_levels_keep_distinct_deadlines_and_shared_modifiers() {
        let cfg: Config = toml::from_str(
            r#"
            schema_version = 1
            [click]
            sensitivity = 30
            click_1 = { action = "super+left", stagger_ms = 120 }
            [force]
            force_offset = 130
            transition_mode = "continuous"
            force_1 = { action = "super+right", stagger_ms = 50 }
        "#,
        )
        .unwrap();
        let sinks = TestSinks::new();
        let haptic = Arc::new(FakeHaptic::new());
        let mut px = drive_ready_proxy(
            &sinks,
            &haptic,
            vec![slide_report(1, 60, 0), slide_report(1, 200, 20)],
        );
        let t0 = std::time::Instant::now();
        for ms in [0, 10, 59] {
            px.handle_frame_at(&cfg, ms_after(t0, ms)).unwrap();
        }
        assert!(key_values(&sinks.pointer, BTN_LEFT).is_empty());
        assert!(key_values(&sinks.pointer, BTN_RIGHT).is_empty());
        px.handle_frame_at(&cfg, ms_after(t0, 60)).unwrap();
        assert_eq!(key_values(&sinks.pointer, BTN_RIGHT), vec![1]);
        assert!(key_values(&sinks.pointer, BTN_LEFT).is_empty());
        px.replace_source(ScriptSource::new(vec![slide_report(1, 60, 30)]));
        px.handle_frame_at(&cfg, ms_after(t0, 70)).unwrap();
        assert_eq!(key_values(&sinks.pointer, BTN_RIGHT), vec![1, 0]);
        assert_eq!(
            key_values(&sinks.extras, 125),
            vec![1],
            "first level still owns Super"
        );
        px.handle_frame_at(&cfg, ms_after(t0, 119)).unwrap();
        assert!(key_values(&sinks.pointer, BTN_LEFT).is_empty());
        px.handle_frame_at(&cfg, ms_after(t0, 120)).unwrap();
        assert_eq!(key_values(&sinks.pointer, BTN_LEFT), vec![1]);
        px.replace_source(ScriptSource::new(vec![slide_report(0, 0, 0)]));
        px.handle_frame_at(&cfg, ms_after(t0, 130)).unwrap();
        assert_eq!(key_values(&sinks.pointer, BTN_LEFT), vec![1, 0]);
        assert_eq!(key_values(&sinks.extras, 125), vec![1, 0]);
        assert_eq!(haptic.calls.lock().unwrap().len(), 2);
        assert_eq!(haptic.releases.lock().unwrap().len(), 2);
    }

    #[test]
    fn binding_timing_same_text_is_selected_by_real_or_spoofed_count() {
        for level in ["click", "force", "slide"] {
            for spoof in [false, true] {
                let (click_part, force_part, slide_part) = match level {
                    "click" => (
                        "click_1 = { action = \"super+left\", stagger_ms = 0 }\nclick_2 = { action = \"super+left\", stagger_ms = 50 }".to_string(),
                        String::new(),
                        String::new(),
                    ),
                    "force" => (
                        "click_2 = \"none\"".to_string(),
                        "force_1 = { action = \"super+left\", stagger_ms = 0 }\nforce_2 = { action = \"super+left\", stagger_ms = 50 }".to_string(),
                        String::new(),
                    ),
                    _ => (
                        "click_2 = \"none\"".to_string(),
                        String::new(),
                        "slide_1 = { action = \"super+left\", stagger_ms = 0 }\nslide_2 = { action = \"super+left\", stagger_ms = 50 }".to_string(),
                    ),
                };
                let cfg: Config = toml::from_str(&format!(
                    "schema_version = 1\n[click]\nsensitivity = 30\n{click_part}\n[force]\nforce_offset = 130\n{force_part}\n[slide]\n{slide_part}\n[spoof]\nactions_follow_spoof = {spoof}\n[spoof.{level}]\n2 = 1\n",
                ))
                .unwrap();
                let sinks = TestSinks::new();
                let haptic = Arc::new(FakeHaptic::new());
                let mut px = drive_ready_proxy(
                    &sinks,
                    &haptic,
                    vec![
                        slide_report(2, if level == "slide" { 10 } else { 60 }, 0),
                        slide_report(2, 200, 0),
                    ],
                );
                let t0 = std::time::Instant::now();
                let mut ev = px.handle_frame_at(&cfg, t0).unwrap();
                let start = if level == "force" {
                    ev = px.handle_frame_at(&cfg, ms_after(t0, 10)).unwrap();
                    10
                } else {
                    0
                };
                assert_eq!(ev.unwrap().finger_count, if spoof { 1 } else { 2 });
                assert_eq!(
                    key_values(&sinks.pointer, BTN_LEFT),
                    if spoof { vec![1] } else { vec![] },
                    "{level}/{spoof}"
                );
                px.replace_source(ScriptSource::new(vec![
                    Ok(None),
                    Ok(None),
                    slide_report(0, 0, 0),
                ]));
                px.handle_frame_at(&cfg, ms_after(t0, start + 49)).unwrap();
                if !spoof {
                    assert!(key_values(&sinks.pointer, BTN_LEFT).is_empty());
                }
                px.handle_frame_at(&cfg, ms_after(t0, start + 50)).unwrap();
                assert_eq!(key_values(&sinks.pointer, BTN_LEFT), vec![1]);
                px.handle_frame_at(&cfg, ms_after(t0, start + 80)).unwrap();
                assert_eq!(key_values(&sinks.pointer, BTN_LEFT), vec![1, 0]);
                assert_eq!(key_values(&sinks.extras, 125), vec![1, 0]);
            }
        }
    }

    #[test]
    fn binding_timing_handoff_zero_and_continuous_force_cannot_shorten_pending_hold() {
        for force_delay in [0, 50, 500] {
            let cfg: Config = toml::from_str(&format!(
                r#"
                schema_version = 1
                [click]
                sensitivity = 30
                click_1 = {{ action = "left+super", stagger_ms = 0 }}
                [force]
                force_offset = 130
                transition_mode = "continuous"
                force_1 = {{ action = "ctrl+left", stagger_ms = {force_delay} }}
                [slide]
                slide_1 = {{ action = "super+left", stagger_ms = 120 }}
            "#
            ))
            .unwrap();
            let sinks = TestSinks::new();
            let haptic = Arc::new(FakeHaptic::new());
            let mut px = drive_ready_proxy(
                &sinks,
                &haptic,
                vec![
                    slide_report(1, 10, 0),
                    slide_report(1, 60, 0),
                    slide_report(1, 200, 0),
                ],
            );
            let t0 = std::time::Instant::now();
            for ms in [0, 10, 20, 119] {
                px.handle_frame_at(&cfg, ms_after(t0, ms)).unwrap();
            }
            assert!(key_values(&sinks.pointer, BTN_LEFT).is_empty());
            px.handle_frame_at(&cfg, ms_after(t0, 120)).unwrap();
            assert_eq!(key_values(&sinks.pointer, BTN_LEFT), vec![1]);
            px.replace_source(ScriptSource::new(vec![
                slide_report(1, 60, 0),
                slide_report(0, 0, 0),
            ]));
            px.handle_frame_at(&cfg, ms_after(t0, 130)).unwrap();
            assert_eq!(
                key_values(&sinks.pointer, BTN_LEFT),
                vec![1],
                "shared first owner survives force release"
            );
            px.handle_frame_at(&cfg, ms_after(t0, 150)).unwrap();
            assert_eq!(key_values(&sinks.pointer, BTN_LEFT), vec![1, 0]);
            assert_eq!(key_values(&sinks.extras, 125), vec![1, 0]);
            assert_eq!(key_values(&sinks.extras, 29), vec![1, 0]);
        }
    }

    #[test]
    fn binding_timing_lift_at_deadline_cancels_and_retouch_gets_fresh_deadline() {
        for lift_ms in [10, 50] {
            let cfg: Config = toml::from_str(
                r#"schema_version = 1
[click]
click_1 = { action = "super+left", stagger_ms = 50 }"#,
            )
            .unwrap();
            let sinks = TestSinks::new();
            let haptic = Arc::new(FakeHaptic::new());
            let mut px = drive_ready_proxy(
                &sinks,
                &haptic,
                vec![slide_report(1, 100, 0), slide_report(0, 0, 0)],
            );
            let t0 = std::time::Instant::now();
            px.handle_frame_at(&cfg, t0).unwrap();
            px.handle_frame_at(&cfg, ms_after(t0, lift_ms)).unwrap();
            assert!(
                px.machine.active().is_none(),
                "cancel must clear latch, preserving release debounce anchor"
            );
            px.handle_frame_at(&cfg, ms_after(t0, lift_ms + 1)).unwrap();
            assert!(key_values(&sinks.pointer, BTN_LEFT).is_empty());
            px.replace_source(ScriptSource::new(vec![slide_report(1, 100, 0)]));
            px.handle_frame_at(&cfg, ms_after(t0, lift_ms + 49))
                .unwrap();
            assert!(px.machine.active().is_none());
            px.replace_source(ScriptSource::new(vec![slide_report(1, 100, 0)]));
            px.handle_frame_at(&cfg, ms_after(t0, lift_ms + 50))
                .unwrap();
            assert!(px.machine.active().is_some());
            px.handle_frame_at(&cfg, ms_after(t0, lift_ms + 99))
                .unwrap();
            assert!(key_values(&sinks.pointer, BTN_LEFT).is_empty());
            px.handle_frame_at(&cfg, ms_after(t0, lift_ms + 100))
                .unwrap();
            assert_eq!(key_values(&sinks.pointer, BTN_LEFT), vec![1]);
            px.replace_source(ScriptSource::new(vec![slide_report(0, 0, 0)]));
            px.handle_frame_at(&cfg, ms_after(t0, lift_ms + 110))
                .unwrap();
            assert_eq!(key_values(&sinks.extras, 125), vec![1, 0, 1, 0]);
            assert_eq!(key_values(&sinks.pointer, BTN_LEFT), vec![1, 0]);
            assert_eq!(haptic.calls.lock().unwrap().len(), 2);
            assert_eq!(haptic.releases.lock().unwrap().len(), 2);
        }
    }

    #[test]
    fn slide_mixed_five_to_one_staggers_while_motion_and_idle_polls_continue() {
        let sinks = TestSinks::new();
        let haptic = Arc::new(FakeHaptic::new());
        let mut px = drive_ready_proxy(
            &sinks,
            &haptic,
            vec![
                slide_report(5, 10, 0),
                slide_report(5, 10, 90),
                Ok(None),
                Ok(None),
                slide_report(0, 0, 0),
            ],
        );
        let cfg = five_slide_cfg();
        assert!(!cfg.spoof.actions_follow_spoof);
        assert_eq!(cfg.force.combo_stagger_ms, 50);
        let t0 = std::time::Instant::now();
        let ev = px.handle_frame_at(&cfg, t0).unwrap().unwrap();
        assert!(ev.is_slide());
        assert_eq!(ev.finger_count, 5, "selection must use real fingers");
        assert_eq!(key_values(&sinks.extras, 125), vec![1]);
        assert!(wire_events(&sinks.extras).ends_with(&[(EV_SYN, SYN_REPORT, 0)]));
        assert!(key_values(&sinks.pointer, BTN_LEFT).is_empty());
        assert!(sinks.pointer.has(EV_KEY, BTN_TOOL_FINGER, 1));
        assert!(!sinks.pointer.has(EV_KEY, BTN_TOOL_QUINTTAP, 1));
        assert_eq!(
            wire_events(&sinks.pointer)
                .iter()
                .filter(|&&(ty, code, value)| ty == EV_ABS
                    && code == ABS_MT_TRACKING_ID
                    && value >= 0)
                .count(),
            1,
            "exactly one virtual contact"
        );
        px.handle_frame_at(&cfg, ms_after(t0, 49)).unwrap();
        assert!(sinks.pointer.has(EV_ABS, ABS_MT_POSITION_X, 90));
        px.handle_frame_at(&cfg, ms_after(t0, 49)).unwrap();
        assert!(key_values(&sinks.pointer, BTN_LEFT).is_empty());
        let mark = wire_events(&sinks.pointer).len();
        px.handle_frame_at(&cfg, ms_after(t0, 50)).unwrap();
        assert_eq!(
            &wire_events(&sinks.pointer)[mark..],
            &[(EV_KEY, BTN_LEFT, 1), (EV_SYN, SYN_REPORT, 0)],
            "deadline must progress without another touch report"
        );
        px.handle_frame_at(&cfg, ms_after(t0, 51)).unwrap();
        assert_eq!(key_values(&sinks.pointer, BTN_LEFT), vec![1, 0]);
        assert_eq!(key_values(&sinks.extras, 125), vec![1, 0]);
        assert_no_haptics(&haptic);
    }

    #[test]
    fn slide_mixed_custom_and_zero_staggers_preserve_report_order() {
        for delay in [0, 1, 120, 500] {
            // One log captures both groups' chronological SYN boundaries.
            let wire = Arc::new(FakeUinput::new());
            let mut px = ClickProxy::new(
                settings(true),
                FakeGrab::ok(),
                SharedUinput(Arc::clone(&wire)),
                FakeHaptic::new(),
                ScriptSource::new(vec![slide_report(5, 10, 0)]),
            );
            px.set_pump_ready(true);
            px.attach(&usb_ready("/dev/hidraw1"));
            let mut cfg = five_slide_cfg();
            cfg.force.combo_stagger_ms = delay;
            cfg.slide.slide_5 = "left+super".into();
            let t0 = std::time::Instant::now();
            px.handle_frame_at(&cfg, t0).unwrap();
            if delay > 0 {
                px.handle_frame_at(&cfg, ms_after(t0, u64::from(delay - 1)))
                    .unwrap();
                assert!(key_values(&wire, BTN_LEFT).is_empty());
                px.handle_frame_at(&cfg, ms_after(t0, u64::from(delay)))
                    .unwrap();
            }
            assert_eq!(key_values(&wire, BTN_LEFT), vec![1]);
            let events = wire_events(&wire);
            let super_at = events.iter().position(|e| *e == (EV_KEY, 125, 1)).unwrap();
            let left_at = events
                .iter()
                .position(|e| *e == (EV_KEY, BTN_LEFT, 1))
                .unwrap();
            assert!(super_at < left_at);
            assert!(events[super_at + 1..left_at].contains(&(EV_SYN, SYN_REPORT, 0)));
            assert!(events.ends_with(&[(EV_SYN, SYN_REPORT, 0)]));
        }
    }

    #[test]
    fn slide_single_group_actions_stay_immediate_and_silent() {
        for action in ["super+a", "left", "empty"] {
            let sinks = TestSinks::new();
            let haptic = Arc::new(FakeHaptic::new());
            let mut px = drive_ready_proxy(&sinks, &haptic, vec![slide_report(5, 10, 0)]);
            let mut cfg = five_slide_cfg();
            cfg.slide.slide_5 = action.into();
            px.handle_frame_at(&cfg, std::time::Instant::now()).unwrap();
            match action {
                "super+a" => {
                    assert_eq!(key_values(&sinks.extras, 125), vec![1]);
                    assert_eq!(key_values(&sinks.extras, 30), vec![1]);
                    assert!(key_values(&sinks.pointer, BTN_LEFT).is_empty());
                }
                "left" => assert_eq!(key_values(&sinks.pointer, BTN_LEFT), vec![1]),
                "empty" => {
                    assert!(key_values(&sinks.extras, 125).is_empty());
                    assert!(key_values(&sinks.pointer, BTN_LEFT).is_empty());
                }
                _ => unreachable!(),
            }
            assert_eq!(
                wire_events(&sinks.extras)
                    .iter()
                    .filter(|e| e.0 == EV_SYN)
                    .count(),
                1
            );
            assert_no_haptics(&haptic);
        }
    }

    #[test]
    fn slide_mixed_count_change_or_liftoff_cancels_even_at_deadline() {
        for n in [0, 4] {
            for cancel_at in [10, 50, 60] {
                let sinks = TestSinks::new();
                let haptic = Arc::new(FakeHaptic::new());
                let mut px = drive_ready_proxy(
                    &sinks,
                    &haptic,
                    vec![slide_report(5, 10, 0), slide_report(n, 10, 0)],
                );
                let cfg = five_slide_cfg();
                let t0 = std::time::Instant::now();
                px.handle_frame_at(&cfg, t0).unwrap();
                px.handle_frame_at(&cfg, ms_after(t0, cancel_at)).unwrap();
                px.handle_frame_at(&cfg, ms_after(t0, 500)).unwrap();
                assert!(key_values(&sinks.pointer, BTN_LEFT).is_empty());
                assert_eq!(key_values(&sinks.extras, 125), vec![1, 0]);
                assert_no_haptics(&haptic);
            }
        }
    }

    #[test]
    fn slide_mixed_press_into_none_keeps_click_and_force_silent() {
        let sinks = TestSinks::new();
        let haptic = Arc::new(FakeHaptic::new());
        let mut px = drive_ready_proxy(
            &sinks,
            &haptic,
            vec![
                slide_report(5, 10, 0),
                slide_report(5, 200, 0),
                slide_report(5, 200, 0),
                slide_report(0, 0, 0),
            ],
        );
        let cfg = five_slide_cfg();
        let t0 = std::time::Instant::now();
        px.handle_frame_at(&cfg, t0).unwrap();
        let first = px.handle_frame_at(&cfg, ms_after(t0, 10)).unwrap().unwrap();
        assert!(first.is_none() && first.level == ClickLevel::First);
        // A force `none` is latched but deliberately absent from primary().
        assert!(px
            .handle_frame_at(&cfg, ms_after(t0, 11))
            .unwrap()
            .is_none());
        let force = px.machine.force_active().unwrap();
        assert!(force.is_none() && force.is_force());
        px.handle_frame_at(&cfg, ms_after(t0, 100)).unwrap();
        px.handle_frame_at(&cfg, ms_after(t0, 500)).unwrap();
        assert!(key_values(&sinks.pointer, BTN_LEFT).is_empty());
        assert_eq!(key_values(&sinks.extras, 125), vec![1, 0]);
        assert_no_haptics(&haptic);
    }

    #[test]
    fn slide_mixed_different_press_cancels_pending_button_and_shares_modifier() {
        let sinks = TestSinks::new();
        let haptic = Arc::new(FakeHaptic::new());
        let mut px = drive_ready_proxy(
            &sinks,
            &haptic,
            vec![slide_report(5, 10, 0), slide_report(5, 60, 0)],
        );
        let mut cfg = five_slide_cfg();
        cfg.click.click_5 = "super+right".into();
        let t0 = std::time::Instant::now();
        px.handle_frame_at(&cfg, t0).unwrap();
        // A different action gets its normal immediate click behavior,
        // even when replacement arrives exactly at the old deadline.
        px.handle_frame_at(&cfg, ms_after(t0, 50)).unwrap();
        px.handle_frame_at(&cfg, ms_after(t0, 100)).unwrap();
        assert!(key_values(&sinks.pointer, BTN_LEFT).is_empty());
        assert_eq!(key_values(&sinks.pointer, BTN_RIGHT), vec![1]);
        assert_eq!(key_values(&sinks.extras, 125), vec![1, 0, 1]);
        px.replace_source(ScriptSource::new(vec![slide_report(0, 0, 0)]));
        px.handle_frame_at(&cfg, ms_after(t0, 110)).unwrap();
        assert_eq!(key_values(&sinks.pointer, BTN_RIGHT), vec![1, 0]);
        assert_eq!(key_values(&sinks.extras, 125), vec![1, 0, 1, 0]);
    }

    #[test]
    fn slide_mixed_replacement_and_return_start_a_fresh_deadline() {
        let sinks = TestSinks::new();
        let haptic = Arc::new(FakeHaptic::new());
        let mut px = drive_ready_proxy(
            &sinks,
            &haptic,
            vec![
                slide_report(5, 10, 0),
                slide_report(4, 10, 0),
                slide_report(5, 10, 0),
            ],
        );
        let mut cfg = five_slide_cfg();
        cfg.slide.slide_4 = "alt+right".into();
        let t0 = std::time::Instant::now();
        for ms in [0, 20, 40] {
            px.handle_frame_at(&cfg, ms_after(t0, ms)).unwrap();
        }
        assert_eq!(key_values(&sinks.extras, 125), vec![1, 0, 1]);
        assert_eq!(key_values(&sinks.extras, 56), vec![1, 0]);
        for ms in [50, 70, 89] {
            px.handle_frame_at(&cfg, ms_after(t0, ms)).unwrap();
            assert!(key_values(&sinks.pointer, BTN_LEFT).is_empty());
            assert!(key_values(&sinks.pointer, BTN_RIGHT).is_empty());
        }
        px.handle_frame_at(&cfg, ms_after(t0, 90)).unwrap();
        assert_eq!(key_values(&sinks.pointer, BTN_LEFT), vec![1]);
        assert_no_haptics(&haptic);
    }

    #[test]
    fn slide_mixed_identical_count_reselect_keeps_deadline() {
        let sinks = TestSinks::new();
        let haptic = Arc::new(FakeHaptic::new());
        let mut px = drive_ready_proxy(
            &sinks,
            &haptic,
            vec![slide_report(5, 10, 0), slide_report(4, 10, 0)],
        );
        let mut cfg = five_slide_cfg();
        cfg.slide.slide_4 = crate::config::ActionBinding {
            action: "left+super".into(),
            stagger_ms: Some(0),
        };
        let t0 = std::time::Instant::now();
        px.handle_frame_at(&cfg, t0).unwrap();
        px.handle_frame_at(&cfg, ms_after(t0, 20)).unwrap();
        assert_eq!(key_values(&sinks.extras, 125), vec![1]);
        assert!(key_values(&sinks.pointer, BTN_LEFT).is_empty());
        px.handle_frame_at(&cfg, ms_after(t0, 50)).unwrap();
        assert_eq!(key_values(&sinks.pointer, BTN_LEFT), vec![1]);
    }

    #[test]
    fn slide_mixed_session_teardown_cancels_pending_buttons() {
        for teardown in ["disarm", "detach", "disconnect"] {
            let sinks = TestSinks::new();
            let haptic = Arc::new(FakeHaptic::new());
            let mut px = drive_ready_proxy(&sinks, &haptic, vec![slide_report(5, 10, 0)]);
            let cfg = five_slide_cfg();
            let t0 = std::time::Instant::now();
            px.handle_frame_at(&cfg, t0).unwrap();
            match teardown {
                "disarm" => px.set_pump_ready(false),
                "detach" => px.detach(&detached("/dev/hidraw1", Transport::Usb)),
                "disconnect" => {
                    px.replace_source(ScriptSource::new(vec![Err(crate::pump::DisconnectError(
                        "test unplug".into(),
                    )
                    .into())]));
                    px.handle_frame_at(&cfg, ms_after(t0, 50)).unwrap();
                }
                _ => unreachable!(),
            }
            assert!(px.is_passthrough());
            assert!(!px.grab().is_grabbed());
            px.replace_source(ScriptSource::new(vec![]));
            px.set_pump_ready(true);
            px.attach(&usb_ready("/dev/hidraw1"));
            px.handle_frame_at(&cfg, ms_after(t0, 500)).unwrap();
            assert!(key_values(&sinks.pointer, BTN_LEFT).is_empty());
            assert_eq!(key_values(&sinks.extras, 125), vec![1, 0]);
            assert_no_haptics(&haptic);
        }
    }

    #[test]
    fn slide_mixed_same_action_handoff_before_or_after_button_is_seamless() {
        for handoff_at in [10, 60] {
            let sinks = TestSinks::new();
            let haptic = Arc::new(FakeHaptic::new());
            let mut px = drive_ready_proxy(&sinks, &haptic, vec![slide_report(5, 10, 0)]);
            let mut cfg = five_slide_cfg();
            cfg.click.click_5 = "left+super".into();
            let t0 = std::time::Instant::now();
            px.handle_frame_at(&cfg, t0).unwrap();
            if handoff_at > 50 {
                px.handle_frame_at(&cfg, ms_after(t0, 50)).unwrap();
            }
            px.replace_source(ScriptSource::new(vec![slide_report(5, 60, 0)]));
            px.handle_frame_at(&cfg, ms_after(t0, handoff_at)).unwrap();
            if handoff_at < 50 {
                assert!(key_values(&sinks.pointer, BTN_LEFT).is_empty());
                px.handle_frame_at(&cfg, ms_after(t0, 50)).unwrap();
            }
            assert_eq!(key_values(&sinks.pointer, BTN_LEFT), vec![1]);
            assert_eq!(key_values(&sinks.extras, 125), vec![1]);
            px.replace_source(ScriptSource::new(vec![slide_report(0, 0, 0)]));
            px.handle_frame_at(&cfg, ms_after(t0, 100)).unwrap();
            assert_eq!(key_values(&sinks.pointer, BTN_LEFT), vec![1, 0]);
            assert_eq!(key_values(&sinks.extras, 125), vec![1, 0]);
            assert_eq!(haptic.calls.lock().unwrap().len(), 1);
            assert_eq!(haptic.releases.lock().unwrap().len(), 1);
        }
    }

    #[test]
    fn slide_mixed_handoff_then_rapid_liftoff_cannot_finish_a_debounced_press() {
        let sinks = TestSinks::new();
        let haptic = Arc::new(FakeHaptic::new());
        let mut px = drive_ready_proxy(
            &sinks,
            &haptic,
            vec![
                slide_report(5, 10, 0),
                slide_report(5, 60, 0),
                slide_report(0, 0, 0),
            ],
        );
        let mut cfg = five_slide_cfg();
        cfg.click.click_5 = "super+left".into();
        let t0 = std::time::Instant::now();
        for ms in [0, 10, 11, 50, 100] {
            px.handle_frame_at(&cfg, ms_after(t0, ms)).unwrap();
        }
        assert!(key_values(&sinks.pointer, BTN_LEFT).is_empty());
        assert_eq!(key_values(&sinks.extras, 125), vec![1, 0]);
    }

    #[test]
    fn slide_mixed_rapid_retouch_after_pending_handoff_retries_after_debounce() {
        for explicit_lift in [false, true] {
            let sinks = TestSinks::new();
            let haptic = Arc::new(FakeHaptic::new());
            let lifted = if explicit_lift {
                Ok(Some(frame(
                    (1..=5)
                        .map(|id| FingerContact {
                            contact: false,
                            ..finger(id, 0, 0, 20)
                        })
                        .collect(),
                    0,
                )))
            } else {
                slide_report(0, 0, 0)
            };
            let mut px = drive_ready_proxy(
                &sinks,
                &haptic,
                vec![slide_report(5, 10, 0), slide_report(5, 60, 0), lifted],
            );
            let mut cfg = five_slide_cfg();
            cfg.click.click_5 = "super+left".into();
            let t0 = std::time::Instant::now();
            for ms in [0, 10, 15] {
                px.handle_frame_at(&cfg, ms_after(t0, ms)).unwrap();
            }
            assert!(px.machine.active().is_none());
            assert!(px.pending_buttons.is_none());
            assert!(px.held_first.is_empty());
            // No report after liftoff: neither the old deadline nor an idle
            // poll can revive the cancelled button.
            px.handle_frame_at(&cfg, ms_after(t0, 19)).unwrap();
            px.replace_source(ScriptSource::new(vec![slide_report(5, 60, 0)]));
            px.handle_frame_at(&cfg, ms_after(t0, 20)).unwrap();
            assert!(
                px.machine.active().is_none(),
                "press debounce still applies"
            );
            for ms in [50, 64] {
                px.handle_frame_at(&cfg, ms_after(t0, ms)).unwrap();
            }
            assert!(key_values(&sinks.pointer, BTN_LEFT).is_empty());
            assert_eq!(key_values(&sinks.extras, 125), vec![1, 0]);
            px.replace_source(ScriptSource::new(vec![slide_report(5, 60, 0)]));
            px.handle_frame_at(&cfg, ms_after(t0, 70)).unwrap();
            assert!(px.machine.active().is_some());
            assert!(!px.held_first.is_empty());
            assert_eq!(key_values(&sinks.extras, 125), vec![1, 0, 1]);
            assert_eq!(key_values(&sinks.pointer, BTN_LEFT), vec![1]);
            px.replace_source(ScriptSource::new(vec![slide_report(0, 0, 0)]));
            px.handle_frame_at(&cfg, ms_after(t0, 110)).unwrap();
            assert_eq!(key_values(&sinks.extras, 125), vec![1, 0, 1, 0]);
            assert_eq!(key_values(&sinks.pointer, BTN_LEFT), vec![1, 0]);
            assert_eq!(haptic.calls.lock().unwrap().len(), 2);
            assert_eq!(haptic.releases.lock().unwrap().len(), 2);
        }
    }

    #[test]
    fn slide_mixed_pending_handoff_pressure_dip_keeps_latch_and_deadline() {
        let sinks = TestSinks::new();
        let haptic = Arc::new(FakeHaptic::new());
        let mut px = drive_ready_proxy(
            &sinks,
            &haptic,
            vec![
                slide_report(5, 10, 0),
                slide_report(5, 60, 0),
                slide_report(5, 10, 0),
                slide_report(5, 60, 0),
            ],
        );
        let mut cfg = five_slide_cfg();
        cfg.click.click_5 = "super+left".into();
        let t0 = std::time::Instant::now();
        for ms in [0, 10, 15, 20, 49] {
            px.handle_frame_at(&cfg, ms_after(t0, ms)).unwrap();
        }
        assert!(px.machine.active().is_some());
        assert!(px.pending_buttons.is_some());
        assert!(key_values(&sinks.pointer, BTN_LEFT).is_empty());
        // Contact never ended: the original slide deadline still applies,
        // even though a pressure dip cleared the per-finger pressed bits.
        px.handle_frame_at(&cfg, ms_after(t0, 50)).unwrap();
        assert_eq!(key_values(&sinks.extras, 125), vec![1]);
        assert_eq!(key_values(&sinks.pointer, BTN_LEFT), vec![1]);
        px.replace_source(ScriptSource::new(vec![slide_report(0, 0, 0)]));
        px.handle_frame_at(&cfg, ms_after(t0, 100)).unwrap();
        assert_eq!(key_values(&sinks.extras, 125), vec![1, 0]);
        assert_eq!(key_values(&sinks.pointer, BTN_LEFT), vec![1, 0]);
        assert_eq!(haptic.calls.lock().unwrap().len(), 1);
        assert_eq!(haptic.releases.lock().unwrap().len(), 1);
    }

    #[test]
    fn pending_force_liftoff_cancels_both_stages_and_retouch_retries() {
        for mods_first in [false, true] {
            let sinks = TestSinks::new();
            let haptic = Arc::new(FakeHaptic::new());
            let mut px = drive_ready_proxy(
                &sinks,
                &haptic,
                vec![slide_report(5, 60, 0), slide_report(5, 200, 0)],
            );
            let mut cfg = five_slide_cfg();
            cfg.click.click_5 = "super+left".into();
            cfg.force.force_5 = "super+left".into();
            let t0 = std::time::Instant::now();
            for ms in [0, 10] {
                px.handle_frame_at(&cfg, ms_after(t0, ms)).unwrap();
            }
            assert!(px.pending_force.is_some());
            if mods_first {
                // No report: force modifiers still advance at the 5 ms
                // floor on idle polls, while the button waits 50 ms more.
                px.handle_frame_at(&cfg, ms_after(t0, 15)).unwrap();
                assert_eq!(key_values(&sinks.extras, 125), vec![1, 0, 1]);
            }
            let lift_at = if mods_first { 16 } else { 12 };
            px.replace_source(ScriptSource::new(vec![slide_report(0, 0, 0)]));
            px.handle_frame_at(&cfg, ms_after(t0, lift_at)).unwrap();
            assert!(px.machine.active().is_none());
            assert!(px.machine.force_active().is_none());
            assert!(px.pending_force.is_none());
            assert!(px.held_first.is_empty() && px.held_force.is_empty());
            let cancelled_super = if mods_first {
                vec![1, 0, 1, 0]
            } else {
                vec![1, 0]
            };
            px.handle_frame_at(&cfg, ms_after(t0, 19)).unwrap();
            px.replace_source(ScriptSource::new(vec![slide_report(5, 200, 0)]));
            px.handle_frame_at(&cfg, ms_after(t0, 20)).unwrap();
            assert!(
                px.machine.active().is_none(),
                "press debounce still applies"
            );
            for ms in [60, 65] {
                px.handle_frame_at(&cfg, ms_after(t0, ms)).unwrap();
            }
            assert_eq!(key_values(&sinks.extras, 125), cancelled_super);
            assert_eq!(key_values(&sinks.pointer, BTN_LEFT), vec![1, 0]);
            // Fresh deep contact retries first, then force on the next
            // report; cut keeps its 5 ms floor and 50 ms button gap.
            px.replace_source(ScriptSource::new(vec![
                slide_report(5, 200, 0),
                slide_report(5, 200, 0),
            ]));
            for ms in [70, 71, 75, 76, 125] {
                px.handle_frame_at(&cfg, ms_after(t0, ms)).unwrap();
            }
            assert!(px.machine.active().is_some());
            assert!(px.machine.force_active().is_some());
            assert!(px.pending_force.is_some());
            assert_eq!(key_values(&sinks.pointer, BTN_LEFT), vec![1, 0, 1, 0]);
            px.handle_frame_at(&cfg, ms_after(t0, 126)).unwrap();
            assert!(px.pending_force.is_none());
            assert_eq!(key_values(&sinks.pointer, BTN_LEFT), vec![1, 0, 1, 0, 1]);
            px.replace_source(ScriptSource::new(vec![slide_report(0, 0, 0)]));
            px.handle_frame_at(&cfg, ms_after(t0, 200)).unwrap();
            assert_eq!(key_values(&sinks.pointer, BTN_LEFT), vec![1, 0, 1, 0, 1, 0]);
            let mut expected_super = cancelled_super;
            expected_super.extend([1, 0, 1, 0]);
            assert_eq!(key_values(&sinks.extras, 125), expected_super);
            assert!(px.machine.active().is_none());
            assert!(px.machine.force_active().is_none());
        }
    }

    #[test]
    fn slide_mixed_cut_after_handoff_cancels_old_deadline_and_stages_force() {
        let sinks = TestSinks::new();
        let haptic = Arc::new(FakeHaptic::new());
        let mut px = drive_ready_proxy(
            &sinks,
            &haptic,
            vec![
                slide_report(5, 10, 0),
                slide_report(5, 60, 0),
                slide_report(5, 200, 0),
            ],
        );
        let mut cfg = five_slide_cfg();
        cfg.click.click_5 = "super+left".into();
        cfg.force.force_5 = "super+left".into();
        let t0 = std::time::Instant::now();
        for ms in [0, 10, 20, 24] {
            px.handle_frame_at(&cfg, ms_after(t0, ms)).unwrap();
        }
        assert_eq!(key_values(&sinks.extras, 125), vec![1, 0]);
        px.handle_frame_at(&cfg, ms_after(t0, 25)).unwrap();
        assert_eq!(key_values(&sinks.extras, 125), vec![1, 0, 1]);
        for ms in [50, 74] {
            px.handle_frame_at(&cfg, ms_after(t0, ms)).unwrap();
            assert!(key_values(&sinks.pointer, BTN_LEFT).is_empty());
        }
        px.handle_frame_at(&cfg, ms_after(t0, 75)).unwrap();
        assert_eq!(key_values(&sinks.pointer, BTN_LEFT), vec![1]);
        assert_eq!(haptic.calls.lock().unwrap().len(), 2);
        px.replace_source(ScriptSource::new(vec![slide_report(0, 0, 0)]));
        px.handle_frame_at(&cfg, ms_after(t0, 100)).unwrap();
        assert_eq!(key_values(&sinks.pointer, BTN_LEFT), vec![1, 0]);
        assert_eq!(key_values(&sinks.extras, 125), vec![1, 0, 1, 0]);
    }

    #[test]
    fn slide_mixed_continuous_force_shares_pending_button_and_retains_first_owner() {
        let sinks = TestSinks::new();
        let haptic = Arc::new(FakeHaptic::new());
        let mut px = drive_ready_proxy(
            &sinks,
            &haptic,
            vec![
                slide_report(5, 10, 0),
                slide_report(5, 60, 0),
                slide_report(5, 200, 0),
                slide_report(5, 200, 0),
            ],
        );
        let mut cfg = five_slide_cfg();
        cfg.click.click_5 = crate::config::ActionBinding {
            action: "super+left".into(),
            stagger_ms: Some(0),
        };
        cfg.force.force_5 = crate::config::ActionBinding {
            action: "ctrl+left".into(),
            stagger_ms: Some(50),
        };
        cfg.force.transition_mode = "continuous".into();
        cfg.slide.slide_5 = crate::config::ActionBinding {
            action: "super+left".into(),
            stagger_ms: Some(120),
        };
        let t0 = std::time::Instant::now();
        for ms in [0, 10, 20] {
            px.handle_frame_at(&cfg, ms_after(t0, ms)).unwrap();
        }
        assert!(key_values(&sinks.pointer, BTN_LEFT).is_empty());
        let mut disarmed = cfg.clone();
        disarmed.force.force_offset = 0;
        px.handle_frame_at(&disarmed, ms_after(t0, 40)).unwrap();
        assert_eq!(key_values(&sinks.extras, 29), vec![1, 0]);
        px.handle_frame_at(&disarmed, ms_after(t0, 119)).unwrap();
        assert!(key_values(&sinks.pointer, BTN_LEFT).is_empty());
        px.handle_frame_at(&disarmed, ms_after(t0, 120)).unwrap();
        assert_eq!(key_values(&sinks.pointer, BTN_LEFT), vec![1]);
        px.replace_source(ScriptSource::new(vec![slide_report(0, 0, 0)]));
        px.handle_frame_at(&disarmed, ms_after(t0, 150)).unwrap();
        assert_eq!(key_values(&sinks.pointer, BTN_LEFT), vec![1, 0]);
        assert_eq!(key_values(&sinks.extras, 125), vec![1, 0]);
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
            click: crate::config::ClickConfig {
                click_1: "right".into(),
                ..Config::default().click
            },
            slide: crate::config::SlideConfig {
                slide_1: "left".into(),
                ..Config::default().slide
            },
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
        assert_eq!(haptic.calls.lock().unwrap().as_slice(), &[(30, 2, true)]);
        assert_eq!(haptic.releases.lock().unwrap().as_slice(), &[(30, true)]);
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
            slide: crate::config::SlideConfig {
                slide_1: "left".into(),
                slide_2: "left".into(),
                ..Config::default().slide
            },
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
            click: crate::config::ClickConfig {
                click_1: "ctrl+left".into(),
                ..Config::default().click
            },
            slide: crate::config::SlideConfig {
                slide_1: "left+ctrl".into(),
                ..Config::default().slide
            },
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
    fn slide_handoff_alias_action_is_seamless() {
        // slide_1 = super+left, click_1 = meta+left: identical kernel codes
        // under different spellings — the hold must continue with no
        // release/re-press (finding #18: token-string compare broke this).
        let sinks = TestSinks::new();
        let haptic = Arc::new(FakeHaptic::new());
        let frames = vec![
            Ok(Some(frame(vec![finger(1, 20, 0, 0)], 0))),
            Ok(Some(frame(vec![finger(1, 200, 0, 0)], 0))),
            Ok(Some(frame(vec![], 0))),
        ];
        let mut px = drive_ready_proxy(&sinks, &haptic, frames);
        let cfg = Config {
            click: crate::config::ClickConfig {
                click_1: "meta+left".into(),
                ..Config::default().click
            },
            slide: crate::config::SlideConfig {
                slide_1: "super+left".into(),
                ..Config::default().slide
            },
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
        // Exactly one down/up pair per wire code: no mid-hold flutter.
        assert_eq!(key_values(&sinks.pointer, BTN_LEFT), vec![1, 0]);
        assert_eq!(key_values(&sinks.extras, 125), vec![1, 0]);
        assert_eq!(haptic.calls.lock().unwrap().len(), 1);
        assert_eq!(haptic.releases.lock().unwrap().len(), 1);
    }

    #[test]
    fn slide_handoff_alias_absorbs_into_matching_press_level() {
        // Staged composition with alias spellings throughout: slide_1 =
        // super+left merges seamlessly into click_1 = meta+left on the stab
        // frame (no wire flutter), then force_1 = meta+left joins the held
        // codes on the next frame with its own pulse. Left + meta: exactly
        // one down/up pair each; right never appears.
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
            click: crate::config::ClickConfig {
                click_1: "meta+left".into(),
                sensitivity: 64,
                margin: 5,
                ..Config::default().click
            },
            force: crate::config::ForceConfig {
                force_1: "meta+left".into(),
                force_offset: 20,
                transition_mode: "continuous".into(),
                ..Config::default().force
            },
            slide: crate::config::SlideConfig {
                slide_1: "super+left".into(),
                ..Config::default().slide
            },
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
        // Force joins the alias-held codes: still down, never fluttered.
        assert_eq!(key_values(&sinks.pointer, BTN_LEFT), vec![1]);
        assert_eq!(key_values(&sinks.extras, 125), vec![1]);
        // Two downs (first + force; the slide edge stays silent) / two ups.
        assert_eq!(haptic.calls.lock().unwrap().len(), 2);
        px.handle_frame_at(&cfg, spaced_at(t0, 3)).unwrap();
        assert_eq!(haptic.releases.lock().unwrap().len(), 2);
        assert_eq!(key_values(&sinks.pointer, BTN_LEFT), vec![1, 0]);
        assert_eq!(key_values(&sinks.extras, 125), vec![1, 0]);
        assert!(sinks.pointer.has(EV_KEY, BTN_LEFT, 0));
    }

    #[test]
    fn slide_reselect_alias_keyset_merges() {
        // slide_1 = control+c, slide_2 = ctrl+c: a count change mid-slide
        // keeps the wire down (alias-equivalent re-select merges silently).
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
            slide: crate::config::SlideConfig {
                slide_1: "control+c".into(),
                slide_2: "ctrl+c".into(),
                ..Config::default().slide
            },
            ..Config::default()
        };
        let t0 = std::time::Instant::now();
        let first = px.handle_frame_at(&cfg, spaced_at(t0, 0)).unwrap().unwrap();
        assert!(first.is_slide() && first.finger_count == 1);
        let second = px.handle_frame_at(&cfg, spaced_at(t0, 1)).unwrap().unwrap();
        assert!(second.is_slide() && second.finger_count == 2);
        assert_eq!(
            key_values(&sinks.extras, 29),
            vec![1],
            "alias re-select must not flutter"
        );
        assert!(haptic.calls.lock().unwrap().is_empty());
        px.handle_frame_at(&cfg, spaced_at(t0, 2)).unwrap();
        assert_eq!(key_values(&sinks.extras, 29), vec![1, 0]);
        assert!(haptic.releases.lock().unwrap().is_empty());
    }

    #[test]
    fn alias_comparison_agrees_with_wire_codes() {
        // Shared-driven drift guarantee: group the shared token universe
        // by wire code. Tokens sharing a code must emit identically and
        // merge under canonical comparison (both directions); tokens with
        // distinct codes must emit apart and never merge. Adding a token
        // to the shared table extends this coverage automatically.
        use crate::action_tokens::{all_known_tokens, token_code};
        use crate::config::parse_click_action;
        let tokens = all_known_tokens();
        let kinds: Vec<crate::config::ActionKind> = tokens
            .iter()
            .map(|t| parse_click_action(t).unwrap().kind)
            .collect();
        for (i, a) in tokens.iter().enumerate() {
            for (j, b) in tokens.iter().enumerate() {
                let same_wire = token_code(a) == token_code(b);
                assert_eq!(
                    key_token_to_code(a) == key_token_to_code(b),
                    same_wire,
                    "wire codes must agree with shared table: '{a}' vs '{b}'"
                );
                assert_eq!(
                    kinds[i].is_same_action(&kinds[j]),
                    same_wire,
                    "comparison must agree with wire codes: '{a}' vs '{b}'"
                );
            }
        }
        // Multi-token handoff shape still merges through canonicalization.
        let kind = |s: &str| parse_click_action(s).unwrap().kind;
        assert!(kind("super+left").is_same_action(&kind("meta+left")));
        assert!(kind("ctrl+c").is_same_action(&kind("control+c")));
        assert!(!kind("super+left").is_same_action(&kind("meta+right")));
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
            click: crate::config::ClickConfig {
                click_1: "left".into(),
                sensitivity: 64,
                margin: 5,
                ..Config::default().click
            },
            force: crate::config::ForceConfig {
                force_1: "right".into(),
                force_offset: 20,
                transition_mode: "continuous".into(),
                ..Config::default().force
            },
            slide: crate::config::SlideConfig {
                slide_1: "left".into(),
                ..Config::default().slide
            },
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
            click: crate::config::ClickConfig {
                click_1: "left".into(),
                sensitivity: 64,
                margin: 5,
                ..Config::default().click
            },
            force: crate::config::ForceConfig {
                force_1: "empty".into(),
                force_offset: 20,
                transition_mode: "cut".into(),
                ..Config::default().force
            },
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
            &[(64, 2, true), (64, 2, false)]
        );
        px.handle_frame_at(&cfg, spaced_at(t0, 2)).unwrap();
        assert_eq!(key_values(&sinks.pointer, BTN_LEFT), vec![1]);
        px.handle_frame_at(&cfg, spaced_at(t0, 3)).unwrap();
        assert_eq!(key_values(&sinks.pointer, BTN_LEFT), vec![1, 0]);
    }

    #[test]
    fn transition_continuous_holds_first_under_force() {
        // Continuous mode: force joins, left never ups mid-hold.
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
            click: crate::config::ClickConfig {
                click_1: "left".into(),
                sensitivity: 64,
                margin: 5,
                ..Config::default().click
            },
            force: crate::config::ForceConfig {
                force_1: "left+super".into(),
                force_offset: 20,
                transition_mode: "continuous".into(),
                ..Config::default().force
            },
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
            click: crate::config::ClickConfig {
                click_1: "left".into(),
                sensitivity: 64,
                margin: 5,
                ..Config::default().click
            },
            force: crate::config::ForceConfig {
                force_1: "left+super".into(),
                force_offset: 20,
                transition_mode: "cut".into(),
                ..Config::default().force
            },
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
            click: crate::config::ClickConfig {
                click_1: "left".into(),
                sensitivity: 64,
                margin: 5,
                ..Config::default().click
            },
            force: crate::config::ForceConfig {
                force_1: "forward".into(),
                force_offset: 20,
                transition_mode: "cut".into(),
                ..Config::default().force
            },
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
            click: crate::config::ClickConfig {
                click_1: "left".into(),
                sensitivity: 64,
                margin: 5,
                ..Config::default().click
            },
            force: crate::config::ForceConfig {
                force_1: "left+super".into(),
                force_offset: 20,
                transition_mode: "cut".into(),
                ..Config::default().force
            },
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
        // Owner NIRI (tune 30/130/5: press 30, force 190, first-release
        // 25, force-release 155): a hold dipping to 150 (just under force
        // release, still firmly pressed) must keep super down — otherwise
        // niri sees super-tap (overview on release) plus a separate click,
        // never the chord, and no drag starts. Only a clean liftoff
        // releases. `combo_stagger_ms = 0` keeps the staging compact here;
        // the stagger itself is pinned elsewhere.
        let sinks = TestSinks::new();
        let haptic = Arc::new(FakeHaptic::new());
        let frames = vec![
            Ok(Some(frame(vec![finger(1, 40, 0, 0)], 0))),
            Ok(Some(frame(vec![finger(1, 200, 0, 0)], 0))),
            Ok(Some(frame(vec![finger(1, 200, 5, 5)], 0))),
            Ok(Some(frame(vec![finger(1, 150, 5, 5)], 0))),
            Ok(Some(frame(vec![finger(1, 200, 9, 9)], 0))),
            Ok(Some(frame(vec![], 0))),
        ];
        let mut px = drive_ready_proxy(&sinks, &haptic, frames);
        let cfg = Config {
            click: crate::config::ClickConfig {
                click_1: "left".into(),
                sensitivity: 30,
                margin: 5,
                ..Config::default().click
            },
            force: crate::config::ForceConfig {
                force_1: "left+super".into(),
                force_offset: 130,
                transition_mode: "cut".into(),
                combo_stagger_ms: 0,
                ..Config::default().force
            },
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
            click: crate::config::ClickConfig {
                click_1: "left".into(),
                sensitivity: 64,
                margin: 5,
                ..Config::default().click
            },
            force: crate::config::ForceConfig {
                force_1: "left+super".into(),
                force_offset: 20,
                transition_mode: "cut".into(),
                ..Config::default().force
            },
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
            click: crate::config::ClickConfig {
                click_1: "left".into(),
                sensitivity: 64,
                margin: 5,
                ..Config::default().click
            },
            force: crate::config::ForceConfig {
                force_1: "left+super".into(),
                force_offset: 20,
                transition_mode: "cut".into(),
                combo_stagger_ms: 0,
                ..Config::default().force
            },
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
            click: crate::config::ClickConfig {
                click_1: "left".into(),
                sensitivity: 64,
                margin: 5,
                ..Config::default().click
            },
            force: crate::config::ForceConfig {
                force_1: "left+super".into(),
                force_offset: 20,
                transition_mode: "cut".into(),
                combo_stagger_ms: 120,
                ..Config::default().force
            },
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
            click: crate::config::ClickConfig {
                click_1: "left".into(),
                sensitivity: 64,
                margin: 5,
                ..Config::default().click
            },
            force: crate::config::ForceConfig {
                force_1: "left+super".into(),
                force_offset: 20,
                transition_mode: "cut".into(),
                ..Config::default().force
            },
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
            click: crate::config::ClickConfig {
                click_1: "left".into(),
                sensitivity: 64,
                margin: 5,
                ..Config::default().click
            },
            force: crate::config::ForceConfig {
                force_1: "left+super".into(),
                force_offset: 20,
                transition_mode: "cut".into(),
                ..Config::default().force
            },
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
            click: crate::config::ClickConfig {
                click_1: "left".into(),
                sensitivity: 64,
                margin: 5,
                ..Config::default().click
            },
            force: crate::config::ForceConfig {
                force_1: "super+left".into(),
                force_offset: 20,
                transition_mode: "cut".into(),
                ..Config::default().force
            },
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
            click: crate::config::ClickConfig {
                click_1: "left".into(),
                sensitivity: 64,
                margin: 5,
                ..Config::default().click
            },
            force: crate::config::ForceConfig {
                force_1: "left+super".into(),
                force_offset: 20,
                transition_mode: "cut".into(),
                ..Config::default().force
            },
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
            click: crate::config::ClickConfig {
                click_1: "left".into(),
                sensitivity: 64,
                margin: 5,
                ..Config::default().click
            },
            force: crate::config::ForceConfig {
                force_1: "left+super".into(),
                force_offset: 20,
                transition_mode: "continuous".into(),
                ..Config::default().force
            },
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
                click: crate::config::ClickConfig {
                    click_1: "left".into(),
                    sensitivity: 64,
                    margin: 5,
                    ..Config::default().click
                },
                force: crate::config::ForceConfig {
                    force_1: force.into(),
                    force_offset: 20,
                    transition_mode: "cut".into(),
                    ..Config::default().force
                },
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
        // [spoof.click] 2 = 3: two real fingers press click_2 (right), but
        // the wire carries three contacts (TRIPLETAP, three slots).
        let sinks = TestSinks::new();
        let haptic = Arc::new(FakeHaptic::new());
        let frames = vec![Ok(Some(frame(
            vec![finger(1, 200, 0, 0), finger(2, 200, 9, 9)],
            0,
        )))];
        let mut px = drive_ready_proxy(&sinks, &haptic, frames);
        let mut click = std::collections::BTreeMap::new();
        click.insert(2u8, 3u16);
        let cfg = Config {
            spoof: crate::config::SpoofConfig {
                click,
                ..Default::default()
            },
            click: crate::config::ClickConfig {
                click_2: "right".into(),
                ..Config::default().click
            },
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
    fn spoof_actions_follow_spoof_switches_wire_and_action_together() {
        // [spoof] actions_follow_spoof = true: the spoofed count also selects click_3.
        let sinks = TestSinks::new();
        let haptic = Arc::new(FakeHaptic::new());
        let frames = vec![Ok(Some(frame(
            vec![finger(1, 200, 0, 0), finger(2, 200, 9, 9)],
            0,
        )))];
        let mut px = drive_ready_proxy(&sinks, &haptic, frames);
        let mut click = std::collections::BTreeMap::new();
        click.insert(2u8, 3u16);
        let cfg = Config {
            spoof: crate::config::SpoofConfig {
                click,
                actions_follow_spoof: true,
                ..Default::default()
            },
            click: crate::config::ClickConfig {
                click_2: "right".into(),
                click_3: "middle".into(),
                ..Config::default().click
            },
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
        let mut slide = std::collections::BTreeMap::new();
        slide.insert(1u8, 2u16);
        let cfg = Config {
            spoof: crate::config::SpoofConfig {
                slide,
                ..Default::default()
            },
            slide: crate::config::SlideConfig {
                slide_1: "left".into(),
                ..Config::default().slide
            },
            ..Config::default()
        };
        let ev = px.handle_frame(&cfg).unwrap().unwrap();
        assert!(ev.is_slide());
        assert_eq!(ev.finger_count, 1);
        assert!(sinks.pointer.has(EV_KEY, BTN_LEFT, 1));
        assert!(sinks.pointer.has(EV_KEY, BTN_TOOL_DOUBLETAP, 1));
        assert!(!sinks.pointer.has(EV_KEY, BTN_TOOL_FINGER, 1));
    }

    #[test]
    fn pump_step_distinguishes_traffic_from_idle() {
        use super::PumpActivity;
        let sinks = TestSinks::new();
        let haptic = Arc::new(FakeHaptic::new());
        let mut px = drive_ready_proxy(
            &sinks,
            &haptic,
            vec![Ok(Some(frame(vec![finger(1, 200, 0, 0)], 0))), Ok(None)],
        );
        let cfg = Config::default();
        let t0 = std::time::Instant::now();
        let (ev, activity) = match px.pump_step_at(&cfg, t0) {
            (Ok(ev), a) => (ev, a),
            (Err(_), a) => (None, a),
        };
        assert!(ev.is_some());
        assert_eq!(activity, PumpActivity::Frame);
        // Steady hold, no pending deadlines: idle poll is a true no-op.
        let (_, activity) = match px.pump_step_at(&cfg, ms_after(t0, 5)) {
            (Ok(ev), a) => (ev, a),
            (Err(_), a) => (None, a),
        };
        assert_eq!(activity, PumpActivity::Idle);
        assert!(px.next_deadline().is_none());
    }

    #[test]
    fn pump_step_reports_deadline_work_and_transient() {
        use super::PumpActivity;
        // Staggered combo: idle polls before the deadline stay Idle, the
        // deadline poll lands the button as Deadline work.
        let cfg: Config = toml::from_str(
            r#"schema_version = 1
[click]
click_1 = { action = "super+left", stagger_ms = 50 }"#,
        )
        .unwrap();
        let sinks = TestSinks::new();
        let haptic = Arc::new(FakeHaptic::new());
        let mut px = drive_ready_proxy(
            &sinks,
            &haptic,
            vec![Ok(Some(frame(vec![finger(1, 200, 0, 0)], 0)))],
        );
        let t0 = std::time::Instant::now();
        let (_, a0) = match px.pump_step_at(&cfg, t0) {
            (Ok(ev), a) => (ev, a),
            (Err(_), a) => (None, a),
        };
        assert_eq!(a0, PumpActivity::Frame);
        assert!(px.pending_buttons.is_some());
        let deadline = px.next_deadline().expect("stagger deadline");
        assert_eq!(deadline, t0 + std::time::Duration::from_millis(50));
        // Before the deadline the pump would block, not poll: a step here
        // must not emit the button.
        px.replace_source(ScriptSource::new(vec![Ok(None)]));
        let (_, early) = match px.pump_step_at(&cfg, ms_after(t0, 49)) {
            (Ok(ev), a) => (ev, a),
            (Err(_), a) => (None, a),
        };
        assert!(key_values(&sinks.pointer, BTN_LEFT).is_empty());
        assert!(matches!(early, PumpActivity::Deadline | PumpActivity::Idle));
        // At the deadline the button lands with identical wire bytes to the
        // old 2 ms poll path (no resampling, no timing change).
        px.replace_source(ScriptSource::new(vec![Ok(None)]));
        let (_, at_deadline) = match px.pump_step_at(&cfg, ms_after(t0, 50)) {
            (Ok(ev), a) => (ev, a),
            (Err(_), a) => (None, a),
        };
        assert_eq!(at_deadline, PumpActivity::Deadline);
        assert_eq!(key_values(&sinks.pointer, BTN_LEFT), vec![1]);
        assert!(px.next_deadline().is_none());
        // Transient read failure drops the frame without aborting.
        px.replace_source(ScriptSource::new(vec![Err(anyhow::anyhow!("fake EIO"))]));
        let (_, transient) = match px.pump_step_at(&cfg, ms_after(t0, 60)) {
            (Ok(ev), a) => (ev, a),
            (Err(_), a) => (None, a),
        };
        assert_eq!(transient, PumpActivity::Transient);
    }

    #[test]
    fn next_deadline_covers_release_and_cut_stages() {
        // Release debounce: press then liftoff holds the latch for 30 ms.
        let sinks = TestSinks::new();
        let haptic = Arc::new(FakeHaptic::new());
        let mut px = drive_ready_proxy(
            &sinks,
            &haptic,
            vec![
                Ok(Some(frame(vec![finger(1, 200, 0, 0)], 0))),
                Ok(Some(frame(vec![], 0))),
            ],
        );
        let cfg = Config::default();
        let t0 = std::time::Instant::now();
        px.handle_frame_at(&cfg, t0).unwrap();
        px.handle_frame_at(&cfg, ms_after(t0, 10)).unwrap();
        assert!(px.machine.active().is_some());
        let deadline = px.next_deadline().expect("release deadline");
        assert_eq!(deadline, t0 + std::time::Duration::from_millis(30));
        // Gate-off / passthrough never arms a deadline.
        let mut off = ClickProxy::new(
            settings(false),
            FakeGrab::ok(),
            FakeUinput::new(),
            FakeHaptic::new(),
            ScriptSource::new(vec![]),
        );
        off.set_pump_ready(true);
        off.attach(&usb_ready("/dev/hidraw1"));
        assert!(off.next_deadline().is_none());
        assert!(off.poll_fd().is_none());
        assert!(!off.has_pending_frame());
    }

    #[test]
    fn idle_proxy_has_no_fd_no_deadline_no_pending() {
        // No device (Idle source, passthrough): the event-driven pump
        // blocks on control only — (near-)zero wakeups.
        let mut px = ClickProxy::new(
            settings(true),
            FakeGrab::ok(),
            FakeUinput::new(),
            FakeHaptic::new(),
            ScriptSource::new(vec![]),
        );
        assert!(px.poll_fd().is_none());
        assert!(px.next_deadline().is_none());
        assert!(!px.has_pending_frame());
        let cfg = Config::default();
        let (_, activity) = match px.pump_step_at(&cfg, std::time::Instant::now()) {
            (Ok(ev), a) => (ev, a),
            (Err(_), a) => (None, a),
        };
        assert_eq!(activity, super::PumpActivity::Idle);
    }

    #[test]
    fn event_loop_fires_stagger_deadline_without_polling() {
        // End-to-end through the event-driven pump thread (no fd): a 50 ms
        // stagger pending must land via the poll timeout, not continuous
        // polling. Generous 500 ms bound; production fires at ~50 ms.
        use std::sync::atomic::AtomicBool;
        let cfg: Config = toml::from_str(
            r#"schema_version = 1
[click]
click_1 = { action = "super+left", stagger_ms = 50 }"#,
        )
        .unwrap();
        let sinks = TestSinks::new();
        let haptic = Arc::new(FakeHaptic::new());
        let mut px = drive_ready_proxy(
            &sinks,
            &haptic,
            vec![Ok(Some(frame(vec![finger(1, 200, 0, 0)], 0)))],
        );
        let t0 = std::time::Instant::now();
        px.handle_frame_at(&cfg, t0).unwrap();
        assert!(px.pending_buttons.is_some());
        // Idle source from here: no more reports, deadline must still fire.
        px.replace_source(ScriptSource::new(vec![]));
        let proxy = Arc::new(Mutex::new(px));
        let stop = Arc::new(AtomicBool::new(false));
        let (handle, control) = crate::pump::spawn_pump(
            "deadline-test".to_string(),
            Arc::clone(&proxy),
            cfg,
            Arc::clone(&stop),
        );
        let give_up = std::time::Instant::now() + std::time::Duration::from_millis(500);
        while key_values(&sinks.pointer, BTN_LEFT).is_empty() && std::time::Instant::now() < give_up
        {
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        crate::pump::stop_pump(&stop, &control);
        handle.join().unwrap();
        assert_eq!(
            key_values(&sinks.pointer, BTN_LEFT),
            vec![1],
            "stagger deadline never fired through the event loop"
        );
    }

    #[test]
    fn stagger_deadline_waits_without_spinning() {
        // Regression for the sub-millisecond truncation spin: a 50 ms stagger
        // deadline must be reached with a bounded number of source reads, not
        // hundreds of 0-timeout polls in the final millisecond. Counts every
        // `poll_frame` while `spawn_pump` waits 90 ms around the deadline.
        use std::sync::atomic::{AtomicUsize, Ordering};
        struct CountIdle(Arc<AtomicUsize>, Option<TouchFrame>);
        impl TouchSource for CountIdle {
            fn tier(&self) -> CapabilityTier {
                CapabilityTier::L2p
            }
            fn pressure_units(&self) -> PressureUnits {
                PressureUnits::Arbitrary
            }
            fn poll_frame(&mut self) -> Result<Option<TouchFrame>> {
                self.0.fetch_add(1, Ordering::SeqCst);
                Ok(self.1.take())
            }
        }
        let cfg: Config = toml::from_str(
            r#"schema_version = 1
[click]
click_1 = { action = "super+left", stagger_ms = 50 }"#,
        )
        .unwrap();
        let sinks = TestSinks::new();
        let count = Arc::new(AtomicUsize::new(0));
        let mut px = ClickProxy::new(
            settings(true),
            FakeGrab::ok(),
            sinks.dual(),
            FakeHaptic::new(),
            CountIdle(
                Arc::clone(&count),
                Some(frame(vec![finger(1, 200, 0, 0)], 0)),
            ),
        );
        px.set_pump_ready(true);
        px.attach(&usb_ready("/dev/hidraw-review"));
        assert!(px.is_attached());
        px.handle_frame_at(&cfg, std::time::Instant::now()).unwrap();
        assert!(px.pending_buttons.is_some());
        let proxy = Arc::new(Mutex::new(px));
        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let (handle, control) = crate::pump::spawn_pump(
            "review-repro".into(),
            Arc::clone(&proxy),
            cfg,
            Arc::clone(&stop),
        );
        std::thread::sleep(std::time::Duration::from_millis(90));
        crate::pump::stop_pump(&stop, &control);
        handle.join().unwrap();
        assert_eq!(key_values(&sinks.pointer, BTN_LEFT), vec![1]);
        let reads = count.load(Ordering::SeqCst);
        assert!(
            reads <= 10,
            "50 ms deadline spun: {reads} source reads in 90 ms (buggy ceiling-truncated loop did ~680)"
        );
    }

    #[test]
    fn pump_step_disconnect_reports_disconnect_and_self_releases() {
        // Same dead-fd path as `disconnect_self_releases_once_with_bounded_polls`,
        // but through the event-loop entry (`pump_step_at`): the step must
        // report `Disconnect` (not `Idle`), self-release exactly once, and
        // then settle into `Idle` without touching the dead source again.
        use super::PumpActivity;
        use anyhow::Error as AnyhowError;
        let grab = Arc::new(FakeGrab::ok());
        let sinks = TestSinks::new();
        let disc = || {
            Err::<Option<TouchFrame>, AnyhowError>(
                crate::pump::DisconnectError("test unplug".into()).into(),
            )
        };
        let mut px = ClickProxy::new(
            settings(true),
            SharedGrab(Arc::clone(&grab)),
            sinks.dual(),
            FakeHaptic::new(),
            ScriptSource::new(vec![Ok(Some(frame(vec![finger(1, 200, 0, 0)], 0))), disc()]),
        );
        px.set_pump_ready(true);
        px.attach(&usb_ready("/dev/hidraw8"));
        let cfg = Config::default();
        let t0 = std::time::Instant::now();
        let (first, a0) = px.pump_step_at(&cfg, t0);
        assert!(first.unwrap().is_some(), "press edge must report");
        assert_eq!(a0, PumpActivity::Frame);
        assert_eq!(key_values(&sinks.pointer, BTN_LEFT), vec![1]);
        let (ev, activity) = px.pump_step_at(&cfg, ms_after(t0, 60));
        assert!(ev.unwrap().is_none());
        assert_eq!(activity, PumpActivity::Disconnect);
        assert!(!grab.is_grabbed(), "dead fd must release the grab");
        assert!(!px.is_attached());
        assert!(px.is_passthrough());
        // Settled passthrough: further steps are Idle and never poll the
        // dead source again (logs bounded by construction).
        let remaining = px.source.frames.borrow().len();
        let (_, a2) = px.pump_step_at(&cfg, ms_after(t0, 120));
        assert_eq!(a2, PumpActivity::Idle);
        assert_eq!(px.source.frames.borrow().len(), remaining);
    }

    #[test]
    fn next_deadline_covers_pending_force_cut_stages() {
        // Arm a force-cut staged press: the edge frame at t0+10 leaves
        // `pending_force` with `mods_at=None`, so the deadline must be
        // `released_at + 5 ms`; once the modifiers land at the floor, the
        // deadline must move to `mods_at + stagger` (50 ms here).
        let sinks = TestSinks::new();
        let haptic = Arc::new(FakeHaptic::new());
        let mut px = drive_ready_proxy(
            &sinks,
            &haptic,
            vec![slide_report(5, 60, 0), slide_report(5, 200, 0)],
        );
        let mut cfg = five_slide_cfg();
        cfg.click.click_5 = "super+left".into();
        cfg.force.force_5 = "super+left".into();
        let t0 = std::time::Instant::now();
        px.handle_frame_at(&cfg, ms_after(t0, 0)).unwrap();
        px.handle_frame_at(&cfg, ms_after(t0, 10)).unwrap();
        assert!(px.pending_force.is_some(), "cut edge must stage a press");
        assert!(px.machine.force_active().is_some(), "force latch must hold");
        assert!(
            px.pending_force.as_ref().unwrap().mods_at.is_none(),
            "modifiers not landed yet"
        );
        let d0 = px.next_deadline().expect("cut stage-1 deadline");
        assert_eq!(
            d0,
            ms_after(t0, 15),
            "stage 1 due at released_at + 5 ms floor"
        );
        // Idle to the floor: modifiers land, buttons wait the stagger.
        px.handle_frame_at(&cfg, ms_after(t0, 15)).unwrap();
        let pending = px.pending_force.as_ref().expect("stage 2 still pending");
        let mods_at = pending.mods_at.expect("modifiers landed");
        assert_eq!(mods_at, ms_after(t0, 15));
        let d1 = px.next_deadline().expect("cut stage-2 deadline");
        assert_eq!(
            d1,
            mods_at + std::time::Duration::from_millis(50),
            "stage 2 due at mods_at + stagger"
        );
        assert_eq!(d1, ms_after(t0, 65));
    }
}
