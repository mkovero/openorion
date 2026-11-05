use super::*;
use openorion_control::{MapOpts, MidiCtl, Mode, OscTarget, Takeover};
use openorion_proto::{Atten, MixerId};

const T_VALUES: &str = "127.0.0.1:9001";
const T_METERS: &str = "127.0.0.1:9002";

fn mx4() -> MixerId {
    MixerId::new(3).unwrap()
}

fn level(ch: u8) -> Param {
    Param::Mixer {
        mixer: mx4(),
        ch,
        field: openorion_control::ChField::Level,
    }
}

fn cc(device: &str, ch: u8, n: u8) -> Source {
    Source::Midi {
        device: device.into(),
        ch: Some(ch),
        ctl: MidiCtl::Cc(n),
    }
}

fn target(addr: &str, values: bool, meters: bool, hz: u32) -> Target {
    Target {
        addr: addr.parse().unwrap(),
        cfg: OscTarget {
            addr: addr.into(),
            values,
            meters,
            meter_hz: hz,
        },
    }
}

/// Engine with one values target, synced to an empty state (all mixer 4
/// levels at 0 dB = norm 1).
fn engine(mappings: Vec<Mapping>) -> (Engine, Instant) {
    let cfg = RemoteConfig {
        mappings,
        ..RemoteConfig::default()
    };
    let mut e = Engine::new(
        cfg,
        Some("127.0.0.1:9000".into()),
        vec![target(T_VALUES, true, false, 20)],
    );
    let now = Instant::now();
    let st = openorion_proto::decode::empty_state();
    e.daemon(
        DaemonMsg::State {
            state: Box::new(st),
        },
        now,
    );
    (e, now)
}

fn map(source: Source, param: Param, opts: MapOpts) -> Mapping {
    Mapping {
        source,
        param,
        opts,
    }
}

fn ops(out: &[Out]) -> Vec<Op> {
    out.iter()
        .filter_map(|o| match o {
            Out::Op(op) => Some(op.clone()),
            _ => None,
        })
        .collect()
}

fn level_of(op: &Op) -> (u8, u8) {
    match op {
        Op::Mixer {
            ch, level: Some(l), ..
        } => (*ch, l.get()),
        o => panic!("not a level op: {o:?}"),
    }
}

fn ms(n: u64) -> Duration {
    Duration::from_millis(n)
}

fn set_level_diff(e: &mut Engine, ch: u8, atten: u8, now: Instant) -> Vec<Out> {
    let mut c = e.state().unwrap().mixers[3].channels[ch as usize - 1];
    c.level = Atten::new(atten).unwrap();
    e.daemon(
        DaemonMsg::Diff {
            path: format!("/mixers/3/channels/{}", ch - 1),
            value: serde_json::to_value(c).unwrap(),
        },
        now,
    )
}

#[test]
fn learn_binds_without_ops_and_saves() {
    let (mut e, t) = engine(vec![]);
    let out = e.remote(RemoteMsg::LearnStart);
    assert!(matches!(
        &out[..],
        [Out::Remote(RemoteMsg::Announce { learning: true, .. })]
    ));
    // Nothing selected: events do nothing but blink.
    let out = e.midi_in("nano", &[0xB0, 7, 10], t);
    assert!(matches!(
        &out[..],
        [Out::Remote(RemoteMsg::Activity { .. })]
    ));
    e.remote(RemoteMsg::LearnSelect {
        param: Some(level(1)),
    });
    let out = e.midi_in("nano", &[0xB0, 7, 20], t + ms(200));
    assert!(ops(&out).is_empty());
    assert!(out.iter().any(|o| *o
        == Out::Remote(RemoteMsg::Learned {
            param: level(1),
            source: cc("nano", 0, 7)
        })));
    assert!(out.iter().any(
        |o| matches!(o, Out::Remote(RemoteMsg::Announce { mappings, .. }) if mappings.len() == 1)
    ));
    // Turning on: no new Learned, still no ops.
    let out = e.midi_in("nano", &[0xB0, 7, 30], t + ms(210));
    assert!(out.is_empty(), "{out:?}");
    // Selection stays; the next control binds to the next param.
    e.remote(RemoteMsg::LearnSelect {
        param: Some(level(2)),
    });
    e.midi_in("nano", &[0xB0, 8, 30], t + ms(400));
    // OSC binds by address, any sender.
    e.remote(RemoteMsg::LearnSelect {
        param: Some(level(3)),
    });
    let from: SocketAddr = "10.0.0.5:5000".parse().unwrap();
    let out = e.osc_in(
        from,
        OscMessage {
            addr: "/1/fader1".into(),
            args: vec![OscType::Float(0.3)],
        },
        t + ms(500),
    );
    assert!(ops(&out).is_empty());
    assert_eq!(e.mappings().len(), 3);
    let osc = Source::Osc {
        from: None,
        addr: "/1/fader1".into(),
        arg: 0,
    };
    assert_eq!(e.mappings().get(&osc).unwrap().param, level(3));
    let out = e.remote(RemoteMsg::LearnStop { save: true });
    let Some(Out::Save(cfg)) = out.iter().find(|o| matches!(o, Out::Save(_))) else {
        panic!("no save: {out:?}")
    };
    assert_eq!(cfg.mappings.len(), 3);
    assert!(!e.learning());
    // Now the controller drives the device.
    let out = e.midi_in("nano", &[0xB0, 7, 0], t + ms(600));
    assert_eq!(level_of(&ops(&out)[0]), (1, 96));
}

