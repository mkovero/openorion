//! `remote.json`: OSC and MIDI settings plus the mappings. JSON (serde),
//! like the rest of the project; every field has a default, so a partial
//! file loads.

use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::mapping::Mapping;

/// Current `RemoteConfig::version`.
pub const CONFIG_VERSION: u32 = 1;
/// Default OSC listen address (loopback only).
pub const DEFAULT_OSC_LISTEN: &str = "127.0.0.1:9000";
/// Default meter rate for OSC targets.
pub const DEFAULT_METER_HZ: u32 = 20;

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("{path}: {err}")]
    Io {
        path: PathBuf,
        #[source]
        err: std::io::Error,
    },
    #[error("{path}: {err}")]
    Json {
        path: PathBuf,
        #[source]
        err: serde_json::Error,
    },
    #[error("{path}: config version {found}, this build reads up to {CONFIG_VERSION}")]
    Version { path: PathBuf, found: u32 },
}

/// Where values and meters go.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OscTarget {
    /// `host:port`.
    pub addr: String,
    /// Send `/orion/<path> f` on every change.
    #[serde(default = "yes")]
    pub values: bool,
    /// Send `/orion/meters/<bank> f…`.
    #[serde(default)]
    pub meters: bool,
    #[serde(default = "meter_hz")]
    pub meter_hz: u32,
}

fn yes() -> bool {
    true
}

