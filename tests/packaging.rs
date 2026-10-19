//! Packaging invariants: udev rules + systemd units.
//!
//! No hardware needed. Guards the coordinator contract:
//! add-only, hidraw, transport-correct identity (USB ATTRS-gated, BT
//! KERNELS-gated), systemd-handoff (never long RUN),
//! no systemd#37063 `ATTRS{idVendor}`-on-input matching, and the service
//! unit shape (simple / on-failure / RuntimeDirectory for driver.sock).
//!
//! Finding #8: string asserts prove the rules *mention* an identity, not that
//! a Bluetooth hidraw node *matches*. The fixture harness below parses each
//! rule line and evaluates it against captured USB/Bluetooth device trees.

use std::collections::{HashMap, HashSet};
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

const USB_COMBOS: [(&str, &str); 2] = [("05ac", "0265"), ("05ac", "0324")];

/// BT HID parent kernel-name patterns (bus `0005`, uppercase `%04X` VID,
/// both VIDs the daemon accepts over BT, both PIDs).
const BT_KERNELS: [&str; 4] = [
    "0005:004C:0265.*",
    "0005:004C:0324.*",
    "0005:05AC:0265.*",
    "0005:05AC:0324.*",
];

/// One device in a captured tree: the hidraw node or one ancestor.
#[derive(Debug, Clone)]
struct FakeDev {
    kernel: String,
    subsystem: String,
    attrs: HashMap<String, String>,
}

/// A captured `add` event: target node + ancestors, immediate parent first.
#[derive(Debug, Clone)]
struct FakeEvent {
    action: String,
    node: FakeDev,
    parents: Vec<FakeDev>,
}

fn dev(kernel: &str, subsystem: &str, attrs: &[(&str, &str)]) -> FakeDev {
    FakeDev {
        kernel: kernel.to_string(),
        subsystem: subsystem.to_string(),
        attrs: attrs
            .iter()
            .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
            .collect(),
    }
}

/// One `==` match condition parsed from a rule line.
struct Cond {
    key: String,
    attr: Option<String>,
    value: String,
}

/// Strict match values used by these rules: exact, or one trailing `*`
/// prefix match (e.g. `KERNELS=="0005:004C:0324.*"`). Case-sensitive like
/// udev `==` (hence uppercase `004C` in `KERNELS` vs lowercase sysfs
/// `ATTRS{idVendor}`).
fn strict_match(pattern: &str, value: &str) -> bool {
    match pattern.strip_suffix('*') {
        Some(prefix) => value.starts_with(prefix),
        None => pattern == value,
    }
}

fn validate_pattern(value: &str) -> Result<(), String> {
    if value.is_empty() {
        return Err("empty match value".to_string());
    }
    for bad in ['|', '?', '[', ']'] {
        if value.contains(bad) {
            return Err(format!("unsupported pattern {value:?}: `{bad}`"));
        }
    }
    if value.contains('*') {
        let stars = value.bytes().filter(|&b| b == b'*').count();
        if stars != 1 || !value.ends_with('*') {
            return Err(format!(
                "unsupported pattern {value:?}: only a single trailing `*`"
            ));
        }
    }
    Ok(())
}

