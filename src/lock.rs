//! Single-instance Unix socket lock.
//!
//! Path: `/run/linux-trackpad-driver/driver.sock` — deliberately distinct
//! from the legacy `magic-trackpad-driver` socket so both generations can
//! coexist during migration (no migration handling: different path, no clash).
//!
//! Protocol: `bind()` wins. On `AddrInUse`, probe the existing socket with a
//! bounded nonblocking connect: success or timeout means a live daemon owns
//! it (new instance exits); connection-refused means a stale socket file
//! (remove + rebind, tolerating a parallel starter that removed it first,
//! then re-checking liveness once); any other probe failure (e.g. EACCES)
//! leaves the file untouched and bails. `Drop` unlinks only when the path
//! still refers to our own socket (dev+ino match), so a dying predecessor
//! never removes a successor's rebound file.
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
/// How often the drain thread sleeps when the queue is empty.
const DRAIN_IDLE_SLEEP: Duration = Duration::from_millis(10);
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
    _listener: UnixListener,
    stop: Arc<AtomicBool>,
    drainer: Option<JoinHandle<()>>,
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
        // Up to two passes: bind, or stale-cleanup then re-bind with a fresh
        // liveness check (a parallel starter may have rebound between).
        for _ in 0..2 {
            match UnixListener::bind(path) {
                Ok(listener) => {
                    listener
                        .set_nonblocking(true)
                        .with_context(|| format!("setting nonblocking on socket '{path}'"))?;
                    let stop = Arc::new(AtomicBool::new(false));
                    let drain_listener = listener
                        .try_clone()
                        .with_context(|| format!("cloning socket '{path}' for drain"))?;
                    let drain_stop = Arc::clone(&stop);
                    let drainer = thread::Builder::new()
                        .name("ltd-sock-drain".to_string())
                        .spawn(move || drain_loop(drain_listener, drain_stop))
                        .with_context(|| format!("spawning drain thread for '{path}'"))?;
                    return Ok(Self {
                        path: path.to_string(),
                        _listener: listener,
                        stop,
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

    /// Accept and close all currently queued probe connections (nonblocking).
    /// Returns the number drained. The background thread calls this in a
    /// loop; tests call it directly to assert drain behavior.
    pub fn drain_pending(&self) -> usize {
        drain_once(&self._listener)
    }
}

/// Accept loop for the holder: dequeue probe connects and close them at
/// once so the kernel queue cannot fill and stall later probes.
fn drain_loop(listener: UnixListener, stop: Arc<AtomicBool>) {
    while !stop.load(Ordering::Relaxed) {
        let drained = drain_once(&listener);
        if drained == 0 {
            thread::sleep(DRAIN_IDLE_SLEEP);
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
pub(crate) fn connect_with_timeout(path: &str, timeout: Duration) -> io::Result<UnixStream> {
    use std::os::unix::io::FromRawFd;

    let bytes = path.as_bytes();
    if bytes.is_empty() || bytes.contains(&0) || bytes.len() > 107 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("invalid socket path '{path}'"),
        ));
    }
    // SAFETY: checked length above; CLOEXEC + NONBLOCK so dial never blocks.
    let fd = unsafe {
        libc::socket(
            libc::AF_UNIX,
            libc::SOCK_STREAM | libc::SOCK_NONBLOCK | libc::SOCK_CLOEXEC,
            0,
        )
    };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // Scope the raw fd so every early return closes it.
    let close_on_err = |fd: i32| unsafe {
        libc::close(fd);
    };

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
            fd,
            &addr as *const _ as *const libc::sockaddr,
            addr_len as libc::socklen_t,
        )
    };
    if ret == 0 {
        clear_nonblocking(fd);
        // SAFETY: connect succeeded, fd is a live stream socket we own.
        return Ok(unsafe { UnixStream::from_raw_fd(fd) });
    }
    let err = io::Error::last_os_error();
    let raw = err.raw_os_error();
    let in_progress = raw == Some(libc::EINPROGRESS)
        || raw == Some(libc::EAGAIN)
        || raw == Some(libc::EWOULDBLOCK);
    if !in_progress {
        close_on_err(fd);
        return Err(err);
    }
    let deadline = Instant::now() + timeout;
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        let timeout_ms = remaining.as_millis().min(i32::MAX as u128) as i32;
        let mut pfd = libc::pollfd {
            fd,
            events: libc::POLLOUT | libc::POLLERR | libc::POLLHUP,
            revents: 0,
        };
        // SAFETY: pfd points to one valid pollfd, timeout >= 0.
        let pr = unsafe { libc::poll(&mut pfd, 1, timeout_ms) };
        if pr < 0 {
            let e = io::Error::last_os_error();
            if e.kind() == io::ErrorKind::Interrupted {
                if Instant::now() >= deadline {
                    close_on_err(fd);
                    return Err(io::Error::from(io::ErrorKind::TimedOut));
                }
                continue;
            }
            close_on_err(fd);
            return Err(e);
        }
        if pr == 0 {
            close_on_err(fd);
            return Err(io::Error::from(io::ErrorKind::TimedOut));
        }
        let mut so_err: i32 = 0;
        let mut len: libc::socklen_t = std::mem::size_of::<i32>() as libc::socklen_t;
        // SAFETY: so_err/len are valid out-params for SO_ERROR.
        let gr = unsafe {
            libc::getsockopt(
                fd,
                libc::SOL_SOCKET,
                libc::SO_ERROR,
                &mut so_err as *mut _ as *mut libc::c_void,
                &mut len,
            )
        };
        if gr != 0 {
            let e = io::Error::last_os_error();
            close_on_err(fd);
            return Err(e);
        }
        if so_err != 0 {
            close_on_err(fd);
            // A completed-but-full queue surfaces here as EAGAIN/EWOULDBLOCK
            // on some kernels: report it as a timeout (live, busy) so
            // callers treat it as "live" rather than stale/deletable.
            if so_err == libc::EAGAIN || so_err == libc::EWOULDBLOCK {
                return Err(io::Error::from(io::ErrorKind::TimedOut));
            }
            return Err(io::Error::from_raw_os_error(so_err));
        }
        clear_nonblocking(fd);
        // SAFETY: SO_ERROR == 0 means the stream connected; we own fd.
        return Ok(unsafe { UnixStream::from_raw_fd(fd) });
    }
}

fn clear_nonblocking(fd: i32) {
    // Best effort: probes drop the stream immediately; a leftover O_NONBLOCK
    // would only matter to a caller that keeps it.
    // SAFETY: fd is a valid socket we own in this function.
    unsafe {
        let flags = libc::fcntl(fd, libc::F_GETFL);
        if flags >= 0 {
            let _ = libc::fcntl(fd, libc::F_SETFL, flags & !libc::O_NONBLOCK);
        }
    }
}

impl Drop for SocketLock {
    fn drop(&mut self) {
        use std::os::unix::fs::MetadataExt;
        use std::os::unix::io::AsRawFd;
        // Stop the drainer first so no cloned fd outlives our unlink check.
        self.stop.store(true, Ordering::Relaxed);
        if let Some(handle) = self.drainer.take() {
            let _ = handle.join();
        }
        // Unlink only if the path still refers to OUR socket (same dev+ino
        // as the bound listener fd). After a stale handoff a successor may
        // have rebound the path between our last use and this drop — its
        // live file must survive our teardown. (Our fd is still open here;
        // struct fields drop after this runs.)
        let ours = unsafe {
            let mut st: libc::stat = std::mem::zeroed();
            if libc::fstat(self._listener.as_raw_fd(), &mut st) == 0 {
                Some((st.st_dev as u64, st.st_ino as u64))
            } else {
                None
            }
        };
        let path_id = std::fs::metadata(&self.path)
            .ok()
            .map(|m| (m.dev(), m.ino()));
        match (ours, path_id) {
            (Some(a), Some(b)) if a == b => {
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

/// True when a live daemon answers at `sock_path`. Used by `reconnect`:
/// absent socket without `-f` means disengage mode. Bounded by
/// [`LIVENESS_TIMEOUT`]; a timed-out probe implies a live listener with a
/// full queue, so it reports present rather than hanging or misreading
/// a busy daemon as absent.
pub fn daemon_socket_present(sock_path: &str) -> bool {
    daemon_socket_present_timeout(sock_path, LIVENESS_TIMEOUT)
}

/// Bounded variant of [`daemon_socket_present`] for tests/callers that need
/// a custom budget.
pub fn daemon_socket_present_timeout(sock_path: &str, timeout: Duration) -> bool {
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
        drop(guard);
        assert!(!daemon_socket_present(sock));
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
    }

    #[test]
    fn repeated_probes_stay_bounded_with_drainer() {
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("drain.sock");
        let sock = sock.to_str().unwrap();
        let guard = SocketLock::acquire(sock).unwrap();
        let start = Instant::now();
        // Rapid sequential probes: each dials, the holder drains + closes.
        // Without a drainer the kernel queue would accumulate one entry per
        // probe; with it the queue stays empty and every probe stays fast.
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
    fn timeout_verdict_is_live_never_stale() {
        // The full-queue timeout path needs ~4k pending connects on this
        // host, so the timeout→live contract is pinned here directly: a
        // timed-out probe implies a listener that is not draining (live),
        // and must never be misread as absent or reclaimed as stale.
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
    /// end once the connect is dispatched (completion may still be in flight;
    /// holding the fd keeps pressure on the server queue).
    fn nonblocking_dial(path: &str) -> io::Result<UnixStream> {
        use std::os::unix::io::FromRawFd;
        let bytes = path.as_bytes();
        // SAFETY: test-only; fd is wrapped or closed on every path below.
        let fd = unsafe {
            libc::socket(
                libc::AF_UNIX,
                libc::SOCK_STREAM | libc::SOCK_NONBLOCK | libc::SOCK_CLOEXEC,
                0,
            )
        };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
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
                fd,
                &addr as *const _ as *const libc::sockaddr,
                len as libc::socklen_t,
            )
        };
        if rc == 0 {
            // SAFETY: connect succeeded; we own fd.
            return Ok(unsafe { UnixStream::from_raw_fd(fd) });
        }
        match io::Error::last_os_error().raw_os_error() {
            Some(code)
                if code == libc::EINPROGRESS
                    || code == libc::EAGAIN
                    || code == libc::EWOULDBLOCK =>
            {
                // SAFETY: dispatched; completion is in flight, fd stays ours.
                Ok(unsafe { UnixStream::from_raw_fd(fd) })
            }
            _ => {
                // SAFETY: connect failed synchronously; fd is ours to close.
                unsafe { libc::close(fd) };
                Err(io::Error::last_os_error())
            }
        }
    }
}
