//! Test-only serialization for process-env access.
//!
//! `std::env::set_var` racing with any concurrent env read is unsound, and
//! the Unix test harness runs tests threaded. Every test that mutates or
//! reads process env (`LTD_CONFIG`, `LTD_SOCKET`) must hold
//! [`lock_env`] for the whole critical section so the previously-flaky
//! config/lock pair cannot interleave.

use std::sync::{Mutex, MutexGuard, OnceLock};

static ENV_LOCK: OnceLock<Mutex<()>> = OnceLock::new();

/// Hold the returned guard for the entire env-touching section.
pub(crate) fn lock_env() -> MutexGuard<'static, ()> {
    ENV_LOCK
        .get_or_init(|| Mutex::new(()))
        .lock()
        .expect("env test lock")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn env_touching_pair_stays_serialized_under_threads() {
        // Regression for the parallel-env flake: the config (`LTD_CONFIG`)
        // and socket (`LTD_SOCKET`) scenarios previously raced when the
        // harness ran them on separate threads. Hammer the pair concurrently;
        // each critical section holds the shared lock, so every read-back
        // must see its own sentinel.
        for _ in 0..50 {
            std::thread::scope(|s| {
                let config_side = s.spawn(|| {
                    let _guard = lock_env();
                    let old = std::env::var(crate::config::CONFIG_ENV).ok();
                    let sentinel = "locked-config-sentinel";
                    unsafe { std::env::set_var(crate::config::CONFIG_ENV, sentinel) };
                    let seen = crate::config::config_path_from_env();
                    match old {
                        Some(v) => unsafe { std::env::set_var(crate::config::CONFIG_ENV, v) },
                        None => unsafe { std::env::remove_var(crate::config::CONFIG_ENV) },
                    }
                    assert_eq!(seen.as_deref(), Some(sentinel));
                });
                let socket_side = s.spawn(|| {
                    let _guard = lock_env();
                    let old = std::env::var(crate::lock::SOCKET_ENV).ok();
                    let sentinel = "/tmp/locked-socket-sentinel.sock";
                    unsafe { std::env::set_var(crate::lock::SOCKET_ENV, sentinel) };
                    let seen = crate::lock::socket_path();
                    match old {
                        Some(v) => unsafe { std::env::set_var(crate::lock::SOCKET_ENV, v) },
                        None => unsafe { std::env::remove_var(crate::lock::SOCKET_ENV) },
                    }
                    assert_eq!(seen, sentinel);
                });
                config_side.join().unwrap();
                socket_side.join().unwrap();
            });
        }
    }
}
