//! Single-instance Unix socket lock.
//!
//! Path: `/run/linux-trackpad-driver/driver.sock` — deliberately distinct
//! from the legacy `magic-trackpad-driver` socket so both generations can
//! coexist during migration (no migration handling: different path, no clash).
//!
//! Protocol: `bind()` wins. On `AddrInUse`, dial the existing socket: success
//! means a live daemon owns it (new instance exits); connection-refused means
//! a stale socket file (remove + rebind under a sidecar `*.lock` flock mutex,
//! then re-checking liveness once); any other dial failure (e.g. EACCES)
//! leaves the file untouched and bails. `Drop` unlinks only while the path
//! still names the filesystem entry our `bind` created (dev+ino captured at
//! acquisition, re-checked under a non-blocking try of the same mutex), so a
//! dying predecessor never removes a successor's rebound file.
//! Tests use temp dirs, never `/run`.

use std::os::unix::net::{UnixListener, UnixStream};
use std::path::Path;

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
pub struct SocketLock {
    path: String,
    /// Filesystem identity (dev, ino) of the socket directory entry created
    /// by our successful `bind`, captured at acquisition while holding the
    /// replacement mutex. `Drop` unlinks the path only while it still names
    /// this entry.
    sock_id: (u64, u64),
    _listener: UnixListener,
}

/// Short-lived cross-process mutex serializing the stale-check/remove/bind
/// sequence in [`SocketLock::acquire`]. Released (fd close drops the flock)
/// once acquisition succeeds or fails; it is not the daemon lock itself.
struct ReplacementMutex {
    _file: Option<std::fs::File>,
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

/// Take `LOCK_EX|LOCK_NB` on `{socket_path}.lock`, returning an unlocked
/// guard when it is contended or unusable. Used by `Drop`, which must never
/// block shutdown on a starter mid-acquire (whose own blocking dial can
/// stall). Callers fall back to the identity check without the mutex.
fn replacement_mutex_try(socket_path: &str) -> ReplacementMutex {
    ReplacementMutex {
        _file: open_lockfile(socket_path).and_then(|f| flock(&f, false).then_some(f)),
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
        let _replacement = replacement_mutex(path);
        // Up to two passes: bind, or stale-cleanup then re-bind with a fresh
        // liveness check (a parallel starter may have rebound between).
        for _ in 0..2 {
            match UnixListener::bind(path) {
                Ok(listener) => {
                    // Capture the filesystem entry our bind created while the
                    // mutex still excludes parallel starters, so Drop can
                    // later tell "our file" from a successor's rebound entry.
                    let sock_id = filesystem_id(path)?;
                    return Ok(Self {
                        path: path.to_string(),
                        sock_id,
                        _listener: listener,
                    });
                }
                Err(e) if e.kind() == std::io::ErrorKind::AddrInUse => {
                    // Stale file or live daemon? A live owner accepts connections.
                    match UnixStream::connect(path) {
                        Ok(_) => {
                            anyhow::bail!("daemon already running (socket '{path}' live)");
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
}

impl Drop for SocketLock {
    fn drop(&mut self) {
        use std::os::unix::fs::MetadataExt;
        // Serialize the identity check + unlink against a successor's
        // stale-check/remove/bind whenever the mutex is immediately
        // available, so a successor cannot rebind the path between our check
        // and our unlink. Bounded by construction: a single LOCK_NB attempt,
        // so shutdown never blocks on a starter mid-acquire (whose own
        // blocking dial can stall on an undrained listener). On contention
        // or an unusable lockfile, fall back to the identity check alone:
        // the listener fd stays open until after this runs, so a racing
        // starter still dials a live socket and backs off rather than
        // treating our file as stale.
        //
        // Unlink only while the path still names the filesystem entry our
        // bind created (dev+ino captured at acquisition). A successor that
        // rebound the path after an external unlink owns a new inode, so a
        // dying predecessor never removes the live file.
        //
        // The entry identity must come from the pathname, not from fstat of
        // the socket fd: on Linux the fd refers to a sockfs inode while the
        // pathname is a filesystem inode, so comparing the two never matches
        // and a normal drop would leak the file.
        let _mutex = replacement_mutex_try(&self.path);
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

/// True when a live daemon answers at `sock_path`. Used by `reconnect`:
/// absent socket without `-f` means disengage mode.
pub fn daemon_socket_present(sock_path: &str) -> bool {
    UnixStream::connect(sock_path).is_ok()
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
    fn drop_cleans_up_when_mutex_contended() {
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("drop-busy.sock");
        let sock = sock.to_str().unwrap().to_string();
        let guard = SocketLock::acquire(&sock).unwrap();
        // Pin the mutex so Drop must use its bounded fallback: it still has
        // to clean up, and must return without hanging.
        let _pinned = hold_replacement_lock(&sock);
        drop(guard);
        assert!(
            !std::path::Path::new(&sock).exists(),
            "drop under a contended mutex must still delete our own pathname"
        );
        assert!(!daemon_socket_present(&sock));
    }

    #[test]
    fn drop_vs_blocked_successor_single_owner() {
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("drop-race.sock");
        let sock = sock.to_str().unwrap().to_string();
        let pred = SocketLock::acquire(&sock).unwrap();
        // Pin the mutex so the successor blocks inside acquire.
        let pinned = hold_replacement_lock(&sock);
        let sock2 = sock.clone();
        let handle =
            std::thread::spawn(move || SocketLock::acquire(&sock2).map_err(|e| e.to_string()));
        // Give the successor time to block on the mutex, then drop the
        // predecessor: its Drop takes the bounded fallback (no hang) and
        // must release the pathname without harming the successor's bind.
        std::thread::sleep(std::time::Duration::from_millis(100));
        drop(pred);
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
}
