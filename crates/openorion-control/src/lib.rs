//! Remote-control model for openorion-remote and the UI's learn mode
//! (MIDI and OSC): the parameters a controller can drive, the mappings
//! from controls to parameters, the relayed learn messages, and the
//! `remote.json` config.
//!
//! Pure: no I/O except loading and saving the config file.

pub mod config;
pub mod mapping;
pub mod msg;
pub mod param;

pub use config::{
    ConfigError, GridColumn, KNOWN_INIT, MeterGrid, MidiConfig, OscConfig, OscTarget, RemoteConfig,
    default_path, is_system_port, known_grid, parse_hex, port_matches, split_midi,
};
pub use mapping::{
    ANY_DEVICE, MapOpts, Mapping, Mappings, MidiCtl, MidiMsg, Mode, PickupState, Source, Takeover,
    denorm7, denorm14, midi_feedback, norm_pitchbend, norm7, norm14, rel7,
};
pub use msg::{PortInfo, RemoteMsg};
pub use param::{
    ChField, CompField, EqField, Kind, MasterField, OSC_PREFIX, Param, ParseParamError,
    ReverbField, afx_slug,
};
