//! `Param`: one mappable control, with a stable path that is also its OSC
//! address under `/orion/`. Numbers in paths are 1-based, as the panel
//! shows them; the fields of `Param` follow the model (0-based, except the
//! mixer channel, which is the wire's 1..=32 as in `Op::Mixer`).
//!
//! ```text
//! mixer/<1-4>/ch/<1-32>/{level,pan,mute,solo,send}
//! mixer/<1-4>/master/{level,mute}
//! volume/{main,line}            mute/{main,line}
//! trim/{line_in,line_out}/{<1-32>,all}
//! link/mixer/<1-4>/<pair 1-16>  link/afx/<pair 1-8>
//! reverb/<1-4>/{room_size,...,reverb_level,on}
//! afx/equalizer/<1-16>/band/<1-5>/{freq,q,gain}
//! afx/compressor/<1-16>/{attack,release,detector,ratio,threshold,knee,makeup,linked}
//! afx/<model>/<inst>/<field>    (fields: `names::afx_fields`)
//! afx/<model>/<inst>/bypass
//! ```
//!
//! Only non-disruptive ops are reachable: no power, rate, clock, presets,
//! reset or routing. Values are normalised to 0..=1; every `to_op` result
//! stays inside the panel's limits (the encoder checks in `codec.rs`).

use std::fmt;
use std::str::FromStr;

use openorion_ipc::Op;
use openorion_proto::names::{afx_fields, afx_max_instances};
use openorion_proto::*;
use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// OSC address prefix of the fixed namespace.
pub const OSC_PREFIX: &str = "/orion/";

/// Declares a fieldless enum with a path name per variant.
macro_rules! named {
    ($(#[$m:meta])* $name:ident { $($var:ident = $s:literal),+ $(,)? }) => {
        $(#[$m])*
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
        pub enum $name { $($var),+ }

        impl $name {
            pub const ALL: &'static [$name] = &[$($name::$var),+];

            pub fn name(self) -> &'static str {
                match self { $($name::$var => $s),+ }
            }

            pub fn from_name(s: &str) -> Option<$name> {
                match s { $($s => Some($name::$var),)+ _ => None }
            }
        }
    };
}

named!(
    /// A mixer channel control.
    ChField { Level = "level", Pan = "pan", Mute = "mute", Solo = "solo", Send = "send" }
);

named!(
    /// A mixer master control. The device keeps the master centred with no
    /// send, and the panel has no master solo.
    MasterField { Level = "level", Mute = "mute" }
);

named!(
    /// A reverb control (density has no panel control and is left alone).
    ReverbField {
        RoomSize = "room_size", Color = "color", Predelay = "predelay",
        EarlyRefGain = "early_ref_gain", LateRefDelay = "late_ref_delay",
        Richness = "richness", ReverbTime = "reverb_time",
        ReverbLevel = "reverb_level", On = "on",
    }
);

named!(
    /// A ClearQ band control. The filter type is not mappable.
    EqField { Freq = "freq", Q = "q", Gain = "gain" }
);

named!(
    /// A PowerFFC control. `detector` steps through the panel's choices
    /// (peak, RMS 20/50/100/150/200).
    CompField {
        Attack = "attack", Release = "release", Detector = "detector",
        Ratio = "ratio", Threshold = "threshold", Knee = "knee",
        Makeup = "makeup", Linked = "linked",
    }
);

/// A mappable control. Construct by hand or with `Param::parse`; invalid
/// indices make `to_op` and `read` return None.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Param {
    /// `ch` 1..=32 (wire numbering).
    Mixer {
        mixer: MixerId,
        ch: u8,
        field: ChField,
    },
    Master {
        mixer: MixerId,
        field: MasterField,
    },
    Volume(VolumeId),
    Mute(VolumeId),
    /// Line in / line out only (the monitor trim set is unconfirmed).
    /// `ch` 0..=31; None = every channel (the panel's "all" knob).
    Trim {
        id: TrimId,
        ch: Option<u8>,
    },
    /// Channel pair 0..=15.
    MixerLink {
        mixer: MixerId,
        pair: u8,
    },
    /// Chain pair 0..=7.
    AfxLink {
        pair: u8,
    },
    /// The reverb slot assigned to `mixer`.
    Reverb {
        mixer: MixerId,
        field: ReverbField,
    },
    /// ClearQ instance 0..=15, band 0..=4. Q exists on bands 1..=3 only.
    Eq {
        inst: u8,
        band: u8,
        field: EqField,
    },
    Comp {
        inst: u8,
        field: CompField,
    },
    /// Field index into `names::afx_fields(ty)` (set fields only).
    AfxRaw {
        ty: AfxType,
        inst: u8,
        field: u8,
    },
    /// 1 = bypassed (the model's `enabled` = 0).
    Bypass {
        ty: AfxType,
        inst: u8,
    },
}

/// How a parameter behaves. Continuous ranges are in display units
/// (dB, dBu, Hz, ms, pan -30..=+30, raw knob values); 0..=1 spans them.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Kind {
    Continuous {
        min: f32,
        max: f32,
    },
    /// On at norm ≥ 0.5.
    Toggle,
    /// Fires on a press; no parameter uses it yet.
    Trigger,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("unknown parameter path \"{0}\"")]
