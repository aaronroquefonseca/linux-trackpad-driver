//! Packaging invariants: udev rules + systemd units.
//!
//! No hardware needed. Guards the coordinator contract:
//! add-only, hidraw, VID:PID-gated, systemd-handoff (never long RUN),
//! no systemd#37063 `ATTRS{idVendor}`-on-input matching, and the service
//! unit shape (simple / on-failure / RuntimeDirectory for driver.sock).

use std::collections::HashSet;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;

fn repo_file(rel: &str) -> String {
    let p: PathBuf = [env!("CARGO_MANIFEST_DIR"), rel].iter().collect();
    std::fs::read_to_string(&p).unwrap_or_else(|e| panic!("read {}: {e}", p.display()))
}

fn rule_lines(rules: &str) -> Vec<&str> {
    rules
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .collect()
}

/// Non-comment lines of a script/unit: prohibitions may be named in comments,
/// but only code lines can touch user sessions.
fn code_lines(text: &str) -> String {
    text.lines()
        .filter(|l| !l.trim_start().starts_with('#'))
        .collect::<Vec<_>>()
        .join("\n")
}

const RULES: &str = include_str!("../udev/99-linux-trackpad-driver.rules");
const SERVICE: &str = include_str!("../systemd/linux-trackpad-driver.service");
const RECONNECT: &str = include_str!("../systemd/linux-trackpad-driver-reconnect@.service");
const RESUME_UNIT: &str = include_str!("../systemd/linux-trackpad-driver-resume.service");
const SLEEP_HOOK: &str = include_str!("../systemd/system-sleep/ltd-resume");

const COMBOS: [(&str, &str); 4] = [
    ("05ac", "0265"),
    ("05ac", "0324"),
    ("004c", "0265"),
    ("004c", "0324"),
];

