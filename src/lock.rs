//! Single-instance Unix socket lock.
//!
//! Path: `/run/linux-trackpad-driver/driver.sock` — deliberately distinct
//! from the legacy `magic-trackpad-driver` socket so both generations can
//! coexist during migration (no migration handling: different path, no clash).
//!
//! Protocol: `bind()` wins. On `AddrInUse`, probe with a bounded nonblocking connect: success or timeout
//! means a live daemon owns it (new instance exits); connection-refused means
//! a stale socket file (remove + rebind under a sidecar `*.lock` flock mutex,
//! then re-checking liveness once); any other dial failure (e.g. EACCES)
//! leaves the file untouched and bails. `Drop` unlinks only while holding a
//! non-blocking take of the same mutex and while the path still names the
//! filesystem entry our `bind` created (dev+ino captured at acquisition), so
//! a dying predecessor never removes a successor's rebound file. When the
//! mutex is contended or unusable, `Drop` skips cleanup and leaves reclaimable
//! residue for the next serialized acquire.
//!
//! The holder drains its listen queue on a background thread (accept +
//! close): liveness probes are connects, and an undrained queue would fill
//! (default backlog ~4096) and stall later probes. Probes additionally carry
//! a timeout so a full queue degrades to a fast live verdict, never a hang.
//! Tests use temp dirs, never `/run`.

use std::io;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::Path;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};

/// Production daemon socket. Kept as a constant so `reconnect -f` logic and
/// the daemon share one source of truth.
pub const DRIVER_SOCK: &str = "/run/linux-trackpad-driver/driver.sock";
/// Legacy predecessor path. Never bound here; listed so a future migration
/// task can detect the old daemon without colliding with it.
pub const LEGACY_SOCK: &str = "/run/magic-trackpad-driver/driver.sock";
/// Env var overriding the socket path (tests, unprivileged runs).
/// Production default is [`DRIVER_SOCK`].
pub const SOCKET_ENV: &str = "LTD_SOCKET";
/// Bound for connect-based liveness probes. A full listen queue must degrade
/// to a fast verdict, never block reconnect/resume/acquire indefinitely.
pub const LIVENESS_TIMEOUT: Duration = Duration::from_millis(500);
/// Per-call accept cap so a burst cannot spin the drainer forever.
const DRAIN_BATCH_LIMIT: usize = 4096;

/// Effective socket path: explicit override, else `$LTD_SOCKET`, else default.
/// Pure over the override so tests never mutate process env.
pub fn socket_path_with(override_path: Option<&str>) -> String {
    override_path
        .map(str::to_string)
        .or_else(|| std::env::var(SOCKET_ENV).ok())
        .unwrap_or_else(|| DRIVER_SOCK.to_string())
}

/// Effective socket path: `$LTD_SOCKET` when set, else [`DRIVER_SOCK`].
pub fn socket_path() -> String {
    socket_path_with(None)
}

/// Guard holding the bound socket; drops (and removes the file) on shutdown.
/// A background thread accepts and closes probe connections so the listen
/// queue cannot fill and stall later liveness checks.
pub struct SocketLock {
    path: String,
    /// Filesystem identity (dev, ino) of the socket directory entry created
    /// by our successful `bind`, captured at acquisition while holding the
    /// replacement mutex. `Drop` unlinks the path only while it still names
    /// this entry.
    sock_id: (u64, u64),
    _listener: UnixListener,
    /// Shutdown flag observed independently of `poll` success, plus the write
    /// end of the shutdown pipe. `Drop` sets the flag then writes one byte
    /// before joining: the flag keeps shutdown observable even if `poll`
    /// keeps failing, and the byte wakes an idle `poll` promptly. A full
    /// pipe still leaves the waiter readable, so no wakeup is lost.
    stop: Arc<AtomicBool>,
    shutdown_tx: std::os::fd::OwnedFd,
    drainer: Option<JoinHandle<()>>,
}

/// Short-lived cross-process mutex serializing the stale-check/remove/bind
/// sequence in [`SocketLock::acquire`]. Released (fd close drops the flock)
/// once acquisition succeeds or fails; it is not the daemon lock itself.
struct ReplacementMutex {
    _file: Option<std::fs::File>,
}

impl ReplacementMutex {
    /// Whether this instance actually holds the sidecar lock. Cleanup of a
    /// fresh pathname is only safe with exclusion; a degraded (mutex-less)
    /// acquire must leave residue for the next serialized acquire, since a
    /// successor may already own the name after an external unlink.
    fn is_held(&self) -> bool {
        self._file.is_some()
    }
}

/// Take `LOCK_EX` on `{socket_path}.lock`. Best-effort: when the lockfile is
/// unusable (read-only dir, odd permissions) acquisition proceeds without
/// mutual exclusion rather than refusing to start.
///
/// The lockfile is intentionally never unlinked: deleting it while a starter
/// blocks on its flock would let a newcomer create a fresh file and split
/// the mutex. It is an empty coordination file in the socket directory.
fn replacement_mutex(socket_path: &str) -> ReplacementMutex {
    let file = open_lockfile(socket_path).and_then(|f| flock(&f, true).then_some(f));
    if file.is_none() {
        log::debug!("proceeding without socket replacement mutex for '{socket_path}'");
    }
    ReplacementMutex { _file: file }
}

/// Take `LOCK_EX|LOCK_NB` on `{socket_path}.lock`. Returns `Some` only when
/// the mutex is actually held. Returns `None` when it is contended or
/// unusable. Used by `Drop`, which must never block shutdown on a starter
/// mid-acquire (whose own blocking dial can stall). A `None` result means the
/// caller holds no exclusion and must skip pathname cleanup, leaving any
/// residue for the next serialized acquire to reclaim. Contention and an
/// unusable lockfile are logged differently: contention is an expected
/// shutdown/acquire overlap, while an unusable lockfile is a degraded
/// best-effort mode where acquisition also proceeds without exclusion.
fn replacement_mutex_try(socket_path: &str) -> Option<ReplacementMutex> {
    let file = match open_lockfile(socket_path) {
        Some(f) => f,
        None => {
            // Already logged in `open_lockfile`: degraded mode, no exclusion.
            log::debug!("skipping socket cleanup for '{socket_path}': mutex unavailable");
            return None;
        }
    };
    if flock(&file, false) {
        Some(ReplacementMutex { _file: Some(file) })
    } else {
        log::debug!("skipping socket cleanup for '{socket_path}': mutex contended");
        None
    }
}

fn open_lockfile(socket_path: &str) -> Option<std::fs::File> {
    let lock_path = format!("{socket_path}.lock");
    match std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&lock_path)
    {
        Ok(f) => Some(f),
        Err(e) => {
            log::debug!("socket replacement mutex unavailable ({lock_path}): {e}");
            None
        }
    }
}

/// `LOCK_EX` on an open lockfile. Blocking when `blocking`, single
/// non-blocking attempt otherwise.
fn flock(file: &std::fs::File, blocking: bool) -> bool {
    use std::os::unix::io::AsRawFd;
    let op = if blocking {
        libc::LOCK_EX
    } else {
        libc::LOCK_EX | libc::LOCK_NB
    };
    loop {
        // SAFETY: `file` is a live open file; `flock` takes the raw fd and
        // no owned state.
        let rc = unsafe { libc::flock(file.as_raw_fd(), op) };
        if rc == 0 {
            return true;
        }
        let err = std::io::Error::last_os_error();
        if blocking && err.kind() == std::io::ErrorKind::Interrupted {
            continue;
        }
        if blocking {
            log::debug!("socket replacement mutex lock failed: {err}");
        }
        return false;
    }
}

/// Filesystem identity of the directory entry at `path`.
fn filesystem_id(path: &str) -> Result<(u64, u64)> {
    use std::os::unix::fs::MetadataExt;
    let m = std::fs::metadata(path).with_context(|| format!("stating bound socket '{path}'"))?;
    Ok((m.dev(), m.ino()))
}

/// Best-effort cleanup of our fresh pathname after a post-bind init failure.
/// Only unlinks when we hold the replacement mutex and the path still names
/// the entry our `bind` created (`sock_id`); otherwise leaves reclaimable
/// residue for the next serialized acquire. In particular a degraded
/// mutex-less acquire never unlinks, and a stale identity (successor rebound
/// after an external unlink) never unlinks the live name. Mirrors the
/// ownership policy of `Drop`; never stat/unlink as if holding exclusion.
fn cleanup_failed_acquire(path: &str, sock_id: (u64, u64), mutex_held: bool) {
    if !mutex_held {
        return;
    }
    if filesystem_id(path).ok() == Some(sock_id) {
        let _ = std::fs::remove_file(path);
    }
}

