//! Proptest round-trips (DESIGN.md 4.4): decode(encode(x)) == x for every
//! set, for every get reply over a whole device state, and for the
//! cyclic reports.

use std::collections::BTreeMap;

use openorion_proto::codec::SET_PID_WHITELIST;
use openorion_proto::decode::*;
use openorion_proto::names::{afx_fields, afx_max_instances};
use openorion_proto::*;
use proptest::collection::{btree_map, vec};
use proptest::option;
use proptest::prelude::*;
use proptest::sample::select;
use proptest::strategy::Union;

// ---------------------------------------------------------------------------
// Value strategies

/// Fixed-size array via a Vec (proptest's array strategies keep whole
/// nested value trees on the stack, which overflows for [[_; 8]; 16]).
fn arr<S, const N: usize>(s: S) -> impl Strategy<Value = [S::Value; N]>
where
    S: Strategy,
    S::Value: std::fmt::Debug,
{
    vec(s, N).prop_map(|v| v.try_into().unwrap())
}

fn atten() -> impl Strategy<Value = Atten> {
    (0u8..=96).prop_map(|v| Atten::new(v).unwrap())
}

fn pan() -> impl Strategy<Value = Pan> {
    (0u8..=63).prop_map(|v| Pan::new(v).unwrap())
}

fn mixer_id() -> impl Strategy<Value = MixerId> {
    (0u8..4).prop_map(|v| MixerId::new(v).unwrap())
}

fn channel() -> impl Strategy<Value = MixerChannel> {
    (atten(), pan(), any::<bool>(), any::<bool>(), atten()).prop_map(
        |(level, pan, mute, solo, send)| MixerChannel {
            level,
            pan,
            mute,
            solo,
            send,
        },
    )
}

/// A channel the encoder accepts: pan on the panel's knob (2..=62).
fn set_channel() -> impl Strategy<Value = MixerChannel> {
    (channel(), Pan::UI_MIN..=Pan::UI_MAX).prop_map(|(c, p)| MixerChannel {
        pan: Pan::new(p).unwrap(),
        ..c
    })
}

fn osc() -> impl Strategy<Value = Osc> {
    (
        select(OscFreq::ALL),
        select(OscFreq::ALL),
        select(OscLevel::ALL),
        any::<bool>(),
        any::<bool>(),
    )
        .prop_map(|(freq_l, freq_r, level, mute_l, mute_r)| Osc {
            freq_l,
            freq_r,
            level,
            mute_l,
            mute_r,
        })
}

fn source() -> impl Strategy<Value = Source> {
    prop_oneof![
        Just(Source::Mute),
        (select(SourceGroup::ALL), any::<u8>()).prop_map(|(group, c)| Source::Input {
            group,
            chan: c % group.size(),
        }),
    ]
}

fn trim_record() -> impl Strategy<Value = TrimRecord> {
    (
        select(TrimControl::ALL),
        arr::<_, 32>(
            (any::<u8>(), any::<u8>()).prop_map(|(whole, fract)| TrimLevel { whole, fract }),
        ),
    )
        .prop_map(|(control, levels)| TrimRecord { control, levels })
}

/// Trims the encoder accepts: 10..=24 dBu.
fn set_trim_record() -> impl Strategy<Value = TrimRecord> {
    (
        select(TrimControl::ALL),
        arr::<_, 32>((0u16..=1400).prop_map(|h| TrimLevel {
            whole: (h / 100) as u8,
            fract: (h % 100) as u8,
        })),
    )
        .prop_map(|(control, levels)| TrimRecord { control, levels })
}

/// Reverb within the UI ranges the encoder checks.
fn reverb() -> impl Strategy<Value = Reverb> {
    (
        mixer_id(),
        (0u8..=100, 0u8..=100, 0u8..=150, any::<u8>()),
        (0u8..=100, 0u8..=150, 0u8..=100, 0u8..=100, 1u8..=100),
        any::<bool>(),
    )
        .prop_map(
            |(mixer, (room_size, color, predelay, density), (e, l, ri, t, lv), on)| Reverb {
                mixer,
                room_size,
                color,
                predelay,
                density,
                early_ref_gain: e,
                late_ref_delay: l,
                richness: ri,
                reverb_time: t,
                reverb_level: lv,
                on,
            },
        )
}

