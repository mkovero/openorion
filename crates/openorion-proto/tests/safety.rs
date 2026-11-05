//! Safety by construction (DESIGN.md 4.3): every request is a 320-byte
//! 0x74 get or a whitelisted 0x70 set; nothing else can be built.

mod common;

use openorion_proto::codec::{HEADER_LEN, ID_GET, ID_SET, SET_PID_WHITELIST};
use openorion_proto::decode::*;
use openorion_proto::*;

fn len_field(r: &Report) -> u32 {
    u32::from_le_bytes(r.as_bytes()[4..8].try_into().unwrap())
}

/// Every `Get` variant, every bank/mixer/type/half.
fn all_gets() -> Vec<Get> {
    let mut v = Get::read_all();
    for &t in AfxType::ALL {
        v.push(Get::AfxConfigs(t, 0));
        v.push(Get::AfxConfigs(t, 1));
        v.push(Get::AfxConfigs(t, 200));
    }
    v
}

#[test]
fn every_get_is_a_plain_0x74() {
    let gets = all_gets();
    // Every variant is represented.
    for g in &gets {
        match g {
            Get::Firmware
            | Get::CompatPassport
            | Get::Passport
            | Get::Registration
            | Get::Routing(_)
            | Get::Mixer(_)
            | Get::AfxConfigs(..)
            | Get::AfxOrder
            | Get::Reverb
            | Get::MixerLinks
            | Get::AfxLinks
            | Get::AfxInstances
            | Get::HdxDelays
            | Get::Trims => {}
        }
    }
    for g in &gets {
        let r = g.report();
        assert_eq!(r.as_bytes().len(), REPORT_LEN);
        assert_eq!(r.id(), ID_GET, "{g:?}");
        assert_eq!(len_field(&r), 16, "{g:?}");
        assert_ne!(r.ext2(), 17, "{g:?}");
        assert!(r.as_bytes()[HEADER_LEN..].iter().all(|&b| b == 0));
        assert_eq!((r.ext2(), r.ext3()), (g.ext2(), g.ext3()));
        // ext3 for 7 is a half (0/1) for EQ, else the type.
        if let Get::AfxConfigs(t, _) = g {
            if *t == AfxType::Equalizer {
                assert!(r.ext3() <= 1);
            } else {
                assert_eq!(r.ext3(), t.as_u8() as u32);
            }
        }
        let back = Get::from_ext(g.ext2(), g.ext3()).unwrap();
        assert_eq!((back.ext2(), back.ext3()), (g.ext2(), g.ext3()));
    }
    assert_eq!(Get::from_ext(17, 0), None);
    assert_eq!(Get::from_ext(17, 1), None);
}

