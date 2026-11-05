//! Device discovery through sysfs (DESIGN.md sec. 5).
//!
//! `<sysfs>/class/hidraw/<name>/device/uevent` carries the `HID_ID` line.
//! The USB device is the first ancestor of the resolved HID device dir that
//! has both `busnum` and `devnum`. Walking up from the HID device that is
//! the Orion's own USB device dir (the interface dir has neither; the root
//! hub and controller sit further up). Its `idVendor`/`idProduct` are
//! checked too, so a hub can never be picked.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use crate::{DeviceNodes, FindError, HID_ID, PRODUCT_ID, VENDOR_ID};

/// Find the Orion under `sysfs_root`, giving node paths under `dev_root`.
/// Does not check permissions.
pub(crate) fn locate(sysfs_root: &Path, dev_root: &Path) -> Result<DeviceNodes, FindError> {
    let class = sysfs_root.join("class/hidraw");
    let entries = match fs::read_dir(&class) {
        Ok(e) => e,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Err(FindError::NotFound),
        Err(e) => return Err(e.into()),
    };
    let mut names: Vec<String> = entries
        .filter_map(|e| e.ok())
        .filter_map(|e| e.file_name().into_string().ok())
        .collect();
    names.sort();

    for name in names {
        let dev_link = class.join(&name).join("device");
        let Ok(uevent) = fs::read_to_string(dev_link.join("uevent")) else {
            continue;
        };
        if !uevent_matches(&uevent) {
            continue;
        }
        let hid_dir = fs::canonicalize(&dev_link)?;
        let Some(usb_sysdir) = usb_device_dir(&hid_dir) else {
            tracing::warn!(hidraw = %name, "Orion HID_ID but no matching USB parent; skipped");
            continue;
        };
        let busnum = read_num(&usb_sysdir.join("busnum"))?;
        let devnum = read_num(&usb_sysdir.join("devnum"))?;
        return Ok(DeviceNodes {
            hidraw: dev_root.join(&name),
            usb: usb_node(dev_root, busnum, devnum),
        });
    }
    Err(FindError::NotFound)
}

fn uevent_matches(uevent: &str) -> bool {
    uevent
        .lines()
        .any(|l| l.trim().eq_ignore_ascii_case(HID_ID))
}

/// Walk up from `start` to the first dir with `busnum` and `devnum`. It
/// must be the Orion itself (`idVendor`/`idProduct`); anything else (a
/// hub, a controller) gives `None`.
fn usb_device_dir(start: &Path) -> Option<PathBuf> {
    let mut dir = start.parent();
    while let Some(d) = dir {
        if d.join("busnum").is_file() && d.join("devnum").is_file() {
            return is_orion_usb_dir(d).then(|| d.to_path_buf());
        }
        dir = d.parent();
    }
    None
}

/// True if the sysfs USB device dir has the Orion's vendor and product ids.
fn is_orion_usb_dir(dir: &Path) -> bool {
    let id = |f: &str| {
        fs::read_to_string(dir.join(f))
            .ok()
            .and_then(|s| u16::from_str_radix(s.trim(), 16).ok())
    };
    id("idVendor") == Some(VENDOR_ID) && id("idProduct") == Some(PRODUCT_ID)
}

fn read_num(path: &Path) -> io::Result<u32> {
    let s = fs::read_to_string(path)?;
    s.trim().parse().map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("{}: not a number: {:?}", path.display(), s.trim()),
        )
    })
}

pub(crate) fn usb_node(dev_root: &Path, busnum: u32, devnum: u32) -> PathBuf {
    dev_root.join(format!("bus/usb/{busnum:03}/{devnum:03}"))
}

