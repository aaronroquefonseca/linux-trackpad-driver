//! hidraw → evdev (`/dev/input/eventN`) resolution.
//!
//! Chain (first hit wins):
//! 1. `$LTD_EVDEV_NODE` override when set + non-empty (explicit operator pin).
//!    Empty is ignored (falls through). Trusted: backs a grab.
//! 2. Sysfs prefix walk: the input event whose canonical `device` dir sits
//!    under the hidraw node's canonical HID device dir. Precise per-node:
//!    on owner hardware hidraw8 (If1) → event13, hidraw9 → event12
//!    (verified live 2026-10-15: event13 under `...:1.0/...0009`,
//!    event12 under `...:1.1/...000A`). Interfaces without input
//!    children (hidraw10/11) resolve to nothing here and fall through.
//!    2b. BT single-node trusted fallback: when no USB interface layout exists
//!    (no `bInterfaceNumber` ancestors — Bluetooth has none) but the HID
//!    topology is unambiguous — exactly one BT trackpad-class hidraw
//!    (`HID_ID` parses to a supported Apple VID:PID with BT vendor
//!    `004c` or BT bus `0005`) plus exactly one MT-capable
//!    `Magic Trackpad`/`Trackpad` event — resolve that single event as
//!    trusted (`BtSingleNode`). This is the BT single-node topology where
//!    the prefix walk misses (event not strictly under the HID dir) yet
//!    parentage is still unambiguous. Multi-device unsafe configurations
//!    (0 or 2+ candidates on either side) fall through to the untrusted
//!    name scan below. Never guesses: ambiguity → passthrough, loudly.
//! 3. Name scan fallback: first `/dev/input/eventN` whose sysfs
//!    `device/name` contains `Magic Trackpad` or `Trackpad` (bare `Apple`
//!    is NOT enough — that would match Apple keyboards and grabbing a
//!    keyboard event node would swallow the keyboard behind the trackpad
//!    opt-in) AND whose `device/capabilities/abs` bitmap carries a
//!    multitouch bit (`ABS_MT_SLOT` 47 / `ABS_MT_PRESSURE` 58; keyboards
//!    report `abs = 0`). Multi-device unsafe (first hit wins); logged
//!    loudly so a two-pad rig notices. On BT (`usb_device_dir` absent)
//!    the fallback logs an explicit DOUBLE-INPUT warning: the daemon
//!    stays passthrough (never fail-closed) while the kernel keeps
//!    emitting, so proxy + native would double without a grab.
//! 4. `None` → caller stays passthrough with a clear log (never guesses
//!    `event0` roulette).
//!
//! Grab semantics: reversible `EVIOCGRAB` only, decided by the caller via
//! [`grab_allowed`]. Failure there keeps passthrough (no input loss) with
//! a loud log — see [`log_grab_outcome`]/[`grab_outcome_line`] for the
//! per-node metric line callers should emit.
//!
//! All roots are injectable for tests; production uses
//! `/sys/class/hidraw`, `/sys/class/input`, `/dev/input`.

use std::path::PathBuf;

/// Env override naming the physical evdev node.
pub const EVDEV_ENV: &str = "LTD_EVDEV_NODE";

/// Where a resolution came from (fallback-chain pin for tests/logs).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EvdevSource {
    EnvOverride(PathBuf),
    Sysfs(PathBuf),
    /// BT single-node trusted fallback (step 2b): unambiguous single
    /// BT hidraw + single MT trackpad event with VID:PID proof.
    /// Trusted like [`EvdevSource::Sysfs`], but flagged so logs/metrics
    /// can tell a direct prefix-walk hit from the BT fallback.
    BtSingleNode(PathBuf),
    NameScan(PathBuf),
}

/// Whether a resolution source may back a grab. `NameScan` is UNTRUSTED:
/// it matches by device name across the whole input subsystem with no
/// hidraw parentage proof, so grabbing it could swallow a foreign node
/// (e.g. a second pad's events, or worse). NameScan hits stay
/// passthrough — the precise sysfs walk, the BT single-node fallback
/// (unambiguous 1×hidraw + 1×event with VID:PID proof), or the env
/// override is the only grab-worthy proof.
pub fn grab_allowed(source: &EvdevSource) -> bool {
    matches!(
        source,
        EvdevSource::EnvOverride(_) | EvdevSource::Sysfs(_) | EvdevSource::BtSingleNode(_)
    )
}

/// One per-node grab metric line (pure, unit-tested). Callers (daemon
/// attach, proxy attach) should emit this after every grab attempt so a
/// `journalctl` grep answers "did BT suppression hold?".
/// `grabbed=true` → `grab=ok`, else `grab=fail` with `detail`.
/// Never fail-closed: a `fail` line always pairs with passthrough.
pub fn grab_outcome_line(
    node: &str,
    source: &EvdevSource,
    grabbed: bool,
    detail: Option<&str>,
) -> String {
    let status = if grabbed { "ok" } else { "fail" };
    let detail = detail.unwrap_or(if grabbed {
        "suppression holds"
    } else {
        "passthrough continues"
    });
    format!("evdev: grab {status} node={node} src={source:?} detail={detail}")
}

/// Log the per-node grab outcome at the right level: `info` on success,
/// `warn` on failure (loud — failure means kernel + proxy both emit).
/// Reversible grab only; never fail-closed (passthrough, never swallowed).
pub fn log_grab_outcome(node: &str, source: &EvdevSource, grabbed: bool, detail: Option<&str>) {
    let line = grab_outcome_line(node, source, grabbed, detail);
    if grabbed {
        log::info!("{line}");
    } else {
        log::warn!("{line}");
    }
}

