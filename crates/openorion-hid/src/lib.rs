//! Device transport (DESIGN.md sec. 5). The `Transport` trait is the WP0
//! contract; `HidrawTransport` provides Linux hidraw access.
//!
//! Rules (protocol-spec.md sec. 1): every write is the 0x00 report-number
//! byte plus exactly 320 bytes; any other size poisons the device until a
//! power cycle. `reset()` resets only the Orion's own USB device node,
//! never a controller or another device.

#[cfg(not(target_os = "linux"))]
compile_error!("openorion device support requires Linux");

mod audio;
mod discover;
mod hidraw;

use std::io;
use std::path::PathBuf;
use std::time::Duration;

use openorion_proto::{REPORT_LEN, Report};

pub use audio::{usb_audio_busy, usb_audio_busy_in};
pub use hidraw::{DeviceNodes, HidrawTransport, find_device};

/// The real device on this platform.
pub type DeviceTransport = HidrawTransport;

/// USB ids of the Orion32 HD.
pub const VENDOR_ID: u16 = 0x23e5;
pub const PRODUCT_ID: u16 = 0xa210;
/// `HID_ID` line in `/sys/class/hidraw/*/device/uevent`.
pub const HID_ID: &str = "HID_ID=0003:000023E5:0000A210";

/// Bytes per hidraw write: the 0x00 report-number byte plus one report.
pub const FRAME_LEN: usize = REPORT_LEN + 1;

/// A byte channel to the device (hidraw, or the simulator).
pub trait Transport: Send {
    /// Write one report (321 bytes to hidraw: 0x00 + 320).
    fn write(&mut self, r: &Report) -> io::Result<()>;
    /// Wait up to `timeout` for one incoming report. `Ok(None)` = timeout.
    fn read(&mut self, timeout: Duration) -> io::Result<Option<[u8; REPORT_LEN]>>;
    /// USBDEVFS_RESET on this device, then reopen.
    fn reset(&mut self) -> io::Result<()>;
}

#[derive(Debug, thiserror::Error)]
pub enum FindError {
    #[error("no Orion32 HD found (no HID interface {VENDOR_ID:04x}:{PRODUCT_ID:04x})")]
    NotFound,
    #[error(
        "permission denied on {0} (install packaging/60-openorion.rules, join the audio group)"
    )]
    PermissionDenied(PathBuf),
    #[error(transparent)]
    Io(#[from] io::Error),
}

/// Prefix the report-number byte. The only way a frame is made.
fn frame(report: &[u8; REPORT_LEN]) -> [u8; FRAME_LEN] {
    let mut f = [0u8; FRAME_LEN];
    f[1..].copy_from_slice(report);
    f
}

/// Send one whole frame with one `write` call. A short write is an error
/// and is never completed: the device would see a non-320 report.
fn write_frame(
    frame: &[u8; FRAME_LEN],
    mut sys_write: impl FnMut(&[u8; FRAME_LEN]) -> io::Result<usize>,
) -> io::Result<()> {
    loop {
        match sys_write(frame) {
            // EINTR before anything was transferred: safe to try again.
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
            Ok(FRAME_LEN) => return Ok(()),
            Ok(n) => {
                return Err(io::Error::new(
                    io::ErrorKind::WriteZero,
                    format!("short hidraw write: {n} of {FRAME_LEN} bytes (not retried)"),
                ));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn report(fill: u8) -> [u8; REPORT_LEN] {
        let mut r = [fill; REPORT_LEN];
        r[0] = 0x74;
        r
    }

    #[test]
    fn frame_is_321_bytes_with_leading_zero() {
        let r = report(0xab);
        let f = frame(&r);
        assert_eq!(f.len(), 321);
        assert_eq!(f[0], 0);
        assert_eq!(&f[1..], &r[..]);
    }

    #[test]
    fn write_frame_ok_only_on_full_write() {
        let f = frame(&report(1));
        let mut calls = 0;
        write_frame(&f, |b| {
            calls += 1;
            assert_eq!(b.len(), FRAME_LEN);
            Ok(FRAME_LEN)
        })
        .unwrap();
        assert_eq!(calls, 1);
    }

    #[test]
    fn short_write_is_an_error_and_not_retried() {
        let f = frame(&report(1));
        let mut calls = 0;
        let e = write_frame(&f, |_| {
            calls += 1;
            Ok(100)
        })
        .unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::WriteZero);
        assert_eq!(calls, 1, "a short write must never be completed");
    }

    #[test]
    fn eintr_retries_whole_frame() {
        let f = frame(&report(1));
        let mut calls = 0;
        write_frame(&f, |b| {
            calls += 1;
            assert_eq!(b, &f);
            if calls == 1 {
                Err(io::Error::from(io::ErrorKind::Interrupted))
            } else {
                Ok(FRAME_LEN)
            }
        })
        .unwrap();
        assert_eq!(calls, 2);
    }
}
