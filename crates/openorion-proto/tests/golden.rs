//! Golden tests: protocol-spec.md sec. 5 examples and the session-1
//! panel writes (fixtures/s1-decoded-sets.tsv) encode byte-exact.

mod common;

use std::collections::BTreeMap;

use common::{hex, report};
use openorion_proto::codec::{HEADER_LEN, ID_SET};
use openorion_proto::*;

fn mid(m: u8) -> MixerId {
    MixerId::new(m).unwrap()
}

fn atten(v: u8) -> Atten {
    Atten::new(v).unwrap()
}

/// Encode `set`, compare with `hex`, check the report framing, and decode
/// the report back.
fn golden(set: Set, want: &str) {
    let want = hex(want);
    let p = set.payload().unwrap();
    assert_eq!(p, want, "{set:?}");
    let r = set.report().unwrap();
    let b = r.as_bytes();
    assert_eq!(r.id(), ID_SET);
    assert_eq!(
        u32::from_le_bytes(b[4..8].try_into().unwrap()) as usize,
        HEADER_LEN + want.len()
    );
    assert_eq!((r.ext2(), r.ext3()), (0, 0));
    assert_eq!(&b[HEADER_LEN..HEADER_LEN + want.len()], &want[..]);
    assert!(b[HEADER_LEN + want.len()..].iter().all(|&x| x == 0));
    assert_eq!(Set::decode(b).unwrap(), set);
    // raw_checked builds the identical report from the payload.
    assert_eq!(Report::raw_checked(&want).unwrap(), r);
}

#[test]
fn spec_section5_examples() {
    golden(Set::Power(false), "03 00");
    golden(Set::Power(true), "03 01");
    golden(Set::SampleRate(SampleRate::R88k2), "04 03");
    golden(Set::ClockSource(ClockSource::Internal), "05 00");
    golden(Set::Volume(VolumeId::Line, atten(2)), "46 01 02");
    golden(Set::Mute(VolumeId::Main, true), "47 00 01");
    golden(Set::PresetSave(PresetSlot::new(5).unwrap()), "08 05");
    golden(Set::PresetRecall(PresetSlot::new(5).unwrap()), "09 05");
    // Confirmed baseline (meter calibration on the wire).
    golden(
        Set::Oscillator(Osc {
            freq_l: OscFreq::Hz1k,
            freq_r: OscFreq::Hz1k,
            level: OscLevel::DbM12,
            mute_l: false,
            mute_r: true,
        }),
        "0a a0",
    );
    golden(
        Set::Oscillator(Osc {
            freq_l: OscFreq::Hz1k,
            freq_r: OscFreq::Hz440,
            level: OscLevel::Db0,
            mute_l: false,
            mute_r: false,
        }),
        "0a 04",
    );
    golden(Set::Brightness(Brightness::new(25).unwrap()), "0b 19");
    let mut levels = [TrimLevel { whole: 8, fract: 0 }; TRIM_CHANNELS];
    levels[0].whole = 9;
    golden(
        Set::Trim(
            TrimId::LineOut,
            TrimRecord {
                control: TrimControl::Individual,
                levels,
            },
        ),
        &format!("cc 42 01 01 09 00 {}", "08 00 ".repeat(31)),
    );
    golden(Set::SpdifSrc(true), "0d 01");
    golden(Set::Smux(SmuxId::MadiIn, true), "4e 00 01");
    golden(Set::MeterSource(MeterBank::Mixer, 3), "50 02 03");
    golden(
        Set::MixerChannel(
            mid(3),
            1,
            MixerChannel {
                level: atten(30),
                pan: Pan::new(2).unwrap(),
                mute: false,
                solo: false,
                send: atten(96),
            },
        ),
        "d1 05 03 01 1e 02 60",
    );
    golden(
        Set::Routing(OutputBank::MixCh4, vec![Source::Mute; 32]),
        &format!("d2 41 0e {}", "0e 00 ".repeat(32)),
    );
    golden(
        Set::StereoLink(
            LinkTarget::Mixer {
                mixer: mid(3),
                pair: 0,
            },
            true,
        ),
        "93 03 30 01",
    );
    golden(
        Set::AfxEq {
            inst: 15,
            band: 2,
            value: EqBand {
                freq: 1000,
                q: 71,
                gain: 300,
                ftype: EqFilter::Peak,
            },
        },
        "d4 0a 01 0f 02 e8 03 47 00 2c 01 02",
    );
    let mut slots = [None; AFX_SLOTS];
    slots[0] = Some(AfxSlot {
        ty: AfxType::Equalizer,
        inst: 15,
    });
    golden(
        Set::AfxOrder { chain: 15, slots },
        &format!("d5 11 0f 01 0f {}", "00 ".repeat(14)),
    );
    golden(
        Set::AfxBypass {
            ty: AfxType::Equalizer,
            inst: 15,
            enabled: false,
        },
        "98 0f 01 00",
    );
    golden(
        Set::Reverb(Reverb {
            mixer: mid(0),
            room_size: 68,
            color: 12,
            predelay: 0,
            density: 100,
            early_ref_gain: 12,
            late_ref_delay: 116,
            richness: 25,
            reverb_time: 23,
            reverb_level: 25,
            on: false,
        }),
        "da 0b 00 44 0c 00 64 0c 74 19 17 19 00",
    );
}

