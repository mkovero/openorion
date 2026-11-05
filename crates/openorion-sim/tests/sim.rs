//! Simulator behaviour: answers, sets, quirks and fault injection.

use std::time::Duration;

use openorion_hid::Transport;
use openorion_proto::decode::{apply_reply, decode_reply, empty_state};
use openorion_proto::*;
use openorion_sim::*;

const MS: Duration = Duration::from_millis(1);

fn sim() -> (SimTransport, SimHandle) {
    let t = SimTransport::fixture().expect("fixture loads");
    let h = t.handle();
    (t, h)
}

fn fixture_state() -> DeviceState {
    let text = std::fs::read_to_string(fixture_path("dump1.json")).unwrap();
    state_from_dump_json(&text).unwrap()
}

/// Send `get` and wait (≤ 200 ms sim time) for its reply.
fn request(t: &mut SimTransport, get: Get) -> Option<[u8; REPORT_LEN]> {
    t.write(&get.report()).unwrap();
    wait_for(t, 200 * MS, |r| {
        get.matches(r) || parse(r) == Incoming::UnknownCmd
    })
}

/// Read until `pred` matches or `budget` of sim time passes.
fn wait_for(
    t: &mut SimTransport,
    budget: Duration,
    mut pred: impl FnMut(&[u8; REPORT_LEN]) -> bool,
) -> Option<[u8; REPORT_LEN]> {
    let h = t.handle();
    let end = h.now() + budget;
    while h.now() < end {
        let left = end - h.now();
        match t.read(left).unwrap() {
            Some(r) if pred(&r) => return Some(r),
            Some(_) => {}
            None => return None,
        }
    }
    None
}

fn status(t: &mut SimTransport) -> Status {
    let r = wait_for(t, 50 * MS, |r| matches!(parse(r), Incoming::Status(..))).unwrap();
    match parse(&r) {
        Incoming::Status(s, _) => *s,
        _ => unreachable!(),
    }
}

fn send(t: &mut SimTransport, set: &Set) {
    t.write(&set.report().unwrap()).unwrap();
}

#[test]
fn full_read_decodes_to_fixture_state() {
    let (mut t, h) = sim();
    let mut got = empty_state();
    for get in Get::read_all() {
        let r = request(&mut t, get).unwrap_or_else(|| panic!("no reply to {get:?}"));
        assert!(get.matches(&r), "{get:?}: {:?}", parse(&r));
        let reply = decode_reply(&get, &r[16..]).unwrap();
        apply_reply(&mut got, reply);
    }
    // Status comes from cyclic, not gets.
    got.status = h.state().status;
    assert_eq!(got, h.state());

    // Apart from the filled-in AFX defaults, the sim state is the fixture.
    let mut fx = fixture_state();
    let mut sim_state = h.state();
    fx.afx.configs.clear();
    sim_state.afx.configs.clear();
    assert_eq!(sim_state, fx);
    assert!(h.rejected().is_empty());
}

#[test]
fn reply_latency_is_about_28ms() {
    let (mut t, h) = sim();
    let t0 = h.now();
    let r = request(&mut t, Get::Firmware).unwrap();
    assert!(Get::Firmware.matches(&r));
    assert_eq!(h.now() - t0, REPLY_LATENCY);
}

#[test]
fn licensing_and_unknown_gets_answer_0x61() {
    let (mut t, h) = sim();
    let mut raw = [0u8; REPORT_LEN];
    raw[0] = 0x74;
    raw[4] = 0x10;
    raw[8] = 17;
    t.write_raw(&raw);
    let r = wait_for(&mut t, 100 * MS, |r| {
        !matches!(parse(r), Incoming::Status(..) | Incoming::AfxMeters(_))
    })
    .unwrap();
    assert_eq!(parse(&r), Incoming::UnknownCmd);
    assert_eq!(&r[8..12], &17u32.to_le_bytes());
    raw[8] = 99;
    t.write_raw(&raw);
    let r = wait_for(&mut t, 100 * MS, |r| {
        !matches!(parse(r), Incoming::Status(..) | Incoming::AfxMeters(_))
    })
    .unwrap();
    assert_eq!(parse(&r), Incoming::UnknownCmd);
    assert!(!h.is_stuck());
}

#[test]
fn set_then_readback() {
    let (mut t, h) = sim();
    let m3 = MixerId::new(3).unwrap();
    let mut ch = h.state().mixers[3].channels[4];
    ch.level = Atten::new(17).unwrap();
    ch.mute = !ch.mute;
    send(&mut t, &Set::MixerChannel(m3, 5, ch));
    // Not applied before ~25 ms.
    h.advance(20 * MS);
    assert_ne!(h.state().mixers[3].channels[4], ch);
    let r = request(&mut t, Get::Mixer(m3)).unwrap();
    match decode_reply(&Get::Mixer(m3), &r[16..]).unwrap() {
        decode::GetReply::Mixer(_, mx) => assert_eq!(mx.channels[4], ch),
        other => panic!("{other:?}"),
    }
    assert_eq!(h.sets(), vec![Set::MixerChannel(m3, 5, ch)]);
    assert_eq!(h.writes().len(), 2);
    assert_eq!(h.writes()[1].get_ext(), Some((4, 3)));
}