/// One instance of every `Set` variant.
fn one_of_each() -> Vec<Set> {
    let m = MixerId::new(3).unwrap();
    let ch = MixerChannel {
        level: Atten::ZERO_DB,
        pan: Pan::CENTER,
        mute: false,
        solo: false,
        send: Atten::NEG_INF,
    };
    let rec = TrimRecord {
        control: TrimControl::All,
        levels: [TrimLevel { whole: 8, fract: 0 }; TRIM_CHANNELS],
    };
    let osc = Osc {
        freq_l: OscFreq::Hz1k,
        freq_r: OscFreq::Hz1k,
        level: OscLevel::DbM12,
        mute_l: false,
        mute_r: true,
    };
    let slot = PresetSlot::new(1).unwrap();
    let raw_fields = |t: AfxType| -> Vec<i32> {
        names::afx_fields(t)
            .unwrap()
            .iter()
            .filter(|f| f.in_set)
            .map(|f| f.default)
            .collect()
    };
    let mut v = vec![
        Set::Power(true),
        Set::SampleRate(SampleRate::R96k),
        Set::ClockSource(ClockSource::Internal),
        Set::Volume(VolumeId::Main, Atten::NEG_INF),
        Set::Mute(VolumeId::Line, true),
        Set::PresetSave(slot),
        Set::PresetRecall(slot),
        Set::Oscillator(osc),
        Set::Brightness(Brightness::new(50).unwrap()),
        Set::Trim(TrimId::LineIn, rec),
        Set::SpdifSrc(false),
        Set::Smux(SmuxId::MadiOut, false),
        Set::MeterSource(MeterBank::A, 28),
        Set::MixerChannel(m, 32, ch),
        Set::StereoLink(LinkTarget::Afx { pair: 7 }, false),
        Set::AfxEq {
            inst: 0,
            band: 0,
            value: EqBand {
                freq: 30,
                q: 71,
                gain: 0,
                ftype: EqFilter::LowShelf,
            },
        },
        Set::AfxCompressor {
            inst: 0,
            params: CompressorParams {
                attack: 1000,
                release: 10000,
                taw: 0xFFFF,
                ratio: 100,
                gain: 0,
                ctrl: 0,
                threshold: 24,
                knee: 0,
                linked: 0,
            },
        },
        Set::AfxOrder {
            chain: 0,
            slots: [None; AFX_SLOTS],
        },
        Set::AfxBypass {
            ty: AfxType::Powergate,
            inst: 11,
            enabled: true,
        },
        Set::Reverb(Reverb {
            mixer: m,
            room_size: 0,
            color: 0,
            predelay: 0,
            density: 100,
            early_ref_gain: 0,
            late_ref_delay: 0,
            richness: 0,
            reverb_time: 0,
            reverb_level: 1,
            on: false,
        }),
    ];
    for &b in OutputBank::ALL {
        v.push(Set::Routing(b, vec![Source::Mute; b.size()]));
    }
    for &t in AfxType::ALL {
        if names::afx_fields(t).is_some() {
            v.push(Set::AfxRaw {
                inst: 0,
                raw: AfxRaw {
                    ty: t,
                    fields: raw_fields(t),
                },
            });
        }
    }
    v
}

#[test]
fn every_set_is_a_whitelisted_0x70() {
    let sets = one_of_each();
    for s in &sets {
        // Exhaustive: a new variant must be added to one_of_each().
        match s {
            Set::Power(_)
            | Set::SampleRate(_)
            | Set::ClockSource(_)
            | Set::Volume(..)
            | Set::Mute(..)
            | Set::PresetSave(_)
            | Set::PresetRecall(_)
            | Set::Oscillator(_)
            | Set::Brightness(_)
            | Set::Trim(..)
            | Set::SpdifSrc(_)
            | Set::Smux(..)
            | Set::MeterSource(..)
            | Set::MixerChannel(..)
            | Set::Routing(..)
            | Set::StereoLink(..)
            | Set::AfxEq { .. }
            | Set::AfxCompressor { .. }
            | Set::AfxRaw { .. }
            | Set::AfxOrder { .. }
            | Set::AfxBypass { .. }
            | Set::Reverb(_) => {}
        }
        let r = s.report().unwrap();
        let p = s.payload().unwrap();
        assert_eq!(r.id(), ID_SET, "{s:?}");
        assert_eq!((r.ext2(), r.ext3()), (0, 0));
        assert_eq!(r.as_bytes().len(), REPORT_LEN);
        assert_eq!(len_field(&r) as usize, HEADER_LEN + p.len());
        assert_eq!(p[0] & 0x3f, s.pid());
        assert!(SET_PID_WHITELIST.contains(&s.pid()), "{s:?}");
        assert_eq!(Set::decode(r.as_bytes()).unwrap(), *s);
        // apply_set accepts it.
        let mut st = empty_state();
        apply_set(&mut st, s).unwrap();
    }
    let pids: std::collections::BTreeSet<u8> = sets.iter().map(|s| s.pid()).collect();
    let wl: std::collections::BTreeSet<u8> = SET_PID_WHITELIST.iter().copied().collect();
    assert_eq!(pids, wl);
}

