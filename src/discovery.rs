//! Device discovery via sysfs (`/sys/class/hidraw`), no libudev dependency.
//!
//! Scans hidraw nodes, reads each node's `device/uevent` for `HID_ID`, and
//! keeps Apple Magic Trackpad 2 nodes (Lightning `05ac:0265` / `004c:0265`,
//! USB-C `05ac:0324` / `004c:0324`). Graceful with no hardware: empty list.
//!
//! Ordering note (probe race): a freshly added node may not yet be bound to
//! `hid-magicmouse`. [`wait_for_ready`] polls for the devnode *and* the driver
//! bind so callers can defer past the probe window before the first
//! `HIDIOCSFEATURE`; init itself retries with backoff
//! (see [`crate::init::send_with_retry`]) and verifies via `HIDIOCGFEATURE`.
//! Per-interface routing groundwork (USB If1 vs If2): init stays idempotent
//! per node (safe to re-run whichever interface node udev hands us), and
//! [`interface_number_for_node`] records which interface accepted multitouch
//! for diagnostics. The reference routes multitouch+polling to If1 and
//! host-click to If2; until sibling discovery lands, we send the full sequence
//! to the given node (device ignores what is not for it) and log the iface.
//!
//! Scan ordering (load-bearing for the pump): input-having interfaces
//! first, then numeric trailing index. The daemon binds the first `Ready`
//! permanently, so a lexicographic sort would hand the pump to hidraw10 —
//! an input-less feature interface with no touch reports.

use anyhow::Result;

use crate::init::Transport;

/// Apple vendor IDs: USB (`05ac`) and Bluetooth (`004c`).
pub const VID_APPLE_USB: u16 = 0x05ac;
pub const VID_APPLE_BT: u16 = 0x004c;
/// Supported product IDs: Lightning MT2 (`0265`), USB-C MT2 (`0324`).
pub const SUPPORTED_PIDS: &[u16] = &[0x0265, 0x0324];

/// HID bus types from `HID_ID` field 0: USB (`0003`), Bluetooth (`0005`).
pub const BUS_USB: u16 = 0x0003;
pub const BUS_BLUETOOTH: u16 = 0x0005;

/// One matching hidraw node.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiscoveredDevice {
    pub devnode: String,
    pub vendor_id: u16,
    pub product_id: u16,
    pub transport: Transport,
}

/// Parse a `HID_ID` value (`bus:vid:pid`, hex, zero-padded) into
/// `(bus, vendor, product)`. Pure, unit-tested.
pub fn parse_hid_id(s: &str) -> Option<(u16, u16, u16)> {
    let mut parts = s.trim().split(':');
    let bus = parts.next()?;
    let vid = parts.next()?;
    let pid = parts.next()?;
    if parts.next().is_some() {
        return None;
    }
    let num = |h: &str| u16::from_str_radix(h.trim_start_matches('0').max("0"), 16).ok();
    Some((num(bus)?, num(vid)?, num(pid)?))
}

/// True for Apple Trackpad 2 VID:PID pairs on either transport.
pub fn is_supported_trackpad(vendor_id: u16, product_id: u16) -> bool {
    (vendor_id == VID_APPLE_USB || vendor_id == VID_APPLE_BT)
        && SUPPORTED_PIDS.contains(&product_id)
}

/// Classify transport: BT vendor ID or BT bus wins; default USB.
pub fn transport_for(vendor_id: u16, bus: u16) -> Transport {
    if vendor_id == VID_APPLE_BT || bus == BUS_BLUETOOTH {
        Transport::Bluetooth
    } else {
        Transport::Usb
    }
}

/// Extract `HID_ID=...` from uevent-format text. Pure, unit-tested.
pub fn hid_id_from_uevent(text: &str) -> Option<String> {
    text.lines().find_map(|line| {
        line.strip_prefix("HID_ID=")
            .map(str::trim)
            .filter(|v| !v.is_empty())
            .map(str::to_string)
    })
}