#[test]
fn learn_revert_and_persist_outside_learning() {
    let orig = map(cc("nano", 0, 7), level(1), MapOpts::default());
    let (mut e, t) = engine(vec![orig.clone()]);
    e.ports(vec!["nano".into(), "pad".into()], vec![]);
    e.remote(RemoteMsg::LearnStart);
    e.remote(RemoteMsg::LearnSelect {
        param: Some(level(2)),
    });
    e.midi_in("nano", &[0xB0, 7, 5], t);
    assert_eq!(e.mappings().get(&cc("nano", 0, 7)).unwrap().param, level(2));
    // Edits while learning are not saved.
    let out = e.remote(RemoteMsg::Unbind {
        param: Some(level(2)),
        source: None,
    });
    assert!(!out.iter().any(|o| matches!(o, Out::Save(_))));
    let out = e.remote(RemoteMsg::SetDevices {
        input: Some(("pad".into(), false)),
        output: None,
    });
    assert!(out.iter().any(|o| matches!(o, Out::MidiPorts(_))));
    assert!(!out.iter().any(|o| matches!(o, Out::Save(_))));
    let out = e.remote(RemoteMsg::LearnStop { save: false });
    assert!(!out.iter().any(|o| matches!(o, Out::Save(_))));
    assert!(
        out.iter()
            .any(|o| matches!(o, Out::MidiPorts(m) if m.in_ports.is_empty()))
    );
    assert_eq!(e.mappings().0, vec![orig.clone()]);
    // Outside learning, edits persist at once.
    let opts = MapOpts {
        invert: true,
        ..MapOpts::default()
    };
    let out = e.remote(RemoteMsg::SetOpts {
        source: cc("nano", 0, 7),
        opts,
    });
    assert!(
        matches!(&out[..], [Out::Save(c), Out::Remote(RemoteMsg::Announce { .. })] if c.mappings[0].opts.invert)
    );
    let out = e.remote(RemoteMsg::Unbind {
        param: None,
        source: Some(cc("nano", 0, 7)),
    });
    assert!(
        matches!(&out[..], [Out::Midi { bytes, .. }, Out::Save(c), _] if c.mappings.is_empty() && bytes == &[0xB0, 7, 0]),
        "{out:?}"
    );
    // Nothing to unbind: no save.
    assert!(
        e.remote(RemoteMsg::Unbind {
            param: Some(level(9)),
            source: None
        })
        .is_empty()
    );
}

#[test]
fn learn_filter_and_cc14_upgrade() {
    let (mut e, t) = engine(vec![]);
    e.remote(RemoteMsg::LearnStart);
    e.remote(RemoteMsg::LearnFilter {
        device: Some("nano".into()),
    });
    e.remote(RemoteMsg::LearnSelect {
        param: Some(level(1)),
    });
    e.midi_in("noisy", &[0xB0, 1, 5], t);
    assert!(e.mappings().is_empty());
    // MSB then LSB of a 14-bit fader: one 14-bit binding.
    e.midi_in("nanoKONTROL", &[0xB0, 1, 5], t);
    e.midi_in("nanoKONTROL", &[0xB0, 33, 5], t);
    e.midi_in("nanoKONTROL", &[0xB0, 1, 6], t);
    let m = &e.mappings().0;
    assert_eq!(m.len(), 1, "{m:?}");
    assert_eq!(
        m[0].source,
        Source::Midi {
            device: "nanoKONTROL".into(),
            ch: Some(0),
            ctl: MidiCtl::Cc14(1)
        }
    );
}