#[test]
fn findings_afx_examples() {
    // set_compressor_cfg `d4 14 02 0f …` (on the wire: ratio 400, thr 40).
    golden(
        Set::AfxCompressor {
            inst: 15,
            params: CompressorParams {
                attack: 1000,
                release: 10000,
                taw: 0xFFFF,
                ratio: 400,
                gain: 0,
                ctrl: 0,
                threshold: 40,
                knee: 0,
                linked: 0,
            },
        },
        "d4 14 02 0f e8 03 00 00 10 27 00 00 ff ff 90 01 00 00 00 28 00 00",
    );
    // A vintage model: BAE-1073 at its defaults, gain -3 (two's complement).
    let mut fields: Vec<i32> = names::afx_fields(AfxType::Neve1073)
        .unwrap()
        .iter()
        .map(|f| f.default)
        .collect();
    fields[0] = -3;
    golden(
        Set::AfxRaw {
            inst: 2,
            raw: AfxRaw {
                ty: AfxType::Neve1073,
                fields,
            },
        },
        "d4 0a 07 02 fd 08 00 08 00 08 00 00",
    );
    // PowerGate: u16 fields, s8 gain.
    golden(
        Set::AfxRaw {
            inst: 0,
            raw: AfxRaw {
                ty: AfxType::Powergate,
                fields: vec![90, 0, 100, 50, 0, -6],
            },
        },
        "d4 0b 27 00 5a 00 64 00 32 00 00 00 fa",
    );
}

#[test]
fn unknown_oscillator_frequency_is_refused() {
    // freq_l = freq_r = 2. Only codes 0/1 are known.
    let r = report(&hex("70000000 12000000 00000000 00000000 0a 9a"));
    assert!(matches!(
        Set::decode(&r),
        Err(ProtoError::UnknownValue {
            what: "oscillator frequency",
            ..
        })
    ));
}

fn parse_fields(s: &str) -> BTreeMap<&str, &str> {
    s.split_whitespace()
        .map(|kv| kv.split_once('=').unwrap())
        .collect()
}

#[test]
fn session1_panel_writes() {
    let text = std::fs::read_to_string(common::fixture("s1-decoded-sets.tsv")).unwrap();
    let (mut ok, mut master_pan0) = (0, 0);
    // The panel writes master pan 0 (seen on the wire).
    for line in text.lines().skip(1) {
        let cols: Vec<&str> = line.split('\t').collect();
        let (payload, pid, fields) = (hex(cols[2]), cols[3], parse_fields(cols[4]));
        let n = |k: &str| -> u8 {
            match fields[k] {
                "True" => 1,
                "False" => 0,
                v => v.parse().unwrap(),
            }
        };
        let r = report(
            &[
                &0x70u32.to_le_bytes()[..],
                &(16 + payload.len() as u32).to_le_bytes(),
                &[0; 8],
                &payload,
            ]
            .concat(),
        );
        let expect = match pid {
            "16" => Set::MeterSource(MeterBank::try_from(n("bank")).unwrap(), n("source")),
            "19" => {
                assert_eq!(n("peripheral"), 3);
                let ch = n("channel");
                Set::StereoLink(
                    LinkTarget::Mixer {
                        mixer: mid(ch / 16),
                        pair: ch % 16,
                    },
                    n("linked") != 0,
                )
            }
            "17" => {
                if n("channel") == 0 && n("pan") == 0 {
                    master_pan0 += 1;
                }
                Set::MixerChannel(
                    mid(n("mixer")),
                    n("channel"),
                    MixerChannel {
                        level: atten(n("level")),
                        pan: Pan::new(n("pan")).unwrap(),
                        mute: n("mute") != 0,
                        solo: n("solo") != 0,
                        send: atten(n("send")),
                    },
                )
            }
            p => panic!("unexpected pid {p}"),
        };
        assert_eq!(Set::decode(&r).unwrap(), expect, "{line}");
        assert_eq!(expect.payload().unwrap(), payload, "{line}");
        ok += 1;
    }
    assert_eq!(ok, 49);
    assert_eq!(master_pan0, 18);
}