/// An EQ band within the panel's limits for `band`.
fn eq_band(band: u8) -> impl Strategy<Value = EqBand> {
    let (lo, hi, types) = match band {
        0 => (20u16, 800u16, vec![EqFilter::LowShelf, EqFilter::Hpf]),
        1 => (20, 800, vec![EqFilter::Peak]),
        2 => (125, 8000, vec![EqFilter::Peak]),
        3 => (400, 20_000, vec![EqFilter::Peak]),
        _ => (400, 20_000, vec![EqFilter::HighShelf, EqFilter::Lpf]),
    };
    let q_min = if band == 0 || band == 4 { 0u16 } else { 50 };
    (lo..=hi, q_min..=1800, -2400i16..=1200, select(types)).prop_map(|(freq, q, gain, ftype)| {
        EqBand {
            freq,
            q,
            gain,
            ftype,
        }
    })
}

/// Compressor settings the encoder accepts (panel detector list, ctrl 0).
fn set_compressor() -> impl Strategy<Value = CompressorParams> {
    (
        compressor(),
        select(vec![65535u16, 2000, 5000, 10000, 15000, 20000]),
    )
        .prop_map(|(p, taw)| CompressorParams { taw, ctrl: 0, ..p })
}

fn compressor() -> impl Strategy<Value = CompressorParams> {
    (
        (100u32..=150_000, 1000u32..=600_000, any::<u16>()),
        (100u16..=2000, 0u16..=2400, any::<u8>()),
        (0u8..=80, 0u8..=30, 0u8..=1),
    )
        .prop_map(
            |((attack, release, taw), (ratio, gain, ctrl), (threshold, knee, linked))| {
                CompressorParams {
                    attack,
                    release,
                    taw,
                    ratio,
                    gain,
                    ctrl,
                    threshold,
                    knee,
                    linked,
                }
            },
        )
}

fn raw_types() -> Vec<AfxType> {
    AfxType::ALL
        .iter()
        .copied()
        .filter(|&t| afx_fields(t).is_some())
        .collect()
}

/// Raw values within each field's range; `set_only` drops get-only fields.
fn raw_params(ty: AfxType, set_only: bool) -> impl Strategy<Value = AfxRaw> {
    let ranges: Vec<_> = afx_fields(ty)
        .unwrap()
        .iter()
        .filter(|f| f.in_set || !set_only)
        .map(|f| f.min..=f.max)
        .collect();
    ranges.prop_map(move |fields| AfxRaw { ty, fields })
}

fn orion_slot() -> impl Strategy<Value = AfxSlot> {
    let types: Vec<AfxType> = AfxType::ALL
        .iter()
        .copied()
        .filter(|t| t.on_orion())
        .collect();
    (select(types), any::<u8>()).prop_map(|(ty, i)| AfxSlot {
        ty,
        inst: i % afx_max_instances(ty),
    })
}

/// A valid chain: packed, distinct.
fn chain_slots() -> impl Strategy<Value = [Option<AfxSlot>; AFX_SLOTS]> {
    vec(orion_slot(), 0..=AFX_SLOTS).prop_map(|v| {
        let mut out = [None; AFX_SLOTS];
        let mut n = 0;
        for s in v {
            if !out[..n].contains(&Some(s)) {
                out[n] = Some(s);
                n += 1;
            }
        }
        out
    })
}

// ---------------------------------------------------------------------------
// Sets

