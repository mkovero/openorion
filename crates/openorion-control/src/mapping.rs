//! Mappings from MIDI / OSC sources to parameters, and the pure value
//! transform between a control and a parameter (range, invert, relative
//! encoders, toggles, soft takeover).
//!
//! A source maps to exactly one parameter; a parameter can have several
//! sources. MIDI sources carry the device (ALSA client name, `*` = any), so
//! CC 7 on two controllers are two bindings.

use std::fmt;

use serde::{Deserialize, Serialize};

use crate::param::{Kind, Param};

/// `Source::Midi::device` value that matches every device.
pub const ANY_DEVICE: &str = "*";

/// The MIDI control a source listens to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MidiCtl {
    /// 7-bit controller 0..=127.
    Cc(u8),
    /// 14-bit controller: MSB on `n` (0..=31), LSB on `n + 32`.
    Cc14(u8),
    /// Note number; velocity is the value, note-off is 0.
    Note(u8),
    PitchBend,
}

impl MidiCtl {
    /// Whether an incoming control `ev` (always `Cc`, `Note` or
    /// `PitchBend`) belongs to this one. A 14-bit controller takes both
    /// its MSB and LSB controllers.
    pub fn matches(self, ev: MidiCtl) -> bool {
        match (self, ev) {
            (MidiCtl::Cc14(m), MidiCtl::Cc(n)) => n == m || n == m + 32,
            (a, b) => a == b,
        }
    }
}

impl fmt::Display for MidiCtl {
    /// Short badge text: `CC7`, `CC14 1`, `N36`, `PB`.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            MidiCtl::Cc(n) => write!(f, "CC{n}"),
            MidiCtl::Cc14(n) => write!(f, "CC14 {n}"),
            MidiCtl::Note(n) => write!(f, "N{n}"),
            MidiCtl::PitchBend => f.write_str("PB"),
        }
    }
}

/// Where a value comes from. Mappings may use wildcards (`*` device, no
/// channel, no sender); events from the engine are always concrete.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Source {
    Midi {
        /// ALSA client name, or `*` for any.
        device: String,
        /// 0-based channel; None = any. 1..=16 in JSON.
        #[serde(default, with = "chan1")]
        ch: Option<u8>,
        ctl: MidiCtl,
    },
    Osc {
        /// Sender host; None = any.
        #[serde(default)]
        from: Option<String>,
        addr: String,
        /// Argument index.
        #[serde(default)]
        arg: usize,
    },
}

/// MIDI channels as 1..=16 in the config file.
mod chan1 {
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(ch: &Option<u8>, s: S) -> Result<S::Ok, S::Error> {
        match ch {
            Some(c) => s.serialize_some(&(c + 1)),
            None => s.serialize_none(),
        }
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Option<u8>, D::Error> {
        match Option::<u8>::deserialize(d)? {
            None => Ok(None),
            Some(c @ 1..=16) => Ok(Some(c - 1)),
            Some(c) => Err(serde::de::Error::custom(format!(
                "MIDI channel {c} (1..=16)"
            ))),
        }
    }
}

impl Source {
    /// How well this (possibly wildcard) source matches a concrete event:
    /// None = no match, higher = more specific.
    pub fn matches(&self, ev: &Source) -> Option<u8> {
        match (self, ev) {
            (
                Source::Midi { device, ch, ctl },
                Source::Midi {
                    device: ed,
                    ch: ec,
                    ctl: ectl,
                },
            ) => {
                let dev = if device == ANY_DEVICE {
                    0
                } else if device == ed {
                    2
                } else {
                    return None;
                };
                let chan = match ch {
                    None => 0,
                    Some(c) if Some(*c) == *ec => 1,
                    Some(_) => return None,
                };
                ctl.matches(*ectl).then_some(dev + chan)
            }
            (
                Source::Osc { from, addr, arg },
                Source::Osc {
                    from: ef,
                    addr: ea,
                    arg: eg,
                },
            ) => {
                if addr != ea || arg != eg {
                    return None;
                }
                match from {
                    None => Some(0),
                    Some(f) if Some(f) == ef.as_ref() => Some(1),
                    Some(_) => None,
                }
            }
            _ => None,
        }
    }

    /// The MIDI device, if a MIDI source.
    pub fn device(&self) -> Option<&str> {
        match self {
            Source::Midi { device, .. } => Some(device),
            Source::Osc { .. } => None,
        }
    }