#[test]
fn fixed_namespace() {
    let (mut e, t) = engine(vec![]);
    let from: SocketAddr = "127.0.0.1:7000".parse().unwrap();
    let msg = |addr: &str, a: OscType| OscMessage {
        addr: addr.into(),
        args: vec![a],
    };
    let out = e.osc_in(
        from,
        msg("/orion/mixer/4/ch/1/level", OscType::Float(0.5)),
        t,
    );
    assert_eq!(level_of(&ops(&out)[0]), (1, 48));
    let out = e.osc_in(from, msg("/orion/mixer/4/ch/1/mute", OscType::Int(1)), t);
    assert!(matches!(
        ops(&out)[..],
        [Op::Mixer {
            mute: Some(true),
            ..
        }]
    ));
    assert!(ops(&e.osc_in(from, msg("/orion/nope", OscType::Float(1.0)), t)).is_empty());
    // No argument: the value comes back to the sender.
    let out = e.osc_in(
        from,
        OscMessage {
            addr: "/orion/mixer/4/ch/2/level".into(),
            args: vec![],
        },
        t,
    );
    assert_eq!(
        out,
        vec![Out::Osc {
            target: from,
            msg: value_msg(&level(2), 1.0),
            reply: true
        }]
    );
    // /orion/sync: everything to the values targets.
    let out = e.osc_in(
        from,
        OscMessage {
            addr: OSC_SYNC.into(),
            args: vec![],
        },
        t,
    );
    assert!(out.len() > 100);
    assert!(out.iter().all(|o| matches!(o, Out::Osc { target, reply: false, .. } if *target == T_VALUES.parse().unwrap())));
}

#[test]
fn mapped_cc_throttle() {
    let (mut e, t) = engine(vec![map(cc("*", 0, 7), level(1), MapOpts::default())]);
    let out = e.midi_in("any", &[0xB0, 7, 0], t);
    assert_eq!(level_of(&ops(&out)[0]), (1, 96));
    // Within the throttle window: held, latest wins.
    assert!(ops(&e.midi_in("any", &[0xB0, 7, 64], t + ms(5))).is_empty());
    assert!(ops(&e.midi_in("any", &[0xB0, 7, 127], t + ms(10))).is_empty());
    assert_eq!(e.next_deadline(), Some(t + THROTTLE));
    assert!(e.tick(t + ms(20)).is_empty());
    let out = e.tick(t + ms(40));
    assert_eq!(ops(&out).len(), 1);
    assert_eq!(level_of(&ops(&out)[0]), (1, 0));
    assert_eq!(e.next_deadline(), None);
}

#[test]
fn relative_encoder() {
    let opts = MapOpts {
        mode: Mode::Relative,
        ..MapOpts::default()
    };
    let (mut e, t) = engine(vec![map(cc("enc", 0, 16), level(1), opts)]);
    set_level_diff(&mut e, 1, 48, t);
    // +1 step: one dB up.
    let out = e.midi_in("enc", &[0xB0, 16, 1], t);
    assert_eq!(level_of(&ops(&out)[0]), (1, 47));
    // Next tick builds on the value just sent, not the stale device one.
    e.midi_in("enc", &[0xB0, 16, 1], t + ms(5));
    let out = e.tick(t + ms(40));
    assert_eq!(level_of(&ops(&out)[0]), (1, 46));
    // -2 steps.
    let out = e.midi_in("enc", &[0xB0, 16, 126], t + ms(100));
    assert_eq!(level_of(&ops(&out)[0]), (1, 48));
}

#[test]
fn pickup_waits_for_the_value() {
    let opts = MapOpts {
        takeover: Takeover::Pickup,
        ..MapOpts::default()
    };
    let (mut e, t) = engine(vec![map(cc("nano", 0, 7), level(1), opts)]);
    set_level_diff(&mut e, 1, 48, t); // norm 0.5
    assert!(ops(&e.midi_in("nano", &[0xB0, 7, 0], t)).is_empty());
    assert!(ops(&e.midi_in("nano", &[0xB0, 7, 30], t + ms(50))).is_empty());
    // Crossing 0.5 catches it.
    let out = e.midi_in("nano", &[0xB0, 7, 70], t + ms(100));
    assert_eq!(ops(&out).len(), 1);
}

