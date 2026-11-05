//! Request encoding and report parsing (DESIGN.md sec. 4.2, 4.3).
//!
//! Signatures are the WP0 contract; bodies are WP1. Wire format:
//! protocol-spec.md sec. 2-5.

use crate::decode::{decode_afx_meters, decode_status};
use crate::error::ProtoError;
use crate::model::*;
use crate::names::{afx_fields, afx_max_instances};

/// Every report is exactly this long. Transports prepend the 0x00 report
/// number byte for hidraw (321 bytes written).
pub const REPORT_LEN: usize = 320;
/// Header: `id, len, ext2, ext3`, little-endian u32 each.
pub const HEADER_LEN: usize = 16;
/// Room for a payload or reply body after the header.
pub const BODY_LEN: usize = REPORT_LEN - HEADER_LEN;

pub const ID_SET: u32 = 0x70;
pub const ID_STATUS: u32 = 0x73;
pub const ID_GET: u32 = 0x74;
pub const ID_REPLY: u32 = 0x75;
pub const ID_AFX_METERS: u32 = 0x83;
pub const ID_UNKNOWN_CMD: u32 = 0x61;
pub const ID_SEGMENT: u32 = 0x1F75;

/// Set payload ids that may be sent (protocol-spec.md sec. 5, DESIGN.md
/// 4.3). Anything else is not representable.
pub const SET_PID_WHITELIST: &[u8] = &[
    3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 16, 17, 18, 19, 20, 21, 24, 26,
];

/// Wire byte of the routing MUTE group.
const MUTE_GROUP: u8 = SOURCE_GROUP_MUTE;
/// Routing entries carried by every set_routing (padded with (0,0)).
pub const ROUTING_ENTRIES: usize = 32;
/// Highest meter bank A/B source index (spec sec. 8).
pub const METER_SOURCE_MAX: u8 = 28;

/// One outgoing report. The only thing a transport writes. There is no
/// constructor from arbitrary bytes; see `raw_checked`.
#[derive(Clone, PartialEq, Eq)]
pub struct Report([u8; REPORT_LEN]);

impl Report {
    /// Build a header-plus-payload report. Crate-private: only `Get`/`Set`
    /// and `raw_checked` call it.
    pub(crate) fn build(
        id: u32,
        len: u32,
        ext2: u32,
        ext3: u32,
        payload: &[u8],
    ) -> Result<Report, ProtoError> {
        if payload.len() > BODY_LEN {
            return Err(ProtoError::TooLong(payload.len()));
        }
        let mut b = [0u8; REPORT_LEN];
        b[0..4].copy_from_slice(&id.to_le_bytes());
        b[4..8].copy_from_slice(&len.to_le_bytes());
        b[8..12].copy_from_slice(&ext2.to_le_bytes());
        b[12..16].copy_from_slice(&ext3.to_le_bytes());
        b[HEADER_LEN..HEADER_LEN + payload.len()].copy_from_slice(payload);
        Ok(Report(b))
    }

    /// Wrap an encoded set payload (`byte0 = pid | nparams << 6 …`) after
    /// checking its pid against `SET_PID_WHITELIST`. For `openorion raw --i-know`.
    ///
    /// Also checks that the payload length agrees with its own
    /// `nparams`/length byte, so a truncated or overlong payload is refused.
    pub fn raw_checked(payload: &[u8]) -> Result<Report, ProtoError> {
        let Some(&b0) = payload.first() else {
            return Err(ProtoError::Refused("empty set payload".into()));
        };
        let pid = b0 & 0x3f;
        if !SET_PID_WHITELIST.contains(&pid) {
            return Err(ProtoError::Refused(format!(
                "set pid {pid} (byte0 {b0:#04x}) is not whitelisted"
            )));
        }
        split_set_payload(payload)?;
        Report::build(ID_SET, (HEADER_LEN + payload.len()) as u32, 0, 0, payload)
    }

    pub fn as_bytes(&self) -> &[u8; REPORT_LEN] {
        &self.0
    }

    pub fn id(&self) -> u32 {
        u32::from_le_bytes(self.0[0..4].try_into().unwrap())
    }

    pub fn ext2(&self) -> u32 {
        u32::from_le_bytes(self.0[8..12].try_into().unwrap())
    }

    pub fn ext3(&self) -> u32 {
        u32::from_le_bytes(self.0[12..16].try_into().unwrap())
    }
}

impl std::fmt::Debug for Report {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let end = self
            .0
            .iter()
            .rposition(|&b| b != 0)
            .map_or(HEADER_LEN, |i| (i + 1).max(HEADER_LEN));
        write!(f, "Report(")?;
        for b in &self.0[..end] {
            write!(f, "{b:02x}")?;
        }
        write!(f, ")")
    }
}

/// Requests (0x74). ext2/ext3 per protocol-spec.md sec. 4. There is no
/// variant for ext2 17 (licensing).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Get {
    Firmware,
    CompatPassport,
    Passport,
    Registration,
    Routing(OutputBank),
    Mixer(MixerId),
    /// ext3 = type, except Equalizer: ext3 = half (0 = inst 0-7, 1 = 8-15).
    AfxConfigs(AfxType, u8),
    AfxOrder,
    Reverb,
    MixerLinks,
    AfxLinks,
    AfxInstances,
    HdxDelays,
    Trims,
}

