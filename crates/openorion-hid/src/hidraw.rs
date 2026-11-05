//! Linux hidraw transport (WP2).

use std::fs::{File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use openorion_proto::{REPORT_LEN, Report};

use crate::{FindError, PRODUCT_ID, Transport, VENDOR_ID, discover, frame, write_frame};

/// `USBDEVFS_RESET` = `_IO('U', 20)`.
const USBDEVFS_RESET: u64 = 0x5514;
/// After a reset: poll sysfs this often, for at most `RESET_WAIT`.
const RESET_POLL: Duration = Duration::from_millis(100);
const RESET_WAIT: Duration = Duration::from_secs(5);

/// Where the Orion was found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceNodes {
    /// `/dev/hidrawN`.
    pub hidraw: PathBuf,
    /// `/dev/bus/usb/BBB/DDD`, for the reset ioctl.
    pub usb: PathBuf,
}

/// Scan `sysfs_root/class/hidraw/*/device/uevent` (normally `/sys`).
/// Node paths are under `/dev`. Checks that the hidraw node opens
/// read-write (`PermissionDenied` otherwise); the USB node is only needed
/// for `reset()`, which reports its own permission error.
pub fn find_device(sysfs_root: &Path) -> Result<DeviceNodes, FindError> {
    find_in(sysfs_root, Path::new("/dev"))
}

fn find_in(sysfs_root: &Path, dev_root: &Path) -> Result<DeviceNodes, FindError> {
    let nodes = discover::locate(sysfs_root, dev_root)?;
    open_hidraw(&nodes.hidraw)?;
    Ok(nodes)
}

fn open_hidraw(path: &Path) -> Result<File, FindError> {
    OpenOptions::new()
        .read(true)
        .write(true)
        .custom_flags(libc::O_CLOEXEC)
        .open(path)
        .map_err(|e| discover::open_error(path, e))
}

/// Wait for and read one report from `file`. `Ok(None)` on timeout.
fn read_report(file: &File, timeout: Duration) -> io::Result<Option<[u8; REPORT_LEN]>> {
    let deadline = Instant::now() + timeout;
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        // Round up so a sub-millisecond remainder still waits.
        let ms = left.as_micros().div_ceil(1000).min(i32::MAX as u128) as i32;
        let mut pfd = libc::pollfd {
            fd: file.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        // SAFETY: one valid pollfd for the duration of the call.
        let rc = unsafe { libc::poll(&mut pfd, 1, ms) };
        if rc < 0 {
            let e = io::Error::last_os_error();
            if e.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(e);
        }
        if rc == 0 {
            if Instant::now() >= deadline {
                return Ok(None);
            }
            continue;
        }
        if pfd.revents & libc::POLLIN == 0 {
            // POLLERR/POLLHUP/POLLNVAL without data: device gone.
            return Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                format!("hidraw poll revents {:#x}", pfd.revents),
            ));
        }
        let mut buf = [0u8; REPORT_LEN + 64];
        let n = match (&*file).read(&mut buf) {
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => continue,
            Err(e) => return Err(e),
            Ok(n) => n,
        };
        if n == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "hidraw read returned 0 bytes",
            ));
        }
        if n != REPORT_LEN {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("hidraw report of {n} bytes, expected {REPORT_LEN}"),
            ));
        }
        let mut out = [0u8; REPORT_LEN];
        out.copy_from_slice(&buf[..REPORT_LEN]);
        return Ok(Some(out));
    }
}

/// Read the USB device descriptor from a usbfs node and check it is the
/// Orion (bLength 18, bDescriptorType 1, idVendor, idProduct).
fn check_usb_descriptor(file: &mut File, path: &Path) -> io::Result<()> {
    let mut d = [0u8; 18];
    file.read_exact(&mut d)?;
    let vid = u16::from_le_bytes([d[8], d[9]]);
    let pid = u16::from_le_bytes([d[10], d[11]]);
    if d[0] != 18 || d[1] != 1 || vid != VENDOR_ID || pid != PRODUCT_ID {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "{} is {vid:04x}:{pid:04x}, not the Orion {VENDOR_ID:04x}:{PRODUCT_ID:04x}; refusing to reset",
                path.display()
            ),
        ));
    }
    Ok(())
}

fn find_to_io(e: FindError) -> io::Error {
    match e {
        FindError::Io(e) => e,
        FindError::NotFound => io::Error::new(io::ErrorKind::NotFound, e.to_string()),
        FindError::PermissionDenied(_) => {
            io::Error::new(io::ErrorKind::PermissionDenied, e.to_string())
        }
    }
}

/// The real device over hidraw.
pub struct HidrawTransport {
    nodes: DeviceNodes,
    /// `None` only between the reset ioctl and the reopen.
    file: Option<File>,
    sysfs_root: PathBuf,
    dev_root: PathBuf,
}

