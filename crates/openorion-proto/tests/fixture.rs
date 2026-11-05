//! Fixture tests: `state_from_dump(fixtures/dump1.json)` reproduces
//! `fixtures/dump1.decoded.txt` (an independent decoder's output).
//! The state is rendered in the Python script's format and compared line
//! by line.

mod common;

use openorion_proto::decode::*;
use openorion_proto::names::*;
use openorion_proto::*;

fn dump1() -> DeviceState {
    let (replies, status, _) = common::load_dump("dump1.json");
    state_from_dump(&replies, &status).unwrap()
}

fn group_and_chan(s: Source) -> (u8, u8) {
    match s {
        Source::Mute => (SOURCE_GROUP_MUTE, 0),
        Source::Input { group, chan } => (group.as_u8(), chan),
    }
}

fn group_name(g: u8) -> &'static str {
    source_group_name(SourceGroup::try_from(g).unwrap())
}

/// oriondecode.py `runs`.
fn runs(entries: &[Source]) -> Vec<String> {
    let e: Vec<(u8, u8)> = entries.iter().map(|&s| group_and_chan(s)).collect();
    let mut out = Vec::new();
    let mut i = 0;
    while i < e.len() {
        let (g, c) = e[i];
        let mut j = i;
        while j + 1 < e.len()
            && e[j + 1].0 == g
            && (g == SOURCE_GROUP_MUTE || e[j + 1].1 as u16 == e[j].1 as u16 + 1)
        {
            j += 1;
        }
        if g != SOURCE_GROUP_MUTE {
            let dst = if i == j {
                format!("{}", i + 1)
            } else {
                format!("{}-{}", i + 1, j + 1)
            };
            let src = if i == j {
                source_name(entries[i])
            } else {
                format!(
                    "{} {}-{}",
                    group_name(g),
                    c as usize + 1,
                    c as usize + 1 + j - i
                )
            };
            out.push(format!("{dst} <- {src}"));
        }
        i = j + 1;
    }
    out
}

fn b(v: bool) -> u8 {
    v as u8
}

