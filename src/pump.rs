//! Real hidraw pump: nonblocking read loop decoding 9-byte blocks.
//!
//! Reimplemented from the old userspace service behavior (read how it
//! pumped hidraw frames then reimplemented; no code copied):
//! - 512 B buffer, one `read(2)` per poll step, report ID in `buf[0]`.
//! - USB `0x02` = 12-byte prefix, BT `0x31` = 4-byte prefix, `0xf7` =
//!   two back-to-back sub-reports (first length byte at offset 1).
//! - One pump thread per attached session ([`spawn_pump`]), frame rate
//!   bounded by the device (no throttle; idle steps sleep briefly to
//!   avoid a busy spin). NOTE: the daemon never flips the stop flag —
//!   the thread lives until process exit by design. A dead pump
//!   surfaces through the session teardown paths (grab released,
//!   passthrough), never through the handle, which is only joined in
//!   tests.
//! - Malformed frames are logged + dropped per frame, never abort.
//!
//! Decode itself reuses [`crate::apple_parse::parse_frame`] /
//! [`crate::apple_parse::parse_double_report`].

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::Result;

use crate::apple_parse::{TouchFrame, TouchSource, DOUBLE_REPORT_ID};

/// Max hidraw report size (matches old service buffer reuse).
pub const MAX_REPORT_BYTES: usize = 512;

/// Idle sleep when a poll step yields no frame (avoids busy spin;
/// live frames are device-paced, so this never throttles input).
pub const IDLE_SLEEP: Duration = Duration::from_millis(2);

/// Injectable hidraw read backend (real fd vs scripted mock for CI).
pub trait HidrawReader: Send {
    fn read_report(&mut self, buf: &mut [u8]) -> std::io::Result<usize>;
}

/// Real file-backed reader.
pub struct FileReader {
    file: std::fs::File,
}

impl FileReader {
    pub fn open_nonblocking(devnode: &str) -> std::io::Result<Self> {
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(devnode)?;
        set_nonblocking(&file)?;
        Ok(Self { file })
    }
}

impl HidrawReader for FileReader {
    fn read_report(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        use std::os::unix::io::AsRawFd;
        let ret = unsafe { libc::read(self.file.as_raw_fd(), buf.as_mut_ptr().cast(), buf.len()) };
        if ret < 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(ret as usize)
    }
}

fn set_nonblocking(file: &std::fs::File) -> std::io::Result<()> {
    use std::os::unix::io::AsRawFd;
    let fd = file.as_raw_fd();
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags < 0 {
        return Err(std::io::Error::last_os_error());
    }
    let ret = unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) };
    if ret < 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

/// Scripted reader for tests: queued reports / errors, no hardware.
#[derive(Debug, Default)]
pub struct ScriptReader {
    queue: VecDeque<ScriptStep>,
}

/// One scripted read outcome.
#[derive(Debug)]
pub enum ScriptStep {
    Bytes(Vec<u8>),
    WouldBlock,
    IoError(i32),
}

impl ScriptReader {
    pub fn new(steps: Vec<ScriptStep>) -> Self {
        Self {
            queue: steps.into_iter().collect(),
        }
    }

    pub fn bytes(frames: Vec<Vec<u8>>) -> Self {
        Self::new(frames.into_iter().map(ScriptStep::Bytes).collect())
    }
}

impl HidrawReader for ScriptReader {
    fn read_report(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        match self.queue.pop_front() {
            None => Err(std::io::Error::from_raw_os_error(libc::EAGAIN)),
            Some(ScriptStep::WouldBlock) => Err(std::io::Error::from_raw_os_error(libc::EAGAIN)),
            Some(ScriptStep::IoError(e)) => Err(std::io::Error::from_raw_os_error(e)),
            Some(ScriptStep::Bytes(b)) => {
                let n = b.len().min(buf.len());
                buf[..n].copy_from_slice(&b[..n]);
                Ok(n)
            }
        }
    }
}

/// Real hidraw touch source: nonblocking reads + [`crate::apple_parse`] decode.
///
/// A queued second frame from a `0xf7` double report is returned first on
/// the next poll without touching the fd.
pub struct HidrawSource<R: HidrawReader> {
    devnode: String,
    reader: R,
    buf: Vec<u8>,
    pending: VecDeque<TouchFrame>,
}

