//! Packaging invariants: udev rules + systemd units.
//!
//! No hardware needed. Guards the coordinator contract:
//! add-only, hidraw, VID:PID-gated, systemd-handoff (never long RUN),
//! no systemd#37063 `ATTRS{idVendor}`-on-input matching, and the service
//! unit shape (simple / on-failure / RuntimeDirectory for driver.sock).

use std::collections::HashSet;
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

const RULES: &str = include_str!("../udev/99-linux-trackpad-driver.rules");
const SERVICE: &str = include_str!("../systemd/linux-trackpad-driver.service");
const RECONNECT: &str = include_str!("../systemd/linux-trackpad-driver-reconnect@.service");

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