/// Scan `/sys/class/hidraw` for supported trackpad nodes.
/// Returns an empty vec (not an error) when no hardware is present.
pub fn scan_trackpads() -> Result<Vec<DiscoveredDevice>> {
    scan_sysfs("/sys/class/hidraw")
}

fn scan_sysfs(class_dir: &str) -> Result<Vec<DiscoveredDevice>> {
    let mut out = Vec::new();
    let entries = match std::fs::read_dir(class_dir) {
        Ok(e) => e,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(anyhow::Error::from(e)),
    };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if !name.starts_with("hidraw") {
            continue;
        }
        let uevent_path = format!("{class_dir}/{name}/device/uevent");
        let Ok(text) = std::fs::read_to_string(&uevent_path) else {
            continue;
        };
        let (Some(hid_id), devnode) = (hid_id_from_uevent(&text), format!("/dev/{name}")) else {
            continue;
        };
        let Some((bus, vid, pid)) = parse_hid_id(&hid_id) else {
            continue;
        };
        if !is_supported_trackpad(vid, pid) {
            continue;
        }
        out.push(DiscoveredDevice {
            devnode,
            vendor_id: vid,
            product_id: pid,
            transport: transport_for(vid, bus),
        });
    }
    // Order matters: the daemon inits (and the pump binds) in scan order,
    // so the multitouch interface must come first. Lexicographic sort
    // puts hidraw10 before hidraw8 (and event10 before event2) — and
    // hidraw10/11 are input-less feature interfaces that would bind the
    // pump to a node with no touch reports. Input-having nodes first,
    // then numeric trailing index, then full string as tiebreak.
    out.sort_by_cached_key(|d| {
        (
            !has_input_children(&d.devnode, class_dir),
            trailing_int(&d.devnode).unwrap_or(u64::MAX),
            d.devnode.clone(),
        )
    });
    Ok(out)
}

/// Trailing decimal integer of a device path (`/dev/hidraw10` → 10).
/// Paths without one sort after numbered ones (as `u64::MAX`).
fn trailing_int(path: &str) -> Option<u64> {
    let digits: String = path
        .chars()
        .rev()
        .take_while(|c| c.is_ascii_digit())
        .collect();
    if digits.is_empty() {
        return None;
    }
    digits.chars().rev().collect::<String>().parse().ok()
}

/// Sort device paths numerically by trailing integer
/// (`event2 < event10`; lexicographic would invert them), then
/// lexicographically as tiebreak. Shared with the evdev eventN hits.
pub(crate) fn sort_numerically(v: &mut [String]) {
    v.sort_by(|a, b| {
        trailing_int(a)
            .unwrap_or(u64::MAX)
            .cmp(&trailing_int(b).unwrap_or(u64::MAX))
            .then_with(|| a.cmp(b))
    });
}

/// True when the hidraw node's HID device dir has `input/` children —
/// i.e. this interface actually feeds the input subsystem (the
/// multitouch interface). Feature-only USB interfaces (owner hardware:
/// hidraw10/11) have none and must sort after the touch interface.
fn has_input_children(devnode: &str, class_root: &str) -> bool {
    let Some(base) = hidraw_name(devnode) else {
        return false;
    };
    let link = std::path::PathBuf::from(format!("{class_root}/{base}/device"));
    let Ok(canon) = std::fs::canonicalize(&link) else {
        return false;
    };
    std::fs::read_dir(&canon)
        .map(|entries| {
            entries
                .flatten()
                .any(|e| e.file_name().to_string_lossy().starts_with("input"))
        })
        .unwrap_or(false)
}

/// Wait for a devnode path to appear (probe/bind race), polling every
/// 50 ms up to `timeout_ms`. Returns `true` if the node exists.
pub fn wait_for_node(devnode: &str, timeout_ms: u64) -> bool {
    wait_for_node_with_deadline(devnode, timeout_ms)
}