/// Map an open error on `path` to `PermissionDenied` where it is one.
pub(crate) fn open_error(path: &Path, e: io::Error) -> FindError {
    if e.kind() == io::ErrorKind::PermissionDenied {
        FindError::PermissionDenied(path.to_path_buf())
    } else {
        FindError::Io(e)
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::os::unix::fs::symlink;

    /// A fake sysfs tree plus a fake /dev.
    pub struct Fake {
        pub dir: tempfile::TempDir,
    }

    impl Fake {
        pub fn new() -> Fake {
            let dir = tempfile::tempdir().unwrap();
            fs::create_dir_all(dir.path().join("sys/class/hidraw")).unwrap();
            fs::create_dir_all(dir.path().join("dev")).unwrap();
            Fake { dir }
        }

        pub fn sys(&self) -> PathBuf {
            self.dir.path().join("sys")
        }

        pub fn dev(&self) -> PathBuf {
            self.dir.path().join("dev")
        }

        /// Add a USB device with one HID interface and a hidraw node (a
        /// regular file in the fake /dev). Returns the USB device sysfs dir.
        #[allow(clippy::too_many_arguments)]
        pub fn add(
            &self,
            hidraw: &str,
            port: &str,
            bus: u32,
            dev: u32,
            vid: u16,
            pid: u16,
            hid_id: &str,
        ) -> PathBuf {
            let sys = self.sys();
            let hub = sys.join(format!("devices/pci0000:00/0000:00:14.0/usb{bus}"));
            fs::create_dir_all(&hub).unwrap();
            // The root hub has busnum/devnum too; it must never be picked.
            put(&hub.join("busnum"), &bus.to_string());
            put(&hub.join("devnum"), "1");
            put(&hub.join("idVendor"), "1d6b");
            put(&hub.join("idProduct"), "0002");

            let usb = hub.join(port);
            let hid = usb
                .join(format!("{port}:1.3"))
                .join(format!("0003:{vid:04X}:{pid:04X}.0001"));
            let node = hid.join("hidraw").join(hidraw);
            fs::create_dir_all(&node).unwrap();
            put(&usb.join("busnum"), &format!("{bus}\n"));
            put(&usb.join("devnum"), &format!("{dev}\n"));
            put(&usb.join("idVendor"), &format!("{vid:04x}\n"));
            put(&usb.join("idProduct"), &format!("{pid:04x}\n"));
            put(
                &hid.join("uevent"),
                &format!("DRIVER=hid-generic\n{hid_id}\nHID_NAME=x\n"),
            );
            symlink(&node, sys.join("class/hidraw").join(hidraw)).unwrap();
            symlink(&hid, node.join("device")).unwrap();
            fs::write(self.dev().join(hidraw), b"").unwrap();
            usb
        }

        pub fn add_orion(&self, hidraw: &str, bus: u32, dev: u32) -> PathBuf {
            self.add(
                hidraw,
                &format!("{bus}-2"),
                bus,
                dev,
                0x23e5,
                0xa210,
                HID_ID,
            )
        }

        pub fn add_other(&self, hidraw: &str, port: &str, dev: u32) -> PathBuf {
            self.add(
                hidraw,
                port,
                1,
                dev,
                0x046d,
                0xc52b,
                "HID_ID=0003:0000046D:0000C52B",
            )
        }
    }

    fn put(p: &Path, s: &str) {
        fs::write(p, s).unwrap();
    }

    #[test]
    fn found() {
        let f = Fake::new();
        f.add_orion("hidraw3", 10, 7);
        let got = locate(&f.sys(), &f.dev()).unwrap();
        assert_eq!(got.hidraw, f.dev().join("hidraw3"));
        assert_eq!(got.usb, f.dev().join("bus/usb/010/007"));
    }

    #[test]
    fn not_found_empty() {
        let f = Fake::new();
        assert!(matches!(
            locate(&f.sys(), &f.dev()),
            Err(FindError::NotFound)
        ));
    }

    #[test]
    fn not_found_no_class_dir() {
        let dir = tempfile::tempdir().unwrap();
        assert!(matches!(
            locate(dir.path(), dir.path()),
            Err(FindError::NotFound)
        ));
    }

    #[test]
    fn not_found_other_devices() {
        let f = Fake::new();
        f.add_other("hidraw0", "1-1", 2);
        assert!(matches!(
            locate(&f.sys(), &f.dev()),
            Err(FindError::NotFound)
        ));
    }

    #[test]
    fn multiple_one_matching() {
        let f = Fake::new();
        f.add_other("hidraw0", "1-1", 2);
        f.add_other("hidraw1", "1-4", 5);
        f.add_orion("hidraw2", 3, 12);
        f.add_other("hidraw3", "1-5", 6);
        let got = locate(&f.sys(), &f.dev()).unwrap();
        assert_eq!(got.hidraw, f.dev().join("hidraw2"));
        assert_eq!(got.usb, f.dev().join("bus/usb/003/012"));
    }

    #[test]
    fn hid_id_case_insensitive() {
        let f = Fake::new();
        f.add(
            "hidraw0",
            "2-1",
            2,
            4,
            0x23e5,
            0xa210,
            "HID_ID=0003:000023e5:0000a210",
        );
        assert!(locate(&f.sys(), &f.dev()).is_ok());
    }

    #[test]
    fn usb_parent_must_be_the_orion() {
        // HID_ID matches but the USB parent has other ids: refuse rather
        // than risk resetting the wrong device.
        let f = Fake::new();
        f.add("hidraw0", "2-1", 2, 4, 0x1234, 0x5678, HID_ID);
        assert!(matches!(
            locate(&f.sys(), &f.dev()),
            Err(FindError::NotFound)
        ));
    }

    #[test]
    fn busnum_devnum_three_digits() {
        let d = Path::new("/dev");
        assert_eq!(usb_node(d, 1, 2), Path::new("/dev/bus/usb/001/002"));
        assert_eq!(usb_node(d, 10, 127), Path::new("/dev/bus/usb/010/127"));
    }

    #[test]
    fn bad_devnum_is_an_error() {
        let f = Fake::new();
        let usb = f.add_orion("hidraw0", 1, 2);
        fs::write(usb.join("devnum"), "garbage").unwrap();
        assert!(matches!(locate(&f.sys(), &f.dev()), Err(FindError::Io(_))));
    }
}