#[test]
fn sets_never_reply() {
    let (mut t, _h) = sim();
    send(&mut t, &Set::Brightness(Brightness::new(2).unwrap()));
    send(
        &mut t,
        &Set::StereoLink(
            LinkTarget::Mixer {
                mixer: MixerId::new(3).unwrap(),
                pair: 0,
            },
            true,
        ),
    );
    let other = wait_for(&mut t, 300 * MS, |r| {
        !matches!(parse(r), Incoming::Status(..) | Incoming::AfxMeters(_))
    });
    assert!(other.is_none(), "unexpected {:?}", other.map(|r| parse(&r)));
}

#[test]
fn status_backed_set_and_current_preset_cleared() {
    let (mut t, h) = sim();
    h.external_change(|s| s.status.current_preset = 2);
    send(&mut t, &Set::Brightness(Brightness::new(1).unwrap()));
    h.advance(30 * MS);
    let st = status(&mut t);
    assert_eq!(st.brightness, 1);
    assert_eq!(st.current_preset, 0);
}

#[test]
#[should_panic(expected = "poisoned")]
fn non_320_write_panics() {
    let (mut t, _h) = sim();
    t.write_raw(&[0x74; 64]);
}

#[test]
fn non_320_write_poisons_until_power_cycle() {
    let (mut t, h) = sim();
    let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| t.write_raw(&[0; 64])));
    assert!(r.is_err());
    assert!(h.is_poisoned() && h.is_stuck());
    // Cyclic still flows, requests go unanswered.
    assert!(request(&mut t, Get::Firmware).is_none());
    // A reset gives at most one answer.
    t.reset().unwrap();
    assert!(request(&mut t, Get::Firmware).is_some());
    assert!(request(&mut t, Get::Firmware).is_none());
    // Power cycle, then the wake reset.
    h.power_cycle();
    assert!(!h.is_poisoned());
    assert!(t.read(50 * MS).unwrap().is_none());
    t.reset().unwrap();
    assert!(request(&mut t, Get::Firmware).is_some());

    // Without panicking.
    h.set_panic_on_bad_write(false);
    t.write_raw(&[0; 321]);
    assert!(h.is_poisoned());
    assert!(request(&mut t, Get::Firmware).is_none());
}

#[test]
fn hang_after_n() {
    let (mut t, h) = sim();
    h.hang_after(2);
    assert!(request(&mut t, Get::Firmware).is_some());
    assert!(request(&mut t, Get::Passport).is_some());
    assert!(request(&mut t, Get::Trims).is_none());
    assert!(h.is_stuck() && !h.is_poisoned());
    assert_eq!(h.answered(), 2);
    // Cyclic keeps flowing.
    assert!(t.read(10 * MS).unwrap().is_some());
    // Sets are swallowed too.
    send(&mut t, &Set::Brightness(Brightness::new(3).unwrap()));
    h.advance(50 * MS);
    assert_ne!(h.state().status.brightness, 3);
}

#[test]
fn no_cyclic_until_reset_then_zero_then_cyclic() {
    let (mut t, h) = sim();
    h.no_cyclic_until_reset();
    assert!(t.read(500 * MS).unwrap().is_none());
    assert!(request(&mut t, Get::Firmware).is_none());
    t.reset().unwrap();
    let first = t.read(20 * MS).unwrap().unwrap();
    assert_eq!(parse(&first), Incoming::Zero);
    let next = t.read(20 * MS).unwrap().unwrap();
    assert!(matches!(parse(&next), Incoming::Status(..)));
    assert!(request(&mut t, Get::Firmware).is_some());
}

#[test]
fn drop_cyclic_is_unplug() {
    let (mut t, h) = sim();
    h.drop_cyclic();
    assert!(t.read(100 * MS).unwrap().is_none());
    assert!(t.reset().is_err());
    h.replug();
    t.reset().unwrap();
    assert_eq!(parse(&t.read(20 * MS).unwrap().unwrap()), Incoming::Zero);
}

#[test]
fn preset_save_and_recall() {
    let (mut t, h) = sim();
    let m3 = MixerId::new(3).unwrap();
    let orig = h.state().mixers[3].channels[0];
    send(&mut t, &Set::PresetSave(PresetSlot::new(4).unwrap()));
    h.advance(30 * MS);
    assert_eq!(status(&mut t).current_preset, 4);

    let mut ch = orig;
    ch.level = Atten::new(40).unwrap();
    send(&mut t, &Set::MixerChannel(m3, 1, ch));
    h.advance(30 * MS);
    assert_eq!(h.state().mixers[3].channels[0], ch);
    assert_eq!(status(&mut t).current_preset, 0);

    send(&mut t, &Set::PresetRecall(PresetSlot::new(4).unwrap()));
    h.advance(30 * MS);
    let st = status(&mut t);
    assert!(st.device_updated);
    assert_eq!(st.current_preset, 4);
    assert_eq!(h.state().mixers[3].channels[0], orig);
    assert_eq!(h.preset(4).unwrap().mixers[3].channels[0], orig);
}