impl Get {
    pub fn ext2(&self) -> u32 {
        match self {
            Get::Firmware | Get::CompatPassport => 0,
            Get::Passport => 1,
            Get::Registration => 2,
            Get::Routing(_) => 3,
            Get::Mixer(_) => 4,
            Get::AfxConfigs(..) => 7,
            Get::AfxOrder => 9,
            Get::Reverb => 10,
            Get::MixerLinks | Get::AfxLinks => 11,
            Get::AfxInstances => 12,
            Get::HdxDelays => 13,
            Get::Trims => 14,
        }
    }

    pub fn ext3(&self) -> u32 {
        match self {
            Get::CompatPassport => 1,
            Get::Routing(b) => b.as_u8() as u32,
            Get::Mixer(m) => m.get() as u32,
            // EQ: half 0 = inst 0-7; any other value = half 1 (inst 8-15).
            Get::AfxConfigs(AfxType::Equalizer, half) => (*half != 0) as u32,
            Get::AfxConfigs(t, _) => t.as_u8() as u32,
            Get::MixerLinks => 3,
            Get::AfxLinks => 4,
            _ => 0,
        }
    }

    pub fn report(&self) -> Report {
        Report::build(ID_GET, HEADER_LEN as u32, self.ext2(), self.ext3(), &[])
            .expect("an empty get payload always fits")
    }

    /// True if `r` is the reply to this request (id 0x75, same ext2/ext3).
    pub fn matches(&self, r: &[u8; REPORT_LEN]) -> bool {
        le32(r, 0) == ID_REPLY && le32(r, 8) == self.ext2() && le32(r, 12) == self.ext3()
    }

    /// Inverse of `ext2`/`ext3`: the get a reply (or dump key) belongs to.
    /// For ext2 7, ext3 0/1 is an EQ half and anything else an `AfxType`
    /// (normalised to `AfxConfigs(ty, 0)`). None for anything unknown,
    /// including ext2 17.
    pub fn from_ext(ext2: u32, ext3: u32) -> Option<Get> {
        let e3 = u8::try_from(ext3).ok();
        Some(match (ext2, ext3) {
            (0, 0) => Get::Firmware,
            (0, 1) => Get::CompatPassport,
            (1, 0) => Get::Passport,
            (2, 0) => Get::Registration,
            (3, _) => Get::Routing(OutputBank::try_from(e3?).ok()?),
            (4, _) => Get::Mixer(MixerId::new(e3?).ok()?),
            (7, 0 | 1) => Get::AfxConfigs(AfxType::Equalizer, ext3 as u8),
            (7, _) => Get::AfxConfigs(AfxType::try_from(e3?).ok()?, 0),
            (9, 0) => Get::AfxOrder,
            (10, 0) => Get::Reverb,
            (11, 3) => Get::MixerLinks,
            (11, 4) => Get::AfxLinks,
            (12, 0) => Get::AfxInstances,
            (13, 0) => Get::HdxDelays,
            (14, 0) => Get::Trims,
            _ => return None,
        })
    }

    /// Every get of a full read: the base gets plus the AFX
    /// configs of every on-Orion model. What a resync sends.
    pub fn read_all() -> Vec<Get> {
        let mut v = vec![
            Get::Firmware,
            Get::CompatPassport,
            Get::Passport,
            Get::Registration,
        ];
        v.extend(OutputBank::ALL.iter().map(|&b| Get::Routing(b)));
        v.extend((0..4).map(|m| Get::Mixer(MixerId::new(m).unwrap())));
        v.push(Get::AfxConfigs(AfxType::Equalizer, 0));
        v.push(Get::AfxConfigs(AfxType::Equalizer, 1));
        v.extend(
            AfxType::ALL
                .iter()
                .filter(|t| t.on_orion() && **t != AfxType::Equalizer)
                .map(|&t| Get::AfxConfigs(t, 0)),
        );
        v.extend([
            Get::AfxOrder,
            Get::Reverb,
            Get::MixerLinks,
            Get::AfxLinks,
            Get::AfxInstances,
            Get::HdxDelays,
            Get::Trims,
        ]);
        v
    }
}

/// UX risk class (DESIGN.md 4.3). Clients confirm `Disruptive`.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize, serde::Deserialize,
)]
pub enum Risk {
    Silent,
    Audible,
    Disruptive,
}

/// Sets (0x70, no reply). Each variant carries every field of its message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Set {
    Power(bool),
    SampleRate(SampleRate),
    ClockSource(ClockSource),
    Volume(VolumeId, Atten),
    Mute(VolumeId, bool),
    PresetSave(PresetSlot),
    PresetRecall(PresetSlot),
    Oscillator(Osc),
    Brightness(Brightness),
    Trim(TrimId, TrimRecord),
    SpdifSrc(bool),
    Smux(SmuxId, bool),
    /// Bank A/B: source 0..=28 (spec sec. 8); mixer bank: mixer index.
    MeterSource(MeterBank, u8),
    /// ch 0 = master, 1..=32.
    MixerChannel(MixerId, u8, MixerChannel),
    /// Up to 32 entries; the encoder pads with (0,0).
    Routing(OutputBank, Vec<Source>),
    StereoLink(LinkTarget, bool),
    AfxEq {
        inst: u8,
        band: u8,
        value: EqBand,
    },
    AfxCompressor {
        inst: u8,
        params: CompressorParams,
    },
    /// Fields in set order (`AfxField::in_set`), as `names::afx_fields`.
    AfxRaw {
        inst: u8,
        raw: AfxRaw,
    },
    AfxOrder {
        chain: u8,
        slots: [Option<AfxSlot>; AFX_SLOTS],
    },
    /// `enabled` = not bypassed. Wire order: inst, type, enabled.
    AfxBypass {
        ty: AfxType,
        inst: u8,
        enabled: bool,
    },
    Reverb(Reverb),
}