impl<R: HidrawReader> HidrawSource<R> {
    pub fn new(devnode: &str, reader: R) -> Self {
        Self {
            devnode: devnode.to_string(),
            reader,
            buf: vec![0u8; MAX_REPORT_BYTES],
            pending: VecDeque::new(),
        }
    }

    pub fn devnode(&self) -> &str {
        &self.devnode
    }
}

impl HidrawSource<FileReader> {
    pub fn open(devnode: &str) -> Result<Self> {
        let reader = FileReader::open_nonblocking(devnode)
            .map_err(|e| anyhow::anyhow!("opening hidraw '{devnode}' nonblocking: {e}"))?;
        Ok(Self::new(devnode, reader))
    }
}

impl<R: HidrawReader> TouchSource for HidrawSource<R> {
    fn tier(&self) -> crate::apple_parse::CapabilityTier {
        crate::apple_parse::CapabilityTier::L2p
    }

    fn pressure_units(&self) -> crate::apple_parse::PressureUnits {
        crate::apple_parse::PressureUnits::Arbitrary
    }

    fn poll_frame(&mut self) -> Result<Option<TouchFrame>> {
        if let Some(f) = self.pending.pop_front() {
            return Ok(Some(f));
        }
        let n = match self.reader.read_report(&mut self.buf) {
            Ok(0) => anyhow::bail!("hidraw {} disconnect (0-byte read)", self.devnode),
            Ok(n) => n,
            Err(e) if e.raw_os_error() == Some(libc::EAGAIN) => return Ok(None),
            Err(e) => return Err(anyhow::anyhow!("hidraw {} read: {e}", self.devnode)),
        };
        let report = &self.buf[..n];
        let Some(&id) = report.first() else {
            log::warn!("pump {}: empty report, dropping frame", self.devnode);
            return Ok(None);
        };
        if id == DOUBLE_REPORT_ID {
            match crate::apple_parse::parse_double_report(report) {
                Ok((first, second)) => {
                    self.pending.push_back(second);
                    Ok(Some(first))
                }
                Err(e) => {
                    log::warn!(
                        "pump {}: malformed double report ({n} B), dropping frame: {e:#}",
                        self.devnode
                    );
                    Ok(None)
                }
            }
        } else {
            match crate::apple_parse::parse_frame(report) {
                Ok(f) => Ok(Some(f)),
                Err(e) => {
                    log::warn!(
                        "pump {}: malformed report id {id:#04x} ({n} B), dropping frame: {e:#}",
                        self.devnode
                    );
                    Ok(None)
                }
            }
        }
    }
}

/// Manager-owned source: idle until a hidraw node attaches.
/// Lets the daemon keep ONE proxy type (idle until a node attaches):
/// `Idle` polls to no-op, `Live` drains its hidraw node.
pub enum PumpSource {
    Idle,
    Live(HidrawSource<FileReader>),
}

impl TouchSource for PumpSource {
    fn tier(&self) -> crate::apple_parse::CapabilityTier {
        crate::apple_parse::CapabilityTier::L2p
    }
    fn pressure_units(&self) -> crate::apple_parse::PressureUnits {
        crate::apple_parse::PressureUnits::Arbitrary
    }
    fn poll_frame(&mut self) -> Result<Option<TouchFrame>> {
        match self {
            PumpSource::Idle => Ok(None),
            PumpSource::Live(s) => s.poll_frame(),
        }
    }
}

/// One pump step for tests/drives: single `handle_frame` call.
/// Returns true when a frame was handled (caller decides sleeps).
pub fn pump_once<G, U, H, S>(
    proxy: &Arc<Mutex<crate::proxy::ClickProxy<G, U, H, S>>>,
    cfg: &crate::config::Config,
) -> bool
where
    G: crate::proxy::GrabDevice,
    U: crate::proxy::UinputSink,
    H: crate::proxy::HapticSender,
    S: TouchSource,
{
    let mut px = proxy.lock().unwrap_or_else(|e| e.into_inner());
    match px.handle_frame(cfg) {
        Ok(_) => true,
        Err(e) => {
            log::warn!("pump: handle_frame error (dropped): {e:#}");
            true
        }
    }
}

