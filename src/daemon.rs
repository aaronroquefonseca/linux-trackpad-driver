//! Daemon lifecycle: `run` loop + `reconnect` udev entry.
//!
//! `run`: acquires [`SocketLock`], discovers devices, runs init per node
//! (USB feature reports / BT single-node), emits [`DeviceReadyEvent`] on the
//! shared [`DeviceReadyHub`], then idles until a shutdown signal
//! (SIGINT/SIGTERM/SIGHUP via [`install_shutdown_handler`]). No devices =
//! log + idle (graceful, exit 0 on signal), never panic.
//!
//! Shutdown is owned by [`DaemonSession`]: the signal flips a
//! process-lifetime static flag, the loop exits, the session establishes
//! stop FIRST then stops + bounded-joins the pump thread with
//! worker-panic reporting, tears down best-effort within a bounded proxy
//! acquire (grab released, held keys/contacts up with trailing `SYN`, both
//! virtual nodes destroyed, source idled), drains queued actuator pulses,
//! and only then emits the observable `Detached` for the torn-down node.
//! When the timed-out worker still owns the proxy, teardown is skipped,
//! `cleanup_incomplete` is reported, and process exit reclaims the kernel
//! grab. Releasing the grab restores native input (the daemon never unbinds
//! drivers, so no hardware re-arm is owed). Teardown is idempotent and
//! bounded on every path (explicit, `Drop`, repeat); [`Drop`] repeats it
//! best-effort so a panic-unwind path still stops the pump thread.
//!
//! `reconnect <dev>`: idempotent per-node re-init for udev `add`.
//! Activation contract (finding #14): the main service is the only automatic
//! activator. Daemon socket absent without `-f`/`-d` → inactive: leave the
//! hardware alone and exit 0 (no init, no disengage write). `-f` forces init
//! anyway (explicit manual bring-up); `-d` always disengages (explicit manual
//! disengage). `-p` overrides the polling request (same ~110 Hz cap). Init
//! always re-sends multitouch enable first: the resume regression (report
//! 0x02 21→8 bytes across power transition) is only fixed by re-init.
//!
//! `resume`: post-sleep re-init over every discovered node. Same contract:
//! daemon socket absent without `-f` → inactive no-op (exit 0, hardware left
//! alone). Plain `resume` is the automatic background form used by the
//! system-sleep hook and the resume unit; `-f` is explicit manual forcing
//! while the daemon is stopped.

use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::{Context, Result};

use crate::apple_parse::TouchSource;
use crate::config::Config;
use crate::discovery::{self, DiscoveredDevice};
use crate::init::{
    self, DeviceReadyEvent, DeviceReadyHub, HidrawSender, Transport, EFFECTIVE_CAP_HZ,
};
use crate::lock::{self, SocketLock};
use crate::proxy::{
    AsyncHaptic, ClickProxy, DualSink, GrabDevice, HapticSender, ProxySettings, RealGrab,
    RealHaptic, RealUinput, UinputSink,
};
use crate::pump::PumpSource;
use crate::resume::{self, ResumeReport};

/// Shared device-ready hub for this process. The daemon proxy subscribes.
pub fn hub() -> DeviceReadyHub {
    DeviceReadyHub::new()
}

/// Shared pump lifecycle: attach tail (grab-refused rollback), detach and
/// shutdown teardown. One generic manager serves production (real
/// grab/uinput/haptic/source) and unit tests (fakes): the rollback,
/// detach and `Drop` paths below are the actual dispatch under test, not
/// copies. The idle source is a plain `fn` pointer (never captures), and
/// tests inject their live source via [`PumpManager::finish_attach`].
pub(crate) struct PumpManager<G, U, H, S>
where
    G: GrabDevice,
    U: UinputSink,
    H: HapticSender,
    S: TouchSource,
{
    proxy: std::sync::Arc<std::sync::Mutex<ClickProxy<G, U, H, S>>>,
    cfg: Config,
    active: std::sync::Mutex<Option<ActiveSession>>,
    haptic_node: std::sync::Mutex<Option<String>>,
    stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
    idle: fn() -> S,
    control: Option<std::sync::Arc<crate::pump::PumpControl>>,
    thread: std::sync::Mutex<Option<std::thread::JoinHandle<()>>>,
    shutdown_once: AtomicBool,
}

impl<G, U, H, S> PumpManager<G, U, H, S>
where
    G: GrabDevice,
    U: UinputSink,
    H: HapticSender,
    S: TouchSource,
{
    pub(crate) fn new(
        proxy: std::sync::Arc<std::sync::Mutex<ClickProxy<G, U, H, S>>>,
        cfg: Config,
        stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
        idle: fn() -> S,
    ) -> Self {
        Self {
            proxy,
            cfg,
            active: std::sync::Mutex::new(None),
            haptic_node: std::sync::Mutex::new(None),
            stop,
            idle,
            control: None,
            thread: std::sync::Mutex::new(None),
            shutdown_once: AtomicBool::new(false),
        }
    }

    pub(crate) fn with_thread(self, handle: std::thread::JoinHandle<()>) -> Self {
        *self.thread.lock().unwrap_or_else(|e| e.into_inner()) = Some(handle);
        self
    }

    pub(crate) fn with_control(
        mut self,
        control: std::sync::Arc<crate::pump::PumpControl>,
    ) -> Self {
        self.control = Some(control);
        self
    }

    /// Wake the event-driven pump (#86) so it re-reads its hidraw fd and
    /// deadline set instead of blocking past this attach/detach. No-op
    /// without a pump (unit tests).
    fn notify_pump(&self) {
        if let Some(control) = &self.control {
            control.notify();
        }
    }

    /// Borrow the managed proxy (tests drive frames / assert wire through this).
    #[cfg(test)]
    pub(crate) fn proxy(&self) -> &std::sync::Arc<std::sync::Mutex<ClickProxy<G, U, H, S>>> {
        &self.proxy
    }

    /// Shared attach tail under one proxy lock: run caller wiring (grab /
    /// haptic node selection), swap in the live source, flip pump-ready,
    /// then grab. A refused grab rolls back through the single teardown
    /// (held keys + virtual contacts terminated when possible) plus idle
    /// source and node destroy, so no orphan nodes linger.
    pub(crate) fn finish_attach(
        &self,
        event: &DeviceReadyEvent,
        source: S,
        wire: impl FnOnce(&mut ClickProxy<G, U, H, S>),
    ) {
        let Some(mut px) = lock_bounded(&self.proxy, PROXY_ACQUIRE_TIMEOUT) else {
            log::warn!("pump: proxy busy, deferring attach for {}", event.devnode);
            return;
        };
        wire(&mut px);
        px.replace_source(source);
        px.set_pump_ready(true);
        px.attach(event);
        if px.is_attached() {
            // Release the proxy before taking `active` so shutdown's
            // bounded active acquire never waits on this wiring.
            drop(px);
            let Some(mut active) = lock_bounded(&self.active, PROXY_ACQUIRE_TIMEOUT) else {
                log::warn!(
                    "pump: active busy after attach for {}; session wired, tracking deferred",
                    event.devnode
                );
                self.notify_pump();
                return;
            };
            *active = Some(ActiveSession {
                devnode: event.devnode.clone(),
                transport: event.transport,
            });
        } else {
            Self::teardown_locked(&mut px, self.idle, "attach rollback: grab refused");
        }
        self.notify_pump();
    }

    /// Idempotent manager teardown: single proxy teardown (contacts +
    /// keys ended on the wire) + source to idle + virtual nodes destroyed.
    /// Used by shutdown (via `Drop`); detach/rollback share it.
    fn teardown_locked(proxy: &mut ClickProxy<G, U, H, S>, idle: fn() -> S, reason: &str) {
        proxy.teardown_session(reason);
        proxy.replace_source(idle());
        proxy.uinput().destroy();
    }

    pub(crate) fn detach(&self, event: &crate::init::DeviceReadyEvent) {
        use crate::init::ReadyKind;
        if event.kind != ReadyKind::Detached {
            return;
        }
        // Bounded active take: never wait indefinitely on a concurrent
        // attach holding `active` while wedged on the worker-owned proxy.
        let session = {
            let Some(mut active) = lock_bounded(&self.active, PROXY_ACQUIRE_TIMEOUT) else {
                log::warn!(
                    "pump: active busy, skipping Detached for {} (shutdown covers it)",
                    event.devnode
                );
                return;
            };
            if active.as_ref().is_some_and(|s| s.devnode == event.devnode) {
                active.take()
            } else {
                None
            }
        };
        let Some(session) = session else {
            // Sibling vanishing while another node owns the pump: touch
            // nothing. The proxy itself also ignores foreign Detached
            // events (ownership gate in `ClickProxy::detach`), so the
            // live grab survives even if this gate is ever bypassed.
            log::debug!("pump: ignoring Detached for unowned {}", event.devnode);
            return;
        };
        // Best-effort bounded teardown: a wedged worker owns the proxy.
        // `shutdown_inner` (or process exit) is the fallback; never hang.
        if !self.try_teardown_session(&session, PROXY_ACQUIRE_TIMEOUT) {
            log::error!(
                "pump: proxy busy, Detached for {} deferred to shutdown/exit",
                session.devnode
            );
            return;
        }
        self.notify_pump();
    }

    /// Snapshot of the owned session, if any.
    fn active_session(&self) -> Option<ActiveSession> {
        // `active` is never worker-owned (only attach/detach/shutdown take
        // it, all bounded); a blocking poison-recovering lock is safe here
        // and keeps the read path simple for reconciliation.
        self.active
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// Tear down one owned session: proxy detach (held keys/contacts up
    /// with trailing `SYN`), pump marked unready, source idled, both
    /// virtual nodes destroyed. The caller takes `active` first, so this
    /// runs at most once per session.
    /// Bounded: returns `false` when the worker still owns the proxy
    /// within `timeout`; the caller reports incomplete cleanup instead of
    /// hanging.
    fn try_teardown_session(&self, session: &ActiveSession, timeout: std::time::Duration) -> bool {
        let event = init::detached(&session.devnode, session.transport);
        let Some(mut proxy) = lock_bounded(&self.proxy, timeout) else {
            return false;
        };
        proxy.detach(&event);
        proxy.set_pump_ready(false);
        proxy.replace_source((self.idle)());
        proxy.uinput().destroy();
        log::info!("pump: {} torn down", session.devnode);
        true
    }
}

impl<G, U, H, S> Drop for PumpManager<G, U, H, S>
where
    G: GrabDevice,
    U: UinputSink,
    H: HapticSender,
    S: TouchSource,
{
    /// Shutdown path: terminate virtual contacts + keys, idle the
    /// source, destroy virtual nodes. Idempotent: safe after an
    /// explicit detach already tore the session down. A poisoned proxy
    /// mutex is recovered, never a reason to skip cleanup. The pump thread
    /// is signaled to stop; its handle detaches with the manager at
    /// process teardown (existing lifecycle, see finding #12).
    fn drop(&mut self) {
        self.stop.store(true, std::sync::atomic::Ordering::SeqCst);
        // Wake a poll-blocked event-driven pump immediately: it must
        // observe stop without waiting for the next deadline, on every
        // Drop path — before any wait or early return below.
        self.notify_pump();
        // An orderly shutdown already ran the full bounded path; never
        // wait twice.
        if self.shutdown_once.swap(true, Ordering::SeqCst) {
            return;
        }
        match lock_bounded(&self.proxy, PROXY_ACQUIRE_TIMEOUT) {
            Some(mut px) => Self::teardown_locked(&mut px, self.idle, "shutdown"),
            None => log::error!("pump: proxy busy at drop; process exit reclaims the grab"),
        }
    }
}

// ---- orderly shutdown: signals ----

/// Signals that request orderly daemon shutdown: terminal interrupt,
/// service-manager stop, and terminal hangup. All three flip the
/// process-lifetime flag (see [`install_shutdown_handler`]); the main loop
/// then runs the owned teardown in [`DaemonSession::shutdown`].
pub fn shutdown_signals() -> [libc::c_int; 3] {
    [libc::SIGINT, libc::SIGTERM, libc::SIGHUP]
}

/// Process-lifetime shutdown flag for the handlers installed by
/// [`install_shutdown_handler`]. A `static` (not a reclaimed raw pointer)
/// so the handler target is valid for every concurrent/in-flight handler
/// by construction: no allocation, no lifetime, no reclamation race.
/// Reset to `true` in [`install_shutdown_handler`] (outside any handler);
/// flipped to `false` only by the async-signal-safe handler.
static SHUTDOWN_RUNNING: AtomicBool = AtomicBool::new(true);

/// Serializes tests sharing process-global signal state (dispositions +
/// [`SHUTDOWN_RUNNING`]). Production installs once per process; tests must
/// hold this across install/raise/drop so parallel tests never race the
/// global dispositions.
#[cfg(test)]
pub(crate) static SIGNAL_TEST_SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// True once a shutdown signal arrived (handler flipped the static flag).
pub fn shutdown_requested() -> bool {
    !SHUTDOWN_RUNNING.load(Ordering::SeqCst)
}

/// True while the daemon should keep running (no shutdown signal yet).
pub fn shutdown_running() -> bool {
    SHUTDOWN_RUNNING.load(Ordering::SeqCst)
}

/// Shutdown request handler: flip the static flag, nothing else. Logging
/// and teardown happen on the main loop, never in the handler, keeping this
/// async-signal-safe (atomic store only: no allocation, locks, logging or
/// `Arc` operations).
extern "C" fn shutdown_signal_handler(_sig: libc::c_int) {
    // Static lifetime: safe for handlers already loaded/running across
    // guard drop or re-install; a late store is harmless.
    SHUTDOWN_RUNNING.store(false, Ordering::SeqCst);
}

/// Restores previous signal dispositions on drop. The static flag is left
/// alone: in-flight handlers storing to it after restore remain safe, and
/// the next [`install_shutdown_handler`] resets it explicitly.
pub struct ShutdownSignalGuard {
    previous: Vec<(libc::c_int, libc::sigaction)>,
}

impl Drop for ShutdownSignalGuard {
    fn drop(&mut self) {
        for (sig, prev) in std::mem::take(&mut self.previous) {
            unsafe {
                libc::sigaction(sig, &prev, std::ptr::null_mut());
            }
        }
    }
}

/// Install [`shutdown_signals`] handlers flipping the process-lifetime flag.
/// Production calls this once per process; repeated installs reset the flag
/// to `true` and reinstall (idempotent). Tests sharing global state must
/// serialize via `SIGNAL_TEST_SERIAL`. A partial-install failure rolls back
/// what was set.
pub fn install_shutdown_handler() -> Result<ShutdownSignalGuard> {
    // Reset outside any handler, before installing.
    SHUTDOWN_RUNNING.store(true, Ordering::SeqCst);
    let mut previous = Vec::new();
    unsafe {
        let mut action: libc::sigaction = std::mem::zeroed();
        action.sa_sigaction = shutdown_signal_handler as *const () as usize;
        libc::sigemptyset(&mut action.sa_mask);
        action.sa_flags = libc::SA_RESTART;
        for sig in shutdown_signals() {
            let mut old: libc::sigaction = std::mem::zeroed();
            if libc::sigaction(sig, &action, &mut old) != 0 {
                let errno = std::io::Error::last_os_error();
                for (done, prev) in previous {
                    libc::sigaction(done, &prev, std::ptr::null_mut());
                }
                anyhow::bail!("installing handler for signal {sig}: {errno}");
            }
            previous.push((sig, old));
        }
    }
    Ok(ShutdownSignalGuard { previous })
}

// ---- owned daemon session ----

type DaemonProxy =
    ClickProxy<RealGrab, DualSink<RealUinput, RealUinput>, AsyncHaptic<RealHaptic>, PumpSource>;

/// Device wiring owned by [`PumpManager`]: which hidraw node holds the grab
/// plus the transport for its shutdown `Detached` event.
#[derive(Debug, Clone)]
pub(crate) struct ActiveSession {
    devnode: String,
    transport: Transport,
}

/// Bound for stopping the pump thread at shutdown: the loop checks the
/// flag every step (2 ms idle sleep), so anything beyond this means a
/// frame handler is wedged on hardware I/O.
pub const PUMP_JOIN_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// Bound for acquiring the worker-owned proxy mutex during shutdown and
/// synchronous detach. The pump holds this mutex across `handle_frame`,
/// so a wedged frame owns it; shutdown must never wait on it indefinitely.
/// One second is generous for a healthy frame (milliseconds) yet keeps the
/// total bounded teardown under ~8 s (5 s join + 1 s proxy + 2 s haptic).
pub const PROXY_ACQUIRE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(1);

/// Result of [`DaemonSession::shutdown`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ShutdownOutcome {
    /// A device session was still wired when shutdown began.
    pub had_session: bool,
    /// The pump worker thread panicked; the message went to the log.
    pub worker_panicked: bool,
    /// The pump thread did not stop within the bound and was detached.
    pub pump_join_timed_out: bool,
    /// Proxy/haptic cleanup could not finish within its bound because the
    /// timed-out worker still owns the proxy mutex. The grab is left for
    /// process-exit reclaim (kernel releases it); the exit must fail so
    /// the incomplete cleanup surfaces instead of claiming success.
    pub cleanup_incomplete: bool,
}

impl ShutdownOutcome {
    fn none() -> Self {
        Self {
            had_session: false,
            worker_panicked: false,
            pump_join_timed_out: false,
            cleanup_incomplete: false,
        }
    }

    /// True when shutdown did not fully succeed (panic, timeout, or
    /// incomplete proxy/haptic cleanup). Callers fail the exit on this.
    pub fn failed(&self) -> bool {
        self.worker_panicked || self.pump_join_timed_out || self.cleanup_incomplete
    }
}

/// Bounded mutex acquisition for shutdown/detach paths: poll `try_lock`
/// until `timeout`, recovering poison immediately. Returns `None` on
/// timeout so the caller reports incomplete cleanup instead of hanging.
/// The 1 ms poll keeps latency low without busy-spinning.
fn lock_bounded<'a, T>(
    mutex: &'a std::sync::Mutex<T>,
    timeout: std::time::Duration,
) -> Option<std::sync::MutexGuard<'a, T>> {
    let deadline = std::time::Instant::now() + timeout;
    loop {
        match mutex.try_lock() {
            Ok(guard) => return Some(guard),
            Err(std::sync::TryLockError::Poisoned(e)) => return Some(e.into_inner()),
            Err(std::sync::TryLockError::WouldBlock) => {}
        }
        if std::time::Instant::now() >= deadline {
            return None;
        }
        std::thread::sleep(std::time::Duration::from_millis(1));
    }
}

