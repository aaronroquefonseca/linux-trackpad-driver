//! Real hidraw pump: nonblocking read loop decoding 9-byte blocks.
//!
//! Reimplemented from the old userspace service behavior (read how it
//! pumped hidraw frames then reimplemented; no code copied):
//! - 512 B buffer, one `read(2)` per poll step, report ID in `buf[0]`.
//! - USB `0x02` = 12-byte prefix, BT `0x31` = 4-byte prefix, `0xf7` =
//!   two back-to-back sub-reports (first length byte at offset 1).
//! - One pump thread per attached session ([`spawn_pump`]), frame rate
//!   bounded by the device (no throttle; idle blocks in `poll(2)` on the
//!   hidraw fd + a control pipe, waking only on input, attach/detach, or
//!   a timing deadline — (near-)zero wakeups with no device or idling
//!   with no touch). NOTE: the daemon never flips the stop flag —
//!   the thread lives until process exit by design. A dead pump
//!   surfaces through the session teardown paths (grab released,
//!   passthrough), never through the handle, which is only joined in
//!   tests.
//! - Malformed frames are logged + dropped per frame, never abort.
//!
//! Decode itself reuses [`crate::apple_parse::parse_frame`] /
//! [`crate::apple_parse::parse_double_report`].

use std::collections::VecDeque;
use std::os::unix::io::RawFd;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::Result;

use crate::apple_parse::{TouchFrame, TouchSource, DOUBLE_REPORT_ID};
use crate::proxy::PumpActivity;

/// Max hidraw report size (matches old service buffer reuse).
pub const MAX_REPORT_BYTES: usize = 512;

/// Backoff after a transient (non-disconnect) read failure so a
/// persistently failing fd cannot busy-spin. Idle (no data) never sleeps:
/// it blocks in `poll(2)` until input/control/deadline. Only errors wait.
pub const ERROR_BACKOFF: Duration = Duration::from_millis(10);

/// Bounded retries for transient transport gaps before giving up on a
/// poll step. BlueZ BT hidraw can fragment/coalesce and briefly return
/// 0-byte / short / `EAGAIN` / `EINTR` mid-click; one such hiccup must
/// not tear down the session (which would emit held ups + SYN and let
/// hotplug re-attach re-press = triple-click). Only sustained
/// 0-byte exhaustion, or `ENODEV`/`ENOENT`, signals
/// [`DisconnectError`].
pub const TRANSIENT_READ_RETRIES: usize = 3;
/// Backoff between transient-read retries (few ms; BT ignores polling
/// rate, so this never changes input rate).
pub const TRANSIENT_RETRY_DELAY: Duration = Duration::from_millis(2);
/// Smallest valid single-frame prefix (BT `0x31` = 4 B). Reads with
/// `0 < n < MIN` are transport-shorts (fragment), retried like 0-byte;
/// longer reads go to the parser, where prefix-vs-body mismatches are
/// counted separately as malformed frames.
pub const MIN_TRANSPORT_REPORT_LEN: usize = 4;

/// Injectable hidraw read backend (real fd vs scripted mock for CI).
pub trait HidrawReader: Send {
    fn read_report(&mut self, buf: &mut [u8]) -> std::io::Result<usize>;
    /// Readiness fd for the pump's `poll(2)` wait. `None` for scripted
    /// backends (tests): the event loop then waits on control/deadlines.
    fn poll_fd(&self) -> Option<RawFd> {
        None
    }
}

/// Persistent device loss (unplug/replug under a live fd): sustained
/// 0-byte reads (node dropped the device) or `ENODEV`/`ENOENT` from
/// `read(2)`. Transient gaps (single 0-byte / short / `EAGAIN` / `EINTR`,
/// retried with backoff inside `poll_frame`) are NOT this. The proxy
/// classifies on this type: a disconnect self-releases the session
/// (bounded logs, fd rebind on re-attach) instead of warning ~500×/s
/// forever on a dead fd.
#[derive(Debug)]
pub struct DisconnectError(pub String);

impl std::fmt::Display for DisconnectError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "hidraw disconnect: {}", self.0)
    }
}

impl std::error::Error for DisconnectError {}

/// True when an [`anyhow::Error`] from `poll_frame` is a persistent
/// disconnect (see [`DisconnectError`]).
pub fn is_disconnect(e: &anyhow::Error) -> bool {
    e.downcast_ref::<DisconnectError>().is_some()
}

/// Real file-backed reader.
pub struct FileReader {
    file: std::fs::File,
}

impl FileReader {
    pub fn open_nonblocking(devnode: &str) -> std::io::Result<Self> {
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(devnode)?;
        set_nonblocking(&file)?;
        Ok(Self { file })
    }
}

impl HidrawReader for FileReader {
    fn read_report(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        use std::os::unix::io::AsRawFd;
        let ret = unsafe { libc::read(self.file.as_raw_fd(), buf.as_mut_ptr().cast(), buf.len()) };
        if ret < 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(ret as usize)
    }

    fn poll_fd(&self) -> Option<RawFd> {
        use std::os::unix::io::AsRawFd;
        Some(self.file.as_raw_fd())
    }
}

fn set_nonblocking(file: &std::fs::File) -> std::io::Result<()> {
    use std::os::unix::io::AsRawFd;
    let fd = file.as_raw_fd();
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags < 0 {
        return Err(std::io::Error::last_os_error());
    }
    let ret = unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) };
    if ret < 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

/// Scripted reader for tests: queued reports / errors, no hardware.
#[derive(Debug, Default)]
pub struct ScriptReader {
    queue: VecDeque<ScriptStep>,
}

/// One scripted read outcome.
#[derive(Debug)]
pub enum ScriptStep {
    Bytes(Vec<u8>),
    WouldBlock,
    IoError(i32),
}

impl ScriptReader {
    pub fn new(steps: Vec<ScriptStep>) -> Self {
        Self {
            queue: steps.into_iter().collect(),
        }
    }

    pub fn bytes(frames: Vec<Vec<u8>>) -> Self {
        Self::new(frames.into_iter().map(ScriptStep::Bytes).collect())
    }
}

impl HidrawReader for ScriptReader {
    fn read_report(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        match self.queue.pop_front() {
            None => Err(std::io::Error::from_raw_os_error(libc::EAGAIN)),
            Some(ScriptStep::WouldBlock) => Err(std::io::Error::from_raw_os_error(libc::EAGAIN)),
            Some(ScriptStep::IoError(e)) => Err(std::io::Error::from_raw_os_error(e)),
            Some(ScriptStep::Bytes(b)) => {
                let n = b.len().min(buf.len());
                buf[..n].copy_from_slice(&b[..n]);
                Ok(n)
            }
        }
    }
}

/// Real hidraw touch source: nonblocking reads + [`crate::apple_parse`] decode.
///
/// A queued second frame from a `0xf7` double report is returned first on
/// the next poll without touching the fd.
pub struct HidrawSource<R: HidrawReader> {
    devnode: String,
    reader: R,
    buf: Vec<u8>,
    pending: VecDeque<TouchFrame>,
    transient_retries: u64,
    transport_shorts: u64,
    malformed_frames: u64,
}

impl<R: HidrawReader> HidrawSource<R> {
    pub fn new(devnode: &str, reader: R) -> Self {
        Self {
            devnode: devnode.to_string(),
            reader,
            buf: vec![0u8; MAX_REPORT_BYTES],
            pending: VecDeque::new(),
            transient_retries: 0,
            transport_shorts: 0,
            malformed_frames: 0,
        }
    }

    pub fn devnode(&self) -> &str {
        &self.devnode
    }

    /// Transient retry count (0-byte/short/`EAGAIN`/`EINTR` retries).
    pub fn transient_retries(&self) -> u64 {
        self.transient_retries
    }

    /// Transport-short reads observed (`0 < n < prefix`).
    pub fn transport_shorts(&self) -> u64 {
        self.transport_shorts
    }

    /// Parse-level malformed frames dropped (parser rejected full read).
    pub fn malformed_frames(&self) -> u64 {
        self.malformed_frames
    }

    /// Queued second halves from `0xF7` (fd-free drain pending).
    pub fn pending_len(&self) -> usize {
        self.pending.len()
    }
}

impl HidrawSource<FileReader> {
    pub fn open(devnode: &str) -> Result<Self> {
        let reader = FileReader::open_nonblocking(devnode)
            .map_err(|e| anyhow::anyhow!("opening hidraw '{devnode}' nonblocking: {e}"))?;
        Ok(Self::new(devnode, reader))
    }
}