fn meter_hz() -> u32 {
    DEFAULT_METER_HZ
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct OscConfig {
    /// UDP `host:port`; empty = OSC off.
    pub listen: String,
    pub targets: Vec<OscTarget>,
}

impl Default for OscConfig {
    fn default() -> Self {
        OscConfig {
            listen: DEFAULT_OSC_LISTEN.into(),
            targets: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct MidiConfig {
    /// Inputs to open: case-insensitive substrings of the ALSA client
    /// name, `*` = all. Empty = every input except the system ones
    /// (`System`, `Midi Through`) and openorion's own ports
    /// (`MidiConfig::input_active`).
    pub in_ports: Vec<String>,
    /// Feedback output per input device, when it differs from the input's
    /// own name (input name → output name pattern).
    pub out_ports: BTreeMap<String, String>,
    /// Outputs that get no feedback (name patterns); empty = feedback
    /// goes to every output paired with an input.
    pub out_off: Vec<String>,
    /// Create a virtual `openorion` in/out port.
    pub virtual_port: bool,
    /// Bytes sent to an output when it is opened, as hex (output name
    /// pattern → e.g. `"F0 47 7F 29 60 00 04 41 08 02 01 F7"`). Overrides
    /// `KNOWN_INIT`; an empty string sends nothing. May hold several
    /// messages (SysEx, then e.g. `B0 18 01` for an APC40 ring style);
    /// they are sent one by one (`split_midi`).
    pub init: BTreeMap<String, String>,
    /// LED meter grids (input name pattern → grid). Overrides
    /// `known_grid`; `"enabled": false` turns a known grid off.
    pub meter_grid: BTreeMap<String, MeterGrid>,
}

/// A button grid used as LED level meters: column `c` shows the mixer
/// channel whose level is bound to CC `fader_cc` on MIDI channel `c`
/// (a linked pair as one column, its louder side), lit bottom-up.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct MeterGrid {
    pub enabled: bool,
    /// MIDI channel (0-based) of the grid's note messages.
    pub ch: u8,
    /// `notes[column][row]`, row 0 = bottom.
    pub notes: Vec<Vec<u8>>,
    /// Row `r` lights at or above `rows_db[r]` dBFS; 0 = clip.
    pub rows_db: Vec<i8>,
    /// Note velocity (the controller's colour) per row.
    pub colors: Vec<u8>,
    pub fader_cc: u8,
    /// More columns, each following one fader control (e.g. the master
    /// fader): a mixer channel or master level bound to it.
    pub extra: Vec<GridColumn>,
    /// Updates per second.
    pub hz: u32,
    /// The top row stays lit this long after a clip.
    pub clip_hold_ms: u32,
}

impl Default for MeterGrid {
    fn default() -> Self {
        MeterGrid {
            enabled: true,
            ch: 0,
            notes: Vec::new(),
            rows_db: Vec::new(),
            colors: Vec::new(),
            fader_cc: 7,
            extra: Vec::new(),
            hz: 20,
            clip_hold_ms: 2000,
        }
    }
}

/// A grid column of its own fader: CC `cc` on MIDI channel `ch`
/// (0-based); `notes` bottom-up, on the grid's channel.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GridColumn {
    pub ch: u8,
    pub cc: u8,
    pub notes: Vec<u8>,
}

/// Built-in grids for known controllers (input name pattern).
pub fn known_grid(name: &str) -> Option<MeterGrid> {
    // Akai APC40 mkII (Ableton mode): clip-launch notes 0..39, bottom row
    // 0..7, top row 32..39; track faders CC7 on MIDI channels 1..8. The
    // scene-launch column (notes 82 top .. 86 bottom) follows the master
    // fader (CC14, channel 1).
    // Velocities pick from its RGB palette: 21 green, 13 yellow, 9 orange,
    // 5 red.
    port_matches("APC40 mkII", name).then(|| MeterGrid {
        notes: (0..8u8)
            .map(|c| (0..5u8).map(|r| c + 8 * r).collect())
            .collect(),
        rows_db: vec![-36, -18, -9, -3, 0],
        colors: vec![21, 21, 13, 9, 5],
        extra: vec![GridColumn {
            ch: 0,
            cc: 14,
            notes: vec![86, 85, 84, 83, 82],
        }],
        ..MeterGrid::default()
    })
}

/// Controllers that only show host feedback (LEDs, rings) after an init
/// message, sent unless `MidiConfig::init` says otherwise.
pub const KNOWN_INIT: &[(&str, &str)] = &[
    // Akai APC40 mkII: introduction message, Ableton Live mode (0x41).
    // In its power-up generic mode the LEDs ignore the host.
    ("APC40 mkII", "F0 47 7F 29 60 00 04 41 08 02 01 F7"),
];

/// A byte stream → MIDI messages: SysEx `F0 .. F7`, or a status byte
/// with its data bytes. Stray data bytes are dropped.
pub fn split_midi(bytes: &[u8]) -> Vec<Vec<u8>> {
    let mut out: Vec<Vec<u8>> = Vec::new();
    let mut in_sysex = false;
    for &b in bytes {
        if in_sysex {
            if let Some(m) = out.last_mut() {
                m.push(b);
            }
            in_sysex = b != 0xF7;
        } else if b >= 0x80 {
            out.push(vec![b]);
            in_sysex = b == 0xF0;
        } else if let Some(m) = out.last_mut() {
            m.push(b);
        }
    }
    out
}

/// `"F0 47 7f"` → bytes. Whitespace between bytes is optional.
pub fn parse_hex(s: &str) -> Result<Vec<u8>, String> {
    let digits: String = s.chars().filter(|c| !c.is_whitespace()).collect();
    if !digits.len().is_multiple_of(2) {
        return Err(format!("odd number of hex digits in \"{s}\""));
    }
    (0..digits.len())
        .step_by(2)
        .map(|i| {
            u8::from_str_radix(&digits[i..i + 2], 16)
                .map_err(|_| format!("bad hex byte \"{}\" in \"{s}\"", &digits[i..i + 2]))
        })
        .collect()
}

impl Default for MidiConfig {
    fn default() -> Self {
        MidiConfig {
            in_ports: Vec::new(),
            out_ports: BTreeMap::new(),
            out_off: Vec::new(),
            virtual_port: true,
            init: BTreeMap::new(),
            meter_grid: BTreeMap::new(),
        }
    }
}

/// ALSA clients never opened by default: the kernel's own clients and
/// openorion's (the virtual port and the remote's connections), which
/// would feed our own output back to us.
pub fn is_system_port(name: &str) -> bool {
    let n = name.to_lowercase();
    n == "system" || n.starts_with("midi through") || n.starts_with("openorion")
}

/// Case-insensitive substring match; `*` matches everything.
pub fn port_matches(pattern: &str, name: &str) -> bool {
    pattern == "*" || name.to_lowercase().contains(&pattern.to_lowercase())
}

impl MidiConfig {
    /// Whether input `name` is enabled by `in_ports`.
    pub fn input_enabled(&self, name: &str) -> bool {
        self.in_ports.iter().any(|p| port_matches(p, name))
    }

    /// Whether input `name` is opened: matched by `in_ports`, or, with
    /// `in_ports` empty, any input but the system and openorion ones.
    pub fn input_active(&self, name: &str) -> bool {
        if self.in_ports.is_empty() {
            !is_system_port(name)
        } else {
            self.input_enabled(name)
        }
    }

    /// Whether output `name` may get feedback.
    pub fn output_active(&self, name: &str) -> bool {
        !is_system_port(name) && !self.out_off.iter().any(|p| port_matches(p, name))
    }

    /// Enable or disable feedback to output `name`.
    pub fn set_output(&mut self, name: &str, on: bool) {
        if on {
            self.out_off.retain(|p| !port_matches(p, name));
        } else if self.output_active(name) {
            self.out_off.push(name.to_owned());
        }
    }

    /// Enable or disable input `name` (stored as the exact name; disabling
    /// drops every pattern that matches it). With `in_ports` empty (the
    /// "all inputs" default), `known` inputs that are active now are
    /// written out first, so disabling one keeps the others.
    pub fn set_input_known(&mut self, name: &str, on: bool, known: &[String]) {
        if self.in_ports.is_empty() {
            self.in_ports = known
                .iter()
                .filter(|k| !is_system_port(k))
                .cloned()
                .collect();
            if self.in_ports.is_empty() && !on {
                return;
            }
        }
        self.set_input(name, on);
    }

    /// Enable or disable input `name` (stored as the exact name; disabling
    /// drops every pattern that matches it).
    pub fn set_input(&mut self, name: &str, on: bool) {
        if on {
            if !self.input_enabled(name) {
                self.in_ports.push(name.to_owned());
            }
        } else {
            self.in_ports.retain(|p| !port_matches(p, name));
        }
    }

    /// The output (name pattern) that gets feedback for input `device`:
    /// the override, else the device's own name.
    pub fn output_for<'a>(&'a self, device: &'a str) -> &'a str {
        self.out_ports.get(device).map_or(device, String::as_str)
    }

    /// Init bytes for output `name`: the first matching `init` entry, else
    /// `KNOWN_INIT`. None = nothing to send (also for a bad hex string).
    pub fn init_for(&self, name: &str) -> Option<Vec<u8>> {
        let hex = match self.init.iter().find(|(p, _)| port_matches(p, name)) {
            Some((_, h)) => h.as_str(),
            None => KNOWN_INIT.iter().find(|(p, _)| port_matches(p, name))?.1,
        };
        parse_hex(hex).ok().filter(|b| !b.is_empty())
    }

    /// The meter grid of input `name`: the first matching `meter_grid`
    /// entry, else `known_grid`. None when off or malformed.
    pub fn grid_for(&self, name: &str) -> Option<MeterGrid> {
        let g = match self.meter_grid.iter().find(|(p, _)| port_matches(p, name)) {
            Some((_, g)) => g.clone(),
            None => known_grid(name)?,
        };
        let rows = g.rows_db.len();
        (g.enabled
            && rows > 0
            && g.colors.len() == rows
            && g.ch < 16
            && g.notes
                .iter()
                .chain(g.extra.iter().map(|c| &c.notes))
                .all(|c| c.len() == rows && c.iter().all(|&n| n < 128)))
        .then_some(g)
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct RemoteConfig {
    pub version: u32,
    pub osc: OscConfig,
    pub midi: MidiConfig,
    pub mappings: Vec<Mapping>,
}

impl Default for RemoteConfig {
    fn default() -> Self {
        RemoteConfig {
            version: CONFIG_VERSION,
            osc: OscConfig::default(),
            midi: MidiConfig::default(),
            mappings: Vec::new(),
        }
    }
}

/// `$XDG_CONFIG_HOME/openorion/remote.json`, else
/// `~/.config/openorion/remote.json`. None without either variable.
pub fn default_path() -> Option<PathBuf> {
    let base = match std::env::var_os("XDG_CONFIG_HOME") {
        Some(d) if !d.is_empty() && Path::new(&d).is_absolute() => PathBuf::from(d),
        _ => {
            let home = std::env::var_os("HOME").filter(|h| !h.is_empty())?;
            PathBuf::from(home).join(".config")
        }
    };
    Some(base.join("openorion").join("remote.json"))
}

impl RemoteConfig {
    /// Read `path`; a missing file gives the defaults.
    pub fn load(path: &Path) -> Result<RemoteConfig, ConfigError> {
        let text = match std::fs::read_to_string(path) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Ok(RemoteConfig::default());
            }
            Err(err) => {
                return Err(ConfigError::Io {
                    path: path.into(),
                    err,
                });
            }
        };
        let cfg: RemoteConfig = serde_json::from_str(&text).map_err(|err| ConfigError::Json {
            path: path.into(),
            err,
        })?;
        if cfg.version > CONFIG_VERSION {
            return Err(ConfigError::Version {
                path: path.into(),
                found: cfg.version,
            });
        }
        Ok(cfg)
    }

    /// Write `path` atomically (temp file in the same directory, then
    /// rename), creating the directory.
    pub fn save(&self, path: &Path) -> Result<(), ConfigError> {
        let io = |err| ConfigError::Io {
            path: path.into(),
            err,
        };
        let dir = path
            .parent()
            .filter(|d| !d.as_os_str().is_empty())
            .unwrap_or(Path::new("."));
        std::fs::create_dir_all(dir).map_err(io)?;
        let mut text = serde_json::to_string_pretty(self).map_err(|err| ConfigError::Json {
            path: path.into(),
            err,
        })?;
        text.push('\n');
        let name = path
            .file_name()
            .map_or("remote.json".into(), |n| n.to_string_lossy());
        let tmp = dir.join(format!(".{name}.{}.tmp", std::process::id()));
        let write = || -> std::io::Result<()> {
            let mut f = std::fs::File::create(&tmp)?;
            f.write_all(text.as_bytes())?;
            f.sync_all()?;
            std::fs::rename(&tmp, path)
        };
        write().map_err(|e| {
            let _ = std::fs::remove_file(&tmp);
            io(e)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mapping::{MapOpts, MidiCtl, Source};
    use crate::param::Param;
    use openorion_proto::VolumeId;

    #[test]
    fn defaults_and_partial() {
        let c: RemoteConfig = serde_json::from_str("{}").unwrap();
        assert_eq!(c, RemoteConfig::default());
        assert_eq!(c.osc.listen, DEFAULT_OSC_LISTEN);
        assert!(c.midi.virtual_port);
        let c: RemoteConfig = serde_json::from_str(
            r#"{"osc":{"targets":[{"addr":"127.0.0.1:9001","meters":true}]}}"#,
        )
        .unwrap();
        let t = &c.osc.targets[0];
        assert!(t.values && t.meters);
        assert_eq!(t.meter_hz, DEFAULT_METER_HZ);
        assert_eq!(c.osc.listen, DEFAULT_OSC_LISTEN);
    }

    #[test]
    fn ports() {
        let mut m = MidiConfig::default();
        assert!(!m.input_enabled("nanoKONTROL2"));
        m.set_input("nanoKONTROL2", true);
        m.set_input("nanoKONTROL2", true);
        assert_eq!(m.in_ports.len(), 1);
        assert!(m.input_enabled("nanokontrol2"));
        m.in_ports.push("*".into());
        m.set_input("nanoKONTROL2", false);
        assert!(!m.input_enabled("nanoKONTROL2"));
        m.out_ports.insert("pad in".into(), "pad out".into());
        assert_eq!(m.output_for("pad in"), "pad out");
        assert_eq!(m.output_for("other"), "other");
    }

    #[test]
    fn init_sysex() {
        assert_eq!(parse_hex("F0 47 7f").unwrap(), vec![0xF0, 0x47, 0x7F]);
        assert_eq!(parse_hex("F0477F").unwrap(), vec![0xF0, 0x47, 0x7F]);
        assert!(parse_hex("F0 4").is_err());
        assert!(parse_hex("zz").is_err());
        let mut m = MidiConfig::default();
        let apc = m.init_for("APC40 mkII").unwrap();
        assert_eq!(apc[..3], [0xF0, 0x47, 0x7F]);
        assert_eq!(*apc.last().unwrap(), 0xF7);
        assert_eq!(m.init_for("nanoKONTROL2"), None);
        // Config overrides the built-in entry; empty turns it off.
        m.init.insert("apc40".into(), "".into());
        assert_eq!(m.init_for("APC40 mkII"), None);
        m.init.insert("nano".into(), "F0 01 F7".into());
        assert_eq!(m.init_for("nanoKONTROL2"), Some(vec![0xF0, 1, 0xF7]));
        // Older files without `init` still load.
        let old: MidiConfig = serde_json::from_str(r#"{"in_ports":[]}"#).unwrap();
        assert!(old.init.is_empty());
    }

    #[test]
    fn midi_split() {
        let b = parse_hex("F0 47 7F 29 60 00 04 41 08 02 01 F7 B0 18 01 B0 38 03 33").unwrap();
        assert_eq!(
            split_midi(&b),
            vec![
                vec![
                    0xF0, 0x47, 0x7F, 0x29, 0x60, 0x00, 0x04, 0x41, 0x08, 0x02, 0x01, 0xF7
                ],
                vec![0xB0, 0x18, 0x01],
                vec![0xB0, 0x38, 0x03, 0x33],
            ]
        );
        assert!(split_midi(&[1, 2]).is_empty());
    }

    #[test]
    fn meter_grids() {
        let mut m = MidiConfig::default();
        let g = m.grid_for("APC40 mkII:APC40 mkII MIDI 1").unwrap();
        assert_eq!(g.notes[0], vec![0, 8, 16, 24, 32]);
        assert_eq!(g.notes[7], vec![7, 15, 23, 31, 39]);
        assert!(m.grid_for("nanoKONTROL2").is_none());
        m.meter_grid.insert(
            "apc40".into(),
            MeterGrid {
                enabled: false,
                ..MeterGrid::default()
            },
        );
        assert!(m.grid_for("APC40 mkII").is_none());
        // Malformed (rows and colours differ): off.
        m.meter_grid.insert(
            "apc40".into(),
            MeterGrid {
                notes: vec![vec![0, 8]],
                rows_db: vec![-20, 0],
                colors: vec![21],
                ..MeterGrid::default()
            },
        );
        assert!(m.grid_for("APC40 mkII").is_none());
        let json = r#"{"meter_grid": {"apc40": {"notes": [[0, 8]], "rows_db": [-20, 0], "colors": [21, 5]}}}"#;
        let m: MidiConfig = serde_json::from_str(json).unwrap();
        let g = m.grid_for("APC40 mkII").unwrap();
        assert_eq!((g.fader_cc, g.hz), (7, 20));
    }

    #[test]
    fn default_all_inputs() {
        let mut m = MidiConfig::default();
        assert!(m.input_active("nanoKONTROL2"));
        assert!(!m.input_active("Midi Through"));
        assert!(!m.input_active("openorion-remote"));
        assert!(!m.input_active("System"));
        let known = vec!["nano".to_string(), "pad".to_string(), "Midi Through".into()];
        m.set_input_known("pad", false, &known);
        assert_eq!(m.in_ports, vec!["nano".to_string()]);
        assert!(m.input_active("nano") && !m.input_active("pad"));
        assert!(m.output_active("nano"));
        m.set_output("nano", false);
        m.set_output("nano", false);
        assert_eq!(m.out_off.len(), 1);
        assert!(!m.output_active("nano"));
        m.set_output("nano", true);
        assert!(m.output_active("nano"));
        assert!(!m.output_active("Midi Through Port-0"));
    }

    #[test]
    fn load_save() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("sub/remote.json");
        assert_eq!(RemoteConfig::load(&p).unwrap(), RemoteConfig::default());
        let mut c = RemoteConfig::default();
        c.mappings.push(Mapping {
            source: Source::Midi {
                device: "*".into(),
                ch: None,
                ctl: MidiCtl::Cc(7),
            },
            param: Param::Volume(VolumeId::Main),
            opts: MapOpts::default(),
        });
        c.save(&p).unwrap();
        assert_eq!(RemoteConfig::load(&p).unwrap(), c);
        // No temp files left behind.
        let n = std::fs::read_dir(p.parent().unwrap()).unwrap().count();
        assert_eq!(n, 1);
        std::fs::write(&p, r#"{"version":99}"#).unwrap();
        assert!(matches!(
            RemoteConfig::load(&p),
            Err(ConfigError::Version { found: 99, .. })
        ));
        std::fs::write(&p, "{").unwrap();
        assert!(matches!(
            RemoteConfig::load(&p),
            Err(ConfigError::Json { .. })
        ));
    }
}