impl SocketLock {
    /// Acquire the lock at `path`. Errors if a live daemon holds it.
    pub fn acquire(path: &str) -> Result<Self> {
        if path == LEGACY_SOCK {
            anyhow::bail!("refusing legacy socket path '{path}'; use '{DRIVER_SOCK}'");
        }
        if let Some(parent) = Path::new(path).parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent)
                    .with_context(|| format!("creating socket dir for '{path}'"))?;
            }
        }
        // The stale-check/remove/bind sequence below runs under a sidecar
        // flock mutex so two starters that both observe a refused socket
        // cannot each unlink the other's fresh bind: the loser blocks on the
        // mutex, then re-binds (AddrInUse), re-dials the winner (live), and
        // exits. Without the mutex the loser could unlink the winner's new
        // pathname using its earlier refusal observation and bind a second
        // listener, leaving two self-declared owners.
        let replacement = replacement_mutex(path);
        // Up to two passes: bind, or stale-cleanup then re-bind with a fresh
        // liveness check (a parallel starter may have rebound between).
        for _ in 0..2 {
            match UnixListener::bind(path) {
                Ok(listener) => {
                    // Capture the filesystem entry our bind created so Drop can
                    // later tell "our file" from a successor's rebound entry.
                    // Without a captured identity later cleanup cannot prove
                    // ownership, so it leaves residue for the next starter.
                    let sock_id = match filesystem_id(path) {
                        Ok(id) => id,
                        Err(err) => {
                            drop(listener);
                            return Err(err);
                        }
                    };
                    if let Err(err) = listener.set_nonblocking(true) {
                        drop(listener);
                        cleanup_failed_acquire(path, sock_id, replacement.is_held());
                        return Err(err)
                            .with_context(|| format!("setting nonblocking on socket '{path}'"));
                    }
                    let stop = Arc::new(AtomicBool::new(false));
                    let (shutdown_tx, shutdown_rx) = match shutdown_pipe() {
                        Ok(pair) => pair,
                        Err(err) => {
                            drop(listener);
                            cleanup_failed_acquire(path, sock_id, replacement.is_held());
                            return Err(err).with_context(|| {
                                format!("creating drain shutdown pipe for '{path}'")
                            });
                        }
                    };
                    let drain_listener = match listener.try_clone() {
                        Ok(clone) => clone,
                        Err(err) => {
                            drop(listener);
                            drop(shutdown_tx);
                            drop(shutdown_rx);
                            cleanup_failed_acquire(path, sock_id, replacement.is_held());
                            return Err(err)
                                .with_context(|| format!("cloning socket '{path}' for drain"));
                        }
                    };
                    // `drain_listener`/`shutdown_rx` move into the spawn closure;
                    // on failure the closure (and both fds) drops with it, so
                    // only the bound listener and `shutdown_tx` need explicit
                    // drops here before unlinking our fresh pathname.
                    // `stop` is only moved on success; on failure it drops.
                    let drain_stop = Arc::clone(&stop);
                    let drainer = match thread::Builder::new()
                        .name("ltd-sock-drain".to_string())
                        .spawn(move || drain_loop(drain_listener, drain_stop, shutdown_rx))
                    {
                        Ok(handle) => handle,
                        Err(err) => {
                            drop(listener);
                            drop(shutdown_tx);
                            cleanup_failed_acquire(path, sock_id, replacement.is_held());
                            return Err(err)
                                .with_context(|| format!("spawning drain thread for '{path}'"));
                        }
                    };
                    return Ok(Self {
                        path: path.to_string(),
                        sock_id,
                        _listener: listener,
                        stop,
                        shutdown_tx,
                        drainer: Some(drainer),
                    });
                }
                Err(e) if e.kind() == std::io::ErrorKind::AddrInUse => {
                    // Stale file or live daemon? A live owner answers connects.
                    match connect_with_timeout(path, LIVENESS_TIMEOUT) {
                        Ok(_) => {
                            anyhow::bail!("daemon already running (socket '{path}' live)");
                        }
                        Err(conn_err) if is_timeout(&conn_err) => {
                            // Listener exists but is not draining fast enough
                            // (full queue): still a live owner, never delete.
                            anyhow::bail!(
                                "daemon already running (socket '{path}' live, probe timed out)"
                            );
                        }
                        Err(conn_err) => match conn_err.kind() {
                            // No listener behind the file: stale residue of a
                            // dead daemon. A parallel starter may remove it
                            // first — ignore NotFound and re-bind (which then
                            // re-checks liveness if still AddrInUse).
                            std::io::ErrorKind::ConnectionRefused
                            | std::io::ErrorKind::NotFound => {
                                log::warn!("removing stale socket '{path}'");
                                match std::fs::remove_file(path) {
                                    Ok(()) => {}
                                    Err(rm)
                                        if rm.kind() == std::io::ErrorKind::NotFound => {}
                                    Err(rm) => {
                                        return Err(rm).with_context(|| {
                                            format!("removing stale socket '{path}'")
                                        });
                                    }
                                }
                                continue;
                            }
                            // Anything else (e.g. EACCES on a live daemon's
                            // socket): unverifiable, so never delete — bail
                            // and keep the file.
                            _ => anyhow::bail!(
                                "socket '{path}' in use but not connectable ({conn_err}); refusing to remove"
                            ),
                        },
                    }
                }
                Err(e) => return Err(e).with_context(|| format!("binding socket '{path}'")),
            }
        }
        anyhow::bail!("socket '{path}' still contested after stale cleanup; retry")
    }

    pub fn path(&self) -> &str {
        &self.path
    }

    /// Accept and close up to [`DRAIN_BATCH_LIMIT`] currently queued probe
    /// connections (nonblocking). Returns the number drained. The background
    /// thread drains via `drain_once` in a loop; tests call this directly
    /// to assert drain behavior.
    #[cfg(test)]
    fn drain_pending(&self) -> usize {
        drain_once(&self._listener)
    }
}

/// Owned self-pipe for drainer shutdown: `(write, read)`, both
/// `CLOEXEC` + nonblocking. The holder keeps the write end; the drain thread
/// blocks in `poll` on the read end alongside the listening socket. Bytes
/// persist in the pipe, so a shutdown queued before the waiter enters `poll`
/// still wakes it — no lost wakeup.
fn shutdown_pipe() -> io::Result<(std::os::fd::OwnedFd, std::os::fd::OwnedFd)> {
    use std::os::fd::{FromRawFd, OwnedFd};
    let mut fds = [-1; 2];
    // SAFETY: `fds` points to two writable descriptor slots.
    if unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC | libc::O_NONBLOCK) } < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: pipe2 returned two newly owned descriptors; fds[0] reads,
    // fds[1] writes.
    Ok(unsafe { (OwnedFd::from_raw_fd(fds[1]), OwnedFd::from_raw_fd(fds[0])) })
}

/// Wake an idle drain waiter: write one byte to the shutdown pipe.
/// Retries `EINTR`; ignores `EAGAIN` (already readable) and any other error
/// because the waiter treats any readability as shutdown and `Drop` joins
/// regardless.
fn notify_shutdown(tx: &std::os::fd::OwnedFd) {
    use std::os::fd::AsRawFd;
    let byte = [1_u8];
    loop {
        // SAFETY: `tx` is the live write end and `byte` is readable for 1 byte.
        let rc = unsafe { libc::write(tx.as_raw_fd(), byte.as_ptr().cast(), 1) };
        if rc == 1 {
            return;
        }
        if rc < 0 {
            let err = io::Error::last_os_error();
            if err.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return;
        }
        return;
    }
}

/// Backoff applied when the readiness wait itself fails or the listener
/// reports an error without input. Prevents hot-spinning; shutdown stays
/// observable via `stop` (checked each pass) and typically resolves within
/// about one backoff. Not a hard realtime bound: scheduling can delay it.
const DRAIN_ERROR_BACKOFF: Duration = Duration::from_millis(50);