#[test]
fn raw_checked_refuses_non_whitelisted() {
    // pid 1 (set_config_feature), in every framing.
    for p in [&[0x01, 0x00][..], &[0x41, 0, 0], &[0xc1, 4, 0, 0, 0, 0]] {
        assert!(matches!(
            Report::raw_checked(p),
            Err(ProtoError::Refused(_))
        ));
    }
    // 0x76 (clear_update_flag id) as byte 0 → pid 0x36; predefined AFX
    // preset (25); HDX 27..31; anything else off the list.
    for pid in 0u8..64 {
        let p = [pid | 0x40, 0, 0];
        let r = Report::raw_checked(&p);
        if SET_PID_WHITELIST.contains(&pid) {
            assert!(r.is_ok(), "pid {pid}");
            assert_eq!(r.unwrap().id(), 0x70);
        } else {
            assert!(matches!(r, Err(ProtoError::Refused(_))), "pid {pid}");
        }
    }
    assert!(Report::raw_checked(&[0x76]).is_err());
    assert!(Report::raw_checked(&[0x76, 0x00]).is_err());
    assert!(Report::raw_checked(&[0x99, 0x01, 0x00, 0x00]).is_err());
    assert!(Report::raw_checked(&[]).is_err());
    // Length must agree with the header.
    assert!(Report::raw_checked(&[0x46, 0x01]).is_err());
    assert!(Report::raw_checked(&[0xd1, 0x05, 3, 1, 0]).is_err());
    assert!(Report::raw_checked(&[0xd1, 0x05, 3, 1, 0, 2, 0x60]).is_ok());
}

#[test]
fn set_decode_refuses_foreign_reports() {
    // 0x76 report id, pid 1 payload, get id.
    for bytes in [
        common::hex("76000000 10000000 00000000 00000000"),
        common::hex("70000000 12000000 00000000 00000000 01 00"),
        common::hex("74000000 10000000 00000000 00000000"),
        // trailing junk vs. header length
        common::hex("70000000 13000000 00000000 00000000 03 01 00"),
        // non-canonical bool
        common::hex("70000000 12000000 00000000 00000000 03 02"),
    ] {
        assert!(
            Set::decode(&common::report(&bytes)).is_err(),
            "{bytes:02x?}"
        );
    }
}

#[test]
fn encoder_range_checks() {
    let bad = [
        Set::MeterSource(MeterBank::A, 29),
        Set::MeterSource(MeterBank::Mixer, 4),
        Set::MixerChannel(
            MixerId::new(0).unwrap(),
            33,
            MixerChannel {
                level: Atten::ZERO_DB,
                pan: Pan::CENTER,
                mute: false,
                solo: false,
                send: Atten::ZERO_DB,
            },
        ),
        Set::Routing(OutputBank::Monitor, vec![Source::Mute; 3]),
        Set::Routing(
            OutputBank::LineOut,
            vec![Source::Input {
                group: SourceGroup::SpdifIn,
                chan: 2,
            }],
        ),
        Set::StereoLink(LinkTarget::Afx { pair: 8 }, true),
        Set::AfxBypass {
            ty: AfxType::Powergate,
            inst: 12,
            enabled: true,
        },
        Set::AfxBypass {
            ty: AfxType::Tape,
            inst: 0,
            enabled: true,
        },
        Set::AfxRaw {
            inst: 0,
            raw: AfxRaw {
                ty: AfxType::Neve1073,
                fields: vec![11, 8, 0, 8, 0, 8, 0, 0],
            },
        },
        Set::AfxRaw {
            inst: 0,
            raw: AfxRaw {
                ty: AfxType::Equalizer,
                fields: vec![],
            },
        },
    ];
    for s in bad {
        assert!(s.report().is_err(), "{s:?}");
    }
    // AFX order with a hole.
    let mut slots = [None; AFX_SLOTS];
    slots[1] = Some(AfxSlot {
        ty: AfxType::Equalizer,
        inst: 0,
    });
    assert!(Set::AfxOrder { chain: 0, slots }.report().is_err());
}