impl<R: HidrawReader> TouchSource for HidrawSource<R> {
    fn tier(&self) -> crate::apple_parse::CapabilityTier {
        crate::apple_parse::CapabilityTier::L2p
    }

    fn pressure_units(&self) -> crate::apple_parse::PressureUnits {
        crate::apple_parse::PressureUnits::Arbitrary
    }

    fn poll_fd(&self) -> Option<RawFd> {
        if self.pending.is_empty() {
            self.reader.poll_fd()
        } else {
            // Queued double-report half drains without touching the fd;
            // report no fd so the loop drains immediately instead of
            // blocking, then re-checks readiness.
            None
        }
    }

    fn has_pending_frame(&self) -> bool {
        !self.pending.is_empty()
    }

    fn poll_frame(&mut self) -> Result<Option<TouchFrame>> {
        // Queued second half of a 0xF7 double report drains fd-free
        // first, so a malformed follow-up read can never clobber it.
        // Preserve (never clear) `pending` on any later error path.
        if let Some(f) = self.pending.pop_front() {
            log::debug!(
                "pump {}: draining queued 0xF7 second half ({} left)",
                self.devnode,
                self.pending.len()
            );
            return Ok(Some(f));
        }
        // Transient-read tolerance: bounded retries with few-ms backoff
        // for 0-byte / transport-short / EAGAIN / EINTR. Exhaustion of
        // 0-byte -> Disconnect (node really gone); exhaustion of short /
        // EAGAIN / EINTR -> idle None (drop the fragment, no teardown).
        // ENODEV/ENOENT -> immediate Disconnect. Other errno -> immediate
        // generic error (proxy drops the frame, no teardown).
        let mut attempt: usize = 0;
        let n = loop {
            match self.reader.read_report(&mut self.buf) {
                Ok(0) => {
                    self.transient_retries += 1;
                    if attempt < TRANSIENT_READ_RETRIES {
                        log::debug!(
                            "pump {}: transient 0-byte read (attempt {}/{})",
                            self.devnode,
                            attempt + 1,
                            TRANSIENT_READ_RETRIES
                        );
                        std::thread::sleep(TRANSIENT_RETRY_DELAY);
                        attempt += 1;
                        continue;
                    }
                    return Err(DisconnectError(format!(
                        "{} sustained 0-byte reads (device dropped the node)",
                        self.devnode
                    ))
                    .into());
                }
                Ok(n) if n < MIN_TRANSPORT_REPORT_LEN => {
                    self.transient_retries += 1;
                    self.transport_shorts += 1;
                    if attempt < TRANSIENT_READ_RETRIES {
                        log::debug!(
                            "pump {}: transport-short {n} B (attempt {}/{})",
                            self.devnode,
                            attempt + 1,
                            TRANSIENT_READ_RETRIES
                        );
                        std::thread::sleep(TRANSIENT_RETRY_DELAY);
                        attempt += 1;
                        continue;
                    }
                    log::warn!(
                        "pump {}: sustained transport-short {n} B, dropping frame",
                        self.devnode
                    );
                    return Ok(None);
                }
                Ok(n) => break n,
                Err(e) if e.raw_os_error() == Some(libc::EAGAIN) => {
                    self.transient_retries += 1;
                    if attempt < TRANSIENT_READ_RETRIES {
                        log::debug!(
                            "pump {}: transient EAGAIN (attempt {}/{})",
                            self.devnode,
                            attempt + 1,
                            TRANSIENT_READ_RETRIES
                        );
                        std::thread::sleep(TRANSIENT_RETRY_DELAY);
                        attempt += 1;
                        continue;
                    }
                    return Ok(None);
                }
                Err(e) if e.raw_os_error() == Some(libc::EINTR) => {
                    self.transient_retries += 1;
                    if attempt < TRANSIENT_READ_RETRIES {
                        log::debug!(
                            "pump {}: transient {e} (attempt {}/{})",
                            self.devnode,
                            attempt + 1,
                            TRANSIENT_READ_RETRIES
                        );
                        std::thread::sleep(TRANSIENT_RETRY_DELAY);
                        attempt += 1;
                        continue;
                    }
                    return Ok(None);
                }
                Err(e)
                    if e.raw_os_error() == Some(libc::ENODEV)
                        || e.raw_os_error() == Some(libc::ENOENT) =>
                {
                    return Err(DisconnectError(format!("{} read: {e}", self.devnode)).into());
                }
                Err(e) => return Err(anyhow::anyhow!("hidraw {} read: {e}", self.devnode)),
            }
        };
        let report = &self.buf[..n];
        let Some(&id) = report.first() else {
            // Unreachable for n >= MIN_TRANSPORT_REPORT_LEN, but keep the
            // guard: count as transport-short, never abort, never touch
            // pending (already drained above).
            self.transport_shorts += 1;
            log::warn!("pump {}: empty report, dropping frame", self.devnode);
            return Ok(None);
        };
        if id == DOUBLE_REPORT_ID {
            match crate::apple_parse::parse_double_report(report) {
                Ok((first, second)) => {
                    self.pending.push_back(second);
                    Ok(Some(first))
                }
                Err(e) => {
                    // Malformed double: pending was already drained, so
                    // nothing queued is lost; just count + drop.
                    self.malformed_frames += 1;
                    log::warn!(
                        "pump {}: malformed double report ({n} B), dropping frame: {e:#}",
                        self.devnode
                    );
                    Ok(None)
                }
            }
        } else {
            match crate::apple_parse::parse_frame(report) {
                Ok(f) => Ok(Some(f)),
                Err(e) => {
                    // Distinguish transport-short (fewer bytes than the
                    // report id's prefix) from parse errors (bad body /
                    // unknown id). Either way: count, drop the frame,
                    // leave pending untouched.
                    if is_transport_short(id, n) {
                        self.transport_shorts += 1;
                        log::warn!(
                            "pump {}: transport-short report id {id:#04x} ({n} B), dropping frame: {e:#}",
                            self.devnode
                        );
                    } else {
                        self.malformed_frames += 1;
                        log::warn!(
                            "pump {}: malformed report id {id:#04x} ({n} B), dropping frame: {e:#}",
                            self.devnode
                        );
                    }
                    Ok(None)
                }
            }
        }
    }
}

/// True when `n` bytes cannot hold the prefix for report `id`
/// (fragment, not a parser-level malformed body).
fn is_transport_short(id: u8, n: usize) -> bool {
    let prefix = match id {
        crate::apple_parse::TRACKPAD2_USB_REPORT_ID => 12,
        crate::apple_parse::TRACKPAD2_BT_REPORT_ID => 4,
        crate::apple_parse::DOUBLE_REPORT_ID => 2,
        _ => return false,
    };
    n < prefix
}

/// Manager-owned source: idle until a hidraw node attaches.
/// Lets the daemon keep ONE proxy type (idle until a node attaches):
/// `Idle` polls to no-op, `Live` drains its hidraw node.
pub enum PumpSource {
    Idle,
    Live(HidrawSource<FileReader>),
}

impl TouchSource for PumpSource {
    fn tier(&self) -> crate::apple_parse::CapabilityTier {
        crate::apple_parse::CapabilityTier::L2p
    }
    fn pressure_units(&self) -> crate::apple_parse::PressureUnits {
        crate::apple_parse::PressureUnits::Arbitrary
    }
    fn poll_fd(&self) -> Option<RawFd> {
        match self {
            PumpSource::Idle => None,
            PumpSource::Live(s) => s.poll_fd(),
        }
    }
    fn has_pending_frame(&self) -> bool {
        match self {
            PumpSource::Idle => false,
            PumpSource::Live(s) => s.has_pending_frame(),
        }
    }
    fn poll_frame(&mut self) -> Result<Option<TouchFrame>> {
        match self {
            PumpSource::Idle => Ok(None),
            PumpSource::Live(s) => s.poll_frame(),
        }
    }
}

/// Explicit wakeup for the event-driven pump: a nonblocking self-pipe.
/// The manager calls [`PumpControl::notify`] after every attach/detach
/// (source fd / deadline set changed) and on shutdown; the pump thread
/// `poll(2)`s the read end alongside the hidraw fd and drains it on wake.
/// Pipe writes are thread-safe (kernel); drain/poll run on the pump thread.
/// Atomic `pipe2(O_NONBLOCK | O_CLOEXEC)` sets both flags at creation, so no
/// post-creation `fcntl` race and no manual close bookkeeping: `OwnedFd`
/// closes exactly once and is `Send + Sync`, so no explicit unsafe impls.
pub struct PumpControl {
    read: std::os::unix::io::OwnedFd,
    write: std::os::unix::io::OwnedFd,
}

