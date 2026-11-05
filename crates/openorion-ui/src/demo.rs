//! A hand-built `DeviceState` modeled on `fixtures/dump1.decoded.txt`, plus
//! JSON load/save. Used when `openorion-ui` runs without `--state`.
//!
//! Differences from dump1, to give every tab something to show: AFX chain 1
//! holds a ClearQ and a PowerFFC (dump1's chains are empty), and EQ /
//! compressor configs carry the panel defaults (protocol-spec.md sec. 10).

use std::path::Path;

use openorion_proto::*;

fn atten(v: u8) -> Atten {
    Atten::new(v).expect("demo atten in range")
}

fn pan(display: i8) -> Pan {
    Pan::new((display + 32) as u8).expect("demo pan in range")
}

fn src(group: SourceGroup, chan1: u8) -> Source {
    Source::Input {
        group,
        chan: chan1 - 1,
    }
}

/// Route `count` consecutive sources starting at `from` (1-based) to bank
/// outputs starting at `out1` (1-based).
fn route(
    banks: &mut [RoutingBank],
    bank: OutputBank,
    out1: usize,
    g: SourceGroup,
    from: u8,
    count: usize,
) {
    let b = &mut banks[bank as usize];
    for i in 0..count {
        b.sources[out1 - 1 + i] = src(g, from + i as u8);
    }
}

fn demo_routing() -> Vec<RoutingBank> {
    use OutputBank as O;
    use SourceGroup as G;
    let mut r: Vec<RoutingBank> = OutputBank::ALL
        .iter()
        .map(|&b| RoutingBank::all_muted(b))
        .collect();
    route(&mut r, O::LineOut, 17, G::AdatIn, 3, 2);
    route(&mut r, O::Monitor, 1, G::Mix1, 1, 2);
    route(&mut r, O::UsbRec1, 1, G::AdatIn, 1, 2);
    route(&mut r, O::UsbRec1, 10, G::AdatIn, 1, 1);
    route(&mut r, O::UsbRec33, 1, G::AdatIn, 1, 1);
    for bank in [O::MadiOut1, O::MixCh1] {
        route(&mut r, bank, 1, G::LineIn, 1, 8);
        route(&mut r, bank, 9, G::AdatIn, 1, 2);
        route(&mut r, bank, 17, G::MadiIn1, 1, 2);
        route(&mut r, bank, 19, G::LineIn, 17, 3);
        route(&mut r, bank, 23, G::LineIn, 21, 8);
    }
    route(&mut r, O::MadiOut1, 13, G::AdatIn, 9, 2);
    route(&mut r, O::MadiOut1, 15, G::MadiIn1, 1, 2);
    route(&mut r, O::MixCh1, 13, G::AdatIn, 9, 4);
    route(&mut r, O::MadiOut33, 1, G::Hdx2Play, 1, 32);
    route(&mut r, O::AdatOut, 1, G::Mix3, 1, 2);
    route(&mut r, O::AdatOut, 3, G::AdatIn, 9, 2);
    route(&mut r, O::AdatOut, 5, G::Hdx1Play, 5, 4);
    route(&mut r, O::AdatOut, 9, G::Mix2, 1, 2);
    route(&mut r, O::AdatOut, 13, G::Hdx1Play, 13, 4);
    route(&mut r, O::SpdifOut, 1, G::Mix1, 1, 2);
    route(&mut r, O::AfxIn, 1, G::LineIn, 20, 1);
    route(&mut r, O::MixCh2, 1, G::AdatIn, 1, 4);
    route(&mut r, O::MixCh2, 5, G::MadiIn1, 1, 2);
    route(&mut r, O::MixCh2, 9, G::AdatIn, 11, 2);
    route(&mut r, O::MixCh2, 11, G::LineIn, 3, 1);
    route(&mut r, O::MixCh3, 3, G::AdatIn, 11, 2);
    route(&mut r, O::MixCh3, 11, G::LineIn, 3, 1);
    r
}

fn strip(level: u8, p: i8, mute: bool, send: u8) -> MixerChannel {
    MixerChannel {
        level: atten(level),
        pan: pan(p),
        mute,
        solo: false,
        send: atten(send),
    }
}