/// Parse the match (`==`) conditions of a rule line. Assignment tokens
/// (`=`, `+=`, `:=`, `-=` — `TAG+=`, `ENV{SYSTEMD_WANTS}+=`) never gate
/// matching and are skipped; anything else without `==`/`!=` is malformed.
/// Only `ACTION`, `SUBSYSTEM`, `KERNELS`, `ATTRS` with `==` and strict
/// patterns are accepted; everything else is an explicit error so future
/// rule edits cannot appear covered while the model ignores them.
fn parse_match_conds(line: &str) -> Result<Vec<Cond>, String> {
    let mut out = Vec::new();
    for token in line.split(',') {
        let token = token.trim();
        if token.is_empty() {
            continue;
        }
        if !token.contains("==") && !token.contains("!=") {
            if token.contains('=') {
                continue; // assignment (`=`, `+=`, `:=`, `-=`): not a match key
            }
            return Err(format!("malformed token without operator: {token:?}"));
        }
        if token.contains("!=") {
            return Err(format!(
                "unsupported match operator `!=` (only `==`): {token:?}"
            ));
        }
        let i = token.find("==").expect("checked for ==");
        let key = token[..i].trim();
        let mut value = token[i + 2..].trim();
        value = value.strip_prefix('"').unwrap_or(value);
        value = value.strip_suffix('"').unwrap_or(value);
        let (key, attr) = match key.find('{') {
            Some(i) => (
                key[..i].trim().to_string(),
                Some(key[i + 1..].trim_end_matches('}').trim().to_string()),
            ),
            None => (key.to_string(), None),
        };
        match key.as_str() {
            "ACTION" | "SUBSYSTEM" | "KERNELS" => {
                if attr.is_some() {
                    return Err(format!("unsupported braces on {key:?}: {token:?}"));
                }
                validate_pattern(value).map_err(|e| format!("{key}: {e}: {token:?}"))?;
                out.push(Cond {
                    key,
                    attr: None,
                    value: value.to_string(),
                });
            }
            "ATTRS" => {
                let Some(attr) = attr.filter(|a| !a.is_empty()) else {
                    return Err(format!("ATTRS needs {{attr}}: {token:?}"));
                };
                validate_pattern(value).map_err(|e| format!("ATTRS: {e}: {token:?}"))?;
                out.push(Cond {
                    key,
                    attr: Some(attr),
                    value: value.to_string(),
                });
            }
            _ => {
                return Err(format!(
                    "unsupported match key {key:?} (only ACTION, SUBSYSTEM, KERNELS, ATTRS): {token:?}"
                ));
            }
        }
    }
    Ok(out)
}

fn dev_key_matches(cond: &Cond, d: &FakeDev, action: &str) -> bool {
    match cond.key.as_str() {
        "ACTION" => strict_match(&cond.value, action),
        "SUBSYSTEM" => strict_match(&cond.value, &d.subsystem),
        _ => panic!("device key must be ACTION or SUBSYSTEM: {}", cond.key),
    }
}

fn parent_key_matches(cond: &Cond, p: &FakeDev) -> bool {
    match cond.key.as_str() {
        "KERNELS" => strict_match(&cond.value, &p.kernel),
        "ATTRS" => cond
            .attr
            .as_ref()
            .and_then(|a| p.attrs.get(a))
            .is_some_and(|v| strict_match(&cond.value, v)),
        _ => panic!("parent key must be KERNELS or ATTRS: {}", cond.key),
    }
}

/// udev matching: every device-level key must hold on the node, and — when
/// the rule carries parent keys — every parent key must hold on ONE AND THE
/// SAME ancestor (systemd udev: "all these keys must match at one and the
/// same parent device"; multiple `ATTRS` likewise share one device).
fn rule_matches(line: &str, ev: &FakeEvent) -> bool {
    let conds = parse_match_conds(line).unwrap_or_else(|e| panic!("invalid rule {line:?}: {e}"));
    if conds
        .iter()
        .filter(|c| c.key == "ACTION" || c.key == "SUBSYSTEM")
        .any(|c| !dev_key_matches(c, &ev.node, &ev.action))
    {
        return false;
    }
    let parent_conds: Vec<&Cond> = conds
        .iter()
        .filter(|c| c.key == "KERNELS" || c.key == "ATTRS")
        .collect();
    if parent_conds.is_empty() {
        return true;
    }
    ev.parents
        .iter()
        .any(|p| parent_conds.iter().all(|c| parent_key_matches(c, p)))
}

fn matching_rules(ev: &FakeEvent) -> Vec<String> {
    rule_lines(RULES)
        .into_iter()
        .filter(|l| rule_matches(l, ev))
        .map(str::to_string)
        .collect()
}

/// Captured USB-C trackpad over USB: hidraw -> HID `0003:05AC:0324` -> USB
/// interface -> USB device carrying both `idVendor`/`idProduct` (pid
/// parameter selects `0265` vs `0324`, incl. the HID parent name).
fn usb_trackpad(pid: &str, hid_pid: &str) -> FakeEvent {
    FakeEvent {
        action: "add".to_string(),
        node: dev("hidraw9", "hidraw", &[]),
        parents: vec![
            dev(&format!("0003:05AC:{hid_pid}.0007"), "hid", &[]),
            dev("1-2:1.1", "usb", &[("bInterfaceNumber", "01")]),
            dev("1-2", "usb", &[("idVendor", "05ac"), ("idProduct", pid)]),
        ],
    }
}