fn any_set() -> impl Strategy<Value = Set> {
    let inst16 = 0u8..16;
    let arms: Vec<BoxedStrategy<Set>> = vec![
        any::<bool>().prop_map(Set::Power).boxed(),
        select(SampleRate::ALL).prop_map(Set::SampleRate).boxed(),
        select(ClockSource::ALL).prop_map(Set::ClockSource).boxed(),
        (select(VolumeId::ALL), atten())
            .prop_map(|(i, a)| Set::Volume(i, a))
            .boxed(),
        (select(VolumeId::ALL), any::<bool>())
            .prop_map(|(i, m)| Set::Mute(i, m))
            .boxed(),
        (1u8..=5)
            .prop_map(|s| Set::PresetSave(PresetSlot::new(s).unwrap()))
            .boxed(),
        (1u8..=5)
            .prop_map(|s| Set::PresetRecall(PresetSlot::new(s).unwrap()))
            .boxed(),
        osc().prop_map(Set::Oscillator).boxed(),
        (0u8..=100)
            .prop_map(|b| Set::Brightness(Brightness::new(b).unwrap()))
            .boxed(),
        (select(TrimId::ALL), set_trim_record())
            .prop_map(|(i, r)| Set::Trim(i, r))
            .boxed(),
        any::<bool>().prop_map(Set::SpdifSrc).boxed(),
        (select(SmuxId::ALL), any::<bool>())
            .prop_map(|(i, o)| Set::Smux(i, o))
            .boxed(),
        (select(MeterBank::ALL), any::<u8>())
            .prop_map(|(b, s)| {
                let max = if b == MeterBank::Mixer { 4 } else { 29 };
                Set::MeterSource(b, s % max)
            })
            .boxed(),
        (mixer_id(), 0u8..=32, set_channel())
            .prop_map(|(m, ch, c)| Set::MixerChannel(m, ch, c))
            .boxed(),
        select(OutputBank::ALL)
            .prop_flat_map(|b| vec(source(), b.size()).prop_map(move |v| Set::Routing(b, v)))
            .boxed(),
        (mixer_id(), 0u8..16, any::<bool>())
            .prop_map(|(mixer, pair, l)| Set::StereoLink(LinkTarget::Mixer { mixer, pair }, l))
            .boxed(),
        (0u8..8, any::<bool>())
            .prop_map(|(pair, l)| Set::StereoLink(LinkTarget::Afx { pair }, l))
            .boxed(),
        (inst16.clone(), 0u8..5)
            .prop_flat_map(|(inst, band)| eq_band(band).prop_map(move |v| (inst, band, v)))
            .prop_map(|(inst, band, value)| Set::AfxEq { inst, band, value })
            .boxed(),
        (inst16.clone(), set_compressor())
            .prop_map(|(inst, params)| Set::AfxCompressor { inst, params })
            .boxed(),
        select(raw_types())
            .prop_flat_map(|t| (0..afx_max_instances(t), raw_params(t, true)))
            .prop_map(|(inst, raw)| Set::AfxRaw { inst, raw })
            .boxed(),
        (0u8..16, chain_slots())
            .prop_map(|(chain, slots)| Set::AfxOrder { chain, slots })
            .boxed(),
        (orion_slot(), any::<bool>())
            .prop_map(|(s, enabled)| Set::AfxBypass {
                ty: s.ty,
                inst: s.inst,
                enabled,
            })
            .boxed(),
        reverb().prop_map(Set::Reverb).boxed(),
    ];
    Union::new(arms)
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(2000))]

    #[test]
    fn set_roundtrip(set in any_set()) {
        let r = set.report().unwrap();
        let p = set.payload().unwrap();
        prop_assert_eq!(r.id(), 0x70);
        prop_assert!(SET_PID_WHITELIST.contains(&set.pid()));
        prop_assert_eq!(
            u32::from_le_bytes(r.as_bytes()[4..8].try_into().unwrap()) as usize,
            16 + p.len()
        );
        prop_assert_eq!(Report::raw_checked(&p).unwrap(), r.clone());
        prop_assert_eq!(Set::decode(r.as_bytes()).unwrap(), set.clone());
        // parse() never mistakes an outgoing set for something it decodes.
        prop_assert_eq!(parse(r.as_bytes()), Incoming::Other(0x70));
    }

    /// Applying a set and reading the affected state back through the
    /// reply codec gives the same state (cache == what the device says).
    #[test]
    fn apply_set_survives_reply_roundtrip(set in any_set()) {
        let mut st = empty_state();
        apply_set(&mut st, &set).unwrap();
        let back = reread(&st);
        prop_assert_eq!(back, st);
    }
}

// ---------------------------------------------------------------------------
// Whole-state reply round trip

fn info() -> impl Strategy<Value = DeviceInfo> {
    (
        any::<(u8, u8)>(),
        "[A-Za-z0-9 .]{0,20}",
        "[0-9]{0,16}",
        "[0-9.]{0,8}",
        any::<u32>(),
        any::<bool>(),
    )
        .prop_map(
            |((major, minor), name, serial, hw, birth, registered)| DeviceInfo {
                firmware: Version { major, minor },
                passport: Passport {
                    // trailing spaces would survive; NULs cannot appear
                    name,
                    serial,
                    hw,
                    birth,
                },
                registered,
            },
        )
}