/// Build a mixer from per-channel pans, mutes and sends.
fn mixer(pans: [i8; 32], mutes: [u8; 32], sends: [u8; 32]) -> Mixer {
    let channels = std::array::from_fn(|i| strip(0, pans[i], mutes[i] != 0, sends[i]));
    Mixer {
        master: strip(0, 0, false, 96),
        channels,
    }
}

/// -30/+30 alternating from channel `from` (1-based) on; 0 before.
fn alt_pans(from: usize) -> [i8; 32] {
    std::array::from_fn(|i| {
        if i + 1 < from {
            0
        } else if (i + 1 - from).is_multiple_of(2) {
            -30
        } else {
            30
        }
    })
}

fn demo_mixers() -> [Mixer; 4] {
    #[rustfmt::skip]
    let m1_pans = [-30, 30, 0, 0, -30, 30, -30, 30, -30, 30, 0, 0, 0, 0, -30, 30,
                   -30, 30, 0, 0, -30, 0, -30, 30, 0, 0, -30, 30, 0, 0, -30, 30];
    #[rustfmt::skip]
    let m1_mutes = [0, 0, 1, 0, 1, 1, 1, 1, 0, 0, 1, 1, 1, 0, 1, 1,
                    0, 0, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1];
    let mut m1 = mixer(m1_pans, m1_mutes, [95; 32]);
    // A few non-zero faders so the strips are not all identical.
    for (i, l) in [
        (0usize, 6u8),
        (1, 6),
        (3, 12),
        (8, 3),
        (9, 3),
        (16, 9),
        (17, 9),
    ] {
        m1.channels[i].level = atten(l);
    }

    let mut m2_pans = alt_pans(13);
    m2_pans[..12].copy_from_slice(&[-30, 30, -30, 30, -30, 30, 0, 0, 0, 0, 0, 30]);
    let m2_sends = std::array::from_fn(|i| if i < 12 { 0 } else { 96 });
    let m2 = mixer(m2_pans, [0; 32], m2_sends);

    let m3_sends = std::array::from_fn(|i| if i < 4 { 0 } else { 96 });
    let m3 = mixer(alt_pans(5), [0; 32], m3_sends);

    let m4 = mixer(alt_pans(1), [0; 32], [96; 32]);
    [m1, m2, m3, m4]
}

fn demo_links() -> [[bool; 16]; 4] {
    let mut m1 = [true; 16];
    for p in [1, 4, 5, 6] {
        m1[p] = false;
    }
    let mut m2 = [false; 16];
    m2[..3].fill(true);
    [m1, m2, [false; 16], [false; 16]]
}

fn trim(whole: u8) -> TrimLevel {
    TrimLevel { whole, fract: 0 }
}

fn demo_trims() -> Trims {
    let mut line_in = [trim(4); TRIM_CHANNELS]; // 20 dBu
    line_in[..4].fill(trim(9)); // 15 dBu
    line_in[19] = trim(14); // 10 dBu
    Trims {
        line_out: TrimRecord {
            control: TrimControl::All,
            levels: [trim(8); TRIM_CHANNELS], // 16 dBu
        },
        line_in: TrimRecord {
            control: TrimControl::Individual,
            levels: line_in,
        },
    }
}

/// ClearQ defaults as read back in afxbase.json.
pub fn default_eq() -> EqParams {
    let f = [30, 200, 1000, 5000, 15000];
    let t = [
        EqFilter::LowShelf,
        EqFilter::Peak,
        EqFilter::Peak,
        EqFilter::Peak,
        EqFilter::HighShelf,
    ];
    EqParams {
        bands: std::array::from_fn(|i| EqBand {
            freq: f[i],
            q: 71,
            gain: 0,
            ftype: t[i],
        }),
    }
}

/// PowerFFC panel defaults (protocol-spec.md sec. 10).
pub fn default_comp() -> CompressorParams {
    CompressorParams {
        attack: 10_000,
        release: 10_000,
        taw: 65535,
        ratio: 100,
        gain: 0,
        ctrl: 0,
        threshold: 24,
        knee: 0,
        linked: 0,
    }
}