/// Values the vendor panel cannot produce are refused (protocol-spec.md
/// sec. 6 and 10); the panel's own values pass.
#[test]
fn encoder_keeps_to_the_panel_limits() {
    let ch = |pan: u8| MixerChannel {
        level: Atten::ZERO_DB,
        pan: Pan::new(pan).unwrap(),
        mute: false,
        solo: false,
        send: Atten::NEG_INF,
    };
    let m = MixerId::new(3).unwrap();
    let trim = |whole: u8, fract: u8| {
        Set::Trim(
            TrimId::LineIn,
            TrimRecord {
                control: TrimControl::All,
                levels: [TrimLevel { whole, fract }; TRIM_CHANNELS],
            },
        )
    };
    let band = |band: u8, freq: u16, q: u16, ftype: EqFilter| Set::AfxEq {
        inst: 0,
        band,
        value: EqBand {
            freq,
            q,
            gain: 0,
            ftype,
        },
    };
    let comp = |taw: u16, ctrl: u8| Set::AfxCompressor {
        inst: 0,
        params: CompressorParams {
            attack: 10_000,
            release: 10_000,
            taw,
            ratio: 100,
            gain: 0,
            ctrl,
            threshold: 24,
            knee: 0,
            linked: 0,
        },
    };
    let mut rev = Reverb {
        mixer: MixerId::new(0).unwrap(),
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
    };
    let good = [
        Set::MixerChannel(m, 1, ch(2)),
        Set::MixerChannel(m, 2, ch(62)),
        Set::MixerChannel(m, 0, ch(0)),
        trim(0, 0),
        trim(14, 0),
        trim(13, 99),
        band(0, 30, 71, EqFilter::LowShelf),
        band(0, 800, 0, EqFilter::Hpf),
        band(2, 1000, 50, EqFilter::Peak),
        band(4, 15_000, 71, EqFilter::HighShelf),
        band(4, 20_000, 0, EqFilter::Lpf),
        comp(65535, 0),
        comp(2000, 0),
        Set::Reverb(rev),
    ];
    for s in good {
        assert!(s.report().is_ok(), "{s:?}");
    }
    rev.reverb_level = 0;
    let bad = [
        Set::MixerChannel(m, 1, ch(0)),
        Set::MixerChannel(m, 1, ch(1)),
        Set::MixerChannel(m, 2, ch(63)),
        trim(15, 0),
        trim(14, 1),
        trim(0, 100),
        band(0, 900, 71, EqFilter::LowShelf),
        band(0, 30, 71, EqFilter::Peak),
        band(1, 200, 0, EqFilter::Peak),
        band(2, 100, 71, EqFilter::Peak),
        band(3, 5000, 71, EqFilter::HighShelf),
        band(4, 300, 71, EqFilter::HighShelf),
        comp(1234, 0),
        comp(65535, 1),
        Set::Reverb(rev),
    ];
    for s in bad {
        assert!(s.report().is_err(), "{s:?}");
    }
}

