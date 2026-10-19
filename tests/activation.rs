//! Finding #14 lifecycle: installed-but-disabled is off.
//!
//! End-to-end through the real binary with an isolated `LTD_SOCKET` (per
//! child-process env, so no process-env races): with no live daemon the
//! automatic background forms (`reconnect` without `-f`, plain `resume`)
//! exit 0 without touching hardware, while explicit forcing still validates
//! and fails closed. Static install/upgrade/remove posture (no trigger, no
//! enable/start) is pinned in `tests/packaging.rs`.

use std::path::PathBuf;
use std::process::Command;

fn ltd() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_ltd"))
}

fn run(args: &[&str], socket: &std::path::Path) -> std::process::Output {
    Command::new(ltd())
        .args(args)
        .env("LTD_SOCKET", socket)
        .output()
        .expect("spawn ltd")
}

fn absent_socket(dir: &tempfile::TempDir) -> PathBuf {
    // No listener bound: the disabled/stopped-daemon state.
    dir.path().join("no-daemon.sock")
}

#[test]
fn disabled_background_reconnect_leaves_hardware_alone() {
    let dir = tempfile::tempdir().unwrap();
    let sock = absent_socket(&dir);
    // Even a bogus node exits 0: the inactive no-op returns before any
    // node wait, scan, or feature-report write.
    for args in [
        vec!["reconnect", "/dev/does-not-exist-anywhere"],
        vec!["reconnect", "/dev/null"],
    ] {
        let out = run(&args, &sock);
        assert_eq!(out.status.code(), Some(0), "{args:?}: {out:?}");
        assert!(
            String::from_utf8_lossy(&out.stderr).contains("inactive: leaving hardware alone"),
            "{args:?}: expected the inactive no-op marker: {out:?}"
        );
    }
}

#[test]
fn disabled_background_resume_leaves_hardware_alone() {
    let dir = tempfile::tempdir().unwrap();
    let sock = absent_socket(&dir);
    // Sleep-hook / resume-unit form: exit 0, no rescan, no re-init.
    let out = run(&["resume"], &sock);
    assert_eq!(out.status.code(), Some(0), "{out:?}");
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("inactive: leaving hardware alone"),
        "expected the inactive no-op marker: {out:?}"
    );
}

#[test]
fn stopped_daemon_manual_forms_still_validate_and_fail_closed() {
    let dir = tempfile::tempdir().unwrap();
    let sock = absent_socket(&dir);
    // Validation runs before the activation gate: bad rates always fail,
    // even when the call would otherwise be an inactive no-op.
    for args in [
        vec!["resume", "-p", "0"],
        vec!["resume", "-f", "-p", "0"],
        vec!["reconnect", "/dev/null", "-p", "0"],
    ] {
        let out = run(&args, &sock);
        assert_eq!(out.status.code(), Some(1), "{args:?}: {out:?}");
    }
    // Explicit forcing without a daemon proceeds past the gate and then
    // fails closed on a non-trackpad node (/dev/null exists, so no node
    // wait; classify rejects it without any init writes).
    let out = run(&["reconnect", "-f", "/dev/null"], &sock);
    assert_eq!(out.status.code(), Some(1), "{out:?}");
    // Forced resume proceeds past the gate into a real hardware pass: the
    // exit code is environment-dependent (0 with no devices, 1 on per-node
    // errors), so assert the gate marker instead of the code.
    let out = run(&["resume", "-f"], &sock);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("but -f given; re-initing hardware directly"),
        "forced resume must proceed past the activation gate: {out:?}"
    );
    assert!(
        !stderr.contains("inactive: leaving hardware alone"),
        "forced resume must not take the inactive path: {out:?}"
    );
}

#[test]
fn live_daemon_background_reconnect_proceeds_past_gate() {
    use std::os::unix::net::UnixListener;
    let dir = tempfile::tempdir().unwrap();
    let sock = dir.path().join("live-daemon.sock");
    let _listener = UnixListener::bind(&sock).unwrap();
    // Daemon live: the automatic form proceeds past the gate, then fails
    // closed on the non-trackpad node — proving the gate (not the
    // classify) is what kept the disabled case quiet.
    let out = run(&["reconnect", "/dev/null"], &sock);
    assert_eq!(out.status.code(), Some(1), "{out:?}");
    // Explicit disengage also proceeds while live.
    let out = run(&["reconnect", "-d", "/dev/null"], &sock);
    assert_eq!(out.status.code(), Some(1), "{out:?}");
}