pub struct ParseParamError(pub String);

// ---------------------------------------------------------------------------
// Value scales

/// ClearQ frequency limits per band (codec `check_eq_band`).
const EQ_FREQ: [(i32, i32); EQ_BANDS] = [
    (20, 800),
    (20, 800),
    (125, 8000),
    (400, 20_000),
    (400, 20_000),
];
/// Q × 100 on bands 1..=3.
const EQ_Q: (i32, i32) = (50, 1800);
/// Gain dB × 100, in the panel's 0.1 dB steps.
const EQ_GAIN: (i32, i32) = (-2400, 1200);
/// The panel's detector choices, in its order.
const TAW: [u16; 6] = [65535, 2000, 5000, 10000, 15000, 20000];
const ATTACK_US: (i32, i32) = (100, 150_000);
const RELEASE_US: (i32, i32) = (1000, 600_000);
/// Ratio × 100.
const RATIO: (i32, i32) = (100, 2000);
/// Makeup dB × 100, 0.1 dB steps.
const MAKEUP: (i32, i32) = (0, 2400);
/// Trim: dB below 24 dBu, whole dB as the panel steps (10..=24 dBu).
const TRIM_MAX_DOWN: i32 = 14;
const TRIM_DBU_MAX: f32 = 24.0;

/// Linear, rounded to whole units.
fn lin(n: f32, lo: i32, hi: i32) -> i32 {
    ((lo as f32 + n * (hi - lo) as f32).round() as i32).clamp(lo, hi)
}

fn unlin(v: i32, lo: i32, hi: i32) -> f32 {
    ((v - lo) as f32 / (hi - lo) as f32).clamp(0.0, 1.0)
}

/// Linear, rounded to multiples of `step`.
fn lin_step(n: f32, lo: i32, hi: i32, step: i32) -> i32 {
    (lin(n, lo / step, hi / step) * step).clamp(lo, hi)
}

/// Logarithmic (frequencies, times, Q, ratio).
fn log(n: f32, lo: i32, hi: i32) -> i32 {
    let v = lo as f64 * (hi as f64 / lo as f64).powf(n as f64);
    (v.round() as i32).clamp(lo, hi)
}

fn unlog(v: i32, lo: i32, hi: i32) -> f32 {
    let v = v.clamp(lo, hi) as f64;
    ((v / lo as f64).ln() / (hi as f64 / lo as f64).ln()) as f32
}

/// 1 = 0 dB, 0 = 96 dB down (-inf), linear in dB.
fn atten(n: f32) -> Atten {
    let a = ((1.0 - n) * Atten::NEG_INF.get() as f32).round() as u8;
    Atten::new(a).unwrap_or(Atten::NEG_INF)
}

fn unatten(a: Atten) -> f32 {
    1.0 - a.get() as f32 / Atten::NEG_INF.get() as f32
}

fn flag(b: bool) -> f32 {
    if b { 1.0 } else { 0.0 }
}

fn reverb_range(f: ReverbField) -> (i32, i32) {
    match f {
        ReverbField::Predelay | ReverbField::LateRefDelay => (0, 150),
        ReverbField::ReverbLevel => (1, 100),
        _ => (0, 100),
    }
}

fn reverb_field(r: &mut Reverb, f: ReverbField) -> Option<&mut u8> {
    Some(match f {
        ReverbField::RoomSize => &mut r.room_size,
        ReverbField::Color => &mut r.color,
        ReverbField::Predelay => &mut r.predelay,
        ReverbField::EarlyRefGain => &mut r.early_ref_gain,
        ReverbField::LateRefDelay => &mut r.late_ref_delay,
        ReverbField::Richness => &mut r.richness,
        ReverbField::ReverbTime => &mut r.reverb_time,
        ReverbField::ReverbLevel => &mut r.reverb_level,
        ReverbField::On => return None,
    })
}

fn reverb_of(state: &DeviceState, m: MixerId) -> Option<Reverb> {
    state
        .reverb
        .iter()
        .flatten()
        .find(|r| r.mixer == m)
        .copied()
}

fn afx_cfg(state: &DeviceState, ty: AfxType, inst: u8) -> Option<&AfxConfig> {
    state.afx.configs.get(&ty)?.get(inst as usize)
}

fn eq_band(state: &DeviceState, inst: u8, band: u8) -> Option<EqBand> {
    match &afx_cfg(state, AfxType::Equalizer, inst)?.params {
        AfxParams::Eq(p) => p.bands.get(band as usize).copied(),
        _ => None,
    }
}

fn comp(state: &DeviceState, inst: u8) -> Option<CompressorParams> {
    match &afx_cfg(state, AfxType::Compressor, inst)?.params {
        AfxParams::Compressor(p) => Some(*p),
        _ => None,
    }
}

fn mixer_channel(state: &DeviceState, m: MixerId, ch: u8) -> Option<&MixerChannel> {
    let mx = &state.mixers[m.get() as usize];
    if ch == 0 {
        Some(&mx.master)
    } else {
        mx.channels.get(ch as usize - 1)
    }
}

