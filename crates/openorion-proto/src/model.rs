//! Device model: plain data with serde. This is the contract every other
//! crate builds on (WP0). Indices are 0-based; display names live in
//! `names.rs`. Wire layouts are in `docs/protocol-spec.md`.
//!
//! Newtypes validate on construction and on deserialize, so a value that
//! exists is always one the device accepts.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::error::ProtoError;

// ---------------------------------------------------------------------------
// Helpers

/// Declares a fieldless `u8` enum with checked conversion from the wire
/// byte. Serde uses the variant names.
macro_rules! wire_enum {
    ($(#[$m:meta])* $name:ident, $what:literal { $($(#[$vm:meta])* $var:ident = $val:literal),+ $(,)? }) => {
        $(#[$m])*
        #[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
        #[repr(u8)]
        pub enum $name { $($(#[$vm])* $var = $val),+ }

        impl $name {
            pub const ALL: &'static [$name] = &[$($name::$var),+];

            pub fn as_u8(self) -> u8 {
                self as u8
            }
        }

        impl TryFrom<u8> for $name {
            type Error = ProtoError;
            fn try_from(v: u8) -> Result<Self, ProtoError> {
                match v {
                    $($val => Ok($name::$var),)+
                    _ => Err(ProtoError::UnknownValue { what: $what, value: v as i64 }),
                }
            }
        }

        impl From<$name> for u8 {
            fn from(v: $name) -> u8 {
                v as u8
            }
        }
    };
}

/// Declares a `u8` newtype restricted to `$min..=$max`.
macro_rules! ranged_u8 {
    ($(#[$m:meta])* $name:ident, $what:literal, $min:literal, $max:literal) => {
        $(#[$m])*
        #[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
        #[serde(try_from = "u8", into = "u8")]
        pub struct $name(u8);

        impl $name {
            pub const MIN: u8 = $min;
            pub const MAX: u8 = $max;

            pub fn new(v: u8) -> Result<Self, ProtoError> {
                if (Self::MIN..=Self::MAX).contains(&v) {
                    Ok($name(v))
                } else {
                    Err(ProtoError::OutOfRange {
                        what: $what,
                        value: v as i64,
                        min: Self::MIN as i64,
                        max: Self::MAX as i64,
                    })
                }
            }

            pub fn get(self) -> u8 {
                self.0
            }
        }

        impl TryFrom<u8> for $name {
            type Error = ProtoError;
            fn try_from(v: u8) -> Result<Self, ProtoError> {
                $name::new(v)
            }
        }

        impl From<$name> for u8 {
            fn from(v: $name) -> u8 {
                v.0
            }
        }
    };
}

// ---------------------------------------------------------------------------
// Scalars

ranged_u8!(
    /// dB of attenuation, 0 = 0 dB. Mixer level ≥95 and send ≥95 mean -inf;
    /// volume 96 means -inf.
    Atten, "attenuation", 0, 96
);

impl Atten {
    pub const ZERO_DB: Atten = Atten(0);
    pub const NEG_INF: Atten = Atten(96);

    /// True when the device treats this value as silence (≥95).
    pub fn is_neg_inf(self) -> bool {
        self.0 >= 95
    }
}

ranged_u8!(
    /// Pan, the full 6-bit wire range, centre 32. Shown as value − 32.
    /// The panel's knob covers `UI_MIN..=UI_MAX`, but it writes master pan
    /// 0 (seen on the wire), so the type accepts every wire value.
    Pan, "pan", 0, 63
);

impl Pan {
    pub const CENTER: Pan = Pan(32);
    /// Range of the panel's pan knob (-30..=+30 displayed).
    pub const UI_MIN: u8 = 2;
    pub const UI_MAX: u8 = 62;

    /// -30..=+30, as the panel shows it.
    pub fn display(self) -> i8 {
        self.0 as i8 - 32
    }
}

ranged_u8!(
    /// Mixer 0..=3 (MIXER 1..4 in the panel).
    MixerId, "mixer", 0, 3
);

ranged_u8!(
    /// Device preset slot 1..=5.
    PresetSlot, "preset slot", 1, 5
);

ranged_u8!(
    /// Front-panel brightness 0..=100.
    Brightness, "brightness", 0, 100
);

/// Number of channel strips per mixer (master excluded).
pub const MIXER_CHANNELS: usize = 32;
/// AFX chains (each fed by one AFX IN channel).
pub const AFX_CHAINS: usize = 16;
/// AFX slots per chain.
pub const AFX_SLOTS: usize = 8;
/// Trim channels per trim record.
pub const TRIM_CHANNELS: usize = 32;

// ---------------------------------------------------------------------------
// Enums (numbering: protocol-spec.md sec. 5 and 8)

wire_enum!(
    /// `set_sample_rate` index, also status +2 low nibble.
    SampleRate, "sample rate" {
        R32k = 0, R44k1 = 1, R48k = 2, R88k2 = 3, R96k = 4, R176k4 = 5, R192k = 6,
    }
);

impl SampleRate {
    pub fn hz(self) -> u32 {
        match self {
            SampleRate::R32k => 32_000,
            SampleRate::R44k1 => 44_100,
            SampleRate::R48k => 48_000,
            SampleRate::R88k2 => 88_200,
            SampleRate::R96k => 96_000,
            SampleRate::R176k4 => 176_400,
            SampleRate::R192k => 192_000,
        }
    }

    pub fn from_hz(hz: u32) -> Option<SampleRate> {
        SampleRate::ALL.iter().copied().find(|r| r.hz() == hz)
    }
}

wire_enum!(
    /// `set_sync_source` index, also status +3.
    ClockSource, "clock source" {
        /// Oven / Atomic (internal).
        Internal = 0, WordClock = 1, Spdif = 2, Adat = 3, Adat2x = 4, Adat4x = 5,
        Madi = 6, Madi2x = 7, Madi4x = 8, Hdx = 9, Usb = 10,
    }
);

impl ClockSource {
    pub fn is_external(self) -> bool {
        self != ClockSource::Internal
    }

    pub fn is_adat(self) -> bool {
        matches!(
            self,
            ClockSource::Adat | ClockSource::Adat2x | ClockSource::Adat4x
        )
    }
}

wire_enum!(
    /// Volume / mute target (`set_volume`, `set_mute`).
    VolumeId, "volume id" { Main = 0, Line = 1 }
);

wire_enum!(
    /// Trim record id. Monitor (0) set is unconfirmed on the wire.
    TrimId, "trim id" { Monitor = 0, LineOut = 1, LineIn = 2 }
);

wire_enum!(
    TrimControl, "trim control" { All = 0, Individual = 1 }
);

wire_enum!(
    /// `set_smux_mode` id.
    SmuxId, "smux id" { MadiIn = 0, MadiOut = 1 }
);

wire_enum!(
    /// `set_meter_source` bank.
    MeterBank, "meter bank" { A = 0, B = 1, Mixer = 2 }
);

wire_enum!(
    /// Routing source group (the `group` byte of a routing entry), except
    /// 14 (MUTE), which is `Source::Mute`.
    SourceGroup, "source group" {
        LineIn = 0, UsbPlay1 = 1, UsbPlay33 = 2, MadiIn1 = 3, MadiIn33 = 4,
        Hdx1Play = 5, Hdx2Play = 6, AdatIn = 7, SpdifIn = 8, AfxOut = 9,
        Mix1 = 10, Mix2 = 11, Mix3 = 12, Mix4 = 13, Oscillator = 15,
    }
);

/// Wire value of the MUTE source group.
pub const SOURCE_GROUP_MUTE: u8 = 14;

impl SourceGroup {
    /// Channels this group offers (ADAT depends on rate; the max is given).
    pub fn size(self) -> u8 {
        match self {
            SourceGroup::AdatIn | SourceGroup::AfxOut => 16,
            SourceGroup::SpdifIn
            | SourceGroup::Mix1
            | SourceGroup::Mix2
            | SourceGroup::Mix3
            | SourceGroup::Mix4
            | SourceGroup::Oscillator => 2,
            _ => 32,
        }
    }
}

wire_enum!(
    /// Routing bank (outputs), the ext3 of get routing and byte 0 of
    /// set routing.
    OutputBank, "output bank" {
        LineOut = 0, Monitor = 1, UsbRec1 = 2, UsbRec33 = 3, MadiOut1 = 4,
        MadiOut33 = 5, Hdx1Rec = 6, Hdx2Rec = 7, AdatOut = 8, SpdifOut = 9,
        AfxIn = 10, MixCh1 = 11, MixCh2 = 12, MixCh3 = 13, MixCh4 = 14,
    }
);

impl OutputBank {
    /// Meaningful entries in this bank.
    pub fn size(self) -> usize {
        match self {
            OutputBank::Monitor | OutputBank::SpdifOut => 2,
            OutputBank::AdatOut | OutputBank::AfxIn => 16,
            _ => 32,
        }
    }

    /// Banks that drive physical outputs. Routing to them is Disruptive.
    pub fn is_physical(self) -> bool {
        matches!(
            self,
            OutputBank::LineOut
                | OutputBank::Monitor
                | OutputBank::MadiOut1
                | OutputBank::MadiOut33
                | OutputBank::AdatOut
                | OutputBank::SpdifOut
        )
    }
}

wire_enum!(
    /// Oscillator frequency (2-bit field).
    OscFreq, "oscillator frequency" { Hz1k = 0, Hz440 = 1 }
);

wire_enum!(
    /// Oscillator level (2-bit field), dBFS.
    OscLevel, "oscillator level" { Db0 = 0, DbM6 = 1, DbM12 = 2, DbM18 = 3 }
);

wire_enum!(
    /// AFX model id (`AfxType`, protocol-spec.md sec. 10). Gaps (10, 40, 49)
    /// are not in the vendor enum.
    AfxType, "afx type" {
        Equalizer = 1, Compressor = 2, GuitarAmp = 3, GuitarCab = 4,
        PultecMeq = 5, PultecEqp = 6, Neve1073 = 7, Api550 = 8, Uad1176 = 9,
        PultecHlf = 11, Helios69 = 12, NeumannPev = 13, LangPeq2 = 14,
        NeumannW492 = 15, SslPink = 16, SslOrange = 17, SslBlack = 18,
        SslBrown = 19, NeumannW495 = 20, Studer089 = 21, Studer169 = 22,
        Studer900 = 23, Neve1023 = 24, Neve1084 = 25, Harrison32c = 26,
        Deesser = 27, Api550b = 28, Dbx903 = 29, GyratecX = 30, Liverpool = 31,
        Tube176 = 32, Dbx160 = 33, Staylevin = 34, Altec436c = 35,
        Summit100a = 36, Uad1178 = 37, Ba6a = 38, Powergate = 39,
        Tubechild670 = 41, Impresser = 42, GyratecIx = 43, TelefunkenV76 = 44,
        Bae1073mp = 45, Tape = 46, ChandlerRd47 = 47, Ba31 = 48, Bae10dcf = 50,
        Expander = 51, AntaresAutotune = 52, AntelopeTremolo = 53,
        SpaceFlanger = 54, Mvchorus = 55, Autowah = 56, Distortion = 57,
        PitchShifter = 58, Opto2a = 59,
    }
);

impl AfxType {
    /// True for the models the Orion panel offers (protocol-spec.md
    /// sec. 10). Only these may be sent.
    pub fn on_orion(self) -> bool {
        use AfxType::*;
        !matches!(
            self,
            GuitarAmp
                | GuitarCab
                | Deesser
                | GyratecIx
                | TelefunkenV76
                | Bae1073mp
                | Tape
                | ChandlerRd47
                | Ba31
                | Bae10dcf
                | Expander
                | AntaresAutotune
                | AntelopeTremolo
                | SpaceFlanger
                | Mvchorus
                | Autowah
                | Distortion
                | PitchShifter
                | Opto2a
        )
    }
}

wire_enum!(
    /// ClearQ band filter type.
    EqFilter, "eq filter type" { LowShelf = 0, HighShelf = 1, Peak = 2, Lpf = 3, Hpf = 4 }
);

// ---------------------------------------------------------------------------
// Device info

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Version {
    pub major: u8,
    pub minor: u8,
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Passport {
    pub name: String,
    pub serial: String,
    pub hw: String,
    /// Unix time.
    pub birth: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct DeviceInfo {
    pub firmware: Version,
    pub passport: Passport,
    pub registered: bool,
}

// ---------------------------------------------------------------------------
// Status (0x73 minus meters)

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct ClockFlags {
    pub oven: bool,
    pub locked: bool,
    pub atomic: bool,
    pub usb: bool,
    pub madi: bool,
    pub adat: bool,
    pub spdif: bool,
    pub dlink: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Osc {
    pub freq_l: OscFreq,
    pub freq_r: OscFreq,
    pub level: OscLevel,
    pub mute_l: bool,
    pub mute_r: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct VolumeState {
    pub atten: Atten,
    pub mute: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Status {
    /// 0 = none / modified, 1..=5 = last recalled or saved slot.
    pub current_preset: u8,
    pub power_on: bool,
    /// Status bit of unknown meaning (not the main mute).
    pub muted_bit: bool,
    pub usb_mode: u8,
    pub device_updated: bool,
    pub clock: ClockFlags,
    /// None when the rate nibble is not a known index.
    pub sample_rate: Option<SampleRate>,
    /// Status +2 high nibble (pull modes), raw.
    pub pull_mode: u8,
    pub clock_source: Option<ClockSource>,
    pub oscillator: Osc,
    pub external_clock_hz: u32,
    pub smux_in: bool,
    pub smux_out: bool,
    pub spdif_src: bool,
    pub brightness: u8,
    pub adat_channels: u8,
    pub madi_channels: u8,
    pub main: VolumeState,
    pub line: VolumeState,
    /// Monitor trim `whole`; dBu = 24 − value.
    pub monitor_trim: u8,
    /// Meter sources for bank A, bank B, mixer, and an unknown 4th byte.
    pub meter_sources: [u8; 4],
}

impl Status {
    /// Whether the selected clock source is locked. The `locked` bit stays 0
    /// on USB clock (seen on hardware while streaming), and the vendor panel's
    /// `is_clock_present` counts USB as always present, so USB counts as
    /// locked while the `usb` flag is set.
    pub fn clock_locked(&self) -> bool {
        self.clock.locked || (self.clock_source == Some(ClockSource::Usb) && self.clock.usb)
    }
}

// ---------------------------------------------------------------------------
// Meters

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AfxMeters {
    /// [chain][slot]; raw = dB below FS, 0 = clip, 96 = idle floor
    /// on the wire (the panel's meter shows ≥60 as empty).
    pub peak: [[u8; AFX_SLOTS]; AFX_CHAINS],
    /// [chain][slot]; per-model gain-reduction scale (protocol-spec.md sec. 10).
    pub gain_reduction: [[u8; AFX_SLOTS]; AFX_CHAINS],
}

impl Default for AfxMeters {
    fn default() -> Self {
        AfxMeters {
            peak: [[METER_FLOOR; AFX_SLOTS]; AFX_CHAINS],
            gain_reduction: [[0; AFX_SLOTS]; AFX_CHAINS],
        }
    }
}

/// Meters from 0x73. Raw = dB below full scale, 0 = clip, 96 = floor.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Meters {
    pub bank_a: [u8; 32],
    pub bank_b: [u8; 32],
    pub mixer: [u8; 32],
    pub reverb_in: [u8; 2],
    pub reverb_out: [u8; 2],
}

impl Default for Meters {
    fn default() -> Self {
        Meters {
            bank_a: [96; 32],
            bank_b: [96; 32],
            mixer: [96; 32],
            reverb_in: [96; 2],
            reverb_out: [96; 2],
        }
    }
}

/// Meter value meaning silence.
pub const METER_FLOOR: u8 = 96;

// ---------------------------------------------------------------------------
// Routing

/// One routing entry. `chan` is 0-based, as on the wire.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Source {
    Mute,
    Input { group: SourceGroup, chan: u8 },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RoutingBank {
    pub bank: OutputBank,
    /// Exactly `bank.size()` entries.
    pub sources: Vec<Source>,
}

impl RoutingBank {
    pub fn all_muted(bank: OutputBank) -> Self {
        RoutingBank {
            bank,
            sources: vec![Source::Mute; bank.size()],
        }
    }
}

// ---------------------------------------------------------------------------
// Mixers

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct MixerChannel {
    pub level: Atten,
    pub pan: Pan,
    pub mute: bool,
    pub solo: bool,
    pub send: Atten,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Mixer {
    pub master: MixerChannel,
    /// Channels 1..=32 at index 0..=31. The wire ch byte is index + 1.
    pub channels: [MixerChannel; MIXER_CHANNELS],
}

// ---------------------------------------------------------------------------
// Trims

/// dBu = 24 − whole − fract/100 (fract sign inferred).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct TrimLevel {
    pub whole: u8,
    pub fract: u8,
}

impl TrimLevel {
    pub fn dbu(self) -> f32 {
        24.0 - self.whole as f32 - self.fract as f32 / 100.0
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TrimRecord {
    pub control: TrimControl,
    pub levels: [TrimLevel; TRIM_CHANNELS],
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Trims {
    pub line_out: TrimRecord,
    pub line_in: TrimRecord,
}

// ---------------------------------------------------------------------------
// Reverb

/// One reverb slot (also the `set_reverb` payload, in wire order).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Reverb {
    pub mixer: MixerId,
    pub room_size: u8,
    pub color: u8,
    pub predelay: u8,
    pub density: u8,
    pub early_ref_gain: u8,
    pub late_ref_delay: u8,
    pub richness: u8,
    pub reverb_time: u8,
    pub reverb_level: u8,
    pub on: bool,
}

// ---------------------------------------------------------------------------
// AFX

/// An occupied chain slot: model plus instance 0..=15.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct AfxSlot {
    pub ty: AfxType,
    pub inst: u8,
}

/// One ClearQ band (wire order freq, qual, gain, ftype).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct EqBand {
    /// Hz.
    pub freq: u16,
    /// Q × 100; bands 0 and 4 always 0.
    pub q: u16,
    /// dB × 100, -2400..=1200.
    pub gain: i16,
    pub ftype: EqFilter,
}

pub const EQ_BANDS: usize = 5;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct EqParams {
    pub bands: [EqBand; EQ_BANDS],
}

/// PowerFFC parameters in wire order (protocol-spec.md sec. 10).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct CompressorParams {
    /// µs, 100..=150000.
    pub attack: u32,
    /// µs, 1000..=600000.
    pub release: u32,
    /// 65535 = peak, else RMS window × 100.
    pub taw: u16,
    /// ratio × 100, 100..=2000.
    pub ratio: u16,
    /// makeup dB × 100, 0..=2400.
    pub gain: u16,
    /// Always 0 from the panel.
    pub ctrl: u8,
    /// −dBFS, 0..=80.
    pub threshold: u8,
    /// dB, 0..=30.
    pub knee: u8,
    pub linked: u8,
}

/// Field description for the raw (vintage / PowerGate) models. Tables are
/// in `names::afx_fields`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AfxField {
    pub name: &'static str,
    /// Bytes on the wire: 1, 2 or 4.
    pub width: u8,
    pub signed: bool,
    pub min: i32,
    pub max: i32,
    pub default: i32,
    /// False for fields present only in the get reply (none known on fw 1.50).
    pub in_set: bool,
}

/// A model without a dedicated struct: values in get-reply field order
/// (without `enabled`), described by `names::afx_fields(ty)`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AfxRaw {
    pub ty: AfxType,
    pub fields: Vec<i32>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "model", rename_all = "snake_case")]
pub enum AfxParams {
    Eq(EqParams),
    Compressor(CompressorParams),
    Raw(AfxRaw),
}

/// One instance's config as read back: `enabled` = not bypassed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AfxConfig {
    pub enabled: bool,
    pub params: AfxParams,
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct AfxState {
    /// [chain][slot]; None = empty (wire (0,0)).
    pub order: [[Option<AfxSlot>; AFX_SLOTS]; AFX_CHAINS],
    /// Stereo link per chain pair.
    pub links: [bool; AFX_CHAINS / 2],
    /// Available instances per type (get 12/0).
    pub instances: BTreeMap<AfxType, u8>,
    /// Configs for the types that are read, indexed by instance.
    pub configs: BTreeMap<AfxType, Vec<AfxConfig>>,
}

// ---------------------------------------------------------------------------
// Whole device

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeviceState {
    pub info: DeviceInfo,
    pub status: Status,
    /// Indexed by `OutputBank as usize`.
    pub routing: Vec<RoutingBank>,
    pub mixers: [Mixer; 4],
    /// [mixer][channel pair]; true = linked.
    pub mixer_links: [[bool; 16]; 4],
    pub trims: Trims,
    /// Slot → config; None = unused slot (mixer_id 0xFF).
    pub reverb: [Option<Reverb>; 4],
    pub afx: AfxState,
    pub hdx_delays: [[u8; 32]; 2],
}

/// Host-side snapshot file (`openorion snapshot save`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Snapshot {
    /// Format version, currently 1.
    pub version: u32,
    /// Unix time of capture.
    pub taken: u64,
    pub state: DeviceState,
}

/// Stereo-link target for `set_stereo_link`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum LinkTarget {
    /// Channel pair 0..=15 (channels 2p+1, 2p+2).
    Mixer { mixer: MixerId, pair: u8 },
    /// Chain pair 0..=7.
    Afx { pair: u8 },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn newtypes_validate() {
        assert!(Pan::new(70).is_err());
        assert!(Pan::new(64).is_err());
        assert!(Pan::new(0).is_ok());
        assert_eq!(Pan::new(32).unwrap().display(), 0);
        assert!(Atten::new(97).is_err());
        assert!(PresetSlot::new(0).is_err());
        assert!(MixerId::new(4).is_err());
        let r: Result<Pan, _> = serde_json::from_str("70");
        assert!(r.is_err());
    }

    #[test]
    fn enums_roundtrip() {
        for &t in AfxType::ALL {
            assert_eq!(AfxType::try_from(t.as_u8()).unwrap(), t);
        }
        assert!(AfxType::try_from(10).is_err());
        assert!(SourceGroup::try_from(SOURCE_GROUP_MUTE).is_err());
        assert_eq!(OutputBank::ALL.len(), 15);
        assert_eq!(SampleRate::from_hz(96_000), Some(SampleRate::R96k));
    }
}
