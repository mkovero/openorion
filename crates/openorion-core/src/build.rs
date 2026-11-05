//! Semantic ops → full set messages, built from the cached state
//! (DESIGN.md 6.4), plus the per-set bookkeeping the session needs:
//! coalescing key, covering read-back, and JSON paths for events.

use openorion_ipc::Op;
use openorion_proto::names::afx_fields;
use openorion_proto::*;
use serde_json::Value;

use crate::Error;

/// Build the sets an op needs, from `state`. Every set carries all fields of
/// its record, taken from `state` where the op leaves them open. Errors when
/// `state` lacks the record (not synced) or a value is out of range. Nothing
/// is sent or changed here.
///
/// `Op::AfxChain` also returns a bypass `enabled = 1` for every instance it
/// removes from the chain (the device disables removed instances; spec
/// sec. 5 "Behaviours"). `Op::Resync` builds nothing.
pub fn build_sets(state: &DeviceState, op: &Op) -> Result<Vec<Set>, Error> {
    let sets = match op {
        Op::Resync | Op::Recheck | Op::Reset { .. } => vec![],
        Op::Power { on } => vec![Set::Power(*on)],
        Op::SampleRate { rate } => vec![Set::SampleRate(*rate)],
        Op::ClockSource { source } => vec![Set::ClockSource(*source)],
        Op::Volume { id, atten } => vec![Set::Volume(*id, *atten)],
        Op::Mute { id, mute } => vec![Set::Mute(*id, *mute)],
        Op::PresetSave { slot } => vec![Set::PresetSave(*slot)],
        Op::PresetRecall { slot } => vec![Set::PresetRecall(*slot)],
        Op::Oscillator { osc } => vec![Set::Oscillator(*osc)],
        Op::Brightness { value } => vec![Set::Brightness(*value)],
        Op::SpdifSrc { on } => vec![Set::SpdifSrc(*on)],
        Op::Smux { id, on } => vec![Set::Smux(*id, *on)],
        Op::MeterSource { bank, src } => vec![Set::MeterSource(*bank, *src)],
        Op::StereoLink { target, linked } => vec![Set::StereoLink(*target, *linked)],
        Op::Reverb { params } => vec![Set::Reverb(*params)],
        Op::AfxCompressor { inst, params } => vec![Set::AfxCompressor {
            inst: *inst,
            params: *params,
        }],
        Op::AfxBypass { ty, inst, enabled } => vec![Set::AfxBypass {
            ty: *ty,
            inst: *inst,
            enabled: *enabled,
        }],
        Op::Trim { id, ch, level } => {
            let mut rec = trim_record(state, *id)?.clone();
            match ch {
                Some(c) => {
                    let l = rec.levels.get_mut(*c as usize).ok_or_else(|| {
                        Error::Invalid(format!("trim channel {c} (0-based, max 31)"))
                    })?;
                    *l = *level;
                }
                None => rec.levels = [*level; TRIM_CHANNELS],
            }
            vec![Set::Trim(*id, rec)]
        }
        Op::TrimControl { id, control } => {
            let mut rec = trim_record(state, *id)?.clone();
            rec.control = *control;
            vec![Set::Trim(*id, rec)]
        }
        Op::Mixer {
            mixer,
            ch,
            level,
            pan,
            mute,
            solo,
            send,
        } => {
            let mx = &state.mixers[mixer.get() as usize];
            let mut c = match *ch {
                0 => mx.master,
                n if (n as usize) <= MIXER_CHANNELS => mx.channels[n as usize - 1],
                n => {
                    return Err(Error::Invalid(format!(
                        "mixer channel {n} (0 = master, 1..=32)"
                    )));
                }
            };
            // The device keeps the master at pan 32 / send 96 whatever is
            // written (hardware, 2026-10-27); refuse instead of drifting.
            if *ch == 0 && (pan.is_some_and(|p| p != c.pan) || send.is_some_and(|s| s != c.send)) {
                return Err(Error::Invalid(
                    "the mixer master has no pan or reverb send".into(),
                ));
            }
            if let Some(v) = level {
                c.level = *v;
            }
            if let Some(v) = pan {
                c.pan = *v;
            }
            if let Some(v) = mute {
                c.mute = *v;
            }
            if let Some(v) = solo {
                c.solo = *v;
            }
            if let Some(v) = send {
                c.send = *v;
            }
            vec![Set::MixerChannel(*mixer, *ch, c)]
        }
        Op::Route { bank, ch, sources } => {
            let rb = state
                .routing
                .iter()
                .find(|r| r.bank == *bank)
                .ok_or_else(|| Error::NotSynced(format!("routing bank {bank:?}")))?;
            let mut v = rb.sources.clone();
            if v.len() != bank.size() {
                return Err(Error::NotSynced(format!(
                    "routing bank {bank:?} incomplete"
                )));
            }
            let start = *ch as usize;
            let end = start + sources.len();
            if end > bank.size() {
                return Err(Error::Invalid(format!(
                    "routing {bank:?}: outputs {start}..{end} exceed the bank size {}",
                    bank.size()
                )));
            }
            v[start..end].copy_from_slice(sources);
            vec![Set::Routing(*bank, v)]
        }
        Op::AfxEqBand {
            inst,
            band,
            freq,
            q,
            gain,
            ftype,
        } => {
            let cfg = afx_config(state, AfxType::Equalizer, *inst)?;
            let AfxParams::Eq(p) = &cfg.params else {
                return Err(Error::NotSynced(format!("EQ {inst} config")));
            };
            let mut b = *p
                .bands
                .get(*band as usize)
                .ok_or_else(|| Error::Invalid(format!("EQ band {band} (0..=4)")))?;
            if let Some(v) = freq {
                b.freq = *v;
            }
            if let Some(v) = q {
                b.q = *v;
            }
            if let Some(v) = gain {
                b.gain = *v;
            }
            if let Some(v) = ftype {
                b.ftype = *v;
            }
            vec![Set::AfxEq {
                inst: *inst,
                band: *band,
                value: b,
            }]
        }
        Op::AfxRaw { ty, inst, fields } => {
            let table = afx_fields(*ty)
                .ok_or_else(|| Error::Invalid(format!("{ty:?} is not a raw AFX model")))?;
            if fields.len() > table.len() {
                return Err(Error::Invalid(format!(
                    "{ty:?}: {} fields, the model has {}",
                    fields.len(),
                    table.len()
                )));
            }
            let cfg = afx_config(state, *ty, *inst)?;
            let AfxParams::Raw(r) = &cfg.params else {
                return Err(Error::NotSynced(format!("{ty:?} {inst} config")));
            };
            if r.ty != *ty || r.fields.len() != table.len() {
                return Err(Error::NotSynced(format!("{ty:?} {inst} config")));
            }
            let mut v = r.fields.clone();
            for (i, f) in fields.iter().enumerate() {
                if let Some(x) = f {
                    if !table[i].in_set {
                        return Err(Error::Invalid(format!(
                            "{ty:?} field {} cannot be set",
                            table[i].name
                        )));
                    }
                    v[i] = *x;
                }
            }
            vec![Set::AfxRaw {
                inst: *inst,
                raw: AfxRaw { ty: *ty, fields: v },
            }]
        }
        Op::AfxChain { chain, slots } => {
            let c = *chain as usize;
            if c >= AFX_CHAINS {
                return Err(Error::Invalid(format!("AFX chain {chain} (0..=15)")));
            }
            if slots.len() > AFX_SLOTS {
                return Err(Error::Invalid(format!(
                    "AFX chain {chain}: {} slots, max {AFX_SLOTS}",
                    slots.len()
                )));
            }
            let mut arr = [None; AFX_SLOTS];
            for (d, s) in arr.iter_mut().zip(slots) {
                *d = Some(*s);
            }
            let mut v = vec![Set::AfxOrder {
                chain: *chain,
                slots: arr,
            }];
            // Removal disables the instance on the device; re-enable it
            // unless it was already bypassed, so its bypass state survives.
            for s in state.afx.order[c].iter().flatten() {
                let was_enabled = state
                    .afx
                    .configs
                    .get(&s.ty)
                    .and_then(|v| v.get(s.inst as usize))
                    .is_none_or(|cfg| cfg.enabled);
                if !slots.contains(s) && was_enabled {
                    v.push(Set::AfxBypass {
                        ty: s.ty,
                        inst: s.inst,
                        enabled: true,
                    });
                }
            }
            v
        }
    };
    for s in &sets {
        s.payload()?;
    }
    Ok(sets)
}