#[test]
fn afx_removal_disables_instance() {
    let (mut t, h) = sim();
    let slot = AfxSlot {
        ty: AfxType::Compressor,
        inst: 0,
    };
    let mut slots = [None; AFX_SLOTS];
    slots[0] = Some(slot);
    send(&mut t, &Set::AfxOrder { chain: 15, slots });
    h.advance(30 * MS);
    assert!(h.state().afx.configs[&AfxType::Compressor][0].enabled);
    send(
        &mut t,
        &Set::AfxOrder {
            chain: 15,
            slots: [None; AFX_SLOTS],
        },
    );
    h.advance(30 * MS);
    assert!(!h.state().afx.configs[&AfxType::Compressor][0].enabled);
    // And a get reads it back disabled.
    let get = Get::AfxConfigs(AfxType::Compressor, 0);
    let r = request(&mut t, get).unwrap();
    match decode_reply(&get, &r[16..]).unwrap() {
        decode::GetReply::AfxConfigs { configs, .. } => assert!(!configs[0].enabled),
        other => panic!("{other:?}"),
    }
}

#[test]
fn cyclic_period_is_8ms_and_afx_4ms_after() {
    let (mut t, h) = sim();
    let mut status_at = Vec::new();
    let mut afx_at = Vec::new();
    while status_at.len() < 20 {
        let r = t.read(100 * MS).unwrap().unwrap();
        match parse(&r) {
            Incoming::Status(..) => status_at.push(h.now()),
            Incoming::AfxMeters(_) => afx_at.push(h.now()),
            other => panic!("{other:?}"),
        }
    }
    for w in status_at.windows(2) {
        assert_eq!(w[1] - w[0], CYCLIC_PERIOD);
    }
    for (s, a) in status_at.iter().zip(&afx_at) {
        assert_eq!(*a - *s, AFX_METER_OFFSET);
    }
}

#[test]
fn meters_move_and_rest_at_floor() {
    let (mut t, _h) = sim();
    let mut seen_a0 = std::collections::BTreeSet::new();
    let mut afx_peak = std::collections::BTreeSet::new();
    for _ in 0..400 {
        let r = t.read(100 * MS).unwrap().unwrap();
        match parse(&r) {
            Incoming::Status(_, m) => {
                seen_a0.insert(m.bank_a[0]);
                assert_eq!(m.bank_a[31], METER_FLOOR);
                assert_eq!(m.mixer[20], METER_FLOOR);
            }
            Incoming::AfxMeters(m) => {
                afx_peak.insert(m.peak[0][0]);
                assert_eq!(m.peak[15][7], METER_FLOOR);
            }
            _ => {}
        }
    }
    assert!(seen_a0.len() > 10, "{seen_a0:?}");
    assert!(afx_peak.len() > 10);
}

#[test]
fn external_change_visible_in_status_and_get() {
    let (mut t, h) = sim();
    h.external_change(|s| {
        s.status.main.atten = Atten::new(33).unwrap();
        s.mixers[0].master.level = Atten::new(12).unwrap();
    });
    assert_eq!(status(&mut t).main.atten.get(), 33);
    let g = Get::Mixer(MixerId::new(0).unwrap());
    let r = request(&mut t, g).unwrap();
    match decode_reply(&g, &r[16..]).unwrap() {
        decode::GetReply::Mixer(_, mx) => assert_eq!(mx.master.level.get(), 12),
        other => panic!("{other:?}"),
    }
}

#[test]
fn backlog_is_bounded() {
    let (mut t, h) = sim();
    h.advance(Duration::from_secs(10));
    let mut n = 0;
    while let Some(r) = t.read(Duration::ZERO).unwrap() {
        assert!(matches!(
            parse(&r),
            Incoming::Status(..) | Incoming::AfxMeters(_)
        ));
        n += 1;
    }
    assert!(
        n <= 2 * (CYCLIC_BACKLOG.as_millis() as usize / 8 + 1),
        "{n}"
    );
}

#[test]
fn real_time_mode_delivers_cyclic() {
    let mut t = SimTransport::fixture()
        .unwrap()
        .with_clock(SimClock::RealTime);
    let start = std::time::Instant::now();
    let mut n = 0;
    while n < 6 {
        if t.read(Duration::from_millis(100)).unwrap().is_some() {
            n += 1;
        }
    }
    // 3 status + 3 AFX reports take ≥ ~16 ms of wall time.
    assert!(start.elapsed() >= Duration::from_millis(14));
    let r = request_rt(&mut t, Get::Firmware);
    assert!(r.is_some());
}

fn request_rt(t: &mut SimTransport, get: Get) -> Option<[u8; REPORT_LEN]> {
    t.write(&get.report()).unwrap();
    let end = std::time::Instant::now() + Duration::from_millis(500);
    while std::time::Instant::now() < end {
        if let Some(r) = t.read(Duration::from_millis(50)).unwrap()
            && get.matches(&r)
        {
            return Some(r);
        }
    }
    None
}

#[test]
fn transport_is_send() {
    fn is_send<T: Send>() {}
    is_send::<SimTransport>();
    is_send::<SimHandle>();
}