impl Set {
    pub fn pid(&self) -> u8 {
        match self {
            Set::Power(_) => 3,
            Set::SampleRate(_) => 4,
            Set::ClockSource(_) => 5,
            Set::Volume(..) => 6,
            Set::Mute(..) => 7,
            Set::PresetSave(_) => 8,
            Set::PresetRecall(_) => 9,
            Set::Oscillator(_) => 10,
            Set::Brightness(_) => 11,
            Set::Trim(..) => 12,
            Set::SpdifSrc(_) => 13,
            Set::Smux(..) => 14,
            Set::MeterSource(..) => 16,
            Set::MixerChannel(..) => 17,
            Set::Routing(..) => 18,
            Set::StereoLink(..) => 19,
            Set::AfxEq { .. } | Set::AfxCompressor { .. } | Set::AfxRaw { .. } => 20,
            Set::AfxOrder { .. } => 21,
            Set::AfxBypass { .. } => 24,
            Set::Reverb(_) => 26,
        }
    }

    /// Encoded payload (`byte0 = pid | nparams << 6 …`, spec sec. 3).
    pub fn payload(&self) -> Result<Vec<u8>, ProtoError> {
        let pid = self.pid();
        debug_assert!(SET_PID_WHITELIST.contains(&pid));
        encode_payload(pid, &self.fields()?)
    }

    pub fn report(&self) -> Result<Report, ProtoError> {
        let p = self.payload()?;
        Report::build(ID_SET, (HEADER_LEN + p.len()) as u32, 0, 0, &p)
    }

    pub fn class(&self) -> Risk {
        match self {
            Set::Power(_)
            | Set::SampleRate(_)
            | Set::ClockSource(_)
            | Set::PresetRecall(_)
            | Set::PresetSave(_) => Risk::Disruptive,
            Set::Routing(b, _) if b.is_physical() => Risk::Disruptive,
            Set::Routing(..)
            | Set::Volume(..)
            | Set::Mute(..)
            | Set::Oscillator(_)
            | Set::Trim(..) => Risk::Audible,
            _ => Risk::Silent,
        }
    }

    /// Inverse of `report`: decode an outgoing set report. The simulator
    /// uses it to apply what clients send. Unknown pids are an error.
    ///
    /// Strict: the decoded set must re-encode to the same payload (bools
    /// other than 0/1, out-of-range values and non-canonical length bytes
    /// are refused). Routing entries past the bank size are ignored.
    pub fn decode(r: &[u8; REPORT_LEN]) -> Result<Set, ProtoError> {
        let id = le32(r, 0);
        if id != ID_SET {
            return Err(malformed("set report", format!("id {id:#x} is not 0x70")));
        }
        if le32(r, 8) != 0 || le32(r, 12) != 0 {
            return Err(malformed("set report", "ext2/ext3 must be 0".into()));
        }
        let len = le32(r, 4) as usize;
        if !(HEADER_LEN + 2..=REPORT_LEN).contains(&len) {
            return Err(malformed("set report", format!("bad length {len}")));
        }
        let payload = &r[HEADER_LEN..len];
        let (pid, f) = split_set_payload(payload)?;
        if !SET_PID_WHITELIST.contains(&pid) {
            return Err(ProtoError::Refused(format!(
                "set pid {pid} is not whitelisted"
            )));
        }
        let set = decode_fields(pid, f)?;
        let re = set.payload()?;
        let same = match &set {
            Set::Routing(b, _) => {
                let n = 2 + 1 + 2 * b.size();
                re.len() == payload.len() && re[..n] == payload[..n]
            }
            _ => re == payload,
        };
        if !same {
            return Err(malformed(
                "set payload",
                "not canonical (does not re-encode identically)".into(),
            ));
        }
        Ok(set)
    }