impl PumpControl {
    pub fn new() -> std::io::Result<Self> {
        use std::os::unix::io::FromRawFd;
        let mut fds = [0 as RawFd; 2];
        if unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_NONBLOCK | libc::O_CLOEXEC) } != 0 {
            return Err(std::io::Error::last_os_error());
        }
        // SAFETY: `pipe2` succeeded, so both entries are valid owned fds.
        let [read_fd, write_fd] = fds;
        Ok(Self {
            read: unsafe { std::os::unix::io::OwnedFd::from_raw_fd(read_fd) },
            write: unsafe { std::os::unix::io::OwnedFd::from_raw_fd(write_fd) },
        })
    }

    pub fn read_fd(&self) -> RawFd {
        use std::os::unix::io::AsRawFd;
        self.read.as_raw_fd()
    }

    /// Wake the pump thread (attach/detach/shutdown). Nonblocking: a full
    /// pipe means the pump is already awake, so `EAGAIN` is success.
    pub fn notify(&self) {
        use std::os::unix::io::AsRawFd;
        let byte = [1u8];
        loop {
            let ret =
                unsafe { libc::write(self.write.as_raw_fd(), byte.as_ptr().cast(), byte.len()) };
            if ret == 1 {
                break;
            }
            if ret < 0 {
                let e = std::io::Error::last_os_error();
                if e.kind() == std::io::ErrorKind::Interrupted {
                    continue;
                }
                break;
            }
            break;
        }
    }

    /// Drain all pending wakeups (pump thread only).
    pub fn drain(&self) {
        use std::os::unix::io::AsRawFd;
        let mut buf = [0u8; 64];
        loop {
            let ret =
                unsafe { libc::read(self.read.as_raw_fd(), buf.as_mut_ptr().cast(), buf.len()) };
            if ret <= 0 {
                if ret < 0 {
                    let e = std::io::Error::last_os_error();
                    if e.kind() == std::io::ErrorKind::Interrupted {
                        continue;
                    }
                }
                break;
            }
        }
    }
}

/// One pump step: single `pump_step_at` call with `Instant::now`.
/// Returns traffic vs idle so callers (and tests) can distinguish a handled
/// hidraw frame or deadline from a no-op poll.
pub fn pump_once<G, U, H, S>(
    proxy: &Arc<Mutex<crate::proxy::ClickProxy<G, U, H, S>>>,
    cfg: &crate::config::Config,
) -> PumpActivity
where
    G: crate::proxy::GrabDevice,
    U: crate::proxy::UinputSink,
    H: crate::proxy::HapticSender,
    S: TouchSource,
{
    pump_once_at(proxy, cfg, Instant::now())
}

/// [`pump_once`] with an explicit clock (deterministic deadline tests).
pub fn pump_once_at<G, U, H, S>(
    proxy: &Arc<Mutex<crate::proxy::ClickProxy<G, U, H, S>>>,
    cfg: &crate::config::Config,
    now: Instant,
) -> PumpActivity
where
    G: crate::proxy::GrabDevice,
    U: crate::proxy::UinputSink,
    H: crate::proxy::HapticSender,
    S: TouchSource,
{
    let mut px = proxy.lock().unwrap_or_else(|e| e.into_inner());
    let (result, activity) = px.pump_step_at(cfg, now);
    // Output-failure logging has one owner: the proxy logs and retires the
    // session in `output_failed`; the pump only propagates the activity.
    let _ = result;
    activity
}

/// Run one step and back off once on a transient read failure, so a
/// persistently failing fd cannot busy-spin. Returns the step activity:
/// `Frame` still drains immediately (no sleep), `Transient` sleeps once.
fn pump_once_with_backoff<G, U, H, S>(
    proxy: &Arc<Mutex<crate::proxy::ClickProxy<G, U, H, S>>>,
    cfg: &crate::config::Config,
) -> PumpActivity
where
    G: crate::proxy::GrabDevice,
    U: crate::proxy::UinputSink,
    H: crate::proxy::HapticSender,
    S: TouchSource,
{
    let activity = pump_once(proxy, cfg);
    if activity == PumpActivity::Transient {
        std::thread::sleep(ERROR_BACKOFF);
    }
    activity
}

