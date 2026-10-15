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
//! Scan ordering (load-bearing for the pump): USB interface 1 first
//! (old-driver rule: If1=pump, If2=actuator; owner hardware 2026-10-15:
//! silent hidraw8=If0, streaming hidraw9=If1, hidraw10=If2, hidraw11=If3).
//! MT abs-bit rank is a tiebreak only — If0 can carry MT-capable input
//! children yet stay silent, so MT proof alone misbinds. Numeric /
//! lexicographic order never decides (hidraw numbers shift across
//! enumeration).

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
    // so the multitouch USB interface (bInterfaceNumber == 1) comes first
    // — the old-driver rule (If1=pump, If2=actuator), hardware-proven on
    // owner MT2 USB-C 2026-10-15 (silent hidraw8=If0, streaming hidraw9=If1).
    // MT abs-bit rank is a tiebreak only: If0 can expose MT-capable input
    // children yet stay silent, so MT proof alone misbinds. Numeric /
    // lexicographic order never decides: hidraw numbers shift across
    // enumeration, and no trailing-index/devnode tiebreak is consulted.
    out.sort_by_cached_key(|d| {
        (
            iface_preference_with_root(&d.devnode, class_dir),
            rank_with_root(&d.devnode, class_dir),
        )
    });
    Ok(out)
}

/// Proven interface rank for hidraw selection (ordering key).
///
/// - `MtCapable`: an input child exposes multitouch abs bits — the only
///   rank trusted to stream touch reports.
/// - `InputOnly`: input children exist but none proves MT (silent
///   multitouch-less interface; owner hidraw8 class).
/// - `Bare`: no input children at all (feature-only hidraw10/11 class).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum InterfaceRank {
    MtCapable,
    InputOnly,
    Bare,
}

/// Rank a node with the production sysfs root.
pub fn rank_for_node(devnode: &str) -> InterfaceRank {
    rank_with_root(devnode, "/sys/class/hidraw")
}

fn rank_with_root(devnode: &str, class_root: &str) -> InterfaceRank {
    if has_mt_children(devnode, class_root) {
        InterfaceRank::MtCapable
    } else if has_input_children(devnode, class_root) {
        InterfaceRank::InputOnly
    } else {
        InterfaceRank::Bare
    }
}

/// Pump-selection preference for a hidraw node: `0` for USB interface 1
/// (the pump interface), `1` for anything else (other interface number,
/// unknown layout, BT). Lower sorts first; MT rank tiebreaks within a
/// preference tier. Pure ordering helper around
/// [`interface_number_with_root`].
fn iface_preference_with_root(devnode: &str, class_root: &str) -> u8 {
    match interface_number_with_root(devnode, class_root) {
        Some(1) => 0,
        _ => 1,
    }
}

/// Human-readable selection reason for the attach INFO log.
/// Production root; see [`selection_reason_with_root`] for tests.
pub fn selection_reason_for_node(devnode: &str) -> String {
    selection_reason_with_root(devnode, "/sys/class/hidraw")
}

pub(crate) fn selection_reason_with_root(devnode: &str, class_root: &str) -> String {
    let iface = interface_number_with_root(devnode, class_root);
    let basis = match iface {
        Some(1) => "interface-1 preferred (If1 pump rule)",
        _ => "not If1 (MT-rank tiebreak only)",
    };
    match rank_with_root(devnode, class_root) {
        InterfaceRank::MtCapable => {
            format!("{devnode}: interface {iface:?}, {basis}; MT-capable input children (abs MT bits present)")
        }
        InterfaceRank::InputOnly => {
            format!("{devnode}: interface {iface:?}, {basis}; input children without MT abs bits (silent-risk)")
        }
        InterfaceRank::Bare => {
            format!("{devnode}: interface {iface:?}, {basis}; no input children (feature-only interface?)")
        }
    }
}

/// Attach-time stream health for a chosen node vs siblings that stream.
///
/// - `Healthy`: the chosen node itself streamed during the probe.
/// - `SilentWhileSiblingStreams`: chosen silent, `streaming` sibling live —
///   the 2026-10-15 owner failure (silent hidraw8 bound, hidraw9 live).
/// - `IdleNoTraffic`: nothing streamed anywhere (finger off the pad is
///   normal — no warning, just debug).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StreamHealth {
    Healthy,
    SilentWhileSiblingStreams { streaming: String },
    IdleNoTraffic,
}