/// Captured BT trackpad: hidraw -> HID `0005:VID:PID.xxxx` -> bluetooth
/// parents. The only USB ancestor is the HOST adapter (`8087:0aaa`) — never
/// the trackpad. `hid_vid` selects `004C` vs `05AC` in the HID parent name.
fn bt_trackpad(hid_vid: &str, pid: &str) -> FakeEvent {
    FakeEvent {
        action: "add".to_string(),
        node: dev("hidraw5", "hidraw", &[]),
        parents: vec![
            dev(&format!("0005:{hid_vid}:{pid}.000A"), "hid", &[]),
            dev("AA:BB:CC:DD:EE:FF", "bluetooth", &[]),
            dev("hci0", "bluetooth", &[("address", "AA:BB:CC:DD:EE:01")]),
            dev("1-1", "usb", &[("idVendor", "8087"), ("idProduct", "0aaa")]),
        ],
    }
}

fn non_trackpad_usb() -> FakeEvent {
    FakeEvent {
        action: "add".to_string(),
        node: dev("hidraw3", "hidraw", &[]),
        parents: vec![
            dev("0003:046D:C52B.0001", "hid", &[]),
            dev("1-4", "usb", &[("idVendor", "046d"), ("idProduct", "c52b")]),
        ],
    }
}

fn virtual_input_node() -> FakeEvent {
    FakeEvent {
        action: "add".to_string(),
        node: dev("event12", "input", &[]),
        parents: vec![dev("input12", "input", &[("name", "ltd virtual")])],
    }
}

