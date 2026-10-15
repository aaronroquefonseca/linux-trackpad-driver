//! hidraw → evdev (`/dev/input/eventN`) resolution.
//!
//! Chain (first hit wins):
//! 1. `$LTD_EVDEV_NODE` override when set + non-empty (explicit operator pin).
//! 2. Sysfs prefix walk: the input event whose canonical `device` dir sits
//!    under the hidraw node's canonical HID device dir. Precise per-node:
//!    on owner hardware hidraw8 (If1) → event13, hidraw9 → event12
//!    (verified live 2026-10-15: event13 under `...:1.0/...0009`,
//!    event12 under `...:1.1/...000A`). Interfaces without input
//!    children (hidraw10/11) resolve to nothing here and fall through.
//! 3. Name scan fallback: first `/dev/input/eventN` whose sysfs
//!    `device/name` contains `Magic Trackpad` or `Apple`. Multi-device
//!    unsafe (first hit wins); logged loudly so a two-pad rig notices.
//! 4. `None` → caller stays passthrough with a clear log (never guesses
//!    `event0` roulette).
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
    NameScan(PathBuf),
}

/// Production entry: env → sysfs walk → name scan.
pub fn resolve_evdev_node(hidraw: &str) -> Option<(PathBuf, EvdevSource)> {
    let env = std::env::var(EVDEV_ENV).ok().filter(|v| !v.is_empty());
    resolve_with_roots(
        hidraw,
        env.as_deref(),
        "/sys/class/hidraw",
        "/sys/class/input",
        "/dev/input",
    )
}

/// Injectable core: `env_override` instead of reading env (tests),
/// plus sysfs/dev roots.
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
    if let Some(p) = name_scan(input_root, dev_prefix) {
        log::warn!(
            "evdev: sysfs walk missed for {hidraw}; name-scan fallback → {p:?} (multi-device unsafe, first hit wins)"
        );
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

fn sysfs_walk(
    hidraw: &str,
    hidraw_root: &str,
    input_root: &str,
    dev_prefix: &str,
) -> Option<PathBuf> {
    let hid_dir = hid_device_dir(hidraw, hidraw_root)?;
    let entries = std::fs::read_dir(input_root).ok()?;
    let mut hits: Vec<PathBuf> = Vec::new();
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
            hits.push(PathBuf::from(format!("{dev_prefix}/{name}")));
        }
    }
    hits.sort();
    hits.into_iter().next()
}

fn name_scan(input_root: &str, dev_prefix: &str) -> Option<PathBuf> {
    let entries = std::fs::read_dir(input_root).ok()?;
    let mut names: Vec<String> = Vec::new();
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.starts_with("event") {
            names.push(name);
        }
    }
    names.sort();
    for name in names {
        let name_file = PathBuf::from(format!("{input_root}/{name}/device/name"));
        let Ok(text) = std::fs::read_to_string(&name_file) else {
            continue;
        };
        if text.contains("Magic Trackpad") || text.contains("Apple") {
            return Some(PathBuf::from(format!("{dev_prefix}/{name}")));
        }
    }
    None
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
        // event with Apple name elsewhere.
        let other = dir.path().join("devices/other/input/input9");
        std::fs::create_dir_all(&other).unwrap();
        std::fs::create_dir_all(dir.path().join("input/event12")).unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(&other, dir.path().join("input/event12/device")).unwrap();
        std::fs::write(
            dir.path().join("input/event12/device/name"),
            "Apple Inc. Magic Trackpad\n",
        )
        .unwrap();
        let got = resolve_with_roots("/dev/hidraw10", None, &hroot, &iroot, &dp).unwrap();
        assert_eq!(got.0, PathBuf::from(format!("{dp}/event12")));
        assert!(matches!(got.1, EvdevSource::NameScan(_)));
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
}
