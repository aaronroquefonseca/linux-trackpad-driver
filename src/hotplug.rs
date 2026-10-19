//! Kernel hotplug wakeups and bounded reconciliation scheduling.
//! Events only request a trusted sysfs rescan; no event supplies a devnode
//! to initialize or grab. Full hardware verification retains its own cadence.

use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::time::{Duration, Instant};

use crate::discovery;

pub const LOOP_INTERVAL: Duration = Duration::from_millis(100);
const COALESCE: Duration = Duration::from_millis(100);
const SETTLE: Duration = Duration::from_secs(1);
const RETRY_WINDOW: Duration = Duration::from_secs(10);
const FIRST_RETRY: Duration = Duration::from_millis(250);
// Leave room for the 100 ms daemon loop while keeping response below 1 s.
const MAX_RETRY: Duration = Duration::from_millis(800);
const VERIFY_INTERVAL: Duration = Duration::from_secs(crate::resume::RESUME_REVERIFY_INTERVAL_SECS);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Work {
    Hotplug,
    Verify,
}

/// One scheduler for the daemon loop, without a worker/thread per event.
/// All time is supplied by the caller so latency and retry bounds are testable.
pub struct Schedule {
    next_verify: Instant,
    next_hotplug: Option<Instant>,
    settle_until: Instant,
    retry_until: Option<Instant>,
    retry_delay: Duration,
}

impl Schedule {
    pub fn new(now: Instant) -> Self {
        Self {
            next_verify: now + VERIFY_INTERVAL,
            next_hotplug: None,
            settle_until: now,
            retry_until: None,
            retry_delay: FIRST_RETRY,
        }
    }

    pub fn wake(&mut self, now: Instant) {
        // A burst cannot keep postponing its first scan.
        let due = now + COALESCE;
        self.next_hotplug = Some(self.next_hotplug.map_or(due, |old| old.min(due)));
        self.settle_until = now + SETTLE;
        self.retry_until = Some(now + RETRY_WINDOW);
        self.retry_delay = FIRST_RETRY;
    }

    pub fn take_due(&mut self, now: Instant, running: bool) -> Option<Work> {
        if !running {
            return None;
        }
        let work = if now >= self.next_verify {
            self.next_verify = now + VERIFY_INTERVAL;
            Work::Verify
        } else if self.next_hotplug.is_some_and(|due| now >= due) {
            Work::Hotplug
        } else {
            return None;
        };
        self.next_hotplug = None;
        Some(work)
    }

    /// Return true once when a pending burst exhausts its fast retry budget.
    pub fn completed(&mut self, now: Instant, pending: bool) -> bool {
        // Startup and periodic discovery can also find a transient failure.
        if pending && self.retry_until.is_none() {
            self.retry_until = Some(now + RETRY_WINDOW);
            self.retry_delay = FIRST_RETRY;
        }
        if let Some(until) = self.retry_until {
            if now < until && (pending || now < self.settle_until) {
                self.next_hotplug = Some((now + self.retry_delay).min(until));
                self.retry_delay = (self.retry_delay * 2).min(MAX_RETRY);
                return false;
            }
            self.retry_until = None;
            self.next_hotplug = None;
            return pending;
        }
        false
    }
}

/// Match only supported HID identities in kernel input/hidraw/HID paths.
/// A late input/eventN or actuator sibling notification wakes the same scan.
fn relevant_event(packet: &[u8], sender_port: u32) -> bool {
    if sender_port != 0 || packet.last() != Some(&0) {
        return false;
    }
    let Ok(text) = std::str::from_utf8(packet) else {
        return false;
    };
    let fields: Vec<&str> = text.split('\0').collect();
    let field = |key: &str| fields.iter().find_map(|s| s.strip_prefix(key));
    if !matches!(
        field("ACTION="),
        Some("add" | "remove" | "change" | "bind" | "unbind")
    ) || !matches!(field("SUBSYSTEM="), Some("hidraw" | "input" | "hid"))
    {
        return false;
    }
    field("DEVPATH=").is_some_and(|path| {
        path.split('/').any(|part| {
            let identity = part.split('.').next().unwrap_or(part);
            discovery::parse_hid_id(identity).is_some_and(|(bus, vendor, product)| {
                matches!(bus, discovery::BUS_USB | discovery::BUS_BLUETOOTH)
                    && discovery::is_supported_trackpad(vendor, product)
            })
        })
    })
}

/// Nonblocking kernel multicast listener. OwnedFd closes every failure path.
/// Bound before the boot scan so arrivals during startup remain queued.
pub struct Monitor(OwnedFd);

impl Monitor {
    pub fn open() -> io::Result<Self> {
        let fd = unsafe {
            libc::socket(
                libc::AF_NETLINK,
                libc::SOCK_DGRAM | libc::SOCK_NONBLOCK | libc::SOCK_CLOEXEC,
                libc::NETLINK_KOBJECT_UEVENT,
            )
        };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        let fd = unsafe { OwnedFd::from_raw_fd(fd) };
        // Startup/init can briefly occupy the daemon while siblings arrive.
        // The kernel may clamp this request; overflow still triggers a scan.
        let receive_bytes: libc::c_int = 1024 * 1024;
        let _ = unsafe {
            libc::setsockopt(
                fd.as_raw_fd(),
                libc::SOL_SOCKET,
                libc::SO_RCVBUF,
                (&receive_bytes as *const libc::c_int).cast(),
                std::mem::size_of_val(&receive_bytes) as libc::socklen_t,
            )
        };
        let mut address: libc::sockaddr_nl = unsafe { std::mem::zeroed() };
        address.nl_family = libc::AF_NETLINK as _;
        address.nl_groups = 1; // Kernel kobject multicast, not udev's group.
        let result = unsafe {
            libc::bind(
                fd.as_raw_fd(),
                (&address as *const libc::sockaddr_nl).cast(),
                std::mem::size_of_val(&address) as libc::socklen_t,
            )
        };
        if result < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(Self(fd))
    }