#[test]
fn cc14_combines() {
    let src = Source::Midi {
        device: "f".into(),
        ch: None,
        ctl: MidiCtl::Cc14(1),
    };
    let (mut e, t) = engine(vec![map(src, level(1), MapOpts::default())]);
    // LSB before any MSB: ignored.
    assert!(ops(&e.midi_in("f", &[0xB0, 33, 127], t)).is_empty());
    let out = e.midi_in("f", &[0xB0, 1, 0], t);
    assert_eq!(level_of(&ops(&out)[0]), (1, 96));
    // MSB 127 alone is 16256/16383; the LSB completes full scale.
    e.midi_in("f", &[0xB0, 1, 127], t + ms(2));
    e.midi_in("f", &[0xB0, 33, 127], t + ms(3));
    let out = e.tick(t + ms(40));
    assert_eq!(level_of(&ops(&out)[0]), (1, 0));
}

#[test]
fn linked_partner_follows() {
    let (mut e, t) = engine(vec![map(cc("nano", 0, 7), level(1), MapOpts::default())]);
    let mut links = e.state().unwrap().mixer_links;
    links[3][0] = true;
    e.daemon(
        DaemonMsg::Diff {
            path: "/mixer_links".into(),
            value: serde_json::to_value(links).unwrap(),
        },
        t,
    );
    let out = e.midi_in("nano", &[0xB0, 7, 0], t);
    let o = ops(&out);
    assert_eq!(o.len(), 2);
    assert_eq!(level_of(&o[0]), (1, 96));
    assert_eq!(level_of(&o[1]), (2, 96));
}

#[test]
fn feedback_and_echo_suppression() {
    let (mut e, t) = engine(vec![map(cc("nano", 0, 7), level(1), MapOpts::default())]);
    let out = set_level_diff(&mut e, 1, 48, t);
    assert!(out.contains(&Out::Osc {
        target: T_VALUES.parse().unwrap(),
        msg: OscMessage {
            addr: "/orion/mixer/4/ch/1/level".into(),
            args: vec![OscType::Float(0.5)]
        },
        reply: false
    }));
    assert!(out.contains(&Out::Midi {
        device: "nano".into(),
        bytes: vec![0xB0, 7, 64]
    }));
    // Only the changed channel is reported.
    assert_eq!(out.len(), 2, "{out:?}");
    // The control just moved: no MIDI echo, OSC still goes.
    let t2 = t + ms(1000);
    e.midi_in("nano", &[0xB0, 7, 0], t2);
    let out = set_level_diff(&mut e, 1, 96, t2 + ms(50));
    assert!(out.iter().any(|o| matches!(o, Out::Osc { .. })));
    assert!(!out.iter().any(|o| matches!(o, Out::Midi { .. })));
    // Later changes (from the UI) reach the controller again.
    let out = set_level_diff(&mut e, 1, 0, t2 + ms(300));
    assert!(out.contains(&Out::Midi {
        device: "nano".into(),
        bytes: vec![0xB0, 7, 127]
    }));
    // Unchanged value: nothing.
    assert!(set_level_diff(&mut e, 1, 0, t2 + ms(400)).is_empty());
}

#[test]
fn first_state_sends_midi_only() {
    let cfg = RemoteConfig {
        mappings: vec![map(cc("nano", 0, 7), level(1), MapOpts::default())],
        ..RemoteConfig::default()
    };
    let mut e = Engine::new(cfg, None, vec![target(T_VALUES, true, false, 20)]);
    let out = e.daemon(
        DaemonMsg::State {
            state: Box::new(openorion_proto::decode::empty_state()),
        },
        Instant::now(),
    );
    assert_eq!(
        out,
        vec![Out::Midi {
            device: "nano".into(),
            bytes: vec![0xB0, 7, 127]
        }]
    );
}