/// Spawn a per-node pump thread draining `proxy` until `stop` flips.
/// Frame rate is device-bounded; idle polls sleep [`IDLE_SLEEP`].
pub fn spawn_pump<G, U, H, S>(
    name: String,
    proxy: Arc<Mutex<crate::proxy::ClickProxy<G, U, H, S>>>,
    cfg: crate::config::Config,
    stop: Arc<AtomicBool>,
) -> std::thread::JoinHandle<()>
where
    G: crate::proxy::GrabDevice + Send + 'static,
    U: crate::proxy::UinputSink + Send + 'static,
    H: crate::proxy::HapticSender + Send + 'static,
    S: TouchSource + Send + 'static,
{
    std::thread::spawn(move || {
        log::info!("pump {name}: thread started");
        while !stop.load(Ordering::SeqCst) {
            pump_once(&proxy, &cfg);
            std::thread::sleep(IDLE_SLEEP);
        }
        log::info!("pump {name}: thread stopped");
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn usb_report(clicks: u8, blocks: &[&[u8; 9]]) -> Vec<u8> {
        let mut r = vec![0x02, clicks, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0];
        for b in blocks {
            r.extend_from_slice(&**b);
        }
        r
    }

    fn contact(id: u8, pressure: u8) -> [u8; 9] {
        [0, 0, 0, 0x80, 10, 6, 8, pressure, id & 0x0f]
    }

    #[test]
    fn tier_contract_matches_parser() {
        let src = HidrawSource::new("/dev/hidrawX", ScriptReader::default());
        assert_eq!(src.tier(), crate::apple_parse::CapabilityTier::L2p);
        assert_eq!(
            src.pressure_units(),
            crate::apple_parse::PressureUnits::Arbitrary
        );
    }

    #[test]
    fn scripted_usb_bytes_decode() {
        let c = contact(3, 100);
        let r = usb_report(0, &[&c]);
        let mut src = HidrawSource::new("/dev/hidraw1", ScriptReader::bytes(vec![r]));
        let f = src.poll_frame().unwrap().unwrap();
        assert_eq!(f.fingers.len(), 1);
        assert_eq!(f.fingers[0].id, 3);
        assert_eq!(f.fingers[0].pressure, 100);
    }

    #[test]
    fn malformed_dropped_never_aborts() {
        let mut src = HidrawSource::new(
            "/dev/hidraw1",
            ScriptReader::new(vec![
                ScriptStep::Bytes(vec![0x02, 0, 0]), // short
                ScriptStep::Bytes({
                    let mut r = vec![0x02, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0];
                    r.extend_from_slice(&[1, 2, 3, 4, 5, 6, 7, 8]); // trailing 8
                    r
                }),
                ScriptStep::Bytes(usb_report(0, &[&contact(7, 50)])),
            ]),
        );
        assert!(src.poll_frame().unwrap().is_none());
        assert!(src.poll_frame().unwrap().is_none());
        let f = src.poll_frame().unwrap().unwrap();
        assert_eq!(f.fingers[0].id, 7);
    }

    #[test]
    fn double_report_queues_second() {
        let sub1 = {
            let mut r = vec![0x31, 0x01, 0, 0];
            r.extend_from_slice(&contact(1, 11));
            r
        };
        let sub2 = {
            let mut r = vec![0x31, 0x00, 0, 0];
            r.extend_from_slice(&contact(2, 22));
            r
        };
        let mut dbl = vec![0xF7, sub1.len() as u8];
        dbl.extend_from_slice(&sub1);
        dbl.extend_from_slice(&sub2);
        let mut src = HidrawSource::new("/dev/hidraw9", ScriptReader::bytes(vec![dbl]));
        let a = src.poll_frame().unwrap().unwrap();
        assert_eq!(a.fingers[0].pressure, 11);
        // Second frame served without another read.
        let b = src.poll_frame().unwrap().unwrap();
        assert_eq!(b.fingers[0].pressure, 22);
    }

    #[test]
    fn wouldblock_is_idle_not_error() {
        let mut src = HidrawSource::new(
            "/dev/hidraw1",
            ScriptReader::new(vec![ScriptStep::WouldBlock]),
        );
        assert!(src.poll_frame().unwrap().is_none());
    }

    #[test]
    fn zero_read_is_disconnect_error() {
        let mut src = HidrawSource::new("/dev/hidraw1", ScriptReader::bytes(vec![Vec::new()]));
        assert!(src.poll_frame().is_err());
    }

    #[test]
    fn pump_source_idle_is_noop() {
        let mut src = PumpSource::Idle;
        assert_eq!(src.tier(), crate::apple_parse::CapabilityTier::L2p);
        assert!(src.poll_frame().unwrap().is_none());
    }
}
