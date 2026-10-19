//! Finding #14 lifecycle: installed-but-disabled is off.
//!
//! End-to-end through the real binary with hermetic child-process env (per
//! child, so no process-env races):
//! - `LTD_SOCKET` isolates the live-daemon probe (absent = stopped).
//! - `LTD_HIDRAW_SYSFS` isolates device discovery to an empty temp dir, so
//!   forced paths can never discover or open host hardware even on a
//!   developer machine with a real trackpad attached.
//! - `LTD_CONFIG` points at a missing file so the binary falls back to
//!   built-in defaults deterministically.
//!
//! With no live daemon the automatic background forms (`reconnect` without
//! `-f`, plain `resume`) exit 0 without touching hardware, while explicit
//! forcing still parses device-first, proceeds past the gate, and fails
//! closed. Static install/upgrade/remove posture (no trigger, no
//! enable/start) is pinned in `tests/packaging.rs`.

use std::path::{Path, PathBuf};
use std::process::Command;

fn ltd() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_ltd"))
}

/// Hermetic child env: isolated socket probe + empty sysfs discovery root +
/// missing config (defaults). Children inherit isolation via env, so even
/// `resume -f` can never reach host `/sys/class/hidraw` or host devices.
fn run(args: &[&str], socket: &Path, sysfs: &Path, config_missing: &Path) -> std::process::Output {
    Command::new(ltd())
        .args(args)
        .env("LTD_SOCKET", socket)
        .env("LTD_HIDRAW_SYSFS", sysfs)
        .env("LTD_CONFIG", config_missing)
        .output()
        .expect("spawn ltd")
}

struct Iso {
    _dir: tempfile::TempDir,
    sock: PathBuf,
    sysfs: PathBuf,
    cfg_missing: PathBuf,
}

fn absent_iso() -> Iso {
    // No listener bound: the disabled/stopped-daemon state. Empty sysfs:
    // no hidraw entries, so discovery is deterministically empty and no
    // host node can be opened even past the gate.
    let dir = tempfile::tempdir().unwrap();
    let sock = dir.path().join("no-daemon.sock");
    let sysfs = dir.path().join("sys/class/hidraw");
    std::fs::create_dir_all(&sysfs).unwrap();
    let cfg_missing = dir.path().join("missing.toml");
    Iso {
        _dir: dir,
        sock,
        sysfs,
        cfg_missing,
    }
}

fn live_iso() -> (Iso, std::os::unix::net::UnixListener) {
    let dir = tempfile::tempdir().unwrap();
    let sock = dir.path().join("live-daemon.sock");
    let sysfs = dir.path().join("sys/class/hidraw");
    std::fs::create_dir_all(&sysfs).unwrap();
    let cfg_missing = dir.path().join("missing.toml");
    let listener = std::os::unix::net::UnixListener::bind(&sock).unwrap();
    (
        Iso {
            _dir: dir,
            sock,
            sysfs,
            cfg_missing,
        },
        listener,
    )
}

#[test]
fn disabled_background_reconnect_leaves_hardware_alone() {
    let iso = absent_iso();
    // Even a bogus node exits 0: the inactive no-op returns before any
    // node wait, scan, or feature-report write.
    for args in [
        vec!["reconnect", "/dev/does-not-exist-anywhere"],
        vec!["reconnect", "/dev/null"],
    ] {
        let out = run(&args, &iso.sock, &iso.sysfs, &iso.cfg_missing);
        assert_eq!(out.status.code(), Some(0), "{args:?}: {out:?}");
        assert!(
            String::from_utf8_lossy(&out.stderr).contains("inactive: leaving hardware alone"),
            "{args:?}: expected the inactive no-op marker: {out:?}"
        );
    }
}

#[test]
fn disabled_background_resume_leaves_hardware_alone() {
    let iso = absent_iso();
    // Sleep-hook / resume-unit form: exit 0, no rescan, no re-init.
    let out = run(&["resume"], &iso.sock, &iso.sysfs, &iso.cfg_missing);
    assert_eq!(out.status.code(), Some(0), "{out:?}");
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("inactive: leaving hardware alone"),
        "expected the inactive no-op marker: {out:?}"
    );
}

#[test]
fn stopped_daemon_manual_forms_still_validate_and_fail_closed() {
    let iso = absent_iso();
    // Validation runs before the activation gate: bad rates always fail,
    // even when the call would otherwise be an inactive no-op.
    for args in [
        vec!["resume", "-p", "0"],
        vec!["resume", "-f", "-p", "0"],
        vec!["reconnect", "/dev/null", "-p", "0"],
    ] {
        let out = run(&args, &iso.sock, &iso.sysfs, &iso.cfg_missing);
        assert_eq!(out.status.code(), Some(1), "{args:?}: {out:?}");
    }
    // Explicit forcing without a daemon proceeds past the gate and then
    // fails closed on a non-trackpad node (/dev/null exists, so no node
    // wait; empty isolated sysfs means classify rejects it without any
    // init writes). Device-first ordering is required by the parser:
    // a parse error (`unknown option`) must never satisfy this test.
    let out = run(
        &["reconnect", "/dev/null", "-f"],
        &iso.sock,
        &iso.sysfs,
        &iso.cfg_missing,
    );
    assert_eq!(out.status.code(), Some(1), "{out:?}");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        !stderr.contains("unknown option"),
        "forced reconnect must parse (device-first), not fail on flags: {out:?}"
    );
    assert!(
        stderr.contains("proceeding with init"),
        "forced reconnect must proceed past the activation gate: {out:?}"
    );
    assert!(
        stderr.contains("not a recognized trackpad"),
        "forced reconnect must fail closed on classification: {out:?}"
    );
    assert!(
        !stderr.contains("inactive: leaving hardware alone"),
        "forced reconnect must not take the inactive path: {out:?}"
    );
    // Forced resume proceeds past the gate with hermetic empty discovery:
    // no host nodes are visible, so the pass does zero hardware writes and
    // exits 0. The gate marker (not a parse error or exit code alone)
    // proves the force path ran.
    let out = run(&["resume", "-f"], &iso.sock, &iso.sysfs, &iso.cfg_missing);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(0), "{out:?}");
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
    let (iso, _listener) = live_iso();
    // Daemon live: the automatic form proceeds past the gate, then fails
    // closed on the non-trackpad node — proving the gate (not the
    // classify) is what kept the disabled case quiet. Empty isolated
    // sysfs keeps this hermetic: no host node can satisfy classify.
    let out = run(
        &["reconnect", "/dev/null"],
        &iso.sock,
        &iso.sysfs,
        &iso.cfg_missing,
    );
    assert_eq!(out.status.code(), Some(1), "{out:?}");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("not a recognized trackpad"),
        "live reconnect must fail closed on classification: {out:?}"
    );
    assert!(
        !stderr.contains("inactive: leaving hardware alone"),
        "live reconnect must not take the inactive path: {out:?}"
    );
    // Explicit disengage (device-first) also proceeds while live, then
    // fails closed on the same non-trackpad node before any write.
    let out = run(
        &["reconnect", "/dev/null", "-d"],
        &iso.sock,
        &iso.sysfs,
        &iso.cfg_missing,
    );
    assert_eq!(out.status.code(), Some(1), "{out:?}");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        !stderr.contains("unknown option"),
        "disengage must parse (device-first), not fail on flags: {out:?}"
    );
    assert!(
        stderr.contains("disengage requested"),
        "live disengage must proceed past the gate: {out:?}"
    );
    assert!(
        !stderr.contains("inactive: leaving hardware alone"),
        "live disengage must not take the inactive path: {out:?}"
    );
}