#[test]
fn rules_cover_usb_and_bt_identities_on_hidraw_add() {
    // Files referenced must be the ones shipped (include! would fail to
    // compile otherwise); also assert they exist on disk for clarity.
    assert!(!repo_file("udev/99-linux-trackpad-driver.rules").is_empty());
    let lines = rule_lines(RULES);
    assert!(!lines.is_empty(), "rules file has no active lines");
    for line in &lines {
        assert!(
            line.contains(r#"ACTION=="add""#),
            "rule must be add-only: {line}"
        );
        assert!(
            line.contains(r#"SUBSYSTEM=="hidraw""#),
            "rule must match hidraw: {line}"
        );
    }
    // USB identity: ATTRS pairs on the physical USB parent.
    let mut usb_covered = HashSet::new();
    for line in &lines {
        for (vid, pid) in USB_COMBOS {
            if line.contains(&format!(r#"ATTRS{{idVendor}}=="{vid}""#))
                && line.contains(&format!(r#"ATTRS{{idProduct}}=="{pid}""#))
            {
                usb_covered.insert((vid, pid));
            }
        }
    }
    for combo in USB_COMBOS {
        assert!(
            usb_covered.contains(&combo),
            "uncovered USB VID:PID {combo:?}"
        );
    }
    // BT identity: KERNELS on the BT HID parent (bus 0005, uppercase VID).
    let mut bt_covered = HashSet::new();
    for line in &lines {
        for pat in BT_KERNELS {
            if line.contains(&format!(r#"KERNELS=="{pat}""#)) {
                bt_covered.insert(pat);
            }
        }
    }
    for pat in BT_KERNELS {
        assert!(bt_covered.contains(&pat), "uncovered BT parent {pat:?}");
    }
    // Dead-match guard (finding #8): `004c` listed as a USB ATTRS pair can
    // never fire — no USB ancestor of a BT node carries the trackpad VID —
    // so it must not appear as ATTRS anywhere in this file.
    for line in &lines {
        assert!(
            !line.contains(r#"ATTRS{idVendor}=="004c""#),
            "BT VID must match via KERNELS, never USB ATTRS: {line}"
        );
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
        assert!(
            !line.contains(r#"SUBSYSTEMS=="input""#),
            "no input-parent matching in this file: {line}"
        );
    }
}

#[test]
fn rules_match_usb_sysfs_fixture() {
    // Captured USB trees must fire exactly one rule each, and only via the
    // USB ATTRS pair sharing the physical USB-device ancestor — the
    // same-ancestor constraint made executable.
    for (pid, hid_pid) in [("0265", "0265"), ("0324", "0324")] {
        let hits = matching_rules(&usb_trackpad(pid, hid_pid));
        assert_eq!(
            hits.len(),
            1,
            "USB {pid} must match exactly one rule: {hits:?}"
        );
        assert!(
            hits[0].contains(&format!(r#"ATTRS{{idProduct}}=="{pid}""#)),
            "USB {pid} must match its own PID rule: {hits:?}"
        );
        assert!(
            !hits[0].contains("KERNELS"),
            "USB match must come from ATTRS, not KERNELS: {hits:?}"
        );
    }
}

#[test]
fn rules_match_bt_hid_fixture_not_usb_ancestor() {
    // Captured BT trees (both PIDs, both VIDs the daemon accepts over bus
    // 0005) must each fire exactly one rule — via KERNELS on the HID parent.
    // The host-adapter USB ancestor (8087:0aaa) must never satisfy a USB
    // ATTRS rule: that confusion is the finding #8 defect.
    for (hid_vid, pid) in [
        ("004C", "0265"),
        ("004C", "0324"),
        ("05AC", "0265"),
        ("05AC", "0324"),
    ] {
        let ev = bt_trackpad(hid_vid, pid);
        let hits = matching_rules(&ev);
        assert_eq!(
            hits.len(),
            1,
            "BT {hid_vid}:{pid} must match exactly one rule: {hits:?}"
        );
        assert!(
            hits[0].contains(&format!(r#"KERNELS=="0005:{hid_vid}:{pid}.*""#)),
            "BT {hid_vid}:{pid} must match its own KERNELS rule: {hits:?}"
        );
        assert!(
            !hits[0].contains("ATTRS{idVendor}"),
            "BT match must not depend on any USB ancestor attrs: {hits:?}"
        );
    }
    // Cross-check every rule individually: any rule matching a BT tree must
    // be a KERNELS rule (adapter-VID trap), and any rule matching a USB
    // tree must be an ATTRS rule (bus-0005 pin keeps transports disjoint).
    for line in rule_lines(RULES) {
        let is_bt_rule = line.contains("KERNELS");
        for (hid_vid, pid) in [("004C", "0265"), ("004C", "0324")] {
            if rule_matches(line, &bt_trackpad(hid_vid, pid)) {
                assert!(
                    is_bt_rule,
                    "BT {hid_vid}:{pid} must never match a USB ATTRS rule: {line}"
                );
            }
        }
        for pid in ["0265", "0324"] {
            if rule_matches(line, &usb_trackpad(pid, pid)) {
                assert!(
                    !is_bt_rule,
                    "USB {pid} must never match a BT KERNELS rule: {line}"
                );
            }
        }
    }
}

#[test]
fn rules_reject_non_trackpad_and_input_nodes() {
    // Wrong VID:PID over USB, virtual input node (systemd#37063), and a
    // non-add action must all match nothing.
    assert!(
        matching_rules(&non_trackpad_usb()).is_empty(),
        "non-Apple USB hidraw must match no rule"
    );
    assert!(
        matching_rules(&virtual_input_node()).is_empty(),
        "virtual input node must match no rule"
    );
    let mut remove = usb_trackpad("0324", "0324");
    remove.action = "remove".to_string();
    assert!(
        matching_rules(&remove).is_empty(),
        "remove action must match no rule"
    );
}

#[test]
fn rules_keep_parent_keys_on_one_ancestor() {
    // systemd udev requires every parent key in a rule to match ONE AND THE
    // SAME ancestor. A rule mixing KERNELS (HID parent) with ATTRS (USB
    // device parent) could never fire on either transport — forbid the mix
    // so the transports stay matched by disjoint single-parent keys.
    for line in rule_lines(RULES) {
        let conds = parse_match_conds(line)
            .unwrap_or_else(|e| panic!("real rule must parse: {line:?}: {e}"));
        let has_kernels = conds.iter().any(|c| c.key == "KERNELS");
        let has_attrs = conds.iter().any(|c| c.key == "ATTRS");
        assert!(
            !(has_kernels && has_attrs),
            "KERNELS+ATTRS mix needs an impossible same-ancestor coincidence: {line}"
        );
    }
}

#[test]
fn strict_match_is_exact_or_trailing_star_only() {
    assert!(strict_match("add", "add"));
    assert!(!strict_match("add", "Add"));
    assert!(!strict_match("05ac", "05AC"));
    assert!(strict_match("0005:004C:0324.*", "0005:004C:0324.000A"));
    assert!(!strict_match("0005:004C:0324.*", "0005:004c:0324.000A"));
    assert!(!strict_match("0005:004C:0324.*", "0005:004C:0325.000A"));
    assert!(!strict_match("hidraw", "hidraw0"));
}

#[test]
fn strict_parser_rejects_unsupported_match_syntax() {
    // Operator, key, pattern, and shape errors must fail explicitly instead
    // of being silently ignored (which would fake coverage of new rules).
    for bad in [
        // Unsupported operator.
        r#"ACTION!="add", SUBSYSTEM=="hidraw""#,
        // Unsupported match keys (only ACTION/SUBSYSTEM/KERNELS/ATTRS).
        r#"ACTION=="add", SUBSYSTEM=="hidraw", KERNEL=="hidraw0""#,
        r#"ACTION=="add", SUBSYSTEM=="hidraw", DRIVER=="usbhid""#,
        r#"ACTION=="add", SUBSYSTEM=="hidraw", SUBSYSTEMS=="usb""#,
        r#"ACTION=="add", SUBSYSTEM=="hidraw", DRIVERS=="usb""#,
        r#"ACTION=="add", SUBSYSTEM=="hidraw", ATTR{name}=="foo""#,
        r#"ACTION=="add", SUBSYSTEM=="hidraw", PROGRAM=="/bin/true""#,
        // Unsupported pattern grammar (only exact or single trailing `*`).
        r#"ACTION=="add", SUBSYSTEM=="hidraw", KERNELS=="0005:004C:0324.*|0005:004C:0265.*""#,
        r#"ACTION=="add", SUBSYSTEM=="hidraw", KERNELS=="0005:004C:0324.000?""#,
        r#"ACTION=="add", SUBSYSTEM=="hidraw", KERNELS=="0005:[04]4C:*""#,
        r#"ACTION=="add", SUBSYSTEM=="hidraw", KERNELS=="*004C*""#,
        r#"ACTION=="add", SUBSYSTEM=="hidraw", KERNELS=="0005:*:0324.*""#,
        r#"ACTION=="add", SUBSYSTEM=="hidraw", ATTRS{idVendor}=="05ac|05AC""#,
        r#"ACTION=="add", SUBSYSTEM=="hidraw", ATTRS{idVendor}=="05**""#,
        // Brace shape errors.
        r#"ACTION=="add", SUBSYSTEM=="hidraw", KERNELS{foo}=="bar""#,
        r#"ACTION=="add", SUBSYSTEM=="hidraw", ATTRS=="05ac""#,
    ] {
        assert!(
            parse_match_conds(bad).is_err(),
            "must reject unsupported syntax: {bad}"
        );
    }
    // Real rule lines must parse (USB ATTRS + BT trailing-star KERNELS).
    for line in rule_lines(RULES) {
        assert!(
            parse_match_conds(line).is_ok(),
            "real rule must parse: {line}"
        );
    }
}

#[test]
fn strict_parser_skips_assignments_without_rejecting() {
    // TAG/ENV assignments never gate matching: they parse to no conds and
    // must not be mistaken for unsupported match keys.
    assert!(parse_match_conds(r#"TAG+="systemd""#)
        .expect("assignment must parse")
        .is_empty());
    let conds = parse_match_conds(
        r#"ACTION=="add", SUBSYSTEM=="hidraw", KERNELS=="0005:004C:0324.*", TAG+="systemd", ENV{SYSTEMD_WANTS}+="linux-trackpad-driver-reconnect@%k.service""#,
    )
    .expect("real BT rule must parse");
    assert_eq!(conds.len(), 3);
    let conds = parse_match_conds(
        r#"ACTION=="add", SUBSYSTEM=="hidraw", ATTRS{idVendor}=="05ac", ATTRS{idProduct}=="0324", TAG+="systemd""#,
    )
    .expect("real USB rule must parse");
    assert_eq!(conds.len(), 4);
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