/// Production pump manager: the generic lifecycle seam above instantiated
/// with real grab/uinput/haptic/source.
type DaemonManager =
    PumpManager<RealGrab, DualSink<RealUinput, RealUinput>, AsyncHaptic<RealHaptic>, PumpSource>;
impl DaemonManager {
    fn attach(&self, event: &crate::init::DeviceReadyEvent) {
        use crate::init::ReadyKind;
        if event.kind != ReadyKind::Ready {
            return;
        }
        // No new attach once shutdown owns the teardown: avoids racing the
        // bounded shutdown sequence on worker-owned state.
        if self.shutdown_once.load(Ordering::SeqCst) {
            return;
        }
        if !self.cfg.enhanced_clicks {
            let Some(mut px) = lock_bounded(&self.proxy, PROXY_ACQUIRE_TIMEOUT) else {
                log::warn!("pump: proxy busy, deferring attach for {}", event.devnode);
                return;
            };
            px.attach(event);
            self.notify_pump();
            return;
        }
        // Enumeration can expose a silent feature interface before
        // the If1 touch source. Do not let that early sibling own the
        // single pump; its later If1 event will wake reconciliation.
        if event.transport == Transport::Usb
            && !touch_interface_eligible(discovery::interface_number_for_node(&event.devnode))
        {
            log::debug!(
                "pump: {} is not the USB touch interface; waiting for If1",
                event.devnode
            );
            return;
        }
        {
            let Some(active) = lock_bounded(&self.active, PROXY_ACQUIRE_TIMEOUT) else {
                log::warn!("pump: active busy, deferring attach for {}", event.devnode);
                return;
            };
            let live = match lock_bounded(&self.proxy, PROXY_ACQUIRE_TIMEOUT) {
                Some(px) => px.is_attached(),
                None => {
                    log::warn!("pump: proxy busy, deferring attach for {}", event.devnode);
                    return;
                }
            };
            match attach_decision(
                active.as_ref().map(|s| s.devnode.as_str()),
                &event.devnode,
                live,
            ) {
                AttachDecision::Adopt => {}
                AttachDecision::IgnoreDuplicate => return,
                AttachDecision::IgnoreSibling => {
                    log::info!(
                        "pump: {} ready but {} active; ignoring sibling",
                        event.devnode,
                        active.as_ref().map(|s| s.devnode.as_str()).unwrap_or("?"),
                    );
                    return;
                }
                AttachDecision::Reattach => {
                    log::info!(
                        "pump: {} known but session dead; tearing down + re-attaching",
                        event.devnode
                    );
                    drop(active);
                    self.detach(&init::detached(&event.devnode, event.transport));
                }
            }
        }
        // Proven-selection log (INFO): which node, which USB interface,
        // and WHY it sorts first (If1 pump rule preferred, MT abs bits
        // tiebreak only). Owner 2026-10-15: silent hidraw8=If0 won on
        // position/MT-rank alone while hidraw9=If1 streamed — this line
        // is the journal proof of the fix.
        {
            let iface = discovery::interface_number_for_node(&event.devnode);
            let reason = discovery::selection_reason_for_node(&event.devnode);
            log::info!(
                "pump: attaching {} (interface {:?}, {})",
                event.devnode,
                iface,
                reason,
            );
        }
        // 1. Create the virtual nodes BEFORE grab: pointer first (the
        // touch emit path grab safety depends on), then extras. A
        // pointer failure stays passthrough; an extras failure only
        // drops combos (pointer-only session, loud log).
        // Bounded: a wedged worker owns the proxy; defer to reconciliation
        // retry instead of hanging the hub callback.
        let created = {
            let Some(px) = lock_bounded(&self.proxy, PROXY_ACQUIRE_TIMEOUT) else {
                log::warn!("pump: proxy busy, deferring attach for {}", event.devnode);
                return;
            };
            match px.uinput().create_pointer() {
                Ok(()) => true,
                Err(e) => {
                    log::warn!(
                        "pump: pointer uinput create failed for {}: {e:#}; passthrough",
                        event.devnode
                    );
                    false
                }
            }
        };
        if !created {
            let Some(mut px) = lock_bounded(&self.proxy, PROXY_ACQUIRE_TIMEOUT) else {
                log::warn!("pump: proxy busy, deferring attach for {}", event.devnode);
                return;
            };
            px.attach(event);
            self.notify_pump();
            return;
        }
        {
            let Some(px) = lock_bounded(&self.proxy, PROXY_ACQUIRE_TIMEOUT) else {
                log::warn!("pump: proxy busy, deferring attach for {}", event.devnode);
                return;
            };
            if let Err(e) = px.uinput().create_extras() {
                log::warn!(
                    "pump: extras uinput create failed for {}: {e:#}; continuing pointer-only (combos dropped)",
                    event.devnode
                );
            }
        }
        // 2. Resolve the physical node (env override → sysfs → name).
        let Some((evdev, src)) = crate::evdev::resolve_evdev_node(&event.devnode) else {
            log::warn!(
                "pump: no evdev for {}; destroying uinput nodes, passthrough",
                event.devnode
            );
            if let Some(px) = lock_bounded(&self.proxy, PROXY_ACQUIRE_TIMEOUT) {
                px.uinput().destroy();
            } else {
                log::warn!(
                    "pump: proxy busy, leaking uinput nodes until retry for {}",
                    event.devnode
                );
            }
            self.notify_pump();
            return;
        };
        if !crate::evdev::grab_allowed(&src) {
            // NameScan hit: untrusted for grabbing (no hidraw
            // parentage proof — could be a foreign node). Refuse the
            // grab, destroy the node we just created, passthrough
            // with a LOUD log. An input-less feature interface
            // (hidraw10/11) lands here by design; the touch
            // interface's sysfs-backed Ready attaches separately.
            log::error!(
                "pump: evdev for {} resolved via {:?} (UNTRUSTED, no grab); destroying uinput nodes, passthrough",
                event.devnode,
                src,
            );
            if let Some(px) = lock_bounded(&self.proxy, PROXY_ACQUIRE_TIMEOUT) {
                px.uinput().destroy();
            } else {
                log::warn!(
                    "pump: proxy busy, leaking uinput nodes until retry for {}",
                    event.devnode
                );
            }
            self.notify_pump();
            return;
        }
        // 3. Open the hidraw source nonblocking.
        let source = match crate::pump::HidrawSource::open(&event.devnode) {
            Ok(s) => s,
            Err(e) => {
                log::warn!(
                    "pump: hidraw open {} failed: {e:#}; destroying uinput nodes, passthrough",
                    event.devnode
                );
                if let Some(px) = lock_bounded(&self.proxy, PROXY_ACQUIRE_TIMEOUT) {
                    px.uinput().destroy();
                } else {
                    log::warn!(
                        "pump: proxy busy, leaking uinput nodes until retry for {}",
                        event.devnode
                    );
                }
                self.notify_pump();
                return;
            }
        };
        // 3b. Attach-time stream verify: nonblocking poll of the chosen
        // node + siblings with a short timeout. Chosen silent while a
        // sibling streams = the 2026-10-15 misbind (silent hidraw8 vs
        // live hidraw9): loud-warn naming both nodes + hint. All
        // silent = finger-off idle, debug only (no false alarm).
        verify_attach_stream(&event.devnode);
        // 4. Wire nodes, flip ready, then grab (attach checks both gates).
        // Haptics go to the interface-2 sibling when the USB layout
        // exposes one (actuator interface); otherwise the touch node
        // itself, loudly logged as pending HW verify.
        // Prepare (open) the haptic endpoint BEFORE the proxy lock in
        // `finish_attach`: the open can block on kernel activity and the
        // pump needs that mutex for every frame. The commit below only
        // swaps Arcs and hands disposal to a background thread. The tail
        // (swap in the live source, grab, adopt-or-rollback) is the shared
        // `finish_attach` dispatch covered by manager unit tests.
        let (hnode, hiface) = crate::evdev::haptic_node_for(&event.devnode);
        let haptic_target = crate::proxy::HapticTarget::prepare(
            &hnode,
            event.transport == crate::init::Transport::Bluetooth,
        );
        self.finish_attach(event, PumpSource::Live(source), |px| {
            px.grab().set_node(&evdev.to_string_lossy());
            // New session: retire any queued pulses from the previous
            // node and pin the prepared actuator target before the pump
            // enqueues.
            px.haptic_mut().retarget_prepared(haptic_target);
            *self.haptic_node.lock().unwrap_or_else(|e| e.into_inner()) = Some(hnode.clone());
            log::info!(
                "pump: haptic node {hnode} (interface {hiface:?}) for {}",
                event.devnode
            );
        });
    }

    /// Health query for the reconciliation loop: true when `devnode`
    /// still needs an attachment attempt. Bounded: a wedged worker owns
    /// the proxy; on timeout return `false` (no attach now, retry on the
    /// next pass) instead of stalling the loop that also watches the
    /// shutdown flag. Late-sibling haptic routes commit generation-fenced
    /// (#85): resolve + open outside the proxy mutex, commit only when
    /// the session is still owned by the same generation.
    fn needs_attach(&self, devnode: &str) -> bool {
        if !self.cfg.enhanced_clicks
            || !touch_interface_eligible(discovery::interface_number_for_node(devnode))
        {
            return false;
        }
        // Snapshot live/owner/routing/generation under brief bounded
        // locks; no I/O. Route resolution and endpoint open happen below
        // outside the proxy mutex so a blocked open never stalls frames.
        let (live, owned, cur_bt, baseline_gen) = {
            let Some(active) = lock_bounded(&self.active, PROXY_ACQUIRE_TIMEOUT) else {
                return false;
            };
            let Some(mut proxy) = lock_bounded(&self.proxy, PROXY_ACQUIRE_TIMEOUT) else {
                return false;
            };
            let live = proxy.is_attached();
            let owned = active.as_ref().is_some_and(|s| s.devnode == devnode);
            let (_, cur_bt) = proxy.haptic_mut().current_routing();
            let baseline_gen = proxy.haptic_mut().generation();
            (live, owned, cur_bt, baseline_gen)
        };
        if live && owned {
            // A later actuator sibling must not require a re-grab of the
            // healthy touch session. Resolve quietly; log only a route change.
            // Resolution may touch sysfs; keep it outside all live locks.
            let (node, _) = crate::evdev::haptic_node_for_quiet(devnode);
            let needs = {
                let tracked = self.haptic_node.lock().unwrap_or_else(|e| e.into_inner());
                tracked.as_deref() != Some(node.as_str())
            };
            if needs {
                // Open outside the proxy mutex; commit generation-fenced
                // below. A blocked open leaves the pump responsive.
                let prepared = crate::proxy::HapticTarget::prepare(&node, cur_bt);
                let Some(active) = lock_bounded(&self.active, PROXY_ACQUIRE_TIMEOUT) else {
                    return false;
                };
                let Some(mut proxy) = lock_bounded(&self.proxy, PROXY_ACQUIRE_TIMEOUT) else {
                    return false;
                };
                let still_owned =
                    proxy.is_attached() && active.as_ref().is_some_and(|s| s.devnode == devnode);
                if still_owned
                    && proxy
                        .haptic_mut()
                        .update_route_prepared(prepared, baseline_gen)
                {
                    *self.haptic_node.lock().unwrap_or_else(|e| e.into_inner()) =
                        Some(node.clone());
                    log::info!("pump: late sibling updated haptic node to {node} for {devnode}");
                }
                let live = proxy.is_attached();
                return session_needs_attach(
                    self.cfg.enhanced_clicks,
                    active.as_ref().map(|s| s.devnode.as_str()),
                    devnode,
                    live,
                );
            }
        }
        let Some(active) = lock_bounded(&self.active, PROXY_ACQUIRE_TIMEOUT) else {
            return false;
        };
        let Some(proxy) = lock_bounded(&self.proxy, PROXY_ACQUIRE_TIMEOUT) else {
            return false;
        };
        let live = proxy.is_attached();
        session_needs_attach(
            self.cfg.enhanced_clicks,
            active.as_ref().map(|s| s.devnode.as_str()),
            devnode,
            live,
        )
    }

    fn shutdown_inner(&self) -> ShutdownOutcome {
        self.shutdown_inner_with_budget(
            PUMP_JOIN_TIMEOUT,
            PROXY_ACQUIRE_TIMEOUT,
            crate::proxy::HAPTIC_SHUTDOWN_TIMEOUT,
        )
        .0
    }

    /// [`shutdown_inner`] with injected budgets for fast deterministic
    /// tests. Returns the outcome plus the torn-down session (for the
    /// post-teardown hub emit in [`DaemonSession::shutdown_with_budget`]).
    pub(crate) fn shutdown_inner_with_budget(
        &self,
        join_timeout: std::time::Duration,
        proxy_timeout: std::time::Duration,
        haptic_timeout: std::time::Duration,
    ) -> (ShutdownOutcome, Option<ActiveSession>) {
        if self.shutdown_once.swap(true, Ordering::SeqCst) {
            log::debug!("pump: shutdown already ran; ignoring repeat");
            return (ShutdownOutcome::none(), None);
        }
        // Establish stop FIRST, before any callback or mutex wait: the
        // wedged worker observes this and exits promptly when able, and
        // every later bounded wait is measured against an already-running
        // budget. Wake a poll-blocked pump immediately so it observes stop
        // without waiting for the next deadline.
        self.stop.store(true, Ordering::SeqCst);
        self.notify_pump();
        // `thread` is never worker-owned (only shutdown takes it), so a
        // blocking poison-recovering lock is safe; the join itself is
        // bounded via `join_bounded` (detaches a wedged worker).
        let (worker_panicked, pump_join_timed_out) =
            match self.thread.lock().unwrap_or_else(|e| e.into_inner()).take() {
                Some(handle) => match crate::pump::join_bounded(handle, join_timeout) {
                    crate::pump::WorkerJoin::Completed(()) => (false, false),
                    crate::pump::WorkerJoin::Panicked(msg) => {
                        log::error!("pump worker thread panicked: {msg}");
                        (true, false)
                    }
                    crate::pump::WorkerJoin::TimedOut => {
                        log::error!("pump thread did not stop within {join_timeout:?}; detached");
                        (false, true)
                    }
                },
                None => (false, false),
            };
        // Bounded active take: a concurrent attach may briefly hold it
        // while wedged on the proxy; time out instead of hanging.
        let session = match lock_bounded(&self.active, proxy_timeout) {
            Some(mut active) => active.take(),
            None => {
                log::error!("pump: active busy at shutdown; cleanup incomplete");
                let outcome = ShutdownOutcome {
                    had_session: true,
                    worker_panicked,
                    pump_join_timed_out,
                    cleanup_incomplete: true,
                };
                return (outcome, None);
            }
        };
        let mut had_session = session.is_some();
        let mut cleanup_incomplete = false;
        // Single bounded proxy scope for teardown + force-release + drain:
        // one acquisition, no wait-again after a timeout.
        match lock_bounded(&self.proxy, proxy_timeout) {
            Some(mut proxy) => {
                if let Some(ref s) = session {
                    let event = init::detached(&s.devnode, s.transport);
                    proxy.detach(&event);
                    proxy.set_pump_ready(false);
                    proxy.replace_source(PumpSource::Idle);
                    proxy.uinput().destroy();
                    log::info!("pump: {} torn down", s.devnode);
                }
                if proxy.is_attached() {
                    // The hub callback claimed `active` but left a live grab
                    // behind: force the release so shutdown never strands
                    // native input.
                    log::warn!("pump: live session survived detach; forcing release");
                    had_session = true;
                    proxy.set_pump_ready(false);
                    proxy.replace_source(PumpSource::Idle);
                    proxy.uinput().destroy();
                }
                // Draining while holding the proxy is safe: the haptic
                // worker never needs the proxy mutex (it owns an `Arc` to
                // the inner sender). Bounded via `AsyncHaptic::shutdown`.
                proxy.haptic_mut().shutdown(haptic_timeout);
            }
            None => {
                log::error!(
                    "pump: proxy busy at shutdown (worker owns it); \
                     skipping teardown/drain, process exit reclaims the grab"
                );
                cleanup_incomplete = true;
                // `had_session` stays true when a session was taken but
                // could not be torn down; when no session was taken the
                // live grab (if any) is unreachable, still incomplete.
                if session.is_none() {
                    had_session = true;
                }
            }
        }
        let outcome = ShutdownOutcome {
            had_session,
            worker_panicked,
            pump_join_timed_out,
            cleanup_incomplete,
        };
        (outcome, session)
    }
}

/// Owned daemon session: the pump thread, the click proxy, and the active
/// device wiring behind one guard.
///
/// [`subscribe_click_proxy`] builds it (spawning the pump thread and
/// subscribing the hub); [`DaemonSession::shutdown`] performs the orderly
/// teardown; [`Drop`] repeats it best-effort so an early return or a
/// panic-unwind path still stops the pump thread instead of leaking it
/// until process exit.
pub struct DaemonSession {
    manager: std::sync::Arc<DaemonManager>,
}

impl DaemonSession {
    /// Health query for the reconciliation loop: true when `devnode`
    /// still needs an attachment attempt.
    pub fn needs_attach(&self, devnode: &str) -> bool {
        self.manager.needs_attach(devnode)
    }

    /// Currently owned session, if any.
    pub fn active_session(&self) -> Option<(String, Transport)> {
        self.manager
            .active_session()
            .map(|s| (s.devnode, s.transport))
    }

    /// Orderly shutdown: run the single bounded teardown FIRST (stop is
    /// established before any callback or mutex wait, then join, then
    /// best-effort proxy/haptic cleanup), and only then emit the
    /// observable `Detached` for the torn-down session. Emitting after the
    /// take makes the manager's own hub callback a no-op (active already
    /// `None`), so there is exactly one cleanup path and no blocking
    /// Detached runs before the deadline. Other subscribers still observe
    /// the detach. When the worker still owns the proxy, teardown is
    /// skipped, `cleanup_incomplete` is set, and process exit reclaims the
    /// kernel grab. Idempotent: repeats find no session and no live
    /// thread, emit nothing, and wait for nothing.
    pub fn shutdown(&self, hub: &DeviceReadyHub) -> ShutdownOutcome {
        self.shutdown_with_budget(
            hub,
            PUMP_JOIN_TIMEOUT,
            PROXY_ACQUIRE_TIMEOUT,
            crate::proxy::HAPTIC_SHUTDOWN_TIMEOUT,
        )
    }