/// Production entry: env → sysfs walk → BT single-node → name scan.
/// The hidraw sysfs root respects `$LTD_HIDRAW_SYSFS` (hermetic tests inherit
/// it); input/dev roots stay production-fixed (daemon input path, not the
/// CLI activation tests).
pub fn resolve_evdev_node(hidraw: &str) -> Option<(PathBuf, EvdevSource)> {
    let env = std::env::var(EVDEV_ENV).ok().filter(|v| !v.is_empty());
    resolve_with_roots(
        hidraw,
        env.as_deref(),
        &crate::discovery::sysfs_class_dir(),
        "/sys/class/input",
        "/dev/input",
    )
}

/// Injectable core: `env_override` instead of reading env (tests),
/// plus sysfs/dev roots. Empty override is ignored (falls through).
pub fn resolve_with_roots(
    hidraw: &str,
    env_override: Option<&str>,
    hidraw_root: &str,
    input_root: &str,
    dev_prefix: &str,
) -> Option<(PathBuf, EvdevSource)> {
    if let Some(node) = env_override.filter(|v| !v.is_empty()) {
        let p = PathBuf::from(node);
        log::info!("evdev: using {EVDEV_ENV} override {p:?} for {hidraw}");
        return Some((p.clone(), EvdevSource::EnvOverride(p)));
    }
    if let Some(p) = sysfs_walk(hidraw, hidraw_root, input_root, dev_prefix) {
        log::info!("evdev: sysfs {hidraw} → {p:?}");
        return Some((p.clone(), EvdevSource::Sysfs(p)));
    }
    if let Some(p) = bt_single_node_walk(hidraw, hidraw_root, input_root, dev_prefix) {
        log::info!("evdev: BT single-node trusted fallback {hidraw} → {p:?} (1xBT-hidraw + 1xMT-event, VID:PID proof)");
        return Some((p.clone(), EvdevSource::BtSingleNode(p)));
    }
    if let Some(p) = name_scan(input_root, dev_prefix) {
        if is_bt_like(hidraw, hidraw_root) {
            log::warn!(
                "evdev: BT {hidraw} sysfs walk + single-node missed; name-scan fallback → {p:?} (multi-device unsafe, first hit wins; DOUBLE-INPUT RISK: untrusted so no grab, kernel keeps emitting — set {EVDEV_ENV} to pin, or single-pair the BT topology)"
            );
        } else {
            log::warn!(
                "evdev: sysfs walk missed for {hidraw}; name-scan fallback → {p:?} (multi-device unsafe, first hit wins)"
            );
        }
        return Some((p.clone(), EvdevSource::NameScan(p)));
    }
    log::warn!(
        "evdev: no event node for {hidraw} (override unset, sysfs miss, name miss); passthrough"
    );
    None
}

fn hid_device_dir(hidraw: &str, hidraw_root: &str) -> Option<PathBuf> {
    let base = hidraw.rsplit('/').next()?;
    if !base.starts_with("hidraw") {
        return None;
    }
    let link = PathBuf::from(format!("{hidraw_root}/{base}/device"));
    std::fs::canonicalize(&link).ok()
}

/// Read a `bInterfaceNumber` attribute walking up from `dir` (the HID
/// device itself carries none; the USB interface parent does). Mirrors
/// [`crate::discovery::interface_number_for_node`] without its fixed
/// sysfs root so tests can inject a fake tree.
fn iface_number_from_tree(mut dir: PathBuf) -> Option<u8> {
    for _ in 0..4 {
        if let Ok(text) = std::fs::read_to_string(dir.join("bInterfaceNumber")) {
            if let Some(n) = crate::discovery::parse_interface_number(&text) {
                return Some(n);
            }
        }
        if !dir.pop() {
            return None;
        }
    }
    None
}

/// USB device dir owning a hidraw node: the parent of the interface dir
/// (the first ancestor carrying `bInterfaceNumber`). `None` for
/// Bluetooth / unknown topologies (no interface layout to route by).
fn usb_device_dir(hidraw: &str, hidraw_root: &str) -> Option<PathBuf> {
    let mut dir = hid_device_dir(hidraw, hidraw_root)?;
    for _ in 0..4 {
        if dir.join("bInterfaceNumber").exists() {
            return dir.parent().map(|p| p.to_path_buf());
        }
        if !dir.pop() {
            return None;
        }
    }
    None
}

/// Resolve the haptic actuator node for a touch hidraw node: the sibling
/// hidraw interface with number 2 under the same USB device (the
/// old-service host-click/actuator interface). Returns the node plus its
/// interface number, or the input node itself with `None` when no
/// interface layout exists — with a loud log that If2 routing is
/// pending HW verify (an actuator on another interface would otherwise
/// stay silent, the NAK swallowed).
pub fn resolve_haptic_node(
    hidraw: &str,
    hidraw_root: &str,
    dev_prefix: &str,
) -> (String, Option<u8>) {
    resolve_haptic_node_inner(hidraw, hidraw_root, dev_prefix, true)
}

fn resolve_haptic_node_inner(
    hidraw: &str,
    hidraw_root: &str,
    dev_prefix: &str,
    diagnose: bool,
) -> (String, Option<u8>) {
    let own = hidraw.to_string();
    let Some(usb_dev) = usb_device_dir(hidraw, hidraw_root) else {
        if diagnose {
            log::warn!(
                "haptic: no USB interface layout for {hidraw}; using touch node (If2 routing pending HW verify)"
            );
        }
        return (own, None);
    };
    let entries = match std::fs::read_dir(hidraw_root) {
        Ok(e) => e,
        Err(_) => return (own, None),
    };
    let mut if2: Vec<String> = Vec::new();
    for entry in entries.flatten() {
        let base = entry.file_name().to_string_lossy().into_owned();
        if !base.starts_with("hidraw") {
            continue;
        }
        let link = PathBuf::from(format!("{hidraw_root}/{base}/device"));
        let Ok(canon) = std::fs::canonicalize(&link) else {
            continue;
        };
        if !canon.starts_with(&usb_dev) {
            continue;
        }
        if iface_number_from_tree(canon) == Some(2) {
            if2.push(base);
        }
    }
    // Numeric (hidraw2 < hidraw10); lexicographic would invert them.
    crate::discovery::sort_numerically(&mut if2);
    match if2.into_iter().next() {
        Some(base) => {
            let node = format!("{dev_prefix}/{base}");
            if diagnose {
                log::info!("haptic: {hidraw} actuator → {node} (interface 2)");
            }
            (node, Some(2))
        }
        None => {
            if diagnose {
                log::warn!(
                    "haptic: no interface-2 sibling for {hidraw}; using touch node (If2 routing pending HW verify)"
                );
            }
            let own_iface = hid_device_dir(hidraw, hidraw_root).and_then(iface_number_from_tree);
            (own, own_iface)
        }
    }
}

