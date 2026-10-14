//! Single-instance Unix socket lock.
//!
//! Path: `/run/linux-trackpad-driver/driver.sock` — deliberately distinct
//! from the legacy `magic-trackpad-driver` socket so both generations can
//! coexist during migration (no migration handling: different path, no clash).
//!
//! Protocol: `bind()` wins. On `AddrInUse`, dial the existing socket: success
//! means a live daemon owns it (new instance exits); refusal means a stale
//! socket file (remove + rebind). Tests use temp dirs, never `/run`.

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

/// Effective socket path: `$LTD_SOCKET` when set, else [`DRIVER_SOCK`].
pub fn socket_path() -> String {
    std::env::var(SOCKET_ENV).unwrap_or_else(|_| DRIVER_SOCK.to_string())
}

/// Guard holding the bound socket; drops (and removes the file) on shutdown.
pub struct SocketLock {
    path: String,
    _listener: UnixListener,
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
        match UnixListener::bind(path) {
            Ok(listener) => Ok(Self {
                path: path.to_string(),
                _listener: listener,
            }),
            Err(e) if e.kind() == std::io::ErrorKind::AddrInUse => {
                // Stale file or live daemon? A live owner accepts connections.
                match UnixStream::connect(path) {
                    Ok(_) => anyhow::bail!("daemon already running (socket '{path}' live)"),
                    Err(_) => {
                        log::warn!("removing stale socket '{path}'");
                        std::fs::remove_file(path)
                            .with_context(|| format!("removing stale socket '{path}'"))?;
                        let listener = UnixListener::bind(path).with_context(|| {
                            format!("binding socket '{path}' after stale cleanup")
                        })?;
                        Ok(Self {
                            path: path.to_string(),
                            _listener: listener,
                        })
                    }
                }
            }
            Err(e) => Err(e).with_context(|| format!("binding socket '{path}'")),
        }
    }

    pub fn path(&self) -> &str {
        &self.path
    }
}

impl Drop for SocketLock {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
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
        // SAFETY: single-threaded test mutating process env for itself.
        unsafe { std::env::remove_var(SOCKET_ENV) };
        assert_eq!(socket_path(), DRIVER_SOCK);
        unsafe { std::env::set_var(SOCKET_ENV, "/tmp/test-ltd.sock") };
        assert_eq!(socket_path(), "/tmp/test-ltd.sock");
        unsafe { std::env::remove_var(SOCKET_ENV) };
    }
}