#[test]
fn activity_is_rate_limited() {
    let (mut e, t) = engine(vec![]);
    let n = |out: Vec<Out>| {
        out.iter()
            .filter(|o| matches!(o, Out::Remote(RemoteMsg::Activity { .. })))
            .count()
    };
    assert_eq!(n(e.midi_in("nano", &[0xB0, 7, 1], t)), 1);
    assert_eq!(n(e.midi_in("nano", &[0xB0, 7, 2], t + ms(50))), 0);
    // Another control is another source.
    assert_eq!(n(e.midi_in("nano", &[0xB0, 8, 2], t + ms(50))), 1);
    assert_eq!(n(e.midi_in("nano", &[0xB0, 7, 3], t + ms(120))), 1);
}

#[test]
fn meters_rate_and_dbfs() {
    let mut e = Engine::new(
        RemoteConfig::default(),
        None,
        vec![
            target(T_VALUES, true, false, 20),
            target(T_METERS, false, true, 20),
        ],
    );
    assert!(e.wants_meters());
    let mut m = Meters::default();
    m.bank_a[0] = 6;
    m.bank_a[1] = 0;
    let t = Instant::now();
    let mut sent = 0;
    // One second of the daemon's 30 Hz stream.
    for i in 0..30u64 {
        let out = e.daemon(
            DaemonMsg::Meters {
                meters: Box::new(m.clone()),
                afx: Box::new(AfxMeters::default()),
            },
            t + Duration::from_micros(i * 33_333),
        );
        for o in &out {
            let Out::Osc { target, msg, .. } = o else {
                panic!("{o:?}")
            };
            assert_eq!(*target, T_METERS.parse().unwrap());
            if msg.addr == "/orion/meters/bank_a" {
                sent += 1;
                assert_eq!(msg.args.len(), 32);
                assert_eq!(msg.args[0], OscType::Float(-6.0));
                assert_eq!(msg.args[1], OscType::Float(0.0));
                assert_eq!(msg.args[2], OscType::Float(-96.0));
            }
        }
        if i == 0 {
            assert_eq!(out.len(), METER_BANKS.len());
        }
    }
    assert!((19..=21).contains(&sent), "{sent} meter frames in 1 s");
    let afx = meter_msgs(&m, &AfxMeters::default());
    assert_eq!(
        afx[5].args.len(),
        openorion_proto::AFX_CHAINS * openorion_proto::AFX_SLOTS
    );
}

#[test]
fn group_of_paths() {
    assert_eq!(Group::of("/mixers/3/channels/0"), Group::Mixer(3, Some(1)));
    assert_eq!(Group::of("/mixers/3/master"), Group::Mixer(3, Some(0)));
    assert_eq!(Group::of("/status"), Group::Status);
    assert_eq!(Group::of(""), Group::All);
    assert_eq!(Group::of("/routing/3"), Group::Nothing);
    assert!(Group::Mixer(3, Some(1)).covers(&level(1)));
    assert!(!Group::Mixer(3, Some(1)).covers(&level(2)));
}

#[test]
fn set_devices_persists() {
    let (mut e, _) = engine(vec![]);
    e.ports(vec!["nano".into(), "pad".into()], vec!["nano".into()]);
    let out = e.remote(RemoteMsg::SetDevices {
        input: Some(("pad".into(), false)),
        output: Some(("nano".into(), false)),
    });
    let Some(Out::Save(c)) = out.iter().find(|o| matches!(o, Out::Save(_))) else {
        panic!()
    };
    assert_eq!(c.midi.in_ports, vec!["nano".to_string()]);
    assert_eq!(c.midi.out_off, vec!["nano".to_string()]);
    let Some(Out::Remote(RemoteMsg::Announce {
        midi_inputs,
        midi_outputs,
        ..
    })) = out.last()
    else {
        panic!()
    };
    assert!(midi_inputs[0].enabled && !midi_inputs[1].enabled);
    assert!(!midi_outputs[0].enabled);
    // Same ports again: no announce.
    assert!(
        e.ports(vec!["nano".into(), "pad".into()], vec!["nano".into()])
            .is_empty()
    );
}