    /// The field block (everything after byte 0, or bytes 0-1 when
    /// `nparams` = 3), with range checks.
    fn fields(&self) -> Result<Vec<u8>, ProtoError> {
        let mut w = Vec::with_capacity(70);
        match self {
            Set::Power(on) => w.push(*on as u8),
            Set::SampleRate(r) => w.push(r.as_u8()),
            Set::ClockSource(c) => w.push(c.as_u8()),
            Set::Volume(id, a) => w.extend([id.as_u8(), a.get()]),
            Set::Mute(id, m) => w.extend([id.as_u8(), *m as u8]),
            Set::PresetSave(s) | Set::PresetRecall(s) => w.push(s.get()),
            Set::Oscillator(o) => w.push(osc_to_u8(o)),
            Set::Brightness(b) => w.push(b.get()),
            Set::Trim(id, rec) => {
                check_trim(rec)?;
                w.extend([id.as_u8(), rec.control.as_u8()]);
                for l in &rec.levels {
                    w.extend([l.whole, l.fract]);
                }
            }
            Set::SpdifSrc(on) => w.push(*on as u8),
            Set::Smux(id, on) => w.extend([id.as_u8(), *on as u8]),
            Set::MeterSource(bank, src) => {
                let max = match bank {
                    MeterBank::Mixer => MixerId::MAX,
                    _ => METER_SOURCE_MAX,
                };
                check_range("meter source", *src as i64, 0, max as i64)?;
                w.extend([bank.as_u8(), *src]);
            }
            Set::MixerChannel(m, ch, c) => {
                check_range("mixer channel", *ch as i64, 0, MIXER_CHANNELS as i64)?;
                // Master writes carry pan 0 (panel); channels stay on the knob.
                if *ch != 0 {
                    check_range(
                        "pan",
                        c.pan.get() as i64,
                        Pan::UI_MIN as i64,
                        Pan::UI_MAX as i64,
                    )?;
                }
                w.extend([m.get(), *ch]);
                w.extend(mixer_channel_bytes(c));
            }
            Set::Routing(bank, sources) => {
                check_range(
                    "routing entries",
                    sources.len() as i64,
                    0,
                    bank.size() as i64,
                )?;
                w.push(bank.as_u8());
                for s in sources {
                    w.extend(source_bytes(*s)?);
                }
                w.resize(1 + 2 * ROUTING_ENTRIES, 0);
            }
            Set::StereoLink(target, linked) => {
                match *target {
                    LinkTarget::Mixer { mixer, pair } => {
                        check_range("mixer link pair", pair as i64, 0, 15)?;
                        w.extend([3, mixer.get() * 16 + pair]);
                    }
                    LinkTarget::Afx { pair } => {
                        check_range("afx link pair", pair as i64, 0, (AFX_CHAINS / 2 - 1) as i64)?;
                        w.extend([4, pair]);
                    }
                }
                w.push(*linked as u8);
            }
            Set::AfxEq { inst, band, value } => {
                check_inst(AfxType::Equalizer, *inst)?;
                check_range("eq band", *band as i64, 0, (EQ_BANDS - 1) as i64)?;
                check_eq_band(*band, value)?;
                w.extend([AfxType::Equalizer.as_u8(), *inst, *band]);
                put_eq_band(&mut w, value);
            }
            Set::AfxCompressor { inst, params } => {
                check_inst(AfxType::Compressor, *inst)?;
                check_compressor(params)?;
                w.extend([AfxType::Compressor.as_u8(), *inst]);
                put_compressor(&mut w, params);
            }
            Set::AfxRaw { inst, raw } => {
                let table = raw_table(raw.ty)?;
                check_inst(raw.ty, *inst)?;
                let n_set = table.iter().filter(|f| f.in_set).count();
                if raw.fields.len() != n_set && raw.fields.len() != table.len() {
                    return Err(malformed(
                        "afx raw fields",
                        format!(
                            "{:?}: {} values, want {n_set} (set) or {} (get)",
                            raw.ty,
                            raw.fields.len(),
                            table.len()
                        ),
                    ));
                }
                w.extend([raw.ty.as_u8(), *inst]);
                for (f, &v) in table.iter().zip(&raw.fields).filter(|(f, _)| f.in_set) {
                    check_range(f.name, v as i64, f.min as i64, f.max as i64)?;
                    put_field(&mut w, f, v);
                }
            }
            Set::AfxOrder { chain, slots } => {
                check_range("afx chain", *chain as i64, 0, (AFX_CHAINS - 1) as i64)?;
                check_chain(slots)?;
                w.push(*chain);
                for s in slots {
                    w.extend(slot_bytes(*s));
                }
            }
            Set::AfxBypass { ty, inst, enabled } => {
                check_on_orion(*ty)?;
                check_inst(*ty, *inst)?;
                w.extend([*inst, ty.as_u8(), *enabled as u8]);
            }
            Set::Reverb(r) => {
                check_reverb(r)?;
                w.extend(reverb_bytes(r));
            }
        }
        Ok(w)
    }
}

/// A parsed incoming report.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Incoming {
    Status(Box<Status>, Box<Meters>),
    AfxMeters(Box<AfxMeters>),
    /// Get reply; `body` is everything after the header (304 bytes).
    Reply {
        ext2: u32,
        ext3: u32,
        body: Vec<u8>,
    },
    /// All-zero report (after reset, while unlocked).
    Zero,
    UnknownCmd,
    Segment {
        total: u32,
        chunk_len: u32,
        offset: u32,
        data: Vec<u8>,
    },
    /// Anything else, by id. Also a 0x73 that fails to decode.
    Other(u32),
}