    /// [`shutdown`] with injected budgets for fast deterministic tests.
    pub(crate) fn shutdown_with_budget(
        &self,
        hub: &DeviceReadyHub,
        join_timeout: std::time::Duration,
        proxy_timeout: std::time::Duration,
        haptic_timeout: std::time::Duration,
    ) -> ShutdownOutcome {
        let (outcome, session) =
            self.manager
                .shutdown_inner_with_budget(join_timeout, proxy_timeout, haptic_timeout);
        // Emit AFTER teardown so the manager's own Detached is a no-op
        // (no proxy wait, no double cleanup). On repeat shutdowns `session`
        // is `None`, so nothing is emitted twice.
        if let Some(s) = session {
            hub.emit(&init::detached(&s.devnode, s.transport));
        }
        outcome
    }
}

impl Drop for DaemonSession {
    fn drop(&mut self) {
        // Explicit `shutdown` already ran on the orderly path; this only
        // matters for early returns and panic unwinds. Bounded and
        // idempotent: repeats wait for nothing. Reports inside.
        self.manager.shutdown_inner();
    }
}

/// Subscribe the opt-in click proxy to `ready_hub` (daemon only).
/// Gate-off (`enhanced_clicks=false`) still subscribes — the proxy then
/// short-circuits to passthrough. The gate is evaluated from the startup
/// config; there is no live reload, so a config change needs a restart.
/// Never blocks init: callbacks do bounded attach/detach only, and grab
/// failure degrades to passthrough with a log.
///
/// Production pump: `attach` builds the real path — BOTH uinput nodes
/// (`UI_DEV_CREATE` pointer FIRST, then extras), hidraw→eventN
/// auto-resolution (`$LTD_EVDEV_NODE` override wins, else sysfs walk, else
/// name scan), then hidraw open, then `set_pump_ready(true)` + grab.
/// `detach` tears down in reverse (release grab, latch reset, trailing
/// `SYN` on both nodes, source to idle, BOTH nodes destroyed). Single
/// active node: first `Ready` wins, siblings ignored until `Detached`
/// clears it — except a `Ready` for the active node whose session died on
/// its own (disconnect self-release): that tears down the stale wiring
/// and re-attaches fresh fds instead of early-returning into a dead
/// session. A single pump thread drains the proxy until session shutdown;
/// frame rate is device-bounded.
///
/// Two-node failure isolation: pointer-create failure → passthrough (no
/// touch emit path, so no grab — same as the old single node). Extras-
/// create failure is NON-fatal: the session continues pointer-only
/// (motion/buttons live, combos dropped) with a loud log, and teardown
/// always destroys both nodes so no sibling leaks.
///
/// Attach decision for a `Ready` event (see [`attach_decision`]):
/// - no active session → adopt (full attach);
/// - same node + live proxy → ignore (duplicate);
/// - same node + dead proxy → teardown + full re-attach;
/// - another node active → ignore (sibling).
///
/// Returns the owned [`DaemonSession`]: stop it with
/// [`DaemonSession::shutdown`] (or drop it) so the pump thread is
/// always stopped and joined, never forgotten.
pub fn subscribe_click_proxy(cfg: &Config, ready_hub: &DeviceReadyHub) -> DaemonSession {
    let uinput_node = std::env::var("LTD_UINPUT_NODE").unwrap_or("/dev/uinput".to_string());
    let proxy = std::sync::Arc::new(std::sync::Mutex::new(DaemonProxy::new(
        ProxySettings::from(cfg),
        RealGrab::new(""),
        DualSink::new(
            RealUinput::new(&uinput_node),
            RealUinput::new_extras(&uinput_node),
        ),
        AsyncHaptic::new(RealHaptic::new()),
        PumpSource::Idle,
    )));
    let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let (thread, control) = crate::pump::spawn_pump(
        "daemon".to_string(),
        std::sync::Arc::clone(&proxy),
        cfg.clone(),
        std::sync::Arc::clone(&stop),
    );
    let manager = std::sync::Arc::new(
        PumpManager::new(proxy, cfg.clone(), stop, || PumpSource::Idle)
            .with_thread(thread)
            .with_control(control),
    );
    // The pump thread is owned by `manager` from here on: `DaemonSession`
    // stops it (flag) and observes it (bounded join with panic reporting)
    // at shutdown, so the handle is never forgotten and a worker panic
    // never vanishes silently.
    let m = std::sync::Arc::clone(&manager);
    ready_hub.subscribe(move |event: &crate::init::DeviceReadyEvent| {
        use crate::init::ReadyKind;
        match event.kind {
            ReadyKind::Ready => m.attach(event),
            ReadyKind::Detached => m.detach(event),
        }
    });
    log::debug!(
        "click proxy subscribed (enhanced_clicks={})",
        cfg.enhanced_clicks
    );
    DaemonSession { manager }
}

// Unknown layouts (including Bluetooth) keep their existing source path.
fn touch_interface_eligible(iface: Option<u8>) -> bool {
    iface.is_none_or(|number| number == 1)
}

fn session_needs_attach(enhanced: bool, active: Option<&str>, devnode: &str, live: bool) -> bool {
    enhanced
        && matches!(
            attach_decision(active, devnode, live),
            AttachDecision::Adopt | AttachDecision::Reattach
        )
}

/// Attach decision for a `Ready` event against manager state.
///
/// `Reattach` is the same-name-replug path: the node is still `active`
/// but its session died on its own (disconnect self-release), so the
/// caller must tear down the stale wiring and attach fresh fds instead
/// of early-returning into a permanently dead session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AttachDecision {
    Adopt,
    IgnoreDuplicate,
    IgnoreSibling,
    Reattach,
}

pub(crate) fn attach_decision(
    active: Option<&str>,
    devnode: &str,
    proxy_attached: bool,
) -> AttachDecision {
    match active {
        None => AttachDecision::Adopt,
        Some(cur) if cur == devnode => {
            if proxy_attached {
                AttachDecision::IgnoreDuplicate
            } else {
                AttachDecision::Reattach
            }
        }
        Some(_) => AttachDecision::IgnoreSibling,
    }
}

/// Per-node probe budget for [`verify_attach_stream`] (short: hub
/// callbacks must stay bounded; 4 interfaces × 40 ms ≈ 160 ms worst).
const ATTACH_PROBE_MS_PER_NODE: u64 = 40;

/// Attach-time stream verify (see call site): poll the chosen node plus
/// live scan siblings nonblocking; loud-warn on silent-chosen-while-
/// sibling-streams, debug otherwise. Pure decision via
/// [`discovery::check_stream_health`]; this wrapper does the I/O + log.
fn verify_attach_stream(chosen: &str) {
    let siblings: Vec<String> = discovery::scan_trackpads()
        .unwrap_or_default()
        .into_iter()
        .map(|d| d.devnode)
        .collect();
    // Probe chosen first so a live chosen short-circuits to Healthy
    // without waiting on silent siblings; then each sibling.
    let mut ordered = vec![chosen.to_string()];
    for s in siblings {
        if s != chosen {
            ordered.push(s);
        }
    }
    let mut streaming = Vec::new();
    for node in &ordered {
        if discovery::poll_has_report(node, ATTACH_PROBE_MS_PER_NODE) {
            streaming.push(node.clone());
        }
    }
    match discovery::check_stream_health(chosen, &streaming) {
        discovery::StreamHealth::Healthy => {
            log::debug!("pump: attach verify: {chosen} streaming");
        }
        discovery::StreamHealth::SilentWhileSiblingStreams { streaming: live } => {
            log::warn!(
                "pump: chosen {chosen} silent while sibling {live} streams; \
                 hidraw numbers shift across enumeration — check MT-proven selection (prefer abs-MT node), \
                 consider LTD_EVDEV_NODE pin or re-attach to {live}",
            );
        }
        discovery::StreamHealth::IdleNoTraffic => {
            log::debug!("pump: attach verify: no touch traffic (idle, finger off)");
        }
    }
}

/// Initialize one discovered node and return its ready event.
/// USB: defer (node+bind) → retrying feature reports → verify → ready.
/// BT: no reports, straight to ready. Native input is suppressed only by a
/// live proxy's reversible evdev grab, never by hardware initialization.
/// Device setup runs independently of the enhanced-clicks gate.
pub fn init_node(
    dev: &DiscoveredDevice,
    polling_hz: u32,
    ready_hub: &DeviceReadyHub,
) -> Result<DeviceReadyEvent> {
    match dev.transport {
        Transport::Bluetooth => {
            let ev = init::init_bluetooth(&dev.devnode);
            ready_hub.emit(&ev);
            Ok(ev)
        }
        Transport::Usb => {
            // Defer-retry-verify: node may lag udev `add`, bind may lag node;
            // retrying writes + read-back verify carry the rest.
            let (node, bound) = discovery::wait_for_ready(&dev.devnode, 2000);
            if !node {
                log::warn!(
                    "node {} not present after 2 s wait; attempting init anyway",
                    dev.devnode
                );
            } else if !bound {
                log::debug!(
                    "node {} present, driver bind not yet visible; retrying writes will cover it",
                    dev.devnode
                );
            }
            let sender = HidrawSender {
                devnode: dev.devnode.clone(),
            };
            let verifier = init::HidrawVerifier {
                devnode: dev.devnode.clone(),
            };
            let iface = discovery::interface_number_for_node(&dev.devnode);
            let outcome = init::init_usb_verified(&sender, &verifier, polling_hz, iface)?;
            log::debug!(
                "USB init on {} accepted (iface {:?}, polling [99 F2 {:02X}])",
                dev.devnode,
                outcome.iface,
                outcome.plan.x,
            );
            let ev = init::usb_ready(&dev.devnode);
            ready_hub.emit(&ev);
            Ok(ev)
        }
    }
}

/// `run` daemon loop. Holds the socket lock, inits all current devices,
/// then parks until SIGINT/SIGTERM/SIGHUP. Shutdown runs the owned
/// [`DaemonSession`] teardown (observable detach, grab release, node
/// teardown, actuator drain, bounded pump join) before the socket lock
/// drops. A pump worker panic, join timeout, or incomplete cleanup fails
/// the exit so it surfaces instead of vanishing.
pub fn run_daemon(cfg: &Config) -> Result<()> {
    log::info!("starting {}", crate::build_identity::IDENTITY);
    let sock = lock::socket_path();
    let _lock = SocketLock::acquire(&sock)?;
    log::info!("lock acquired at {sock}");
    log::info!(
        "config: polling {} Hz, enhanced_clicks={}",
        cfg.device.polling_rate,
        cfg.enhanced_clicks
    );
    if cfg.device.polling_rate > EFFECTIVE_CAP_HZ as u32 {
        log::warn!(
            "requested {} Hz above device cap; clamped to ~110 Hz (X=0x23)",
            cfg.device.polling_rate
        );
    }

    // SIGINT/SIGTERM/SIGHUP all request the same owned teardown below.
    // The handlers only flip the process-lifetime static flag
    // (async-signal-safe); the loop owns the rest. The guard restores
    // previous dispositions when `run` returns. One daemon per process:
    // a second install would reset the flag and reinstall.
    let _signals = install_shutdown_handler()?;

    let mut monitor = match crate::hotplug::Monitor::open() {
        Ok(monitor) => {
            log::info!("hotplug: kernel device monitor active (30 s verification fallback)");
            Some(monitor)
        }
        Err(error) => {
            log::warn!("hotplug: monitor unavailable: {error}; using 30 s discovery fallback");
            None
        }
    };
    let ready_hub = hub();
    let session = subscribe_click_proxy(cfg, &ready_hub);
    // Query actual proxy health, independent of hardware polling state.
    // Siblings wait while an owner exists; a dead owner retries its own
    // node first, using the existing teardown/re-attach path.
    let needs_attach = |devnode: &str| session.needs_attach(devnode);
    let initial: Vec<DiscoveredDevice> = match discovery::scan_trackpads() {
        Ok(devs) if devs.is_empty() => {
            log::info!("no supported trackpads found; idling until signal");
            Vec::new()
        }
        Ok(devs) => {
            log::info!("found {} trackpad node(s)", devs.len());
            devs
        }
        Err(e) => {
            log::warn!("device scan failed (continuing idle): {e:#}");
            Vec::new()
        }
    };

    // Belt-and-braces resume detection: the system-sleep hook (`ltd resume`)
    // is the primary post-sleep re-init (immediate); this tick covers
    // add/change/remove that arrive with no sleep hook (re-verify polling
    // via GET_FEATURE, multitouch-first re-init on mismatch), plus
    // independent retry of missing/dead enhanced sessions. Each pass is
    // bounded by the existing hardware init/attach budgets.
    // Start empty so failed hardware init remains pending on later passes.
    let mut watch = ResumeWatch::new(&[]);
    watch.tick_with_scan(cfg, &ready_hub, initial, &needs_attach);
    let mut schedule = crate::hotplug::Schedule::new(std::time::Instant::now());
    if watch.has_pending(cfg, &needs_attach) {
        schedule.wake(std::time::Instant::now());
    }
    let mut overflow_reported = false;
    while shutdown_running() {
        std::thread::sleep(crate::hotplug::LOOP_INTERVAL);
        let now = std::time::Instant::now();
        if let Some(source) = &monitor {
            match source.drain() {
                Ok(true) => schedule.wake(now),
                Ok(false) => {}
                Err(error) if error.raw_os_error() == Some(libc::ENOBUFS) => {
                    if !overflow_reported {
                        log::warn!("hotplug: kernel event overflow; rescanning, monitor and 30 s fallback retained");
                        overflow_reported = true;
                    }
                    schedule.wake(now);
                }
                Err(error) => {
                    // A lost/overflowed stream is only a wake hint. The
                    // authoritative scan and periodic fallback remain safe.
                    log::warn!("hotplug: monitor failed: {error}; rescan now, then 30 s fallback");
                    schedule.wake(now);
                    monitor = None;
                }
            }
        }
        if let Some(work) = schedule.take_due(now, shutdown_running()) {
            match work {
                crate::hotplug::Work::Hotplug => {
                    watch.scan(cfg, &ready_hub, &needs_attach, false);
                }
                crate::hotplug::Work::Verify => {
                    watch.tick(cfg, &ready_hub, &needs_attach);
                }
            }
            if schedule.completed(
                std::time::Instant::now(),
                watch.has_pending(cfg, &needs_attach),
            ) {
                log::warn!("hotplug: fast readiness retries exhausted; pending setup/session will retry on the 30 s pass");
            }
        }
    }
    log::info!("shutdown requested; detaching session and stopping pump...");
    let outcome = session.shutdown(&ready_hub);
    log::info!(
        "shutdown complete (had_session={}, worker_panicked={}, pump_join_timed_out={}, cleanup_incomplete={})",
        outcome.had_session,
        outcome.worker_panicked,
        outcome.pump_join_timed_out,
        outcome.cleanup_incomplete
    );
    if outcome.worker_panicked {
        anyhow::bail!("pump worker thread panicked; see earlier errors");
    }
    if outcome.pump_join_timed_out {
        anyhow::bail!("pump thread did not stop within {PUMP_JOIN_TIMEOUT:?}");
    }
    if outcome.cleanup_incomplete {
        anyhow::bail!(
            "shutdown could not reclaim worker-owned proxy within {PROXY_ACQUIRE_TIMEOUT:?}; \
             process exit reclaims the grab"
        );
    }
    Ok(())
}

/// Classify a devnode against a scanned device list. Fails closed: an
/// unrecognized node is rejected, never defaulted to USB — a stray BT node
/// given the USB feature-report dance would just collect NAKs, and a
/// non-trackpad node must not receive init reports at all.
pub fn classify_node(devnode: &str, devs: &[DiscoveredDevice]) -> Result<Transport> {
    devs.iter()
        .find(|d| d.devnode == devnode)
        .map(|d| d.transport)
        .with_context(|| format!("device node '{devnode}' is not a recognized trackpad"))
}

/// `reconnect` mode decision: pure, unit-tested.
///
/// Activation contract (single, for daemon/hotplug/sleep paths):
/// - `-d`/`--disengage` always wins → host-click off, no init (explicit
///   manual disengage, touches hardware as requested).
/// - Daemon socket absent without `-f` → inactive: leave the hardware alone
///   (no init, no disengage write). This is the automatic background form
///   the udev reconnect template relies on: installing (never started) or
///   stopping the main service (socket absent) is a comprehensible off
///   switch (`disable` alone does not stop a running service).
/// - Daemon socket absent with `-f` → init anyway (explicit manual forcing).
/// - Daemon live (with or without `-f`) → init, unless `-d` was given.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReconnectMode {
    Init,
    Disengage,
    /// Automatic background call with no live daemon: touch nothing, exit 0.
    Inactive,
}

pub fn resolve_reconnect_mode(daemon_live: bool, force: bool, disengage: bool) -> ReconnectMode {
    if disengage {
        return ReconnectMode::Disengage;
    }
    if !daemon_live && !force {
        return ReconnectMode::Inactive;
    }
    ReconnectMode::Init
}

/// `resume` mode decision: pure, unit-tested. Same contract as reconnect:
/// the system-sleep hook and the resume unit run plain `resume` (automatic
/// background → inactive no-op unless the daemon is live); `-f`/`--force`
/// is explicit manual forcing while the daemon is stopped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResumeMode {
    Proceed,
    /// No live daemon and no `--force`: touch nothing, exit 0.
    Inactive,
}

pub fn resolve_resume_mode(daemon_live: bool, force: bool) -> ResumeMode {
    if !daemon_live && !force {
        return ResumeMode::Inactive;
    }
    ResumeMode::Proceed
}

/// Validate a polling request for the USB arm: range plus the shared device
/// floor ([`crate::init::check_usb_polling_usable`], the same gate doctor
/// applies to configs), with a cap warning on top. Kept out of the
/// disengage/BT arms on purpose — both discard the rate, so rejecting there
/// would break valid hotplug events. Pure so the USB-only gating is
/// unit-tested without hardware.
pub fn validate_polling_rate(polling_hz: u32) -> Result<()> {
    crate::init::check_usb_polling_usable(polling_hz)?;
    if polling_hz > EFFECTIVE_CAP_HZ as u32 {
        log::warn!("requested {polling_hz} Hz above device cap; clamped to ~110 Hz (X=0x23)");
    }
    Ok(())
}