    /// Short badge text without the device: `CC7`, `N36`, `OSC`.
    pub fn badge(&self) -> String {
        match self {
            Source::Midi { ctl, .. } => ctl.to_string(),
            Source::Osc { .. } => "OSC".into(),
        }
    }
}

impl fmt::Display for Source {
    /// `nanoKONTROL2 ch1 CC7`, `* CC7`, `OSC /fader1`, `OSC /xy[1] from host`.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Source::Midi { device, ch, ctl } => {
                write!(f, "{device}")?;
                if let Some(c) = ch {
                    write!(f, " ch{}", c + 1)?;
                }
                write!(f, " {ctl}")
            }
            Source::Osc { from, addr, arg } => {
                write!(f, "OSC {addr}")?;
                if *arg != 0 {
                    write!(f, "[{arg}]")?;
                }
                if let Some(h) = from {
                    write!(f, " from {h}")?;
                }
                Ok(())
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Options and the value transform

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Mode {
    /// The control's position is the value.
    #[default]
    Absolute,
    /// Endless encoder: 7-bit two's complement steps (1 = +1, 127 = -1).
    Relative,
    /// Each press (note-on, CC ≥ 64 after < 64) flips the parameter.
    Toggle,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Takeover {
    /// The parameter jumps to the control.
    #[default]
    Jump,
    /// Soft takeover: ignore the control until it reaches the value.
    Pickup,
}

/// Per-mapping options. `lo`/`hi` are the parameter range (norm) the
/// control spans; `hi < lo` works and reverses the direction.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct MapOpts {
    pub invert: bool,
    pub lo: f32,
    pub hi: f32,
    pub mode: Mode,
    pub takeover: Takeover,
}

impl Default for MapOpts {
    fn default() -> Self {
        MapOpts {
            invert: false,
            lo: 0.0,
            hi: 1.0,
            mode: Mode::Absolute,
            takeover: Takeover::Jump,
        }
    }
}

/// Soft-takeover and edge state of one mapping, kept by the engine.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PickupState {
    /// Norm distance that counts as "at the value" (`Param::pickup_tolerance`).
    pub tol: f32,
    /// The control has caught the parameter.
    pub caught: bool,
    /// Previous transformed input (absolute) or raw input (toggle).
    pub last_in: Option<f32>,
    /// Last value this mapping produced.
    pub sent: Option<f32>,
}

impl PickupState {
    pub fn new(tol: f32) -> Self {
        PickupState {
            tol,
            caught: false,
            last_in: None,
            sent: None,
        }
    }

    pub fn for_param(p: &Param) -> Self {
        PickupState::new(p.pickup_tolerance())
    }

    /// Feed a parameter value seen in feedback. `quiet` = this mapping's
    /// control sent nothing recently (so the change came from elsewhere,
    /// not from our own writes catching up). A foreign change releases
    /// the control; it must pick the value up again.
    pub fn observe(&mut self, current: f32, quiet: bool) {
        if quiet && self.sent.is_none_or(|s| (s - current).abs() > self.tol) {
            self.caught = false;
        }
    }
}

impl Default for PickupState {
    fn default() -> Self {
        PickupState::new(0.02)
    }
}

impl MapOpts {
    /// Defaults for a new binding: notes on toggle parameters flip
    /// (`Toggle`), everything else is absolute.
    pub fn default_for(source: &Source, param: &Param) -> MapOpts {
        let note = matches!(
            source,
            Source::Midi {
                ctl: MidiCtl::Note(_),
                ..
            }
        );
        let mode = if note && param.kind() == Kind::Toggle {
            Mode::Toggle
        } else {
            Mode::Absolute
        };
        MapOpts {
            mode,
            ..MapOpts::default()
        }
    }

    fn bounds(&self) -> (f32, f32) {
        let lo = self.lo.clamp(0.0, 1.0);
        let hi = self.hi.clamp(0.0, 1.0);
        (lo, hi)
    }