fn monitor_trim() -> Error {
    Error::Unsupported("the monitor trim set is unconfirmed on the wire (O3)".into())
}

fn trim_record(state: &DeviceState, id: TrimId) -> Result<&TrimRecord, Error> {
    match id {
        TrimId::LineOut => Ok(&state.trims.line_out),
        TrimId::LineIn => Ok(&state.trims.line_in),
        TrimId::Monitor => Err(monitor_trim()),
    }
}

fn afx_config(state: &DeviceState, ty: AfxType, inst: u8) -> Result<&AfxConfig, Error> {
    state
        .afx
        .configs
        .get(&ty)
        .and_then(|v| v.get(inst as usize))
        .ok_or_else(|| Error::NotSynced(format!("{ty:?} {inst} config not read")))
}

// ---------------------------------------------------------------------------
// Bookkeeping

/// The target a set writes: coalescing and verify are per key.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum Key {
    Power,
    SampleRate,
    ClockSource,
    Volume(VolumeId),
    Mute(VolumeId),
    Oscillator,
    Brightness,
    Trim(TrimId),
    SpdifSrc,
    Smux(SmuxId),
    MeterSource(MeterBank),
    Mixer(MixerId, u8),
    Routing(OutputBank),
    Link(LinkTarget),
    AfxEq(u8, u8),
    AfxCompressor(u8),
    AfxRaw(AfxType, u8),
    AfxOrder(u8),
    AfxBypass(AfxType, u8),
    Reverb(MixerId),
}