fn demo_afx() -> AfxState {
    let mut instances = std::collections::BTreeMap::new();
    for &t in AfxType::ALL {
        let n = match t.as_u8() {
            39 => 12,
            _ if t.on_orion() => 16,
            _ => 0,
        };
        if t.as_u8() <= 50 {
            instances.insert(t, n);
        }
    }
    let mut eq0 = default_eq();
    eq0.bands[1].gain = -300;
    eq0.bands[1].freq = 320;
    eq0.bands[3].gain = 250;
    eq0.bands[3].q = 150;
    let mut eqs = vec![
        AfxConfig {
            enabled: true,
            params: AfxParams::Eq(default_eq()),
        };
        16
    ];
    eqs[0].params = AfxParams::Eq(eq0);
    let mut comp0 = default_comp();
    comp0.ratio = 400;
    comp0.gain = 300;
    comp0.knee = 6;
    let mut comps = vec![
        AfxConfig {
            enabled: true,
            params: AfxParams::Compressor(default_comp()),
        };
        16
    ];
    comps[0].params = AfxParams::Compressor(comp0);

    let mut configs = std::collections::BTreeMap::new();
    configs.insert(AfxType::Equalizer, eqs);
    configs.insert(AfxType::Compressor, comps);
    configs.insert(
        AfxType::Neve1073,
        vec![
            AfxConfig {
                enabled: true,
                params: AfxParams::Raw(AfxRaw {
                    ty: AfxType::Neve1073,
                    fields: vec![0, 8, 0, 8, 0, 8, 0, 0],
                }),
            };
            16
        ],
    );

    let mut order = [[None; AFX_SLOTS]; AFX_CHAINS];
    order[0][0] = Some(AfxSlot {
        ty: AfxType::Equalizer,
        inst: 0,
    });
    order[0][1] = Some(AfxSlot {
        ty: AfxType::Compressor,
        inst: 0,
    });
    order[1][0] = Some(AfxSlot {
        ty: AfxType::Neve1073,
        inst: 0,
    });
    AfxState {
        order,
        links: [false; AFX_CHAINS / 2],
        instances,
        configs,
    }
}