    /// Transform a control input into a parameter value (norm), or None
    /// when nothing should be sent (not picked up yet, toggle release,
    /// relative with no known value).
    ///
    /// `raw` depends on the mode: absolute and toggle take the control's
    /// position 0..=1 (`norm7` and friends); relative takes a signed norm
    /// delta (`rel7(v) as f32 * param.tick()` for MIDI encoders).
    /// `current` is the parameter's value from the device state.
    pub fn apply(&self, raw: f32, current: Option<f32>, st: &mut PickupState) -> Option<f32> {
        if !raw.is_finite() {
            return None;
        }
        let (lo, hi) = self.bounds();
        match self.mode {
            Mode::Absolute => {
                let x = raw.clamp(0.0, 1.0);
                let x = if self.invert { 1.0 - x } else { x };
                let v = lo + x * (hi - lo);
                let prev = st.last_in.replace(v);
                if self.takeover == Takeover::Pickup
                    && !st.caught
                    && let Some(c) = current
                {
                    let near = (v - c).abs() <= st.tol;
                    let crossed = prev.is_some_and(|p| (p - c) * (v - c) <= 0.0);
                    if !near && !crossed {
                        return None;
                    }
                }
                st.caught = true;
                st.sent = Some(v);
                Some(v)
            }
            Mode::Relative => {
                let c = current?;
                let d = if self.invert { -raw } else { raw };
                let v = (c + d).clamp(lo.min(hi), lo.max(hi));
                st.sent = Some(v);
                Some(v)
            }
            Mode::Toggle => {
                let pressed = raw >= 0.5;
                let was = st.last_in.replace(raw).is_some_and(|p| p >= 0.5);
                if !pressed || was {
                    return None;
                }
                let at_hi = current.is_some_and(|c| (c - hi).abs() < (c - lo).abs());
                let v = if at_hi { lo } else { hi };
                st.sent = Some(v);
                Some(v)
            }
        }
    }

    /// Control position (0..=1) that shows parameter value `norm`, for
    /// feedback (motor faders, LED rings). Inverse of absolute `apply`.
    pub fn feedback(&self, norm: f32) -> f32 {
        let (lo, hi) = self.bounds();
        let x = if (hi - lo).abs() < f32::EPSILON {
            if norm >= lo { 1.0 } else { 0.0 }
        } else {
            ((norm - lo) / (hi - lo)).clamp(0.0, 1.0)
        };
        if self.invert { 1.0 - x } else { x }
    }
}

// ---------------------------------------------------------------------------
// MIDI helpers

/// 7-bit value → 0..=1.
pub fn norm7(v: u8) -> f32 {
    v.min(127) as f32 / 127.0
}

/// 14-bit value (MSB, LSB) → 0..=1.
pub fn norm14(msb: u8, lsb: u8) -> f32 {
    (((msb as u32 & 0x7F) << 7) | (lsb as u32 & 0x7F)) as f32 / 16383.0
}

/// Pitch bend data bytes (LSB first, as on the wire) → 0..=1.
pub fn norm_pitchbend(lsb: u8, msb: u8) -> f32 {
    norm14(msb, lsb)
}

/// 0..=1 → 7-bit.
pub fn denorm7(n: f32) -> u8 {
    (n.clamp(0.0, 1.0) * 127.0).round() as u8
}

/// 0..=1 → 14-bit (MSB, LSB).
pub fn denorm14(n: f32) -> (u8, u8) {
    let v = (n.clamp(0.0, 1.0) * 16383.0).round() as u16;
    ((v >> 7) as u8, (v & 0x7F) as u8)
}

/// Relative encoder value, 7-bit two's complement: 1..=63 up, 65..=127
/// down (127 = -1), 64 = -64.
pub fn rel7(v: u8) -> i32 {
    let v = (v & 0x7F) as i32;
    if v < 64 { v } else { v - 128 }
}

/// A channel message the mapper understands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MidiMsg {
    Cc {
        ch: u8,
        num: u8,
        value: u8,
    },
    /// Velocity 0 is parsed as `NoteOff`.
    NoteOn {
        ch: u8,
        note: u8,
        vel: u8,
    },
    NoteOff {
        ch: u8,
        note: u8,
    },
    /// 0..=16383, centre 8192.
    PitchBend {
        ch: u8,
        value: u16,
    },
}

impl MidiMsg {
    /// Parse one channel message (running status is not handled).
    pub fn parse(b: &[u8]) -> Option<MidiMsg> {
        let (&st, d) = b.split_first()?;
        let ch = st & 0x0F;
        let d1 = *d.first()? & 0x7F;
        let d2 = d.get(1).map(|v| v & 0x7F);
        Some(match st & 0xF0 {
            0xB0 => MidiMsg::Cc {
                ch,
                num: d1,
                value: d2?,
            },
            0x90 if d2? > 0 => MidiMsg::NoteOn {
                ch,
                note: d1,
                vel: d2?,
            },
            0x90 | 0x80 => MidiMsg::NoteOff { ch, note: d1 },
            0xE0 => MidiMsg::PitchBend {
                ch,
                value: (d2? as u16) << 7 | d1 as u16,
            },
            _ => return None,
        })
    }