fn trim_record(state: &DeviceState, id: TrimId) -> Option<&TrimRecord> {
    match id {
        TrimId::LineIn => Some(&state.trims.line_in),
        TrimId::LineOut => Some(&state.trims.line_out),
        TrimId::Monitor => None,
    }
}

fn mixer_op(mixer: MixerId, ch: u8) -> Op {
    Op::Mixer {
        mixer,
        ch,
        level: None,
        pan: None,
        mute: None,
        solo: None,
        send: None,
    }
}

/// The raw field `field` of `ty`, if it is a settable field.
fn raw_field(ty: AfxType, field: u8) -> Option<&'static AfxField> {
    afx_fields(ty)?.get(field as usize).filter(|f| f.in_set)
}

fn is_switch(f: &AfxField) -> bool {
    f.min == 0 && f.max == 1
}

/// Path name of an AFX model: the variant name in snake case
/// (`PultecMeq` → `pultec_meq`, `Neve1073` → `neve1073`).
pub fn afx_slug(ty: AfxType) -> String {
    let mut s = String::new();
    for (i, c) in format!("{ty:?}").chars().enumerate() {
        if c.is_ascii_uppercase() {
            if i > 0 {
                s.push('_');
            }
            s.push(c.to_ascii_lowercase());
        } else {
            s.push(c);
        }
    }
    s
}

fn afx_from_slug(s: &str) -> Option<AfxType> {
    AfxType::ALL.iter().copied().find(|&t| afx_slug(t) == s)
}

fn volume_name(id: VolumeId) -> &'static str {
    match id {
        VolumeId::Main => "main",
        VolumeId::Line => "line",
    }
}

fn trim_name(id: TrimId) -> &'static str {
    match id {
        TrimId::Monitor => "monitor",
        TrimId::LineOut => "line_out",
        TrimId::LineIn => "line_in",
    }
}

// ---------------------------------------------------------------------------
// Param

impl Param {
    /// True when every index is in range and the target is mappable.
    pub fn is_valid(&self) -> bool {
        match *self {
            Param::Mixer { ch, .. } => (1..=MIXER_CHANNELS as u8).contains(&ch),
            Param::Master { .. } | Param::Volume(_) | Param::Mute(_) | Param::Reverb { .. } => true,
            Param::Trim { id, ch } => {
                id != TrimId::Monitor && ch.is_none_or(|c| (c as usize) < TRIM_CHANNELS)
            }
            Param::MixerLink { pair, .. } => pair < (MIXER_CHANNELS / 2) as u8,
            Param::AfxLink { pair } => pair < (AFX_CHAINS / 2) as u8,
            Param::Eq { inst, band, field } => {
                inst < afx_max_instances(AfxType::Equalizer)
                    && (band as usize) < EQ_BANDS
                    && (field != EqField::Q || (1..=3).contains(&band))
            }
            Param::Comp { inst, .. } => inst < afx_max_instances(AfxType::Compressor),
            Param::AfxRaw { ty, inst, field } => {
                ty.on_orion() && inst < afx_max_instances(ty) && raw_field(ty, field).is_some()
            }
            Param::Bypass { ty, inst } => ty.on_orion() && inst < afx_max_instances(ty),
        }
    }

    pub fn kind(&self) -> Kind {
        let c = |min: f32, max: f32| Kind::Continuous { min, max };
        match *self {
            Param::Mixer { field, .. } => match field {
                ChField::Level | ChField::Send => c(-96.0, 0.0),
                ChField::Pan => c(
                    (Pan::UI_MIN as i8 - 32) as f32,
                    (Pan::UI_MAX as i8 - 32) as f32,
                ),
                ChField::Mute | ChField::Solo => Kind::Toggle,
            },
            Param::Master { field, .. } => match field {
                MasterField::Level => c(-96.0, 0.0),
                MasterField::Mute => Kind::Toggle,
            },
            Param::Volume(_) => c(-96.0, 0.0),
            Param::Trim { .. } => c(TRIM_DBU_MAX - TRIM_MAX_DOWN as f32, TRIM_DBU_MAX),
            Param::Mute(_) | Param::MixerLink { .. } | Param::AfxLink { .. } => Kind::Toggle,
            Param::Bypass { .. } => Kind::Toggle,
            Param::Reverb { field, .. } => match field {
                ReverbField::On => Kind::Toggle,
                f => {
                    let (lo, hi) = reverb_range(f);
                    c(lo as f32, hi as f32)
                }
            },
            Param::Eq { band, field, .. } => match field {
                EqField::Freq => {
                    let (lo, hi) = EQ_FREQ[(band as usize).min(EQ_BANDS - 1)];
                    c(lo as f32, hi as f32)
                }
                EqField::Q => c(EQ_Q.0 as f32 / 100.0, EQ_Q.1 as f32 / 100.0),
                EqField::Gain => c(EQ_GAIN.0 as f32 / 100.0, EQ_GAIN.1 as f32 / 100.0),
            },
            Param::Comp { field, .. } => match field {
                CompField::Attack => c(ATTACK_US.0 as f32 / 1000.0, ATTACK_US.1 as f32 / 1000.0),
                CompField::Release => c(RELEASE_US.0 as f32 / 1000.0, RELEASE_US.1 as f32 / 1000.0),
                CompField::Detector => c(0.0, (TAW.len() - 1) as f32),
                CompField::Ratio => c(RATIO.0 as f32 / 100.0, RATIO.1 as f32 / 100.0),
                CompField::Threshold => c(-80.0, 0.0),
                CompField::Knee => c(0.0, 30.0),
                CompField::Makeup => c(MAKEUP.0 as f32 / 100.0, MAKEUP.1 as f32 / 100.0),
                CompField::Linked => Kind::Toggle,
            },
            Param::AfxRaw { ty, field, .. } => match raw_field(ty, field) {
                Some(f) if is_switch(f) => Kind::Toggle,
                Some(f) => c(f.min as f32, f.max as f32),
                None => Kind::Trigger,
            },
        }
    }