#[test]
fn toggle_feedback_ignores_echo_window() {
    // A button LED only follows the host: right after the press that
    // toggled mute, the new state must still go back to the controller.
    let mute = Param::Mixer {
        mixer: mx4(),
        ch: 1,
        field: openorion_control::ChField::Mute,
    };
    let src = Source::Midi {
        device: "apc".into(),
        ch: Some(0),
        ctl: MidiCtl::Note(52),
    };
    let opts = MapOpts::default_for(&src, &mute);
    assert_eq!(opts.mode, Mode::Toggle);
    let (mut e, t) = engine(vec![map(src, mute, opts)]);
    let t1 = t + ms(1000);
    let out = e.midi_in("apc", &[0x90, 52, 127], t1);
    assert_eq!(ops(&out).len(), 1, "{out:?}");
    let mut c = e.state().unwrap().mixers[3].channels[0];
    c.mute = true;
    let out = e.daemon(
        DaemonMsg::Diff {
            path: "/mixers/3/channels/0".into(),
            value: serde_json::to_value(c).unwrap(),
        },
        t1 + ms(20),
    );
    assert!(
        out.contains(&Out::Midi {
            device: "apc".into(),
            bytes: vec![0x90, 52, 127]
        }),
        "{out:?}"
    );
}

#[test]
fn ready_device_gets_mapped_values() {
    let (mut e, _) = engine(vec![
        map(cc("apc", 0, 7), level(1), MapOpts::default()),
        map(cc("other", 0, 8), level(2), MapOpts::default()),
        map(
            cc(openorion_control::ANY_DEVICE, 0, 9),
            level(3),
            MapOpts::default(),
        ),
    ]);
    let out = e.midi_ready("apc");
    assert_eq!(
        out,
        vec![
            Out::Midi {
                device: "apc".into(),
                bytes: vec![0xB0, 7, 127]
            },
            Out::Midi {
                device: "apc".into(),
                bytes: vec![0xB0, 9, 127]
            },
        ]
    );
}

fn midi_outs(out: &[Out]) -> Vec<(String, Vec<u8>)> {
    out.iter()
        .filter_map(|o| match o {
            Out::Midi { device, bytes } => Some((device.clone(), bytes.clone())),
            _ => None,
        })
        .collect()
}

#[test]
fn unbound_buttons_go_dark() {
    let (mut e, t) = engine(vec![map(cc("apc", 0, 7), level(1), MapOpts::default())]);
    // An unbound button (APC A/B, ch 7 note 66): LED off on press and release.
    let out = e.midi_in("apc", &[0x96, 66, 127], t);
    assert_eq!(midi_outs(&out), vec![("apc".into(), vec![0x96, 66, 0])]);
    let out = e.midi_in("apc", &[0x86, 66, 0], t + ms(100));
    assert_eq!(midi_outs(&out), vec![("apc".into(), vec![0x96, 66, 0])]);
    // An unbound knob gets nothing back.
    let out = e.midi_in("apc", &[0xB0, 16, 40], t + ms(200));
    assert!(midi_outs(&out).is_empty(), "{out:?}");
}

#[test]
fn bind_lights_and_unbind_darkens() {
    let (mut e, t) = engine(vec![]);
    e.remote(RemoteMsg::LearnStart);
    e.remote(RemoteMsg::LearnSelect {
        param: Some(level(1)),
    });
    // Binding shows the current value (0 dB = 127) at once.
    let out = e.midi_in("apc", &[0xB0, 16, 5], t);
    assert_eq!(midi_outs(&out), vec![("apc".into(), vec![0xB0, 16, 127])]);
    // Cancelling the session takes the binding back: ring off.
    let out = e.remote(RemoteMsg::LearnStop { save: false });
    assert_eq!(midi_outs(&out), vec![("apc".into(), vec![0xB0, 16, 0])]);
    // Unbind outside learn: off too.
    let (mut e, _) = engine(vec![map(cc("apc", 0, 17), level(2), MapOpts::default())]);
    let out = e.remote(RemoteMsg::Unbind {
        param: Some(level(2)),
        source: None,
    });
    assert_eq!(midi_outs(&out), vec![("apc".into(), vec![0xB0, 17, 0])]);
}

const APC: &str = "APC40 mkII";