impl HidrawTransport {
    /// Find and open the device under `/sys`.
    pub fn open() -> Result<Self, FindError> {
        Self::open_in(Path::new("/sys"), Path::new("/dev"))
    }

    pub fn open_nodes(nodes: DeviceNodes) -> Result<Self, FindError> {
        Self::open_nodes_in(nodes, Path::new("/sys"), Path::new("/dev"))
    }

    pub fn nodes(&self) -> &DeviceNodes {
        &self.nodes
    }

    fn open_in(sysfs_root: &Path, dev_root: &Path) -> Result<Self, FindError> {
        let nodes = discover::locate(sysfs_root, dev_root)?;
        Self::open_nodes_in(nodes, sysfs_root, dev_root)
    }

    fn open_nodes_in(
        nodes: DeviceNodes,
        sysfs_root: &Path,
        dev_root: &Path,
    ) -> Result<Self, FindError> {
        let file = open_hidraw(&nodes.hidraw)?;
        tracing::debug!(hidraw = %nodes.hidraw.display(), usb = %nodes.usb.display(), "opened");
        Ok(HidrawTransport {
            nodes,
            file: Some(file),
            sysfs_root: sysfs_root.to_path_buf(),
            dev_root: dev_root.to_path_buf(),
        })
    }

    fn file(&self) -> io::Result<&File> {
        self.file.as_ref().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotConnected,
                "hidraw node not open (reset did not complete)",
            )
        })
    }

    /// Write one 320-byte report as a 321-byte frame. The array types make
    /// any other size unrepresentable.
    fn write_report(&mut self, report: &[u8; REPORT_LEN]) -> io::Result<()> {
        let mut file = self.file()?;
        write_frame(&frame(report), |f| file.write(f))
    }

    /// Wait for the hidraw node to come back after a reset and reopen it.
    fn reopen_after_reset(&mut self) -> io::Result<()> {
        let deadline = Instant::now() + RESET_WAIT;
        loop {
            std::thread::sleep(RESET_POLL);
            let last = match discover::locate(&self.sysfs_root, &self.dev_root)
                .and_then(|n| open_hidraw(&n.hidraw).map(|f| (n, f)))
            {
                Ok((nodes, file)) => {
                    tracing::info!(hidraw = %nodes.hidraw.display(), "reopened after reset");
                    self.nodes = nodes;
                    self.file = Some(file);
                    return Ok(());
                }
                Err(e) => e,
            };
            if Instant::now() >= deadline {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    format!("Orion did not come back within {RESET_WAIT:?} of the reset: {last}"),
                ));
            }
        }
    }
}

impl Transport for HidrawTransport {
    fn write(&mut self, r: &Report) -> io::Result<()> {
        self.write_report(r.as_bytes())
    }

    fn read(&mut self, timeout: Duration) -> io::Result<Option<[u8; REPORT_LEN]>> {
        read_report(self.file()?, timeout)
    }

    /// USBDEVFS_RESET on the Orion's own USB device node only. The node's
    /// device descriptor is read first and must be 23e5:a210, so a stale
    /// or wrong path can never reset a hub, controller or other device.
    fn reset(&mut self) -> io::Result<()> {
        let path = self.nodes.usb.clone();
        let mut usb = OpenOptions::new()
            .read(true)
            .write(true)
            .custom_flags(libc::O_CLOEXEC)
            .open(&path)
            .map_err(|e| find_to_io(discover::open_error(&path, e)))?;
        check_usb_descriptor(&mut usb, &path)?;

        // The hidraw node goes away with the reset; drop our fd first.
        self.file = None;
        tracing::info!(usb = %path.display(), "USBDEVFS_RESET");
        // SAFETY: valid fd; USBDEVFS_RESET takes no argument.
        let rc = unsafe { libc::ioctl(usb.as_raw_fd(), USBDEVFS_RESET as _, 0) };
        let res = if rc < 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(())
        };
        drop(usb);
        if let Err(e) = res {
            // Try to get the hidraw fd back so the transport stays usable.
            let _ = self.reopen_after_reset();
            return Err(e);
        }
        self.reopen_after_reset()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::FRAME_LEN;
    use crate::discover::tests::Fake;
    use std::os::fd::FromRawFd;

    fn report(fill: u8) -> [u8; REPORT_LEN] {
        let mut r = [fill; REPORT_LEN];
        r[0] = 0x74;
        r
    }

