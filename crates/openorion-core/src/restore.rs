//! Restore planning: the minimal ops that take one state to another,
//! covering only what can be set. Read-only parts (device info, AFX
//! instance counts, HDX delays, the monitor trim, unused reverb slots) are
//! skipped. The ops are applied through the normal op path.

use openorion_ipc::Op;
use std::collections::BTreeMap;

use openorion_proto::decode::{apply_set, default_afx_config};
use openorion_proto::names::afx_fields;
use openorion_proto::*;

use crate::build::build_sets;

struct Planner {
    /// `current` with every planned op applied, so later sections see the
    /// effects of earlier ones (e.g. a chain change re-enabling instances).
    sim: DeviceState,
    ops: Vec<Op>,
}

impl Planner {
    fn push(&mut self, op: Op) {
        match build_sets(&self.sim, &op) {
            Ok(sets) => {
                for s in &sets {
                    let _ = apply_set(&mut self.sim, s);
                }
                self.ops.push(op);
            }
            Err(e) => tracing::debug!("restore: skipping {op:?}: {e}"),
        }
    }
}

/// Ops that turn `current` into `target` (settable state only), in a safe
/// order: routing and mixers first, AFX, then status settings, with the
/// sample rate, clock source and power last.
pub fn plan_restore(current: &DeviceState, target: &DeviceState) -> Vec<Op> {
    let mut p = Planner {
        sim: current.clone(),
        ops: Vec::new(),
    };

    // Routing: one op per bank over the differing range.
    for tb in &target.routing {
        let Some(cb) = p.sim.routing.iter().find(|r| r.bank == tb.bank) else {
            continue;
        };
        let n = tb.sources.len().min(cb.sources.len());
        let diff: Vec<usize> = (0..n).filter(|&i| tb.sources[i] != cb.sources[i]).collect();
        if let (Some(&a), Some(&b)) = (diff.first(), diff.last()) {
            let op = Op::Route {
                bank: tb.bank,
                ch: a as u8,
                sources: tb.sources[a..=b].to_vec(),
            };
            p.push(op);
        }
    }

    // Mixers: only the differing fields of differing channels.
    for (m, tm) in target.mixers.iter().enumerate() {
        let mixer = MixerId::new(m as u8).expect("4 mixers");
        for ch in 0..=MIXER_CHANNELS {
            let (t, c) = if ch == 0 {
                (tm.master, p.sim.mixers[m].master)
            } else {
                (tm.channels[ch - 1], p.sim.mixers[m].channels[ch - 1])
            };
            if t != c {
                let d = |a: bool| a.then_some(());
                p.push(Op::Mixer {
                    mixer,
                    ch: ch as u8,
                    level: d(t.level != c.level).map(|_| t.level),
                    pan: d(t.pan != c.pan).map(|_| t.pan),
                    mute: d(t.mute != c.mute).map(|_| t.mute),
                    solo: d(t.solo != c.solo).map(|_| t.solo),
                    send: d(t.send != c.send).map(|_| t.send),
                });
            }
        }
    }
    for m in 0..4 {
        for pair in 0..16 {
            let t = target.mixer_links[m][pair];
            if t != p.sim.mixer_links[m][pair] {
                p.push(Op::StereoLink {
                    target: LinkTarget::Mixer {
                        mixer: MixerId::new(m as u8).expect("4 mixers"),
                        pair: pair as u8,
                    },
                    linked: t,
                });
            }
        }
    }

    // Trims (line out, line in).
    for id in [TrimId::LineOut, TrimId::LineIn] {
        let pick = |s: &DeviceState| match id {
            TrimId::LineOut => s.trims.line_out.clone(),
            _ => s.trims.line_in.clone(),
        };
        let t = pick(target);
        if t.control != pick(&p.sim).control {
            p.push(Op::TrimControl {
                id,
                control: t.control,
            });
        }
        let c = pick(&p.sim);
        let diff: Vec<usize> = (0..TRIM_CHANNELS)
            .filter(|&i| t.levels[i] != c.levels[i])
            .collect();
        let uniform = t.levels.iter().all(|l| *l == t.levels[0]);
        if diff.len() > 1 && uniform {
            p.push(Op::Trim {
                id,
                ch: None,
                level: t.levels[0],
            });
        } else {
            for i in diff {
                p.push(Op::Trim {
                    id,
                    ch: Some(i as u8),
                    level: t.levels[i],
                });
            }
        }
    }

    // Reverb: assigned slots only (a slot cannot be unassigned).
    for r in target.reverb.iter().flatten() {
        let cur = p
            .sim
            .reverb
            .iter()
            .flatten()
            .find(|c| c.mixer == r.mixer)
            .copied();
        if cur != Some(*r) {
            p.push(Op::Reverb { params: *r });
        }
    }

    // AFX chains. First pass: remove what the target drops from each chain,
    // so a moved instance never sits in two chains; second pass: the
    // target chains.
    let packed = |c: &[Option<AfxSlot>; AFX_SLOTS]| c.iter().flatten().copied().collect::<Vec<_>>();
    for chain in 0..AFX_CHAINS {
        let t = target.afx.order[chain];
        let cur = packed(&p.sim.afx.order[chain]);
        let keep: Vec<AfxSlot> = cur
            .iter()
            .copied()
            .filter(|s| t.contains(&Some(*s)))
            .collect();
        if keep != cur {
            p.push(Op::AfxChain {
                chain: chain as u8,
                slots: keep,
            });
        }
    }
    for chain in 0..AFX_CHAINS {
        let t = target.afx.order[chain];
        if t != p.sim.afx.order[chain] {
            p.push(Op::AfxChain {
                chain: chain as u8,
                slots: packed(&t),
            });
        }
    }
    for pair in 0..AFX_CHAINS / 2 {
        let t = target.afx.links[pair];
        if t != p.sim.afx.links[pair] {
            p.push(Op::StereoLink {
                target: LinkTarget::Afx { pair: pair as u8 },
                linked: t,
            });
        }
    }

    // AFX configs, then bypass state (after chain changes).
    for (ty, tv) in &target.afx.configs {
        for (inst, tc) in tv.iter().enumerate() {
            let Some(cc) = p.sim.afx.configs.get(ty).and_then(|v| v.get(inst)).cloned() else {
                continue;
            };
            let inst = inst as u8;
            match (&tc.params, &cc.params) {
                (AfxParams::Eq(t), AfxParams::Eq(c)) => {
                    for band in 0..EQ_BANDS {
                        let (tb, cb) = (t.bands[band], c.bands[band]);
                        if tb != cb {
                            p.push(Op::AfxEqBand {
                                inst,
                                band: band as u8,
                                freq: Some(tb.freq),
                                q: Some(tb.q),
                                gain: Some(tb.gain),
                                ftype: Some(tb.ftype),
                            });
                        }
                    }
                }
                (AfxParams::Compressor(t), AfxParams::Compressor(c)) if t != c => {
                    p.push(Op::AfxCompressor { inst, params: *t });
                }
                (AfxParams::Raw(t), AfxParams::Raw(c)) if t.ty == c.ty => {
                    let table = afx_fields(t.ty).unwrap_or(&[]);
                    let fields: Vec<Option<i32>> = (0..table.len())
                        .map(|i| {
                            let (tv, cv) = (t.fields.get(i), c.fields.get(i));
                            (table[i].in_set && tv.is_some() && tv != cv).then(|| *tv.unwrap())
                        })
                        .collect();
                    if fields.iter().any(Option::is_some) {
                        p.push(Op::AfxRaw {
                            ty: *ty,
                            inst,
                            fields,
                        });
                    }
                }
                _ => {}
            }
            let now = p.sim.afx.configs[ty][inst as usize].enabled;
            if tc.enabled != now {
                p.push(Op::AfxBypass {
                    ty: *ty,
                    inst,
                    enabled: tc.enabled,
                });
            }
        }
    }

    // Status-backed settings.
    let (ts, cs) = (&target.status, p.sim.status.clone());
    for (id, t, c) in [
        (VolumeId::Main, ts.main, cs.main),
        (VolumeId::Line, ts.line, cs.line),
    ] {
        if t.atten != c.atten {
            p.push(Op::Volume { id, atten: t.atten });
        }
        if t.mute != c.mute {
            p.push(Op::Mute { id, mute: t.mute });
        }
    }
    if ts.oscillator != cs.oscillator {
        p.push(Op::Oscillator { osc: ts.oscillator });
    }
    if ts.brightness != cs.brightness
        && let Ok(value) = Brightness::new(ts.brightness)
    {
        p.push(Op::Brightness { value });
    }
    if ts.spdif_src != cs.spdif_src {
        p.push(Op::SpdifSrc { on: ts.spdif_src });
    }
    if ts.smux_in != cs.smux_in {
        p.push(Op::Smux {
            id: SmuxId::MadiIn,
            on: ts.smux_in,
        });
    }
    if ts.smux_out != cs.smux_out {
        p.push(Op::Smux {
            id: SmuxId::MadiOut,
            on: ts.smux_out,
        });
    }
    for bank in [MeterBank::A, MeterBank::B, MeterBank::Mixer] {
        let i = bank.as_u8() as usize;
        if ts.meter_sources[i] != cs.meter_sources[i] {
            p.push(Op::MeterSource {
                bank,
                src: ts.meter_sources[i],
            });
        }
    }
    if let (Some(t), Some(c)) = (ts.sample_rate, cs.sample_rate)
        && t != c
    {
        p.push(Op::SampleRate { rate: t });
    }
    if let (Some(t), Some(c)) = (ts.clock_source, cs.clock_source)
        && t != c
    {
        p.push(Op::ClockSource { source: t });
    }
    if ts.power_on != cs.power_on {
        p.push(Op::Power { on: ts.power_on });
    }
    p.ops
}

/// Power-up AFX recovery. A power cycle resets the AFX configs to factory
/// defaults while routing and mixers survive (findings), and the vendor
/// software re-sends them on connect. Returns `current` with every
/// instance taken from `saved` whose config differs from it *and* whose
/// params are the factory defaults now (the power-up signature), plus the
/// instances picked. An instance changed to other values (another client
/// while we were away) is left alone. Feed the result to `plan_restore`.
pub fn afx_power_up_target(
    current: &DeviceState,
    saved: &BTreeMap<AfxType, Vec<AfxConfig>>,
) -> (DeviceState, Vec<(AfxType, u8)>) {
    let mut target = current.clone();
    let mut picked = Vec::new();
    for (ty, cv) in &current.afx.configs {
        let (Some(sv), Some(default)) = (saved.get(ty), default_afx_config(*ty)) else {
            continue;
        };
        for (inst, (c, s)) in cv.iter().zip(sv).enumerate() {
            if c != s
                && c.params == default.params
                && std::mem::discriminant(&s.params) == std::mem::discriminant(&c.params)
            {
                target.afx.configs.get_mut(ty).expect("same type")[inst] = s.clone();
                picked.push((*ty, inst as u8));
            }
        }
    }
    (target, picked)
}