/// Set `stop` and wake the pump thread so a `poll(2)`-blocked loop exits
/// promptly. The loop also checks `stop` after every wakeup.
pub fn stop_pump(stop: &Arc<AtomicBool>, control: &Arc<PumpControl>) {
    stop.store(true, Ordering::SeqCst);
    control.notify();
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PollWake {
    Control,
    Hidraw,
    Both,
    Timeout,
    /// `poll(2)` itself failed with a persistent non-EINTR error (EINVAL,
    /// ENOMEM, ...). Distinct from `Timeout` so the caller can bound retries
    /// even when idle: a `Timeout` with no fd/deadline does no work and must
    /// not sleep, while a `WaitError` must back off unconditionally or the
    /// loop would spin on the immediately-failing wait.
    WaitError,
}

type RawPollFn = unsafe extern "C" fn(*mut libc::pollfd, libc::nfds_t, i32) -> i32;

fn poll_wait_with(
    hidraw_fd: Option<RawFd>,
    control_fd: RawFd,
    timeout_ms: i32,
    poll_fn: RawPollFn,
) -> PollWake {
    use std::mem::zeroed;
    let mut fds = unsafe { [zeroed::<libc::pollfd>(), zeroed::<libc::pollfd>()] };
    let mut nfds = 0usize;
    if let Some(fd) = hidraw_fd {
        fds[nfds].fd = fd;
        fds[nfds].events = libc::POLLIN | libc::POLLERR | libc::POLLHUP;
        nfds += 1;
    }
    let control_idx = nfds;
    fds[control_idx].fd = control_fd;
    fds[control_idx].events = libc::POLLIN | libc::POLLERR | libc::POLLHUP;
    nfds += 1;
    let ret = unsafe { poll_fn(fds.as_mut_ptr(), nfds as libc::nfds_t, timeout_ms) };
    if ret < 0 {
        let e = std::io::Error::last_os_error();
        if e.kind() == std::io::ErrorKind::Interrupted {
            // Signal interrupted the wait: re-evaluate stop/fd/deadline
            // promptly instead of restarting the full timeout.
            return PollWake::Timeout;
        }
        // Persistent wait failure: report distinctly so the caller backs off
        // even when idle/control-only. EINTR stays a responsive Timeout.
        log::warn!("pump: poll(2) failed, backing off: {e:#}");
        return PollWake::WaitError;
    }
    if ret == 0 {
        return PollWake::Timeout;
    }
    let hidraw_ready = hidraw_fd.is_some_and(|_| fds[0].revents != 0);
    let control_ready = fds[control_idx].revents != 0;
    match (hidraw_ready, control_ready) {
        (true, true) => PollWake::Both,
        (true, false) => PollWake::Hidraw,
        (false, true) => PollWake::Control,
        (false, false) => PollWake::Timeout,
    }
}

fn deadline_timeout_ms(deadline: Option<Instant>) -> i32 {
    match deadline {
        None => -1,
        Some(d) => {
            let now = Instant::now();
            if d <= now {
                0
            } else {
                let remaining = d - now;
                // Ceiling to whole milliseconds: `as_millis` truncates, so a
                // positive sub-millisecond remainder would become a 0 timeout
                // and busy-spin until the deadline. Overdue stays 0, absent
                // stays -1; future deadlines wait at least 1 ms and never fire
                // early, preserving the 5/30 ms floors and release ordering.
                let floor_ms = remaining.as_millis();
                let ceil_ms = if remaining.subsec_nanos().is_multiple_of(1_000_000) {
                    floor_ms
                } else {
                    floor_ms.saturating_add(1)
                };
                ceil_ms.min(i64::from(i32::MAX) as u128) as i32
            }
        }
    }
}

/// Bounded sleep for a failed `poll(2)` wait. Independent of source activity
/// (idle/control-only failures must not hot-loop) but deadline-aware so an
/// imminent deadline is not overshot by a full [`ERROR_BACKOFF`].
fn wait_error_backoff(deadline: Option<Instant>) -> Duration {
    match deadline {
        Some(d) => {
            let now = Instant::now();
            if d <= now {
                ERROR_BACKOFF
            } else {
                (d - now).min(ERROR_BACKOFF)
            }
        }
        None => ERROR_BACKOFF,
    }
}

/// Spawn a per-node pump thread draining `proxy` until `stop` flips.
/// Returns the join handle plus the control pipe: the manager must call
/// [`PumpControl::notify`] after every attach/detach (fd/deadline change)
/// and [`stop_pump`] on shutdown, or the thread can block in `poll(2)`
/// past the change.
///
/// Idle behavior: with no device (or gate-off/passthrough) both the hidraw
/// fd and the deadline are `None`, so the thread blocks indefinitely on
/// the control pipe — (near-)zero wakeups. Live idle (device attached, no
/// touch, no pending deadlines) blocks on the hidraw fd. Timing deadlines
/// (30 ms release hold, stagger gaps, cut stages) wake via the poll
/// timeout with no continuous polling. Frame rate stays device-bounded:
/// handled frames loop immediately with no sleep, so high-rate bursts
/// drain without the old unconditional 2 ms delay per poll.
pub fn spawn_pump<G, U, H, S>(
    name: String,
    proxy: Arc<Mutex<crate::proxy::ClickProxy<G, U, H, S>>>,
    cfg: crate::config::Config,
    stop: Arc<AtomicBool>,
) -> (std::thread::JoinHandle<()>, Arc<PumpControl>)
where
    G: crate::proxy::GrabDevice + Send + 'static,
    U: crate::proxy::UinputSink + Send + 'static,
    H: crate::proxy::HapticSender + Send + 'static,
    S: TouchSource + Send + 'static,
{
    spawn_pump_with_poll(name, proxy, cfg, stop, libc::poll)
}

/// Test-injectable pump loop: `poll_fn` replaces `libc::poll` so persistent
/// wait failures can be simulated without touching process/host rlimits.
/// Production always uses [`libc::poll`] via [`spawn_pump`].
pub(crate) fn spawn_pump_with_poll<G, U, H, S>(
    name: String,
    proxy: Arc<Mutex<crate::proxy::ClickProxy<G, U, H, S>>>,
    cfg: crate::config::Config,
    stop: Arc<AtomicBool>,
    poll_fn: RawPollFn,
) -> (std::thread::JoinHandle<()>, Arc<PumpControl>)
where
    G: crate::proxy::GrabDevice + Send + 'static,
    U: crate::proxy::UinputSink + Send + 'static,
    H: crate::proxy::HapticSender + Send + 'static,
    S: TouchSource + Send + 'static,
{
    let control = Arc::new(PumpControl::new().expect("pump control pipe"));
    let thread_control = Arc::clone(&control);
    let handle = std::thread::spawn(move || {
        log::info!("pump {name}: thread started");
        while !stop.load(Ordering::SeqCst) {
            // Snapshot fd + queued frame + deadline without holding the
            // proxy lock across the blocking wait.
            let (hidraw_fd, has_pending, deadline) = {
                let px = proxy.lock().unwrap_or_else(|e| e.into_inner());
                (px.poll_fd(), px.has_pending_frame(), px.next_deadline())
            };
            if has_pending {
                // Queued double-report half: drain without blocking. The
                // helper sleeps once on transient reads; frames drain
                // immediately with no throttle.
                pump_once_with_backoff(&proxy, &cfg);
                continue;
            }
            if hidraw_fd.is_none() {
                // No readiness fd (idle source, passthrough/gate-off, or a
                // scripted test backend): try one nonblocking step so
                // queued in-memory frames still drain, then block until
                // control/deadline. Only traffic (`Frame`) loops
                // immediately; deadline work re-arms via `poll(2)` so a
                // not-yet-due stagger never busy-spins. Steady idle does
                // one trial per wakeup, not a continuous poll. The helper
                // already slept on `Transient`, so that arm just continues.
                match pump_once_with_backoff(&proxy, &cfg) {
                    PumpActivity::Frame | PumpActivity::Transient => continue,
                    PumpActivity::Idle | PumpActivity::Deadline | PumpActivity::Disconnect => {}
                }
            }
            match poll_wait_with(
                hidraw_fd,
                thread_control.read_fd(),
                deadline_timeout_ms(deadline),
                poll_fn,
            ) {
                PollWake::Control => {
                    thread_control.drain();
                }
                PollWake::Both => {
                    // Drain control first so a concurrent attach/detach
                    // snapshot refreshes next loop; then drain input.
                    thread_control.drain();
                    pump_once_with_backoff(&proxy, &cfg);
                }
                PollWake::Hidraw => {
                    pump_once_with_backoff(&proxy, &cfg);
                }
                PollWake::Timeout => {
                    // Deadline expired: advance deadlines. With no fd and no
                    // deadline this is unreachable (infinite wait); a
                    // spurious/EINTR wake re-evaluates.
                    if deadline.is_some() || hidraw_fd.is_some() {
                        pump_once_with_backoff(&proxy, &cfg);
                    }
                }
                PollWake::WaitError => {
                    // Persistent wait failure (EINVAL/ENOMEM/...): bound the
                    // retry even when idle/control-only. Drain any control
                    // notify that arrived before the failure, make best-effort
                    // deadline progress, then sleep deadline-aware so an
                    // imminent deadline is not overshot by a full backoff.
                    thread_control.drain();
                    if stop.load(Ordering::SeqCst) {
                        continue;
                    }
                    let overdue = deadline.is_some_and(|d| d <= Instant::now());
                    if overdue || hidraw_fd.is_some() {
                        let _ = pump_once(&proxy, &cfg);
                    }
                    if stop.load(Ordering::SeqCst) {
                        continue;
                    }
                    let backoff = wait_error_backoff(deadline);
                    if backoff.is_zero() {
                        std::thread::sleep(Duration::from_millis(1));
                    } else {
                        std::thread::sleep(backoff);
                    }
                }
            }
        }
        log::info!("pump {name}: thread stopped");
    });
    (handle, control)
}

/// Outcome of a bounded thread join ([`join_bounded`]).
#[derive(Debug)]
pub enum WorkerJoin<T> {
    /// The thread finished; carries its return value.
    Completed(T),
    /// The thread panicked; carries a best-effort payload description.
    Panicked(String),
    /// The thread did not finish within the bound. The handle is
    /// detached (dropped): bounded shutdown must never hang the process
    /// on a wedged worker.
    TimedOut,
}

/// Join `handle`, waiting at most `timeout`. A finished thread is joined
/// (surfacing panics as [`WorkerJoin::Panicked`]); an unfinished one is
/// detached after the bound expires. Polls every 2 ms so shutdown
/// latency stays near the bound without busy-spinning.
pub fn join_bounded<T>(handle: std::thread::JoinHandle<T>, timeout: Duration) -> WorkerJoin<T> {
    let deadline = std::time::Instant::now() + timeout;
    let mut pending = Some(handle);
    loop {
        if pending.as_ref().is_some_and(|h| h.is_finished()) {
            return match pending.take().expect("just checked Some").join() {
                Ok(value) => WorkerJoin::Completed(value),
                Err(payload) => WorkerJoin::Panicked(panic_message(&payload)),
            };
        }
        if std::time::Instant::now() >= deadline {
            // Drop detaches: the wedged thread keeps running without its
            // handle. Process exit reclaims it shortly after.
            drop(pending.take());
            return WorkerJoin::TimedOut;
        }
        std::thread::sleep(std::time::Duration::from_millis(2));
    }
}

/// Best-effort panic payload description for worker-failure reporting.
/// String payloads round-trip; anything else reports a fixed marker.
pub fn panic_message(payload: &Box<dyn std::any::Any + Send + 'static>) -> String {
    if let Some(s) = payload.downcast_ref::<&str>() {
        (*s).to_string()
    } else if let Some(s) = payload.downcast_ref::<String>() {
        s.clone()
    } else {
        "non-string panic payload".to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn usb_report(clicks: u8, blocks: &[&[u8; 9]]) -> Vec<u8> {
        let mut r = vec![0x02, clicks, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0];
        for b in blocks {
            r.extend_from_slice(&**b);
        }
        r
    }

    fn contact(id: u8, pressure: u8) -> [u8; 9] {
        [0, 0, 0, 0x80, 10, 6, 8, pressure, id & 0x0f]
    }

    fn bt_report(clicks: u8, blocks: &[&[u8; 9]]) -> Vec<u8> {
        let mut r = vec![0x31, clicks, 0, 0];
        for b in blocks {
            r.extend_from_slice(&**b);
        }
        r
    }

    fn double_report(sub1: Vec<u8>, sub2: Vec<u8>) -> Vec<u8> {
        let mut dbl = vec![0xF7, sub1.len() as u8];
        dbl.extend_from_slice(&sub1);
        dbl.extend_from_slice(&sub2);
        dbl
    }

    #[test]
    fn tier_contract_matches_parser() {
        let src = HidrawSource::new("/dev/hidrawX", ScriptReader::default());
        assert_eq!(src.tier(), crate::apple_parse::CapabilityTier::L2p);
        assert_eq!(
            src.pressure_units(),
            crate::apple_parse::PressureUnits::Arbitrary
        );
    }

    #[test]
    fn scripted_usb_bytes_decode() {
        let c = contact(3, 100);
        let r = usb_report(0, &[&c]);
        let mut src = HidrawSource::new("/dev/hidraw1", ScriptReader::bytes(vec![r]));
        let f = src.poll_frame().unwrap().unwrap();
        assert_eq!(f.fingers.len(), 1);
        assert_eq!(f.fingers[0].id, 3);
        assert_eq!(f.fingers[0].pressure, 100);
    }

    #[test]
    fn malformed_dropped_never_aborts() {
        // Each malformed shape is isolated so transient-read retries
        // cannot coalesce script steps: one poll per case, then a valid
        // frame must still decode.
        for bad in [
            vec![0x02, 0, 0], // transport-short (< 12 B USB prefix)
            {
                let mut r = vec![0x02, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0];
                r.extend_from_slice(&[1, 2, 3, 4, 5, 6, 7, 8]); // trailing 8
                r
            },
        ] {
            let mut src = HidrawSource::new(
                "/dev/hidraw1",
                ScriptReader::new(vec![
                    ScriptStep::Bytes(bad),
                    // Pad with idle so a transport-short retry (which
                    // re-reads) lands on EAGAIN instead of the valid
                    // frame when retries are exhausted.
                    ScriptStep::WouldBlock,
                    ScriptStep::WouldBlock,
                    ScriptStep::WouldBlock,
                    ScriptStep::Bytes(usb_report(0, &[&contact(7, 50)])),
                ]),
            );
            assert!(src.poll_frame().unwrap().is_none());
            let f = src.poll_frame().unwrap().unwrap();
            assert_eq!(f.fingers[0].id, 7);
        }
    }

    #[test]
    fn malformed_counts_transport_short_vs_parse_error() {
        // Transport-short: USB id but fewer than 12 prefix bytes.
        let mut short = HidrawSource::new(
            "/dev/hidraw1",
            ScriptReader::new(vec![
                ScriptStep::Bytes(vec![0x02, 0, 0, 0, 0]),
                ScriptStep::WouldBlock,
                ScriptStep::WouldBlock,
                ScriptStep::WouldBlock,
            ]),
        );
        assert!(short.poll_frame().unwrap().is_none());
        assert!(short.transport_shorts() >= 1);
        assert_eq!(short.malformed_frames(), 0);
        // Parse error: full prefix but body not a multiple of 9.
        let mut malformed = HidrawSource::new(
            "/dev/hidraw1",
            ScriptReader::new(vec![ScriptStep::Bytes({
                let mut r = vec![0x02, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0];
                r.extend_from_slice(&[1, 2, 3, 4, 5, 6, 7, 8]);
                r
            })]),
        );
        assert!(malformed.poll_frame().unwrap().is_none());
        assert_eq!(malformed.malformed_frames(), 1);
    }

    #[test]
    fn double_report_queues_second() {
        let sub1 = {
            let mut r = vec![0x31, 0x01, 0, 0];
            r.extend_from_slice(&contact(1, 11));
            r
        };
        let sub2 = {
            let mut r = vec![0x31, 0x00, 0, 0];
            r.extend_from_slice(&contact(2, 22));
            r
        };
        let mut dbl = vec![0xF7, sub1.len() as u8];
        dbl.extend_from_slice(&sub1);
        dbl.extend_from_slice(&sub2);
        let mut src = HidrawSource::new("/dev/hidraw9", ScriptReader::bytes(vec![dbl]));
        let a = src.poll_frame().unwrap().unwrap();
        assert_eq!(a.fingers[0].pressure, 11);
        // Second frame served without another read.
        let b = src.poll_frame().unwrap().unwrap();
        assert_eq!(b.fingers[0].pressure, 22);
    }

    #[test]
    fn wouldblock_is_idle_not_error() {
        let mut src = HidrawSource::new(
            "/dev/hidraw1",
            ScriptReader::new(vec![ScriptStep::WouldBlock]),
        );
        assert!(src.poll_frame().unwrap().is_none());
    }

    #[test]
    fn transient_zero_byte_recovers_mid_click_no_teardown() {
        // BT hiccup mid-click: down, transient 0-byte, up. The 0-byte
        // must be retried (debug, backoff) and the up frame returned,
        // never a Disconnect (which would tear down the session and
        // triple-click via hotplug re-press).
        let down = bt_report(0, &[&contact(1, 120)]);
        let up = bt_report(0, &[]);
        let mut src = HidrawSource::new(
            "/dev/hidrawBT",
            ScriptReader::new(vec![
                ScriptStep::Bytes(down),
                ScriptStep::Bytes(Vec::new()),
                ScriptStep::Bytes(up),
            ]),
        );
        let a = src.poll_frame().unwrap().unwrap();
        assert_eq!(a.fingers.len(), 1);
        let b = src.poll_frame().unwrap().unwrap();
        assert!(b.fingers.is_empty());
        assert!(src.transient_retries() >= 1);
    }

    #[test]
    fn sustained_zero_bytes_signal_disconnect() {
        // More consecutive 0-byte reads than the retry budget: the node
        // is really gone, so signal Disconnect for proxy teardown.
        let zeros = (0..TRANSIENT_READ_RETRIES + 2)
            .map(|_| ScriptStep::Bytes(Vec::new()))
            .collect();
        let mut src = HidrawSource::new("/dev/hidraw1", ScriptReader::new(zeros));
        let err = src.poll_frame().unwrap_err();
        assert!(is_disconnect(&err));
    }

    #[test]
    fn sustained_enodev_is_disconnect_enoent_too() {
        for errno in [libc::ENODEV, libc::ENOENT] {
            let mut gone = HidrawSource::new(
                "/dev/hidraw1",
                ScriptReader::new(vec![ScriptStep::IoError(errno)]),
            );
            assert!(is_disconnect(&gone.poll_frame().unwrap_err()));
        }
    }

    #[test]
    fn eio_is_transient_not_disconnect() {
        let mut flaky = HidrawSource::new(
            "/dev/hidraw1",
            ScriptReader::new(vec![ScriptStep::IoError(libc::EIO)]),
        );
        assert!(!is_disconnect(&flaky.poll_frame().unwrap_err()));
    }

    #[test]
    fn eintr_retried_then_idle_or_recovers() {
        // EINTR then idle: tolerated, no error, no teardown.
        let mut idle = HidrawSource::new(
            "/dev/hidraw1",
            ScriptReader::new(vec![
                ScriptStep::IoError(libc::EINTR),
                ScriptStep::WouldBlock,
            ]),
        );
        assert!(idle.poll_frame().unwrap().is_none());
        // EINTR then a valid frame: the frame is returned.
        let mut recovers = HidrawSource::new(
            "/dev/hidraw1",
            ScriptReader::new(vec![
                ScriptStep::IoError(libc::EINTR),
                ScriptStep::Bytes(usb_report(0, &[&contact(4, 60)])),
            ]),
        );
        let f = recovers.poll_frame().unwrap().unwrap();
        assert_eq!(f.fingers[0].id, 4);
    }

    #[test]
    fn transport_short_retried_then_recovers() {
        // 2-byte fragment then a valid frame: retry lands on the frame.
        let mut src = HidrawSource::new(
            "/dev/hidraw1",
            ScriptReader::new(vec![
                ScriptStep::Bytes(vec![0x31, 0x00]),
                ScriptStep::Bytes(bt_report(0, &[&contact(9, 77)])),
            ]),
        );
        let f = src.poll_frame().unwrap().unwrap();
        assert_eq!(f.fingers[0].id, 9);
        assert!(src.transport_shorts() >= 1);
    }

    #[test]
    fn malformed_frame_preserves_pending_half() {
        // Valid 0xF7 queues its second half; a later malformed frame
        // must not lose it: pending drains fd-free first.
        let sub1 = {
            let mut r = vec![0x31, 0x01, 0, 0];
            r.extend_from_slice(&contact(1, 11));
            r
        };
        let sub2 = {
            let mut r = vec![0x31, 0x00, 0, 0];
            r.extend_from_slice(&contact(2, 22));
            r
        };
        let dbl = double_report(sub1, sub2);
        let bad = {
            let mut r = vec![0x02, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0];
            r.extend_from_slice(&[9u8; 8]);
            r
        };
        let after = usb_report(0, &[&contact(7, 50)]);
        let mut src = HidrawSource::new(
            "/dev/hidraw9",
            ScriptReader::new(vec![
                ScriptStep::Bytes(dbl),
                ScriptStep::Bytes(bad),
                ScriptStep::Bytes(after),
            ]),
        );
        let a = src.poll_frame().unwrap().unwrap();
        assert_eq!(a.fingers[0].pressure, 11);
        assert_eq!(src.pending_len(), 1);
        // Second half drains without touching the fd, even though the
        // next queued read is malformed.
        let b = src.poll_frame().unwrap().unwrap();
        assert_eq!(b.fingers[0].pressure, 22);
        assert_eq!(src.pending_len(), 0);
        // Now the malformed frame drops but counts, pending stays empty.
        assert!(src.poll_frame().unwrap().is_none());
        assert_eq!(src.malformed_frames(), 1);
        assert_eq!(src.pending_len(), 0);
        // Stream recovers on the next valid frame.
        let c = src.poll_frame().unwrap().unwrap();
        assert_eq!(c.fingers[0].id, 7);
    }

    #[test]
    fn malformed_double_does_not_queue_or_stall() {
        // Malformed 0xF7 (overshoot length) drops, queues nothing, and
        // the next valid frame still decodes.
        let mut overshoot = vec![0xF7, 255u8];
        overshoot.extend_from_slice(&bt_report(0, &[&contact(1, 10)]));
        let mut src = HidrawSource::new(
            "/dev/hidraw9",
            ScriptReader::new(vec![
                ScriptStep::Bytes(overshoot),
                ScriptStep::Bytes(usb_report(0, &[&contact(5, 90)])),
            ]),
        );
        assert!(src.poll_frame().unwrap().is_none());
        assert_eq!(src.pending_len(), 0);
        let f = src.poll_frame().unwrap().unwrap();
        assert_eq!(f.fingers[0].id, 5);
    }

    #[test]
    fn pump_source_idle_is_noop() {
        let mut src = PumpSource::Idle;
        assert_eq!(src.tier(), crate::apple_parse::CapabilityTier::L2p);
        assert!(src.poll_frame().unwrap().is_none());
        assert!(src.poll_fd().is_none());
        assert!(!src.has_pending_frame());
    }

    #[test]
    fn double_report_second_half_has_pending_no_fd() {
        // After the first half, the second half is queued: the pump must
        // drain it immediately (has_pending) rather than blocking on the fd.
        let sub1 = {
            let mut r = vec![0x31, 0x01, 0, 0];
            r.extend_from_slice(&contact(1, 11));
            r
        };
        let sub2 = {
            let mut r = vec![0x31, 0x00, 0, 0];
            r.extend_from_slice(&contact(2, 22));
            r
        };
        let mut dbl = vec![0xF7, sub1.len() as u8];
        dbl.extend_from_slice(&sub1);
        dbl.extend_from_slice(&sub2);
        let mut src = HidrawSource::new("/dev/hidraw9", ScriptReader::bytes(vec![dbl]));
        assert!(!src.has_pending_frame());
        let _ = src.poll_frame().unwrap().unwrap();
        assert!(src.has_pending_frame());
        assert!(src.poll_fd().is_none());
    }

    #[test]
    fn control_notify_wakes_blocked_poll() {
        // A control-only poll (no hidraw fd, no deadline) blocks until
        // notified: the event-driven idle path for no-device operation.
        let control = PumpControl::new().unwrap();
        let start = Instant::now();
        std::thread::scope(|s| {
            s.spawn(|| {
                std::thread::sleep(Duration::from_millis(50));
                control.notify();
            });
            let wake = poll_wait_with(None, control.read_fd(), -1, libc::poll);
            assert_eq!(wake, PollWake::Control);
        });
        assert!(start.elapsed() >= Duration::from_millis(40));
        control.drain();
        // After drain, a 0-timeout poll with nothing ready times out.
        assert_eq!(
            poll_wait_with(None, control.read_fd(), 0, libc::poll),
            PollWake::Timeout
        );
    }

    #[test]
    fn control_pipe_is_nonblocking_cloexec_and_coalesces() {
        use std::os::unix::io::AsRawFd;
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<PumpControl>();
        let control = PumpControl::new().unwrap();
        for fd in [control.read.as_raw_fd(), control.write.as_raw_fd()] {
            let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
            assert!(flags >= 0 && flags & libc::O_NONBLOCK != 0);
            let fd_flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
            assert!(fd_flags >= 0 && fd_flags & libc::FD_CLOEXEC != 0);
        }
        // Full-pipe coalescing: repeated notifies must never block, even when
        // the pipe fills; drain then restores Timeout.
        let start = Instant::now();
        for _ in 0..100_000 {
            control.notify();
        }
        assert!(
            start.elapsed() < Duration::from_secs(5),
            "notify must not block on a full pipe"
        );
        control.drain();
        assert_eq!(
            poll_wait_with(None, control.read_fd(), 0, libc::poll),
            PollWake::Timeout
        );
        // A fresh notify still wakes after the drain (lifetime intact).
        control.notify();
        assert_eq!(
            poll_wait_with(None, control.read_fd(), 0, libc::poll),
            PollWake::Control
        );
        control.drain();
    }

    #[test]
    fn deadline_timeout_is_capped_and_immediate_when_overdue() {
        assert_eq!(deadline_timeout_ms(None), -1);
        let past = Instant::now() - Duration::from_millis(5);
        assert_eq!(deadline_timeout_ms(Some(past)), 0);
        let future = Instant::now() + Duration::from_millis(30);
        let ms = deadline_timeout_ms(Some(future));
        assert!((1..=30).contains(&ms), "deadline ms: {ms}");
    }

    #[test]
    fn deadline_timeout_ceils_submillisecond_without_early_fire() {
        // Root cause of the pre-deadline spin: `as_millis` truncates a
        // positive sub-ms remainder to a 0 timeout, so `poll` returns
        // immediately until the clock reaches the deadline. Ceiling keeps a
        // strictly-future deadline at >=1 ms and never fires early.
        let deadline = Instant::now() + Duration::from_micros(800);
        let timeout = deadline_timeout_ms(Some(deadline));
        if deadline > Instant::now() {
            assert_eq!(
                timeout, 1,
                "sub-ms future must ceil to 1 ms, not truncate to 0"
            );
        } else {
            // Slept past an 800 us deadline between the two `now()`s
            // (pathological deschedule); overdue correctly reports 0.
            assert_eq!(timeout, 0);
        }
        // Exact-ms deadlines stay exact; overdue stays 0; absent stays -1.
        let exact = Instant::now() + Duration::from_millis(50);
        let exact_ms = deadline_timeout_ms(Some(exact));
        assert!(
            (49..=50).contains(&exact_ms),
            "50 ms deadline must not fire early or spin: {exact_ms}"
        );
        assert_eq!(deadline_timeout_ms(Some(Instant::now())), 0);
        assert_eq!(deadline_timeout_ms(None), -1);
    }

    #[test]
    fn poll_wait_maps_errors_distinctly_with_mocked_poll() {
        // Safe mocked poll: no rlimit/host changes. EINTR stays a responsive
        // Timeout; persistent failures become WaitError (never Timeout, or
        // the idle loop would hot-spin).
        unsafe extern "C" fn mock_eintr(
            _fds: *mut libc::pollfd,
            _nfds: libc::nfds_t,
            _timeout: i32,
        ) -> i32 {
            *libc::__errno_location() = libc::EINTR;
            -1
        }
        unsafe extern "C" fn mock_einval(
            _fds: *mut libc::pollfd,
            _nfds: libc::nfds_t,
            _timeout: i32,
        ) -> i32 {
            *libc::__errno_location() = libc::EINVAL;
            -1
        }
        unsafe extern "C" fn mock_enomem(
            _fds: *mut libc::pollfd,
            _nfds: libc::nfds_t,
            _timeout: i32,
        ) -> i32 {
            *libc::__errno_location() = libc::ENOMEM;
            -1
        }
        let control = PumpControl::new().unwrap();
        assert_eq!(
            poll_wait_with(None, control.read_fd(), -1, mock_eintr),
            PollWake::Timeout
        );
        assert_eq!(
            poll_wait_with(None, control.read_fd(), -1, mock_einval),
            PollWake::WaitError
        );
        assert_eq!(
            poll_wait_with(None, control.read_fd(), -1, mock_enomem),
            PollWake::WaitError
        );
        // Control-only failure is also distinct (idle must back off, not spin).
        assert_eq!(
            poll_wait_with(None, control.read_fd(), 0, mock_einval),
            PollWake::WaitError
        );
    }

    #[test]
    fn wait_error_backoff_is_bounded_and_deadline_aware() {
        assert_eq!(wait_error_backoff(None), ERROR_BACKOFF);
        assert_eq!(
            wait_error_backoff(Some(Instant::now() - Duration::from_millis(1))),
            ERROR_BACKOFF
        );
        // Far deadline caps at the 10 ms backoff, never overshoots.
        let far = Instant::now() + Duration::from_secs(60);
        assert_eq!(wait_error_backoff(Some(far)), ERROR_BACKOFF);
        // Imminent deadline sleeps only until the deadline.
        let near = Instant::now() + Duration::from_millis(3);
        let backoff = wait_error_backoff(Some(near));
        assert!(
            backoff <= Duration::from_millis(3) && !backoff.is_zero(),
            "imminent deadline must bound sleep, got {backoff:?}"
        );
    }

    static FAILING_POLL_CALLS: std::sync::atomic::AtomicUsize =
        std::sync::atomic::AtomicUsize::new(0);

    unsafe extern "C" fn mock_poll_always_fail(
        _fds: *mut libc::pollfd,
        _nfds: libc::nfds_t,
        _timeout: i32,
    ) -> i32 {
        FAILING_POLL_CALLS.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        *libc::__errno_location() = libc::EINVAL;
        -1
    }

    #[test]
    fn failing_poll_backs_off_when_idle_and_stops_promptly() {
        // Idle/control-only session (no fd, no deadline) with a persistently
        // failing wait: must back off at ~10 ms intervals, not hot-spin, and
        // `stop_pump` during the backoff must exit promptly. Uses injected
        // mock poll, never touching process/host rlimits.
        use crate::proxy::{ClickProxy, ProxySettings};
        FAILING_POLL_CALLS.store(0, std::sync::atomic::Ordering::SeqCst);
        let count = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let proxy = Arc::new(Mutex::new(ClickProxy::new(
            ProxySettings { enhanced: true },
            PumpTestGrab,
            PumpTestSink,
            PumpTestHaptic,
            CountingSource {
                polls: Arc::new(AtomicBool::new(false)),
                count: Arc::clone(&count),
            },
        )));
        {
            let mut px = proxy.lock().unwrap();
            px.set_pump_ready(true);
            px.attach(&crate::init::usb_ready("/dev/hidraw1"));
            assert!(px.is_attached());
            assert!(px.poll_fd().is_none());
            assert!(px.next_deadline().is_none());
        }
        let cfg = crate::config::Config {
            enhanced_clicks: true,
            ..crate::config::Config::default()
        };
        let stop = Arc::new(AtomicBool::new(false));
        let start = Instant::now();
        let (handle, control) = spawn_pump_with_poll(
            "failing-idle-test".to_string(),
            Arc::clone(&proxy),
            cfg,
            Arc::clone(&stop),
            mock_poll_always_fail,
        );
        // Control notify during backoff must not deadlock or get lost.
        control.notify();
        std::thread::sleep(Duration::from_millis(120));
        let calls = FAILING_POLL_CALLS.load(std::sync::atomic::Ordering::SeqCst);
        // 120 ms at 10 ms backoff ≈ 12 waits; spinning would be thousands.
        assert!(
            calls <= 30,
            "persistently failing idle wait spun: {calls} polls in 120 ms"
        );
        assert!(
            calls >= 2,
            "failing wait should still retry with backoff, got {calls}"
        );
        stop_pump(&stop, &control);
        handle.join().unwrap();
        let elapsed = start.elapsed();
        assert!(
            elapsed < Duration::from_secs(2),
            "stop during backoff took too long: {elapsed:?}"
        );
    }

    // Minimal proxy doubles for pump-thread tests (no hardware).
    struct PumpTestGrab;
    struct PumpTestSink;
    struct PumpTestHaptic;
    struct CountingSource {
        polls: Arc<AtomicBool>,
        count: Arc<std::sync::atomic::AtomicUsize>,
    }

    impl crate::proxy::GrabDevice for PumpTestGrab {
        fn grab(&self) -> Result<()> {
            Ok(())
        }
        fn ungrab(&self) {}
        fn is_grabbed(&self) -> bool {
            false
        }
    }

    impl crate::proxy::UinputSink for PumpTestSink {
        fn emit(&self, _: u16, _: u16, _: i32) -> Result<()> {
            Ok(())
        }
    }

    impl crate::proxy::HapticSender for PumpTestHaptic {
        fn click_feedback(&self, _: u8, _: u8, _: bool) -> Result<()> {
            Ok(())
        }
        fn release_feedback(&self, _: u8, _: bool) -> Result<()> {
            Ok(())
        }
    }

    /// Nonblocking pipe: read end as a `File`, write end as a raw fd.
    fn pipe_nonblocking() -> (std::fs::File, RawFd) {
        use std::os::unix::io::FromRawFd;
        let mut fds = [0 as RawFd; 2];
        assert_eq!(unsafe { libc::pipe(fds.as_mut_ptr()) }, 0);
        let file = unsafe { std::fs::File::from_raw_fd(fds[0]) };
        set_nonblocking(&file).unwrap();
        (file, fds[1])
    }

    fn write_all(fd: RawFd, mut bytes: &[u8]) {
        while !bytes.is_empty() {
            let ret = unsafe { libc::write(fd, bytes.as_ptr().cast(), bytes.len()) };
            assert!(ret > 0, "pipe write failed");
            bytes = &bytes[ret as usize..];
        }
    }

    fn drain_fd(fd: RawFd) {
        let mut buf = [0u8; 64];
        loop {
            let ret = unsafe { libc::read(fd, buf.as_mut_ptr().cast(), buf.len()) };
            if ret <= 0 {
                break;
            }
        }
    }

    struct CountingSink {
        count: Arc<std::sync::atomic::AtomicUsize>,
    }

    impl crate::proxy::UinputSink for CountingSink {
        fn emit(&self, _: u16, _: u16, _: i32) -> Result<()> {
            self.count.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(())
        }
        fn is_ready(&self) -> bool {
            true
        }
    }

    #[test]
    fn poll_wait_classifies_hidraw_control_and_both() {
        // Deterministic wake classification (no thread): data-only reads as
        // Hidraw, notify-only as Control, both-at-once as Both.
        let (rfile, wfd) = pipe_nonblocking();
        use std::os::unix::io::AsRawFd;
        let rfd = rfile.as_raw_fd();
        let control = PumpControl::new().unwrap();
        assert_eq!(
            poll_wait_with(Some(rfd), control.read_fd(), 0, libc::poll),
            PollWake::Timeout,
            "idle fds must time out"
        );
        write_all(wfd, &[0xAA]);
        assert_eq!(
            poll_wait_with(Some(rfd), control.read_fd(), 0, libc::poll),
            PollWake::Hidraw
        );
        control.notify();
        assert_eq!(
            poll_wait_with(Some(rfd), control.read_fd(), 0, libc::poll),
            PollWake::Both
        );
        drain_fd(rfd);
        control.drain();
        control.notify();
        assert_eq!(
            poll_wait_with(Some(rfd), control.read_fd(), 0, libc::poll),
            PollWake::Control
        );
        control.drain();
        unsafe { libc::close(wfd) };
    }

    fn wait_until(timeout: Duration, mut cond: impl FnMut() -> bool) {
        let give_up = Instant::now() + timeout;
        while !cond() {
            assert!(
                Instant::now() < give_up,
                "condition never became true within {timeout:?}"
            );
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    #[test]
    fn spawn_pump_with_live_fd_drains_hidraw_wake_without_notify() {
        // End-to-end through `spawn_pump` with a real readiness fd: a report
        // written to the hidraw pipe must be consumed via the Hidraw wake
        // with no control notify and no armed deadline. A second report
        // paired with a notify must drain too (Both wake).
        use crate::proxy::{ClickProxy, ProxySettings};
        let (rfile, wfd) = pipe_nonblocking();
        let count = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let proxy = Arc::new(Mutex::new(ClickProxy::new(
            ProxySettings { enhanced: true },
            PumpTestGrab,
            CountingSink {
                count: Arc::clone(&count),
            },
            PumpTestHaptic,
            HidrawSource::new("/dev/hidraw-test", FileReader { file: rfile }),
        )));
        {
            let mut px = proxy.lock().unwrap();
            px.set_pump_ready(true);
            px.attach(&crate::init::usb_ready("/dev/hidraw-test"));
            assert!(px.is_attached());
            assert!(px.poll_fd().is_some(), "live fd must be polled");
            assert!(px.next_deadline().is_none());
        }
        let cfg = crate::config::Config {
            enhanced_clicks: true,
            ..crate::config::Config::default()
        };
        let stop = Arc::new(AtomicBool::new(false));
        let (handle, control) = spawn_pump(
            "live-fd-test".to_string(),
            Arc::clone(&proxy),
            cfg,
            Arc::clone(&stop),
        );
        // Phase 1 (Hidraw): data only, no notify — must still drain.
        write_all(wfd, &usb_report(0, &[&contact(1, 200)]));
        wait_until(Duration::from_secs(3), || {
            count.load(std::sync::atomic::Ordering::SeqCst) > 0
        });
        let phase1 = count.load(std::sync::atomic::Ordering::SeqCst);
        // Phase 2 (Both): data plus a control notify drains as well.
        write_all(wfd, &usb_report(0, &[&contact(2, 200)]));
        control.notify();
        wait_until(Duration::from_secs(3), || {
            count.load(std::sync::atomic::Ordering::SeqCst) > phase1
        });
        stop_pump(&stop, &control);
        handle.join().unwrap();
        unsafe { libc::close(wfd) };
    }

    impl TouchSource for CountingSource {
        fn tier(&self) -> crate::apple_parse::CapabilityTier {
            crate::apple_parse::CapabilityTier::L2p
        }
        fn pressure_units(&self) -> crate::apple_parse::PressureUnits {
            crate::apple_parse::PressureUnits::Arbitrary
        }
        fn poll_frame(&mut self) -> Result<Option<TouchFrame>> {
            self.count.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let _ = self.polls.load(Ordering::SeqCst);
            Ok(None)
        }
    }

    #[test]
    fn idle_pump_thread_produces_near_zero_wakeups() {
        // Attached enhanced proxy with a silent-but-live source: every pump
        // step reaches source.poll_frame (no early short-circuit), so the
        // count genuinely measures pump wakeups. The pump must block on
        // control/deadline, not poll every 2 ms (~500 wakeups/s). Budget:
        // at most the single startup trial over 200 ms (old code: ~100).
        use crate::proxy::{ClickProxy, ProxySettings};
        let count = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let proxy = Arc::new(Mutex::new(ClickProxy::new(
            ProxySettings { enhanced: true },
            PumpTestGrab,
            PumpTestSink,
            PumpTestHaptic,
            CountingSource {
                polls: Arc::new(AtomicBool::new(false)),
                count: Arc::clone(&count),
            },
        )));
        {
            let mut px = proxy.lock().unwrap();
            px.set_pump_ready(true);
            px.attach(&crate::init::usb_ready("/dev/hidraw1"));
            assert!(px.is_attached());
            assert!(px.poll_fd().is_none());
            assert!(px.next_deadline().is_none());
        }
        let cfg = crate::config::Config {
            enhanced_clicks: true,
            ..crate::config::Config::default()
        };
        let stop = Arc::new(AtomicBool::new(false));
        let (handle, control) = spawn_pump(
            "idle-test".to_string(),
            Arc::clone(&proxy),
            cfg,
            Arc::clone(&stop),
        );
        std::thread::sleep(Duration::from_millis(200));
        stop_pump(&stop, &control);
        handle.join().unwrap();
        let polls = count.load(std::sync::atomic::Ordering::SeqCst);
        assert!(
            polls <= 2,
            "idle pump polled {polls}x in 200 ms (old 2 ms poll: ~100x)"
        );
    }

    #[test]
    fn pump_once_reports_frame_for_queued_script_bytes() {
        // High-rate path: queued reports each report Frame (traffic), never
        // Idle, with no per-frame sleep in the pump step itself.
        use crate::proxy::{ClickProxy, ProxySettings};
        let frames: Vec<TouchFrame> = (0..50)
            .map(|x| TouchFrame {
                fingers: vec![],
                clicks: (x % 2) as u8,
                timestamp_ms: 0,
            })
            .collect();
        struct QueueSource {
            queue: VecDeque<TouchFrame>,
        }
        impl TouchSource for QueueSource {
            fn tier(&self) -> crate::apple_parse::CapabilityTier {
                crate::apple_parse::CapabilityTier::L2p
            }
            fn pressure_units(&self) -> crate::apple_parse::PressureUnits {
                crate::apple_parse::PressureUnits::Arbitrary
            }
            fn poll_frame(&mut self) -> Result<Option<TouchFrame>> {
                Ok(self.queue.pop_front())
            }
        }
        struct ReadySink;
        impl crate::proxy::UinputSink for ReadySink {
            fn emit(&self, _: u16, _: u16, _: i32) -> Result<()> {
                Ok(())
            }
            fn is_ready(&self) -> bool {
                true
            }
        }
        let proxy = Arc::new(Mutex::new(ClickProxy::new(
            ProxySettings { enhanced: true },
            PumpTestGrab,
            ReadySink,
            PumpTestHaptic,
            QueueSource {
                queue: frames.into_iter().collect(),
            },
        )));
        {
            let mut px = proxy.lock().unwrap();
            px.set_pump_ready(true);
            px.attach(&crate::init::usb_ready("/dev/hidraw1"));
            assert!(px.is_attached());
        }
        let cfg = crate::config::Config::default();
        let start = Instant::now();
        for _ in 0..50 {
            assert_eq!(pump_once(&proxy, &cfg), PumpActivity::Frame);
        }
        let elapsed = start.elapsed();
        assert!(
            elapsed < Duration::from_millis(100),
            "50 frames took {elapsed:?} (old 2 ms sleep: ~100 ms)"
        );
        assert_eq!(pump_once(&proxy, &cfg), PumpActivity::Idle);
    }

    #[test]
    fn join_bounded_reports_completion() {
        let handle = std::thread::spawn(|| 42);
        match join_bounded(handle, Duration::from_secs(5)) {
            WorkerJoin::Completed(value) => assert_eq!(value, 42),
            WorkerJoin::Panicked(msg) => panic!("expected completion, got panic: {msg}"),
            WorkerJoin::TimedOut => panic!("healthy thread must join within the bound"),
        }
    }

    #[test]
    fn join_bounded_reports_panic_payload() {
        let handle = std::thread::spawn(|| panic!("boom-test-payload"));
        match join_bounded(handle, Duration::from_secs(5)) {
            WorkerJoin::Completed(()) => panic!("panicking thread must not report completion"),
            WorkerJoin::Panicked(msg) => assert!(
                msg.contains("boom-test-payload"),
                "payload must surface, got: {msg}"
            ),
            WorkerJoin::TimedOut => panic!("dead thread is finished; must not time out"),
        }
    }

    #[test]
    fn join_bounded_times_out_instead_of_hanging() {
        let (tx, rx) = std::sync::mpsc::channel::<()>();
        let handle = std::thread::spawn(move || {
            let _ = rx.recv();
        });
        let start = std::time::Instant::now();
        match join_bounded(handle, Duration::from_millis(50)) {
            WorkerJoin::Completed(()) => panic!("blocked thread must not complete"),
            WorkerJoin::Panicked(msg) => panic!("blocked thread must not panic: {msg}"),
            WorkerJoin::TimedOut => {}
        }
        assert!(
            start.elapsed() < Duration::from_secs(5),
            "bounded join must not hang: {:?}",
            start.elapsed()
        );
        drop(tx); // let the detached thread exit on its own
    }

    #[test]
    fn panic_message_round_trips_strings() {
        let from_str: Box<dyn std::any::Any + Send> = Box::new("static slice");
        assert_eq!(panic_message(&from_str), "static slice");
        let from_string: Box<dyn std::any::Any + Send> = Box::new("owned".to_string());
        assert_eq!(panic_message(&from_string), "owned");
        let other: Box<dyn std::any::Any + Send> = Box::new(42u32);
        assert_eq!(panic_message(&other), "non-string panic payload");
    }
}