/// Everything oriondecode.py prints, except lines built from status bytes
/// the model does not carry (spdif_gains/usb, hdx_control/pm_bank_src).
fn render(st: &DeviceState) -> Vec<String> {
    let mut o = Vec::new();
    let i = &st.info;
    o.push(format!(
        "firmware {}.{:02}",
        i.firmware.major, i.firmware.minor
    ));
    o.push(format!(
        "passport name={} serial={} hw={} birth={}",
        i.passport.name, i.passport.serial, i.passport.hw, i.passport.birth
    ));
    o.push(format!("registered={}", b(i.registered)));
    let s = &st.status;
    o.push("== status (0x73)".into());
    o.push(format!(
        "  preset={} power={} muted={} usb_mode={} updated={}",
        s.current_preset,
        b(s.power_on),
        b(s.muted_bit),
        s.usb_mode,
        b(s.device_updated)
    ));
    let c = &s.clock;
    let flags: Vec<&str> = [
        (c.oven, "oven"),
        (c.locked, "locked"),
        (c.atomic, "atomic"),
        (c.usb, "usb"),
        (c.madi, "madi"),
        (c.adat, "adat"),
        (c.spdif, "spdif"),
        (c.dlink, "dlink"),
    ]
    .iter()
    .filter(|f| f.0)
    .map(|f| f.1)
    .collect();
    o.push(format!("  flags: {}", flags.join(" ")));
    o.push(format!(
        "  rate={} source={} ext_freq={} Hz",
        sample_rate_name(s.sample_rate.unwrap()),
        clock_source_name(s.clock_source.unwrap()),
        s.external_clock_hz
    ));
    let osc = &s.oscillator;
    o.push(format!(
        "  osc freq_l={} freq_r={} level={} mute_l={} mute_r={}",
        osc.freq_l.as_u8(),
        osc.freq_r.as_u8(),
        osc.level.as_u8(),
        b(osc.mute_l),
        b(osc.mute_r)
    ));
    o.push(format!(
        "  smux_in={} smux_out={} spdif_src={} brightness={} adat_ch={} madi_ch={}",
        b(s.smux_in),
        b(s.smux_out),
        b(s.spdif_src),
        s.brightness,
        s.adat_channels,
        s.madi_channels
    ));
    o.push(format!(
        "  main vol=-{} dB mute={}  line vol=-{} dB mute={}  monitor_trim={}",
        s.main.atten.get(),
        b(s.main.mute),
        s.line.atten.get(),
        b(s.line.mute),
        s.monitor_trim
    ));
    o.push("== routing".into());
    for rb in &st.routing {
        let r = runs(&rb.sources);
        let body = if r.is_empty() {
            "(all muted)".to_string()
        } else {
            r.join("; ")
        };
        o.push(format!("  {}: {body}", output_bank_name(rb.bank)));
    }
    for (m, mx) in st.mixers.iter().enumerate() {
        o.push(format!("== mixer {}", m + 1));
        for ch in 0..=32usize {
            let c = if ch == 0 {
                mx.master
            } else {
                mx.channels[ch - 1]
            };
            if !(ch == 0 || c.level.get() < 95 || c.mute || c.solo) {
                continue;
            }
            let lab = if ch == 0 {
                "master".to_string()
            } else {
                format!("ch{ch:<2}")
            };
            let lk = if ch == 0 {
                String::new()
            } else {
                format!(" link={}", b(st.mixer_links[m][(ch - 1) / 2]))
            };
            o.push(format!(
                "  {lab} level=-{} pan={:+} mute={} solo={} send={}{lk}",
                c.level.get(),
                c.pan.display(),
                b(c.mute),
                b(c.solo),
                c.send.get()
            ));
        }
    }
    o.push("== reverb".into());
    for r in st.reverb.iter().flatten() {
        o.push(format!(
            "  mixer_id={} room_size={} color={} predelay={} density={} early_ref_gain={} \
             late_ref_delay={} richness={} reverb_time={} reverb_level={} on={}",
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
            b(r.on)
        ));
    }
    o.push("== trims".into());
    for (name, t) in [
        ("LINE_OUT", &st.trims.line_out),
        ("LINE_IN", &st.trims.line_in),
    ] {
        let dbu: Vec<String> = t
            .levels
            .iter()
            .map(|l| (24 - l.whole as i32).to_string())
            .collect();
        assert!(t.levels.iter().all(|l| l.fract == 0));
        let ctl = match t.control {
            TrimControl::All => "ALL",
            TrimControl::Individual => "INDIVIDUAL",
        };
        o.push(format!("  {name} control={ctl} dBu=[{}]", dbu.join(", ")));
    }
    // instances: compared separately (types missing from AfxType)
    let used: Vec<String> = st
        .afx
        .order
        .iter()
        .enumerate()
        .flat_map(|(ch, slots)| {
            slots.iter().enumerate().filter_map(move |(si, s)| {
                s.map(|s| format!("({ch}, {si}, {}, {})", s.ty.as_u8(), s.inst))
            })
        })
        .collect();
    o.push(format!(
        "== afx order {}",
        if used.is_empty() {
            "(empty)".to_string()
        } else {
            format!("[{}]", used.join(", "))
        }
    ));
    let list = |v: &[u8]| {
        format!(
            "[{}]",
            v.iter()
                .map(|x| x.to_string())
                .collect::<Vec<_>>()
                .join(", ")
        )
    };
    let links: Vec<u8> = st.afx.links.iter().map(|&l| l as u8).collect();
    o.push(format!("== afx links {}", list(&links)));
    o.push(format!(
        "== hdx delays {} {}",
        list(&st.hdx_delays[0]),
        list(&st.hdx_delays[1])
    ));
    o
}

#[test]
fn dump1_matches_oriondecode() {
    let st = dump1();
    let text = std::fs::read_to_string(common::fixture("dump1.decoded.txt")).unwrap();
    let want: Vec<&str> = text
        .lines()
        .filter(|l| {
            !l.starts_with("  spdif_gains=")
                && !l.starts_with("  hdx_control=")
                && !l.starts_with("== afx instances")
        })
        .collect();
    let got = render(&st);
    for (i, (g, w)) in got.iter().zip(&want).enumerate() {
        assert_eq!(g, w, "line {i}");
    }
    assert_eq!(got.len(), want.len());
}

#[test]
fn dump1_afx_instances() {
    let st = dump1();
    let text = std::fs::read_to_string(common::fixture("dump1.decoded.txt")).unwrap();
    let line = text
        .lines()
        .find(|l| l.starts_with("== afx instances"))
        .unwrap();
    let mut n_known = 0;
    for tok in line
        .split_whitespace()
        .filter(|t| t.contains(':') && !t.starts_with('('))
    {
        let (t, c) = tok.split_once(':').unwrap();
        let (t, c): (u8, u8) = (t.parse().unwrap(), c.parse().unwrap());
        match AfxType::try_from(t) {
            Ok(ty) => {
                assert_eq!(st.afx.instances.get(&ty), Some(&c), "{ty:?}");
                n_known += 1;
            }
            // 10, 40, 49 are gaps in the vendor enum; skipped.
            Err(_) => assert!([10, 40, 49].contains(&t) && c == 0),
        }
    }
    assert_eq!(st.afx.instances.len(), n_known);
    assert_eq!(st.afx.instances[&AfxType::Powergate], 12);
}