/// None for preset save/recall (not keyed, never coalesced or verified).
pub(crate) fn key(set: &Set) -> Option<Key> {
    Some(match set {
        Set::Power(_) => Key::Power,
        Set::SampleRate(_) => Key::SampleRate,
        Set::ClockSource(_) => Key::ClockSource,
        Set::Volume(id, _) => Key::Volume(*id),
        Set::Mute(id, _) => Key::Mute(*id),
        Set::PresetSave(_) | Set::PresetRecall(_) => return None,
        Set::Oscillator(_) => Key::Oscillator,
        Set::Brightness(_) => Key::Brightness,
        Set::Trim(id, _) => Key::Trim(*id),
        Set::SpdifSrc(_) => Key::SpdifSrc,
        Set::Smux(id, _) => Key::Smux(*id),
        Set::MeterSource(b, _) => Key::MeterSource(*b),
        Set::MixerChannel(m, ch, _) => Key::Mixer(*m, *ch),
        Set::Routing(b, _) => Key::Routing(*b),
        Set::StereoLink(t, _) => Key::Link(*t),
        Set::AfxEq { inst, band, .. } => Key::AfxEq(*inst, *band),
        Set::AfxCompressor { inst, .. } => Key::AfxCompressor(*inst),
        Set::AfxRaw { inst, raw } => Key::AfxRaw(raw.ty, *inst),
        Set::AfxOrder { chain, .. } => Key::AfxOrder(*chain),
        Set::AfxBypass { ty, inst, .. } => Key::AfxBypass(*ty, *inst),
        Set::Reverb(r) => Key::Reverb(r.mixer),
    })
}