pub fn parse(r: &[u8; REPORT_LEN]) -> Incoming {
    if r.iter().all(|&b| b == 0) {
        return Incoming::Zero;
    }
    let id = le32(r, 0);
    let body = &r[HEADER_LEN..];
    match id {
        ID_STATUS => match decode_status(body) {
            Ok((s, m)) => Incoming::Status(Box::new(s), Box::new(m)),
            Err(_) => Incoming::Other(id),
        },
        ID_AFX_METERS => match decode_afx_meters(body) {
            Ok(m) => Incoming::AfxMeters(Box::new(m)),
            Err(_) => Incoming::Other(id),
        },
        ID_REPLY => Incoming::Reply {
            ext2: le32(r, 8),
            ext3: le32(r, 12),
            body: body.to_vec(),
        },
        ID_UNKNOWN_CMD => Incoming::UnknownCmd,
        ID_SEGMENT => {
            let chunk_len = le32(r, 8);
            let n = (chunk_len as usize).min(BODY_LEN);
            Incoming::Segment {
                total: le32(r, 4),
                chunk_len,
                offset: le32(r, 12),
                data: body[..n].to_vec(),
            }
        }
        _ => Incoming::Other(id),
    }
}

// ---------------------------------------------------------------------------
// Payload framing

/// Split an encoded set payload into `(pid, fields)`, checking the length
/// against byte 0 (and byte 1 when `nparams` = 3). Does not check the pid.
pub fn split_set_payload(payload: &[u8]) -> Result<(u8, &[u8]), ProtoError> {
    let Some(&b0) = payload.first() else {
        return Err(malformed("set payload", "empty".into()));
    };
    let pid = b0 & 0x3f;
    let nparams = b0 >> 6;
    let (start, n) = if nparams < 3 {
        (1usize, nparams as usize + 1)
    } else {
        match payload.get(1) {
            Some(&n) => (2usize, n as usize),
            None => return Err(malformed("set payload", "missing length byte".into())),
        }
    };
    if payload.len() != start + n {
        return Err(malformed(
            "set payload",
            format!(
                "length {} disagrees with its header (want {})",
                payload.len(),
                start + n
            ),
        ));
    }
    if start + n > BODY_LEN {
        return Err(ProtoError::TooLong(start + n));
    }
    Ok((pid, &payload[start..]))
}

/// Encode a field block as a set payload (spec sec. 3).
fn encode_payload(pid: u8, fields: &[u8]) -> Result<Vec<u8>, ProtoError> {
    let n = fields.len();
    let mut out = Vec::with_capacity(n + 2);
    if n == 0 {
        return Err(malformed("set payload", "no fields".into()));
    } else if n < 4 {
        out.push(pid | ((n as u8 - 1) << 6));
    } else {
        if n > 255 || n + 2 > BODY_LEN {
            return Err(ProtoError::TooLong(n + 2));
        }
        out.push(pid | (3 << 6));
        out.push(n as u8);
    }
    out.extend_from_slice(fields);
    Ok(out)
}

fn decode_fields(pid: u8, f: &[u8]) -> Result<Set, ProtoError> {
    let need = |n: usize| -> Result<(), ProtoError> {
        if f.len() == n {
            Ok(())
        } else {
            Err(malformed(
                "set fields",
                format!("pid {pid}: {} bytes, want {n}", f.len()),
            ))
        }
    };
    let set = match pid {
        3 => {
            need(1)?;
            Set::Power(f[0] != 0)
        }
        4 => {
            need(1)?;
            Set::SampleRate(SampleRate::try_from(f[0])?)
        }
        5 => {
            need(1)?;
            Set::ClockSource(ClockSource::try_from(f[0])?)
        }
        6 => {
            need(2)?;
            Set::Volume(VolumeId::try_from(f[0])?, Atten::new(f[1])?)
        }
        7 => {
            need(2)?;
            Set::Mute(VolumeId::try_from(f[0])?, f[1] != 0)
        }
        8 => {
            need(1)?;
            Set::PresetSave(PresetSlot::new(f[0])?)
        }
        9 => {
            need(1)?;
            Set::PresetRecall(PresetSlot::new(f[0])?)
        }
        10 => {
            need(1)?;
            Set::Oscillator(osc_from_u8(f[0])?)
        }
        11 => {
            need(1)?;
            Set::Brightness(Brightness::new(f[0])?)
        }
        12 => {
            need(2 + 2 * TRIM_CHANNELS)?;
            Set::Trim(TrimId::try_from(f[0])?, trim_record(f[1], &f[2..])?)
        }
        13 => {
            need(1)?;
            Set::SpdifSrc(f[0] != 0)
        }
        14 => {
            need(2)?;
            Set::Smux(SmuxId::try_from(f[0])?, f[1] != 0)
        }
        16 => {
            need(2)?;
            Set::MeterSource(MeterBank::try_from(f[0])?, f[1])
        }
        17 => {
            need(5)?;
            Set::MixerChannel(MixerId::new(f[0])?, f[1], mixer_channel_from(&f[2..5])?)
        }
        18 => {
            need(1 + 2 * ROUTING_ENTRIES)?;
            let bank = OutputBank::try_from(f[0])?;
            let sources = (0..bank.size())
                .map(|i| source_from(f[1 + 2 * i], f[2 + 2 * i]))
                .collect::<Result<Vec<_>, _>>()?;
            Set::Routing(bank, sources)
        }
        19 => {
            need(3)?;
            let target = match f[0] {
                3 => LinkTarget::Mixer {
                    mixer: MixerId::new(f[1] / 16)?,
                    pair: f[1] % 16,
                },
                4 => LinkTarget::Afx { pair: f[1] },
                p => {
                    return Err(ProtoError::UnknownValue {
                        what: "stereo link peripheral",
                        value: p as i64,
                    });
                }
            };
            Set::StereoLink(target, f[2] != 0)
        }
        20 => {
            if f.len() < 2 {
                return Err(short("afx set", 2, f.len()));
            }
            let ty = AfxType::try_from(f[0])?;
            let inst = f[1];
            let mut rd = Rd::new("afx set", &f[2..]);
            let set = match ty {
                AfxType::Equalizer => {
                    let band = rd.u8()?;
                    let value = get_eq_band(&mut rd)?;
                    Set::AfxEq { inst, band, value }
                }
                AfxType::Compressor => Set::AfxCompressor {
                    inst,
                    params: get_compressor(&mut rd)?,
                },
                _ => {
                    let table = raw_table(ty)?;
                    let fields = table
                        .iter()
                        .filter(|f| f.in_set)
                        .map(|fd| get_field(&mut rd, fd))
                        .collect::<Result<Vec<_>, _>>()?;
                    Set::AfxRaw {
                        inst,
                        raw: AfxRaw { ty, fields },
                    }
                }
            };
            if rd.remaining() != 0 {
                return Err(malformed(
                    "afx set",
                    format!("{ty:?}: {} trailing bytes", rd.remaining()),
                ));
            }
            set
        }
        21 => {
            need(1 + 2 * AFX_SLOTS)?;
            let mut slots = [None; AFX_SLOTS];
            for (i, s) in slots.iter_mut().enumerate() {
                *s = slot_from(f[1 + 2 * i], f[2 + 2 * i])?;
            }
            Set::AfxOrder { chain: f[0], slots }
        }
        24 => {
            need(3)?;
            Set::AfxBypass {
                inst: f[0],
                ty: AfxType::try_from(f[1])?,
                enabled: f[2] != 0,
            }
        }
        26 => {
            need(11)?;
            Set::Reverb(reverb_from(f)?)
        }
        _ => {
            return Err(ProtoError::Refused(format!(
                "set pid {pid} is not whitelisted"
            )));
        }
    };
    Ok(set)
}

