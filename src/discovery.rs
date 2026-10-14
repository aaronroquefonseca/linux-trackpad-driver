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
    out.sort_by(|a, b| a.devnode.cmp(&b.devnode));
    Ok(out)
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
/// Walks `<class_root>/<hidraw>/device` up to 3 parents looking for a
/// `bInterfaceNumber` attribute (the HID device itself has none; the USB
/// interface parent does). Returns `None` for BT nodes / unknown layouts —
/// callers treat that as "route the full idempotent sequence anyway".
pub fn interface_number_for_node(devnode: &str) -> Option<u8> {
    interface_number_with_root(devnode, "/sys/class/hidraw")
}

fn interface_number_with_root(devnode: &str, class_root: &str) -> Option<u8> {
    let name = hidraw_name(devnode)?;
    let mut dir = std::path::PathBuf::from(format!("{class_root}/{name}/device"));
    for _ in 0..4 {
        let candidate = dir.join("bInterfaceNumber");
        if let Ok(text) = std::fs::read_to_string(&candidate) {
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
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("hidraw");
        std::fs::create_dir_all(root.join("hidraw7/device")).unwrap();
        std::fs::write(root.join("hidraw7/device/bInterfaceNumber"), "02\n").unwrap();
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
    fn ready_waits_for_both_signals() {
        // /dev/null exists but has no hidraw sysfs entry: node true, bind false.
        let (node, bound) = wait_for_ready_with_root("/dev/null", 60, "/nonexistent-xyz");
        assert!(node);
        assert!(!bound);
        let (node, _) = wait_for_ready_with_root("/nonexistent-hidraw-xyz", 60, "/nonexistent-xyz");
        assert!(!node);
    }
}