/// `reconnect` entry. Idempotent; safe to re-run on every udev `add`.
/// Missing node / missing hardware → `Err` (caller logs + exits nonzero),
/// never a panic.
///
/// Order: defer (wait for the node) first, then scan + fail-closed
/// classify — classifying before the node appears would reject live
/// hardware on a stale scan.
pub fn run_reconnect(
    devnode: &str,
    force: bool,
    polling_override: Option<u32>,
    disengage: bool,
    cfg: &Config,
) -> Result<()> {
    if devnode.is_empty() {
        anyhow::bail!("reconnect requires a hidraw device path");
    }

    let sock = lock::socket_path();
    let daemon_live = lock::daemon_socket_present(&sock);
    let mode = resolve_reconnect_mode(daemon_live, force, disengage);
    if mode == ReconnectMode::Inactive {
        // Automatic background call (udev one-shot) with no live daemon:
        // installed-but-stopped (socket absent) is off. Touch nothing,
        // exit 0 — before any node wait/scan so the no-op is fast and
        // cannot fail on hardware.
        log::info!(
            "daemon socket absent ({sock}); inactive: leaving hardware alone (use -f to init, -d to disengage)"
        );
        return Ok(());
    }
    if disengage {
        log::info!("disengage requested; applying host-click off");
    } else if !daemon_live {
        log::info!("daemon socket absent but -f given; proceeding with init");
    }

    // Defer first: the node may lag udev `add` (probe/bind race).
    if !discovery::wait_for_node(devnode, 2000) {
        anyhow::bail!("device node '{devnode}' not present");
    }
    let devs = discovery::scan_trackpads().context("scanning hidraw for reconnect")?;
    let transport = classify_node(devnode, &devs)?;
    reconnect_classified_node(devnode, transport, mode, polling_override, cfg)
}

/// Hardware setup after discovery has validated the reconnect target.
/// A one-shot owns no proxy or live hub; it must leave native BT input alone
/// in every mode. Only the daemon can acquire a reversible evdev grab.
fn reconnect_classified_node(
    devnode: &str,
    transport: Transport,
    mode: ReconnectMode,
    polling_override: Option<u32>,
    cfg: &Config,
) -> Result<()> {
    if mode == ReconnectMode::Inactive {
        // Defensive: `run_reconnect` returns before classify in this mode.
        log::info!("{devnode}: inactive (no live daemon); leaving hardware alone");
        return Ok(());
    }
    if mode == ReconnectMode::Disengage {
        match transport {
            Transport::Bluetooth => {
                // No BT host-click report or driver binding changes.
                log::info!("BT {devnode}: disengage (native input retained)");
                return Ok(());
            }
            Transport::Usb => {
                let sender = HidrawSender {
                    devnode: devnode.to_string(),
                };
                init::disengage_usb_with_retry(&sender)?;
                return Ok(());
            }
        }
    }

    match transport {
        Transport::Bluetooth => {
            // No BT polling-rate equivalent (USB-first scope): -p/ config rate
            // is intentionally ignored here, never placed on the wire.
            if polling_override.is_some() {
                log::warn!(
                    "BT {devnode}: skipping polling rate (unsupported on Bluetooth; USB-first)"
                );
            }
            init::init_bluetooth(devnode);
            Ok(())
        }
        Transport::Usb => {
            // Rate validation lives here only: disengage + BT discard the
            // rate, so they must not reject it.
            let polling_hz = polling_override.unwrap_or(cfg.device.polling_rate);
            validate_polling_rate(polling_hz)?;
            // Node is present (deferred above); still check the driver bind —
            // retrying writes + read-back verify cover a lagging bind.
            let (_, bound) = discovery::wait_for_ready(devnode, 2000);
            if !bound {
                log::debug!(
                    "node {devnode} present, driver bind not yet visible; retrying writes will cover it"
                );
            }
            let sender = HidrawSender {
                devnode: devnode.to_string(),
            };
            let verifier = init::HidrawVerifier {
                devnode: devnode.to_string(),
            };
            let iface = discovery::interface_number_for_node(devnode);
            let outcome = init::init_usb_verified(&sender, &verifier, polling_hz, iface)?;
            log::info!(
                "USB init on {devnode} accepted (iface {:?}, polling [99 F2 {:02X}])",
                outcome.iface,
                outcome.plan.x,
            );
            Ok(())
        }
    }
}

/// `resume` entry (system-sleep hook + resume unit + manual re-init).
/// System-side only: re-verifies every discovered node and re-runs the USB
/// sequence from multitouch enable on mismatch. Unlike `reconnect`, the
/// plain form has no per-node target: it covers every discovered node.
///
/// Activation contract: plain `resume` is automatic background (hook/unit)
/// → inactive no-op without a live daemon (hardware left alone, exit 0).
/// `-f`/`--force` is explicit manual forcing while the daemon is stopped.
/// Per-node failures are collected in the report, never fatal to
/// siblings; only a failed scan is an `Err`. Polling validation runs before
/// the gate so bad rates always error, even when inactive.
pub fn run_resume(
    cfg: &Config,
    polling_override: Option<u32>,
    force: bool,
) -> Result<ResumeReport> {
    let polling_hz = polling_override.unwrap_or(cfg.device.polling_rate);
    validate_polling_rate(polling_hz)?;
    let sock = lock::socket_path();
    let daemon_live = lock::daemon_socket_present(&sock);
    if resolve_resume_mode(daemon_live, force) == ResumeMode::Inactive {
        log::info!(
            "resume: daemon socket absent ({sock}); inactive: leaving hardware alone (use -f to force re-init)"
        );
        return Ok(ResumeReport::default());
    }
    if daemon_live {
        log::debug!("resume: daemon socket present ({sock}); re-init covers live daemon too");
    } else {
        log::info!(
            "resume: daemon socket absent ({sock}) but -f given; re-initing hardware directly"
        );
    }
    // Journal trace proving the hook/one-shot ran (hook failures are
    // swallowed to exit 0, so this line is the post-sleep evidence).
    log::info!("resume: hook/one-shot pass starting (polling {polling_hz} Hz)");
    // The hook can fire before the node re-enumerates: rescan bounded
    // instead of recording a silent empty no-op. Anything slower is the
    // daemon tick's job (≤ RESUME_REVERIFY_INTERVAL_SECS).
    let devs = resume::scan_with_rescan(
        &|| discovery::scan_trackpads().unwrap_or_default(),
        resume::RESUME_RESCAN_TIMEOUT_MS,
        resume::RESUME_RESCAN_INTERVAL_MS,
    );
    if devs.is_empty() {
        log::warn!(
            "resume: no supported trackpads found after rescan; nothing to re-init; {}",
            resume::empty_scan_recovery_hint()
        );
    }
    Ok(resume::run_resume_scan(&devs, polling_hz))
}

/// One daemon tick action, for logs/tests.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TickAction {
    /// Node appeared since the last pass → full init (add path).
    Initialized(String),
    /// Known USB node drifted → multitouch-first re-init (change path).
    Reinitialized(String),
    /// Known USB node still armed; nothing sent.
    UpToDate(String),
    /// Known USB node unreadable several passes running; assuming armed,
    /// nothing sent (reprobe every [`resume::UNREADABLE_RETRY_EVERY_TICKS`]).
    AssumedArmed(String),
    /// Hardware is initialized but the enhanced session needs attachment.
    AttachmentRetried(String),
    /// Node vanished or was replaced → `Detached` emitted (remove path).
    Detached(String),
}

/// Daemon-side resume/add/change/remove watch. Owns the last-seen device
/// list plus per-node unreadable streaks; each [`ResumeWatch::tick`] diffs a
/// fresh scan against it:
///
/// - added/replaced node or previously failed init → full
///   [`init_node`] (same ordering as boot init);
/// - still-present known USB node → one `GET_FEATURE` re-verify, threaded
///   into [`resume::resume_usb_node`] (no double read); multitouch-first
///   re-init on mismatch, [`resume::needs_reinit`] as the decision;
/// - still-present known USB node with a consecutively-unreadable verify →
///   re-init once (assume transient), then assume armed until the periodic
///   reprobe ([`resume::unreadable_disposition`]) — firmware that NAKs reads
///   must not cause a re-init + `Ready` storm every pass;
/// - still-present known BT node → no hardware init or binding changes;
/// - vanished/replaced instance → `Detached` emit so clicks releases it;
/// - initialized hardware with a missing/dead enhanced session → bounded
///   attachment-only `Ready`, independent of USB verify or unchanged BT
///   hardware.
///
/// Healthy sessions and siblings do not receive retry events.
///
/// Removal `Detached`s emit before the scan loop so a same-window
/// renumber self-heals (stale session torn down, then the new node
/// adopted); per-node init emits happen after each node's init step —
/// clicks re-attaches without reordering init.
pub struct ResumeWatch {
    known: Vec<NodeState>,
}

struct NodeState {
    dev: DiscoveredDevice,
    unreadable_streak: u32,
    // A required setup failure invalidates historical success. Pending
    // setup must complete the full sequence before attachment is allowed.
    initialized: bool,
    identity: Option<NodeIdentity>,
}

/// devtmpfs instance identity, not just the reusable hidraw path/rdev.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct NodeIdentity {
    filesystem: u64,
    inode: u64,
    device: u64,
}

fn node_identity(devnode: &str) -> Option<NodeIdentity> {
    use std::os::unix::fs::MetadataExt;
    let meta = std::fs::metadata(devnode).ok()?;
    Some(NodeIdentity {
        filesystem: meta.dev(),
        inode: meta.ino(),
        device: meta.rdev(),
    })
}

// Small I/O seam: lifecycle regressions inject hardware outcomes without
// opening trackpads, grabbing evdev or creating virtual devices.
trait WatchHardware {
    fn identity(&self, devnode: &str) -> Option<NodeIdentity> {
        node_identity(devnode)
    }
    fn initialize(&self, dev: &DiscoveredDevice) -> Result<()>;
    fn verify(&self, devnode: &str) -> Result<bool>;
    fn resume(&self, devnode: &str, verify: &Result<bool>) -> Result<resume::ResumeAction>;
}

struct LiveWatchHardware<'a> {
    cfg: &'a Config,
    hub: &'a DeviceReadyHub,
}

impl WatchHardware for LiveWatchHardware<'_> {
    fn initialize(&self, dev: &DiscoveredDevice) -> Result<()> {
        init_node(dev, self.cfg.device.polling_rate, self.hub)?;
        Ok(())
    }
    fn verify(&self, devnode: &str) -> Result<bool> {
        resume::verify_node_polling(devnode, self.cfg.device.polling_rate)
    }
    fn resume(&self, devnode: &str, verify: &Result<bool>) -> Result<resume::ResumeAction> {
        resume::resume_usb_node(
            devnode,
            self.cfg.device.polling_rate,
            Some(self.hub),
            verify,
        )
    }
}

impl ResumeWatch {
    /// Seed with successfully initialized devices only. Production starts
    /// empty and processes its boot scan through the same retry path.
    pub fn new(initial: &[DiscoveredDevice]) -> Self {
        Self {
            known: initial
                .iter()
                .map(|dev| NodeState {
                    dev: dev.clone(),
                    unreadable_streak: 0,
                    initialized: true,
                    identity: node_identity(&dev.devnode),
                })
                .collect(),
        }
    }

    pub fn tick(
        &mut self,
        cfg: &Config,
        ready_hub: &DeviceReadyHub,
        needs_attach: &dyn Fn(&str) -> bool,
    ) -> Vec<TickAction> {
        self.scan(cfg, ready_hub, needs_attach, true)
    }

    fn has_pending(&self, cfg: &Config, needs_attach: &dyn Fn(&str) -> bool) -> bool {
        self.known.iter().any(|state| {
            !state.initialized || (cfg.enhanced_clicks && needs_attach(&state.dev.devnode))
        })
    }

    fn scan(
        &mut self,
        cfg: &Config,
        ready_hub: &DeviceReadyHub,
        needs_attach: &dyn Fn(&str) -> bool,
        verify_hardware: bool,
    ) -> Vec<TickAction> {
        let current = match discovery::scan_trackpads() {
            Ok(devs) => devs,
            Err(e) => {
                log::warn!("resume tick: scan failed, keeping last-seen list: {e:#}");
                return Vec::new();
            }
        };
        self.reconcile_with_hardware(
            cfg,
            ready_hub,
            current,
            needs_attach,
            &LiveWatchHardware {
                cfg,
                hub: ready_hub,
            },
            verify_hardware,
        )
    }

    /// One pass over an injected scan (production passes the live scan
    /// from [`ResumeWatch::tick`]; tests inject renumbers/replugs).
    fn tick_with_scan(
        &mut self,
        cfg: &Config,
        ready_hub: &DeviceReadyHub,
        current: Vec<DiscoveredDevice>,
        needs_attach: &dyn Fn(&str) -> bool,
    ) -> Vec<TickAction> {
        self.tick_with_hardware(
            cfg,
            ready_hub,
            current,
            needs_attach,
            &LiveWatchHardware {
                cfg,
                hub: ready_hub,
            },
        )
    }

    fn tick_with_hardware(
        &mut self,
        cfg: &Config,
        ready_hub: &DeviceReadyHub,
        current: Vec<DiscoveredDevice>,
        needs_attach: &dyn Fn(&str) -> bool,
        hardware: &dyn WatchHardware,
    ) -> Vec<TickAction> {
        self.reconcile_with_hardware(cfg, ready_hub, current, needs_attach, hardware, true)
    }

