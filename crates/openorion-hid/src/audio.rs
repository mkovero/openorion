//! Is the Orion's USB audio streaming? A USB reset cuts a running stream
//! (seen on hardware 2026-10-27: ALSA "playback device disconnected"), so
//! resets are refused while it runs unless forced.
//!
//! snd-usb-audio lists each card's USB id in `/proc/asound/cardN/usbid`
//! and the stream state in `stream0` ("Status: Running" / "Status: Stop"
//! under "Playback:" and "Capture:").

use std::path::Path;

use crate::{PRODUCT_ID, VENDOR_ID};

/// Which directions of the Orion's USB audio run, e.g.
/// `Some("playback + capture running (card 2)")`; None when nothing runs,
/// no Orion card exists or `/proc` cannot be read.
pub fn usb_audio_busy() -> Option<String> {
    usb_audio_busy_in(Path::new("/proc/asound"))
}

pub fn usb_audio_busy_in(asound: &Path) -> Option<String> {
    let want = format!("{VENDOR_ID:04x}:{PRODUCT_ID:04x}");
    for e in std::fs::read_dir(asound).ok()?.flatten() {
        let name = e.file_name().to_string_lossy().into_owned();
        let Some(n) = name.strip_prefix("card") else {
            continue;
        };
        let dir = e.path();
        let id = std::fs::read_to_string(dir.join("usbid")).unwrap_or_default();
        if id.trim() != want {
            continue;
        }
        let stream = std::fs::read_to_string(dir.join("stream0")).unwrap_or_default();
        let running = running_directions(&stream);
        if !running.is_empty() {
            return Some(format!("{} running (card {n})", running.join(" + ")));
        }
    }
    None
}

/// Directions whose `Status:` line says Running.
fn running_directions(stream0: &str) -> Vec<&'static str> {
    let mut dir = None;
    let mut out = Vec::new();
    for line in stream0.lines() {
        match line.trim() {
            "Playback:" => dir = Some("playback"),
            "Capture:" => dir = Some("capture"),
            l if l.starts_with("Status:") => {
                if l.contains("Running")
                    && let Some(d) = dir
                {
                    out.push(d);
                }
            }
            _ => {}
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const STREAM: &str = "Antelope Audio Orion32HD at usb-0000:08:00.3-2, super speed : USB Audio

Playback:
  Status: Running
    Interface = 1
    Altset = 1
  Interface 1
    Format: S24_3LE

Capture:
  Status: Stop
  Interface 2
    Format: S24_3LE
";

    #[test]
    fn finds_running_directions() {
        assert_eq!(running_directions(STREAM), vec!["playback"]);
        assert!(running_directions(&STREAM.replace("Running", "Stop")).is_empty());
    }

    #[test]
    fn matches_the_orion_card_only() {
        let d = tempfile::tempdir().unwrap();
        let card = |n: u32, id: &str, stream: &str| {
            let c = d.path().join(format!("card{n}"));
            std::fs::create_dir(&c).unwrap();
            std::fs::write(c.join("usbid"), format!("{id}\n")).unwrap();
            std::fs::write(c.join("stream0"), stream).unwrap();
        };
        card(1, "2a39:3fc0", STREAM);
        assert_eq!(usb_audio_busy_in(d.path()), None);
        card(2, "23e5:a210", &STREAM.replace("Stop", "Running"));
        assert_eq!(
            usb_audio_busy_in(d.path()).as_deref(),
            Some("playback + capture running (card 2)")
        );
    }
}