// ---------------------------------------------------------------------------
// Shared field helpers (also used by decode.rs)

pub(crate) fn le32(b: &[u8], off: usize) -> u32 {
    u32::from_le_bytes(b[off..off + 4].try_into().unwrap())
}

pub(crate) fn malformed(what: &'static str, detail: String) -> ProtoError {
    ProtoError::Malformed { what, detail }
}

pub(crate) fn short(what: &'static str, need: usize, got: usize) -> ProtoError {
    ProtoError::Short { what, need, got }
}

pub(crate) fn check_range(
    what: &'static str,
    v: i64,
    min: i64,
    max: i64,
) -> Result<(), ProtoError> {
    if (min..=max).contains(&v) {
        Ok(())
    } else {
        Err(ProtoError::OutOfRange {
            what,
            value: v,
            min,
            max,
        })
    }
}

/// Little-endian byte reader with `Short` errors.
pub(crate) struct Rd<'a> {
    what: &'static str,
    b: &'a [u8],
    pos: usize,
}

impl<'a> Rd<'a> {
    pub(crate) fn new(what: &'static str, b: &'a [u8]) -> Self {
        Rd { what, b, pos: 0 }
    }

    pub(crate) fn take(&mut self, n: usize) -> Result<&'a [u8], ProtoError> {
        if self.pos + n > self.b.len() {
            return Err(short(self.what, self.pos + n, self.b.len()));
        }
        let s = &self.b[self.pos..self.pos + n];
        self.pos += n;
        Ok(s)
    }

    pub(crate) fn remaining(&self) -> usize {
        self.b.len() - self.pos
    }

    pub(crate) fn u8(&mut self) -> Result<u8, ProtoError> {
        Ok(self.take(1)?[0])
    }

    pub(crate) fn u16(&mut self) -> Result<u16, ProtoError> {
        Ok(u16::from_le_bytes(self.take(2)?.try_into().unwrap()))
    }

    pub(crate) fn i16(&mut self) -> Result<i16, ProtoError> {
        Ok(i16::from_le_bytes(self.take(2)?.try_into().unwrap()))
    }

    pub(crate) fn u32(&mut self) -> Result<u32, ProtoError> {
        Ok(u32::from_le_bytes(self.take(4)?.try_into().unwrap()))
    }
}

pub(crate) fn osc_to_u8(o: &Osc) -> u8 {
    o.freq_l.as_u8()
        | o.freq_r.as_u8() << 2
        | o.level.as_u8() << 4
        | (o.mute_l as u8) << 6
        | (o.mute_r as u8) << 7
}

pub(crate) fn osc_from_u8(b: u8) -> Result<Osc, ProtoError> {
    Ok(Osc {
        freq_l: OscFreq::try_from(b & 3)?,
        freq_r: OscFreq::try_from(b >> 2 & 3)?,
        level: OscLevel::try_from(b >> 4 & 3)?,
        mute_l: b >> 6 & 1 != 0,
        mute_r: b >> 7 != 0,
    })
}

pub(crate) fn mixer_channel_bytes(c: &MixerChannel) -> [u8; 3] {
    [
        c.level.get(),
        c.pan.get() | (c.mute as u8) << 6 | (c.solo as u8) << 7,
        c.send.get(),
    ]
}