    /// Distinct values the device can take (None for toggles and
    /// triggers). Sets the step of relative encoders and the pickup
    /// tolerance.
    pub fn steps(&self) -> Option<u32> {
        if !matches!(self.kind(), Kind::Continuous { .. }) {
            return None;
        }
        let span = |lo: i32, hi: i32| Some((hi - lo) as u32 + 1);
        match *self {
            Param::Mixer {
                field: ChField::Pan,
                ..
            } => span(Pan::UI_MIN as i32, Pan::UI_MAX as i32),
            Param::Mixer { .. } | Param::Master { .. } | Param::Volume(_) => {
                span(0, Atten::NEG_INF.get() as i32)
            }
            Param::Trim { .. } => span(0, TRIM_MAX_DOWN),
            Param::Reverb { field, .. } => {
                let (lo, hi) = reverb_range(field);
                span(lo, hi)
            }
            Param::Eq { band, field, .. } => match field {
                EqField::Freq => {
                    let (lo, hi) = EQ_FREQ[band as usize];
                    span(lo, hi)
                }
                EqField::Q => span(EQ_Q.0, EQ_Q.1),
                EqField::Gain => span(EQ_GAIN.0 / 10, EQ_GAIN.1 / 10),
            },
            Param::Comp { field, .. } => match field {
                CompField::Attack => span(ATTACK_US.0, ATTACK_US.1),
                CompField::Release => span(RELEASE_US.0, RELEASE_US.1),
                CompField::Detector => span(0, TAW.len() as i32 - 1),
                CompField::Ratio => span(RATIO.0, RATIO.1),
                CompField::Threshold => span(0, 80),
                CompField::Knee => span(0, 30),
                CompField::Makeup => span(MAKEUP.0 / 10, MAKEUP.1 / 10),
                CompField::Linked => None,
            },
            Param::AfxRaw { ty, field, .. } => {
                raw_field(ty, field).and_then(|f| span(f.min, f.max))
            }
            _ => None,
        }
    }

    /// Norm change of one relative-encoder tick: one device step, but at
    /// least 1/200 of the range so fine-grained values still move.
    pub fn tick(&self) -> f32 {
        match self.steps() {
            Some(n) if n > 1 => (1.0 / (n - 1) as f32).max(1.0 / 200.0),
            _ => 1.0,
        }
    }

    /// How close (in norm) a control must come to the device value to take
    /// it over in pickup mode: half a device step, at least 0.02.
    pub fn pickup_tolerance(&self) -> f32 {
        match self.steps() {
            Some(n) if n > 1 => (0.5 / (n - 1) as f32 + 0.005).max(0.02),
            _ => 0.5,
        }
    }