    fn reconcile_with_hardware(
        &mut self,
        cfg: &Config,
        ready_hub: &DeviceReadyHub,
        current: Vec<DiscoveredDevice>,
        needs_attach: &dyn Fn(&str) -> bool,
        hardware: &dyn WatchHardware,
        verify_hardware: bool,
    ) -> Vec<TickAction> {
        let mut actions = Vec::new();
        // Nodes recorded Detached mid-pass (present in the scan but gone by
        // the wait) are dropped from the stored snapshot below — trusting
        // the fresher wait over the stale scan. They re-enter as added nodes
        // if they come back.
        let mut next: Vec<NodeState> = current
            .into_iter()
            .map(|dev| {
                let identity = hardware.identity(&dev.devnode);
                let prior = self.known.iter().find(|k| {
                    // Missing metadata alone does not prove a replacement.
                    k.dev == dev
                        && !(k.identity.is_some() && identity.is_some() && k.identity != identity)
                });
                NodeState {
                    unreadable_streak: prior.map_or(0, |k| k.unreadable_streak),
                    initialized: prior.is_some_and(|k| k.initialized),
                    identity: identity.or_else(|| prior.and_then(|k| k.identity)),
                    dev,
                }
            })
            .collect();
        // Retire old instances BEFORE any Ready, including replacements
        // reusing the same node name entirely between two scans.
        for old in &self.known {
            let survives = next.iter().any(|n| {
                n.dev == old.dev
                    && !(old.identity.is_some()
                        && n.identity.is_some()
                        && old.identity != n.identity)
            });
            if !survives {
                ready_hub.emit(&init::detached(&old.dev.devnode, old.dev.transport));
                actions.push(TickAction::Detached(old.dev.devnode.clone()));
            }
        }
        let mut detached: Vec<String> = Vec::new();
        let mut ready_sent: Vec<String> = Vec::new();
        for state in &mut next {
            let dev = &state.dev;
            if !state.initialized {
                match hardware.initialize(dev) {
                    Ok(()) => {
                        state.initialized = true;
                        state.unreadable_streak = 0;
                        // Mode-switch init can itself replace the node;
                        // remember the instance accepted by the sequence.
                        state.identity = hardware.identity(&dev.devnode).or(state.identity);
                        ready_sent.push(dev.devnode.clone());
                        actions.push(TickAction::Initialized(dev.devnode.clone()));
                    }
                    Err(e) => log::error!("resume tick: init failed for {}: {e:#}", dev.devnode),
                }
                continue;
            }
            if dev.transport != Transport::Usb
                || (!verify_hardware && (!cfg.enhanced_clicks || !needs_attach(&dev.devnode)))
            {
                // Healthy known hardware readback stays on the 30 s pass.
                // A dead USB session still verifies before reacquiring a
                // grab: polling drift may require a full rearm first.
                continue;
            }
            let verify = hardware.verify(&dev.devnode);
            if !resume::needs_reinit(&verify) {
                state.unreadable_streak = 0;
                actions.push(TickAction::UpToDate(dev.devnode.clone()));
                continue;
            }
            if verify.is_err() {
                let prior = state.unreadable_streak;
                state.unreadable_streak = prior + 1;
                match resume::unreadable_disposition(prior) {
                    resume::UnreadableDisposition::Suppress => {
                        if prior == 1 {
                            log::warn!(
                                "resume tick: {} verify unreadable again; assuming armed, suppressing re-init (reprobe every {} ticks)",
                                dev.devnode,
                                resume::UNREADABLE_RETRY_EVERY_TICKS,
                            );
                        } else {
                            log::debug!(
                                "resume tick: {} still unreadable (streak {}); assuming armed",
                                dev.devnode,
                                prior + 1,
                            );
                        }
                        actions.push(TickAction::AssumedArmed(dev.devnode.clone()));
                        continue;
                    }
                    resume::UnreadableDisposition::ReinitNow => {
                        log::warn!(
                            "resume tick: {} verify unreadable; re-initing anyway",
                            dev.devnode
                        );
                    }
                }
            } else {
                state.unreadable_streak = 0;
            }
            match hardware.resume(&dev.devnode, &verify) {
                Ok(resume::ResumeAction::Reinitialized) => {
                    state.identity = hardware.identity(&dev.devnode).or(state.identity);
                    ready_sent.push(dev.devnode.clone());
                    actions.push(TickAction::Reinitialized(dev.devnode.clone()))
                }
                Ok(resume::ResumeAction::Detached) => {
                    log::info!(
                        "resume tick: {} detached mid-pass; dropping from snapshot",
                        dev.devnode
                    );
                    detached.push(dev.devnode.clone());
                    actions.push(TickAction::Detached(dev.devnode.clone()))
                }
                Ok(resume::ResumeAction::UpToDate) => {
                    actions.push(TickAction::UpToDate(dev.devnode.clone()))
                }
                Err(e) => {
                    // Polling may match later even if multitouch enable
                    // failed. Require full setup, not attachment-only
                    // recovery or another polling-only decision.
                    state.initialized = false;
                    log::error!("resume tick: re-init failed for {}: {e:#}", dev.devnode);
                }
            }
        }
        // Reconcile sessions separately: hardware success does not imply
        // an attached proxy session. At most one Ready per node
        // per scheduled pass, including init/re-init emits. Healthy owners and
        // siblings produce no additional hub traffic; gate-off never retries.
        next.retain(|s| !detached.contains(&s.dev.devnode));
        for state in &next {
            if cfg.enhanced_clicks
                && state.initialized
                && !ready_sent.contains(&state.dev.devnode)
                && needs_attach(&state.dev.devnode)
            {
                let event = match state.dev.transport {
                    Transport::Usb => init::usb_ready(&state.dev.devnode),
                    Transport::Bluetooth => init::init_bluetooth(&state.dev.devnode),
                };
                ready_hub.emit(&event);
                actions.push(TickAction::AttachmentRetried(state.dev.devnode.clone()));
            }
        }
        self.known = next;
        actions
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::init::Recorder;

    use crate::init::ReadyKind;
    use crate::proxy::{ClickProxy, GrabDevice, LogHaptic, ProxySettings, UinputSink};
    use crate::pump::{HidrawSource, ScriptReader, ScriptStep};
    use std::cell::Cell;
    use std::sync::{Arc, Mutex};

    #[derive(Default)]
    struct TestGrab {
        fail: AtomicBool,
        grabbed: AtomicBool,
    }
    impl GrabDevice for TestGrab {
        fn grab(&self) -> Result<()> {
            if self.fail.load(Ordering::SeqCst) {
                anyhow::bail!("transient grab failure");
            }
            self.grabbed.store(true, Ordering::SeqCst);
            Ok(())
        }
        fn ungrab(&self) {
            self.grabbed.store(false, Ordering::SeqCst);
        }
        fn is_grabbed(&self) -> bool {
            self.grabbed.load(Ordering::SeqCst)
        }
    }
    #[derive(Default)]
    struct TestSink {
        fail: AtomicBool,
        destroyed: AtomicBool,
    }
    impl UinputSink for TestSink {
        fn emit(&self, _: u16, _: u16, _: i32) -> Result<()> {
            anyhow::ensure!(!self.fail.load(Ordering::SeqCst), "fatal output failure");
            Ok(())
        }
        fn is_ready(&self) -> bool {
            !self.destroyed.load(Ordering::SeqCst)
        }
        fn destroy(&self) {
            self.destroyed.store(true, Ordering::SeqCst);
        }
    }
    type TestProxy = ClickProxy<TestGrab, TestSink, LogHaptic, HidrawSource<ScriptReader>>;
    struct TestSession {
        proxy: TestProxy,
        active: Option<String>,
        attempts: usize,
        events: Vec<ReadyKind>,
    }
    impl TestSession {
        fn subscribe(hub: &DeviceReadyHub, fail: bool) -> Arc<Mutex<Self>> {
            Self::subscribe_filtered(hub, fail, |_| true)
        }

        fn subscribe_filtered(
            hub: &DeviceReadyHub,
            fail: bool,
            eligible: impl Fn(&DeviceReadyEvent) -> bool + Send + Sync + 'static,
        ) -> Arc<Mutex<Self>> {
            let session = Arc::new(Mutex::new(Self {
                proxy: TestProxy::new(
                    ProxySettings { enhanced: true },
                    TestGrab::default(),
                    TestSink::default(),
                    LogHaptic,
                    HidrawSource::new("fixture", ScriptReader::default()),
                ),
                active: None,
                attempts: 0,
                events: Vec::new(),
            }));
            session
                .lock()
                .unwrap()
                .proxy
                .grab()
                .fail
                .store(fail, Ordering::SeqCst);
            let capture = Arc::clone(&session);
            hub.subscribe(move |event| {
                let mut s = capture.lock().unwrap();
                s.events.push(event.kind);
                if event.kind == ReadyKind::Detached {
                    if s.active.as_deref() == Some(&event.devnode) {
                        // `detach` runs the full teardown; no second pass needed.
                        s.proxy.detach(event);
                        s.active = None;
                    }
                    return;
                }
                if !eligible(event) {
                    return;
                }
                if !session_needs_attach(
                    true,
                    s.active.as_deref(),
                    &event.devnode,
                    s.proxy.is_attached(),
                ) {
                    return;
                }
                s.attempts += 1;
                // Disposable replacement source, mirroring the manager's
                // fresh-FD wiring after a failed grab or dead source.
                s.proxy
                    .replace_source(HidrawSource::new(&event.devnode, ScriptReader::default()));
                // The real manager recreates retired output nodes before
                // setting pump readiness and acquiring a fresh grab.
                s.proxy.uinput().destroyed.store(false, Ordering::SeqCst);
                s.proxy.set_pump_ready(true);
                s.proxy.attach(event);
                if s.proxy.is_attached() {
                    s.active = Some(event.devnode.clone());
                }
            });
            session
        }
        fn needs_attach(&self, node: &str) -> bool {
            session_needs_attach(true, self.active.as_deref(), node, self.proxy.is_attached())
        }
    }

    struct TestHardware<'a> {
        hub: &'a DeviceReadyHub,
        init_fails: Cell<bool>,
        resume_fails: Cell<bool>,
        verify: Cell<Option<bool>>,
        verifies: Cell<usize>,
        inits: Cell<usize>,
        reinits: Cell<usize>,
    }
    impl<'a> TestHardware<'a> {
        fn new(hub: &'a DeviceReadyHub) -> Self {
            Self {
                hub,
                init_fails: Cell::new(false),
                resume_fails: Cell::new(false),
                verify: Cell::new(Some(true)),
                verifies: Cell::new(0),
                inits: Cell::new(0),
                reinits: Cell::new(0),
            }
        }
        fn ready(&self, dev: &DiscoveredDevice) {
            self.hub.emit(&match dev.transport {
                Transport::Usb => init::usb_ready(&dev.devnode),
                Transport::Bluetooth => init::init_bluetooth(&dev.devnode),
            });
        }
    }
    impl WatchHardware for TestHardware<'_> {
        fn initialize(&self, dev: &DiscoveredDevice) -> Result<()> {
            self.inits.set(self.inits.get() + 1);
            if self.init_fails.get() {
                anyhow::bail!("transient init failure");
            }
            self.ready(dev);
            Ok(())
        }
        fn verify(&self, _: &str) -> Result<bool> {
            self.verifies.set(self.verifies.get() + 1);
            self.verify
                .get()
                .ok_or_else(|| anyhow::anyhow!("unreadable firmware"))
        }
        fn resume(&self, devnode: &str, _: &Result<bool>) -> Result<resume::ResumeAction> {
            self.reinits.set(self.reinits.get() + 1);
            if self.resume_fails.get() {
                anyhow::bail!("USB multitouch rearm failed");
            }
            self.hub.emit(&init::usb_ready(devnode));
            Ok(resume::ResumeAction::Reinitialized)
        }
    }

    fn enhanced_config() -> Config {
        Config {
            enhanced_clicks: true,
            ..Config::default()
        }
    }

    #[test]
    fn hotplug_after_periodic_scan_observes_short_connection_and_coalesces_duplicates() {
        use crate::hotplug::{Schedule, Work};
        use std::time::{Duration, Instant};
        let hub = hub();
        let session = TestSession::subscribe(&hub, false);
        let hardware = TestHardware::new(&hub);
        let cfg = enhanced_config();
        let mut watch = ResumeWatch::new(&[]);
        let start = Instant::now();
        let mut schedule = Schedule::new(start);
        let health = |node: &str| session.lock().unwrap().needs_attach(node);
        let dev = fixture_devs().remove(0);
        let mut passes = 0;
        let mut first_attach = None;
        let mut removal_seen = false;
        // Empty periodic scan just ran at 30 s; a connection at 30.1 s
        // must not wait for the next periodic pass at 60 s.
        for ms in (30_000..=32_000).step_by(100) {
            let now = start + Duration::from_millis(ms);
            if ms == 30_100 || ms == 30_800 || ms == 31_300 {
                for _ in 0..20 {
                    schedule.wake(now);
                } // interface burst
            }
            if let Some(work) = schedule.take_due(now, true) {
                passes += 1;
                let present = (30_100..30_800).contains(&ms) || ms >= 31_300;
                let scan = if present { vec![dev.clone()] } else { vec![] };
                let actions = watch.reconcile_with_hardware(
                    &cfg,
                    &hub,
                    scan,
                    &health,
                    &hardware,
                    work == Work::Verify,
                );
                if first_attach.is_none() && session.lock().unwrap().proxy.is_attached() {
                    first_attach = Some(ms);
                }
                removal_seen |= actions.contains(&TickAction::Detached(dev.devnode.clone()));
                schedule.completed(now, watch.has_pending(&cfg, &health));
            }
        }
        assert!(first_attach.unwrap() - 30_100 <= 1_000);
        assert!(
            removal_seen,
            "subsecond connection must be adopted and removed"
        );
        assert!(passes < 10, "duplicate notifications coalesce");
        let s = session.lock().unwrap();
        assert!(s.proxy.is_attached());
        assert_eq!(s.attempts, 2, "one grab per connection");
        assert_eq!(
            s.events,
            vec![ReadyKind::Ready, ReadyKind::Detached, ReadyKind::Ready]
        );
        assert_eq!(hardware.inits.get(), 2);
        assert_eq!(
            hardware.verifies.get(),
            0,
            "healthy duplicates need no readback"
        );
    }

    #[test]
    fn hotplug_retries_late_touch_init_and_evdev_without_adopting_early_silent_interface() {
        use crate::hotplug::{Schedule, Work};
        use std::time::{Duration, Instant};
        let hub = hub();
        let session = TestSession::subscribe_filtered(&hub, true, |ev| {
            touch_interface_eligible(Some(if ev.devnode.ends_with('0') { 0 } else { 1 }))
        });
        let hardware = TestHardware::new(&hub);
        let cfg = enhanced_config();
        let mut watch = ResumeWatch::new(&[]);
        let start = Instant::now();
        let mut schedule = Schedule::new(start);
        let touch = fixture_devs().remove(0);
        let early = DiscoveredDevice {
            devnode: "/dev/hidraw0".into(),
            ..touch.clone()
        };
        let health = |node: &str| {
            touch_interface_eligible(Some(if node.ends_with('0') { 0 } else { 1 }))
                && session.lock().unwrap().needs_attach(node)
        };
        let mut attached_at = None;
        for ms in (0..=3_000).step_by(100) {
            let now = start + Duration::from_millis(ms);
            if ms == 0 || ms == 400 {
                schedule.wake(now);
            }
            hardware.init_fails.set((400..600).contains(&ms));
            session
                .lock()
                .unwrap()
                .proxy
                .grab()
                .fail
                .store(ms < 1_000, Ordering::SeqCst);
            if let Some(work) = schedule.take_due(now, true) {
                let scan = if ms < 400 {
                    vec![early.clone()]
                } else {
                    vec![touch.clone(), early.clone()]
                };
                watch.reconcile_with_hardware(
                    &cfg,
                    &hub,
                    scan,
                    &health,
                    &hardware,
                    work == Work::Verify,
                );
                if session.lock().unwrap().proxy.is_attached() && attached_at.is_none() {
                    attached_at = Some(ms);
                }
                schedule.completed(now, watch.has_pending(&cfg, &health));
            }
            if ms < 400 {
                assert_eq!(session.lock().unwrap().attempts, 0);
            }
        }
        assert!(
            attached_at.unwrap() - 1_000 <= 1_000,
            "retry within 1 s once evdev is ready"
        );
        let s = session.lock().unwrap();
        assert_eq!(s.active.as_deref(), Some(touch.devnode.as_str()));
        assert_eq!(s.attempts, 2, "failed grab then successful attach");
        assert_eq!(
            hardware.inits.get(),
            3,
            "early node once, touch init fails then succeeds"
        );
        assert_eq!(hardware.reinits.get(), 0);
        assert_eq!(
            hardware.verifies.get(),
            1,
            "only the unhealthy touch session is verified"
        );
    }

    #[test]
    fn hotplug_dead_usb_verifies_before_attach_and_pending_rearm_cannot_be_bypassed() {
        let hub = hub();
        let session = TestSession::subscribe(&hub, false);
        let hardware = TestHardware::new(&hub);
        let cfg = enhanced_config();
        let mut watch = ResumeWatch::new(&[]);
        let dev = fixture_devs().remove(0);
        let health = |node: &str| session.lock().unwrap().needs_attach(node);
        watch.reconcile_with_hardware(&cfg, &hub, vec![dev.clone()], &health, &hardware, false);
        session.lock().unwrap().proxy.set_pump_ready(false);
        hardware.verify.set(Some(false));
        hardware.resume_fails.set(true);
        hardware.init_fails.set(true);
        watch.reconcile_with_hardware(&cfg, &hub, vec![dev.clone()], &health, &hardware, false);
        assert!(!watch.known[0].initialized);
        assert!(!session.lock().unwrap().proxy.grab().is_grabbed());
        hardware.verify.set(Some(true));
        watch.reconcile_with_hardware(&cfg, &hub, vec![dev.clone()], &health, &hardware, false);
        assert_eq!(session.lock().unwrap().attempts, 1);
        hardware.init_fails.set(false);
        watch.reconcile_with_hardware(&cfg, &hub, vec![dev], &health, &hardware, false);
        assert!(session.lock().unwrap().proxy.is_attached());
        assert_eq!(session.lock().unwrap().attempts, 2);
        assert_eq!(hardware.verifies.get(), 1);
        assert_eq!(hardware.reinits.get(), 1);
    }

    #[test]
    fn hotplug_same_name_replacement_retires_owner_before_fresh_init() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("hidraw");
        std::fs::write(&path, []).unwrap();
        let mut dev = fixture_devs().remove(0);
        dev.devnode = path.to_string_lossy().into_owned();
        let hub = hub();
        let session = TestSession::subscribe(&hub, false);
        let hardware = TestHardware::new(&hub);
        let cfg = enhanced_config();
        let mut watch = ResumeWatch::new(&[]);
        let health = |node: &str| session.lock().unwrap().needs_attach(node);
        watch.reconcile_with_hardware(&cfg, &hub, vec![dev.clone()], &health, &hardware, false);
        // Preserve the old inode so the fixture cannot reuse its identity.
        std::fs::rename(&path, dir.path().join("retired")).unwrap();
        std::fs::write(&path, []).unwrap();
        let actions =
            watch.reconcile_with_hardware(&cfg, &hub, vec![dev.clone()], &health, &hardware, false);
        assert_eq!(
            actions,
            vec![
                TickAction::Detached(dev.devnode.clone()),
                TickAction::Initialized(dev.devnode)
            ]
        );
        let s = session.lock().unwrap();
        assert!(s.proxy.is_attached());
        assert_eq!(s.attempts, 2);
        assert_eq!(
            s.events,
            vec![ReadyKind::Ready, ReadyKind::Detached, ReadyKind::Ready]
        );
        assert_eq!(hardware.inits.get(), 2);
        assert_eq!(hardware.verifies.get(), 0);
    }

    #[test]
    fn hotplug_wake_attaches_session_without_waiting_for_verify() {
        // Finding #9: a plug event must reconcile to an attached session
        // on the fast hotplug pass, not at the next 30 s verification.
        // Both transports: USB (feature-report init) and BT (direct Ready).
        use crate::hotplug::{Schedule, Work};
        use std::time::{Duration, Instant};
        for dev in fixture_devs() {
            let hub = hub();
            let session = TestSession::subscribe(&hub, false);
            let hardware = TestHardware::new(&hub);
            let cfg = enhanced_config();
            let mut watch = ResumeWatch::new(&[]);
            let start = Instant::now();
            let mut schedule = Schedule::new(start);
            let health = |node: &str| session.lock().unwrap().needs_attach(node);
            // No event: nothing due well before the 30 s verification.
            assert_eq!(
                schedule.take_due(start + Duration::from_secs(5), true),
                None
            );
            // Plug event at 5 s; the coalesced fast pass fires, never a Verify.
            schedule.wake(start + Duration::from_secs(5));
            let due = start + Duration::from_secs(5) + Duration::from_millis(100);
            assert_eq!(schedule.take_due(due, true), Some(Work::Hotplug));
            let actions = watch.reconcile_with_hardware(
                &cfg,
                &hub,
                vec![dev.clone()],
                &health,
                &hardware,
                false,
            );
            assert_eq!(actions, vec![TickAction::Initialized(dev.devnode.clone())]);
            schedule.completed(due, watch.has_pending(&cfg, &health));
            assert!(session.lock().unwrap().proxy.is_attached());
            assert_eq!(session.lock().unwrap().attempts, 1);
            assert_eq!(hardware.inits.get(), 1);
            assert_eq!(hardware.verifies.get(), 0);
        }
    }

    #[test]
    fn hotplug_gate_off_discovers_new_hardware_promptly_without_session_work() {
        // Finding #9: gate-off must still init new hardware on the fast
        // hotplug pass (no 30 s wait); it just never builds a proxy session.
        let hub = hub();
        let hardware = TestHardware::new(&hub);
        let cfg = Config {
            enhanced_clicks: false,
            ..Config::default()
        };
        let mut watch = ResumeWatch::new(&[]);
        let devs = fixture_devs();
        // Hotplug fast pass: no hardware verification, like production
        // `watch.scan(..., verify_hardware=false)`.
        let actions = watch.reconcile_with_hardware(
            &cfg,
            &hub,
            devs.clone(),
            &|_| panic!("gate off must not query session health"),
            &hardware,
            false,
        );
        assert_eq!(
            actions,
            vec![
                TickAction::Initialized(devs[0].devnode.clone()),
                TickAction::Initialized(devs[1].devnode.clone()),
            ]
        );
        assert_eq!(hardware.inits.get(), 2);
        assert_eq!(hardware.verifies.get(), 0);
        assert_eq!(hardware.reinits.get(), 0);
        // Steady state on the fast pass: no readbacks, no attachment traffic.
        let actions = watch.reconcile_with_hardware(
            &cfg,
            &hub,
            devs,
            &|_| panic!("gate off must not query session health"),
            &hardware,
            false,
        );
        assert!(actions.is_empty());
        assert_eq!(hardware.verifies.get(), 0);
    }

    #[test]
    fn hotplug_gate_off_skips_session_work_and_known_hardware_reads() {
        let hub = hub();
        let hardware = TestHardware::new(&hub);
        let cfg = Config {
            enhanced_clicks: false,
            ..Config::default()
        };
        let devs = fixture_devs();
        let mut watch = ResumeWatch::new(&devs);
        let actions = watch.reconcile_with_hardware(
            &cfg,
            &hub,
            devs,
            &|_| panic!("gate off"),
            &hardware,
            false,
        );
        assert!(actions.is_empty());
        assert_eq!(hardware.inits.get(), 0);
        assert_eq!(hardware.verifies.get(), 0);
    }

    #[test]
    fn failed_attachment_recovers_with_unchanged_usb_or_bt_hardware() {
        for dev in fixture_devs() {
            let hub = hub();
            let session = TestSession::subscribe(&hub, true);
            let hardware = TestHardware::new(&hub);
            let cfg = enhanced_config();
            let mut watch = ResumeWatch::new(&[]);
            let health = |node: &str| session.lock().unwrap().needs_attach(node);
            // Boot init succeeds, but the physical grab fails. No second
            // attempt in the same pass; the scan name never changes.
            watch.tick_with_hardware(&cfg, &hub, vec![dev.clone()], &health, &hardware);
            assert_eq!(session.lock().unwrap().attempts, 1);
            assert!(session.lock().unwrap().proxy.is_passthrough());
            session
                .lock()
                .unwrap()
                .proxy
                .grab()
                .fail
                .store(false, Ordering::SeqCst);
            let actions =
                watch.tick_with_hardware(&cfg, &hub, vec![dev.clone()], &health, &hardware);
            assert!(actions.contains(&TickAction::AttachmentRetried(dev.devnode.clone())));
            assert!(session.lock().unwrap().proxy.is_attached());
            assert_eq!(session.lock().unwrap().attempts, 2);
            // Healthy sessions stay quiet for subsequent passes.
            for _ in 0..5 {
                watch.tick_with_hardware(&cfg, &hub, vec![dev.clone()], &health, &hardware);
            }
            assert_eq!(session.lock().unwrap().attempts, 2);
            assert_eq!(session.lock().unwrap().events.len(), 2);
            assert_eq!(hardware.inits.get(), 1);
            assert_eq!(hardware.reinits.get(), 0);
        }
    }

    #[test]
    fn failed_initial_hardware_init_remains_pending_until_success() {
        let hub = hub();
        let session = TestSession::subscribe(&hub, false);
        let hardware = TestHardware::new(&hub);
        hardware.init_fails.set(true);
        let cfg = enhanced_config();
        let mut watch = ResumeWatch::new(&[]);
        let dev = fixture_devs().remove(0);
        let health = |node: &str| session.lock().unwrap().needs_attach(node);
        for _ in 0..3 {
            watch.tick_with_hardware(&cfg, &hub, vec![dev.clone()], &health, &hardware);
        }
        assert_eq!(hardware.inits.get(), 3, "one init attempt per pass");
        assert!(session.lock().unwrap().events.is_empty());
        hardware.init_fails.set(false);
        watch.tick_with_hardware(&cfg, &hub, vec![dev], &health, &hardware);
        assert!(session.lock().unwrap().proxy.is_attached());
        assert_eq!(hardware.inits.get(), 4);
    }

    #[test]
    fn unchanged_scan_recovers_a_self_released_source_session() {
        for dev in fixture_devs() {
            let hub = hub();
            let session = TestSession::subscribe(&hub, false);
            let hardware = TestHardware::new(&hub);
            let cfg = enhanced_config();
            let mut watch = ResumeWatch::new(&[]);
            let health = |node: &str| session.lock().unwrap().needs_attach(node);
            watch.tick_with_hardware(&cfg, &hub, vec![dev.clone()], &health, &hardware);
            {
                let mut s = session.lock().unwrap();
                s.proxy.replace_source(HidrawSource::new(
                    &dev.devnode,
                    ScriptReader::new(vec![ScriptStep::IoError(libc::ENODEV)]),
                ));
                s.proxy.handle_frame(&cfg).unwrap();
                assert!(!s.proxy.is_attached());
                assert!(!s.proxy.grab().is_grabbed());
                assert_eq!(s.active.as_deref(), Some(dev.devnode.as_str()));
            }
            watch.tick_with_hardware(&cfg, &hub, vec![dev], &health, &hardware);
            assert!(session.lock().unwrap().proxy.is_attached());
            assert_eq!(session.lock().unwrap().attempts, 2);
            assert_eq!(hardware.inits.get(), 1);
            assert_eq!(hardware.reinits.get(), 0);
        }
    }

    #[test]
    fn unchanged_hardware_recovers_after_fatal_output_cleanup() {
        for dev in fixture_devs() {
            let hub = hub();
            let session = TestSession::subscribe(&hub, false);
            let hardware = TestHardware::new(&hub);
            let cfg = enhanced_config();
            let mut watch = ResumeWatch::new(&[]);
            let health = |node: &str| session.lock().unwrap().needs_attach(node);
            watch.tick_with_hardware(&cfg, &hub, vec![dev.clone()], &health, &hardware);
            let (report_id, prefix_len) = match dev.transport {
                Transport::Usb => (0x02, 12),
                Transport::Bluetooth => (0x31, 4),
            };
            let mut report = vec![0; prefix_len];
            report[0] = report_id;
            {
                let mut s = session.lock().unwrap();
                s.proxy.uinput().fail.store(true, Ordering::SeqCst);
                // An empty decoded report still emits the touch/tool/SYN
                // frame, exercising the fallible sink and fatal cleanup.
                s.proxy.replace_source(HidrawSource::new(
                    &dev.devnode,
                    ScriptReader::new(vec![ScriptStep::Bytes(report.clone())]),
                ));
                assert!(s.proxy.handle_frame(&cfg).is_err());
                assert!(!s.proxy.is_attached());
                assert!(!s.proxy.grab().is_grabbed());
                assert!(s.proxy.uinput().destroyed.load(Ordering::SeqCst));
                assert_eq!(s.active.as_deref(), Some(dev.devnode.as_str()));
                s.proxy.uinput().fail.store(false, Ordering::SeqCst);
            }
            let actions =
                watch.tick_with_hardware(&cfg, &hub, vec![dev.clone()], &health, &hardware);
            assert!(actions.contains(&TickAction::AttachmentRetried(dev.devnode.clone())));
            {
                let mut s = session.lock().unwrap();
                assert!(s.proxy.is_attached());
                assert!(s.proxy.grab().is_grabbed());
                assert!(!s.proxy.uinput().destroyed.load(Ordering::SeqCst));
                assert_eq!(s.attempts, 2);
                s.proxy.replace_source(HidrawSource::new(
                    &dev.devnode,
                    ScriptReader::new(vec![ScriptStep::Bytes(report)]),
                ));
                s.proxy.handle_frame(&cfg).unwrap();
                assert!(s.proxy.is_attached());
            }
            // Recovery does not re-init hardware or repeat attachment
            // once the recreated output session is healthy.
            watch.tick_with_hardware(&cfg, &hub, vec![dev], &health, &hardware);
            assert_eq!(hardware.inits.get(), 1);
            assert_eq!(hardware.reinits.get(), 0);
            assert_eq!(session.lock().unwrap().attempts, 2);
        }
    }

    // Lose an initialized session, require USB rearm, then fail that
    // required setup. Neither failure path may reacquire the grab.
    fn failed_usb_rearm_requires_full_initialization(output_loss: bool) {
        let hub = hub();
        let session = TestSession::subscribe(&hub, false);
        let hardware = TestHardware::new(&hub);
        let cfg = enhanced_config();
        let mut watch = ResumeWatch::new(&[]);
        let dev = fixture_devs().remove(0);
        assert_eq!(dev.transport, Transport::Usb);
        let health = |node: &str| session.lock().unwrap().needs_attach(node);
        watch.tick_with_hardware(&cfg, &hub, vec![dev.clone()], &health, &hardware);
        {
            let mut s = session.lock().unwrap();
            if output_loss {
                s.proxy.uinput().fail.store(true, Ordering::SeqCst);
                let mut report = vec![0; 12];
                report[0] = 0x02;
                s.proxy.replace_source(HidrawSource::new(
                    &dev.devnode,
                    ScriptReader::new(vec![ScriptStep::Bytes(report)]),
                ));
                assert!(s.proxy.handle_frame(&cfg).is_err());
                assert!(s.proxy.uinput().destroyed.load(Ordering::SeqCst));
                s.proxy.uinput().fail.store(false, Ordering::SeqCst);
            } else {
                s.proxy.replace_source(HidrawSource::new(
                    &dev.devnode,
                    ScriptReader::new(vec![ScriptStep::IoError(libc::ENODEV)]),
                ));
                s.proxy.handle_frame(&cfg).unwrap();
            }
            assert!(!s.proxy.is_attached());
            assert!(!s.proxy.grab().is_grabbed());
        }
        hardware.verify.set(Some(false));
        hardware.resume_fails.set(true);
        hardware.init_fails.set(true);
        let actions = watch.tick_with_hardware(&cfg, &hub, vec![dev.clone()], &health, &hardware);
        {
            let s = session.lock().unwrap();
            assert!(!s.proxy.is_attached(),
                "failed USB rearm reattached: actions={actions:?}, events={:?}, attempts={}, grabbed={}",
                s.events, s.attempts, s.proxy.grab().is_grabbed());
            assert!(!s.proxy.grab().is_grabbed());
            assert_eq!(s.attempts, 1);
            assert_eq!(s.events, vec![ReadyKind::Ready]);
        }
        assert!(!watch.known[0].initialized);
        assert_eq!(hardware.reinits.get(), 1);
        assert_eq!(hardware.verifies.get(), 1);

        // Matching polling alone cannot prove the multitouch-enable step
        // succeeded. Persistent setup failures must retry the full sequence
        // once per pass, without Ready events, grabs or verification bypass.
        hardware.verify.set(Some(true));
        for pass in 1..=3 {
            assert!(watch
                .tick_with_hardware(&cfg, &hub, vec![dev.clone()], &health, &hardware)
                .is_empty());
            assert_eq!(hardware.inits.get(), 1 + pass);
            assert_eq!(hardware.reinits.get(), 1);
            assert_eq!(hardware.verifies.get(), 1);
            let mut s = session.lock().unwrap();
            let mut short_report = vec![0; 8];
            short_report[0] = 0x02;
            s.proxy.replace_source(HidrawSource::new(
                &dev.devnode,
                ScriptReader::new(vec![ScriptStep::Bytes(short_report)]),
            ));
            assert!(s.proxy.handle_frame(&cfg).unwrap().is_none());
            assert!(!s.proxy.is_attached());
            assert!(!s.proxy.grab().is_grabbed());
            assert!(s.proxy.is_passthrough());
            assert_eq!(s.attempts, 1);
            assert_eq!(s.events, vec![ReadyKind::Ready]);
        }
        hardware.init_fails.set(false);
        let actions = watch.tick_with_hardware(&cfg, &hub, vec![dev.clone()], &health, &hardware);
        assert_eq!(actions, vec![TickAction::Initialized(dev.devnode.clone())]);
        assert!(watch.known[0].initialized);
        {
            let s = session.lock().unwrap();
            assert!(s.proxy.is_attached());
            assert!(s.proxy.grab().is_grabbed());
            assert_eq!(s.attempts, 2);
            assert_eq!(s.events, vec![ReadyKind::Ready, ReadyKind::Ready]);
        }
        // Successful full setup restores ordinary quiet polling/health
        // reconciliation; historical failure cannot trigger extra init.
        let actions = watch.tick_with_hardware(&cfg, &hub, vec![dev.clone()], &health, &hardware);
        assert_eq!(actions, vec![TickAction::UpToDate(dev.devnode)]);
        assert_eq!(hardware.inits.get(), 5);
        assert_eq!(hardware.reinits.get(), 1);
        assert_eq!(hardware.verifies.get(), 2);
        assert_eq!(session.lock().unwrap().attempts, 2);
    }

    #[test]
    fn failed_usb_rearm_after_source_loss_blocks_attachment_until_full_setup() {
        failed_usb_rearm_requires_full_initialization(false);
    }

    #[test]
    fn failed_usb_rearm_after_fatal_output_blocks_attachment_until_full_setup() {
        failed_usb_rearm_requires_full_initialization(true);
    }

    #[test]
    fn same_name_replacement_detaches_before_init_even_if_proxy_still_looks_live() {
        let fixture = tempfile::tempdir().unwrap();
        let path = fixture.path().join("hidraw");
        std::fs::write(&path, []).unwrap();
        let mut dev = fixture_devs().remove(0);
        dev.devnode = path.to_string_lossy().into_owned();
        let hub = hub();
        let session = TestSession::subscribe(&hub, false);
        let hardware = TestHardware::new(&hub);
        let cfg = enhanced_config();
        let mut watch = ResumeWatch::new(&[]);
        let health = |node: &str| session.lock().unwrap().needs_attach(node);
        watch.tick_with_hardware(&cfg, &hub, vec![dev.clone()], &health, &hardware);
        let first = node_identity(&dev.devnode).unwrap();
        let steady = watch.tick_with_hardware(&cfg, &hub, vec![dev.clone()], &health, &hardware);
        assert_eq!(steady, vec![TickAction::UpToDate(dev.devnode.clone())]);
        assert_eq!(session.lock().unwrap().attempts, 1);
        assert_eq!(hardware.inits.get(), 1);
        // Keep the old inode alive under a different name so the fixture
        // cannot accidentally reuse it; the scan never sees an absence.
        std::fs::rename(&path, fixture.path().join("retired")).unwrap();
        std::fs::write(&path, []).unwrap();
        assert_ne!(Some(first), node_identity(&dev.devnode));
        let actions = watch.tick_with_hardware(&cfg, &hub, vec![dev.clone()], &health, &hardware);
        assert_eq!(
            actions,
            vec![
                TickAction::Detached(dev.devnode.clone()),
                TickAction::Initialized(dev.devnode)
            ]
        );
        let s = session.lock().unwrap();
        assert_eq!(
            s.events,
            vec![ReadyKind::Ready, ReadyKind::Detached, ReadyKind::Ready]
        );
        assert!(s.proxy.is_attached());
        assert_eq!(hardware.inits.get(), 2);
    }

    #[test]
    fn persistent_attachment_failures_are_limited_to_one_attempt_per_pass() {
        let hub = hub();
        let session = TestSession::subscribe(&hub, true);
        let hardware = TestHardware::new(&hub);
        // Exercise unreadable cooldown too: suppressed hardware init must
        // not suppress independent session recovery.
        hardware.verify.set(None);
        let cfg = enhanced_config();
        let mut watch = ResumeWatch::new(&[]);
        let dev = fixture_devs().remove(0);
        let health = |node: &str| session.lock().unwrap().needs_attach(node);
        for pass in 1..=6 {
            watch.tick_with_hardware(&cfg, &hub, vec![dev.clone()], &health, &hardware);
            assert_eq!(session.lock().unwrap().attempts, pass);
        }
        assert_eq!(hardware.inits.get(), 1);
        assert_eq!(hardware.reinits.get(), 1);
        assert!(session.lock().unwrap().proxy.is_passthrough());
    }

    #[test]
    fn gate_off_and_owned_siblings_do_not_receive_attachment_retries() {
        let hub = hub();
        let hardware = TestHardware::new(&hub);
        let devs = fixture_devs();
        let mut watch = ResumeWatch::new(&devs);
        let events = Arc::new(Mutex::new(Vec::new()));
        let capture = Arc::clone(&events);
        hub.subscribe(move |ev| capture.lock().unwrap().push(ev.clone()));
        let health_queries = Cell::new(0);
        // Gate-off must not request session work, or emit Ready.
        let disabled = Config {
            enhanced_clicks: false,
            ..Config::default()
        };
        watch.tick_with_hardware(
            &disabled,
            &hub,
            devs.clone(),
            &|_| {
                health_queries.set(health_queries.get() + 1);
                true
            },
            &hardware,
        );
        assert_eq!(health_queries.get(), 0);
        assert!(events.lock().unwrap().is_empty());
        assert_eq!(hardware.inits.get(), 0);
        assert_eq!(hardware.reinits.get(), 0);
        for live in [false, true] {
            let needs_attach =
                |node: &str| session_needs_attach(true, Some(&devs[0].devnode), node, live);
            assert!(!needs_attach(&devs[1].devnode));
        }
        // Healthy owner blocks duplicates for itself and all siblings.
        watch.tick_with_hardware(
            &enhanced_config(),
            &hub,
            devs.clone(),
            &|node| session_needs_attach(true, Some(&devs[0].devnode), node, true),
            &hardware,
        );
        assert!(events.lock().unwrap().is_empty());
    }

    #[test]
    fn reconnect_rejects_empty_node() {
        let cfg = Config::default();
        assert!(run_reconnect("", false, None, false, &cfg).is_err());
    }

    #[test]
    fn resume_rejects_bad_polling_without_touching_hardware() {
        let cfg = Config::default();
        // Validation runs before the activation gate, so bad rates error
        // even when the call would otherwise be an inactive no-op.
        assert!(run_resume(&cfg, Some(0), false).is_err());
        assert!(run_resume(&cfg, Some(70_000), true).is_err());
    }

    #[test]
    fn resume_tick_emits_detached_for_vanished_node() {
        // Watch thinks an impossible node exists; the real scan can never
        // contain it → vanish path → Detached on the hub, never Ready.
        let hub = DeviceReadyHub::new();
        let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let capture = std::sync::Arc::clone(&seen);
        hub.subscribe(move |ev| capture.lock().expect("seen lock").push(ev.clone()));
        let mut watch = ResumeWatch::new(&[DiscoveredDevice {
            devnode: "/dev/hidraw-nope".into(),
            vendor_id: 0x05ac,
            product_id: 0x0324,
            transport: Transport::Usb,
        }]);
        let cfg = Config::default();
        let actions = watch.tick(&cfg, &hub, &|_| false);
        assert_eq!(
            actions,
            vec![TickAction::Detached("/dev/hidraw-nope".into())]
        );
        let seen = seen.lock().expect("seen lock");
        assert_eq!(seen.len(), 1);
        assert_eq!(seen[0].kind, crate::init::ReadyKind::Detached);
        // Second tick: steady state, no repeat traffic.
        let actions = watch.tick(&cfg, &hub, &|_| false);
        assert!(actions.is_empty());
        assert_eq!(seen.len(), 1);
    }

    #[test]
    fn renumber_inside_one_tick_heals_on_new_node() {
        // Same-window renumber hidraw8→hidraw12: removals must emit
        // BEFORE the scan loop adopts the addition, so the new node's
        // Ready is live and the stale Detached fires exactly once.
        let hub = DeviceReadyHub::new();
        let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let capture = std::sync::Arc::clone(&seen);
        hub.subscribe(move |ev| capture.lock().expect("seen lock").push(ev.clone()));
        let mut watch = ResumeWatch::new(&[DiscoveredDevice {
            devnode: "/dev/hidraw8".into(),
            vendor_id: 0x05ac,
            product_id: 0x0324,
            transport: Transport::Usb,
        }]);
        let cfg = Config::default();
        let current = vec![DiscoveredDevice {
            devnode: "/dev/hidraw12".into(),
            vendor_id: 0x004c,
            product_id: 0x0324,
            transport: Transport::Bluetooth,
        }];
        let actions = watch.tick_with_scan(&cfg, &hub, current, &|_| false);
        // Removal first, then adoption — the order that heals.
        assert_eq!(
            actions,
            vec![
                TickAction::Detached("/dev/hidraw8".into()),
                TickAction::Initialized("/dev/hidraw12".into()),
            ]
        );
        let seen = seen.lock().expect("seen lock");
        assert_eq!(seen.len(), 2);
        assert_eq!(seen[0].devnode, "/dev/hidraw8");
        assert_eq!(seen[0].kind, crate::init::ReadyKind::Detached);
        assert_eq!(seen[1].devnode, "/dev/hidraw12");
        assert_eq!(seen[1].kind, crate::init::ReadyKind::Ready);
        // Snapshot rebuilt on the new node: next pass is steady state.
        let actions = watch.tick_with_scan(
            &cfg,
            &hub,
            vec![DiscoveredDevice {
                devnode: "/dev/hidraw12".into(),
                vendor_id: 0x004c,
                product_id: 0x0324,
                transport: Transport::Bluetooth,
            }],
            &|_| false,
        );
        assert!(actions.is_empty());
        assert_eq!(seen.len(), 2, "stale Detached must fire exactly once");
    }

    #[test]
    fn attach_decision_matrix() {
        use AttachDecision::*;
        // Fresh: adopt.
        assert_eq!(attach_decision(None, "/dev/hidraw8", false), Adopt);
        assert_eq!(attach_decision(None, "/dev/hidraw8", true), Adopt);
        // Live duplicate: ignore, never rebind a healthy session.
        assert_eq!(
            attach_decision(Some("/dev/hidraw8"), "/dev/hidraw8", true),
            IgnoreDuplicate
        );
        // Same name, dead session (disconnect self-release): teardown +
        // re-attach. Early-return here would brick the node forever: no
        // further Ready is owed for an already-known devnode.
        assert_eq!(
            attach_decision(Some("/dev/hidraw8"), "/dev/hidraw8", false),
            Reattach
        );
        // Sibling while another owns the pump: ignore either way.
        assert_eq!(
            attach_decision(Some("/dev/hidraw8"), "/dev/hidraw10", false),
            IgnoreSibling
        );
        assert_eq!(
            attach_decision(Some("/dev/hidraw8"), "/dev/hidraw10", true),
            IgnoreSibling
        );
    }

    #[test]
    fn reconnect_rejects_bad_polling() {
        // USB-arm validation is pure: no hardware, no 2 s node wait.
        assert!(validate_polling_rate(0).is_err());
        assert!(validate_polling_rate(70_000).is_err());
        assert!(validate_polling_rate(110).is_ok());
        // Above cap warns but still validates (clamped downstream).
        assert!(validate_polling_rate(500).is_ok());
    }

    #[test]
    fn reconnect_missing_node_errors_without_panic() {
        let cfg = Config::default();
        // No hardware in test env; unrecognized node fails closed, no panic.
        let r = run_reconnect("/nonexistent-hidraw-xyz", true, Some(110), false, &cfg);
        assert!(r.is_err());
    }

    #[test]
    fn reconnect_rejects_unrecognized_live_node() {
        let cfg = Config::default();
        // /dev/null exists but is not a trackpad: fail closed, never init-dance it.
        let r = run_reconnect("/dev/null", true, Some(110), false, &cfg);
        assert!(r.is_err());
    }

    #[test]
    fn reconnect_rejects_below_floor() {
        assert!(validate_polling_rate(1).is_err());
        assert!(validate_polling_rate(15).is_err());
        assert!(validate_polling_rate(16).is_ok());
    }

    #[test]
    fn polling_validation_agrees_with_init_gate() {
        // Startup and init share one floor check (finding #16): every
        // boundary must agree, including the doctor-visible config gate.
        for hz in [0u32, 1, 15] {
            assert!(validate_polling_rate(hz).is_err());
            assert!(crate::init::check_usb_polling_usable(hz).is_err());
            assert!(crate::init::resolve_polling(hz).is_err());
        }
        for hz in [16u32, 60, 110, 500] {
            assert!(validate_polling_rate(hz).is_ok());
            assert!(crate::init::check_usb_polling_usable(hz).is_ok());
        }
        assert!(validate_polling_rate(70_000).is_err());
    }

    fn fixture_devs() -> Vec<DiscoveredDevice> {
        vec![
            DiscoveredDevice {
                devnode: "/dev/hidraw1".into(),
                vendor_id: 0x05ac,
                product_id: 0x0324,
                transport: Transport::Usb,
            },
            DiscoveredDevice {
                devnode: "/dev/hidraw9".into(),
                vendor_id: 0x004c,
                product_id: 0x0324,
                transport: Transport::Bluetooth,
            },
        ]
    }

    #[test]
    fn classify_node_known_transports() {
        let devs = fixture_devs();
        assert_eq!(
            classify_node("/dev/hidraw1", &devs).unwrap(),
            Transport::Usb
        );
        assert_eq!(
            classify_node("/dev/hidraw9", &devs).unwrap(),
            Transport::Bluetooth
        );
    }

    #[test]
    fn classify_node_unknown_fails_closed() {
        let devs = fixture_devs();
        // Must err — never fall back to USB for a stray node.
        assert!(classify_node("/dev/hidraw0", &devs).is_err());
        assert!(classify_node("/dev/null", &devs).is_err());
    }

    #[test]
    fn init_node_bt_emits_ready_without_reports() {
        let hub = DeviceReadyHub::new();
        let dev = DiscoveredDevice {
            devnode: "/dev/hidraw9".into(),
            vendor_id: 0x004c,
            product_id: 0x0324,
            transport: Transport::Bluetooth,
        };
        let ev = init_node(&dev, 110, &hub).unwrap();
        assert_eq!(ev.transport, Transport::Bluetooth);
        assert_eq!(ev.kind, crate::init::ReadyKind::Ready);
    }

    #[test]
    fn retrying_usb_sequence_hits_recorder() {
        let rec = Recorder::default();
        let plan = init::init_usb_with_retry(&rec, 110).unwrap();
        assert_eq!(plan.x, 0x23);
        assert_eq!(rec.sent.borrow().len(), 3);
    }

    #[test]
    fn reconnect_mode_matrix() {
        // All 8 (live, force, disengage) combos: -d always wins.
        for (live, force) in [(true, true), (true, false), (false, true), (false, false)] {
            assert_eq!(
                resolve_reconnect_mode(live, force, true),
                ReconnectMode::Disengage,
                "disengage must win for live={live} force={force}"
            );
        }
        // Socket absent without -f → inactive (hardware left alone; the
        // automatic background form the udev template relies on).
        assert_eq!(
            resolve_reconnect_mode(false, false, false),
            ReconnectMode::Inactive
        );
        // Socket absent with -f → init anyway.
        assert_eq!(
            resolve_reconnect_mode(false, true, false),
            ReconnectMode::Init
        );
        // Daemon live → init (with or without -f).
        assert_eq!(
            resolve_reconnect_mode(true, false, false),
            ReconnectMode::Init
        );
        assert_eq!(
            resolve_reconnect_mode(true, true, false),
            ReconnectMode::Init
        );
    }

    #[test]
    fn resume_mode_matrix() {
        // Automatic background (hook/unit plain resume) without a live
        // daemon stays inactive; explicit -f forces; live daemon proceeds.
        assert_eq!(resolve_resume_mode(false, false), ResumeMode::Inactive);
        assert_eq!(resolve_resume_mode(false, true), ResumeMode::Proceed);
        assert_eq!(resolve_resume_mode(true, false), ResumeMode::Proceed);
        assert_eq!(resolve_resume_mode(true, true), ResumeMode::Proceed);
    }

    #[test]
    fn socket_absent_without_force_leaves_hardware_alone() {
        // Decision-level coverage for the udev socket-absent path: without
        // -f the mode must be Inactive (no init, no disengage write).
        assert_eq!(
            resolve_reconnect_mode(
                /* daemon_live */ false, /* force */ false, /* disengage */ false
            ),
            ReconnectMode::Inactive
        );
        assert_eq!(
            resolve_resume_mode(/* daemon_live */ false, /* force */ false),
            ResumeMode::Inactive
        );
    }

    #[test]
    fn inactive_reconnect_skips_node_wait_scan_and_classify() {
        // Lifecycle: installed-but-stopped (socket absent) is off. With no
        // live daemon and no -f, even a bogus node returns Ok immediately —
        // no 2 s node wait, no sysfs scan, no fail-closed classify error,
        // no writes.
        let _env_guard = crate::test_env::lock_env();
        let old = std::env::var(crate::lock::SOCKET_ENV).ok();
        let dir = tempfile::tempdir().unwrap();
        let absent = dir.path().join("no-daemon.sock");
        unsafe { std::env::set_var(crate::lock::SOCKET_ENV, &absent) };
        let cfg = Config::default();
        let result = run_reconnect("/nonexistent-hidraw-xyz", false, None, false, &cfg);
        match old {
            Some(v) => unsafe { std::env::set_var(crate::lock::SOCKET_ENV, v) },
            None => unsafe { std::env::remove_var(crate::lock::SOCKET_ENV) },
        }
        assert!(
            result.is_ok(),
            "inactive reconnect must exit 0, got {result:?}"
        );
    }

    #[test]
    fn inactive_resume_returns_empty_report_without_scan() {
        // Lifecycle: sleep hook / resume unit with no live daemon must not
        // rescan or re-init. Empty report, no errors, exit-0 shape.
        let _env_guard = crate::test_env::lock_env();
        let old = std::env::var(crate::lock::SOCKET_ENV).ok();
        let dir = tempfile::tempdir().unwrap();
        let absent = dir.path().join("no-daemon.sock");
        unsafe { std::env::set_var(crate::lock::SOCKET_ENV, &absent) };
        let cfg = Config::default();
        let report = run_resume(&cfg, None, false).unwrap();
        match old {
            Some(v) => unsafe { std::env::set_var(crate::lock::SOCKET_ENV, v) },
            None => unsafe { std::env::remove_var(crate::lock::SOCKET_ENV) },
        }
        assert!(report.nodes.is_empty());
        assert!(!report.failed());
    }

    #[test]
    fn inactive_classified_node_touches_nothing() {
        // Defensive arm: even if reached, Inactive performs no writes.
        let node = tempfile::NamedTempFile::new().unwrap();
        let cfg = Config::default();
        for transport in [Transport::Usb, Transport::Bluetooth] {
            reconnect_classified_node(
                node.path().to_str().unwrap(),
                transport,
                ReconnectMode::Inactive,
                None,
                &cfg,
            )
            .unwrap();
        }
        // A bogus node is equally fine — nothing is opened or classified.
        reconnect_classified_node(
            "/nonexistent-hidraw-xyz",
            Transport::Usb,
            ReconnectMode::Inactive,
            None,
            &cfg,
        )
        .unwrap();
    }

    #[test]
    fn bt_reconnect_needs_no_enhanced_session_in_every_mode() {
        // Exercise the classified one-shot path without host discovery or
        // socket access. No daemon/force/disengage combination owns a proxy.
        let node = tempfile::NamedTempFile::new().unwrap();
        for enhanced_clicks in [false, true] {
            let cfg = Config {
                enhanced_clicks,
                ..Config::default()
            };
            for daemon_live in [false, true] {
                for force in [false, true] {
                    for disengage in [false, true] {
                        let mode = resolve_reconnect_mode(daemon_live, force, disengage);
                        // BT ignores USB polling options even when invalid.
                        reconnect_classified_node(
                            node.path().to_str().unwrap(),
                            Transport::Bluetooth,
                            mode,
                            Some(0),
                            &cfg,
                        )
                        .unwrap();
                    }
                }
            }
        }
    }

    #[test]
    fn bt_init_without_proxy_and_failed_listener_keep_siblings_independent() {
        let hub = DeviceReadyHub::new();
        let mut devs = fixture_devs();
        // No daemon/proxy listener: init remains hardware-only, even at an
        // invalid USB rate. No enhanced path exists to replace native input.
        let first = devs.pop().unwrap();
        let ev = init_node(&first, 0, &hub).unwrap();
        assert_eq!(ev, init::init_bluetooth(&first.devnode));
        hub.subscribe(|_| panic!("simulated attachment failure"));
        let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
        let capture = Arc::clone(&seen);
        hub.subscribe(move |ev| capture.lock().unwrap().push(ev.clone()));
        let second = DiscoveredDevice {
            devnode: "/dev/hidraw-other-bt".into(),
            ..first.clone()
        };
        for dev in [&first, &second] {
            init_node(dev, 0, &hub).unwrap();
        }
        let seen = seen.lock().unwrap();
        assert_eq!(seen.len(), 2);
        assert_eq!(seen[0].devnode, first.devnode);
        assert_eq!(seen[1].devnode, second.devnode);
        assert!(seen.iter().all(|ev| ev.kind == init::ReadyKind::Ready));
    }

    #[test]
    fn known_bt_ticks_leave_live_session_ownership_alone() {
        let dev = fixture_devs().pop().unwrap();
        let hub = DeviceReadyHub::new();
        let events = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let capture = Arc::clone(&events);
        hub.subscribe(move |_| {
            capture.fetch_add(1, Ordering::SeqCst);
        });
        for enhanced_clicks in [false, true] {
            let cfg = Config {
                enhanced_clicks,
                ..Config::default()
            };
            let mut watch = ResumeWatch::new(std::slice::from_ref(&dev));
            for _ in 0..20 {
                assert!(watch
                    .tick_with_scan(&cfg, &hub, vec![dev.clone()], &|node| session_needs_attach(
                        enhanced_clicks,
                        Some(&dev.devnode),
                        node,
                        true
                    ))
                    .is_empty());
            }
        }
        assert_eq!(events.load(Ordering::SeqCst), 0);
    }

    /// True when `control` becomes readable within `timeout` (drains the
    /// byte so phases stay independent).
    fn control_wakes_within(
        control: &Arc<crate::pump::PumpControl>,
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
    fn pump_manager_attach_and_detach_wake_blocked_pump() {
        // Gate-off config: attach/detach touch no hardware (no uinput
        // create, no evdev/hidraw) but each owned transition must still
        // wake a poll-blocked pump via control. An unowned detach changes
        // nothing and must stay silent.
        let cfg = Config {
            enhanced_clicks: false,
            ..Config::default()
        };
        let control = Arc::new(crate::pump::PumpControl::new().unwrap());
        type DaemonProxy = ClickProxy<
            RealGrab,
            DualSink<RealUinput, RealUinput>,
            AsyncHaptic<RealHaptic>,
            PumpSource,
        >;
        let proxy = Arc::new(Mutex::new(DaemonProxy::new(
            crate::proxy::ProxySettings::from(&cfg),
            crate::proxy::RealGrab::new(""),
            crate::proxy::DualSink::new(
                crate::proxy::RealUinput::new("/dev/uinput"),
                crate::proxy::RealUinput::new_extras("/dev/uinput"),
            ),
            crate::proxy::AsyncHaptic::new(crate::proxy::RealHaptic::new()),
            crate::pump::PumpSource::Idle,
        )));
        let manager =
            PumpManager::new(proxy, cfg.clone(), Arc::new(AtomicBool::new(false)), || {
                PumpSource::Idle
            })
            .with_thread(std::thread::spawn(|| {}))
            .with_control(Arc::clone(&control));
        manager.attach(&crate::init::usb_ready("/dev/hidraw1"));
        assert!(
            control_wakes_within(&control, std::time::Duration::from_secs(2)),
            "attach did not notify a poll-blocked pump"
        );
        // Owned detach (seeded active owner) must wake too.
        *manager.active.lock().unwrap() = Some(ActiveSession {
            devnode: "/dev/hidraw1".to_string(),
            transport: Transport::Usb,
        });
        manager.detach(&crate::init::detached("/dev/hidraw1", Transport::Usb));
        assert!(
            control_wakes_within(&control, std::time::Duration::from_secs(2)),
            "owned detach did not notify a poll-blocked pump"
        );
        // Unowned sibling detach changes nothing: no wake.
        manager.detach(&crate::init::detached("/dev/hidraw9", Transport::Usb));
        assert!(
            !control_wakes_within(&control, std::time::Duration::from_millis(100)),
            "unowned detach must not wake the pump"
        );
    }
    // ---- finding #12: owned shutdown ----

    #[test]
    fn shutdown_signal_set_is_int_term_hup() {
        assert_eq!(
            shutdown_signals(),
            [libc::SIGINT, libc::SIGTERM, libc::SIGHUP]
        );
    }

    #[test]
    fn shutdown_handler_fires_on_int_term_hup() {
        // Real `raise(2)` through the installed handlers: each signal must
        // flip the process-lifetime static flag. Serialized: dispositions
        // and the static flag are process-global.
        let _serial = SIGNAL_TEST_SERIAL.lock().unwrap();
        let _guard = install_shutdown_handler().expect("install test handler");
        for sig in shutdown_signals() {
            SHUTDOWN_RUNNING.store(true, Ordering::SeqCst);
            unsafe {
                libc::raise(sig);
            }
            assert!(shutdown_requested(), "signal {sig} must request shutdown");
            assert!(
                !shutdown_running(),
                "signal {sig} must clear the running flag"
            );
        }
    }

    #[test]
    fn shutdown_signal_static_target_outlives_concurrent_handlers() {
        // By construction the handler touches only a `static`: no heap
        // allocation, no raw pointer, no reclamation. Concurrent/in-flight
        // raises remain safe, and the target is valid after guard drop.
        let _serial = SIGNAL_TEST_SERIAL.lock().unwrap();
        let _guard = install_shutdown_handler().expect("install test handler");
        // Static address is stable across installs.
        let addr_before: *const AtomicBool = &SHUTDOWN_RUNNING;
        SHUTDOWN_RUNNING.store(true, Ordering::SeqCst);
        let handles: Vec<_> = (0..4)
            .map(|_| {
                std::thread::spawn(|| unsafe {
                    libc::raise(libc::SIGINT);
                })
            })
            .collect();
        for h in handles {
            h.join().unwrap();
        }
        assert!(
            shutdown_requested(),
            "concurrent raises must flip the static flag"
        );
        let addr_after: *const AtomicBool = &SHUTDOWN_RUNNING;
        assert_eq!(
            addr_before, addr_after,
            "static target must not move across handlers"
        );
        drop(_guard);
        // Still valid after guard drop (restores dispositions, keeps static).
        assert!(shutdown_requested());
        SHUTDOWN_RUNNING.store(true, Ordering::SeqCst);
        assert!(shutdown_running());
        // Re-install succeeds: rollback/restoration left dispositions usable.
        let _guard2 = install_shutdown_handler().expect("reinstall must succeed");
        assert!(shutdown_running());
    }

    #[test]
    fn shutdown_signal_guard_restores_and_reinstalls() {
        // Guard drop restores previous dispositions; a fresh install works.
        // No signal is raised after drop (the default disposition for TERM
        // would terminate the test process), so restoration is proven by a
        // clean reinstall, not by delivering to the old disposition.
        let _serial = SIGNAL_TEST_SERIAL.lock().unwrap();
        {
            let _guard = install_shutdown_handler().expect("install must succeed");
            assert!(shutdown_running());
        }
        let _guard2 = install_shutdown_handler().expect("reinstall must succeed");
        assert!(shutdown_running());
    }

    #[test]
    fn session_shutdown_without_device_stops_pump_and_reports_healthy() {
        let hub = hub();
        let cfg = Config {
            enhanced_clicks: false,
            ..Config::default()
        };
        let session = subscribe_click_proxy(&cfg, &hub);
        assert!(session.active_session().is_none());
        let outcome = session.shutdown(&hub);
        assert!(!outcome.had_session);
        assert!(!outcome.worker_panicked);
        assert!(!outcome.pump_join_timed_out);
        assert!(!outcome.cleanup_incomplete);
        // Pump thread is gone (handle taken) and the stop flag is set.
        assert!(session.manager.stop.load(Ordering::SeqCst));
        assert!(session.manager.thread.lock().unwrap().is_none());
        // Idempotent: a repeat (and the `Drop` at scope end) is a no-op.
        let again = session.shutdown(&hub);
        assert_eq!(again, ShutdownOutcome::none());
    }

    #[test]
    fn shutdown_emits_detached_for_active_node_and_clears_it() {
        let hub = hub();
        let seen = Arc::new(Mutex::new(Vec::new()));
        let capture = Arc::clone(&seen);
        hub.subscribe(move |event: &DeviceReadyEvent| {
            if event.kind == ReadyKind::Detached {
                capture.lock().unwrap().push(event.devnode.clone());
            }
        });
        let cfg = Config {
            enhanced_clicks: true,
            ..Config::default()
        };
        let session = subscribe_click_proxy(&cfg, &hub);
        // Claim a node the way `attach` does, without hardware I/O: the
        // teardown below must still clear it and stay observable.
        session
            .manager
            .active
            .lock()
            .unwrap()
            .replace(ActiveSession {
                devnode: "/dev/hidraw9".to_string(),
                transport: Transport::Usb,
            });
        let outcome = session.shutdown(&hub);
        assert!(outcome.had_session);
        assert!(!outcome.worker_panicked);
        assert!(!outcome.pump_join_timed_out);
        assert!(!outcome.cleanup_incomplete);
        assert_eq!(*seen.lock().unwrap(), vec!["/dev/hidraw9".to_string()]);
        assert!(session.active_session().is_none());
        assert!(!session.manager.proxy.lock().unwrap().is_attached());
    }

    #[test]
    fn shutdown_inner_tears_down_remaining_wiring_without_hub_emit() {
        // Direct `shutdown_inner` (the `Drop` path): whatever is still
        // wired is torn down even when no hub `Detached` went out first.
        let hub = hub();
        let cfg = Config {
            enhanced_clicks: true,
            ..Config::default()
        };
        let session = subscribe_click_proxy(&cfg, &hub);
        session
            .manager
            .active
            .lock()
            .unwrap()
            .replace(ActiveSession {
                devnode: "/dev/hidraw9".to_string(),
                transport: Transport::Bluetooth,
            });
        let outcome = session.manager.shutdown_inner();
        assert!(outcome.had_session);
        assert!(!outcome.worker_panicked);
        assert!(!outcome.pump_join_timed_out);
        assert!(!outcome.cleanup_incomplete);
        assert!(session.active_session().is_none());
        assert!(session.manager.stop.load(Ordering::SeqCst));
    }

    #[test]
    fn pump_worker_panic_surfaces_through_bounded_join() {
        use crate::pump::HidrawReader;
        struct PanicReader;
        impl HidrawReader for PanicReader {
            fn read_report(&mut self, _buf: &mut [u8]) -> std::io::Result<usize> {
                panic!("simulated pump worker panic");
            }
        }
        type PanicProxy = ClickProxy<TestGrab, TestSink, LogHaptic, HidrawSource<PanicReader>>;
        let proxy = Arc::new(Mutex::new(PanicProxy::new(
            ProxySettings { enhanced: true },
            TestGrab::default(),
            TestSink::default(),
            LogHaptic,
            HidrawSource::new("fixture", PanicReader),
        )));
        // Attach so the pump actually polls the panicking source.
        proxy.lock().unwrap().set_pump_ready(true);
        proxy
            .lock()
            .unwrap()
            .attach(&crate::init::usb_ready("/dev/hidraw9"));
        assert!(proxy.lock().unwrap().is_attached());
        let stop = Arc::new(AtomicBool::new(false));
        let (handle, _control) = crate::pump::spawn_pump(
            "test-panic".to_string(),
            Arc::clone(&proxy),
            Config::default(),
            Arc::clone(&stop),
        );
        // The worker panics on its first frame; the bounded join must
        // report the payload instead of hanging or swallowing it.
        match crate::pump::join_bounded(handle, std::time::Duration::from_secs(5)) {
            crate::pump::WorkerJoin::Panicked(msg) => {
                assert!(msg.contains("simulated pump worker panic"), "got: {msg}");
            }
            crate::pump::WorkerJoin::Completed(()) => {
                panic!("panicking worker must not report completion")
            }
            crate::pump::WorkerJoin::TimedOut => {
                panic!("dead worker is finished; must not time out")
            }
        }
    }

    #[test]
    fn shutdown_inner_force_releases_surviving_live_session() {
        // The hub callback claimed `active` but left the proxy attached
        // (e.g. it never finished): with no active session left, the
        // forced-release branch must still drop the live session.
        let hub = hub();
        let cfg = Config {
            enhanced_clicks: true,
            ..Config::default()
        };
        let session = subscribe_click_proxy(&cfg, &hub);
        {
            let mut proxy = session.manager.proxy.lock().unwrap();
            proxy.test_claim_live("/dev/hidraw9");
            proxy.uinput().pointer().test_mark_created();
        }
        assert!(session.manager.proxy.lock().unwrap().is_attached());
        let outcome = session.manager.shutdown_inner();
        assert!(outcome.had_session);
        assert!(!outcome.worker_panicked);
        assert!(!outcome.pump_join_timed_out);
        assert!(!outcome.cleanup_incomplete);
        let proxy = session.manager.proxy.lock().unwrap();
        assert!(!proxy.is_attached());
        assert!(!proxy.uinput().is_ready());
        assert!(!proxy.grab().is_grabbed());
    }

    #[test]
    fn daemon_session_drop_teardowns_without_prior_shutdown() {
        // `Drop` without an explicit `shutdown` still stops and joins the
        // pump and clears claimed wiring (best-effort, no hub emit).
        let hub = hub();
        let seen = Arc::new(Mutex::new(Vec::new()));
        let capture = Arc::clone(&seen);
        hub.subscribe(move |event: &DeviceReadyEvent| {
            if event.kind == ReadyKind::Detached {
                capture.lock().unwrap().push(event.devnode.clone());
            }
        });
        let cfg = Config {
            enhanced_clicks: true,
            ..Config::default()
        };
        let session = subscribe_click_proxy(&cfg, &hub);
        let manager = Arc::clone(&session.manager);
        manager.active.lock().unwrap().replace(ActiveSession {
            devnode: "/dev/hidraw9".to_string(),
            transport: Transport::Usb,
        });
        drop(session);
        assert!(manager.stop.load(Ordering::SeqCst));
        assert!(manager.thread.lock().unwrap().is_none());
        assert!(manager.active.lock().unwrap().is_none());
        assert!(seen.lock().unwrap().is_empty());
    }

    #[test]
    fn daemon_session_shutdown_surfaces_worker_panic_end_to_end() {
        // A panicking pump worker surfaces through `DaemonSession::shutdown`,
        // not just through the `join_bounded` unit path.
        let hub = hub();
        let cfg = Config {
            enhanced_clicks: false,
            ..Config::default()
        };
        let session = subscribe_click_proxy(&cfg, &hub);
        let old = session.manager.thread.lock().unwrap().take();
        assert!(old.is_some(), "pump thread must be running");
        session
            .manager
            .thread
            .lock()
            .unwrap()
            .replace(std::thread::spawn(|| panic!("e2e worker panic")));
        drop(old); // detaches the healthy pump thread; the stop flag exits it
        let outcome = session.shutdown(&hub);
        assert!(outcome.worker_panicked);
        assert!(!outcome.pump_join_timed_out);
        assert!(!outcome.cleanup_incomplete);
        assert!(outcome.failed());
    }

    #[test]
    fn daemon_session_shutdown_surfaces_pump_join_timeout_end_to_end() {
        // A wedged pump thread detaches after the bound instead of hanging
        // shutdown, and the outcome reports it.
        let hub = hub();
        let cfg = Config {
            enhanced_clicks: false,
            ..Config::default()
        };
        let session = subscribe_click_proxy(&cfg, &hub);
        let old = session.manager.thread.lock().unwrap().take();
        assert!(old.is_some(), "pump thread must be running");
        session
            .manager
            .thread
            .lock()
            .unwrap()
            .replace(std::thread::spawn(|| {
                std::thread::sleep(std::time::Duration::from_secs(3600))
            }));
        drop(old); // detaches the healthy pump thread; the stop flag exits it
        let start = std::time::Instant::now();
        let outcome = session.shutdown(&hub);
        assert!(!outcome.worker_panicked);
        assert!(outcome.pump_join_timed_out);
        assert!(!outcome.cleanup_incomplete);
        assert!(outcome.failed());
        assert!(
            start.elapsed() >= PUMP_JOIN_TIMEOUT,
            "join must wait out the bound: {:?}",
            start.elapsed()
        );
        assert!(
            start.elapsed() < PUMP_JOIN_TIMEOUT + std::time::Duration::from_secs(30),
            "join must not hang past the bound: {:?}",
            start.elapsed()
        );
    }

    // ---- finding #12 regressions: wedged worker owns the real proxy mutex ----
    //
    // Short injected budgets (not repeated 8 s waits): join 200 ms, proxy
    // 200 ms, haptic 50 ms. The fixture holds the ACTUAL manager proxy
    // mutex the way `pump_once` does across `handle_frame`, so an unbounded
    // implementation would hang past the bound.

    fn retire_healthy_pump(session: &DaemonSession) {
        // Stop and join the healthy pump thread so the fixture can model a
        // stalled frame holding the same mutex. Restores `stop=false` so the
        // shutdown under test establishes it.
        session.manager.stop.store(true, Ordering::SeqCst);
        let old = session
            .manager
            .thread
            .lock()
            .unwrap()
            .take()
            .expect("pump thread must be running");
        old.join().expect("healthy pump must join");
        session.manager.stop.store(false, Ordering::SeqCst);
    }

    #[test]
    fn explicit_shutdown_bounded_when_worker_holds_proxy_mutex() {
        use std::time::Duration;
        let join_budget = Duration::from_millis(200);
        let proxy_budget = Duration::from_millis(200);
        let haptic_budget = Duration::from_millis(50);
        let hub = hub();
        let seen = Arc::new(Mutex::new(Vec::new()));
        let capture = Arc::clone(&seen);
        hub.subscribe(move |event: &DeviceReadyEvent| {
            if event.kind == ReadyKind::Detached {
                capture.lock().unwrap().push(event.devnode.clone());
            }
        });
        let cfg = Config {
            enhanced_clicks: true,
            ..Config::default()
        };
        let session = subscribe_click_proxy(&cfg, &hub);
        retire_healthy_pump(&session);
        // Fixture holds the ACTUAL proxy mutex until the gate opens.
        let proxy = Arc::clone(&session.manager.proxy);
        let (locked_tx, locked_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
        let wedged = std::thread::spawn(move || {
            let _held = proxy.lock().unwrap();
            locked_tx.send(()).unwrap();
            let _ = release_rx.recv();
        });
        locked_rx.recv().unwrap();
        *session.manager.thread.lock().unwrap() = Some(wedged);
        session
            .manager
            .active
            .lock()
            .unwrap()
            .replace(ActiveSession {
                devnode: "/dev/hidraw-review-fixture".to_string(),
                transport: Transport::Usb,
            });
        let start = std::time::Instant::now();
        let outcome = session.shutdown_with_budget(&hub, join_budget, proxy_budget, haptic_budget);
        let elapsed = start.elapsed();
        // Stop established FIRST, before any blocking cleanup.
        assert!(
            session.manager.stop.load(Ordering::SeqCst),
            "stop must be set even when the worker owns proxy"
        );
        // Bounded: well under the production 8 s budget.
        assert!(
            elapsed < Duration::from_secs(2),
            "explicit shutdown must return within bound, took {elapsed:?}"
        );
        assert!(outcome.had_session);
        assert!(outcome.pump_join_timed_out);
        assert!(outcome.cleanup_incomplete);
        assert!(outcome.failed());
        // Healthy observability preserved even when incomplete: the
        // post-teardown emit still notifies other subscribers.
        assert_eq!(
            *seen.lock().unwrap(),
            vec!["/dev/hidraw-review-fixture".to_string()]
        );
        // Own detach is a no-op (active already taken): no double cleanup.
        assert!(session.active_session().is_none());
        // Release the gate so the detached fixture can exit and free the
        // mutex; poll until the lock is reclaimable (detached, cannot join).
        release_tx.send(()).unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        loop {
            if session.manager.proxy.try_lock().is_ok() {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "fixture must release the proxy mutex after the gate opens"
            );
            std::thread::sleep(Duration::from_millis(5));
        }
        // Repeated shutdown/Drop must not wait again.
        let start2 = std::time::Instant::now();
        let again = session.shutdown_with_budget(&hub, join_budget, proxy_budget, haptic_budget);
        assert_eq!(again, ShutdownOutcome::none());
        assert!(
            start2.elapsed() < Duration::from_millis(100),
            "repeat shutdown must not wait, took {:?}",
            start2.elapsed()
        );
        assert!(
            seen.lock().unwrap().len() == 1,
            "repeat must not emit again"
        );
    }

    #[test]
    fn drop_path_bounded_when_worker_holds_proxy_mutex() {
        use std::time::Duration;
        // Same fixture through `shutdown_inner_with_budget`: the exact code
        // `Drop` calls (no hub emit). Short budgets, no 8 s wait.
        let join_budget = Duration::from_millis(200);
        let proxy_budget = Duration::from_millis(200);
        let haptic_budget = Duration::from_millis(50);
        let hub = hub();
        let cfg = Config {
            enhanced_clicks: true,
            ..Config::default()
        };
        let session = subscribe_click_proxy(&cfg, &hub);
        retire_healthy_pump(&session);
        let proxy = Arc::clone(&session.manager.proxy);
        let (locked_tx, locked_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
        let wedged = std::thread::spawn(move || {
            let _held = proxy.lock().unwrap();
            locked_tx.send(()).unwrap();
            let _ = release_rx.recv();
        });
        locked_rx.recv().unwrap();
        *session.manager.thread.lock().unwrap() = Some(wedged);
        session
            .manager
            .active
            .lock()
            .unwrap()
            .replace(ActiveSession {
                devnode: "/dev/hidraw-review-fixture".to_string(),
                transport: Transport::Usb,
            });
        let start = std::time::Instant::now();
        let (outcome, _taken) =
            session
                .manager
                .shutdown_inner_with_budget(join_budget, proxy_budget, haptic_budget);
        let elapsed = start.elapsed();
        assert!(
            session.manager.stop.load(Ordering::SeqCst),
            "stop must be set even when the worker owns proxy"
        );
        assert!(
            elapsed < Duration::from_secs(2),
            "Drop-path shutdown must return within bound, took {elapsed:?}"
        );
        assert!(outcome.had_session);
        assert!(outcome.pump_join_timed_out);
        assert!(outcome.cleanup_incomplete);
        assert!(outcome.failed());
        assert!(session.active_session().is_none());
        release_tx.send(()).unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        loop {
            if session.manager.proxy.try_lock().is_ok() {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "fixture must release the proxy mutex after the gate opens"
            );
            std::thread::sleep(Duration::from_millis(5));
        }
        // Repeat (what `Drop` at scope end does) waits for nothing.
        let start2 = std::time::Instant::now();
        let (again, taken2) =
            session
                .manager
                .shutdown_inner_with_budget(join_budget, proxy_budget, haptic_budget);
        assert_eq!(again, ShutdownOutcome::none());
        assert!(taken2.is_none());
        assert!(
            start2.elapsed() < Duration::from_millis(100),
            "repeat Drop must not wait, took {:?}",
            start2.elapsed()
        );
    }

    #[test]
    fn poisoned_proxy_recovers_during_bounded_shutdown() {
        use std::time::Duration;
        // Poison the proxy mutex, then shut down with short budgets: the
        // bounded acquire must recover the guard (not report incomplete).
        let hub = hub();
        let cfg = Config {
            enhanced_clicks: true,
            ..Config::default()
        };
        let session = subscribe_click_proxy(&cfg, &hub);
        session
            .manager
            .active
            .lock()
            .unwrap()
            .replace(ActiveSession {
                devnode: "/dev/hidraw-poison".to_string(),
                transport: Transport::Usb,
            });
        let proxy = Arc::clone(&session.manager.proxy);
        let poisoner = std::thread::spawn(move || {
            let _guard = proxy.lock().unwrap();
            panic!("poison the proxy mutex");
        });
        let _ = poisoner.join();
        assert!(session.manager.proxy.is_poisoned());
        let outcome = session.shutdown_with_budget(
            &hub,
            Duration::from_millis(200),
            Duration::from_millis(200),
            Duration::from_millis(50),
        );
        assert!(outcome.had_session);
        assert!(!outcome.worker_panicked);
        assert!(!outcome.cleanup_incomplete);
        assert!(session.active_session().is_none());
    }

    #[test]
    fn needs_attach_does_not_hang_when_proxy_busy() {
        use std::time::Duration;
        // Reconciliation must stay responsive (and let the loop observe the
        // shutdown flag) even when the worker owns the proxy.
        let hub = hub();
        let cfg = Config {
            enhanced_clicks: true,
            ..Config::default()
        };
        let session = subscribe_click_proxy(&cfg, &hub);
        retire_healthy_pump(&session);
        let proxy = Arc::clone(&session.manager.proxy);
        let (locked_tx, locked_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
        let holder = std::thread::spawn(move || {
            let _held = proxy.lock().unwrap();
            locked_tx.send(()).unwrap();
            let _ = release_rx.recv();
        });
        locked_rx.recv().unwrap();
        let start = std::time::Instant::now();
        // Must return promptly (bounded try-lock), not hang.
        let _ = session.needs_attach("/dev/hidraw9");
        assert!(
            start.elapsed() < Duration::from_secs(2),
            "needs_attach must be bounded, took {:?}",
            start.elapsed()
        );
        release_tx.send(()).unwrap();
        holder.join().unwrap();
    }
}