/// Engine with the APC grid: CC7 ch 1 = mixer 4 ch 1 (linked to ch 2,
/// fed by AFX chain 5), CC7 ch 2 = mixer 4 ch 3; mixer 4 metered.
fn grid_engine() -> (Engine, Instant) {
    let cfg = RemoteConfig {
        mappings: vec![
            map(cc(APC, 0, 7), level(1), MapOpts::default()),
            map(cc(APC, 1, 7), level(3), MapOpts::default()),
            map(
                Source::Midi {
                    device: APC.into(),
                    ch: Some(0),
                    ctl: MidiCtl::Note(32),
                },
                Param::Mixer {
                    mixer: mx4(),
                    ch: 1,
                    field: openorion_control::ChField::Mute,
                },
                MapOpts::default(),
            ),
        ],
        ..RemoteConfig::default()
    };
    let mut e = Engine::new(cfg, None, Vec::new());
    assert!(e.wants_meters());
    let mut st = openorion_proto::decode::empty_state();
    st.status.meter_sources[openorion_proto::MeterBank::Mixer as usize] = 3;
    st.mixer_links[3][0] = true;
    let bank = openorion_proto::OutputBank::ALL[openorion_proto::OutputBank::MixCh1 as usize + 3];
    if let Some(r) = st.routing.iter_mut().find(|r| r.bank == bank) {
        r.sources[0] = openorion_proto::Source::Input {
            group: openorion_proto::SourceGroup::AfxOut,
            chan: 4,
        };
    } else {
        let mut sources = vec![openorion_proto::Source::Mute; bank.size()];
        sources[0] = openorion_proto::Source::Input {
            group: openorion_proto::SourceGroup::AfxOut,
            chan: 4,
        };
        st.routing
            .push(openorion_proto::RoutingBank { bank, sources });
    }
    let now = Instant::now();
    e.daemon(
        DaemonMsg::State {
            state: Box::new(st),
        },
        now,
    );
    (e, now)
}

fn meter_frame(e: &mut Engine, mixer: &[(usize, u8)], afx5: u8, now: Instant) -> Vec<(u8, u8)> {
    let mut m = Meters::default();
    for &(i, v) in mixer {
        m.mixer[i] = v;
    }
    let mut afx = AfxMeters::default();
    for p in afx.peak.iter_mut().flatten() {
        *p = 96;
    }
    afx.peak[4][0] = afx5;
    let out = e.daemon(
        DaemonMsg::Meters {
            meters: Box::new(m),
            afx: Box::new(afx),
        },
        now,
    );
    midi_outs(&out)
        .into_iter()
        .map(|(d, b)| {
            assert_eq!(d, APC);
            assert_eq!(b[0], 0x90);
            (b[1], b[2])
        })
        .collect()
}

#[test]
fn grid_meters_follow_faders() {
    let (mut e, t) = grid_engine();
    // Ch 2 (linked partner) at -8, AFX 5 at -20: column 1 shows -8;
    // ch 3 at -40 is below the bottom row (-36): column 2 stays dark.
    let leds = meter_frame(&mut e, &[(1, 8), (2, 40)], 20, t);
    let lit: Vec<(u8, u8)> = leds.iter().copied().filter(|l| l.1 != 0).collect();
    assert_eq!(lit, vec![(0, 21), (8, 21), (16, 13)]);
    // Every other pad goes dark once (scene column included), except the
    // bound note 32.
    assert_eq!(leds.len(), 39 + 5);
    assert!(!leds.iter().any(|l| l.0 == 32));
    // Same level again inside the period: nothing; after it: no change.
    assert!(meter_frame(&mut e, &[(1, 8), (2, 40)], 20, t).is_empty());
    let t2 = t + Duration::from_millis(60);
    assert!(meter_frame(&mut e, &[(1, 8), (2, 40)], 20, t2).is_empty());
}

#[test]
fn grid_clip_holds_and_afx_counts() {
    let (mut e, t) = grid_engine();
    meter_frame(&mut e, &[], 96, t);
    // The mixer meter is below full scale (limiter); the AFX chain clips.
    let t1 = t + Duration::from_millis(60);
    let leds = meter_frame(&mut e, &[(0, 3)], 0, t1);
    let lit: Vec<u8> = leds.iter().filter(|l| l.1 != 0).map(|l| l.0).collect();
    assert_eq!(lit, vec![0, 8, 16, 24]); // 32 is bound: no clip pad there
    // Column 2 clips: its top pad (33) is red and held.
    let t2 = t1 + Duration::from_millis(60);
    let leds = meter_frame(&mut e, &[(2, 0)], 96, t2);
    assert!(leds.contains(&(33, 5)));
    let t3 = t2 + Duration::from_millis(1000);
    let leds = meter_frame(&mut e, &[], 96, t3);
    assert!(!leds.iter().any(|l| l.0 == 33));
    let t4 = t2 + Duration::from_millis(2100);
    let leds = meter_frame(&mut e, &[], 96, t4);
    assert!(leds.contains(&(33, 0)));
}