/// Pure decision: does the chosen node need a loud warn?
/// `streaming` lists nodes that produced a touch report during the probe.
pub fn check_stream_health(chosen: &str, streaming: &[String]) -> StreamHealth {
    if streaming.iter().any(|n| n == chosen) {
        return StreamHealth::Healthy;
    }
    match streaming.first() {
        Some(live) => StreamHealth::SilentWhileSiblingStreams {
            streaming: live.clone(),
        },
        None => StreamHealth::IdleNoTraffic,
    }
}

/// Nonblocking probe: true when a touch report arrives within `timeout_ms`.
/// Best-effort: open/read failures (vanished node, permissions) are `false`,
/// never an error. Callee opens its own fd so sibling polling never disturbs
/// the live pump fd.
pub fn poll_has_report(devnode: &str, timeout_ms: u64) -> bool {
    use std::os::unix::io::AsRawFd;
    let Ok(file) = std::fs::OpenOptions::new().read(true).open(devnode) else {
        return false;
    };
    let fd = file.as_raw_fd();
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags >= 0 {
        unsafe {
            libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK);
        }
    }
    let deadline = std::time::Instant::now() + std::time::Duration::from_millis(timeout_ms);
    let mut buf = [0u8; 512];
    while std::time::Instant::now() < deadline {
        let ret = unsafe { libc::read(fd, buf.as_mut_ptr().cast(), buf.len()) };
        if ret > 0 {
            return true;
        }
        if ret < 0 {
            let err = std::io::Error::last_os_error();
            if err.raw_os_error() == Some(libc::EAGAIN)
                || err.raw_os_error() == Some(libc::EWOULDBLOCK)
                || err.raw_os_error() == Some(libc::EINTR)
            {
                std::thread::sleep(std::time::Duration::from_millis(5));
                continue;
            }
            return false;
        }
        // 0-byte read: device dropped the node — not a stream.
        return false;
    }
    false
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