#[test]
fn dump1_key_values() {
    let st = dump1();
    assert_eq!(
        st.info.firmware,
        Version {
            major: 1,
            minor: 50
        }
    );
    assert!(st.info.registered);
    assert_eq!(st.status.meter_sources, [7, 22, 3, 0]);
    assert_eq!(st.status.sample_rate, Some(SampleRate::R96k));
    assert_eq!(st.status.clock_source, Some(ClockSource::Internal));
    assert_eq!(st.status.external_clock_hz, 96_000);
    let ch1 = st.mixers[0].channels[0];
    assert_eq!(ch1.pan.display(), -30);
    assert_eq!(ch1.send.get(), 95);
    assert!(st.mixer_links[0][0]);
    let monitor = &st.routing[OutputBank::Monitor.as_u8() as usize];
    assert_eq!(monitor.bank, OutputBank::Monitor);
    assert_eq!(
        monitor.sources,
        vec![
            Source::Input {
                group: SourceGroup::Mix1,
                chan: 0
            },
            Source::Input {
                group: SourceGroup::Mix1,
                chan: 1
            }
        ]
    );
    assert_eq!(st.trims.line_in.levels[19].dbu(), 10.0);
    assert_eq!(st.reverb[0].unwrap().room_size, 68);
    assert!(st.reverb[1..].iter().all(|r| r.is_none()));
    assert!(st.afx.configs.is_empty());
}

#[test]
fn dump1_status_and_meters_parse() {
    let (_, status, afx) = common::load_dump("dump1.json");
    let r = common::report(&status);
    match parse(&r) {
        Incoming::Status(s, m) => {
            assert_eq!(*s, dump1().status);
            assert_eq!(&m.bank_a[..4], &[22, 21, 64, 71]);
            assert_eq!(m.reverb_in, [96, 96]);
        }
        other => panic!("{other:?}"),
    }
    match parse(&common::report(&afx)) {
        Incoming::AfxMeters(m) => assert_eq!(m.peak[0][0], 0x60),
        other => panic!("{other:?}"),
    }
    // Re-encoding the decoded status reproduces the bytes the model
    // carries (the rest of the report is not modelled).
    let (s, m) = decode_status(&r[16..]).unwrap();
    let re = encode_status(&s, &m);
    assert_eq!(decode_status(&re[16..]).unwrap(), (s, m));
    assert_eq!(&re[..16 + 15], &r[..16 + 15]);
}

#[test]
fn dump1_replies_reencode_identically() {
    // encode_reply(state) reproduces every captured reply byte for byte.
    let (replies, status, _) = common::load_dump("dump1.json");
    let st = state_from_dump(&replies, &status).unwrap();
    for (&(e2, e3), rep) in &replies {
        let get = Get::from_ext(e2, e3).unwrap();
        let enc = encode_reply(&get, &st).unwrap();
        // AFX instances: the vendor enum gaps (10, 40, 49) are dropped.
        if get == Get::AfxInstances {
            continue;
        }
        assert_eq!(&enc[..], &rep[..], "{get:?}");
        assert!(get.matches(&enc));
    }
}

#[test]
fn afxbase_is_factory_state() {
    let (replies, _, _) = common::load_dump("afxbase.json");
    let st = state_from_dump(&replies, &[]).unwrap();
    let eq = &st.afx.configs[&AfxType::Equalizer];
    assert_eq!(eq.len(), 16);
    let dflt = default_afx_config(AfxType::Equalizer).unwrap();
    assert!(eq.iter().all(|c| *c == dflt), "{eq:?}");
    let comp = &st.afx.configs[&AfxType::Compressor];
    assert_eq!(comp.len(), 16);
    let dflt = default_afx_config(AfxType::Compressor).unwrap();
    assert!(comp.iter().all(|c| *c == dflt), "{comp:?}");
    assert!(st.afx.order.iter().flatten().all(|s| s.is_none()));
    for (&(e2, e3), rep) in &replies {
        let get = Get::from_ext(e2, e3).unwrap();
        assert_eq!(&encode_reply(&get, &st).unwrap()[..], &rep[..], "{get:?}");
    }
}