#[test]
fn grid_pad_press_is_redrawn() {
    let (mut e, t) = grid_engine();
    meter_frame(&mut e, &[(2, 10)], 96, t);
    // Pressing an unbound meter pad: dark on press, the next frame redraws.
    let out = e.midi_in(APC, &[0x90, 1, 127], t);
    assert_eq!(midi_outs(&out), vec![(APC.to_owned(), vec![0x90, 1, 0])]);
    let t1 = t + Duration::from_millis(60);
    assert_eq!(meter_frame(&mut e, &[(2, 10)], 96, t1), vec![(1, 21)]);
}

#[test]
fn grid_master_column_follows_metered_bus() {
    use openorion_proto::{MeterBank, OutputBank, RoutingBank, SourceGroup};
    let master = Param::Master {
        mixer: mx4(),
        field: openorion_control::MasterField::Level,
    };
    let cfg = RemoteConfig {
        mappings: vec![map(cc(APC, 0, 14), master, MapOpts::default())],
        ..RemoteConfig::default()
    };
    let mut e = Engine::new(cfg, None, Vec::new());
    let mut st = openorion_proto::decode::empty_state();
    // Bank B meters ADAT OUT (source 14 + 8); ADAT OUT 3-4 <- MIX L/R4.
    st.status.meter_sources[MeterBank::B as usize] = 14 + OutputBank::AdatOut as u8;
    st.routing.retain(|r| r.bank != OutputBank::AdatOut);
    let mut sources = vec![openorion_proto::Source::Mute; OutputBank::AdatOut.size()];
    for (i, s) in sources.iter_mut().enumerate().skip(2).take(2) {
        *s = openorion_proto::Source::Input {
            group: SourceGroup::Mix4,
            chan: i as u8 - 2,
        };
    }
    st.routing.push(RoutingBank {
        bank: OutputBank::AdatOut,
        sources,
    });
    let t = Instant::now();
    e.daemon(
        DaemonMsg::State {
            state: Box::new(st.clone()),
        },
        t,
    );
    let frame = |e: &mut Engine, b: &[(usize, u8)], now| {
        let mut m = Meters::default();
        for &(i, v) in b {
            m.bank_b[i] = v;
        }
        let out = e.daemon(
            DaemonMsg::Meters {
                meters: Box::new(m),
                afx: Box::new(AfxMeters::default()),
            },
            now,
        );
        midi_outs(&out)
            .into_iter()
            .map(|(_, b)| (b[1], b[2]))
            .filter(|l| l.1 != 0)
            .collect::<Vec<_>>()
    };
    // ADAT OUT 4 at -5, ADAT OUT 1 (not the bus) at 0: no clip.
    assert_eq!(
        frame(&mut e, &[(3, 5), (0, 0)], t),
        vec![(86, 21), (85, 21), (84, 13)]
    );
    // The bus on bank A directly (source 10 + 3): its R side clips.
    st.status.meter_sources[MeterBank::A as usize] = SourceGroup::Mix4 as u8;
    st.status.meter_sources[MeterBank::B as usize] = 7;
    let t1 = t + Duration::from_millis(60);
    e.daemon(
        DaemonMsg::State {
            state: Box::new(st),
        },
        t1,
    );
    let mut m = Meters::default();
    m.bank_a[1] = 0;
    let out = e.daemon(
        DaemonMsg::Meters {
            meters: Box::new(m),
            afx: Box::new(AfxMeters::default()),
        },
        t1,
    );
    let lit: Vec<u8> = midi_outs(&out)
        .into_iter()
        .filter(|(_, b)| b[2] != 0)
        .map(|(_, b)| b[1])
        .collect();
    assert_eq!(lit, vec![83, 82]);
}

#[test]
fn device_status_updates_mirror() {
    let (mut e, t) = engine(Vec::new());
    let mut status = e.state().unwrap().status.clone();
    status.meter_sources[0] = 11;
    e.daemon(
        DaemonMsg::Status {
            status: Box::new(status),
        },
        t,
    );
    assert_eq!(e.state().unwrap().status.meter_sources[0], 11);
}