#[test]
fn rules_cover_all_vid_pid_combos_on_hidraw_add() {
    // Files referenced must be the ones shipped (include! would fail to
    // compile otherwise); also assert they exist on disk for clarity.
    assert!(!repo_file("udev/99-linux-trackpad-driver.rules").is_empty());
    let lines = rule_lines(RULES);
    assert!(!lines.is_empty(), "rules file has no active lines");
    let mut covered = HashSet::new();
    for line in &lines {
        assert!(
            line.contains(r#"ACTION=="add""#),
            "rule must be add-only: {line}"
        );
        assert!(
            line.contains(r#"SUBSYSTEM=="hidraw""#),
            "rule must match hidraw: {line}"
        );
        for (vid, pid) in COMBOS {
            if line.contains(&format!(r#"ATTRS{{idVendor}}=="{vid}""#))
                && line.contains(&format!(r#"ATTRS{{idProduct}}=="{pid}""#))
            {
                covered.insert((vid, pid));
            }
        }
    }
    for combo in COMBOS {
        assert!(covered.contains(&combo), "uncovered VID:PID {combo:?}");
    }
}

#[test]
fn rules_hand_off_to_systemd_without_long_run() {
    for line in rule_lines(RULES) {
        assert!(
            line.contains(r#"TAG+="systemd""#),
            "rule must tag for systemd: {line}"
        );
        assert!(
            line.contains("SYSTEMD_WANTS"),
            "rule must pull the reconnect unit: {line}"
        );
        assert!(
            line.contains("linux-trackpad-driver-reconnect@%k.service"),
            "SYSTEMD_WANTS must target the reconnect template: {line}"
        );
        assert!(
            !line.contains("RUN+="),
            "never long-run inside udev RUN: {line}"
        );
    }
}

#[test]
fn rules_avoid_input_idvendor_trap() {
    for line in rule_lines(RULES) {
        // systemd#37063: ATTRS{idVendor} never matches virtual input nodes.
        // No input-subsystem matching belongs here at all.
        assert!(
            !line.contains(r#"SUBSYSTEM=="input""#),
            "no input-node matching in this file: {line}"
        );
    }
}

#[test]
fn service_unit_shape() {
    assert!(
        SERVICE.contains("Type=simple"),
        "service must be Type=simple"
    );
    assert!(
        SERVICE.contains("Restart=on-failure"),
        "service must restart on failure"
    );
    assert!(
        SERVICE.contains("RuntimeDirectory=linux-trackpad-driver"),
        "RuntimeDirectory must create the socket dir for driver.sock"
    );
    assert!(
        SERVICE.contains("ExecStart=/usr/bin/ltd run"),
        "service must exec the daemon entry"
    );
}

#[test]
fn reconnect_template_runs_reconnect_for_instance() {
    assert!(
        RECONNECT.contains("Type=oneshot"),
        "reconnect template must be oneshot"
    );
    assert!(
        RECONNECT.contains("ExecStart=/usr/bin/ltd reconnect /dev/%I"),
        "template must run reconnect for the udev instance"
    );
    assert!(
        RECONNECT.contains("LTD_CONFIG="),
        "template must pass the system config env so hotplug honors enhanced_clicks"
    );
}

#[test]
fn resume_unit_shape() {
    assert!(
        RESUME_UNIT.contains("Type=oneshot"),
        "resume unit must be oneshot"
    );
    assert!(
        RESUME_UNIT.contains("ExecStart=/usr/bin/ltd resume"),
        "resume unit must run the resume entry (multitouch-first re-init)"
    );
    // The After= line itself must carry all four sleep targets — a bare
    // contains() could match a comment instead.
    let after = RESUME_UNIT
        .lines()
        .find(|l| l.trim_start().starts_with("After="))
        .expect("resume unit needs an After= line");
    for target in [
        "suspend.target",
        "hibernate.target",
        "hybrid-sleep.target",
        "suspend-then-hibernate.target",
    ] {
        assert!(
            after.contains(target),
            "After= line must order {target}: {after}"
        );
    }
    assert!(
        RESUME_UNIT.contains("LTD_CONFIG="),
        "resume unit must pass the system config env like the reconnect template"
    );
}

#[test]
fn resume_unit_never_runs_pre_sleep() {
    // WantedBy=sleep targets would run BEFORE suspend — the wrong direction
    // for re-init. Post-resume activation comes from the system-sleep hook
    // (post branch); this unit must not bind to sleep targets.
    for target in [
        "WantedBy=suspend.target",
        "WantedBy=hibernate.target",
        "WantedBy=hybrid-sleep.target",
        "WantedBy=suspend-then-hibernate.target",
        "WantedBy=sleep.target",
    ] {
        assert!(
            !RESUME_UNIT.contains(target),
            "resume unit must not bind to sleep targets (pre-sleep direction): {target}"
        );
    }
}

#[test]
fn sleep_hook_reinits_on_post_only() {
    assert!(
        SLEEP_HOOK.starts_with("#!"),
        "sleep hook must be an executable script with a shebang"
    );
    // Code lines only: the header comment names pre|post for documentation,
    // which must not satisfy this test.
    let hook_code = code_lines(SLEEP_HOOK);
    // The case arm itself must match post for every sleep type.
    let arm = hook_code
        .lines()
        .find(|l| l.contains("post "))
        .expect("sleep hook needs a post case arm");
    for sleep in [
        "post suspend",
        "post hibernate",
        "post hybrid-sleep",
        "post suspend-then-hibernate",
    ] {
        assert!(arm.contains(sleep), "post arm must cover {sleep}: {arm}");
    }
    // Ordering: the post arm precedes the resume invocation, so re-init
    // runs after resume — never on the pre branch.
    let post = hook_code.find("\"post ").expect("post arm");
    let resume = hook_code.find("\"$LTD_BIN\" resume").expect("resume call");
    assert!(
        post < resume,
        "resume invocation must sit under the post arm (after resume, not pre)"
    );
}

#[test]
fn sleep_hook_leaves_journal_trace_and_recovery() {
    // (d) hook failures were swallowed by `|| echo` + unconditional
    // exit 0 with no trace the hook ran at all. The post arm must log
    // entry, success, and failure-with-rc, and the failure line must name
    // the exact recovery command — else a dead resume is undebuggable.
    let hook_code = code_lines(SLEEP_HOOK);
    assert!(
        hook_code.contains("running"),
        "hook must log entry when the post arm fires"
    );
    assert!(
        hook_code.contains("rc=") || hook_code.contains("rc=$?"),
        "hook must capture the ltd resume exit code on failure"
    );
    assert!(
        SLEEP_HOOK.contains("systemctl start linux-trackpad-driver-resume.service"),
        "hook failure line must name the resume unit recovery command"
    );
    assert!(
        SLEEP_HOOK.contains("ltd resume"),
        "hook failure line must name ltd resume"
    );
}

#[test]
fn sleep_hook_is_executable() {
    let p: PathBuf = [
        env!("CARGO_MANIFEST_DIR"),
        "systemd/system-sleep/ltd-resume",
    ]
    .iter()
    .collect();
    let mode = std::fs::metadata(&p)
        .unwrap_or_else(|e| panic!("stat {}: {e}", p.display()))
        .permissions()
        .mode();
    assert!(
        mode & 0o111 != 0,
        "sleep hook must be executable (systemd ignores non-executable hooks)"
    );
}

#[test]
fn ltd_config_default_path_parity() {
    // Daemon, hook, and tick must verify against the same rate: all four
    // shipped files must agree on the system config path. (A hook at
    // defaults vs. a daemon at /etc rate = perpetual mismatch re-init.)
    const PATH: &str = "/etc/linux-trackpad-driver/config.toml";
    for (name, text) in [
        ("sleep hook", SLEEP_HOOK),
        ("main unit", SERVICE),
        ("reconnect template", RECONNECT),
        ("resume unit", RESUME_UNIT),
    ] {
        assert!(text.contains(PATH), "{name} must reference {PATH}");
    }
    // Reference is not enough for the hook: sleep hooks inherit PID1's env,
    // so the default must be set and exported in code, not just documented.
    let hook_code = code_lines(SLEEP_HOOK);
    assert!(
        hook_code.contains("LTD_CONFIG=") && hook_code.contains("export LTD_CONFIG"),
        "sleep hook must set and export LTD_CONFIG in code"
    );
}

#[test]
fn sleep_hook_and_resume_unit_never_touch_user_sessions() {
    // user.slice is frozen while sleep hooks run (RESEARCH §6): any of these
    // in *code* (non-comment lines) would block on, or signal into, a frozen
    // user session. Comments may name them only to prohibit them.
    let hook_code = code_lines(SLEEP_HOOK);
    let unit_code = code_lines(RESUME_UNIT);
    for banned in [
        "systemctl --user",
        "--user ",
        "machinectl",
        "notify-send",
        "dbus-send",
        "runuser",
        "sudo -u",
        "su -",
        "DISPLAY=",
    ] {
        assert!(
            !hook_code.contains(banned),
            "sleep hook must not touch user sessions: {banned:?}"
        );
        assert!(
            !unit_code.contains(banned),
            "resume unit must not touch user sessions: {banned:?}"
        );
    }
}