/// Single readiness wait: one blocking `poll(-1)` over the listener and the
/// shutdown pipe. Returns `(listener_revents, shutdown_revents)`. No retry or
/// sleep here; the caller applies the stop/error policy so `EINTR` and
/// persistent failures never bypass shutdown.
fn poll_two(
    listener_fd: std::os::fd::RawFd,
    shutdown_fd: std::os::fd::RawFd,
) -> io::Result<(i16, i16)> {
    let mut fds = [
        libc::pollfd {
            fd: listener_fd,
            events: libc::POLLIN,
            revents: 0,
        },
        libc::pollfd {
            fd: shutdown_fd,
            events: libc::POLLIN,
            revents: 0,
        },
    ];
    // SAFETY: `fds` is a valid two-element array for the duration of the call.
    let rc = unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as _, -1) };
    if rc >= 0 {
        Ok((fds[0].revents, fds[1].revents))
    } else {
        Err(io::Error::last_os_error())
    }
}

/// Readiness waiter replacing the old 10ms idle sleep: block in `poll` until
/// the listener is readable or the shutdown pipe fires. Each input wake
/// drains one bounded `drain_once` pass; a pass that makes no progress while
/// the socket stays readable backs off instead of repolling immediately.
fn drain_loop(listener: UnixListener, stop: Arc<AtomicBool>, shutdown_rx: std::os::fd::OwnedFd) {
    drain_loop_with_wait(listener, stop, shutdown_rx, poll_two)
}

/// Wait-injected waiter core. `wait` performs one readiness wait over the
/// two fds; production passes [`poll_two`], tests inject failures or error
/// revents without touching host limits. Shutdown (`stop` + any shutdown
/// revents) always wins over listener handling.
fn drain_loop_with_wait(
    listener: UnixListener,
    stop: Arc<AtomicBool>,
    shutdown_rx: std::os::fd::OwnedFd,
    mut wait: impl FnMut(std::os::fd::RawFd, std::os::fd::RawFd) -> io::Result<(i16, i16)>,
) {
    use std::os::fd::AsRawFd;
    let listener_fd = listener.as_raw_fd();
    let shutdown_fd = shutdown_rx.as_raw_fd();
    loop {
        // Independent of `poll` success so a persistent wait failure cannot
        // hide shutdown (F1): `Drop` sets `stop` before notifying.
        if stop.load(Ordering::Relaxed) {
            return;
        }
        match wait(listener_fd, shutdown_fd) {
            Ok((listener_revents, shutdown_revents)) => {
                // Any shutdown readability (including pipe errors) exits;
                // the pipe is only ever used for this signal.
                if shutdown_revents != 0 {
                    return;
                }
                if stop.load(Ordering::Relaxed) {
                    return;
                }
                if listener_revents & libc::POLLNVAL != 0 {
                    // Invalid listener fd: unrecoverable without fd surgery.
                    // Exit so `Drop` can still join; the holder keeps its
                    // pathname semantics and the next acquire reclaims.
                    log::debug!("drain waiter exiting: listener fd invalid");
                    return;
                }
                if listener_revents & libc::POLLIN != 0 {
                    // One bounded pass per wake; the next wait returns
                    // immediately while backlog remains, so bursts drain
                    // promptly without spinning. A pass that drains nothing
                    // while the socket stays readable (e.g. accept keeps
                    // failing under EMFILE) would otherwise repoll forever:
                    // back off instead of hot-spinning. Shutdown stays
                    // prompt: `stop` is checked before sleeping and at the
                    // top, and a queued shutdown byte makes the next wait
                    // return with `shutdown_revents` set.
                    if drain_once(&listener) == 0 {
                        if stop.load(Ordering::Relaxed) {
                            return;
                        }
                        thread::sleep(DRAIN_ERROR_BACKOFF);
                    }
                } else if listener_revents & (libc::POLLERR | libc::POLLHUP) != 0 {
                    // Error with no input to drain: `poll` would return
                    // immediately again, so back off instead of hot-spinning.
                    // Shutdown stays prompt: `stop` is checked at the top
                    // and a queued shutdown byte makes the next wait return
                    // with `shutdown_revents` set.
                    thread::sleep(DRAIN_ERROR_BACKOFF);
                }
                // Otherwise spurious (e.g. zero revents): re-wait.
            }
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {
                // `EINTR`: re-wait; `stop` is rechecked at the top, so a
                // signal storm cannot delay shutdown beyond one wait.
                continue;
            }
            Err(e) => {
                // Persistent wait failure (e.g. ENOMEM): the queued shutdown
                // byte may never surface through `poll`, so `stop` (not the
                // pipe) is the shutdown signal here. Back off to avoid
                // hot-spinning; the top-of-loop check exits typically within
                // about one backoff after `Drop` sets `stop`.
                log::debug!("drain wait failed ({e}); backing off");
                if stop.load(Ordering::Relaxed) {
                    return;
                }
                thread::sleep(DRAIN_ERROR_BACKOFF);
            }
        }
    }
}

/// Single nonblocking drain pass, bounded by [`DRAIN_BATCH_LIMIT`].
fn drain_once(listener: &UnixListener) -> usize {
    let mut drained = 0;
    for _ in 0..DRAIN_BATCH_LIMIT {
        match listener.accept() {
            Ok((stream, _)) => {
                drop(stream);
                drained += 1;
            }
            Err(e)
                if e.kind() == io::ErrorKind::WouldBlock
                    || e.kind() == io::ErrorKind::Interrupted =>
            {
                break;
            }
            Err(_) => break,
        }
    }
    drained
}

fn is_timeout(e: &io::Error) -> bool {
    e.kind() == io::ErrorKind::TimedOut
}

/// Bounded liveness dial: nonblocking connect + `poll(2)` for writability.
/// Returns `TimedOut` instead of blocking forever on a full listen queue.
/// Immediate refusals (stale file) and permission errors surface unchanged.
/// A saturated filesystem queue fails fast with `EAGAIN` from `connect`
/// (no connection is in progress), which maps to `TimedOut` live/busy;
/// only `EINPROGRESS` waits on `poll` with the remaining deadline.
pub(crate) fn connect_with_timeout(path: &str, timeout: Duration) -> io::Result<UnixStream> {
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};

    let bytes = path.as_bytes();
    if bytes.is_empty() || bytes.contains(&0) || bytes.len() > 107 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("invalid socket path '{path}'"),
        ));
    }
    // SAFETY: checked length above; CLOEXEC + NONBLOCK so dial never blocks.
    let raw = unsafe {
        libc::socket(
            libc::AF_UNIX,
            libc::SOCK_STREAM | libc::SOCK_NONBLOCK | libc::SOCK_CLOEXEC,
            0,
        )
    };
    if raw < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: `socket` returned a newly owned fd; wrap immediately so every
    // early return closes it without manual `close` calls.
    let fd: OwnedFd = unsafe { OwnedFd::from_raw_fd(raw) };

    // SAFETY: zeroed sockaddr_un, family + nul-terminated filesystem path.
    let mut addr: libc::sockaddr_un = unsafe { std::mem::zeroed() };
    addr.sun_family = libc::AF_UNIX as libc::sa_family_t;
    for (i, b) in bytes.iter().enumerate() {
        addr.sun_path[i] = *b as libc::c_char;
    }
    let base = (&addr.sun_path as *const _ as usize) - (&addr as *const _ as usize);
    let addr_len = base + bytes.len() + 1;

    // SAFETY: addr is a valid sockaddr_un of addr_len bytes, fd is ours.
    let ret = unsafe {
        libc::connect(
            fd.as_raw_fd(),
            &addr as *const _ as *const libc::sockaddr,
            addr_len as libc::socklen_t,
        )
    };
    if ret == 0 {
        // Probe verdict only; callers drop the stream immediately, so the
        // leftover O_NONBLOCK flag is irrelevant (no fcntl round-trip).
        return Ok(UnixStream::from(fd));
    }
    let err = io::Error::last_os_error();
    let raw_err = err.raw_os_error();
    // Saturated queue: no asynchronous connection is in progress, so polling
    // for writability could report POLLOUT|HUP with SO_ERROR == 0 on an
    // unconnected stream. Fail fast as live/busy instead.
    if raw_err == Some(libc::EAGAIN) || raw_err == Some(libc::EWOULDBLOCK) {
        return Err(io::Error::from(io::ErrorKind::TimedOut));
    }
    if raw_err != Some(libc::EINPROGRESS) {
        return Err(err);
    }
    let deadline = Instant::now() + timeout;
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        let timeout_ms = remaining.as_millis().min(i32::MAX as u128) as i32;
        let mut pfd = libc::pollfd {
            fd: fd.as_raw_fd(),
            events: libc::POLLOUT | libc::POLLERR | libc::POLLHUP,
            revents: 0,
        };
        // SAFETY: pfd points to one valid pollfd, timeout >= 0.
        let pr = unsafe { libc::poll(&mut pfd, 1, timeout_ms) };
        if pr < 0 {
            let e = io::Error::last_os_error();
            if e.kind() == io::ErrorKind::Interrupted {
                if Instant::now() >= deadline {
                    return Err(io::Error::from(io::ErrorKind::TimedOut));
                }
                continue;
            }
            return Err(e);
        }
        if pr == 0 {
            return Err(io::Error::from(io::ErrorKind::TimedOut));
        }
        let mut so_err: i32 = 0;
        let mut len: libc::socklen_t = std::mem::size_of::<i32>() as libc::socklen_t;
        // SAFETY: so_err/len are valid out-params for SO_ERROR.
        let gr = unsafe {
            libc::getsockopt(
                fd.as_raw_fd(),
                libc::SOL_SOCKET,
                libc::SO_ERROR,
                &mut so_err as *mut _ as *mut libc::c_void,
                &mut len,
            )
        };
        if gr != 0 {
            return Err(io::Error::last_os_error());
        }
        if so_err != 0 {
            // A completed-but-full queue surfaces here as EAGAIN/EWOULDBLOCK
            // on some kernels: report it as a timeout (live, busy) so
            // callers treat it as "live" rather than stale/deletable.
            if so_err == libc::EAGAIN || so_err == libc::EWOULDBLOCK {
                return Err(io::Error::from(io::ErrorKind::TimedOut));
            }
            return Err(io::Error::from_raw_os_error(so_err));
        }
        // SO_ERROR == 0 after EINPROGRESS means the stream connected; we own fd.
        // No fcntl reset: the probe stream is dropped by every caller.
        return Ok(UnixStream::from(fd));
    }
}