    pub fn ch(&self) -> u8 {
        match *self {
            MidiMsg::Cc { ch, .. }
            | MidiMsg::NoteOn { ch, .. }
            | MidiMsg::NoteOff { ch, .. }
            | MidiMsg::PitchBend { ch, .. } => ch,
        }
    }

    /// The control this message moves (14-bit pairing is the caller's job:
    /// a CC arrives as `Cc` and `MidiCtl::matches` pairs it).
    pub fn ctl(&self) -> MidiCtl {
        match *self {
            MidiMsg::Cc { num, .. } => MidiCtl::Cc(num),
            MidiMsg::NoteOn { note, .. } | MidiMsg::NoteOff { note, .. } => MidiCtl::Note(note),
            MidiMsg::PitchBend { .. } => MidiCtl::PitchBend,
        }
    }

    /// The value as 0..=1 (7-bit for CC and notes).
    pub fn norm(&self) -> f32 {
        match *self {
            MidiMsg::Cc { value, .. } => norm7(value),
            MidiMsg::NoteOn { vel, .. } => norm7(vel),
            MidiMsg::NoteOff { .. } => 0.0,
            MidiMsg::PitchBend { value, .. } => value as f32 / 16383.0,
        }
    }

    /// The concrete source of this message on `device`.
    pub fn source(&self, device: &str) -> Source {
        Source::Midi {
            device: device.to_owned(),
            ch: Some(self.ch()),
            ctl: self.ctl(),
        }
    }
}

/// MIDI bytes that show `pos` (0..=1) on control `ctl`, channel `ch`
/// (0-based): CC, both halves of a 14-bit CC, note-on (velocity 0 = off)
/// or pitch bend.
pub fn midi_feedback(ch: u8, ctl: MidiCtl, pos: f32) -> Vec<u8> {
    let ch = ch & 0x0F;
    match ctl {
        MidiCtl::Cc(n) => vec![0xB0 | ch, n & 0x7F, denorm7(pos)],
        MidiCtl::Cc14(n) => {
            let (msb, lsb) = denorm14(pos);
            vec![0xB0 | ch, n & 0x1F, msb, 0xB0 | ch, (n & 0x1F) + 32, lsb]
        }
        MidiCtl::Note(n) => vec![0x90 | ch, n & 0x7F, denorm7(pos)],
        MidiCtl::PitchBend => {
            let (msb, lsb) = denorm14(pos);
            vec![0xE0 | ch, lsb, msb]
        }
    }
}

// ---------------------------------------------------------------------------
// Mappings

/// One binding. The options are flattened into the mapping in JSON.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Mapping {
    pub source: Source,
    pub param: Param,
    #[serde(flatten)]
    pub opts: MapOpts,
}

impl Mapping {
    /// MIDI bytes that show parameter value `norm` on this mapping's
    /// control. None for OSC sources. A wildcard channel answers on
    /// channel 1.
    pub fn midi_feedback(&self, norm: f32) -> Option<Vec<u8>> {
        match &self.source {
            Source::Midi { ch, ctl, .. } => Some(midi_feedback(
                ch.unwrap_or(0),
                *ctl,
                self.opts.feedback(norm),
            )),
            Source::Osc { .. } => None,
        }
    }
}

/// The binding table.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Mappings(pub Vec<Mapping>);

impl Mappings {
    pub fn new() -> Self {
        Mappings::default()
    }