#[test]
fn risk_classes() {
    let m = MixerId::new(0).unwrap();
    assert_eq!(Set::Power(true).class(), Risk::Disruptive);
    assert_eq!(Set::SampleRate(SampleRate::R48k).class(), Risk::Disruptive);
    assert_eq!(
        Set::ClockSource(ClockSource::Adat).class(),
        Risk::Disruptive
    );
    assert_eq!(
        Set::PresetRecall(PresetSlot::new(1).unwrap()).class(),
        Risk::Disruptive
    );
    assert_eq!(
        Set::PresetSave(PresetSlot::new(1).unwrap()).class(),
        Risk::Disruptive
    );
    for &b in OutputBank::ALL {
        let want = if b.is_physical() {
            Risk::Disruptive
        } else {
            Risk::Audible
        };
        assert_eq!(Set::Routing(b, vec![]).class(), want, "{b:?}");
    }
    assert_eq!(
        Set::Volume(VolumeId::Main, Atten::ZERO_DB).class(),
        Risk::Audible
    );
    assert_eq!(Set::Mute(VolumeId::Main, true).class(), Risk::Audible);
    assert_eq!(Set::MeterSource(MeterBank::Mixer, 3).class(), Risk::Silent);
    assert_eq!(
        Set::StereoLink(LinkTarget::Mixer { mixer: m, pair: 0 }, true).class(),
        Risk::Silent
    );
}

#[test]
fn parse_classifies_reports() {
    use openorion_proto::Incoming;
    assert_eq!(parse(&[0; REPORT_LEN]), Incoming::Zero);
    assert_eq!(
        parse(&common::report(&common::hex("61000000"))),
        Incoming::UnknownCmd
    );
    assert_eq!(
        parse(&common::report(&common::hex("99000000"))),
        Incoming::Other(0x99)
    );
    // A 0x73 with an undecodable oscillator byte (freq code 3).
    let mut st = common::hex("73000000 00000000 00000000 00000000");
    st.resize(16 + 5, 0);
    st[16 + 4] = 0x03;
    assert_eq!(parse(&common::report(&st)), Incoming::Other(0x73));
    let seg = common::hex("751f0000 00020000 04000000 30010000 de ad be ef");
    assert_eq!(
        parse(&common::report(&seg)),
        Incoming::Segment {
            total: 0x200,
            chunk_len: 4,
            offset: 0x130,
            data: vec![0xde, 0xad, 0xbe, 0xef]
        }
    );
    let rep = common::hex("75000000 10000000 04000000 03000000 00 20 60");
    let r = common::report(&rep);
    assert!(Get::Mixer(MixerId::new(3).unwrap()).matches(&r));
    assert!(!Get::Mixer(MixerId::new(2).unwrap()).matches(&r));
    match parse(&r) {
        Incoming::Reply { ext2, ext3, body } => {
            assert_eq!((ext2, ext3, body.len()), (4, 3, 304));
        }
        o => panic!("{o:?}"),
    }
}