    /// The op that sets this parameter to `norm` (0..=1, clamped). Ops that
    /// carry a whole record (reverb, compressor) take the other fields from
    /// `state`. None when the parameter is invalid, `norm` is not finite,
    /// or `state` lacks the record (e.g. no reverb slot on that mixer).
    /// Toggles are on at norm ≥ 0.5.
    pub fn to_op(&self, norm: f32, state: &DeviceState) -> Option<Op> {
        if !norm.is_finite() || !self.is_valid() {
            return None;
        }
        let n = norm.clamp(0.0, 1.0);
        let on = n >= 0.5;
        Some(match *self {
            Param::Mixer { mixer, ch, field } => {
                let mut op = mixer_op(mixer, ch);
                if let Op::Mixer {
                    level,
                    pan,
                    mute,
                    solo,
                    send,
                    ..
                } = &mut op
                {
                    match field {
                        ChField::Level => *level = Some(atten(n)),
                        ChField::Send => *send = Some(atten(n)),
                        ChField::Mute => *mute = Some(on),
                        ChField::Solo => *solo = Some(on),
                        ChField::Pan => {
                            let p = lin(n, Pan::UI_MIN as i32, Pan::UI_MAX as i32);
                            *pan = Some(Pan::new(p as u8).ok()?)
                        }
                    }
                }
                op
            }
            Param::Master { mixer, field } => {
                let mut op = mixer_op(mixer, 0);
                if let Op::Mixer { level, mute, .. } = &mut op {
                    match field {
                        MasterField::Level => *level = Some(atten(n)),
                        MasterField::Mute => *mute = Some(on),
                    }
                }
                op
            }
            Param::Volume(id) => Op::Volume {
                id,
                atten: atten(n),
            },
            Param::Mute(id) => Op::Mute { id, mute: on },
            Param::Trim { id, ch } => Op::Trim {
                id,
                ch,
                level: TrimLevel {
                    whole: lin(1.0 - n, 0, TRIM_MAX_DOWN) as u8,
                    fract: 0,
                },
            },
            Param::MixerLink { mixer, pair } => Op::StereoLink {
                target: LinkTarget::Mixer { mixer, pair },
                linked: on,
            },
            Param::AfxLink { pair } => Op::StereoLink {
                target: LinkTarget::Afx { pair },
                linked: on,
            },
            Param::Reverb { mixer, field } => {
                let mut r = reverb_of(state, mixer)?;
                match reverb_field(&mut r, field) {
                    Some(v) => {
                        let (lo, hi) = reverb_range(field);
                        *v = lin(n, lo, hi) as u8;
                    }
                    None => r.on = on,
                }
                Op::Reverb { params: r }
            }
            Param::Eq { inst, band, field } => {
                let b = eq_band(state, inst, band)?;
                let mut op = Op::AfxEqBand {
                    inst,
                    band,
                    freq: None,
                    q: None,
                    gain: None,
                    ftype: None,
                };
                if let Op::AfxEqBand { freq, q, gain, .. } = &mut op {
                    match field {
                        EqField::Freq => {
                            let (lo, hi) = EQ_FREQ[band as usize];
                            *freq = Some(log(n, lo, hi) as u16);
                        }
                        EqField::Q => *q = Some(log(n, EQ_Q.0, EQ_Q.1) as u16),
                        EqField::Gain => {
                            // Cut filters have no gain (the panel greys it).
                            if matches!(b.ftype, EqFilter::Lpf | EqFilter::Hpf) {
                                return None;
                            }
                            *gain = Some(lin_step(n, EQ_GAIN.0, EQ_GAIN.1, 10) as i16);
                        }
                    }
                }
                op
            }
            Param::Comp { inst, field } => {
                let mut p = comp(state, inst)?;
                match field {
                    CompField::Attack => p.attack = log(n, ATTACK_US.0, ATTACK_US.1) as u32,
                    CompField::Release => p.release = log(n, RELEASE_US.0, RELEASE_US.1) as u32,
                    CompField::Detector => p.taw = TAW[lin(n, 0, TAW.len() as i32 - 1) as usize],
                    CompField::Ratio => p.ratio = log(n, RATIO.0, RATIO.1) as u16,
                    CompField::Threshold => p.threshold = lin(1.0 - n, 0, 80) as u8,
                    CompField::Knee => p.knee = lin(n, 0, 30) as u8,
                    CompField::Makeup => p.gain = lin_step(n, MAKEUP.0, MAKEUP.1, 10) as u16,
                    CompField::Linked => p.linked = on as u8,
                }
                Op::AfxCompressor { inst, params: p }
            }
            Param::AfxRaw { ty, inst, field } => {
                let f = raw_field(ty, field)?;
                let mut fields = vec![None; field as usize + 1];
                fields[field as usize] = Some(lin(n, f.min, f.max));
                Op::AfxRaw { ty, inst, fields }
            }
            Param::Bypass { ty, inst } => Op::AfxBypass {
                ty,
                inst,
                enabled: !on,
            },
        })
    }

