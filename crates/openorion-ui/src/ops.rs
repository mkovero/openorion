//! Human text for ops: one-line descriptions (outbox, confirm dialog) and
//! the warnings a confirm dialog shows.

use openorion_ipc::Op;
use openorion_proto::{LinkTarget, OutputBank, Risk, Source};

use crate::labels::*;

/// One-line description, e.g. "Sample rate -> 48k".
pub fn describe(op: &Op) -> String {
    match op {
        Op::Power { on } => format!("Power -> {}", if *on { "on" } else { "standby" }),
        Op::SampleRate { rate } => format!("Sample rate -> {}", sample_rate_name(*rate)),
        Op::ClockSource { source } => format!("Clock source -> {}", clock_source_name(*source)),
        Op::Volume { id, atten } => format!("{id:?} volume -> {} dB", volume_label(*atten)),
        Op::Mute { id, mute } => format!("{id:?} mute -> {}", on_off(*mute)),
        Op::PresetSave { slot } => format!("Save device preset {}", slot.get()),
        Op::PresetRecall { slot } => format!("Recall device preset {}", slot.get()),
        Op::Oscillator { osc } => format!(
            "Oscillator L {} / R {}, {}",
            osc_freq_name(osc.freq_l),
            osc_freq_name(osc.freq_r),
            osc_level_name(osc.level)
        ),
        Op::Brightness { value } => format!("Brightness -> {}", value.get()),
        Op::Trim { id, ch, level } => format!(
            "{} trim {} -> {:.0} dBu",
            trim_id_name(*id),
            ch.map_or("all".to_owned(), |c| format!("ch {}", c + 1)),
            level.dbu()
        ),
        Op::TrimControl { id, control } => {
            format!(
                "{} trim control -> {}",
                trim_id_name(*id),
                trim_control_name(*control)
            )
        }
        Op::SpdifSrc { on } => format!("S/PDIF SRC -> {}", on_off(*on)),
        Op::Smux { id, on } => format!("S-MUX {id:?} -> {}", on_off(*on)),
        Op::MeterSource { bank, src } => format!(
            "Meter {bank:?} -> {}",
            match bank {
                openorion_proto::MeterBank::Mixer => format!("MIXER {}", src + 1),
                _ => meter_source_name(*src).unwrap_or("?").to_owned(),
            }
        ),
        Op::Mixer {
            mixer,
            ch,
            level,
            pan,
            mute,
            solo,
            send,
        } => {
            let strip = if *ch == 0 {
                "master".to_owned()
            } else {
                format!("ch {ch}")
            };
            let mut parts = Vec::new();
            if let Some(v) = level {
                parts.push(format!("level {}", atten_label(*v)));
            }
            if let Some(v) = pan {
                parts.push(format!("pan {}", pan_label(*v)));
            }
            if let Some(v) = mute {
                parts.push(format!("mute {}", on_off(*v)));
            }
            if let Some(v) = solo {
                parts.push(format!("solo {}", on_off(*v)));
            }
            if let Some(v) = send {
                parts.push(format!("send {}", atten_label(*v)));
            }
            format!("Mixer {} {strip}: {}", mixer.get() + 1, parts.join(", "))
        }
        Op::Route { bank, ch, sources } => {
            let first = sources.first().copied().unwrap_or(Source::Mute);
            let n = sources.len();
            let outs = if n > 1 {
                format!("{}-{}", ch + 1, *ch as usize + n)
            } else {
                format!("{}", ch + 1)
            };
            let src = if n > 1 && first != Source::Mute {
                format!("{} (+{})", source_name(first), n - 1)
            } else {
                source_name(first)
            };
            format!("Route {} {outs} <- {src}", output_bank_name(*bank))
        }
        Op::StereoLink { target, linked } => match target {
            LinkTarget::Mixer { mixer, pair } => format!(
                "Mixer {} link ch {}-{} -> {}",
                mixer.get() + 1,
                pair * 2 + 1,
                pair * 2 + 2,
                on_off(*linked)
            ),
            LinkTarget::Afx { pair } => format!(
                "AFX link chains {}-{} -> {}",
                pair * 2 + 1,
                pair * 2 + 2,
                on_off(*linked)
            ),
        },
        Op::AfxEqBand { inst, band, .. } => format!("ClearQ #{} band {}", inst + 1, band + 1),
        Op::AfxCompressor { inst, .. } => format!("PowerFFC #{}", inst + 1),
        Op::AfxRaw { ty, inst, .. } => format!("{} #{}", afx_type_name(*ty), inst + 1),
        Op::AfxChain { chain, slots } => {
            let names: Vec<_> = slots.iter().map(|s| afx_type_name(s.ty)).collect();
            format!("AFX chain {}: [{}]", chain + 1, names.join(", "))
        }
        Op::AfxBypass { ty, inst, enabled } => format!(
            "{} #{} -> {}",
            afx_type_name(*ty),
            inst + 1,
            if *enabled { "active" } else { "bypassed" }
        ),
        Op::Reverb { params } => format!(
            "Reverb (mixer {}) {}",
            params.mixer.get() + 1,
            if params.on { "on" } else { "off" }
        ),
        Op::Resync => "Resync from device".to_owned(),
        Op::Recheck => "Recheck the control channel".to_owned(),
        Op::Reset { force: false } => "USB reset of the Orion".to_owned(),
        Op::Reset { force: true } => "USB reset of the Orion (interrupts USB audio)".to_owned(),
    }
}