#[test]
fn apply_set_behaviours() {
    let (replies, status, _) = common::load_dump("dump1.json");
    let mut st = state_from_dump(&replies, &status).unwrap();
    st.status.current_preset = 5;
    let before = st.clone();

    // Preset save/recall: left to the caller, state untouched.
    apply_set(&mut st, &Set::PresetRecall(PresetSlot::new(5).unwrap())).unwrap();
    assert_eq!(st, before);

    // Stereo link: only the flag; any set clears current_preset.
    let m3 = MixerId::new(3).unwrap();
    apply_set(
        &mut st,
        &Set::StereoLink(LinkTarget::Mixer { mixer: m3, pair: 0 }, true),
    )
    .unwrap();
    assert!(st.mixer_links[3][0]);
    assert_eq!(st.status.current_preset, 0);
    assert_eq!(st.mixers, before.mixers);

    // Status-backed fields.
    apply_set(
        &mut st,
        &Set::Volume(VolumeId::Line, Atten::new(2).unwrap()),
    )
    .unwrap();
    apply_set(&mut st, &Set::Mute(VolumeId::Main, true)).unwrap();
    apply_set(&mut st, &Set::Smux(SmuxId::MadiIn, true)).unwrap();
    apply_set(&mut st, &Set::SpdifSrc(true)).unwrap();
    apply_set(&mut st, &Set::SampleRate(SampleRate::R88k2)).unwrap();
    apply_set(&mut st, &Set::ClockSource(ClockSource::WordClock)).unwrap();
    apply_set(&mut st, &Set::Brightness(Brightness::new(25).unwrap())).unwrap();
    apply_set(&mut st, &Set::MeterSource(MeterBank::Mixer, 1)).unwrap();
    let s = &st.status;
    assert_eq!(s.line.atten.get(), 2);
    assert!(s.main.mute);
    assert!(s.smux_in && s.spdif_src);
    assert_eq!(s.sample_rate, Some(SampleRate::R88k2));
    assert_eq!(s.clock_source, Some(ClockSource::WordClock));
    assert_eq!(s.brightness, 25);
    assert_eq!(s.meter_sources[2], 1);

    // Routing pads short lists with (0,0) = LINE IN 1, like the wire.
    apply_set(
        &mut st,
        &Set::Routing(OutputBank::Monitor, vec![Source::Mute]),
    )
    .unwrap();
    assert_eq!(
        st.routing[1].sources,
        vec![
            Source::Mute,
            Source::Input {
                group: SourceGroup::LineIn,
                chan: 0
            }
        ]
    );

    // AFX: insert, then remove from the chain → enabled = false.
    let slot = AfxSlot {
        ty: AfxType::Compressor,
        inst: 15,
    };
    let bypass = |enabled| Set::AfxBypass {
        ty: AfxType::Compressor,
        inst: 15,
        enabled,
    };
    apply_set(&mut st, &bypass(true)).unwrap();
    let mut slots = [None; AFX_SLOTS];
    slots[0] = Some(slot);
    apply_set(&mut st, &Set::AfxOrder { chain: 15, slots }).unwrap();
    assert_eq!(st.afx.order[15][0], Some(slot));
    assert!(st.afx.configs[&AfxType::Compressor][15].enabled);
    apply_set(
        &mut st,
        &Set::AfxOrder {
            chain: 15,
            slots: [None; AFX_SLOTS],
        },
    )
    .unwrap();
    assert!(!st.afx.configs[&AfxType::Compressor][15].enabled);
    apply_set(&mut st, &bypass(true)).unwrap();
    assert!(st.afx.configs[&AfxType::Compressor][15].enabled);

    // Raw AFX: the set carries every field.
    apply_set(
        &mut st,
        &Set::AfxRaw {
            inst: 3,
            raw: AfxRaw {
                ty: AfxType::Powergate,
                fields: vec![100, 5, 10, 20, 30, -1],
            },
        },
    )
    .unwrap();
    match &st.afx.configs[&AfxType::Powergate][3].params {
        AfxParams::Raw(r) => assert_eq!(r.fields, vec![100, 5, 10, 20, 30, -1]),
        p => panic!("{p:?}"),
    }

    // Reverb goes to the slot of its mixer.
    let mut rv = st.reverb[0].unwrap();
    rv.on = true;
    apply_set(&mut st, &Set::Reverb(rv)).unwrap();
    assert!(st.reverb[0].unwrap().on);

    // Out-of-range sets change nothing.
    let snapshot = st.clone();
    assert!(apply_set(&mut st, &Set::MeterSource(MeterBank::A, 99)).is_err());
    assert_eq!(st, snapshot);
}

/// A clock-source change drops the old source's lock flag, so a client
/// waiting for lock cannot mistake the old lock for the new one.
#[test]
fn clock_source_change_clears_lock() {
    let (replies, status, _) = common::load_dump("dump1.json");
    let mut st = state_from_dump(&replies, &status).unwrap();
    st.status.clock_source = Some(ClockSource::WordClock);
    st.status.clock.locked = true;
    apply_set(&mut st, &Set::ClockSource(ClockSource::WordClock)).unwrap();
    assert!(st.status.clock.locked, "same source keeps its lock");
    apply_set(&mut st, &Set::ClockSource(ClockSource::Spdif)).unwrap();
    assert!(!st.status.clock.locked);
    assert!(!st.status.clock_locked());
}