    /// The current value in `state`, normalised to 0..=1 (toggles 0 or 1).
    /// None when the parameter is invalid or `state` lacks the record.
    pub fn read(&self, state: &DeviceState) -> Option<f32> {
        if !self.is_valid() {
            return None;
        }
        Some(match *self {
            Param::Mixer { mixer, ch, field } => {
                let c = mixer_channel(state, mixer, ch)?;
                match field {
                    ChField::Level => unatten(c.level),
                    ChField::Send => unatten(c.send),
                    ChField::Mute => flag(c.mute),
                    ChField::Solo => flag(c.solo),
                    ChField::Pan => {
                        unlin(c.pan.get() as i32, Pan::UI_MIN as i32, Pan::UI_MAX as i32)
                    }
                }
            }
            Param::Master { mixer, field } => {
                let c = mixer_channel(state, mixer, 0)?;
                match field {
                    MasterField::Level => unatten(c.level),
                    MasterField::Mute => flag(c.mute),
                }
            }
            Param::Volume(id) => unatten(match id {
                VolumeId::Main => state.status.main.atten,
                VolumeId::Line => state.status.line.atten,
            }),
            Param::Mute(id) => flag(match id {
                VolumeId::Main => state.status.main.mute,
                VolumeId::Line => state.status.line.mute,
            }),
            Param::Trim { id, ch } => {
                let l = trim_record(state, id)?.levels[ch.unwrap_or(0) as usize];
                let down = TRIM_DBU_MAX - l.dbu();
                (1.0 - down / TRIM_MAX_DOWN as f32).clamp(0.0, 1.0)
            }
            Param::MixerLink { mixer, pair } => {
                flag(state.mixer_links[mixer.get() as usize][pair as usize])
            }
            Param::AfxLink { pair } => flag(state.afx.links[pair as usize]),
            Param::Reverb { mixer, field } => {
                let mut r = reverb_of(state, mixer)?;
                let on = r.on;
                match reverb_field(&mut r, field) {
                    Some(v) => {
                        let (lo, hi) = reverb_range(field);
                        unlin(*v as i32, lo, hi)
                    }
                    None => flag(on),
                }
            }
            Param::Eq { inst, band, field } => {
                let b = eq_band(state, inst, band)?;
                match field {
                    EqField::Freq => {
                        let (lo, hi) = EQ_FREQ[band as usize];
                        unlog(b.freq as i32, lo, hi)
                    }
                    EqField::Q => unlog(b.q as i32, EQ_Q.0, EQ_Q.1),
                    EqField::Gain => unlin(b.gain as i32, EQ_GAIN.0, EQ_GAIN.1),
                }
            }
            Param::Comp { inst, field } => {
                let p = comp(state, inst)?;
                match field {
                    CompField::Attack => unlog(p.attack as i32, ATTACK_US.0, ATTACK_US.1),
                    CompField::Release => unlog(p.release as i32, RELEASE_US.0, RELEASE_US.1),
                    CompField::Detector => {
                        let i = TAW.iter().position(|&t| t == p.taw)?;
                        unlin(i as i32, 0, TAW.len() as i32 - 1)
                    }
                    CompField::Ratio => unlog(p.ratio as i32, RATIO.0, RATIO.1),
                    CompField::Threshold => 1.0 - unlin(p.threshold as i32, 0, 80),
                    CompField::Knee => unlin(p.knee as i32, 0, 30),
                    CompField::Makeup => unlin(p.gain as i32, MAKEUP.0, MAKEUP.1),
                    CompField::Linked => flag(p.linked != 0),
                }
            }
            Param::AfxRaw { ty, inst, field } => {
                let f = raw_field(ty, field)?;
                let AfxParams::Raw(r) = &afx_cfg(state, ty, inst)?.params else {
                    return None;
                };
                if r.ty != ty {
                    return None;
                }
                unlin(*r.fields.get(field as usize)?, f.min, f.max)
            }
            Param::Bypass { ty, inst } => flag(!afx_cfg(state, ty, inst)?.enabled),
        })
    }

    /// Every valid parameter, independent of the device state (several
    /// thousand: most are AFX fields). Order is stable.
    pub fn all() -> Vec<Param> {
        let mut v = Vec::new();
        for m in 0..=MixerId::MAX {
            let mixer = MixerId::new(m).expect("0..=3");
            for ch in 1..=MIXER_CHANNELS as u8 {
                for &field in ChField::ALL {
                    v.push(Param::Mixer { mixer, ch, field });
                }
            }
            for &field in MasterField::ALL {
                v.push(Param::Master { mixer, field });
            }
        }
        for &id in VolumeId::ALL {
            v.push(Param::Volume(id));
            v.push(Param::Mute(id));
        }
        for id in [TrimId::LineIn, TrimId::LineOut] {
            v.push(Param::Trim { id, ch: None });
            for ch in 0..TRIM_CHANNELS as u8 {
                v.push(Param::Trim { id, ch: Some(ch) });
            }
        }
        for m in 0..=MixerId::MAX {
            let mixer = MixerId::new(m).expect("0..=3");
            for pair in 0..(MIXER_CHANNELS / 2) as u8 {
                v.push(Param::MixerLink { mixer, pair });
            }
        }
        for pair in 0..(AFX_CHAINS / 2) as u8 {
            v.push(Param::AfxLink { pair });
        }
        for m in 0..=MixerId::MAX {
            let mixer = MixerId::new(m).expect("0..=3");
            for &field in ReverbField::ALL {
                v.push(Param::Reverb { mixer, field });
            }
        }
        for &ty in AfxType::ALL.iter().filter(|t| t.on_orion()) {
            for inst in 0..afx_max_instances(ty) {
                match ty {
                    AfxType::Equalizer => {
                        for band in 0..EQ_BANDS as u8 {
                            for &field in EqField::ALL {
                                v.push(Param::Eq { inst, band, field });
                            }
                        }
                    }
                    AfxType::Compressor => {
                        for &field in CompField::ALL {
                            v.push(Param::Comp { inst, field });
                        }
                    }
                    _ => {
                        let n = afx_fields(ty).map_or(0, |t| t.len());
                        for field in 0..n as u8 {
                            v.push(Param::AfxRaw { ty, inst, field });
                        }
                    }
                }
                v.push(Param::Bypass { ty, inst });
            }
        }
        v.retain(Param::is_valid);
        v
    }

    /// The parameters `state` holds a value for (feedback, `Announce`).
    pub fn available(state: &DeviceState) -> Vec<Param> {
        Param::all()
            .into_iter()
            .filter(|p| p.read(state).is_some())
            .collect()
    }