    pub fn iter(&self) -> std::slice::Iter<'_, Mapping> {
        self.0.iter()
    }

    pub fn len(&self) -> usize {
        self.0.len()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// The mapping for a concrete event: the most specific match (exact
    /// device over `*`, exact channel over any; first on a tie).
    pub fn lookup(&self, ev: &Source) -> Option<&Mapping> {
        let mut best: Option<(u8, &Mapping)> = None;
        for m in &self.0 {
            if let Some(s) = m.source.matches(ev)
                && best.is_none_or(|(b, _)| s > b)
            {
                best = Some((s, m));
            }
        }
        best.map(|(_, m)| m)
    }

    /// The mapping with exactly this source.
    pub fn get(&self, source: &Source) -> Option<&Mapping> {
        self.0.iter().find(|m| m.source == *source)
    }

    /// Mappings that drive `param` (for feedback).
    pub fn for_param<'a>(&'a self, param: &'a Param) -> impl Iterator<Item = &'a Mapping> + 'a {
        self.0.iter().filter(move |m| m.param == *param)
    }

    /// Bind `source` to `param` with default options. A source maps to one
    /// parameter: an existing binding of the same source is replaced (and
    /// returned). Rebinding to the same parameter keeps its options.
    pub fn bind(&mut self, source: Source, param: Param) -> Option<Mapping> {
        if let Some(m) = self
            .0
            .iter()
            .find(|m| m.source == source && m.param == param)
        {
            return Some(m.clone());
        }
        let opts = MapOpts::default_for(&source, &param);
        self.insert(Mapping {
            source,
            param,
            opts,
        })
    }

    /// Insert a mapping, replacing (and returning) one with the same
    /// source, in place.
    pub fn insert(&mut self, m: Mapping) -> Option<Mapping> {
        match self.0.iter_mut().find(|x| x.source == m.source) {
            Some(x) => Some(std::mem::replace(x, m)),
            None => {
                self.0.push(m);
                None
            }
        }
    }

    /// Remove the mapping with this exact source.
    pub fn unbind_source(&mut self, source: &Source) -> Option<Mapping> {
        let i = self.0.iter().position(|m| m.source == *source)?;
        Some(self.0.remove(i))
    }

    /// Remove every mapping of `param`; returns how many.
    pub fn unbind_param(&mut self, param: &Param) -> usize {
        let n = self.0.len();
        self.0.retain(|m| m.param != *param);
        n - self.0.len()
    }

    /// Change the options of the mapping with this source.
    pub fn set_opts(&mut self, source: &Source, opts: MapOpts) -> bool {
        match self.0.iter_mut().find(|m| m.source == *source) {
            Some(m) => {
                m.opts = opts;
                true
            }
            None => false,
        }
    }
}