    #[test]
    fn open_and_write_to_fake_node() {
        let f = Fake::new();
        f.add_orion("hidraw0", 1, 2);
        let mut t = HidrawTransport::open_in(&f.sys(), &f.dev()).unwrap();
        assert_eq!(t.nodes().hidraw, f.dev().join("hidraw0"));
        let r = report(0x5a);
        t.write_report(&r).unwrap();
        t.write_report(&r).unwrap();
        let got = std::fs::read(f.dev().join("hidraw0")).unwrap();
        assert_eq!(got.len(), 2 * FRAME_LEN);
        assert_eq!(got[0], 0);
        assert_eq!(&got[1..FRAME_LEN], &r[..]);
        assert_eq!(got[FRAME_LEN], 0);
    }

    #[test]
    fn find_in_fake_tree() {
        let f = Fake::new();
        f.add_other("hidraw0", "1-1", 3);
        f.add_orion("hidraw1", 2, 9);
        let n = find_in(&f.sys(), &f.dev()).unwrap();
        assert_eq!(n.hidraw, f.dev().join("hidraw1"));
        assert_eq!(n.usb, f.dev().join("bus/usb/002/009"));
    }

    #[test]
    fn find_permission_denied() {
        use std::os::unix::fs::PermissionsExt;
        if unsafe { libc::geteuid() } == 0 {
            return; // root ignores modes
        }
        let f = Fake::new();
        f.add_orion("hidraw0", 1, 2);
        let node = f.dev().join("hidraw0");
        std::fs::set_permissions(&node, std::fs::Permissions::from_mode(0o444)).unwrap();
        match find_in(&f.sys(), &f.dev()) {
            Err(FindError::PermissionDenied(p)) => assert_eq!(p, node),
            other => panic!("{other:?}"),
        }
    }

    /// (read end, write end) of a pipe.
    fn pipe() -> (File, File) {
        let mut fds = [0; 2];
        // SAFETY: fds has room for two descriptors.
        assert_eq!(unsafe { libc::pipe(fds.as_mut_ptr()) }, 0);
        // SAFETY: fresh descriptors we own.
        unsafe { (File::from_raw_fd(fds[0]), File::from_raw_fd(fds[1])) }
    }

    #[test]
    fn read_timeout_gives_none() {
        let (r, _w) = pipe();
        let t0 = Instant::now();
        assert_eq!(read_report(&r, Duration::from_millis(30)).unwrap(), None);
        assert!(t0.elapsed() >= Duration::from_millis(30));
    }

    #[test]
    fn read_one_report() {
        let (r, mut w) = pipe();
        let rep = report(7);
        w.write_all(&rep).unwrap();
        let got = read_report(&r, Duration::from_secs(1)).unwrap().unwrap();
        assert_eq!(got, rep);
    }

    #[test]
    fn read_wrong_size_is_an_error() {
        let (r, mut w) = pipe();
        w.write_all(&[1u8; 64]).unwrap();
        let e = read_report(&r, Duration::from_secs(1)).unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn read_hangup_is_an_error() {
        let (r, w) = pipe();
        drop(w);
        assert!(read_report(&r, Duration::from_secs(1)).is_err());
    }

    fn descriptor(vid: u16, pid: u16) -> Vec<u8> {
        let mut d = vec![18, 1, 0x00, 0x02, 0, 0, 0, 64];
        d.extend_from_slice(&vid.to_le_bytes());
        d.extend_from_slice(&pid.to_le_bytes());
        d.extend_from_slice(&[0, 1, 1, 2, 3, 1]);
        d
    }

    #[test]
    fn reset_refuses_other_device() {
        let f = Fake::new();
        f.add_orion("hidraw0", 1, 2);
        let mut t = HidrawTransport::open_in(&f.sys(), &f.dev()).unwrap();
        let usb = &t.nodes().usb.clone();
        std::fs::create_dir_all(usb.parent().unwrap()).unwrap();
        // A hub's descriptor at the node path.
        std::fs::write(usb, descriptor(0x1d6b, 0x0002)).unwrap();
        let e = t.reset().unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::InvalidInput);
        // Refused before anything happened: still open and usable.
        t.write_report(&report(0)).unwrap();
    }

    #[test]
    fn descriptor_check_accepts_orion() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("002");
        std::fs::write(&p, descriptor(VENDOR_ID, PRODUCT_ID)).unwrap();
        check_usb_descriptor(&mut File::open(&p).unwrap(), &p).unwrap();
        std::fs::write(&p, [18u8, 1, 0]).unwrap();
        assert!(check_usb_descriptor(&mut File::open(&p).unwrap(), &p).is_err());
    }

    #[test]
    fn reset_missing_usb_node_errors() {
        let f = Fake::new();
        f.add_orion("hidraw0", 1, 2);
        let mut t = HidrawTransport::open_in(&f.sys(), &f.dev()).unwrap();
        assert!(t.reset().is_err());
        t.write_report(&report(0)).unwrap();
    }
}