/// Production haptic-node entry (`$LTD_HIDRAW_SYSFS` or `/sys/class/hidraw`, `/dev`).
pub fn haptic_node_for(hidraw: &str) -> (String, Option<u8>) {
    resolve_haptic_node(hidraw, &crate::discovery::sysfs_class_dir(), "/dev")
}

/// Reconcile late sibling routing without logging unchanged healthy state.
pub fn haptic_node_for_quiet(hidraw: &str) -> (String, Option<u8>) {
    resolve_haptic_node_inner(hidraw, &crate::discovery::sysfs_class_dir(), "/dev", false)
}

fn sysfs_walk(
    hidraw: &str,
    hidraw_root: &str,
    input_root: &str,
    dev_prefix: &str,
) -> Option<PathBuf> {
    let hid_dir = hid_device_dir(hidraw, hidraw_root)?;
    let entries = std::fs::read_dir(input_root).ok()?;
    let mut hits: Vec<String> = Vec::new();
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if !name.starts_with("event") {
            continue;
        }
        let dev_link = PathBuf::from(format!("{input_root}/{name}/device"));
        let Ok(canon) = std::fs::canonicalize(&dev_link) else {
            continue;
        };
        if canon.starts_with(&hid_dir) {
            hits.push(format!("{dev_prefix}/{name}"));
        }
    }
    // Numeric order (event2 < event10); lexicographic would invert them.
    crate::discovery::sort_numerically(&mut hits);
    hits.into_iter().next().map(PathBuf::from)
}

/// ABS codes proving a multitouch pad (linux/input-event-codes.h).
/// Keyboards report `abs = 0`; the trackpad reports both bits live
/// (owner hardware 2026-10-15: event12/13 `abs=6738…`).
const ABS_MT_SLOT: u32 = 47;
const ABS_MT_PRESSURE: u32 = 58;

/// True when a sysfs `capabilities/abs` bitmap carries a multitouch bit.
/// The `abs` map is tiny (`ABS_MAX = 0x3f`): one 64-bit word on 64-bit
/// kernels, printed as bare lowercase hex. Multi-word input (32-bit
/// kernels print two words, most-significant first) is handled
/// generically; unparsable text is a conservative `false`.
///
/// `pub(crate)` so discovery can apply the same MT proof at SELECTION
/// time (hidraw ranking), not just at grab time.
pub(crate) fn abs_has_mt(text: &str) -> bool {
    let words: Vec<u64> = text
        .replace(',', " ")
        .split_whitespace()
        .filter_map(|w| u64::from_str_radix(w.trim_start_matches("0x"), 16).ok())
        .collect();
    if words.is_empty() {
        return false;
    }
    let has = |bit: u32| {
        let idx = (bit / 64) as usize;
        if words.len() == 1 {
            // Fast path: single-word map, bit N is bit N.
            idx == 0 && (words[0] >> (bit % 64)) & 1 == 1
        } else {
            // MSB-first word order (kernel bitmap_print): bit N lives in
            // the word `len - 1 - N / 64` from the front.
            words.len() > idx && (words[words.len() - 1 - idx] >> (bit % 64)) & 1 == 1
        }
    };
    has(ABS_MT_SLOT) || has(ABS_MT_PRESSURE)
}

fn mt_capable(input_root: &str, event: &str) -> bool {
    let caps = PathBuf::from(format!("{input_root}/{event}/device/capabilities/abs"));
    let Ok(text) = std::fs::read_to_string(&caps) else {
        return false;
    };
    abs_has_mt(&text)
}

fn trackpad_name_ok(text: &str) -> bool {
    // Bare "Apple" excluded on purpose: Apple keyboards must never
    // resolve here (grabbing one would swallow the keyboard).
    text.contains("Magic Trackpad") || text.contains("Trackpad")
}

/// All MT-capable trackpad-class events, numerically sorted
/// (`event2 < event10`). Shared by the untrusted name scan (first hit)
/// and the BT single-node gate (exactly-one check).
fn trackpad_event_candidates(input_root: &str, dev_prefix: &str) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(input_root) else {
        return Vec::new();
    };
    let mut names: Vec<String> = Vec::new();
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.starts_with("event") {
            names.push(name);
        }
    }
    crate::discovery::sort_numerically(&mut names);
    let mut out = Vec::new();
    for name in names {
        let name_file = PathBuf::from(format!("{input_root}/{name}/device/name"));
        let Ok(text) = std::fs::read_to_string(&name_file) else {
            continue;
        };
        if !trackpad_name_ok(&text) {
            continue;
        }
        if !mt_capable(input_root, &name) {
            log::warn!("evdev: name-scan candidate {name} ({text:?}) lacks MT abs bits; refusing");
            continue;
        }
        out.push(PathBuf::from(format!("{dev_prefix}/{name}")));
    }
    out
}

fn name_scan(input_root: &str, dev_prefix: &str) -> Option<PathBuf> {
    trackpad_event_candidates(input_root, dev_prefix)
        .into_iter()
        .next()
}

