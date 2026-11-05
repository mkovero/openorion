//! Reply and status decoders, plus the encoders the simulator and the
//! round-trip tests need (DESIGN.md 4.2, 4.4). Signatures are the WP0
//! contract; bodies are WP1.

use std::collections::BTreeMap;

use crate::codec::{
    BODY_LEN, Get, HEADER_LEN, ID_AFX_METERS, ID_REPLY, ID_STATUS, REPORT_LEN, Rd, Report, Set,
    get_compressor, get_eq_band, get_field, le32, malformed, mixer_channel_bytes, osc_from_u8,
    osc_to_u8, put_compressor, put_eq_band, put_field, raw_table, reverb_bytes, reverb_from, short,
    slot_bytes, slot_from, source_bytes, source_from, trim_record,
};
use crate::error::ProtoError;
use crate::model::*;
use crate::names::afx_fields;

/// A decoded get reply, one variant per `Get`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GetReply {
    Firmware(Version),
    CompatPassport(Version),
    Passport(Passport),
    Registration(bool),
    Routing(RoutingBank),
    Mixer(MixerId, Mixer),
    /// Instances in reply order. EQ halves carry 8 entries for
    /// inst `half*8 ..`; other types carry `count` entries from inst 0.
    AfxConfigs {
        ty: AfxType,
        first_inst: u8,
        configs: Vec<AfxConfig>,
    },
    AfxOrder([[Option<AfxSlot>; AFX_SLOTS]; AFX_CHAINS]),
    Reverb([Option<Reverb>; 4]),
    MixerLinks([[bool; 16]; 4]),
    AfxLinks([bool; AFX_CHAINS / 2]),
    AfxInstances(Vec<(AfxType, u8)>),
    HdxDelays([[u8; 32]; 2]),
    Trims(Trims),
}

// ---------------------------------------------------------------------------
// Layout constants

/// Instances per get_eq_configs reply (one half).
pub const EQ_HALF: usize = 8;
/// Entries in a non-EQ AFX config reply (`Effect.max_instances`). For
/// PowerGate the device has only 12; entries 12..15 are whatever the
/// firmware pads with.
pub const AFX_CONFIG_ENTRIES: usize = 16;
const PASSPORT_NAME: usize = 20;
const PASSPORT_SERIAL: usize = 16;
const PASSPORT_HW: usize = 8;
const MIXER_ENTRIES: usize = 1 + MIXER_CHANNELS;
const TRIM_RECORD_LEN: usize = 2 + 2 * TRIM_CHANNELS;
const REVERB_SLOT_LEN: usize = 11;
const REVERB_UNUSED: u8 = 0xFF;
/// Status payload bytes `decode_status` needs (through the reverb meters).
pub const STATUS_LEN: usize = 226;
const ST_METER_SRC: usize = 106;
const ST_BANK_A: usize = 110;
const ST_BANK_B: usize = 142;
const ST_MIXER: usize = 174;
const ST_REVERB: usize = 222;