/// Continuous controls (faders, pans, knobs): at most one send per key per
/// coalescing interval, latest wins. Others go out as soon as the queue
/// reaches them (still latest-wins while queued).
pub(crate) fn continuous(k: Key) -> bool {
    matches!(
        k,
        Key::Mixer(..)
            | Key::Volume(_)
            | Key::Trim(_)
            | Key::Reverb(_)
            | Key::AfxEq(..)
            | Key::AfxCompressor(_)
            | Key::AfxRaw(..)
            | Key::Brightness
    )
}

/// How a set is confirmed: by the status stream or by a get.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Cover {
    Status,
    Get(Get),
}

pub(crate) fn cover(set: &Set) -> Option<Cover> {
    Some(match set {
        Set::Power(_)
        | Set::SampleRate(_)
        | Set::ClockSource(_)
        | Set::Volume(..)
        | Set::Mute(..)
        | Set::Oscillator(_)
        | Set::Brightness(_)
        | Set::SpdifSrc(_)
        | Set::Smux(..)
        | Set::MeterSource(..)
        | Set::Trim(TrimId::Monitor, _) => Cover::Status,
        Set::PresetSave(_) | Set::PresetRecall(_) => return None,
        Set::Trim(..) => Cover::Get(Get::Trims),
        Set::MixerChannel(m, ..) => Cover::Get(Get::Mixer(*m)),
        Set::Routing(b, _) => Cover::Get(Get::Routing(*b)),
        Set::StereoLink(LinkTarget::Mixer { .. }, _) => Cover::Get(Get::MixerLinks),
        Set::StereoLink(LinkTarget::Afx { .. }, _) => Cover::Get(Get::AfxLinks),
        Set::AfxEq { inst, .. } => Cover::Get(afx_get(AfxType::Equalizer, *inst)),
        Set::AfxCompressor { inst, .. } => Cover::Get(afx_get(AfxType::Compressor, *inst)),
        Set::AfxRaw { inst, raw } => Cover::Get(afx_get(raw.ty, *inst)),
        Set::AfxBypass { ty, inst, .. } => Cover::Get(afx_get(*ty, *inst)),
        Set::AfxOrder { .. } => Cover::Get(Get::AfxOrder),
        Set::Reverb(_) => Cover::Get(Get::Reverb),
    })
}

/// The config get holding `(ty, inst)`, normalised as in `Get::from_ext`.
pub(crate) fn afx_get(ty: AfxType, inst: u8) -> Get {
    if ty == AfxType::Equalizer {
        Get::AfxConfigs(ty, (inst as usize / openorion_proto::decode::EQ_HALF) as u8)
    } else {
        Get::AfxConfigs(ty, 0)
    }
}

fn to_value<T: serde::Serialize>(v: &T) -> Value {
    serde_json::to_value(v).unwrap_or(Value::Null)
}

fn routing_index(state: &DeviceState, b: OutputBank) -> Option<usize> {
    state.routing.iter().position(|r| r.bank == b)
}

