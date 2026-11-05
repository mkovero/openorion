//! MIDI control map (DESIGN.md sec. 9 item 3), a simple line format
//! instead of TOML (no TOML crate in the workspace):
//!
//! ```text
//! # cc <channel 1-16 | *> <controller 0-127> <target>
//! cc 1 7   mixer 4 ch 1 level     # 127 = 0 dB, 0 = -inf
//! cc 1 8   mixer 4 ch 1 mute      # value >= 64 = muted
//! cc 1 9   mixer 4 ch 1 solo
//! cc 1 10  mixer 4 ch 1 send
//! cc 1 11  mixer 4 master level
//! cc * 20  volume main            # monitor volume
//! cc * 21  volume line
//! cc * 22  mute main
//! ```
//!
//! Mixers are numbered 1..4 as in the panel. Comments start with `#`.

use openorion_ipc::Op;
use openorion_proto::{Atten, MIXER_CHANNELS, MixerId, VolumeId};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum MixerParam {
    Level,
    Mute,
    Solo,
    Send,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Target {
    /// ch 0 = master.
    Mixer {
        mixer: MixerId,
        ch: u8,
        param: MixerParam,
    },
    Volume(VolumeId),
    Mute(VolumeId),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Binding {
    /// 0-based MIDI channel; None = any.
    pub channel: Option<u8>,
    pub cc: u8,
    pub target: Target,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MidiMap {
    pub bindings: Vec<Binding>,
}

fn parse_volume_id(s: &str) -> Result<VolumeId, String> {
    match s.to_ascii_lowercase().as_str() {
        "main" | "monitor" => Ok(VolumeId::Main),
        "line" => Ok(VolumeId::Line),
        _ => Err(format!("unknown volume \"{s}\" (main, line)")),
    }
}

fn parse_target(w: &[&str]) -> Result<Target, String> {
    match w {
        ["volume", v] => Ok(Target::Volume(parse_volume_id(v)?)),
        ["mute", v] => Ok(Target::Mute(parse_volume_id(v)?)),
        ["mixer", m, rest @ ..] => {
            let m: u8 = m.parse().map_err(|_| format!("bad mixer \"{m}\""))?;
            if !(1..=4).contains(&m) {
                return Err(format!("mixer {m} out of 1..4"));
            }
            let mixer = MixerId::new(m - 1).map_err(|e| e.to_string())?;
            let (ch, p) = match rest {
                ["master", p] => (0u8, *p),
                ["ch", c, p] => {
                    let c: u8 = c.parse().map_err(|_| format!("bad channel \"{c}\""))?;
                    if !(1..=MIXER_CHANNELS as u8).contains(&c) {
                        return Err(format!("channel {c} out of 1..{MIXER_CHANNELS}"));
                    }
                    (c, *p)
                }
                _ => return Err("expected `master <param>` or `ch <n> <param>`".into()),
            };
            let param = match p {
                "level" => MixerParam::Level,
                "mute" => MixerParam::Mute,
                "solo" => MixerParam::Solo,
                "send" => MixerParam::Send,
                _ => return Err(format!("unknown mixer parameter \"{p}\"")),
            };
            Ok(Target::Mixer { mixer, ch, param })
        }
        _ => Err("unknown target".into()),
    }
}

impl MidiMap {
    pub fn parse(text: &str) -> Result<MidiMap, String> {
        let mut bindings = Vec::new();
        for (i, raw) in text.lines().enumerate() {
            let line = raw.split('#').next().unwrap_or("").trim();
            if line.is_empty() {
                continue;
            }
            let err = |e: String| format!("line {}: {e}: {raw}", i + 1);
            let w: Vec<&str> = line.split_whitespace().collect();
            let ["cc", chan, cc, target @ ..] = w.as_slice() else {
                return Err(err("expected `cc <channel> <controller> <target>`".into()));
            };
            let channel = if *chan == "*" {
                None
            } else {
                match chan.parse::<u8>() {
                    Ok(c @ 1..=16) => Some(c - 1),
                    _ => return Err(err(format!("bad MIDI channel \"{chan}\""))),
                }
            };
            let cc = match cc.parse::<u8>() {
                Ok(c @ 0..=127) => c,
                _ => return Err(err(format!("bad controller \"{cc}\""))),
            };
            let target = parse_target(target).map_err(err)?;
            bindings.push(Binding {
                channel,
                cc,
                target,
            });
        }
        Ok(MidiMap { bindings })
    }

    /// Targets bound to a CC message (raw status byte, controller, value).
    /// Non-CC messages give nothing.
    pub fn lookup(&self, status: u8, cc: u8) -> impl Iterator<Item = Target> + '_ {
        let is_cc = status & 0xF0 == 0xB0;
        let chan = status & 0x0F;
        self.bindings
            .iter()
            .filter(move |b| is_cc && b.cc == cc && b.channel.is_none_or(|c| c == chan))
            .map(|b| b.target)
    }
}

/// CC 0..127 → attenuation: 127 = 0 dB, 0 = -inf (96), linear in dB.
pub fn cc_to_atten(v: u8) -> Atten {
    let v = v.min(127) as u32;
    let a = ((127 - v) * 96 + 63) / 127;
    Atten::new(a as u8).unwrap_or(Atten::NEG_INF)
}

/// The op for a target at CC value `v`.
pub fn op_for(t: Target, v: u8) -> Op {
    let on = v >= 64;
    match t {
        Target::Volume(id) => Op::Volume {
            id,
            atten: cc_to_atten(v),
        },
        Target::Mute(id) => Op::Mute { id, mute: on },
        Target::Mixer { mixer, ch, param } => {
            let mut op = Op::Mixer {
                mixer,
                ch,
                level: None,
                pan: None,
                mute: None,
                solo: None,
                send: None,
            };
            if let Op::Mixer {
                level,
                mute,
                solo,
                send,
                ..
            } = &mut op
            {
                match param {
                    MixerParam::Level => *level = Some(cc_to_atten(v)),
                    MixerParam::Send => *send = Some(cc_to_atten(v)),
                    MixerParam::Mute => *mute = Some(on),
                    MixerParam::Solo => *solo = Some(on),
                }
            }
            op
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_and_lookup() {
        let m = MidiMap::parse(
            "# demo\n\
             cc 1 7 mixer 4 ch 1 level\n\
             cc 2 8 mixer 1 master mute  # trailing\n\
             \n\
             cc * 20 volume main\n",
        )
        .unwrap();
        assert_eq!(m.bindings.len(), 3);
        let t: Vec<Target> = m.lookup(0xB0, 7).collect();
        assert_eq!(
            t,
            vec![Target::Mixer {
                mixer: MixerId::new(3).unwrap(),
                ch: 1,
                param: MixerParam::Level
            }]
        );
        assert_eq!(m.lookup(0xB1, 7).count(), 0, "other channel");
        assert_eq!(m.lookup(0x90, 7).count(), 0, "note on");
        assert_eq!(m.lookup(0xBF, 20).count(), 1, "any channel");
        assert_eq!(m.lookup(0xB1, 8).count(), 1);

        for bad in [
            "cc 0 7 volume main",
            "cc 1 128 volume main",
            "cc 1 7 mixer 5 ch 1 level",
            "cc 1 7 mixer 1 ch 33 level",
            "cc 1 7 mixer 1 ch 1 pan",
            "note 1 7 volume main",
            "cc 1 7 volume side",
        ] {
            assert!(MidiMap::parse(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn values() {
        assert_eq!(cc_to_atten(127), Atten::ZERO_DB);
        assert_eq!(cc_to_atten(0), Atten::NEG_INF);
        assert_eq!(cc_to_atten(64).get(), 48);
        assert_eq!(
            op_for(Target::Mute(VolumeId::Main), 127),
            Op::Mute {
                id: VolumeId::Main,
                mute: true
            }
        );
        let op = op_for(
            Target::Mixer {
                mixer: MixerId::new(3).unwrap(),
                ch: 0,
                param: MixerParam::Send,
            },
            127,
        );
        assert!(
            matches!(op, Op::Mixer { ch: 0, send: Some(a), level: None, .. } if a == Atten::ZERO_DB)
        );
    }
}