impl Drop for SocketLock {
    fn drop(&mut self) {
        use std::os::unix::fs::MetadataExt;
        // Stop the drainer first so no cloned fd outlives our unlink check.
        // Flag THEN notify BEFORE joining: the flag keeps shutdown observable
        // even if `poll` keeps failing, and one byte wakes an idle `poll`
        // waiter promptly. The byte persists if the waiter has not entered
        // `poll` yet, so the wakeup cannot be lost. A full pipe already
        // leaves the waiter readable, and `EINTR` retries; all other write
        // outcomes are ignored because the thread exits on flag or pipe.
        self.stop.store(true, Ordering::Relaxed);
        notify_shutdown(&self.shutdown_tx);
        if let Some(handle) = self.drainer.take() {
            let _ = handle.join();
        }
        // Serialize the identity check + unlink against a successor's
        // stale-check/remove/bind by holding the replacement mutex across
        // both steps. Bounded by construction: a single LOCK_NB attempt, so
        // shutdown never blocks on a starter mid-acquire (whose bounded probe
        // can still wait on a busy listener).
        //
        // When the mutex cannot be taken immediately (contended or unusable),
        // skip pathname cleanup entirely: without exclusion a successor could
        // rebind between our check and our unlink and we would delete its
        // live pathname. Closing the listener still leaves stale residue that
        // the next serialized acquire reclaims, so skipping never leaks
        // permanently. Never stat/unlink as if holding a failed mutex.
        //
        // Unlink only while the path still names the filesystem entry our
        // bind created (dev+ino captured at acquisition). A successor that
        // rebound the path after an external unlink owns a new inode, so a
        // dying predecessor holding the mutex never removes the live file.
        //
        // The entry identity must come from the pathname, not from fstat of
        // the socket fd: on Linux the fd refers to a sockfs inode while the
        // pathname is a filesystem inode, so comparing the two never matches
        // and a normal drop would leak the file.
        let Some(_mutex) = replacement_mutex_try(&self.path) else {
            // No exclusion: leave residue for the next acquire. Distinguish
            // in logs via `replacement_mutex_try` (contended vs unavailable).
            return;
        };
        let current = std::fs::metadata(&self.path)
            .ok()
            .map(|m| (m.dev(), m.ino()));
        match current {
            Some(id) if id == self.sock_id => {
                let _ = std::fs::remove_file(&self.path);
            }
            _ => {
                // Mismatch (successor rebound) or unreadable: leave the file.
                // A stale residue is reclaimed by the next starter.
                log::debug!("keeping socket '{}' (not ours or unreadable)", self.path);
            }
        }
    }
}

/// True when a live daemon answers at `sock_path`. Used by `reconnect` and
/// `resume`: absent socket without `-f`/`-d` means inactive (hardware left
/// alone) under the activation contract.
/// Bounded by [`LIVENESS_TIMEOUT`]; a timed-out probe implies a live
/// listener with a full queue, so it reports present rather than
/// misreading a busy daemon as absent.
pub fn daemon_socket_present(sock_path: &str) -> bool {
    daemon_socket_present_timeout(sock_path, LIVENESS_TIMEOUT)
}

/// Bounded variant of [`daemon_socket_present`] for tests that need
/// a custom budget.
fn daemon_socket_present_timeout(sock_path: &str, timeout: Duration) -> bool {
    present_from_probe(connect_with_timeout(sock_path, timeout))
}