    /// Drain a bounded batch; leave excess queued for the next 100 ms loop.
    pub fn drain(&self) -> io::Result<bool> {
        let mut dirty = false;
        let mut buffer = [0u8; 16384];
        for _ in 0..64 {
            let mut sender: libc::sockaddr_nl = unsafe { std::mem::zeroed() };
            let mut len = std::mem::size_of_val(&sender) as libc::socklen_t;
            let n = unsafe {
                libc::recvfrom(
                    self.0.as_raw_fd(),
                    buffer.as_mut_ptr().cast(),
                    buffer.len(),
                    libc::MSG_DONTWAIT | libc::MSG_TRUNC,
                    (&mut sender as *mut libc::sockaddr_nl).cast(),
                    &mut len,
                )
            };
            if n < 0 {
                let error = io::Error::last_os_error();
                match error.kind() {
                    io::ErrorKind::WouldBlock => return Ok(dirty),
                    io::ErrorKind::Interrupted => continue,
                    _ => return Err(error), // Includes ENOBUFS / lost events.
                }
            }
            if len as usize == std::mem::size_of_val(&sender)
                && sender.nl_family == libc::AF_NETLINK as _
                && sender.nl_pid == 0
            {
                // A truncated kernel packet may have hidden a relevant event.
                dirty |= n as usize > buffer.len()
                    || relevant_event(&buffer[..n as usize], sender.nl_pid);
            }
        }
        Ok(dirty)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn packet(action: &str, subsystem: &str, hid: &str) -> Vec<u8> {
        format!("add@/devices/test\0ACTION={action}\0SUBSYSTEM={subsystem}\0DEVPATH=/devices/test/{hid}/input/input9/event4\0").into_bytes()
    }

    #[test]
    fn kernel_trackpad_events_include_late_input_and_actuator_and_removal() {
        for action in ["add", "remove", "change", "bind", "unbind"] {
            for subsystem in ["hidraw", "hid", "input"] {
                for hid in [
                    "0003:05AC:0324.0008",
                    "0003:05ac:0265.0001",
                    "0005:004C:0324.0002",
                ] {
                    assert!(relevant_event(&packet(action, subsystem, hid), 0));
                }
            }
        }
    }

    #[test]
    fn unrelated_malformed_and_userspace_events_do_not_wake() {
        let good = packet("add", "hidraw", "0003:05AC:0324.0001");
        assert!(!relevant_event(&good, 42));
        assert!(!relevant_event(&good[..good.len() - 1], 0));
        for (action, subsystem, hid) in [
            ("add", "input", "0003:05AC:024F.0001"),
            ("add", "input", "0003:1234:0324.0001"),
            ("add", "block", "0003:05AC:0324.0001"),
            ("offline", "hid", "0003:05AC:0324.0001"),
        ] {
            assert!(!relevant_event(&packet(action, subsystem, hid), 0));
        }
        assert!(!relevant_event(b"\xff\0", 0));
        assert!(!relevant_event(b"ACTION=add\0SUBSYSTEM=hidraw\0", 0));
    }

    #[test]
    fn burst_coalesces_within_one_second_without_delaying_full_verification() {
        let start = Instant::now();
        let mut scheduler = Schedule::new(start);
        for ms in 1..90 {
            scheduler.wake(start + Duration::from_millis(ms));
        }
        assert_eq!(
            scheduler.take_due(start + Duration::from_millis(100), true),
            None
        );
        assert_eq!(
            scheduler.take_due(start + Duration::from_millis(200), true),
            Some(Work::Hotplug)
        );
        scheduler.completed(start + Duration::from_secs(2), false);
        assert_eq!(
            scheduler.take_due(start + Duration::from_secs(29), true),
            None
        );
        assert_eq!(
            scheduler.take_due(start + Duration::from_secs(30), true),
            Some(Work::Verify)
        );
    }

    #[test]
    fn retries_back_off_to_one_second_and_stop_at_budget() {
        let start = Instant::now();
        let mut scheduler = Schedule::new(start);
        scheduler.wake(start);
        let mut passes = Vec::new();
        for tick in 1..=120 {
            let now = start + LOOP_INTERVAL * tick;
            if scheduler.take_due(now, true).is_some() {
                passes.push(now);
                if scheduler.completed(now, true) {
                    assert!(now.duration_since(start) >= RETRY_WINDOW);
                }
            }
        }
        assert!(passes.len() < 16, "bounded work: {passes:?}");
        for pair in passes.windows(2) {
            assert!(pair[1].duration_since(pair[0]) <= MAX_RETRY);
        }
        assert_eq!(
            scheduler.take_due(start + Duration::from_secs(20), true),
            None
        );
    }

    #[test]
    fn unavailable_or_lost_monitor_has_periodic_fallback_and_shutdown_wins() {
        let start = Instant::now();
        let mut scheduler = Schedule::new(start);
        assert_eq!(
            scheduler.take_due(start + Duration::from_secs(29), true),
            None
        );
        assert_eq!(scheduler.take_due(start + VERIFY_INTERVAL, false), None);
        assert_eq!(
            scheduler.take_due(start + VERIFY_INTERVAL, true),
            Some(Work::Verify)
        );
        scheduler.wake(start + VERIFY_INTERVAL); // Conservative wake on receive failure.
        assert_eq!(
            scheduler.take_due(start + VERIFY_INTERVAL + COALESCE, false),
            None
        );
        assert_eq!(
            scheduler.take_due(start + VERIFY_INTERVAL + COALESCE, true),
            Some(Work::Hotplug)
        );
    }
}