impl<'a> IntoIterator for &'a Mappings {
    type Item = &'a Mapping;
    type IntoIter = std::slice::Iter<'a, Mapping>;
    fn into_iter(self) -> Self::IntoIter {
        self.0.iter()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use openorion_proto::{MixerId, VolumeId};

    fn cc(device: &str, ch: Option<u8>, n: u8) -> Source {
        Source::Midi {
            device: device.into(),
            ch,
            ctl: MidiCtl::Cc(n),
        }
    }

    fn level(ch: u8) -> Param {
        Param::Mixer {
            mixer: MixerId::new(3).unwrap(),
            ch,
            field: crate::ChField::Level,
        }
    }

    #[test]
    fn rebind_rules() {
        let mut m = Mappings::new();
        assert!(m.bind(cc("nano", Some(0), 7), level(1)).is_none());
        // Same source again: replaced, still one mapping.
        let old = m.bind(cc("nano", Some(0), 7), level(2)).unwrap();
        assert_eq!(old.param, level(1));
        assert_eq!(m.len(), 1);
        assert_eq!(m.get(&cc("nano", Some(0), 7)).unwrap().param, level(2));
        // Same CC on another device is a separate binding; a param can
        // have several sources.
        m.bind(cc("other", Some(0), 7), level(2));
        assert_eq!(m.len(), 2);
        assert_eq!(m.for_param(&level(2)).count(), 2);
        // Rebinding to the same param keeps the options.
        let src = cc("nano", Some(0), 7);
        let opts = MapOpts {
            invert: true,
            ..MapOpts::default()
        };
        assert!(m.set_opts(&src, opts));
        m.bind(src.clone(), level(2));
        assert!(m.get(&src).unwrap().opts.invert);
        // Unbind.
        assert_eq!(m.unbind_param(&level(2)), 2);
        assert!(m.is_empty());
        m.bind(src.clone(), level(3));
        assert!(m.unbind_source(&src).is_some());
        assert!(m.unbind_source(&src).is_none());
    }

    #[test]
    fn lookup_wildcards() {
        let mut m = Mappings::new();
        m.bind(cc(ANY_DEVICE, None, 7), level(1));
        m.bind(cc("nano", None, 7), level(2));
        m.bind(cc("nano", Some(1), 7), level(3));
        let ev = |d: &str, ch: u8| cc(d, Some(ch), 7);
        assert_eq!(m.lookup(&ev("other", 0)).unwrap().param, level(1));
        assert_eq!(m.lookup(&ev("nano", 0)).unwrap().param, level(2));
        assert_eq!(m.lookup(&ev("nano", 1)).unwrap().param, level(3));
        assert!(m.lookup(&cc("nano", Some(0), 8)).is_none());

        let c14 = Source::Midi {
            device: "x".into(),
            ch: None,
            ctl: MidiCtl::Cc14(1),
        };
        m.bind(c14, level(4));
        assert_eq!(m.lookup(&ev2("x", 1)).unwrap().param, level(4));
        assert_eq!(m.lookup(&ev2("x", 33)).unwrap().param, level(4));
        fn ev2(d: &str, n: u8) -> Source {
            cc(d, Some(0), n)
        }

        let osc = |from: Option<&str>| Source::Osc {
            from: from.map(str::to_owned),
            addr: "/1/fader1".into(),
            arg: 0,
        };
        m.bind(osc(None), Param::Volume(VolumeId::Main));
        m.bind(osc(Some("tablet")), Param::Volume(VolumeId::Line));
        assert_eq!(
            m.lookup(&osc(Some("phone"))).unwrap().param,
            Param::Volume(VolumeId::Main)
        );
        assert_eq!(
            m.lookup(&osc(Some("tablet"))).unwrap().param,
            Param::Volume(VolumeId::Line)
        );
    }

    #[test]
    fn absolute_range_invert() {
        let o = MapOpts {
            lo: 0.2,
            hi: 0.6,
            invert: true,
            ..MapOpts::default()
        };
        let mut st = PickupState::default();
        assert_eq!(o.apply(0.0, None, &mut st), Some(0.6));
        let v = o.apply(1.0, None, &mut st).unwrap();
        assert!((v - 0.2).abs() < 1e-6);
        assert!((o.feedback(0.6) - 0.0).abs() < 1e-6);
        assert!((o.feedback(0.2) - 1.0).abs() < 1e-6);
        assert!(o.apply(f32::NAN, None, &mut st).is_none());
    }

    #[test]
    fn pickup() {
        let o = MapOpts {
            takeover: Takeover::Pickup,
            ..MapOpts::default()
        };
        let mut st = PickupState::new(0.02);
        // Device at 0.5; control far below: ignored until it gets there.
        assert_eq!(o.apply(0.1, Some(0.5), &mut st), None);
        assert_eq!(o.apply(0.3, Some(0.5), &mut st), None);
        // Crossing the value catches it.
        assert_eq!(o.apply(0.55, Some(0.5), &mut st), Some(0.55));
        // Caught: follows even while the device lags.
        assert_eq!(o.apply(0.9, Some(0.55), &mut st), Some(0.9));
        // Our own echo does not release.
        st.observe(0.9, true);
        assert!(st.caught);
        // A change from elsewhere (UI) releases.
        st.observe(0.2, true);
        assert!(!st.caught);
        assert_eq!(o.apply(0.8, Some(0.2), &mut st), None);
        assert_eq!(o.apply(0.21, Some(0.2), &mut st), Some(0.21), "near");
        // Jump mode never waits.
        let j = MapOpts::default();
        assert_eq!(
            j.apply(0.1, Some(0.9), &mut PickupState::default()),
            Some(0.1)
        );
    }

    #[test]
    fn relative() {
        assert_eq!(rel7(1), 1);
        assert_eq!(rel7(63), 63);
        assert_eq!(rel7(127), -1);
        assert_eq!(rel7(65), -63);
        assert_eq!(rel7(64), -64);
        let o = MapOpts {
            mode: Mode::Relative,
            ..MapOpts::default()
        };
        let mut st = PickupState::default();
        let t = 1.0 / 96.0;
        let up = o.apply(rel7(1) as f32 * t, Some(0.5), &mut st).unwrap();
        assert!((up - (0.5 + t)).abs() < 1e-6);
        let down = o.apply(rel7(126) as f32 * t, Some(0.5), &mut st).unwrap();
        assert!((down - (0.5 - 2.0 * t)).abs() < 1e-6);
        assert_eq!(o.apply(0.5, Some(0.9), &mut st), Some(1.0), "clamped");
        assert_eq!(o.apply(0.1, None, &mut st), None, "needs the value");
        let inv = MapOpts { invert: true, ..o };
        assert!(inv.apply(0.1, Some(0.5), &mut st).unwrap() < 0.5);
    }

    #[test]
    fn toggle() {
        let o = MapOpts {
            mode: Mode::Toggle,
            ..MapOpts::default()
        };
        let mut st = PickupState::default();
        // Press flips, release does nothing.
        assert_eq!(o.apply(1.0, Some(0.0), &mut st), Some(1.0));
        assert_eq!(o.apply(0.0, Some(1.0), &mut st), None);
        assert_eq!(o.apply(1.0, Some(1.0), &mut st), Some(0.0));
        // A held CC (stream of ≥ 64) flips once.
        assert_eq!(o.apply(0.8, Some(0.0), &mut st), None);
        assert_eq!(o.apply(0.2, Some(0.0), &mut st), None);
        assert_eq!(o.apply(0.6, Some(0.0), &mut st), Some(1.0));
        // Note sources on toggle params default to toggle mode.
        let note = Source::Midi {
            device: "pad".into(),
            ch: None,
            ctl: MidiCtl::Note(36),
        };
        let mute = Param::Mute(VolumeId::Main);
        assert_eq!(MapOpts::default_for(&note, &mute).mode, Mode::Toggle);
        assert_eq!(MapOpts::default_for(&note, &level(1)).mode, Mode::Absolute);
        assert_eq!(
            MapOpts::default_for(&cc("x", None, 1), &mute).mode,
            Mode::Absolute
        );
    }

    #[test]
    fn midi_bytes() {
        assert_eq!(norm7(127), 1.0);
        assert_eq!(norm14(127, 127), 1.0);
        assert_eq!(norm_pitchbend(0, 64), 8192.0 / 16383.0);
        assert_eq!(denorm14(1.0), (127, 127));
        assert_eq!(
            MidiMsg::parse(&[0xB2, 7, 100]),
            Some(MidiMsg::Cc {
                ch: 2,
                num: 7,
                value: 100
            })
        );
        assert_eq!(
            MidiMsg::parse(&[0x90, 36, 0]),
            Some(MidiMsg::NoteOff { ch: 0, note: 36 })
        );
        assert_eq!(
            MidiMsg::parse(&[0xE1, 0, 64]),
            Some(MidiMsg::PitchBend { ch: 1, value: 8192 })
        );
        assert_eq!(MidiMsg::parse(&[0xF8]), None);
        assert_eq!(MidiMsg::parse(&[0xB0, 7]), None);
        assert_eq!(midi_feedback(1, MidiCtl::Cc(7), 1.0), vec![0xB1, 7, 127]);
        assert_eq!(
            midi_feedback(0, MidiCtl::Cc14(1), 1.0),
            vec![0xB0, 1, 127, 0xB0, 33, 127]
        );
        assert_eq!(midi_feedback(0, MidiCtl::Note(36), 0.0), vec![0x90, 36, 0]);
        assert_eq!(midi_feedback(0, MidiCtl::PitchBend, 0.5), vec![0xE0, 0, 64]);
        let m = Mapping {
            source: cc("nano", None, 7),
            param: level(1),
            opts: MapOpts {
                invert: true,
                ..MapOpts::default()
            },
        };
        assert_eq!(m.midi_feedback(1.0), Some(vec![0xB0, 7, 0]));
    }

    #[test]
    fn json_shape() {
        let m = Mapping {
            source: cc("nano", Some(0), 7),
            param: level(1),
            opts: MapOpts::default(),
        };
        let s = serde_json::to_string(&m).unwrap();
        assert_eq!(
            s,
            r#"{"source":{"kind":"midi","device":"nano","ch":1,"ctl":{"cc":7}},"param":"mixer/4/ch/1/level","invert":false,"lo":0.0,"hi":1.0,"mode":"absolute","takeover":"jump"}"#
        );
        assert_eq!(serde_json::from_str::<Mapping>(&s).unwrap(), m);
        // Options and wildcards may be left out.
        let short: Mapping = serde_json::from_str(
            r#"{"source":{"kind":"osc","addr":"/x"},"param":"volume/main","mode":"relative"}"#,
        )
        .unwrap();
        assert_eq!(short.opts.mode, Mode::Relative);
        assert_eq!(short.opts.hi, 1.0);
        let bad = r#"{"source":{"kind":"midi","device":"*","ch":17,"ctl":"pitch_bend"},"param":"volume/main"}"#;
        assert!(serde_json::from_str::<Mapping>(bad).is_err());
    }
}