/// True when any input child of the hidraw node's HID device dir carries
/// multitouch abs bits (`ABS_MT_SLOT` / `ABS_MT_PRESSURE` in
/// `device/capabilities/abs`). This is the SELECTION-time MT proof: the
/// same bit test evdev applies at grab time, applied here to rank the
/// streaming interface first. Layout walked: `<hid>/input/inputN/
/// capabilities/abs` (two levels: `input/` container, then `inputN`
/// device dirs) plus the direct `<hid>/<inputN>/capabilities/abs` form
/// so fake trees and kernel variants both count.
fn has_mt_children(devnode: &str, class_root: &str) -> bool {
    let Some(base) = hidraw_name(devnode) else {
        return false;
    };
    let link = std::path::PathBuf::from(format!("{class_root}/{base}/device"));
    let Ok(canon) = std::fs::canonicalize(&link) else {
        return false;
    };
    let Ok(entries) = std::fs::read_dir(&canon) else {
        return false;
    };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if !name.starts_with("input") {
            continue;
        }
        let child = entry.path();
        // Direct form: <hid>/inputN/capabilities/abs.
        if let Ok(text) = std::fs::read_to_string(child.join("capabilities/abs")) {
            if crate::evdev::abs_has_mt(&text) {
                return true;
            }
        }
        // Container form: <hid>/input/inputN/capabilities/abs.
        if let Ok(nested) = std::fs::read_dir(&child) {
            for sub in nested.flatten() {
                if let Ok(text) = std::fs::read_to_string(sub.path().join("capabilities/abs")) {
                    if crate::evdev::abs_has_mt(&text) {
                        return true;
                    }
                }
            }
        }
    }
    false
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
        // Ready permanently, so input-having interfaces must sort before
        // the bare one. No interface layout here (all non-If1), so MT
        // rank tiebreaks; the bare node sorts last while the two tied
        // touch nodes may appear in either order (numeric never decides).
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
        let mut nodes: Vec<&str> = devs.iter().map(|d| d.devnode.as_str()).collect();
        let last = nodes.pop().unwrap();
        assert_eq!(last, "/dev/hidraw10");
        nodes.sort_unstable();
        assert_eq!(nodes, vec!["/dev/hidraw2", "/dev/hidraw8"]);
    }

    fn fake_trackpad_node_ranked(
        root: &std::path::Path,
        base: &str,
        hid_dir: &std::path::Path,
        input_kind: &str,
    ) {
        // input_kind: "bare" | "input" | "mt"
        std::fs::create_dir_all(root.join(base)).unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(hid_dir, root.join(base).join("device")).unwrap();
        std::fs::write(
            root.join(base).join("device/uevent"),
            "HID_ID=0003:000005AC:00000324\nHID_NAME=Apple Inc. Magic Trackpad\n",
        )
        .unwrap();
        match input_kind {
            "mt" => {
                let caps = hid_dir.join("input/input20/capabilities");
                std::fs::create_dir_all(&caps).unwrap();
                // Live owner abs value: carries bits 47 + 58.
                std::fs::write(caps.join("abs"), "673800001000003\n").unwrap();
            }
            "input" => {
                let caps = hid_dir.join("input/input21/capabilities");
                std::fs::create_dir_all(&caps).unwrap();
                // Input child, zero MT bits (silent-risk class).
                std::fs::write(caps.join("abs"), "0\n").unwrap();
            }
            _ => {}
        }
    }

    #[test]
    fn scan_mt_beats_numerically_first_input_only() {
        // Owner 2026-10-15: silent hidraw8 (input, no MT proof) sorted
        // before streaming hidraw9 (MT). Proven rank must invert that:
        // MT-capable wins regardless of trailing index.
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("hidraw");
        std::fs::create_dir_all(&root).unwrap();
        let hid_silent = dir.path().join("devices/silent");
        let hid_live = dir.path().join("devices/live");
        for d in [&hid_silent, &hid_live] {
            std::fs::create_dir_all(d).unwrap();
        }
        fake_trackpad_node_ranked(&root, "hidraw8", &hid_silent, "input");
        fake_trackpad_node_ranked(&root, "hidraw9", &hid_live, "mt");
        let devs = scan_sysfs(root.to_str().unwrap()).unwrap();
        let nodes: Vec<&str> = devs.iter().map(|d| d.devnode.as_str()).collect();
        assert_eq!(nodes, vec!["/dev/hidraw9", "/dev/hidraw8"]);
        assert_eq!(
            rank_with_root("/dev/hidraw9", root.to_str().unwrap()),
            InterfaceRank::MtCapable
        );
        assert_eq!(
            rank_with_root("/dev/hidraw8", root.to_str().unwrap()),
            InterfaceRank::InputOnly
        );
    }

    fn fake_iface_trackpad_node(
        root: &std::path::Path,
        base: &str,
        usb_dev: &std::path::Path,
        iface: u8,
        hid_leaf: &str,
        input_kind: &str,
    ) {
        // Owner USB layout: <usb_dev>/ifN/bInterfaceNumber + <usb_dev>/ifN/<hid>/...
        // `device` symlinks into the HID dir; the iface number is found by
        // walking up from the canonicalized HID dir.
        let if_dir = usb_dev.join(format!("if{iface}"));
        let hid_dir = if_dir.join(hid_leaf);
        std::fs::create_dir_all(&hid_dir).unwrap();
        std::fs::write(if_dir.join("bInterfaceNumber"), format!("{iface:02}\n")).unwrap();
        std::fs::create_dir_all(root.join(base)).unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(&hid_dir, root.join(base).join("device")).unwrap();
        std::fs::write(
            root.join(base).join("device/uevent"),
            "HID_ID=0003:000005AC:00000324\nHID_NAME=Apple Inc. Magic Trackpad\n",
        )
        .unwrap();
        match input_kind {
            "mt" => {
                let caps = hid_dir.join("input/input20/capabilities");
                std::fs::create_dir_all(&caps).unwrap();
                // Live owner abs value: carries bits 47 + 58.
                std::fs::write(caps.join("abs"), "673800001000003\n").unwrap();
            }
            "input" => {
                let caps = hid_dir.join("input/input21/capabilities");
                std::fs::create_dir_all(&caps).unwrap();
                std::fs::write(caps.join("abs"), "0\n").unwrap();
            }
            _ => {}
        }
    }

    #[test]
    fn scan_if1_beats_mt_capable_if0_owner_layout() {
        // Owner 2026-10-15 (MT2 USB-C 05ac:0324): hidraw8=If0 silent,
        // hidraw9=If1 STREAMS, hidraw10=If2, hidraw11=If3. Adversarial bit:
        // If0 carries MT-capable input children (the old ranker promoted
        // it) and is numerically first — If1 must still win on the
        // interface-number rule; MT rank is a tiebreak only and numeric
        // order never decides.
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("hidraw");
        std::fs::create_dir_all(&root).unwrap();
        let usb_dev = dir.path().join("devices/usb1");
        fake_iface_trackpad_node(&root, "hidraw8", &usb_dev, 0, "hid0", "mt");
        fake_iface_trackpad_node(&root, "hidraw9", &usb_dev, 1, "hid1", "mt");
        fake_iface_trackpad_node(&root, "hidraw10", &usb_dev, 2, "hid2", "bare");
        fake_iface_trackpad_node(&root, "hidraw11", &usb_dev, 3, "hid3", "bare");
        let rs = root.to_str().unwrap();
        assert_eq!(interface_number_with_root("/dev/hidraw8", rs), Some(0));
        assert_eq!(interface_number_with_root("/dev/hidraw9", rs), Some(1));
        assert_eq!(interface_number_with_root("/dev/hidraw10", rs), Some(2));
        assert_eq!(interface_number_with_root("/dev/hidraw11", rs), Some(3));
        let devs = scan_sysfs(rs).unwrap();
        let nodes: Vec<&str> = devs.iter().map(|d| d.devnode.as_str()).collect();
        // If1 pump node first; silent If0 second (MT tiebreak over the
        // bare feature interfaces); bare If2/If3 trail in either order.
        assert_eq!(nodes[0], "/dev/hidraw9");
        assert_eq!(nodes[1], "/dev/hidraw8");
        let mut tail = vec![nodes[2], nodes[3]];
        tail.sort_unstable();
        assert_eq!(tail, vec!["/dev/hidraw10", "/dev/hidraw11"]);
        // Selection basis names the interface rule.
        let reason = selection_reason_with_root("/dev/hidraw9", rs);
        assert!(reason.contains("/dev/hidraw9"), "{reason}");
        assert!(reason.contains("interface-1 preferred"), "{reason}");
        assert!(reason.contains("Some(1)"), "{reason}");
        let silent = selection_reason_with_root("/dev/hidraw8", rs);
        assert!(silent.contains("not If1"), "{silent}");
    }

    #[test]
    fn scan_rank_tiers_mt_then_input_then_bare() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("hidraw");
        std::fs::create_dir_all(&root).unwrap();
        let hid_bare = dir.path().join("devices/bare");
        let hid_input = dir.path().join("devices/inp");
        let hid_mt = dir.path().join("devices/mt");
        for d in [&hid_bare, &hid_input, &hid_mt] {
            std::fs::create_dir_all(d).unwrap();
        }
        // Numeric order deliberately adversarial: bare=1, input=2, mt=3.
        fake_trackpad_node_ranked(&root, "hidraw1", &hid_bare, "bare");
        fake_trackpad_node_ranked(&root, "hidraw2", &hid_input, "input");
        fake_trackpad_node_ranked(&root, "hidraw3", &hid_mt, "mt");
        let devs = scan_sysfs(root.to_str().unwrap()).unwrap();
        let nodes: Vec<&str> = devs.iter().map(|d| d.devnode.as_str()).collect();
        assert_eq!(nodes, vec!["/dev/hidraw3", "/dev/hidraw2", "/dev/hidraw1"]);
    }

    #[test]
    fn selection_reason_names_rank() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("hidraw");
        std::fs::create_dir_all(&root).unwrap();
        let hid_bare = dir.path().join("devices/b");
        let hid_input = dir.path().join("devices/i");
        let hid_mt = dir.path().join("devices/m");
        for d in [&hid_bare, &hid_input, &hid_mt] {
            std::fs::create_dir_all(d).unwrap();
        }
        fake_trackpad_node_ranked(&root, "hidraw1", &hid_bare, "bare");
        fake_trackpad_node_ranked(&root, "hidraw2", &hid_input, "input");
        fake_trackpad_node_ranked(&root, "hidraw3", &hid_mt, "mt");
        let rs = root.to_str().unwrap();
        assert!(selection_reason_with_root("/dev/hidraw3", rs).contains("MT-capable"));
        assert!(selection_reason_with_root("/dev/hidraw2", rs).contains("without MT"));
        assert!(selection_reason_with_root("/dev/hidraw1", rs).contains("no input"));
        // Reason embeds the node: the attach INFO log names the node.
        assert!(selection_reason_with_root("/dev/hidraw3", rs).contains("/dev/hidraw3"));
    }

    #[test]
    fn stream_health_warns_on_silent_chosen() {
        // Chosen streaming → healthy, no warn.
        assert_eq!(
            check_stream_health("/dev/hidraw9", &["/dev/hidraw9".to_string()]),
            StreamHealth::Healthy
        );
        // Owner failure: chosen hidraw8 silent, hidraw9 live → loud-warn path.
        assert_eq!(
            check_stream_health("/dev/hidraw8", &["/dev/hidraw9".to_string()]),
            StreamHealth::SilentWhileSiblingStreams {
                streaming: "/dev/hidraw9".to_string(),
            }
        );
        // Finger off: nothing anywhere → idle, no warn.
        assert_eq!(
            check_stream_health("/dev/hidraw8", &[]),
            StreamHealth::IdleNoTraffic
        );
    }

    #[test]
    fn poll_missing_node_is_silent_not_error() {
        assert!(!poll_has_report("/nonexistent-hidraw-xyz", 20));
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