/// A plausible device state (dump1 plus the AFX additions above).
pub fn demo_state() -> DeviceState {
    DeviceState {
        info: DeviceInfo {
            firmware: Version {
                major: 1,
                minor: 50,
            },
            passport: Passport {
                name: "Orion32HD".into(),
                serial: "0000000000000".into(),
                hw: "3.1".into(),
                birth: 1_524_573_936,
            },
            registered: true,
        },
        status: Status {
            current_preset: 0,
            power_on: true,
            muted_bit: false,
            usb_mode: 0,
            device_updated: false,
            clock: ClockFlags {
                atomic: true,
                adat: true,
                ..ClockFlags::default()
            },
            sample_rate: Some(SampleRate::R96k),
            pull_mode: 0,
            clock_source: Some(ClockSource::Internal),
            oscillator: Osc {
                freq_l: OscFreq::Hz1k,
                freq_r: OscFreq::Hz1k,
                level: OscLevel::DbM12,
                mute_l: false,
                mute_r: true,
            },
            external_clock_hz: 96_000,
            smux_in: false,
            smux_out: false,
            spdif_src: false,
            brightness: 0,
            adat_channels: 8,
            madi_channels: 32,
            main: VolumeState {
                atten: Atten::ZERO_DB,
                mute: false,
            },
            line: VolumeState {
                atten: Atten::ZERO_DB,
                mute: false,
            },
            monitor_trim: 9,
            meter_sources: [7, 22, 3, 0],
        },
        routing: demo_routing(),
        mixers: demo_mixers(),
        mixer_links: demo_links(),
        trims: demo_trims(),
        reverb: [
            Some(Reverb {
                mixer: MixerId::new(0).expect("mixer 0"),
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
            None,
            None,
            None,
        ],
        afx: demo_afx(),
        hdx_delays: [[0; 32]; 2],
    }
}

/// Load a `DeviceState` (serde_json). Also accepts a `Snapshot` file.
pub fn load_state(path: &Path) -> Result<DeviceState, String> {
    let text = std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
    parse_state(&text).map_err(|e| format!("{}: {e}", path.display()))
}

pub fn parse_state(text: &str) -> Result<DeviceState, String> {
    match serde_json::from_str::<DeviceState>(text) {
        Ok(s) => Ok(s),
        Err(e) => serde_json::from_str::<Snapshot>(text)
            .map(|s| s.state)
            .map_err(|_| e.to_string()),
    }
}

/// A fake openorion-remote announce for `--demo`, so the Remote tab and the
/// learn outlines can be looked at offline (no binds happen).
pub fn demo_remote() -> crate::learn::RemoteView {
    use openorion_control::{
        ChField, MapOpts, Mapping, Mappings, MasterField, MidiCtl, Mode, OscTarget, Param,
        PortInfo, Source as Src,
    };
    use openorion_proto::VolumeId;
    let m4 = MixerId::new(3).expect("mixer 3");
    let nano = |ctl| Src::Midi {
        device: "nanoKONTROL2".into(),
        ch: Some(0),
        ctl,
    };
    let map = |source, param, opts| Mapping {
        source,
        param,
        opts,
    };
    crate::learn::RemoteView {
        listen: Some("127.0.0.1:9000".into()),
        midi_inputs: vec![
            PortInfo {
                name: "nanoKONTROL2".into(),
                enabled: true,
            },
            PortInfo {
                name: "Launch Control XL".into(),
                enabled: false,
            },
        ],
        midi_outputs: vec![PortInfo {
            name: "nanoKONTROL2".into(),
            enabled: true,
        }],
        osc_targets: vec![OscTarget {
            addr: "127.0.0.1:9001".into(),
            values: true,
            meters: true,
            meter_hz: 20,
        }],
        mappings: Mappings(vec![
            map(
                nano(MidiCtl::Cc(0)),
                Param::Mixer {
                    mixer: m4,
                    ch: 1,
                    field: ChField::Level,
                },
                MapOpts::default(),
            ),
            map(
                nano(MidiCtl::Cc(16)),
                Param::Mixer {
                    mixer: m4,
                    ch: 1,
                    field: ChField::Pan,
                },
                MapOpts {
                    takeover: openorion_control::Takeover::Pickup,
                    ..MapOpts::default()
                },
            ),
            map(
                nano(MidiCtl::Note(48)),
                Param::Master {
                    mixer: m4,
                    field: MasterField::Mute,
                },
                MapOpts {
                    mode: Mode::Toggle,
                    ..MapOpts::default()
                },
            ),
            map(
                Src::Osc {
                    from: Some("tablet".into()),
                    addr: "/1/fader1".into(),
                    arg: 0,
                },
                Param::Volume(VolumeId::Main),
                MapOpts {
                    lo: 0.4,
                    ..MapOpts::default()
                },
            ),
        ]),
        osc_senders: vec![("tablet".into(), f64::NEG_INFINITY)],
        demo: true,
        ..Default::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn demo_roundtrips_through_json() {
        let s = demo_state();
        let json = serde_json::to_string_pretty(&s).unwrap();
        assert_eq!(parse_state(&json).unwrap(), s);

        let dir = std::env::temp_dir().join(format!("openorion-ui-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("state.json");
        std::fs::write(&path, &json).unwrap();
        assert_eq!(load_state(&path).unwrap(), s);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn snapshot_file_also_loads() {
        let snap = Snapshot {
            version: 1,
            taken: 0,
            state: demo_state(),
        };
        let json = serde_json::to_string(&snap).unwrap();
        assert_eq!(parse_state(&json).unwrap(), snap.state);
        assert!(parse_state("{}").is_err());
    }

    #[test]
    fn demo_matches_dump1() {
        let s = demo_state();
        assert_eq!(s.routing.len(), OutputBank::ALL.len());
        for (i, b) in s.routing.iter().enumerate() {
            assert_eq!(b.bank as usize, i);
            assert_eq!(b.sources.len(), b.bank.size());
        }
        let mon = &s.routing[OutputBank::Monitor as usize].sources;
        assert_eq!(mon[0], src(SourceGroup::Mix1, 1));
        assert_eq!(s.mixers[0].channels[0].pan.display(), -30);
        assert!(s.mixers[0].channels[2].mute);
        assert!((s.trims.line_in.levels[19].dbu() - 10.0).abs() < 1e-6);
        assert_eq!(s.afx.instances[&AfxType::Powergate], 12);
    }
}