/// Strict decode of `(level, pan|mute|solo, send)`.
pub(crate) fn mixer_channel_from(b: &[u8]) -> Result<MixerChannel, ProtoError> {
    Ok(MixerChannel {
        level: Atten::new(b[0])?,
        pan: Pan::new(b[1] & 63)?,
        mute: b[1] >> 6 & 1 != 0,
        solo: b[1] >> 7 != 0,
        send: Atten::new(b[2])?,
    })
}

pub(crate) fn source_bytes(s: Source) -> Result<[u8; 2], ProtoError> {
    Ok(match s {
        Source::Mute => [MUTE_GROUP, 0],
        Source::Input { group, chan } => {
            check_range(
                "routing source channel",
                chan as i64,
                0,
                group.size() as i64 - 1,
            )?;
            [group.as_u8(), chan]
        }
    })
}

pub(crate) fn source_from(group: u8, chan: u8) -> Result<Source, ProtoError> {
    if group == MUTE_GROUP {
        return Ok(Source::Mute);
    }
    Ok(Source::Input {
        group: SourceGroup::try_from(group)?,
        chan,
    })
}

pub(crate) fn slot_bytes(s: Option<AfxSlot>) -> [u8; 2] {
    match s {
        None => [0, 0],
        Some(s) => [s.ty.as_u8(), s.inst],
    }
}

pub(crate) fn slot_from(ty: u8, inst: u8) -> Result<Option<AfxSlot>, ProtoError> {
    if ty == 0 {
        if inst != 0 {
            return Err(malformed(
                "afx slot",
                format!("empty type with inst {inst}"),
            ));
        }
        return Ok(None);
    }
    Ok(Some(AfxSlot {
        ty: AfxType::try_from(ty)?,
        inst,
    }))
}

pub(crate) fn trim_record(control: u8, levels: &[u8]) -> Result<TrimRecord, ProtoError> {
    if levels.len() < 2 * TRIM_CHANNELS {
        return Err(short("trim record", 2 * TRIM_CHANNELS, levels.len()));
    }
    let mut out = [TrimLevel::default(); TRIM_CHANNELS];
    for (i, l) in out.iter_mut().enumerate() {
        *l = TrimLevel {
            whole: levels[2 * i],
            fract: levels[2 * i + 1],
        };
    }
    Ok(TrimRecord {
        control: TrimControl::try_from(control)?,
        levels: out,
    })
}

pub(crate) fn reverb_bytes(r: &Reverb) -> [u8; 11] {
    [
        r.mixer.get(),
        r.room_size,
        r.color,
        r.predelay,
        r.density,
        r.early_ref_gain,
        r.late_ref_delay,
        r.richness,
        r.reverb_time,
        r.reverb_level,
        r.on as u8,
    ]
}

pub(crate) fn reverb_from(b: &[u8]) -> Result<Reverb, ProtoError> {
    Ok(Reverb {
        mixer: MixerId::new(b[0])?,
        room_size: b[1],
        color: b[2],
        predelay: b[3],
        density: b[4],
        early_ref_gain: b[5],
        late_ref_delay: b[6],
        richness: b[7],
        reverb_time: b[8],
        reverb_level: b[9],
        on: b[10] != 0,
    })
}

/// Trims: the panel's 10..24 dBu (whole + fract/100 ≤ 14).
fn check_trim(rec: &TrimRecord) -> Result<(), ProtoError> {
    for l in &rec.levels {
        check_range("trim fract", l.fract as i64, 0, 99)?;
        check_range(
            "trim (dB below 24 dBu, ×100)",
            l.whole as i64 * 100 + l.fract as i64,
            0,
            1400,
        )?;
    }
    Ok(())
}

/// Reverb UI ranges (spec sec. 6). Density has no UI and stays raw.
fn check_reverb(r: &Reverb) -> Result<(), ProtoError> {
    check_range("reverb reverb_level", r.reverb_level as i64, 1, 100)?;
    for (what, v, max) in [
        ("reverb room_size", r.room_size, 100),
        ("reverb color", r.color, 100),
        ("reverb predelay", r.predelay, 150),
        ("reverb early_ref_gain", r.early_ref_gain, 100),
        ("reverb late_ref_delay", r.late_ref_delay, 150),
        ("reverb richness", r.richness, 100),
        ("reverb reverb_time", r.reverb_time, 100),
        ("reverb reverb_level", r.reverb_level, 100),
    ] {
        check_range(what, v as i64, 0, max)?;
    }
    Ok(())
}

pub(crate) fn put_eq_band(w: &mut Vec<u8>, b: &EqBand) {
    w.extend(b.freq.to_le_bytes());
    w.extend(b.q.to_le_bytes());
    w.extend(b.gain.to_le_bytes());
    w.push(b.ftype.as_u8());
}

pub(crate) fn get_eq_band(rd: &mut Rd) -> Result<EqBand, ProtoError> {
    Ok(EqBand {
        freq: rd.u16()?,
        q: rd.u16()?,
        gain: rd.i16()?,
        ftype: EqFilter::try_from(rd.u8()?)?,
    })
}