/// Verdict mapping for a liveness probe: answered or timed out means a live
/// listener owns the path (a full queue implies someone is listening);
/// anything else means absent/stale/unverifiable (never treated as live).
/// Split out so the timeout→live contract is unit-tested without filling
/// a ~4k kernel queue.
fn present_from_probe(res: io::Result<UnixStream>) -> bool {
    match res {
        Ok(_) => true,
        Err(e) if is_timeout(&e) => true,
        Err(_) => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn acquire_and_presence_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("driver.sock");
        let sock = sock.to_str().unwrap();
        assert!(!daemon_socket_present(sock));
        let guard = SocketLock::acquire(sock).unwrap();
        assert_eq!(guard.path(), sock);
        assert!(daemon_socket_present(sock));
        assert!(
            std::path::Path::new(sock).exists(),
            "bound socket pathname must exist while held"
        );
        drop(guard);
        assert!(!daemon_socket_present(sock));
        assert!(
            !std::path::Path::new(sock).exists(),
            "normal drop must delete the socket pathname, not just stop answering"
        );
    }

    #[test]
    fn second_acquire_fails_while_live() {
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("d.sock");
        let sock = sock.to_str().unwrap();
        let _first = SocketLock::acquire(sock).unwrap();
        assert!(SocketLock::acquire(sock).is_err());
    }

    #[test]
    fn legacy_path_refused() {
        assert!(SocketLock::acquire(LEGACY_SOCK).is_err());
    }

    #[test]
    fn paths_do_not_collide() {
        assert_ne!(DRIVER_SOCK, LEGACY_SOCK);
    }

    #[test]
    fn socket_path_default_and_override() {
        // Pure override parameter — no process-env mutation, safe threaded.
        // The default-arm read still touches process env, so hold the
        // shared env lock; restore the prior value instead of leaking
        // the removal.
        let _env_guard = crate::test_env::lock_env();
        let old = std::env::var(SOCKET_ENV).ok();
        unsafe { std::env::remove_var(SOCKET_ENV) };
        assert_eq!(socket_path(), DRIVER_SOCK);
        assert_eq!(
            socket_path_with(Some("/tmp/test-ltd.sock")),
            "/tmp/test-ltd.sock"
        );
        match old {
            Some(v) => unsafe { std::env::set_var(SOCKET_ENV, v) },
            None => unsafe { std::env::remove_var(SOCKET_ENV) },
        }
    }

    #[test]
    fn predecessor_drop_keeps_successor_socket() {
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("handoff.sock");
        let sock = sock.to_str().unwrap();
        let first = SocketLock::acquire(sock).unwrap();
        // External deletion (stale-cleanup by another actor) + successor rebind.
        std::fs::remove_file(sock).unwrap();
        let second = SocketLock::acquire(sock).unwrap();
        assert!(daemon_socket_present(sock));
        drop(first); // dying predecessor must not unlink the successor's file
        assert!(
            daemon_socket_present(sock),
            "successor socket must survive predecessor drop"
        );
        assert!(std::path::Path::new(sock).exists());
        drop(second);
        assert!(!daemon_socket_present(sock));
        assert!(
            !std::path::Path::new(sock).exists(),
            "final drop must delete the socket pathname"
        );
    }

    #[test]
    fn stale_socket_file_is_reclaimed() {
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("stale.sock");
        let sock = sock.to_str().unwrap();
        // Dead daemon residue: bound listener dropped without cleanup —
        // file exists, nobody answers.
        let listener = UnixListener::bind(sock).unwrap();
        drop(listener);
        assert!(!daemon_socket_present(sock));
        let guard = SocketLock::acquire(sock).unwrap();
        assert!(daemon_socket_present(sock));
        drop(guard);
        assert!(!daemon_socket_present(sock));
        assert!(
            !std::path::Path::new(sock).exists(),
            "reclaimed stale socket must be deleted on drop"
        );
    }

    #[test]
    fn concurrent_stale_starters_single_owner() {
        use std::sync::{Arc, Barrier};
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("race.sock");
        let sock = sock.to_str().unwrap().to_string();
        // Dead daemon residue: bound listener dropped without cleanup —
        // file exists, nobody answers. Every starter below observes it.
        let listener = UnixListener::bind(&sock).unwrap();
        drop(listener);
        assert!(!daemon_socket_present(&sock));

        // Release all starters at once onto the same stale file. Exactly one
        // must bind; the rest must observe the live winner and bail — no
        // loser may unlink the winner's fresh pathname via its earlier
        // refusal observation. The second barrier holds every guard until all
        // acquires finished, so a winner cannot drop (and free the path for
        // a late loser) mid-race. Guards (not bools) cross the join so the
        // winner's pathname can be checked live before its drop.
        const STARTERS: usize = 8;
        let start = Arc::new(Barrier::new(STARTERS));
        let settled = Arc::new(Barrier::new(STARTERS));
        let racers: Vec<Option<SocketLock>> = std::thread::scope(|s| {
            (0..STARTERS)
                .map(|_| {
                    s.spawn(|| {
                        start.wait();
                        let guard = SocketLock::acquire(&sock).ok();
                        settled.wait();
                        guard
                    })
                })
                .collect::<Vec<_>>()
                .into_iter()
                .map(|h| h.join().unwrap())
                .collect()
        });
        let mut winners: Vec<SocketLock> = racers.into_iter().flatten().collect();
        assert_eq!(
            winners.len(),
            1,
            "exactly one starter must win the stale socket"
        );
        // The winner's pathname must survive every loser's stale cleanup.
        let winner = winners.pop().unwrap();
        assert!(daemon_socket_present(&sock));
        assert!(
            std::path::Path::new(&sock).exists(),
            "winner pathname must exist while held"
        );
        drop(winner);
        assert!(
            !std::path::Path::new(&sock).exists(),
            "winner drop must delete the socket pathname after the race"
        );
        assert!(!daemon_socket_present(&sock));
    }

    /// Hold `LOCK_EX` on `{sock}.lock` to pin the replacement mutex.
    /// The returned file keeps the lock until dropped.
    fn hold_replacement_lock(sock: &str) -> std::fs::File {
        use std::os::unix::io::AsRawFd;
        let lock_path = format!("{sock}.lock");
        let f = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(&lock_path)
            .unwrap();
        // SAFETY: `f` is a live open file.
        assert_eq!(unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX) }, 0);
        f
    }

    #[test]
    fn drop_skips_cleanup_when_mutex_contended() {
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("drop-busy.sock");
        let sock = sock.to_str().unwrap().to_string();
        let guard = SocketLock::acquire(&sock).unwrap();
        // Pin the mutex so Drop cannot take exclusion: it must return
        // without hanging and must NOT unlink, leaving reclaimable residue.
        // Unlinking without the mutex could delete a successor that rebinds
        // between check and unlink.
        let pinned = hold_replacement_lock(&sock);
        drop(guard);
        assert!(
            std::path::Path::new(&sock).exists(),
            "contended drop must leave residue instead of unlinking without exclusion"
        );
        assert!(
            !daemon_socket_present(&sock),
            "dropped listener must not answer even though its pathname remains"
        );
        drop(pinned);
        // The next serialized acquire reclaims the residue.
        let successor = SocketLock::acquire(&sock).unwrap();
        assert!(daemon_socket_present(&sock));
        assert!(std::path::Path::new(&sock).exists());
        drop(successor);
        assert!(!std::path::Path::new(&sock).exists());
        assert!(!daemon_socket_present(&sock));
    }

    #[test]
    fn drop_vs_blocked_successor_single_owner() {
        use std::sync::mpsc;
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("drop-race.sock");
        let sock = sock.to_str().unwrap().to_string();
        let pred = SocketLock::acquire(&sock).unwrap();
        // Pin the mutex so the successor blocks inside acquire.
        let pinned = hold_replacement_lock(&sock);
        let (started_tx, started_rx) = mpsc::channel();
        let sock2 = sock.clone();
        let handle = std::thread::spawn(move || {
            started_tx.send(()).unwrap();
            SocketLock::acquire(&sock2).map_err(|e| e.to_string())
        });
        // Deterministic handshake: the successor thread is alive and has
        // entered acquire before we drop. It must be blocked on the pinned
        // mutex regardless of exact scheduling, because the pin is held
        // across both the drop and the successor's lock attempt. No sleep.
        started_rx.recv().unwrap();
        // Predecessor Drop contends on the pinned mutex: it must skip cleanup
        // (no hang) and leave residue instead of unlinking without exclusion.
        drop(pred);
        assert!(
            std::path::Path::new(&sock).exists(),
            "contended predecessor drop must leave residue for serialized reclaim"
        );
        drop(pinned);
        let succ = handle
            .join()
            .unwrap()
            .expect("successor must win after predecessor drop");
        assert_eq!(succ.path(), sock);
        assert!(daemon_socket_present(&sock));
        assert!(std::path::Path::new(&sock).exists());
        drop(succ);
        assert!(!std::path::Path::new(&sock).exists());
        assert!(!daemon_socket_present(&sock));
    }

    #[test]
    fn contended_drop_after_external_unlink_preserves_successor() {
        use std::sync::mpsc;
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("drop-ext-unlink.sock");
        let sock = sock.to_str().unwrap().to_string();
        let pred = SocketLock::acquire(&sock).unwrap();
        // Pin the replacement mutex so predecessor Drop cannot take
        // exclusion. This is the actual contention boundary (real flock),
        // not a mocked flag.
        let pinned = hold_replacement_lock(&sock);
        let (started_tx, started_rx) = mpsc::channel();
        let dropper = std::thread::spawn(move || {
            started_tx.send(()).unwrap();
            drop(pred);
        });
        // Ensure the Drop thread is alive while we perform the supported
        // external-unlink handoff step from
        // `predecessor_drop_keeps_successor_socket`: the old pathname is
        // unlinked while the predecessor listener is still open. With the
        // mutex pinned, Drop must skip cleanup, so this unlink is the only
        // remover and no racing unlink can delete the successor below.
        started_rx.recv().unwrap();
        std::fs::remove_file(&sock).unwrap();
        dropper.join().unwrap();
        // Contended Drop must not have unlinked anything after the external
        // unlink (it holds no exclusion). The path is free for replacement.
        assert!(
            !std::path::Path::new(&sock).exists(),
            "path must stay free after contended drop + external unlink"
        );
        drop(pinned);
        // Serialized successor rebinds the free path and owns it.
        let succ = SocketLock::acquire(&sock).unwrap();
        assert!(daemon_socket_present(&sock));
        assert!(
            std::path::Path::new(&sock).exists(),
            "successor pathname must exist after serialized rebind"
        );
        // No late predecessor unlink can follow: the predecessor already
        // finished while contended and skipped cleanup.
        assert!(daemon_socket_present(&sock));
        drop(succ);
        assert!(!std::path::Path::new(&sock).exists());
        assert!(!daemon_socket_present(&sock));
    }

    #[test]
    fn repeated_probes_stay_bounded_with_drainer() {
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("drain.sock");
        let sock = sock.to_str().unwrap();
        let guard = SocketLock::acquire(sock).unwrap();
        let start = Instant::now();
        // Rapid sequential probes stay fast and answer live. With the default
        // backlog (~4k) this passes even without draining, so it pins
        // boundedness only; drainer liveness is pinned separately by
        // `background_drainer_accepts_and_closes`.
        for _ in 0..300 {
            assert!(
                daemon_socket_present_timeout(sock, Duration::from_millis(200)),
                "live holder must answer every probe"
            );
        }
        let elapsed = start.elapsed();
        assert!(
            elapsed < Duration::from_secs(10),
            "300 probes took {elapsed:?}, queue likely undrained"
        );
        // A contested starter must still get a fast live verdict, not hang.
        let contest = Instant::now();
        assert!(SocketLock::acquire(sock).is_err());
        assert!(
            contest.elapsed() < Duration::from_secs(2),
            "contested acquire hung: {:?}",
            contest.elapsed()
        );
        // Explicit drain pass must also leave the socket answerable.
        assert!(guard.drain_pending() < DRAIN_BATCH_LIMIT);
        assert!(daemon_socket_present(sock));
    }

    #[test]
    fn drain_once_clears_queued_probes() {
        // Accept-drain logic without any thread or timing race: blocking
        // dials return only once the server has queued them, so a single
        // bounded drain pass must collect exactly every pending probe.
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("drain-once.sock");
        let sock_str = sock.to_str().unwrap();
        let listener = UnixListener::bind(sock_str).unwrap();
        listener.set_nonblocking(true).unwrap();
        let mut clients = Vec::new();
        for _ in 0..16 {
            clients.push(UnixStream::connect(sock_str).unwrap());
        }
        assert_eq!(drain_once(&listener), 16);
        assert_eq!(drain_once(&listener), 0);
        drop(clients);
        drop(listener);
    }

    #[test]
    fn loaded_undrained_queue_probe_stays_bounded() {
        // A holder that never accepts (pre-fix behavior) accumulates one
        // queued entry per probe. Even with hundreds of entries pending, the
        // bounded probe must return fast instead of stalling: it either
        // queues behind them or degrades to a timeout verdict.
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("loaded.sock");
        let sock_str = sock.to_str().unwrap();
        let listener = UnixListener::bind(sock_str).unwrap();
        listener.set_nonblocking(true).unwrap();
        // Nonblocking dials never stall the test thread; holding the client
        // fds keeps the server queue occupied.
        let mut pending = Vec::new();
        for _ in 0..300 {
            pending.push(nonblocking_dial(sock_str).unwrap());
        }
        let start = Instant::now();
        let res = connect_with_timeout(sock_str, Duration::from_millis(500));
        let elapsed = start.elapsed();
        assert!(
            elapsed < Duration::from_secs(2),
            "bounded probe stalled on loaded queue: {elapsed:?}"
        );
        match res {
            Ok(_) => {}
            Err(e) if is_timeout(&e) => {}
            Err(e) => panic!("loaded-queue probe must queue or time out, got: {e}"),
        }
        drop(pending);
        drop(listener);
    }

    #[test]
    fn saturated_queue_maps_to_timeout_live_verdict() {
        // Filesystem backlog full: nonblocking `connect` fails with EAGAIN
        // without queueing anything. The helper must report live/busy
        // (`TimedOut`), never `Ok` with an unconnected stream, and callers
        // must keep the pathname (no stale reclaim).
        use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};

        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("tiny.sock");
        let sock_str = sock.to_str().unwrap();
        let listener = UnixListener::bind(sock_str).unwrap();
        // Shrink this fixture's backlog; no global somaxconn change.
        assert_eq!(unsafe { libc::listen(listener.as_raw_fd(), 1) }, 0);

        // Fill until the kernel reports saturation. Held streams keep queued
        // entries alive; the saturating dial queued nothing and is dropped.
        let mut queued: Vec<UnixStream> = Vec::new();
        let mut saturated = false;
        for _ in 0..64 {
            let raw = unsafe {
                libc::socket(
                    libc::AF_UNIX,
                    libc::SOCK_STREAM | libc::SOCK_NONBLOCK | libc::SOCK_CLOEXEC,
                    0,
                )
            };
            assert!(raw >= 0);
            // SAFETY: `socket` returned a newly owned fd.
            let fd: OwnedFd = unsafe { OwnedFd::from_raw_fd(raw) };
            let bytes = sock_str.as_bytes();
            let mut addr: libc::sockaddr_un = unsafe { std::mem::zeroed() };
            addr.sun_family = libc::AF_UNIX as libc::sa_family_t;
            for (i, b) in bytes.iter().enumerate() {
                addr.sun_path[i] = *b as libc::c_char;
            }
            let base = (&addr.sun_path as *const _ as usize) - (&addr as *const _ as usize);
            let len = base + bytes.len() + 1;
            let rc = unsafe {
                libc::connect(
                    fd.as_raw_fd(),
                    &addr as *const _ as *const libc::sockaddr,
                    len as libc::socklen_t,
                )
            };
            if rc == 0 {
                queued.push(UnixStream::from(fd));
                continue;
            }
            match io::Error::last_os_error().raw_os_error() {
                Some(code) if code == libc::EINPROGRESS => {
                    queued.push(UnixStream::from(fd));
                }
                Some(code) if code == libc::EAGAIN || code == libc::EWOULDBLOCK => {
                    saturated = true;
                    drop(fd);
                    break;
                }
                Some(code) => panic!("unexpected fill dial errno {code}"),
                None => panic!("fill dial failed without errno"),
            }
        }
        assert!(saturated, "tiny backlog must saturate within bound");

        let start = Instant::now();
        assert!(daemon_socket_present(sock_str));
        assert!(SocketLock::acquire(sock_str).is_err());
        assert!(Path::new(sock_str).exists());
        assert!(
            start.elapsed() < Duration::from_secs(2),
            "saturated-queue verdicts must stay fast"
        );
        let err = connect_with_timeout(sock_str, Duration::from_millis(50)).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::TimedOut);
        // Pathname survives: a full queue is live/busy, never stale.
        assert!(Path::new(sock_str).exists());
        drop(queued);
        drop(listener);
    }

    #[test]
    fn background_drainer_accepts_and_closes() {
        // Durable drainer proof: the holder's thread accepts the probe and
        // closes it, so the client observes EOF. The 300-probe test above
        // fits the default backlog and cannot prove this on its own.
        use std::io::Read;

        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("thread.sock");
        let sock_str = sock.to_str().unwrap();
        let _guard = SocketLock::acquire(sock_str).unwrap();
        let mut client = UnixStream::connect(sock_str).unwrap();
        client
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        assert_eq!(client.read(&mut [0_u8; 1]).unwrap(), 0);
    }

    #[test]
    fn drainer_accepts_after_idle() {
        // Idle-window regression for the readiness waiter: after a quiet
        // period with an empty queue (longer than the old 10ms idle sleep),
        // a new probe must still be accepted and closed in the background.
        // The assertion is synchronized (EOF with a generous read timeout),
        // not a timing measurement; no power/CPU claim is made.
        use std::io::Read;

        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("idle.sock");
        let sock_str = sock.to_str().unwrap();
        let guard = SocketLock::acquire(sock_str).unwrap();
        assert_eq!(guard.drain_pending(), 0);
        std::thread::sleep(Duration::from_millis(100));
        let mut client = UnixStream::connect(sock_str).unwrap();
        client
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        assert_eq!(client.read(&mut [0_u8; 1]).unwrap(), 0);
    }

    #[test]
    fn idle_waiter_wakes_on_shutdown_without_connection() {
        // Explicit shutdown wakeup with no connection ever queued. The wait
        // wrapper signals actual `poll_two` entry before stop is requested,
        // so this test cannot pass via the loop-top flag check without ever
        // entering `poll`: a missed pipe wakeup (lost signal) would hang the
        // join. The pipe byte persists, so the waiter must still exit.
        // Bounded via channels so a regression fails instead of hanging the
        // suite, and the worker join propagates panics.
        use std::sync::mpsc;

        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("wake.sock");
        let sock_str = sock.to_str().unwrap();
        let listener = UnixListener::bind(sock_str).unwrap();
        listener.set_nonblocking(true).unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let (tx, rx) = shutdown_pipe().unwrap();
        let drain_stop = Arc::clone(&stop);
        let (entered_tx, entered_rx) = mpsc::channel::<()>();
        let handle = std::thread::Builder::new()
            .name("test-drain-wake".to_string())
            .spawn(move || {
                drain_loop_with_wait(listener, drain_stop, rx, move |listener_fd, shutdown_fd| {
                    let _ = entered_tx.send(());
                    poll_two(listener_fd, shutdown_fd)
                })
            })
            .unwrap();
        entered_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("waiter must enter poll before shutdown");
        // Flag then notify, mirroring `Drop`: covers both an idle `poll`
        // waiter (byte) and a failing waiter (flag).
        stop.store(true, Ordering::Relaxed);
        notify_shutdown(&tx);
        let (done_tx, done_rx) = mpsc::channel();
        std::thread::spawn(move || {
            handle.join().expect("worker thread must not panic");
            let _ = done_tx.send(());
        });
        done_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("idle poll waiter must wake on shutdown");
    }

    #[test]
    fn persistent_wait_error_plus_shutdown_returns_promptly() {
        // F1 regression with an injected wait seam (no host rlimit/fd games):
        // the readiness wait always fails (e.g. ENOMEM), so the shutdown pipe
        // byte can never surface through `poll`. The `stop` flag must still
        // end the worker. The entered-gate below proves the failing wait
        // actually ran before stop is requested: without it the worker could
        // exit at the loop-top flag check having never exercised the
        // persistent-error path. Bounded via channels so failure reports
        // instead of hanging the suite.
        use std::sync::{atomic::AtomicUsize, mpsc};

        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("wait-err.sock");
        let sock_str = sock.to_str().unwrap();
        let listener = UnixListener::bind(sock_str).unwrap();
        let (_tx, rx) = shutdown_pipe().unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let drain_stop = Arc::clone(&stop);
        let calls = Arc::new(AtomicUsize::new(0));
        let calls_in = Arc::clone(&calls);
        let (entered_tx, entered_rx) = mpsc::channel::<()>();
        let handle = std::thread::Builder::new()
            .name("test-drain-wait-err".to_string())
            .spawn(move || {
                drain_loop_with_wait(listener, drain_stop, rx, move |_, _| {
                    if calls_in.fetch_add(1, Ordering::Relaxed) == 0 {
                        let _ = entered_tx.send(());
                    }
                    Err::<(i16, i16), _>(io::Error::from_raw_os_error(libc::ENOMEM))
                })
            })
            .unwrap();
        entered_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("failing wait must run at least once before stop");
        stop.store(true, Ordering::Relaxed);
        let (done_tx, done_rx) = mpsc::channel();
        std::thread::spawn(move || {
            handle.join().expect("worker thread must not panic");
            let _ = done_tx.send(());
        });
        done_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("waiter must exit on stop even when poll keeps failing");
        assert!(
            calls.load(Ordering::Relaxed) >= 1,
            "failing wait must have run"
        );
    }

    #[test]
    fn listener_error_revents_back_off_without_spin() {
        // F2 regression with an injected wait seam: the listener reports
        // POLLERR with no POLLIN, which `poll` would return immediately and
        // forever. The waiter must back off instead of hot-spinning. The
        // entered-gate synchronizes on the first injected wait so a delayed
        // scheduler cannot observe zero runs; the bounded window then asserts
        // the wait-call count stays tiny (a spinning waiter would call
        // thousands of times here).
        use std::sync::{atomic::AtomicUsize, mpsc};

        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("listen-err.sock");
        let sock_str = sock.to_str().unwrap();
        let listener = UnixListener::bind(sock_str).unwrap();
        let (tx, rx) = shutdown_pipe().unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let drain_stop = Arc::clone(&stop);
        let calls = Arc::new(AtomicUsize::new(0));
        let calls_in = Arc::clone(&calls);
        let (entered_tx, entered_rx) = mpsc::channel::<()>();
        let handle = std::thread::Builder::new()
            .name("test-drain-listen-err".to_string())
            .spawn(move || {
                drain_loop_with_wait(listener, drain_stop, rx, move |_, _| {
                    if calls_in.fetch_add(1, Ordering::Relaxed) == 0 {
                        let _ = entered_tx.send(());
                    }
                    Ok((libc::POLLERR, 0))
                })
            })
            .unwrap();
        entered_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("waiter must run at least once before the window");
        std::thread::sleep(Duration::from_millis(150));
        stop.store(true, Ordering::Relaxed);
        notify_shutdown(&tx);
        let (done_tx, done_rx) = mpsc::channel();
        std::thread::spawn(move || {
            handle.join().expect("worker thread must not panic");
            let _ = done_tx.send(());
        });
        done_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("error-revent waiter must still exit on stop");
        let n = calls.load(Ordering::Relaxed);
        assert!(
            n <= 10,
            "POLLERR without input must back off, not spin: {n} waits in ~150ms"
        );
        assert!(n >= 1, "waiter must have waited at least once");
    }

    #[test]
    fn listener_nval_exits_worker_without_shutdown() {
        // Terminal policy: POLLNVAL means the listener fd is unrecoverable.
        // The worker exits on its own (no stop needed) so `Drop` can still
        // join instead of spinning or blocking forever.
        use std::sync::mpsc;

        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("listen-nval.sock");
        let sock_str = sock.to_str().unwrap();
        let listener = UnixListener::bind(sock_str).unwrap();
        let (_tx, rx) = shutdown_pipe().unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let handle = std::thread::Builder::new()
            .name("test-drain-listen-nval".to_string())
            .spawn(move || drain_loop_with_wait(listener, stop, rx, |_, _| Ok((libc::POLLNVAL, 0))))
            .unwrap();
        let (done_tx, done_rx) = mpsc::channel();
        std::thread::spawn(move || {
            handle.join().expect("worker thread must not panic");
            let _ = done_tx.send(());
        });
        done_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("POLLNVAL waiter must exit without requiring shutdown");
    }

    #[test]
    fn pollin_without_drain_progress_backs_off_and_stops() {
        // R1 regression with an injected wait seam: the listener reports
        // POLLIN but the bounded drain pass makes no progress (models accept
        // failing behind a readable queue, e.g. EMFILE, where `poll` stays
        // readable). The waiter must back off instead of repolling
        // immediately, and `stop` must still end it promptly. The empty real
        // queue keeps `drain_once` at zero without touching host limits.
        use std::sync::{atomic::AtomicUsize, mpsc};

        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("no-progress.sock");
        let sock_str = sock.to_str().unwrap();
        let listener = UnixListener::bind(sock_str).unwrap();
        listener.set_nonblocking(true).unwrap();
        let (tx, rx) = shutdown_pipe().unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let drain_stop = Arc::clone(&stop);
        let calls = Arc::new(AtomicUsize::new(0));
        let calls_in = Arc::clone(&calls);
        let (entered_tx, entered_rx) = mpsc::channel::<()>();
        let handle = std::thread::Builder::new()
            .name("test-drain-no-progress".to_string())
            .spawn(move || {
                drain_loop_with_wait(listener, drain_stop, rx, move |_, _| {
                    if calls_in.fetch_add(1, Ordering::Relaxed) == 0 {
                        let _ = entered_tx.send(());
                    }
                    Ok((libc::POLLIN, 0))
                })
            })
            .unwrap();
        entered_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("waiter must poll at least once before the window");
        std::thread::sleep(Duration::from_millis(200));
        stop.store(true, Ordering::Relaxed);
        notify_shutdown(&tx);
        let (done_tx, done_rx) = mpsc::channel();
        std::thread::spawn(move || {
            handle.join().expect("worker thread must not panic");
            let _ = done_tx.send(());
        });
        done_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("no-progress waiter must still exit on stop");
        let n = calls.load(Ordering::Relaxed);
        assert!(
            (2..=12).contains(&n),
            "POLLIN with zero drain progress must back off, not spin: {n} waits in ~200ms"
        );
    }

    #[test]
    fn failed_acquire_cleanup_preserves_live_successor() {
        // R2 regression: post-bind failure cleanup must never unlink a live
        // successor. A real successor holds the path while each failure
        // branch's cleanup decision is replayed: degraded (no exclusion, any
        // identity) and held-mutex-but-stale-identity must both leave the
        // live name alone. Mirrors the ownership policy of `Drop`.
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("succ-live.sock");
        let sock_str = sock.to_str().unwrap();
        let successor = SocketLock::acquire(sock_str).unwrap();
        assert!(daemon_socket_present(sock_str));
        // Degraded predecessor: no exclusion, even with the live identity.
        cleanup_failed_acquire(sock_str, successor.sock_id, false);
        assert!(std::path::Path::new(sock_str).exists());
        assert!(daemon_socket_present(sock_str));
        // Held-mutex predecessor with a stale identity (successor rebound
        // after an external unlink between our bind and our failure).
        let stale = (successor.sock_id.0, successor.sock_id.1.wrapping_add(1));
        cleanup_failed_acquire(sock_str, stale, true);
        assert!(
            std::path::Path::new(sock_str).exists(),
            "stale-identity cleanup must not unlink the live successor"
        );
        assert!(daemon_socket_present(sock_str));
        drop(successor);
        assert!(
            !std::path::Path::new(sock_str).exists(),
            "normal drop must still delete the socket pathname"
        );
        assert!(!daemon_socket_present(sock_str));
    }

    #[test]
    fn failed_acquire_cleanup_removes_own_entry() {
        // Positive side of the R2 policy: with exclusion held and a matching
        // identity, our own fresh entry is still removed (no leak).
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("own-entry.sock");
        let sock_str = sock.to_str().unwrap();
        let listener = UnixListener::bind(sock_str).unwrap();
        let id = filesystem_id(sock_str).unwrap();
        drop(listener);
        // Stale residue of a dead binder, identity still matches: reclaimable.
        cleanup_failed_acquire(sock_str, id, true);
        assert!(
            !std::path::Path::new(sock_str).exists(),
            "owned matching entry must be removed"
        );
    }

    #[test]
    fn degraded_acquire_drop_leaves_reclaimable_residue() {
        // End-to-end degraded mode: an unusable sidecar lockfile (here a
        // directory, so the lock open fails) still allows acquire, and both
        // failure-path and `Drop` cleanup skip the unlink without exclusion.
        // The next serialized acquire reclaims the residue.
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("degraded.sock");
        let sock_str = sock.to_str().unwrap().to_string();
        std::fs::create_dir(format!("{sock_str}.lock")).unwrap();
        let guard = SocketLock::acquire(&sock_str).unwrap();
        drop(guard);
        assert!(
            std::path::Path::new(&sock_str).exists(),
            "degraded drop must leave residue instead of unlinking without exclusion"
        );
        assert!(
            !daemon_socket_present(&sock_str),
            "dropped listener must not answer even though its pathname remains"
        );
        std::fs::remove_dir(format!("{sock_str}.lock")).unwrap();
        let successor = SocketLock::acquire(&sock_str).unwrap();
        assert!(daemon_socket_present(&sock_str));
        drop(successor);
        assert!(!std::path::Path::new(&sock_str).exists());
        assert!(!daemon_socket_present(&sock_str));
    }

    #[test]
    fn bounded_stop_under_queued_load() {
        // `Drop` must join promptly even with backlog queued: notify fires
        // before join, and the waiter exits instead of draining forever.
        // Generous 5s bound; failure reports via timeout, not a hung suite.
        use std::sync::mpsc;

        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("loaded-stop.sock");
        let sock_str = sock.to_str().unwrap().to_string();
        let guard = SocketLock::acquire(&sock_str).unwrap();
        // Queue pressure without stalling: nonblocking dials held open.
        let mut pending = Vec::new();
        for _ in 0..200 {
            match nonblocking_dial(&sock_str) {
                Ok(s) => pending.push(s),
                Err(_) => break,
            }
        }
        assert!(!pending.is_empty(), "must queue load for stop test");
        let (done_tx, done_rx) = mpsc::channel();
        std::thread::spawn(move || {
            drop(guard);
            let _ = done_tx.send(());
        });
        done_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("drop must join promptly under queued load");
        drop(pending);
    }

    #[test]
    fn shutdown_pipe_is_nonblocking_cloexec() {
        // Descriptor contract without touching rlimits: both ends must be
        // nonblocking (drainer `poll` + `Drop` write never block) and
        // close-on-exec (no leak into `reconnect`/helper children).
        use std::os::fd::AsRawFd;
        let (tx, rx) = shutdown_pipe().unwrap();
        for fd in [tx.as_raw_fd(), rx.as_raw_fd()] {
            // SAFETY: `fd` is a live owned pipe end for the duration.
            let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
            assert!(flags >= 0, "fcntl F_GETFL must succeed");
            assert_eq!(
                flags & libc::O_NONBLOCK,
                libc::O_NONBLOCK,
                "shutdown pipe ends must be nonblocking"
            );
            let fd_flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
            assert!(fd_flags >= 0, "fcntl F_GETFD must succeed");
            assert_eq!(
                fd_flags & libc::FD_CLOEXEC,
                libc::FD_CLOEXEC,
                "shutdown pipe ends must be CLOEXEC"
            );
        }
    }

    #[test]
    fn timeout_verdict_is_live_never_stale() {
        // Unit-pinned mapping for the timeout→live contract (a timed-out
        // probe implies a listener that is not draining). The kernel path is
        // covered separately by `saturated_queue_maps_to_timeout_live_verdict`
        // with a tiny backlog, without needing ~4k pending connects.
        assert!(present_from_probe(Ok(mock_stream())));
        assert!(present_from_probe(Err(io::Error::from(
            io::ErrorKind::TimedOut
        ))));
        for kind in [
            io::ErrorKind::ConnectionRefused,
            io::ErrorKind::NotFound,
            io::ErrorKind::PermissionDenied,
            io::ErrorKind::InvalidInput,
        ] {
            assert!(
                !present_from_probe(Err(io::Error::from(kind))),
                "{kind:?} must not read as live"
            );
        }
    }

    /// Connected UnixStream pair end for verdict-mapping tests (no path).
    fn mock_stream() -> UnixStream {
        UnixStream::pair().unwrap().0
    }

    #[test]
    fn stale_and_live_probe_kinds_are_distinct_and_fast() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("missing.sock");
        let missing = missing.to_str().unwrap();
        let start = Instant::now();
        assert!(!daemon_socket_present_timeout(
            missing,
            Duration::from_millis(200)
        ));
        assert!(start.elapsed() < Duration::from_secs(2));
        // Invalid paths fail fast rather than hanging.
        let err = connect_with_timeout("", Duration::from_millis(100)).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
    }

    /// Nonblocking dial that never stalls the test thread: returns the client
    /// end once the connect is queued (`0`) or in progress (`EINPROGRESS`);
    /// holding the fd keeps pressure on the server queue. A saturated queue
    /// fails with `EAGAIN`/`EWOULDBLOCK` without queueing anything, so that
    /// maps to `Err` (caller treats it as saturation, not a queued probe).
    fn nonblocking_dial(path: &str) -> io::Result<UnixStream> {
        use std::os::fd::{AsRawFd as _, FromRawFd, OwnedFd};
        let bytes = path.as_bytes();
        // SAFETY: test-only; raw fd is wrapped immediately below.
        let raw = unsafe {
            libc::socket(
                libc::AF_UNIX,
                libc::SOCK_STREAM | libc::SOCK_NONBLOCK | libc::SOCK_CLOEXEC,
                0,
            )
        };
        if raw < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: `socket` returned a newly owned fd.
        let fd: OwnedFd = unsafe { OwnedFd::from_raw_fd(raw) };
        // SAFETY: zeroed sockaddr_un with the nul-terminated path.
        let mut addr: libc::sockaddr_un = unsafe { std::mem::zeroed() };
        addr.sun_family = libc::AF_UNIX as libc::sa_family_t;
        for (i, b) in bytes.iter().enumerate() {
            addr.sun_path[i] = *b as libc::c_char;
        }
        let base = (&addr.sun_path as *const _ as usize) - (&addr as *const _ as usize);
        let len = base + bytes.len() + 1;
        // SAFETY: addr is a valid sockaddr_un of len bytes; fd is ours.
        let rc = unsafe {
            libc::connect(
                fd.as_raw_fd(),
                &addr as *const _ as *const libc::sockaddr,
                len as libc::socklen_t,
            )
        };
        if rc == 0 {
            return Ok(UnixStream::from(fd));
        }
        match io::Error::last_os_error().raw_os_error() {
            Some(code) if code == libc::EINPROGRESS => {
                // Dispatched; completion is in flight, fd stays ours.
                Ok(UnixStream::from(fd))
            }
            _ => Err(io::Error::last_os_error()),
        }
    }
}