    /// The namespace as patterns, one line per family (for `--help-osc`).
    pub fn namespace() -> Vec<String> {
        let names = |n: &[&str]| n.join(",");
        let ch: Vec<_> = ChField::ALL.iter().map(|f| f.name()).collect();
        let master: Vec<_> = MasterField::ALL.iter().map(|f| f.name()).collect();
        let reverb: Vec<_> = ReverbField::ALL.iter().map(|f| f.name()).collect();
        let comp: Vec<_> = CompField::ALL.iter().map(|f| f.name()).collect();
        let mut v = vec![
            format!("mixer/<1-4>/ch/<1-32>/{{{}}}", names(&ch)),
            format!("mixer/<1-4>/master/{{{}}}", names(&master)),
            "volume/{main,line}".into(),
            "mute/{main,line}".into(),
            "trim/{line_in,line_out}/{<1-32>,all}".into(),
            "link/mixer/<1-4>/<pair 1-16>".into(),
            "link/afx/<pair 1-8>".into(),
            format!("reverb/<1-4>/{{{}}}", names(&reverb)),
            "afx/equalizer/<1-16>/band/<1-5>/{freq,gain}  (q on bands 2-4)".into(),
            format!("afx/compressor/<1-16>/{{{},bypass}}", names(&comp)),
        ];
        for &ty in AfxType::ALL.iter().filter(|t| t.on_orion()) {
            let Some(t) = afx_fields(ty) else { continue };
            let mut f: Vec<_> = t.iter().filter(|f| f.in_set).map(|f| f.name).collect();
            f.push("bypass");
            v.push(format!(
                "afx/{}/<1-{}>/{{{}}}",
                afx_slug(ty),
                afx_max_instances(ty),
                names(&f)
            ));
        }
        v.push("afx/equalizer/<1-16>/bypass".into());
        v
    }

    /// `/orion/<path>`.
    pub fn osc_addr(&self) -> String {
        format!("{OSC_PREFIX}{self}")
    }

    /// Parse an OSC address in the fixed namespace (`/orion/<path>`).
    pub fn from_osc(addr: &str) -> Option<Param> {
        Param::parse(addr.strip_prefix(OSC_PREFIX)?).ok()
    }

    /// Parse a path (leading `/` allowed). Only valid parameters parse.
    pub fn parse(s: &str) -> Result<Param, ParseParamError> {
        parse_path(s.trim().trim_start_matches('/'))
            .filter(Param::is_valid)
            .ok_or_else(|| ParseParamError(s.to_owned()))
    }
}

/// 1-based path number → 0-based index below `n`.
fn idx(s: &str, n: usize) -> Option<u8> {
    let v: usize = s.parse().ok()?;
    (1..=n).contains(&v).then(|| (v - 1) as u8)
}

fn mixer_id(s: &str) -> Option<MixerId> {
    MixerId::new(idx(s, 4)?).ok()
}

fn parse_volume(s: &str) -> Option<VolumeId> {
    VolumeId::ALL.iter().copied().find(|&v| volume_name(v) == s)
}

fn parse_path(s: &str) -> Option<Param> {
    let w: Vec<&str> = s.split('/').collect();
    Some(match w.as_slice() {
        ["mixer", m, "ch", c, f] => Param::Mixer {
            mixer: mixer_id(m)?,
            ch: idx(c, MIXER_CHANNELS)? + 1,
            field: ChField::from_name(f)?,
        },
        ["mixer", m, "master", f] => Param::Master {
            mixer: mixer_id(m)?,
            field: MasterField::from_name(f)?,
        },
        ["volume", v] => Param::Volume(parse_volume(v)?),
        ["mute", v] => Param::Mute(parse_volume(v)?),
        ["trim", id, c] => Param::Trim {
            id: [TrimId::LineIn, TrimId::LineOut]
                .into_iter()
                .find(|&t| trim_name(t) == *id)?,
            ch: match *c {
                "all" => None,
                c => Some(idx(c, TRIM_CHANNELS)?),
            },
        },
        ["link", "mixer", m, p] => Param::MixerLink {
            mixer: mixer_id(m)?,
            pair: idx(p, MIXER_CHANNELS / 2)?,
        },
        ["link", "afx", p] => Param::AfxLink {
            pair: idx(p, AFX_CHAINS / 2)?,
        },
        ["reverb", m, f] => Param::Reverb {
            mixer: mixer_id(m)?,
            field: ReverbField::from_name(f)?,
        },
        ["afx", "equalizer", i, "band", b, f] => Param::Eq {
            inst: idx(i, 16)?,
            band: idx(b, EQ_BANDS)?,
            field: EqField::from_name(f)?,
        },
        ["afx", t, i, f] => {
            let ty = afx_from_slug(t)?;
            let inst = idx(i, afx_max_instances(ty) as usize)?;
            if *f == "bypass" {
                Param::Bypass { ty, inst }
            } else if ty == AfxType::Compressor {
                Param::Comp {
                    inst,
                    field: CompField::from_name(f)?,
                }
            } else {
                let field = afx_fields(ty)?
                    .iter()
                    .position(|x| x.in_set && x.name == *f)?;
                Param::AfxRaw {
                    ty,
                    inst,
                    field: field as u8,
                }
            }
        }
        _ => return None,
    })
}

