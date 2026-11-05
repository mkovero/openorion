//! openorion-jack (DESIGN.md sec. 9): JACK port names that follow the Orion's
//! routing, a sample-rate guard, MIDI control and a setup doctor.
//!
//! Everything but `jackrun` is pure or read-only and builds without JACK.

pub mod doctor;
pub mod midimap;
pub mod naming;
pub mod rate;
pub mod track;

#[cfg(feature = "jack")]
pub mod jackrun;

/// Default case-insensitive match for the Orion's ALSA card and JACK ports.
pub const DEFAULT_DEVICE_MATCH: &str = "Orion32";

/// Print a user-facing warning: one clear line on stderr.
pub fn warn(msg: &str) {
    eprintln!("openorion-jack: WARNING: {msg}");
}

/// Print a status line on stderr.
pub fn info(msg: &str) {
    eprintln!("openorion-jack: {msg}");
}