fn status() -> impl Strategy<Value = Status> {
    (
        (0u8..8, any::<bool>(), any::<bool>(), 0u8..4, any::<bool>()),
        any::<[bool; 8]>(),
        (
            option::of(select(SampleRate::ALL)),
            0u8..16,
            option::of(select(ClockSource::ALL)),
        ),
        (osc(), 0u32..(1 << 24)),
        (any::<[bool; 3]>(), any::<[u8; 3]>()),
        (atten(), any::<bool>(), atten(), any::<bool>(), any::<u8>()),
        any::<[u8; 4]>(),
    )
        .prop_map(
            |(
                (current_preset, power_on, muted_bit, usb_mode, device_updated),
                f,
                (sample_rate, pull_mode, clock_source),
                (oscillator, external_clock_hz),
                (sm, [brightness, adat_channels, madi_channels]),
                (ma, mm, la, lm, monitor_trim),
                meter_sources,
            )| Status {
                current_preset,
                power_on,
                muted_bit,
                usb_mode,
                device_updated,
                clock: ClockFlags {
                    oven: f[0],
                    locked: f[1],
                    atomic: f[2],
                    usb: f[3],
                    madi: f[4],
                    adat: f[5],
                    spdif: f[6],
                    dlink: f[7],
                },
                sample_rate,
                pull_mode,
                clock_source,
                oscillator,
                external_clock_hz,
                smux_in: sm[0],
                smux_out: sm[1],
                spdif_src: sm[2],
                brightness,
                adat_channels,
                madi_channels,
                main: VolumeState {
                    atten: ma,
                    mute: mm,
                },
                line: VolumeState {
                    atten: la,
                    mute: lm,
                },
                monitor_trim,
                meter_sources,
            },
        )
}

fn meters() -> impl Strategy<Value = Meters> {
    (
        arr::<_, 32>(any::<u8>()),
        arr::<_, 32>(any::<u8>()),
        arr::<_, 32>(any::<u8>()),
        any::<[u8; 2]>(),
        any::<[u8; 2]>(),
    )
        .prop_map(|(bank_a, bank_b, mixer, reverb_in, reverb_out)| Meters {
            bank_a,
            bank_b,
            mixer,
            reverb_in,
            reverb_out,
        })
}

fn configs(ty: AfxType) -> BoxedStrategy<Vec<AfxConfig>> {
    let params: BoxedStrategy<AfxParams> = match ty {
        AfxType::Equalizer => uniform5_eq().prop_map(AfxParams::Eq).boxed(),
        AfxType::Compressor => compressor().prop_map(AfxParams::Compressor).boxed(),
        _ => raw_params(ty, false).prop_map(AfxParams::Raw).boxed(),
    };
    vec(
        (any::<bool>(), params).prop_map(|(enabled, params)| AfxConfig { enabled, params }),
        16,
    )
    .boxed()
}

fn uniform5_eq() -> impl Strategy<Value = EqParams> {
    // Replies are not range-checked: any wire values round-trip.
    let band = (
        any::<u16>(),
        any::<u16>(),
        any::<i16>(),
        select(EqFilter::ALL),
    )
        .prop_map(|(freq, q, gain, ftype)| EqBand {
            freq,
            q,
            gain,
            ftype,
        });
    arr::<_, 5>(band).prop_map(|bands| EqParams { bands })
}

fn afx_state() -> impl Strategy<Value = AfxState> {
    let any_slot = (select(AfxType::ALL), any::<u8>()).prop_map(|(ty, inst)| AfxSlot { ty, inst });
    (
        arr::<_, 16>(arr::<_, 8>(option::of(any_slot))),
        arr::<_, 8>(any::<bool>()),
        btree_map(select(AfxType::ALL), any::<u8>(), 0..AfxType::ALL.len()),
        select(raw_types()),
        select(raw_types()),
    )
        .prop_flat_map(|(order, links, instances, r1, r2)| {
            (
                Just((order, links, instances)),
                configs(AfxType::Equalizer),
                configs(AfxType::Compressor),
                Just(r1),
                configs(r1),
                Just(r2),
                configs(r2),
            )
        })
        .prop_map(|((order, links, instances), eq, comp, r1, c1, r2, c2)| {
            let mut configs = BTreeMap::new();
            configs.insert(AfxType::Equalizer, eq);
            configs.insert(AfxType::Compressor, comp);
            configs.insert(r1, c1);
            configs.insert(r2, c2);
            AfxState {
                order,
                links,
                instances,
                configs,
            }
        })
}