/// `(bus, vendor, product)` from a hidraw node's `device/uevent`
/// `HID_ID` field. `None` when unreadable/unparsable (conservative:
/// no VID:PID proof, no BT trust).
fn hidraw_ids(hidraw: &str, hidraw_root: &str) -> Option<(u16, u16, u16)> {
    let base = crate::discovery::hidraw_name(hidraw)?;
    let text = std::fs::read_to_string(format!("{hidraw_root}/{base}/device/uevent")).ok()?;
    let hid_id = crate::discovery::hid_id_from_uevent(&text)?;
    crate::discovery::parse_hid_id(&hid_id)
}

/// True for Bluetooth-class trackpad hidraw nodes: supported Apple
/// VID:PID with the BT vendor (`004c`) or BT bus (`0005`).
fn is_bt_trackpad_ids(ids: (u16, u16, u16)) -> bool {
    let (bus, vid, pid) = ids;
    crate::discovery::is_supported_trackpad(vid, pid)
        && (vid == crate::discovery::VID_APPLE_BT || bus == crate::discovery::BUS_BLUETOOTH)
}

/// True when the hidraw node looks BT-like for logging purposes:
/// no USB interface layout (BT has none), with a resolvable HID dir.
/// Best-effort sysfs probe; false on any unresolvable layout.
fn is_bt_like(hidraw: &str, hidraw_root: &str) -> bool {
    usb_device_dir(hidraw, hidraw_root).is_none() && hid_device_dir(hidraw, hidraw_root).is_some()
}