/// The panel's per-band limits (protocol-spec.md sec. 10). Bands 0 and 4 have
/// no Q knob: the panel sends 0, the device default is 71.
fn check_eq_band(band: u8, b: &EqBand) -> Result<(), ProtoError> {
    let (lo, hi, types): (i64, i64, &[EqFilter]) = match band {
        0 => (20, 800, &[EqFilter::LowShelf, EqFilter::Hpf]),
        1 => (20, 800, &[EqFilter::Peak]),
        2 => (125, 8000, &[EqFilter::Peak]),
        3 => (400, 20_000, &[EqFilter::Peak]),
        _ => (400, 20_000, &[EqFilter::HighShelf, EqFilter::Lpf]),
    };
    check_range("eq freq", b.freq as i64, lo, hi)?;
    let q_min = if band == 0 || band == 4 { 0 } else { 50 };
    check_range("eq q", b.q as i64, q_min, 1800)?;
    check_range("eq gain", b.gain as i64, -2400, 1200)?;
    if !types.contains(&b.ftype) {
        return Err(malformed(
            "eq filter type",
            format!("{:?} not allowed on band {band}", b.ftype),
        ));
    }
    Ok(())
}

pub(crate) fn put_compressor(w: &mut Vec<u8>, p: &CompressorParams) {
    w.extend(p.attack.to_le_bytes());
    w.extend(p.release.to_le_bytes());
    w.extend(p.taw.to_le_bytes());
    w.extend(p.ratio.to_le_bytes());
    w.extend(p.gain.to_le_bytes());
    w.extend([p.ctrl, p.threshold, p.knee, p.linked]);
}

pub(crate) fn get_compressor(rd: &mut Rd) -> Result<CompressorParams, ProtoError> {
    Ok(CompressorParams {
        attack: rd.u32()?,
        release: rd.u32()?,
        taw: rd.u16()?,
        ratio: rd.u16()?,
        gain: rd.u16()?,
        ctrl: rd.u8()?,
        threshold: rd.u8()?,
        knee: rd.u8()?,
        linked: rd.u8()?,
    })
}

fn check_compressor(p: &CompressorParams) -> Result<(), ProtoError> {
    check_range("compressor attack", p.attack as i64, 100, 150_000)?;
    check_range("compressor release", p.release as i64, 1000, 600_000)?;
    // Detector: 65535 = peak, else RMS window × 100 (panel combo).
    if ![65535, 2000, 5000, 10000, 15000, 20000].contains(&p.taw) {
        return Err(malformed(
            "compressor taw",
            format!("{} (65535 = peak, or 2000/5000/10000/15000/20000)", p.taw),
        ));
    }
    check_range("compressor ratio", p.ratio as i64, 100, 2000)?;
    check_range("compressor gain", p.gain as i64, 0, 2400)?;
    check_range("compressor threshold", p.threshold as i64, 0, 80)?;
    check_range("compressor knee", p.knee as i64, 0, 30)?;
    check_range("compressor ctrl", p.ctrl as i64, 0, 0)?;
    check_range("compressor linked", p.linked as i64, 0, 1)
}

/// Write one raw AFX field (two's complement for signed fields).
pub(crate) fn put_field(w: &mut Vec<u8>, f: &AfxField, v: i32) {
    match f.width {
        1 => w.push(v as u8),
        2 => w.extend((v as u16).to_le_bytes()),
        _ => w.extend((v as u32).to_le_bytes()),
    }
}

pub(crate) fn get_field(rd: &mut Rd, f: &AfxField) -> Result<i32, ProtoError> {
    Ok(match (f.width, f.signed) {
        (1, false) => rd.u8()? as i32,
        (1, true) => rd.u8()? as i8 as i32,
        (2, false) => rd.u16()? as i32,
        (2, true) => rd.i16()? as i32,
        (_, false) => rd.u32()? as i32,
        (_, true) => rd.u32()? as i32,
    })
}

pub(crate) fn raw_table(ty: AfxType) -> Result<&'static [AfxField], ProtoError> {
    afx_fields(ty).ok_or_else(|| {
        malformed(
            "afx type",
            format!("{ty:?} has no raw field table (not a raw model on the Orion)"),
        )
    })
}

fn check_on_orion(ty: AfxType) -> Result<(), ProtoError> {
    if ty.on_orion() {
        Ok(())
    } else {
        Err(ProtoError::Refused(format!(
            "{ty:?} is not an Orion AFX model"
        )))
    }
}

fn check_inst(ty: AfxType, inst: u8) -> Result<(), ProtoError> {
    check_range(
        "afx instance",
        inst as i64,
        0,
        afx_max_instances(ty) as i64 - 1,
    )
}

/// A chain must be packed from slot 0 (no holes), on-Orion models only,
/// each instance at most once.
fn check_chain(slots: &[Option<AfxSlot>; AFX_SLOTS]) -> Result<(), ProtoError> {
    let used = slots.iter().take_while(|s| s.is_some()).count();
    if slots[used..].iter().any(|s| s.is_some()) {
        return Err(malformed(
            "afx order",
            "slots must be packed from slot 0".into(),
        ));
    }
    for (i, s) in slots[..used].iter().flatten().enumerate() {
        check_on_orion(s.ty)?;
        check_inst(s.ty, s.inst)?;
        if slots[..i].iter().flatten().any(|o| o == s) {
            return Err(malformed(
                "afx order",
                format!("{:?} inst {} appears twice", s.ty, s.inst),
            ));
        }
    }
    Ok(())
}