fn on_off(b: bool) -> &'static str {
    if b { "on" } else { "off" }
}

/// Why an op needs confirmation; empty for ops that do not.
pub fn warnings(op: &Op) -> Vec<String> {
    let mut w = Vec::new();
    if op.risk() != Risk::Disruptive {
        return w;
    }
    match op {
        Op::ClockSource { source } => {
            w.push("Audio drops out while the clock relocks.".into());
            if source.is_external() {
                w.push("If LOCK does not come up, switch back to Oven/Atomic.".into());
            }
        }
        Op::SampleRate { .. } => {
            w.push("Audio drops out; JACK must be restarted at the new rate.".into())
        }
        Op::Power { on: false } => w.push("Standby silences every output.".into()),
        Op::Power { on: true } => w.push("Wakes the device from standby.".into()),
        Op::PresetRecall { .. } => {
            w.push("Replaces routing, mixers and trims with the stored preset.".into())
        }
        Op::PresetSave { slot } => w.push(format!("Overwrites device preset slot {}.", slot.get())),
        Op::Route { bank, .. } => {
            debug_assert!(bank.is_physical());
            w.push(format!(
                "{} drives physical outputs: the change is audible.",
                output_bank_name(*bank)
            ));
            if *bank == OutputBank::Monitor {
                w.push("MONITOR feeds the speakers.".into());
            }
        }
        _ => {}
    }
    w
}

#[cfg(test)]
mod tests {
    use super::*;
    use openorion_proto::*;

    #[test]
    fn describes() {
        let op = Op::Route {
            bank: OutputBank::LineOut,
            ch: 16,
            sources: vec![
                Source::Input {
                    group: SourceGroup::AdatIn,
                    chan: 2,
                },
                Source::Input {
                    group: SourceGroup::AdatIn,
                    chan: 3,
                },
            ],
        };
        assert_eq!(describe(&op), "Route LINE OUT 17-18 <- ADAT IN 3 (+1)");
        assert!(!warnings(&op).is_empty());
        let op = Op::ClockSource {
            source: ClockSource::Adat,
        };
        assert!(warnings(&op).iter().any(|w| w.contains("LOCK")));
        assert!(
            warnings(&Op::Brightness {
                value: Brightness::new(5).unwrap()
            })
            .is_empty()
        );
        assert_eq!(
            describe(&Op::SampleRate {
                rate: SampleRate::R48k
            }),
            "Sample rate -> 48k"
        );
    }
}
