//! Read-only hardware probe: print the Orion's firmware version.
//!
//!     cargo run -p openorion-hid --example probe
//!
//! Sends exactly one request, get firmware (0x74, ext2 0, ext3 0), and
//! nothing else. Flow (protocol-spec.md sec. 1): open; wait up to 200 ms
//! for any report (cyclic 0x73); if none, one USBDEVFS_RESET of the Orion
//! and wait up to 2 s (Linux only); then the get; wait up to 500 ms for
//! its 0x75 reply.
//!
//! Talks to real hardware: run it only on purpose (DESIGN.md sec. 11).

use std::process::ExitCode;
use std::time::{Duration, Instant};

use openorion_hid::{DeviceTransport, Transport};
use openorion_proto::{Get, REPORT_LEN};

const ID_REPLY: u32 = 0x75;

fn word(r: &[u8; REPORT_LEN], i: usize) -> u32 {
    u32::from_le_bytes(r[i * 4..i * 4 + 4].try_into().unwrap())
}

/// Wait up to `total` for the first report that `want` accepts.
fn wait_for(
    t: &mut DeviceTransport,
    total: Duration,
    want: impl Fn(&[u8; REPORT_LEN]) -> bool,
) -> std::io::Result<Option<[u8; REPORT_LEN]>> {
    let deadline = Instant::now() + total;
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Ok(None);
        }
        match t.read(left)? {
            Some(r) if want(&r) => return Ok(Some(r)),
            Some(_) => continue,
            None => return Ok(None),
        }
    }
}

fn run() -> Result<(), Box<dyn std::error::Error>> {
    let mut t = DeviceTransport::open()?;
    let n = t.nodes();
    println!("hidraw {}  usb {}", n.hidraw.display(), n.usb.display());

    match wait_for(&mut t, Duration::from_millis(200), |_| true)? {
        Some(r) => println!("cyclic flowing (first id {:#x})", word(&r, 0)),
        None => {
            println!("no reports in 200 ms; one device reset");
            t.reset()?;
            match wait_for(&mut t, Duration::from_secs(2), |_| true)? {
                Some(r) => println!("after reset: id {:#x}", word(&r, 0)),
                None => println!("warning: still no reports 2 s after the reset"),
            }
        }
    }

    let get = Get::Firmware.report();
    assert_eq!(
        (get.id(), get.ext2(), get.ext3()),
        (0x74, 0, 0),
        "probe sends only the firmware get"
    );
    t.write(&get)?;
    let reply = wait_for(&mut t, Duration::from_millis(500), |r| {
        let id = word(r, 0);
        if id != 0x73 && id != 0x83 {
            println!("got id {id:#x}: {:02x?}", &r[..24]);
        }
        id == ID_REPLY && word(r, 2) == 0 && word(r, 3) == 0
    })?;
    match reply {
        Some(r) => {
            println!("firmware {}.{:02}", r[16], r[17]);
            Ok(())
        }
        None => Err("no firmware reply within 500 ms (hung or poisoned? power-cycle)".into()),
    }
}

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("probe: {e}");
            ExitCode::FAILURE
        }
    }
}