impl fmt::Display for Param {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            Param::Mixer { mixer, ch, field } => {
                write!(f, "mixer/{}/ch/{ch}/{}", mixer.get() + 1, field.name())
            }
            Param::Master { mixer, field } => {
                write!(f, "mixer/{}/master/{}", mixer.get() + 1, field.name())
            }
            Param::Volume(id) => write!(f, "volume/{}", volume_name(id)),
            Param::Mute(id) => write!(f, "mute/{}", volume_name(id)),
            Param::Trim { id, ch: None } => write!(f, "trim/{}/all", trim_name(id)),
            Param::Trim { id, ch: Some(c) } => write!(f, "trim/{}/{}", trim_name(id), c + 1),
            Param::MixerLink { mixer, pair } => {
                write!(f, "link/mixer/{}/{}", mixer.get() + 1, pair + 1)
            }
            Param::AfxLink { pair } => write!(f, "link/afx/{}", pair + 1),
            Param::Reverb { mixer, field } => {
                write!(f, "reverb/{}/{}", mixer.get() + 1, field.name())
            }
            Param::Eq { inst, band, field } => write!(
                f,
                "afx/equalizer/{}/band/{}/{}",
                inst + 1,
                band + 1,
                field.name()
            ),
            Param::Comp { inst, field } => {
                write!(f, "afx/compressor/{}/{}", inst + 1, field.name())
            }
            Param::AfxRaw { ty, inst, field } => {
                let name = afx_fields(ty)
                    .and_then(|t| t.get(field as usize))
                    .map_or("?", |x| x.name);
                write!(f, "afx/{}/{}/{name}", afx_slug(ty), inst + 1)
            }
            Param::Bypass { ty, inst } => write!(f, "afx/{}/{}/bypass", afx_slug(ty), inst + 1),
        }
    }
}

impl FromStr for Param {
    type Err = ParseParamError;
    fn from_str(s: &str) -> Result<Param, ParseParamError> {
        Param::parse(s)
    }
}

/// Serialised as its path string.
impl Serialize for Param {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for Param {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Param, D::Error> {
        let s = String::deserialize(d)?;
        Param::parse(&s).map_err(serde::de::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths() {
        let p = Param::parse("mixer/4/ch/1/level").unwrap();
        assert_eq!(
            p,
            Param::Mixer {
                mixer: MixerId::new(3).unwrap(),
                ch: 1,
                field: ChField::Level
            }
        );
        assert_eq!(p.osc_addr(), "/orion/mixer/4/ch/1/level");
        assert_eq!(Param::from_osc("/orion/mixer/4/ch/1/level"), Some(p));
        assert_eq!(
            Param::parse("/volume/main").unwrap(),
            Param::Volume(VolumeId::Main)
        );
        assert_eq!(afx_slug(AfxType::PultecMeq), "pultec_meq");
        assert_eq!(afx_slug(AfxType::Neve1073), "neve1073");
        assert_eq!(
            Param::parse("afx/uad1176/3/ratio").unwrap().to_string(),
            "afx/uad1176/3/ratio"
        );
        for bad in [
            "mixer/0/ch/1/level",
            "mixer/5/ch/1/level",
            "mixer/1/ch/33/level",
            "mixer/1/master/pan",
            "mixer/1/master/send",
            "trim/monitor/all",
            "trim/line_in/0",
            "afx/equalizer/1/band/1/q",
            "afx/equalizer/17/band/2/q",
            "afx/powergate/13/bypass",
            "afx/guitar_amp/1/bypass",
            "afx/uad1176/1/nope",
            "reverb/1/density",
            "link/afx/9",
            "",
        ] {
            assert!(Param::parse(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn serde_as_path() {
        let p = Param::Mute(VolumeId::Line);
        assert_eq!(serde_json::to_string(&p).unwrap(), r#""mute/line""#);
        let back: Param = serde_json::from_str(r#""mute/line""#).unwrap();
        assert_eq!(back, p);
        assert!(serde_json::from_str::<Param>(r#""mute/side""#).is_err());
    }

    #[test]
    fn scales() {
        assert_eq!(atten(1.0), Atten::ZERO_DB);
        assert_eq!(atten(0.0), Atten::NEG_INF);
        assert_eq!(atten(0.5).get(), 48);
        assert_eq!(log(0.0, 20, 800), 20);
        assert_eq!(log(1.0, 20, 800), 800);
        assert!((unlog(log(0.37, 100, 150_000), 100, 150_000) - 0.37).abs() < 0.001);
        assert_eq!(lin_step(0.5, -2400, 1200, 10), -600);
        assert_eq!(lin_step(1.0, -2400, 1200, 10), 1200);
    }

    #[test]
    fn namespace_covers_every_family() {
        let ns = Param::namespace();
        assert!(ns.iter().any(|l| l.starts_with("afx/powergate/<1-12>/")));
        assert!(ns.iter().all(|l| !l.contains("guitar")));
    }
}