fn need(what: &'static str, body: &[u8], n: usize) -> Result<(), ProtoError> {
    if body.len() < n {
        Err(short(what, n, body.len()))
    } else {
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Defaults

/// Factory config of one AFX instance (enabled). EQ and compressor values
/// are the device's factory state read on the wire; raw models use the panel defaults from `afx_fields`.
/// None for types without a codec (not on the Orion).
pub fn default_afx_config(ty: AfxType) -> Option<AfxConfig> {
    let params = match ty {
        AfxType::Equalizer => {
            let freqs = [30, 200, 1000, 5000, 15000];
            let ftypes = [
                EqFilter::LowShelf,
                EqFilter::Peak,
                EqFilter::Peak,
                EqFilter::Peak,
                EqFilter::HighShelf,
            ];
            let mut bands = [EqBand {
                freq: 0,
                q: 71,
                gain: 0,
                ftype: EqFilter::Peak,
            }; EQ_BANDS];
            for (i, b) in bands.iter_mut().enumerate() {
                b.freq = freqs[i];
                b.ftype = ftypes[i];
            }
            AfxParams::Eq(EqParams { bands })
        }
        AfxType::Compressor => AfxParams::Compressor(CompressorParams {
            attack: 1000,
            release: 10000,
            taw: 0xFFFF,
            ratio: 100,
            gain: 0,
            ctrl: 0,
            threshold: 24,
            knee: 0,
            linked: 0,
        }),
        _ => AfxParams::Raw(AfxRaw {
            ty,
            fields: afx_fields(ty)?.iter().map(|f| f.default).collect(),
        }),
    };
    Some(AfxConfig {
        enabled: true,
        params,
    })
}

fn default_channel() -> MixerChannel {
    MixerChannel {
        level: Atten::ZERO_DB,
        pan: Pan::CENTER,
        mute: false,
        solo: false,
        send: Atten::NEG_INF,
    }
}

fn default_status() -> Status {
    Status {
        current_preset: 0,
        power_on: true,
        muted_bit: false,
        usb_mode: 0,
        device_updated: false,
        clock: ClockFlags::default(),
        sample_rate: None,
        pull_mode: 0,
        clock_source: None,
        oscillator: Osc {
            freq_l: OscFreq::Hz1k,
            freq_r: OscFreq::Hz1k,
            level: OscLevel::DbM12,
            mute_l: true,
            mute_r: true,
        },
        external_clock_hz: 0,
        smux_in: false,
        smux_out: false,
        spdif_src: false,
        brightness: 0,
        adat_channels: 0,
        madi_channels: 0,
        main: VolumeState {
            atten: Atten::NEG_INF,
            mute: false,
        },
        line: VolumeState {
            atten: Atten::NEG_INF,
            mute: false,
        },
        monitor_trim: 0,
        meter_sources: [0; 4],
    }
}

/// A blank state to fill from replies: every bank muted, mixers at 0 dB /
/// centre / send -inf, no reverb, empty AFX. Not a device state by itself.
pub fn empty_state() -> DeviceState {
    let mixer = Mixer {
        master: default_channel(),
        channels: [default_channel(); MIXER_CHANNELS],
    };
    let trim = TrimRecord {
        control: TrimControl::All,
        levels: [TrimLevel::default(); TRIM_CHANNELS],
    };
    DeviceState {
        info: DeviceInfo::default(),
        status: default_status(),
        routing: OutputBank::ALL
            .iter()
            .map(|&b| RoutingBank::all_muted(b))
            .collect(),
        mixers: std::array::from_fn(|_| mixer.clone()),
        mixer_links: [[false; 16]; 4],
        trims: Trims {
            line_out: trim.clone(),
            line_in: trim,
        },
        reverb: [None; 4],
        afx: AfxState::default(),
        hdx_delays: [[0; 32]; 2],
    }
}

// ---------------------------------------------------------------------------
// Replies

fn cstr(b: &[u8]) -> String {
    let end = b.iter().position(|&c| c == 0).unwrap_or(b.len());
    String::from_utf8_lossy(&b[..end]).into_owned()
}

/// Lenient channel decode for replies: values the newtypes reject are
/// clamped (attenuation >96) so one odd field cannot stop a whole read.
/// Pan takes all 6 bits as-is.
fn mixer_channel_lenient(b: &[u8]) -> MixerChannel {
    MixerChannel {
        level: Atten::new(b[0].min(Atten::MAX)).unwrap(),
        pan: Pan::new(b[1] & 63).unwrap(),
        mute: b[1] >> 6 & 1 != 0,
        solo: b[1] >> 7 != 0,
        send: Atten::new(b[2].min(Atten::MAX)).unwrap(),
    }
}

fn decode_afx_entry(ty: AfxType, rd: &mut Rd) -> Result<AfxConfig, ProtoError> {
    match ty {
        AfxType::Equalizer => {
            let mut bands = [EqBand {
                freq: 0,
                q: 0,
                gain: 0,
                ftype: EqFilter::Peak,
            }; EQ_BANDS];
            for b in bands.iter_mut() {
                *b = get_eq_band(rd)?;
            }
            let enabled = rd.u8()? != 0;
            Ok(AfxConfig {
                enabled,
                params: AfxParams::Eq(EqParams { bands }),
            })
        }
        AfxType::Compressor => {
            let enabled = rd.u8()? != 0;
            Ok(AfxConfig {
                enabled,
                params: AfxParams::Compressor(get_compressor(rd)?),
            })
        }
        _ => {
            let table = raw_table(ty)?;
            let enabled = rd.u8()? != 0;
            let fields = table
                .iter()
                .map(|f| get_field(rd, f))
                .collect::<Result<Vec<_>, _>>()?;
            Ok(AfxConfig {
                enabled,
                params: AfxParams::Raw(AfxRaw { ty, fields }),
            })
        }
    }
}

/// Decode the body (bytes after the 16-byte header) of the reply to `get`.
pub fn decode_reply(get: &Get, body: &[u8]) -> Result<GetReply, ProtoError> {
    Ok(match *get {
        Get::Firmware | Get::CompatPassport => {
            need("firmware reply", body, 2)?;
            let v = Version {
                major: body[0],
                minor: body[1],
            };
            if *get == Get::Firmware {
                GetReply::Firmware(v)
            } else {
                GetReply::CompatPassport(v)
            }
        }
        Get::Passport => {
            let n = PASSPORT_NAME + PASSPORT_SERIAL + PASSPORT_HW;
            need("passport reply", body, n + 4)?;
            GetReply::Passport(Passport {
                name: cstr(&body[..PASSPORT_NAME]),
                serial: cstr(&body[PASSPORT_NAME..PASSPORT_NAME + PASSPORT_SERIAL]),
                hw: cstr(&body[PASSPORT_NAME + PASSPORT_SERIAL..n]),
                birth: le32(body, n),
            })
        }
        Get::Registration => {
            need("registration reply", body, 1)?;
            GetReply::Registration(body[0] != 0)
        }
        Get::Routing(bank) => {
            let n = bank.size();
            need("routing reply", body, 1 + 2 * n)?;
            if body[0] != bank.as_u8() {
                return Err(malformed(
                    "routing reply",
                    format!("bank byte {} for bank {}", body[0], bank.as_u8()),
                ));
            }
            let sources = (0..n)
                .map(|i| source_from(body[1 + 2 * i], body[2 + 2 * i]))
                .collect::<Result<Vec<_>, _>>()?;
            GetReply::Routing(RoutingBank { bank, sources })
        }
        Get::Mixer(m) => {
            need("mixer reply", body, 3 * MIXER_ENTRIES)?;
            let ch = |i: usize| mixer_channel_lenient(&body[3 * i..3 * i + 3]);
            GetReply::Mixer(
                m,
                Mixer {
                    master: ch(0),
                    channels: std::array::from_fn(|i| ch(i + 1)),
                },
            )
        }
        Get::AfxConfigs(ty, _) => {
            let (first_inst, n) = if ty == AfxType::Equalizer {
                (Get::ext3(get) as u8 * EQ_HALF as u8, EQ_HALF)
            } else {
                (0, AFX_CONFIG_ENTRIES)
            };
            let mut rd = Rd::new("afx config reply", body);
            let configs = (0..n)
                .map(|_| decode_afx_entry(ty, &mut rd))
                .collect::<Result<Vec<_>, _>>()?;
            GetReply::AfxConfigs {
                ty,
                first_inst,
                configs,
            }
        }
        Get::AfxOrder => {
            need("afx order reply", body, 2 * AFX_CHAINS * AFX_SLOTS)?;
            let mut order = [[None; AFX_SLOTS]; AFX_CHAINS];
            for (c, chain) in order.iter_mut().enumerate() {
                for (s, slot) in chain.iter_mut().enumerate() {
                    let o = 2 * (c * AFX_SLOTS + s);
                    *slot = slot_from(body[o], body[o + 1])?;
                }
            }
            GetReply::AfxOrder(order)
        }
        Get::Reverb => {
            need("reverb reply", body, 4 * REVERB_SLOT_LEN)?;
            let mut slots = [None; 4];
            for (i, s) in slots.iter_mut().enumerate() {
                let b = &body[i * REVERB_SLOT_LEN..(i + 1) * REVERB_SLOT_LEN];
                if b[0] != REVERB_UNUSED {
                    *s = Some(reverb_from(b)?);
                }
            }
            GetReply::Reverb(slots)
        }
        Get::MixerLinks => {
            need("mixer links reply", body, 64)?;
            GetReply::MixerLinks(std::array::from_fn(|m| {
                std::array::from_fn(|p| body[m * 16 + p] != 0)
            }))
        }
        Get::AfxLinks => {
            need("afx links reply", body, AFX_CHAINS / 2)?;
            GetReply::AfxLinks(std::array::from_fn(|p| body[p] != 0))
        }
        Get::AfxInstances => {
            // Pairs until type 0. Types missing from the enum (10, 40, 49
            // on the wire, count 0) are skipped.
            let v = body
                .chunks_exact(2)
                .take_while(|p| p[0] != 0)
                .filter_map(|p| AfxType::try_from(p[0]).ok().map(|t| (t, p[1])))
                .collect();
            GetReply::AfxInstances(v)
        }
        Get::HdxDelays => {
            need("hdx delays reply", body, 64)?;
            GetReply::HdxDelays(std::array::from_fn(|i| {
                body[32 * i..32 * i + 32].try_into().unwrap()
            }))
        }
        Get::Trims => {
            need("trims reply", body, 2 * TRIM_RECORD_LEN)?;
            let rec = |i: usize, want: TrimId| -> Result<TrimRecord, ProtoError> {
                let b = &body[i * TRIM_RECORD_LEN..(i + 1) * TRIM_RECORD_LEN];
                if b[0] != want.as_u8() {
                    return Err(malformed(
                        "trims reply",
                        format!("record {i} has trim id {}, want {}", b[0], want.as_u8()),
                    ));
                }
                trim_record(b[1], &b[2..])
            };
            GetReply::Trims(Trims {
                line_out: rec(0, TrimId::LineOut)?,
                line_in: rec(1, TrimId::LineIn)?,
            })
        }
    })
}

// ---------------------------------------------------------------------------
// Status and meters

/// Decode a 0x73 payload (after the header) into status and meters.
pub fn decode_status(payload: &[u8]) -> Result<(Status, Meters), ProtoError> {
    need("status report", payload, STATUS_LEN)?;
    let p = payload;
    let vol = |b: u8| VolumeState {
        atten: Atten::new((b & 127).min(Atten::MAX)).unwrap(),
        mute: b >> 7 != 0,
    };
    let f = p[1];
    let status = Status {
        current_preset: p[0] & 7,
        power_on: p[0] >> 3 & 1 != 0,
        muted_bit: p[0] >> 4 & 1 != 0,
        usb_mode: p[0] >> 5 & 3,
        device_updated: p[0] >> 7 != 0,
        clock: ClockFlags {
            oven: f & 0x01 != 0,
            locked: f & 0x02 != 0,
            atomic: f & 0x04 != 0,
            usb: f & 0x08 != 0,
            madi: f & 0x10 != 0,
            adat: f & 0x20 != 0,
            spdif: f & 0x40 != 0,
            dlink: f & 0x80 != 0,
        },
        sample_rate: SampleRate::try_from(p[2] & 15).ok(),
        pull_mode: p[2] >> 4,
        clock_source: ClockSource::try_from(p[3]).ok(),
        oscillator: osc_from_u8(p[4])?,
        external_clock_hz: (p[5] as u32) << 16 | (p[6] as u32) << 8 | p[7] as u32,
        smux_in: p[8] & 1 != 0,
        smux_out: p[8] & 2 != 0,
        spdif_src: p[8] & 4 != 0,
        brightness: p[9],
        adat_channels: p[10],
        madi_channels: p[11],
        main: vol(p[12]),
        line: vol(p[13]),
        monitor_trim: p[14],
        meter_sources: p[ST_METER_SRC..ST_METER_SRC + 4].try_into().unwrap(),
    };
    let arr32 = |o: usize| -> [u8; 32] { p[o..o + 32].try_into().unwrap() };
    let meters = Meters {
        bank_a: arr32(ST_BANK_A),
        bank_b: arr32(ST_BANK_B),
        mixer: arr32(ST_MIXER),
        reverb_in: [p[ST_REVERB], p[ST_REVERB + 1]],
        reverb_out: [p[ST_REVERB + 2], p[ST_REVERB + 3]],
    };
    Ok((status, meters))
}

/// Decode a 0x83 payload (after the header).
pub fn decode_afx_meters(payload: &[u8]) -> Result<AfxMeters, ProtoError> {
    let n = AFX_CHAINS * AFX_SLOTS;
    need("afx meters", payload, 2 * n)?;
    let grid = |o: usize| -> [[u8; AFX_SLOTS]; AFX_CHAINS] {
        std::array::from_fn(|c| {
            payload[o + c * AFX_SLOTS..o + (c + 1) * AFX_SLOTS]
                .try_into()
                .unwrap()
        })
    };
    Ok(AfxMeters {
        peak: grid(0),
        gain_reduction: grid(n),
    })
}

fn full_report(id: u32, len: u32, ext2: u32, ext3: u32, body: &[u8]) -> [u8; REPORT_LEN] {
    *Report::build(id, len, ext2, ext3, body)
        .expect("body fits")
        .as_bytes()
}

/// Encode a full 0x73 report (simulator).
pub fn encode_status(status: &Status, meters: &Meters) -> [u8; REPORT_LEN] {
    let s = status;
    let mut p = [0u8; BODY_LEN];
    p[0] = s.current_preset & 7
        | (s.power_on as u8) << 3
        | (s.muted_bit as u8) << 4
        | (s.usb_mode & 3) << 5
        | (s.device_updated as u8) << 7;
    let c = &s.clock;
    p[1] = c.oven as u8
        | (c.locked as u8) << 1
        | (c.atomic as u8) << 2
        | (c.usb as u8) << 3
        | (c.madi as u8) << 4
        | (c.adat as u8) << 5
        | (c.spdif as u8) << 6
        | (c.dlink as u8) << 7;
    p[2] = s.sample_rate.map_or(15, |r| r.as_u8()) | (s.pull_mode & 15) << 4;
    p[3] = s.clock_source.map_or(0xFF, |c| c.as_u8());
    p[4] = osc_to_u8(&s.oscillator);
    p[5] = (s.external_clock_hz >> 16) as u8;
    p[6] = (s.external_clock_hz >> 8) as u8;
    p[7] = s.external_clock_hz as u8;
    p[8] = s.smux_in as u8 | (s.smux_out as u8) << 1 | (s.spdif_src as u8) << 2;
    p[9] = s.brightness;
    p[10] = s.adat_channels;
    p[11] = s.madi_channels;
    p[12] = s.main.atten.get() | (s.main.mute as u8) << 7;
    p[13] = s.line.atten.get() | (s.line.mute as u8) << 7;
    p[14] = s.monitor_trim;
    p[ST_METER_SRC..ST_METER_SRC + 4].copy_from_slice(&s.meter_sources);
    p[ST_BANK_A..ST_BANK_A + 32].copy_from_slice(&meters.bank_a);
    p[ST_BANK_B..ST_BANK_B + 32].copy_from_slice(&meters.bank_b);
    p[ST_MIXER..ST_MIXER + 32].copy_from_slice(&meters.mixer);
    p[ST_REVERB..ST_REVERB + 2].copy_from_slice(&meters.reverb_in);
    p[ST_REVERB + 2..ST_REVERB + 4].copy_from_slice(&meters.reverb_out);
    full_report(ID_STATUS, 0, 0, 0, &p)
}

/// Encode a full 0x83 report (simulator).
pub fn encode_afx_meters(meters: &AfxMeters) -> [u8; REPORT_LEN] {
    let mut p = Vec::with_capacity(2 * AFX_CHAINS * AFX_SLOTS);
    for c in &meters.peak {
        p.extend_from_slice(c);
    }
    for c in &meters.gain_reduction {
        p.extend_from_slice(c);
    }
    full_report(ID_AFX_METERS, 0, 0, 0, &p)
}

// ---------------------------------------------------------------------------
// Reply encoding (simulator)

fn put_str(w: &mut Vec<u8>, what: &'static str, s: &str, len: usize) -> Result<(), ProtoError> {
    let b = s.as_bytes();
    if b.len() > len {
        return Err(ProtoError::OutOfRange {
            what,
            value: b.len() as i64,
            min: 0,
            max: len as i64,
        });
    }
    w.extend_from_slice(b);
    w.resize(w.len() + len - b.len(), 0);
    Ok(())
}

fn encode_afx_entry(ty: AfxType, c: &AfxConfig, w: &mut Vec<u8>) -> Result<(), ProtoError> {
    match (&c.params, ty) {
        (AfxParams::Eq(p), AfxType::Equalizer) => {
            for b in &p.bands {
                put_eq_band(w, b);
            }
            w.push(c.enabled as u8);
        }
        (AfxParams::Compressor(p), AfxType::Compressor) => {
            w.push(c.enabled as u8);
            put_compressor(w, p);
        }
        (AfxParams::Raw(r), _) if r.ty == ty => {
            let table = raw_table(ty)?;
            w.push(c.enabled as u8);
            for (i, f) in table.iter().enumerate() {
                // A set-only field list (no get-only tail) uses defaults.
                let v = r.fields.get(i).copied().unwrap_or(f.default);
                put_field(w, f, v);
            }
        }
        _ => {
            return Err(malformed(
                "afx config",
                format!("params do not match type {ty:?}"),
            ));
        }
    }
    Ok(())
}

fn reply_body(get: &Get, st: &DeviceState) -> Result<Vec<u8>, ProtoError> {
    let mut w = Vec::with_capacity(BODY_LEN);
    match *get {
        Get::Firmware | Get::CompatPassport => {
            w.extend([st.info.firmware.major, st.info.firmware.minor]);
        }
        Get::Passport => {
            let p = &st.info.passport;
            put_str(&mut w, "passport name", &p.name, PASSPORT_NAME)?;
            put_str(&mut w, "passport serial", &p.serial, PASSPORT_SERIAL)?;
            put_str(&mut w, "passport hw", &p.hw, PASSPORT_HW)?;
            w.extend(p.birth.to_le_bytes());
        }
        Get::Registration => w.push(st.info.registered as u8),
        Get::Routing(bank) => {
            w.push(bank.as_u8());
            if let Some(rb) = st.routing.iter().find(|r| r.bank == bank) {
                for s in rb.sources.iter().take(64) {
                    w.extend(source_bytes(*s)?);
                }
            } else {
                for _ in 0..bank.size() {
                    w.extend(source_bytes(Source::Mute)?);
                }
            }
            w.resize(1 + 2 * 64, 0);
        }
        Get::Mixer(m) => {
            let mx = &st.mixers[m.get() as usize];
            w.extend(mixer_channel_bytes(&mx.master));
            for c in &mx.channels {
                w.extend(mixer_channel_bytes(c));
            }
        }
        Get::AfxConfigs(ty, _) => {
            let (first, n) = if ty == AfxType::Equalizer {
                (Get::ext3(get) as usize * EQ_HALF, EQ_HALF)
            } else {
                (0, AFX_CONFIG_ENTRIES)
            };
            let have = st.afx.configs.get(&ty);
            let dflt = default_afx_config(ty);
            for i in first..first + n {
                let c = have
                    .and_then(|v| v.get(i))
                    .or(dflt.as_ref())
                    .ok_or_else(|| malformed("afx config", format!("no codec for {ty:?}")))?;
                encode_afx_entry(ty, c, &mut w)?;
            }
        }
        Get::AfxOrder => {
            for chain in &st.afx.order {
                for s in chain {
                    w.extend(slot_bytes(*s));
                }
            }
        }
        Get::Reverb => {
            for slot in &st.reverb {
                match slot {
                    Some(r) => w.extend(reverb_bytes(r)),
                    None => {
                        w.push(REVERB_UNUSED);
                        w.extend([0; REVERB_SLOT_LEN - 1]);
                    }
                }
            }
        }
        Get::MixerLinks => {
            for m in &st.mixer_links {
                w.extend(m.iter().map(|&l| l as u8));
            }
        }
        Get::AfxLinks => w.extend(st.afx.links.iter().map(|&l| l as u8)),
        Get::AfxInstances => {
            for (t, n) in &st.afx.instances {
                w.extend([t.as_u8(), *n]);
            }
        }
        Get::HdxDelays => {
            for d in &st.hdx_delays {
                w.extend_from_slice(d);
            }
        }
        Get::Trims => {
            for (id, rec) in [
                (TrimId::LineOut, &st.trims.line_out),
                (TrimId::LineIn, &st.trims.line_in),
            ] {
                w.extend([id.as_u8(), rec.control.as_u8()]);
                for l in &rec.levels {
                    w.extend([l.whole, l.fract]);
                }
            }
        }
    }
    if w.len() > BODY_LEN {
        return Err(ProtoError::TooLong(w.len()));
    }
    Ok(w)
}

/// Encode the full 0x75 report answering `get` from `state` (simulator).
pub fn encode_reply(get: &Get, state: &DeviceState) -> Result<[u8; REPORT_LEN], ProtoError> {
    let body = reply_body(get, state)?;
    Ok(full_report(
        ID_REPLY,
        HEADER_LEN as u32,
        get.ext2(),
        get.ext3(),
        &body,
    ))
}

// ---------------------------------------------------------------------------
// State updates

fn routing_mut(state: &mut DeviceState, bank: OutputBank) -> &mut RoutingBank {
    let i = match state.routing.iter().position(|r| r.bank == bank) {
        Some(i) => i,
        None => {
            state.routing.push(RoutingBank::all_muted(bank));
            state.routing.sort_by_key(|r| r.bank.as_u8());
            state.routing.iter().position(|r| r.bank == bank).unwrap()
        }
    };
    &mut state.routing[i]
}

/// The config slot of `(ty, inst)`. A missing list is created with
/// `AFX_CONFIG_ENTRIES` factory configs, the size a get reply fills.
fn afx_config_mut(
    state: &mut DeviceState,
    ty: AfxType,
    inst: u8,
) -> Result<&mut AfxConfig, ProtoError> {
    let dflt = default_afx_config(ty)
        .ok_or_else(|| malformed("afx config", format!("no codec for {ty:?}")))?;
    let v = state.afx.configs.entry(ty).or_default();
    let want = (inst as usize + 1).max(AFX_CONFIG_ENTRIES);
    if v.len() < want {
        v.resize(want, dflt);
    }
    Ok(&mut v[inst as usize])
}

/// Store a decoded reply into the state.
pub fn apply_reply(state: &mut DeviceState, reply: GetReply) {
    match reply {
        GetReply::Firmware(v) => state.info.firmware = v,
        GetReply::CompatPassport(_) => {}
        GetReply::Passport(p) => state.info.passport = p,
        GetReply::Registration(r) => state.info.registered = r,
        GetReply::Routing(rb) => {
            let bank = rb.bank;
            *routing_mut(state, bank) = rb;
        }
        GetReply::Mixer(m, mx) => state.mixers[m.get() as usize] = mx,
        GetReply::AfxConfigs {
            ty,
            first_inst,
            configs,
        } => {
            let Some(dflt) = default_afx_config(ty) else {
                return;
            };
            let v = state.afx.configs.entry(ty).or_default();
            // Lists are kept at least AFX_CONFIG_ENTRIES long, so an EQ
            // half and a full reply leave the same shape.
            let end = (first_inst as usize + configs.len()).max(AFX_CONFIG_ENTRIES);
            if v.len() < end {
                v.resize(end, dflt);
            }
            for (i, c) in configs.into_iter().enumerate() {
                v[first_inst as usize + i] = c;
            }
        }
        GetReply::AfxOrder(o) => state.afx.order = o,
        GetReply::Reverb(r) => state.reverb = r,
        GetReply::MixerLinks(l) => state.mixer_links = l,
        GetReply::AfxLinks(l) => state.afx.links = l,
        GetReply::AfxInstances(v) => state.afx.instances = v.into_iter().collect(),
        GetReply::HdxDelays(d) => state.hdx_delays = d,
        GetReply::Trims(t) => state.trims = t,
    }
}

/// Apply a set to the state the way the device does (optimistic cache in
/// the core, ground truth in the simulator). Includes the side effects in
/// protocol-spec.md sec. 5 "Behaviours" that are visible in state: any set
/// clears `current_preset`; set_stereo_link changes only the flag.
/// Preset save/recall are left to the caller (they need the slot store).
///
/// Also mirrored: removing an instance from its chain (set_afx_order)
/// sets its `enabled` to false. Preset save/recall leave the state
/// untouched, `current_preset` included. A set that fails to encode
/// (out of range) is refused without changing anything.
pub fn apply_set(state: &mut DeviceState, set: &Set) -> Result<(), ProtoError> {
    set.payload()?;
    if matches!(set, Set::PresetSave(_) | Set::PresetRecall(_)) {
        return Ok(());
    }
    let st = &mut state.status;
    match set {
        Set::PresetSave(_) | Set::PresetRecall(_) => {}
        Set::Power(on) => st.power_on = *on,
        Set::SampleRate(r) => st.sample_rate = Some(*r),
        Set::ClockSource(c) => {
            // The lock flag belongs to the old source; the next status
            // from the device tells whether the new one locks.
            if st.clock_source != Some(*c) {
                st.clock.locked = false;
            }
            st.clock_source = Some(*c);
        }
        Set::Volume(VolumeId::Main, a) => st.main.atten = *a,
        Set::Volume(VolumeId::Line, a) => st.line.atten = *a,
        Set::Mute(VolumeId::Main, m) => st.main.mute = *m,
        Set::Mute(VolumeId::Line, m) => st.line.mute = *m,
        Set::Oscillator(o) => st.oscillator = *o,
        Set::Brightness(b) => st.brightness = b.get(),
        Set::Trim(TrimId::Monitor, rec) => st.monitor_trim = rec.levels[0].whole,
        Set::Trim(TrimId::LineOut, rec) => state.trims.line_out = rec.clone(),
        Set::Trim(TrimId::LineIn, rec) => state.trims.line_in = rec.clone(),
        Set::SpdifSrc(on) => st.spdif_src = *on,
        Set::Smux(SmuxId::MadiIn, on) => st.smux_in = *on,
        Set::Smux(SmuxId::MadiOut, on) => st.smux_out = *on,
        Set::MeterSource(bank, src) => st.meter_sources[bank.as_u8() as usize] = *src,
        Set::MixerChannel(m, ch, c) => {
            let mx = &mut state.mixers[m.get() as usize];
            match *ch {
                0 => mx.master = *c,
                n => mx.channels[n as usize - 1] = *c,
            }
        }
        Set::Routing(bank, sources) => {
            // The wire pads with (0,0) = LINE IN 1.
            let pad = Source::Input {
                group: SourceGroup::LineIn,
                chan: 0,
            };
            let mut v = sources.clone();
            v.resize(bank.size(), pad);
            routing_mut(state, *bank).sources = v;
        }
        Set::StereoLink(LinkTarget::Mixer { mixer, pair }, l) => {
            state.mixer_links[mixer.get() as usize][*pair as usize] = *l;
        }
        Set::StereoLink(LinkTarget::Afx { pair }, l) => state.afx.links[*pair as usize] = *l,
        Set::AfxEq { inst, band, value } => {
            let c = afx_config_mut(state, AfxType::Equalizer, *inst)?;
            match &mut c.params {
                AfxParams::Eq(p) => p.bands[*band as usize] = *value,
                _ => return Err(malformed("afx config", "EQ entry is not EQ".into())),
            }
        }
        Set::AfxCompressor { inst, params } => {
            afx_config_mut(state, AfxType::Compressor, *inst)?.params =
                AfxParams::Compressor(*params);
        }
        Set::AfxRaw { inst, raw } => {
            let table = raw_table(raw.ty)?;
            let c = afx_config_mut(state, raw.ty, *inst)?;
            match &mut c.params {
                AfxParams::Raw(r) if r.ty == raw.ty => {
                    r.fields.resize(table.len(), 0);
                    for (i, f) in table.iter().enumerate() {
                        if f.in_set {
                            r.fields[i] = raw.fields[i];
                        }
                    }
                }
                _ => return Err(malformed("afx config", "raw entry type mismatch".into())),
            }
        }
        Set::AfxOrder { chain, slots } => {
            let old = state.afx.order[*chain as usize];
            state.afx.order[*chain as usize] = *slots;
            // The device disables a removed instance (protocol-spec.md
            // sec. 5). Fill in factory defaults if its config was never read.
            for s in old.iter().flatten() {
                if !slots.contains(&Some(*s)) {
                    afx_config_mut(state, s.ty, s.inst)?.enabled = false;
                }
            }
        }
        Set::AfxBypass { ty, inst, enabled } => {
            afx_config_mut(state, *ty, *inst)?.enabled = *enabled;
        }
        Set::Reverb(r) => {
            let i = state
                .reverb
                .iter()
                .position(|s| s.is_some_and(|s| s.mixer == r.mixer))
                .unwrap_or(r.mixer.get() as usize);
            state.reverb[i] = Some(*r);
        }
    }
    state.status.current_preset = 0;
    Ok(())
}

/// Build a `DeviceState` from a read-all dump in the JSON
/// format (`fixtures/dump1.json`): `replies` keyed `"ext2/ext3"` (hex
/// reports) plus `cyclic` `0x73`. The input is the parsed hex map, so this
/// crate stays free of serde_json.
///
/// Empty reports (no reply) and unknown keys are skipped; missing parts
/// keep `empty_state()` values. An empty `status_report` keeps the
/// default status.
pub fn state_from_dump(
    replies: &BTreeMap<(u32, u32), Vec<u8>>,
    status_report: &[u8],
) -> Result<DeviceState, ProtoError> {
    let mut state = empty_state();
    for (&(e2, e3), rep) in replies {
        if rep.is_empty() {
            continue;
        }
        let Some(get) = Get::from_ext(e2, e3) else {
            continue;
        };
        need("dump reply", rep, HEADER_LEN)?;
        if le32(rep, 0) != ID_REPLY {
            return Err(malformed(
                "dump reply",
                format!("{e2}/{e3}: id {:#x}", le32(rep, 0)),
            ));
        }
        let reply = decode_reply(&get, &rep[HEADER_LEN..])?;
        apply_reply(&mut state, reply);
    }
    if !status_report.is_empty() {
        need("dump status", status_report, HEADER_LEN)?;
        let (s, _) = decode_status(&status_report[HEADER_LEN..])?;
        state.status = s;
    }
    Ok(state)
}