fn device_state() -> impl Strategy<Value = DeviceState> {
    let routing: Vec<BoxedStrategy<RoutingBank>> = OutputBank::ALL
        .iter()
        .map(|&bank| {
            vec(source(), bank.size())
                .prop_map(move |sources| RoutingBank { bank, sources })
                .boxed()
        })
        .collect();
    let mixer = (channel(), arr::<_, 32>(channel()))
        .prop_map(|(master, channels)| Mixer { master, channels });
    (
        info(),
        status(),
        routing,
        arr::<_, 4>(mixer),
        arr::<_, 4>(arr::<_, 16>(any::<bool>())),
        (trim_record(), trim_record()),
        arr::<_, 4>(option::of(reverb_any())),
        afx_state(),
        arr::<_, 2>(arr::<_, 32>(any::<u8>())),
    )
        .prop_map(
            |(info, status, routing, mixers, mixer_links, (lo, li), reverb, afx, hdx_delays)| {
                DeviceState {
                    info,
                    status,
                    routing,
                    mixers,
                    mixer_links,
                    trims: Trims {
                        line_out: lo,
                        line_in: li,
                    },
                    reverb,
                    afx,
                    hdx_delays,
                }
            },
        )
}

/// Replies carry any byte in the reverb fields.
fn reverb_any() -> impl Strategy<Value = Reverb> {
    (mixer_id(), any::<[u8; 9]>(), any::<bool>()).prop_map(|(mixer, b, on)| Reverb {
        mixer,
        room_size: b[0],
        color: b[1],
        predelay: b[2],
        density: b[3],
        early_ref_gain: b[4],
        late_ref_delay: b[5],
        richness: b[6],
        reverb_time: b[7],
        reverb_level: b[8],
        on,
    })
}

/// Encode every reply (and the status) from `st`, parse the reports, and
/// rebuild a state from scratch.
fn reread(st: &DeviceState) -> DeviceState {
    let mut out = empty_state();
    let gets: Vec<Get> = Get::read_all()
        .into_iter()
        .filter(|g| match g {
            Get::AfxConfigs(t, _) => st.afx.configs.contains_key(t),
            _ => true,
        })
        .collect();
    for g in gets {
        let r = encode_reply(&g, st).unwrap();
        assert!(g.matches(&r), "{g:?}");
        match parse(&r) {
            Incoming::Reply { ext2, ext3, body } => {
                let g2 = Get::from_ext(ext2, ext3).unwrap();
                apply_reply(&mut out, decode_reply(&g2, &body).unwrap());
            }
            o => panic!("{g:?}: {o:?}"),
        }
    }
    match parse(&encode_status(&st.status, &Meters::default())) {
        Incoming::Status(s, m) => {
            out.status = *s;
            assert_eq!(*m, Meters::default());
        }
        o => panic!("{o:?}"),
    }
    out
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(300))]

    #[test]
    fn state_roundtrip(st in device_state()) {
        prop_assert_eq!(reread(&st), st);
    }

    #[test]
    fn status_roundtrip(s in status(), m in meters()) {
        let r = encode_status(&s, &m);
        prop_assert_eq!(parse(&r), Incoming::Status(Box::new(s), Box::new(m)));
    }

    #[test]
    fn afx_meters_roundtrip(peak in arr::<_, 16>(arr::<_, 8>(any::<u8>())),
                            gain_reduction in arr::<_, 16>(arr::<_, 8>(any::<u8>()))) {
        let m = AfxMeters { peak, gain_reduction };
        let r = encode_afx_meters(&m);
        prop_assert_eq!(parse(&r), Incoming::AfxMeters(Box::new(m)));
    }
}