/// BT trackpad-class hidraw nodes under `hidraw_root`. Unreadable/
/// unparsable nodes are skipped (no proof, no count).
fn bt_trackpad_hidraws(hidraw_root: &str) -> Vec<String> {
    let Ok(entries) = std::fs::read_dir(hidraw_root) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for entry in entries.flatten() {
        let base = entry.file_name().to_string_lossy().into_owned();
        if !base.starts_with("hidraw") {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(format!("{hidraw_root}/{base}/device/uevent"))
        else {
            continue;
        };
        let Some(hid_id) = crate::discovery::hid_id_from_uevent(&text) else {
            continue;
        };
        let Some(ids) = crate::discovery::parse_hid_id(&hid_id) else {
            continue;
        };
        if is_bt_trackpad_ids(ids) {
            out.push(base);
        }
    }
    out
}

/// BT single-node trusted fallback (step 2b).
///
/// Succeeds only when ALL hold:
/// - the requested hidraw resolves to a canonical HID dir with NO USB
///   interface layout (`usb_device_dir` is `None` — BT has none);
/// - the requested hidraw's own `device/uevent` `HID_ID` is a supported
///   Apple trackpad pair on the BT transport (`004c` vendor or `0005`
///   bus; `0265`/`0324` products);
/// - exactly one BT trackpad-class hidraw exists under `hidraw_root`
///   (single-hidraw; a two-pad rig falls through to the untrusted scan);
/// - exactly one MT-capable trackpad-class event exists under
///   `input_root` (single-evdev; 0 or 2+ falls through).
///
/// Returns the single event node. The caller wraps it as
/// [`EvdevSource::BtSingleNode`] (trusted: uniqueness + VID:PID + MT +
/// name is the BT parentage substitute). Anything else → `None`
/// (caller falls through to name scan → passthrough, loudly).
fn bt_single_node_walk(
    hidraw: &str,
    hidraw_root: &str,
    input_root: &str,
    dev_prefix: &str,
) -> Option<PathBuf> {
    // No USB layout: BT/unknown only. USB nodes keep the prefix walk.
    if usb_device_dir(hidraw, hidraw_root).is_some() {
        return None;
    }
    hid_device_dir(hidraw, hidraw_root)?;
    let ids = hidraw_ids(hidraw, hidraw_root)?;
    if !is_bt_trackpad_ids(ids) {
        return None;
    }
    // Single-hidraw: the requested node must be the unique BT trackpad.
    let mut bt_nodes = bt_trackpad_hidraws(hidraw_root);
    crate::discovery::sort_numerically(&mut bt_nodes);
    if bt_nodes.len() != 1 {
        return None;
    }
    let want = crate::discovery::hidraw_name(hidraw)?;
    if bt_nodes[0] != want {
        return None;
    }
    // Single-evdev: exactly one MT trackpad event system-wide.
    let candidates = trackpad_event_candidates(input_root, dev_prefix);
    if candidates.len() != 1 {
        return None;
    }
    candidates.into_iter().next()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fake_hidraw_tree(root: &std::path::Path) -> (String, String) {
        let hidraw_root = root.join("hidraw");
        let input_root = root.join("input");
        std::fs::create_dir_all(&hidraw_root).unwrap();
        std::fs::create_dir_all(&input_root).unwrap();
        (
            hidraw_root.to_str().unwrap().to_string(),
            input_root.to_str().unwrap().to_string(),
        )
    }

    #[test]
    fn env_override_wins_over_sysfs() {
        let dir = tempfile::tempdir().unwrap();
        let (hroot, iroot) = fake_hidraw_tree(dir.path());
        let dev = dir.path().join("devinput");
        std::fs::create_dir_all(&dev).unwrap();
        let dp = dev.to_str().unwrap();
        let got = resolve_with_roots(
            "/dev/hidraw9",
            Some("/dev/input/event99"),
            &hroot,
            &iroot,
            dp,
        )
        .unwrap();
        assert_eq!(got.1, EvdevSource::EnvOverride("/dev/input/event99".into()));
    }

    #[test]
    fn sysfs_prefix_walk_matches_child_event() {
        let dir = tempfile::tempdir().unwrap();
        let (hroot, iroot) = fake_hidraw_tree(dir.path());
        let dev = dir.path().join("devinput");
        std::fs::create_dir_all(&dev).unwrap();
        let dp = dev.to_str().unwrap().to_string();
        // Layout: hid device dir with an input child, like live hardware.
        let hid = dir.path().join("devices/hid0");
        let input_child = hid.join("input/input15");
        std::fs::create_dir_all(&input_child).unwrap();
        std::fs::create_dir_all(dir.path().join("hidraw/hidraw8")).unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(&hid, dir.path().join("hidraw/hidraw8/device")).unwrap();
        std::fs::create_dir_all(dir.path().join("input/event13")).unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(&input_child, dir.path().join("input/event13/device")).unwrap();
        // Unrelated event with no hid link is ignored by the walk.
        std::fs::create_dir_all(dir.path().join("input/event99")).unwrap();
        let got = resolve_with_roots("/dev/hidraw8", None, &hroot, &iroot, &dp).unwrap();
        assert_eq!(got.0, PathBuf::from(format!("{dp}/event13")));
        assert!(matches!(got.1, EvdevSource::Sysfs(_)));
    }

    #[test]
    fn name_scan_fallback_when_walk_misses() {
        let dir = tempfile::tempdir().unwrap();
        let (hroot, iroot) = fake_hidraw_tree(dir.path());
        let dev = dir.path().join("devinput");
        std::fs::create_dir_all(&dev).unwrap();
        let dp = dev.to_str().unwrap().to_string();
        // hidraw device exists but has no input children.
        let hid = dir.path().join("devices/hid9");
        std::fs::create_dir_all(&hid).unwrap();
        std::fs::create_dir_all(dir.path().join("hidraw/hidraw10")).unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(&hid, dir.path().join("hidraw/hidraw10/device")).unwrap();
        // event with Apple trackpad name elsewhere + MT abs bits (live
        // owner value for event12).
        let other = dir.path().join("devices/other/input/input9");
        std::fs::create_dir_all(&other).unwrap();
        std::fs::create_dir_all(other.join("capabilities")).unwrap();
        std::fs::create_dir_all(dir.path().join("input/event12")).unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(&other, dir.path().join("input/event12/device")).unwrap();
        std::fs::write(
            dir.path().join("input/event12/device/name"),
            "Apple Inc. Magic Trackpad\n",
        )
        .unwrap();
        std::fs::write(
            dir.path().join("input/event12/device/capabilities/abs"),
            "673800001000003\n",
        )
        .unwrap();
        let got = resolve_with_roots("/dev/hidraw10", None, &hroot, &iroot, &dp).unwrap();
        assert_eq!(got.0, PathBuf::from(format!("{dp}/event12")));
        assert!(matches!(got.1, EvdevSource::NameScan(_)));
    }

    #[test]
    fn namescan_resolution_never_backs_grab() {
        // Decision pin for the daemon: only sysfs parentage proof (or
        // the explicit env override, or the BT single-node fallback with
        // VID:PID + uniqueness proof) may back an EVIOCGRAB. A NameScan
        // hit — e.g. an input-less hidraw10 resolving to some trackpad
        // event node by name — must stay passthrough, never grab.
        assert!(grab_allowed(&EvdevSource::Sysfs(
            "/dev/input/event13".into()
        )));
        assert!(grab_allowed(&EvdevSource::BtSingleNode(
            "/dev/input/event5".into()
        )));
        assert!(grab_allowed(&EvdevSource::EnvOverride(
            "/dev/input/event13".into()
        )));
        assert!(!grab_allowed(&EvdevSource::NameScan(
            "/dev/input/event13".into()
        )));
    }

    #[test]
    fn haptic_prefers_interface_2_sibling() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("hidraw/hidraw8")).unwrap();
        std::fs::create_dir_all(dir.path().join("hidraw/hidraw9")).unwrap();
        let if1 = dir.path().join("devices/usb/if1");
        let if2 = dir.path().join("devices/usb/if2");
        std::fs::create_dir_all(if1.join("hid0")).unwrap();
        std::fs::create_dir_all(if2.join("hid2")).unwrap();
        std::fs::write(if1.join("bInterfaceNumber"), "01\n").unwrap();
        std::fs::write(if2.join("bInterfaceNumber"), "02\n").unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(if1.join("hid0"), dir.path().join("hidraw/hidraw8/device"))
            .unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(if2.join("hid2"), dir.path().join("hidraw/hidraw9/device"))
            .unwrap();
        let hroot = dir.path().join("hidraw").to_str().unwrap().to_string();
        let dp = "/dev";
        // From the If1 touch node → If2 sibling.
        assert_eq!(
            resolve_haptic_node("/dev/hidraw8", &hroot, dp),
            ("/dev/hidraw9".to_string(), Some(2))
        );
        // Already If2 → itself.
        assert_eq!(
            resolve_haptic_node("/dev/hidraw9", &hroot, dp),
            ("/dev/hidraw9".to_string(), Some(2))
        );
    }

    #[test]
    fn haptic_resolves_if2_in_owner_four_iface_layout() {
        // Owner 2026-10-15: hidraw8=If0, hidraw9=If1 (pump), hidraw10=If2
        // (actuator), hidraw11=If3 — all under one USB device. From the If1
        // pump node the haptic node must be the hidraw10-class If2 sibling,
        // never If3 or the touch node itself.
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("hidraw/hidraw8")).unwrap();
        std::fs::create_dir_all(dir.path().join("hidraw/hidraw9")).unwrap();
        std::fs::create_dir_all(dir.path().join("hidraw/hidraw10")).unwrap();
        std::fs::create_dir_all(dir.path().join("hidraw/hidraw11")).unwrap();
        let usb = dir.path().join("devices/usb1");
        for (iface, hid) in [(0u8, "hid0"), (1, "hid1"), (2, "hid2"), (3, "hid3")] {
            let if_dir = usb.join(format!("if{iface}"));
            std::fs::create_dir_all(if_dir.join(hid)).unwrap();
            std::fs::write(if_dir.join("bInterfaceNumber"), format!("{iface:02}\n")).unwrap();
        }
        #[cfg(unix)]
        for (base, iface, hid) in [
            ("hidraw8", 0u8, "hid0"),
            ("hidraw9", 1, "hid1"),
            ("hidraw10", 2, "hid2"),
            ("hidraw11", 3, "hid3"),
        ] {
            std::os::unix::fs::symlink(
                usb.join(format!("if{iface}/{hid}")),
                dir.path().join(format!("hidraw/{base}/device")),
            )
            .unwrap();
        }
        let hroot = dir.path().join("hidraw").to_str().unwrap().to_string();
        assert_eq!(
            resolve_haptic_node("/dev/hidraw9", &hroot, "/dev"),
            ("/dev/hidraw10".to_string(), Some(2))
        );
    }

    #[test]
    fn quiet_haptic_resolution_observes_late_actuator_and_removal() {
        let dir = tempfile::tempdir().unwrap();
        let hroot = dir.path().join("hidraw");
        let usb = dir.path().join("devices/usb");
        for (base, iface) in [("hidraw8", 1), ("hidraw9", 2)] {
            std::fs::create_dir_all(hroot.join(base)).unwrap();
            std::fs::create_dir_all(usb.join(format!("if{iface}/hid"))).unwrap();
            std::fs::write(
                usb.join(format!("if{iface}/bInterfaceNumber")),
                format!("{iface:02x}"),
            )
            .unwrap();
        }
        std::os::unix::fs::symlink(usb.join("if1/hid"), hroot.join("hidraw8/device")).unwrap();
        let resolve =
            || resolve_haptic_node_inner("/dev/hidraw8", hroot.to_str().unwrap(), "/dev", false);
        assert_eq!(resolve(), ("/dev/hidraw8".into(), Some(1)));
        std::os::unix::fs::symlink(usb.join("if2/hid"), hroot.join("hidraw9/device")).unwrap();
        assert_eq!(resolve(), ("/dev/hidraw9".into(), Some(2)));
        // Repeated health checks retain the target; disappearance restores
        // the existing fallback without tearing down the touch session.
        assert_eq!(resolve(), ("/dev/hidraw9".into(), Some(2)));
        std::fs::remove_file(hroot.join("hidraw9/device")).unwrap();
        assert_eq!(resolve(), ("/dev/hidraw8".into(), Some(1)));
    }

    #[test]
    fn haptic_falls_back_to_touch_node_without_layout() {
        let dir = tempfile::tempdir().unwrap();
        let hroot = dir.path().join("hidraw");
        std::fs::create_dir_all(hroot.join("hidraw0")).unwrap();
        let plain = dir.path().join("devices/plain");
        std::fs::create_dir_all(&plain).unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(&plain, hroot.join("hidraw0/device")).unwrap();
        let hr = hroot.to_str().unwrap();
        // No bInterfaceNumber anywhere (BT-like): own node, loud log.
        assert_eq!(
            resolve_haptic_node("/dev/hidraw0", hr, "/dev"),
            ("/dev/hidraw0".to_string(), None)
        );
    }

    #[test]
    fn abs_bitmap_gates_mt_bits() {
        // Live owner values (2026-10-15): both trackpad events carry
        // bits 47 + 58; keyboards/consumer-controls carry neither.
        assert!(abs_has_mt("673810001000003\n"));
        assert!(abs_has_mt("673800001000003"));
        assert!(!abs_has_mt("0"));
        assert!(!abs_has_mt("100000000"));
        assert!(!abs_has_mt(""));
        assert!(!abs_has_mt("not-hex"));
    }

    #[test]
    fn apple_keyboard_never_resolves() {
        // An Apple keyboard (`abs = 0`, no MT bits) must be refused even
        // though its name contains "Apple": grabbing it would swallow the
        // keyboard behind the trackpad opt-in. Result is passthrough (None).
        let dir = tempfile::tempdir().unwrap();
        let (hroot, iroot) = fake_hidraw_tree(dir.path());
        let dev = dir.path().join("devinput");
        std::fs::create_dir_all(&dev).unwrap();
        let dp = dev.to_str().unwrap().to_string();
        let hid = dir.path().join("devices/hid9");
        std::fs::create_dir_all(&hid).unwrap();
        std::fs::create_dir_all(dir.path().join("hidraw/hidraw10")).unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(&hid, dir.path().join("hidraw/hidraw10/device")).unwrap();
        // Keyboard: Apple name, zero abs caps.
        let kbd = dir.path().join("devices/kbd/input/input3");
        std::fs::create_dir_all(&kbd).unwrap();
        std::fs::create_dir_all(kbd.join("capabilities")).unwrap();
        std::fs::create_dir_all(dir.path().join("input/event5")).unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(&kbd, dir.path().join("input/event5/device")).unwrap();
        std::fs::write(
            dir.path().join("input/event5/device/name"),
            "Apple Inc. Magic Keyboard\n",
        )
        .unwrap();
        std::fs::write(
            dir.path().join("input/event5/device/capabilities/abs"),
            "0\n",
        )
        .unwrap();
        assert!(
            resolve_with_roots("/dev/hidraw10", None, &hroot, &iroot, &dp).is_none(),
            "Apple keyboard must never resolve to a grab target"
        );
    }

    #[test]
    fn trackpad_name_without_mt_bits_refused() {
        // Name matches but abs lacks MT (unbound node, zeroed caps):
        // refuse rather than grab blind.
        let dir = tempfile::tempdir().unwrap();
        let (hroot, iroot) = fake_hidraw_tree(dir.path());
        let dev = dir.path().join("devinput");
        std::fs::create_dir_all(&dev).unwrap();
        let dp = dev.to_str().unwrap().to_string();
        let hid = dir.path().join("devices/hid9");
        std::fs::create_dir_all(&hid).unwrap();
        std::fs::create_dir_all(dir.path().join("hidraw/hidraw10")).unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(&hid, dir.path().join("hidraw/hidraw10/device")).unwrap();
        let pad = dir.path().join("devices/pad/input/input9");
        std::fs::create_dir_all(&pad).unwrap();
        std::fs::create_dir_all(pad.join("capabilities")).unwrap();
        std::fs::create_dir_all(dir.path().join("input/event12")).unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(&pad, dir.path().join("input/event12/device")).unwrap();
        std::fs::write(
            dir.path().join("input/event12/device/name"),
            "Apple Inc. Magic Trackpad\n",
        )
        .unwrap();
        std::fs::write(
            dir.path().join("input/event12/device/capabilities/abs"),
            "0\n",
        )
        .unwrap();
        assert!(resolve_with_roots("/dev/hidraw10", None, &hroot, &iroot, &dp).is_none(),);
    }

    #[test]
    fn nothing_resolves_to_none() {
        let dir = tempfile::tempdir().unwrap();
        let (hroot, iroot) = fake_hidraw_tree(dir.path());
        let dev = dir.path().join("devinput");
        std::fs::create_dir_all(&dev).unwrap();
        let dp = dev.to_str().unwrap();
        assert!(resolve_with_roots("/dev/hidraw0", None, &hroot, &iroot, dp).is_none());
        assert!(resolve_with_roots("/dev/input/event5", None, &hroot, &iroot, dp).is_none());
    }

    // ---- BT single-node trusted fallback (PR3) ----

    fn fake_mt_event(root: &std::path::Path, event: &str, name: &str) {
        let other = root.join(format!("devices/other-{event}/input/input9"));
        std::fs::create_dir_all(&other).unwrap();
        std::fs::create_dir_all(other.join("capabilities")).unwrap();
        std::fs::create_dir_all(root.join(format!("input/{event}"))).unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(&other, root.join(format!("input/{event}/device"))).unwrap();
        std::fs::write(root.join(format!("input/{event}/device/name")), name).unwrap();
        std::fs::write(
            root.join(format!("input/{event}/device/capabilities/abs")),
            "673800001000003\n",
        )
        .unwrap();
    }

    fn bt_single_hidraw_tree(root: &std::path::Path, base: &str, hid_id: &str) -> (String, String) {
        // Minimal BT layout: hid dir + hidraw symlink + uevent through
        // the canonical dir.
        let hroot = root.join("hidraw");
        let iroot = root.join("input");
        std::fs::create_dir_all(&hroot).unwrap();
        std::fs::create_dir_all(&iroot).unwrap();
        let hid = root.join("devices/bt");
        std::fs::create_dir_all(&hid).unwrap();
        std::fs::write(
            hid.join("uevent"),
            format!("HID_ID={hid_id}\nHID_NAME=Apple Inc. Magic Trackpad\n"),
        )
        .unwrap();
        std::fs::create_dir_all(hroot.join(base)).unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(&hid, hroot.join(base).join("device")).unwrap();
        (
            hroot.to_str().unwrap().to_string(),
            iroot.to_str().unwrap().to_string(),
        )
    }

    #[test]
    fn bt_single_node_trusted_resolves_as_grabbable() {
        // BT miss mode: event NOT under the HID dir (prefix walk misses),
        // but 1×BT-hidraw (004c:0265) + 1×MT-event → trusted BtSingleNode.
        let dir = tempfile::tempdir().unwrap();
        let (hroot, iroot) = bt_single_hidraw_tree(dir.path(), "hidraw0", "0005:0000004C:00000265");
        let dev = dir.path().join("devinput");
        std::fs::create_dir_all(&dev).unwrap();
        let dp = dev.to_str().unwrap().to_string();
        fake_mt_event(dir.path(), "event5", "Apple Inc. Magic Trackpad\n");
        let got = resolve_with_roots("/dev/hidraw0", None, &hroot, &iroot, &dp).unwrap();
        assert_eq!(got.0, PathBuf::from(format!("{dp}/event5")));
        assert!(
            matches!(got.1, EvdevSource::BtSingleNode(_)),
            "BT single-node must be trusted, got {:?}",
            got.1
        );
        assert!(grab_allowed(&got.1));
    }

    #[test]
    fn bt_single_node_refuses_two_events_to_namescan() {
        // Two MT trackpad events: ambiguous → no BT trust, falls to
        // untrusted name scan (first hit, no grab). Never fail-closed:
        // passthrough with a loud log.
        let dir = tempfile::tempdir().unwrap();
        let (hroot, iroot) = bt_single_hidraw_tree(dir.path(), "hidraw0", "0005:0000004C:00000265");
        let dev = dir.path().join("devinput");
        std::fs::create_dir_all(&dev).unwrap();
        let dp = dev.to_str().unwrap().to_string();
        fake_mt_event(dir.path(), "event5", "Apple Inc. Magic Trackpad\n");
        fake_mt_event(dir.path(), "event6", "Apple Inc. Magic Trackpad\n");
        let got = resolve_with_roots("/dev/hidraw0", None, &hroot, &iroot, &dp).unwrap();
        assert!(matches!(got.1, EvdevSource::NameScan(_)), "got {:?}", got.1);
        assert!(!grab_allowed(&got.1));
    }

    #[test]
    fn bt_single_node_refuses_two_hidraw() {
        // Two BT pads: ambiguous hidraw side → no trust.
        let dir = tempfile::tempdir().unwrap();
        let hroot = dir.path().join("hidraw");
        let iroot = dir.path().join("input");
        std::fs::create_dir_all(&hroot).unwrap();
        std::fs::create_dir_all(&iroot).unwrap();
        for base in ["hidraw0", "hidraw1"] {
            let hid = dir.path().join(format!("devices/bt-{base}"));
            std::fs::create_dir_all(&hid).unwrap();
            std::fs::write(
                hid.join("uevent"),
                "HID_ID=0005:0000004C:00000265\nHID_NAME=Apple Inc. Magic Trackpad\n",
            )
            .unwrap();
            std::fs::create_dir_all(hroot.join(base)).unwrap();
            #[cfg(unix)]
            std::os::unix::fs::symlink(&hid, hroot.join(base).join("device")).unwrap();
        }
        let dev = dir.path().join("devinput");
        std::fs::create_dir_all(&dev).unwrap();
        let dp = dev.to_str().unwrap().to_string();
        fake_mt_event(dir.path(), "event5", "Apple Inc. Magic Trackpad\n");
        let hr = hroot.to_str().unwrap();
        let ir = iroot.to_str().unwrap();
        let got = resolve_with_roots("/dev/hidraw0", None, hr, ir, &dp).unwrap();
        assert!(matches!(got.1, EvdevSource::NameScan(_)), "got {:?}", got.1);
        assert!(!grab_allowed(&got.1));
    }

    #[test]
    fn bt_single_node_refuses_wrong_vid() {
        // Non-trackpad BT VID (Logitech) → no BT trust even when 1×1.
        let dir = tempfile::tempdir().unwrap();
        let (hroot, iroot) = bt_single_hidraw_tree(dir.path(), "hidraw0", "0005:0000046D:0000C52B");
        let dev = dir.path().join("devinput");
        std::fs::create_dir_all(&dev).unwrap();
        let dp = dev.to_str().unwrap().to_string();
        fake_mt_event(dir.path(), "event5", "Apple Inc. Magic Trackpad\n");
        let got = resolve_with_roots("/dev/hidraw0", None, &hroot, &iroot, &dp).unwrap();
        // Falls to the generic name scan (untrusted) — never trusted.
        assert!(matches!(got.1, EvdevSource::NameScan(_)), "got {:?}", got.1);
        assert!(!grab_allowed(&got.1));
    }

    #[test]
    fn bt_usb_layout_never_uses_single_node_path() {
        // USB interface layout present → BT path abstains even with
        // BT-looking VID; the prefix walk (or name scan) decides.
        let dir = tempfile::tempdir().unwrap();
        let hroot = dir.path().join("hidraw");
        let iroot = dir.path().join("input");
        std::fs::create_dir_all(&hroot).unwrap();
        std::fs::create_dir_all(&iroot).unwrap();
        let usb = dir.path().join("devices/usb1");
        let if_dir = usb.join("if1");
        let hid = if_dir.join("hid1");
        std::fs::create_dir_all(&hid).unwrap();
        std::fs::write(if_dir.join("bInterfaceNumber"), "01\n").unwrap();
        std::fs::write(
            hid.join("uevent"),
            "HID_ID=0003:0000004C:00000265\nHID_NAME=Apple Inc. Magic Trackpad\n",
        )
        .unwrap();
        std::fs::create_dir_all(hroot.join("hidraw9")).unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(&hid, hroot.join("hidraw9").join("device")).unwrap();
        assert!(bt_single_node_walk(
            "/dev/hidraw9",
            hroot.to_str().unwrap(),
            iroot.to_str().unwrap(),
            "/dev/input"
        )
        .is_none());
    }

    #[test]
    fn env_override_empty_falls_through() {
        // Empty pin is ignored (falls to sysfs/name/None), never an
        // empty-path grab target.
        let dir = tempfile::tempdir().unwrap();
        let (hroot, iroot) = fake_hidraw_tree(dir.path());
        let dev = dir.path().join("devinput");
        std::fs::create_dir_all(&dev).unwrap();
        let dp = dev.to_str().unwrap();
        assert!(resolve_with_roots("/dev/hidraw0", Some(""), &hroot, &iroot, dp).is_none());
    }

    #[test]
    fn env_override_trusted_on_bt_topology() {
        // Operator pin wins even where BT auto-resolution would apply.
        let dir = tempfile::tempdir().unwrap();
        let (hroot, iroot) = bt_single_hidraw_tree(dir.path(), "hidraw0", "0005:0000004C:00000265");
        let dev = dir.path().join("devinput");
        std::fs::create_dir_all(&dev).unwrap();
        let dp = dev.to_str().unwrap().to_string();
        fake_mt_event(dir.path(), "event5", "Apple Inc. Magic Trackpad\n");
        let got = resolve_with_roots(
            "/dev/hidraw0",
            Some("/dev/input/event9"),
            &hroot,
            &iroot,
            &dp,
        )
        .unwrap();
        assert_eq!(got.0, PathBuf::from("/dev/input/event9"));
        assert!(matches!(got.1, EvdevSource::EnvOverride(_)));
        assert!(grab_allowed(&got.1));
    }

    #[test]
    fn grab_outcome_line_pins_format() {
        let ok = grab_outcome_line(
            "/dev/hidraw0",
            &EvdevSource::BtSingleNode("/dev/input/event5".into()),
            true,
            None,
        );
        assert!(ok.contains("grab ok"), "{ok}");
        assert!(ok.contains("/dev/hidraw0"), "{ok}");
        assert!(ok.contains("BtSingleNode"), "{ok}");
        let fail = grab_outcome_line(
            "/dev/hidraw0",
            &EvdevSource::NameScan("/dev/input/event5".into()),
            false,
            Some("EVIOCGRAB busy"),
        );
        assert!(fail.contains("grab fail"), "{fail}");
        assert!(
            fail.contains("passthrough") || fail.contains("EVIOCGRAB busy"),
            "{fail}"
        );
    }
}