/// JSON pointer into `DeviceState` of what `set` changes, and its current
/// value in `state`. None for preset save/recall.
pub fn set_path(set: &Set, state: &DeviceState) -> Option<(String, Value)> {
    Some(match set {
        Set::PresetSave(_) | Set::PresetRecall(_) => return None,
        Set::MixerChannel(m, 0, _) => {
            let m = m.get() as usize;
            (
                format!("/mixers/{m}/master"),
                to_value(&state.mixers[m].master),
            )
        }
        Set::MixerChannel(m, ch, _) => {
            let m = m.get() as usize;
            let i = (*ch as usize - 1).min(MIXER_CHANNELS - 1);
            (
                format!("/mixers/{m}/channels/{i}"),
                to_value(&state.mixers[m].channels[i]),
            )
        }
        Set::Routing(b, _) => {
            let i = routing_index(state, *b)?;
            (format!("/routing/{i}"), to_value(&state.routing[i]))
        }
        Set::Trim(TrimId::LineOut, _) => {
            ("/trims/line_out".into(), to_value(&state.trims.line_out))
        }
        Set::Trim(TrimId::LineIn, _) => ("/trims/line_in".into(), to_value(&state.trims.line_in)),
        Set::StereoLink(LinkTarget::Mixer { mixer, pair }, _) => {
            let (m, p) = (mixer.get() as usize, *pair as usize);
            (
                format!("/mixer_links/{m}/{p}"),
                to_value(&state.mixer_links[m].get(p)?),
            )
        }
        Set::StereoLink(LinkTarget::Afx { pair }, _) => {
            let p = *pair as usize;
            (
                format!("/afx/links/{p}"),
                to_value(&state.afx.links.get(p)?),
            )
        }
        Set::AfxEq { inst, .. } => afx_path(state, AfxType::Equalizer, *inst)?,
        Set::AfxCompressor { inst, .. } => afx_path(state, AfxType::Compressor, *inst)?,
        Set::AfxRaw { inst, raw } => afx_path(state, raw.ty, *inst)?,
        Set::AfxBypass { ty, inst, .. } => afx_path(state, *ty, *inst)?,
        Set::AfxOrder { chain, .. } => {
            let c = *chain as usize;
            (format!("/afx/order/{c}"), to_value(state.afx.order.get(c)?))
        }
        Set::Reverb(_) => ("/reverb".into(), to_value(&state.reverb)),
        _ => ("/status".into(), to_value(&state.status)),
    })
}

fn afx_path(state: &DeviceState, ty: AfxType, inst: u8) -> Option<(String, Value)> {
    let c = state.afx.configs.get(&ty)?.get(inst as usize)?;
    Some((format!("/afx/configs/{ty:?}/{inst}"), to_value(c)))
}

/// JSON pointer and value of what `get` reads.
pub(crate) fn region(get: &Get, state: &DeviceState) -> (String, Value) {
    match get {
        Get::Firmware | Get::CompatPassport | Get::Passport | Get::Registration => {
            ("/info".into(), to_value(&state.info))
        }
        Get::Routing(b) => match routing_index(state, *b) {
            Some(i) => (format!("/routing/{i}"), to_value(&state.routing[i])),
            None => (format!("/routing/{}", b.as_u8()), Value::Null),
        },
        Get::Mixer(m) => {
            let m = m.get() as usize;
            (format!("/mixers/{m}"), to_value(&state.mixers[m]))
        }
        Get::AfxConfigs(ty, _) => (
            format!("/afx/configs/{ty:?}"),
            to_value(&state.afx.configs.get(ty)),
        ),
        Get::AfxOrder => ("/afx/order".into(), to_value(&state.afx.order)),
        Get::Reverb => ("/reverb".into(), to_value(&state.reverb)),
        Get::MixerLinks => ("/mixer_links".into(), to_value(&state.mixer_links)),
        Get::AfxLinks => ("/afx/links".into(), to_value(&state.afx.links)),
        Get::AfxInstances => ("/afx/instances".into(), to_value(&state.afx.instances)),
        Get::HdxDelays => ("/hdx_delays".into(), to_value(&state.hdx_delays)),
        Get::Trims => ("/trims".into(), to_value(&state.trims)),
    }
}

/// Re-apply a not-yet-confirmed set on top of fresh device data. Like
/// `apply_set`, but without side effects on other records (an order set
/// would otherwise disable instances again).
pub(crate) fn overlay(state: &mut DeviceState, set: &Set) {
    match set {
        Set::AfxOrder { chain, slots } => {
            if let Some(c) = state.afx.order.get_mut(*chain as usize) {
                *c = *slots;
            }
        }
        _ => {
            let preset = state.status.current_preset;
            let _ = openorion_proto::decode::apply_set(state, set);
            state.status.current_preset = preset;
        }
    }
}