fn wait_for_node_with_deadline(devnode: &str, timeout_ms: u64) -> bool {
    let waited = std::time::Instant::now();
    let budget = std::time::Duration::from_millis(timeout_ms);
    while waited.elapsed() < budget {
        if std::path::Path::new(devnode).exists() {
            return true;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    std::path::Path::new(devnode).exists()
}

/// Basename of a hidraw devnode (`/dev/hidraw3` → `Some("hidraw3")`).
/// Returns `None` for non-hidraw paths.
pub fn hidraw_name(devnode: &str) -> Option<String> {
    let base = devnode.rsplit('/').next()?;
    if base.starts_with("hidraw") && base.len() > "hidraw".len() {
        Some(base.to_string())
    } else {
        None
    }
}

/// Read a `bInterfaceNumber` attribute file (`"01\n"` → `Some(1)`).
/// Pure, unit-tested.
pub fn parse_interface_number(text: &str) -> Option<u8> {
    let t = text.trim().trim_start_matches('0');
    let t = if t.is_empty() { "0" } else { t };
    t.parse::<u8>().ok()
}

/// USB interface number for a hidraw node, via sysfs.
///
/// `{class_root}/{hidraw}/device` is a symlink into the device hierarchy, so
/// it is canonicalized first and then the resolved dir plus up to 3 parents
/// are probed for `bInterfaceNumber` (the HID device itself has none; the USB
/// interface parent does). Returns `None` for BT nodes / unknown layouts —
/// callers treat that as "route the full idempotent sequence anyway".
pub fn interface_number_for_node(devnode: &str) -> Option<u8> {
    interface_number_with_root(devnode, "/sys/class/hidraw")
}

fn interface_number_with_root(devnode: &str, class_root: &str) -> Option<u8> {
    let name = hidraw_name(devnode)?;
    let link = std::path::PathBuf::from(format!("{class_root}/{name}/device"));
    // Canonicalize: `device` is a symlink; textual parent-walking from the
    // class branch would never reach the device hierarchy where
    // `bInterfaceNumber` actually lives.
    let mut dir = std::fs::canonicalize(&link).ok()?;
    for _ in 0..4 {
        if let Ok(text) = std::fs::read_to_string(dir.join("bInterfaceNumber")) {
            if let Some(n) = parse_interface_number(&text) {
                return Some(n);
            }
        }
        if !dir.pop() {
            return None;
        }
    }
    None
}

/// True when the hidraw node's HID device is bound to a driver
/// (`.../device/driver` symlink exists). Best-effort: `false` on any error.
pub fn driver_bound_for_node(devnode: &str) -> bool {
    driver_bound_with_root(devnode, "/sys/class/hidraw")
}

fn driver_bound_with_root(devnode: &str, class_root: &str) -> bool {
    let Some(name) = hidraw_name(devnode) else {
        return false;
    };
    let link = std::path::PathBuf::from(format!("{class_root}/{name}/device/driver"));
    std::fs::symlink_metadata(&link).is_ok()
}

/// Defer past the probe/bind race: wait for the node *and* the driver bind.
/// Returns `(node_present, driver_bound)`; callers proceed with retrying
/// writes when the node exists even if the bind is not yet visible (the
/// bind symlink layout varies across kernels).
pub fn wait_for_ready(devnode: &str, timeout_ms: u64) -> (bool, bool) {
    wait_for_ready_with_root(devnode, timeout_ms, "/sys/class/hidraw")
}

fn wait_for_ready_with_root(devnode: &str, timeout_ms: u64, class_root: &str) -> (bool, bool) {
    let waited = std::time::Instant::now();
    let budget = std::time::Duration::from_millis(timeout_ms);
    let mut ready = (
        std::path::Path::new(devnode).exists(),
        driver_bound_with_root(devnode, class_root),
    );
    while waited.elapsed() < budget {
        let node = std::path::Path::new(devnode).exists();
        let bound = driver_bound_with_root(devnode, class_root);
        ready = (node, bound);
        if node && bound {
            return ready;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    ready
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_hid_id_forms() {
        assert_eq!(
            parse_hid_id("0003:000005AC:00000324"),
            Some((3, 0x05ac, 0x0324))
        );
        assert_eq!(
            parse_hid_id("0005:0000004C:00000324"),
            Some((5, 0x004c, 0x0324))
        );
        assert_eq!(parse_hid_id("0003:05ac:0265"), Some((3, 0x05ac, 0x0265)));
        assert_eq!(parse_hid_id("garbage"), None);
        assert_eq!(parse_hid_id("0003:05ac"), None);
    }

    #[test]
    fn support_matrix() {
        assert!(is_supported_trackpad(0x05ac, 0x0265));
        assert!(is_supported_trackpad(0x05ac, 0x0324));
        assert!(is_supported_trackpad(0x004c, 0x0265));
        assert!(is_supported_trackpad(0x004c, 0x0324));
        assert!(!is_supported_trackpad(0x05ac, 0x1234));
        assert!(!is_supported_trackpad(0x046d, 0x0324));
    }

    #[test]
    fn transport_classification() {
        assert_eq!(transport_for(0x004c, BUS_BLUETOOTH), Transport::Bluetooth);
        assert_eq!(transport_for(0x004c, BUS_USB), Transport::Bluetooth);
        assert_eq!(transport_for(0x05ac, BUS_USB), Transport::Usb);
    }

    #[test]
    fn uevent_extraction() {
        let text = "DRIVER=hid-generic\nHID_ID=0003:000005AC:00000324\nHID_NAME=foo\n";
        assert_eq!(
            hid_id_from_uevent(text).as_deref(),
            Some("0003:000005AC:00000324")
        );
        assert_eq!(hid_id_from_uevent("NOPE=1\n"), None);
    }

    #[test]
    fn missing_sysfs_is_empty_not_error() {
        let devs = scan_sysfs("/nonexistent-class-dir-xyz").unwrap();
        assert!(devs.is_empty());
    }

    #[test]
    fn wait_for_node_missing_times_out() {
        assert!(!wait_for_node("/nonexistent-hidraw-xyz", 60));
    }

    #[test]
    fn wait_for_node_present_returns_fast() {
        assert!(wait_for_node("/dev/null", 1000));
    }

    #[test]
    fn hidraw_name_basename() {
        assert_eq!(hidraw_name("/dev/hidraw3").as_deref(), Some("hidraw3"));
        assert_eq!(hidraw_name("/dev/input/event5"), None);
        assert_eq!(hidraw_name(""), None);
    }

    #[test]
    fn parses_interface_numbers() {
        assert_eq!(parse_interface_number("01\n"), Some(1));
        assert_eq!(parse_interface_number("2"), Some(2));
        assert_eq!(parse_interface_number("0"), Some(0));
        assert_eq!(parse_interface_number("nope"), None);
    }

    #[test]
    fn interface_and_bind_from_fake_sysfs() {
        // Mirror real sysfs: `device` is a symlink into the device hierarchy;
        // `bInterfaceNumber` lives on the USB-interface parent, never on the
        // HID device dir itself.
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("hidraw");
        let hiddev = root.join("devices/usb-iface/hiddev0");
        std::fs::create_dir_all(&hiddev).unwrap();
        std::fs::write(root.join("devices/usb-iface/bInterfaceNumber"), "02\n").unwrap();
        std::fs::create_dir_all(root.join("hidraw7")).unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink("../devices/usb-iface/hiddev0", root.join("hidraw7/device"))
            .unwrap();
        assert_eq!(
            interface_number_with_root("/dev/hidraw7", root.to_str().unwrap()),
            Some(2)
        );
        assert!(!driver_bound_with_root(
            "/dev/hidraw7",
            root.to_str().unwrap()
        ));
        #[cfg(unix)]
        std::os::unix::fs::symlink("/tmp", root.join("hidraw7/device/driver")).unwrap();
        assert!(driver_bound_with_root(
            "/dev/hidraw7",
            root.to_str().unwrap()
        ));
        assert_eq!(
            interface_number_with_root("/dev/hidraw9", root.to_str().unwrap()),
            None
        );
    }

    #[test]
    fn interface_walk_ignores_class_branch_attrs() {
        // A `bInterfaceNumber` sitting next to the symlink (i.e. on the class
        // branch, a layout that never exists) must NOT be picked up: only
        // the canonicalized device hierarchy counts.
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("hidraw");
        let hiddev = root.join("devices/hiddev1");
        std::fs::create_dir_all(&hiddev).unwrap();
        std::fs::create_dir_all(root.join("hidraw8")).unwrap();
        std::fs::write(root.join("hidraw8/bInterfaceNumber"), "01\n").unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink("../devices/hiddev1", root.join("hidraw8/device")).unwrap();
        assert_eq!(
            interface_number_with_root("/dev/hidraw8", root.to_str().unwrap()),
            None
        );
    }

    #[test]
    fn trailing_int_parses_or_abstains() {
        assert_eq!(trailing_int("/dev/hidraw10"), Some(10));
        assert_eq!(trailing_int("/dev/input/event2"), Some(2));
        assert_eq!(trailing_int("/dev/hidraw"), None);
        assert_eq!(trailing_int(""), None);
    }

    #[test]
    fn sort_numerically_fixes_lexicographic_trap() {
        let mut v = vec![
            "/dev/input/event13".to_string(),
            "/dev/input/event2".to_string(),
            "/dev/input/event10".to_string(),
        ];
        sort_numerically(&mut v);
        assert_eq!(
            v,
            vec![
                "/dev/input/event2",
                "/dev/input/event10",
                "/dev/input/event13"
            ]
        );
    }

    fn fake_trackpad_node(
        root: &std::path::Path,
        base: &str,
        hid_dir: &std::path::Path,
        with_input: bool,
    ) {
        std::fs::create_dir_all(root.join(base)).unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(hid_dir, root.join(base).join("device")).unwrap();
        std::fs::write(
            root.join(base).join("device/uevent"),
            "HID_ID=0003:000005AC:00000324\nHID_NAME=Apple Inc. Magic Trackpad\n",
        )
        .unwrap();
        if with_input {
            std::fs::create_dir_all(hid_dir.join("input/input15")).unwrap();
        }
    }

    #[test]
    fn scan_prefers_input_having_node_numerically() {
        // Lexicographic order would be hidraw10, hidraw2, hidraw8: the
        // input-less feature interface first. The pump binds the first
        // Ready permanently, so the touch interfaces must win, numerically.
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("hidraw");
        std::fs::create_dir_all(&root).unwrap();
        let hid_plain = dir.path().join("devices/plain");
        let hid_touch_a = dir.path().join("devices/touch-a");
        let hid_touch_b = dir.path().join("devices/touch-b");
        for d in [&hid_plain, &hid_touch_a, &hid_touch_b] {
            std::fs::create_dir_all(d).unwrap();
        }
        fake_trackpad_node(&root, "hidraw10", &hid_plain, false);
        fake_trackpad_node(&root, "hidraw8", &hid_touch_a, true);
        fake_trackpad_node(&root, "hidraw2", &hid_touch_b, true);
        let devs = scan_sysfs(root.to_str().unwrap()).unwrap();
        let nodes: Vec<&str> = devs.iter().map(|d| d.devnode.as_str()).collect();
        assert_eq!(nodes, vec!["/dev/hidraw2", "/dev/hidraw8", "/dev/hidraw10"]);
    }

    #[test]
    fn ready_waits_for_both_signals() {
        // /dev/null exists but has no hidraw sysfs entry: node true, bind false.
        let (node, bound) = wait_for_ready_with_root("/dev/null", 60, "/nonexistent-xyz");
        assert!(node);
        assert!(!bound);
        let (node, _) = wait_for_ready_with_root("/nonexistent-hidraw-xyz", 60, "/nonexistent-xyz");
        assert!(!node);
    }
}
