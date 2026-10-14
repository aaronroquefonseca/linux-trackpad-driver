//! Device discovery via sysfs (`/sys/class/hidraw`), no libudev dependency.
//!
//! Scans hidraw nodes, reads each node's `device/uevent` for `HID_ID`, and
//! keeps Apple Magic Trackpad 2 nodes (Lightning `05ac:0265` / `004c:0265`,
//! USB-C `05ac:0324` / `004c:0324`). Graceful with no hardware: empty list.
//!
//! Ordering note (probe race): a freshly added node may not yet be bound to
//! `hid-magicmouse`. [`wait_for_node`] polls for the devnode so callers can
//! wait before the first `HIDIOCSFEATURE`; init itself retries with backoff
//! (see [`crate::init::send_with_retry`]). Per-interface routing (USB If1 vs
//! If2) is intentionally NOT done here — init is idempotent per node and the
//! Phase-1 `reconnect-bt` task owns refined routing.

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
}
