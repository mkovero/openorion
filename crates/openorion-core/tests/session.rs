//! Session tests against the scripted fake transport (tests/support).
//! Real time, short timings: the transport runs on its own thread, so
//! tokio's paused clock would race it.

mod support;

use std::time::Duration;

use openorion_core::{Error, Event, Session, SessionOpts, build_sets, plan_restore};
use openorion_ipc::{Health, Op};
use openorion_proto::decode::{apply_set, default_afx_config};
use openorion_proto::*;
use support::*;

#[tokio::test]
async fn ops_refused_before_first_sync_then_hung() {
    let fake = Fake::new();
    fake.dev().answer_gets = false;
    let dir = tempfile::tempdir().unwrap();
    let (h, _rx) = Session::start(fake.transport(), opts(&dir)).await.unwrap();
    let r = h.op(mixer_level(m3(), 1, 30)).await;
    assert!(matches!(r, Err(Error::NotSynced(_))), "{r:?}");
    assert!(h.snapshot().is_err());
    let r = h.wait_synced().await;
    assert!(matches!(r, Err(Error::Hung)), "{:?}", r.err());
    assert!(
        fake.dev().sets.is_empty(),
        "nothing set from an unsynced cache"
    );
}

const ADAT8: Source = Source::Input {
    group: SourceGroup::AdatIn,
    chan: 7,
};

fn atten(v: u8) -> Atten {
    Atten::new(v).unwrap()
}

fn m3() -> MixerId {
    MixerId::new(3).unwrap()
}

fn mixer_level(mixer: MixerId, ch: u8, v: u8) -> Op {
    Op::Mixer {
        mixer,
        ch,
        level: Some(atten(v)),
        pan: None,
        mute: None,
        solo: None,
        send: None,
    }
}

/// The fixture with the AFX configs a sync reads (the dump has none; the
/// fake answers them with factory configs).
fn expected_synced(fixture: &DeviceState) -> DeviceState {
    let mut s = fixture.clone();
    for (&ty, &n) in &fixture.afx.instances {
        if n == 0 || !ty.on_orion() {
            continue;
        }
        if let Some(d) = default_afx_config(ty) {
            s.afx.configs.insert(ty, vec![d; 16]);
        }
    }
    s
}

#[tokio::test]
async fn startup_and_full_sync_match_fixture() {
    let h = synced().await;
    let fixture = fixture_state();
    let state = h.handle.state();
    assert_eq!(state, expected_synced(&fixture));
    assert_eq!(h.handle.health().health, Health::Ok);

    let d = h.fake.dev();
    assert_eq!(d.resets, 0);
    assert!(d.sets.is_empty(), "a sync never sets");
    // Panel order: info, routing 0..14, mixers, links, AFX, reverb, trims, HDX.
    let g = &d.gets;
    assert_eq!(&g[..3], &[Get::Firmware, Get::Passport, Get::Registration]);
    for (i, b) in OutputBank::ALL.iter().enumerate() {
        assert_eq!(g[3 + i], Get::Routing(*b));
    }
    assert_eq!(g[18], Get::Mixer(MixerId::new(0).unwrap()));
    assert_eq!(g[22], Get::MixerLinks);
    assert_eq!(g[23], Get::AfxOrder);
    assert_eq!(g[24], Get::AfxLinks);
    assert_eq!(g[25], Get::AfxInstances);
    assert_eq!(g[26], Get::AfxConfigs(AfxType::Equalizer, 0));
    assert_eq!(g[27], Get::AfxConfigs(AfxType::Equalizer, 1));
    let n = g.len();
    assert_eq!(&g[n - 3..], &[Get::Reverb, Get::Trims, Get::HdxDelays]);
}

#[tokio::test]
async fn synced_event_carries_state() {
    let fake = Fake::new();
    let dir = tempfile::tempdir().unwrap();
    let (_h, mut rx) = Session::start(fake.transport(), opts(&dir)).await.unwrap();
    let e = wait_event(&mut rx, Duration::from_secs(10), |e| {
        matches!(e, Event::Synced(_))
    })
    .await
    .expect("Synced");
    let Event::Synced(s) = e else { unreachable!() };
    assert_eq!(*s, expected_synced(&fixture_state()));
}

#[tokio::test]
async fn no_cyclic_resets_once_and_recovers() {
    let fake = Fake::new();
    fake.dev().cyclic = false;
    let h = synced_with(fake).await;
    assert_eq!(h.fake.dev().resets, 1);
}

#[tokio::test]
async fn no_cyclic_even_after_reset_fails() {
    let fake = Fake::new();
    {
        let mut d = fake.dev();
        d.cyclic = false;
        d.cyclic_after_reset = false;
    }
    let dir = tempfile::tempdir().unwrap();
    let r = Session::start(fake.transport(), opts(&dir)).await;
    assert!(matches!(r, Err(Error::NoCyclic)), "{:?}", r.err());
    let d = fake.dev();
    assert_eq!(d.resets, 1);
    assert!(d.writes.is_empty());
}

#[tokio::test]
async fn lock_contention() {
    let dir = tempfile::tempdir().unwrap();
    let (a, _rx) = Session::start(Fake::new().transport(), opts(&dir))
        .await
        .unwrap();
    let r = Session::start(Fake::new().transport(), opts(&dir)).await;
    match r {
        Err(Error::Locked(p)) => assert_eq!(p, dir.path().join("openorion.lock")),
        other => panic!("expected Locked, got {:?}", other.err()),
    }
    // Released when the session stops.
    a.shutdown();
    let mut ok = false;
    for _ in 0..50 {
        sleep_ms(20).await;
        if let Ok((b, _)) = Session::start(Fake::new().transport(), opts(&dir)).await {
            b.shutdown();
            ok = true;
            break;
        }
    }
    assert!(ok, "lock released after shutdown");
}

/// The set reports written since `from`.
fn set_reports(fake: &Fake, from: usize) -> Vec<[u8; REPORT_LEN]> {
    fake.dev().writes[from..]
        .iter()
        .filter(|w| w[0] == 0x70)
        .copied()
        .collect()
}

#[tokio::test]
async fn op_mixer4_ch1_level_exact_bytes() {
    let h = synced().await;
    let before = h.fake.dev().writes.len();
    h.handle.op(mixer_level(m3(), 1, 30)).await.unwrap();
    sleep_ms(60).await;
    let sets = set_reports(&h.fake, before);
    assert_eq!(sets.len(), 1);
    let fx = fixture_state();
    let mut ch = fx.mixers[3].channels[0];
    ch.level = atten(30);
    let want = Set::MixerChannel(m3(), 1, ch).report().unwrap();
    assert_eq!(&sets[0], want.as_bytes());
    // Header and the documented prefix: d1 05 03 01 1e …
    assert_eq!(&sets[0][..4], &[0x70, 0, 0, 0]);
    assert_eq!(&sets[0][16..21], &[0xd1, 0x05, 0x03, 0x01, 0x1e]);
    // Cache updated at once.
    assert_eq!(h.handle.state().mixers[3].channels[0].level, atten(30));
}

#[tokio::test]
async fn op_routing_bank14_exact_bytes() {
    let h = synced().await;
    let before = h.fake.dev().writes.len();
    let src = Source::Input {
        group: SourceGroup::LineIn,
        chan: 5,
    };
    h.handle
        .op(Op::Route {
            bank: OutputBank::MixCh4,
            ch: 2,
            sources: vec![src],
        })
        .await
        .unwrap();
    sleep_ms(60).await;
    let sets = set_reports(&h.fake, before);
    assert_eq!(sets.len(), 1);
    let mut v = fixture_state().routing[14].sources.clone();
    v[2] = src;
    let want = Set::Routing(OutputBank::MixCh4, v).report().unwrap();
    assert_eq!(&sets[0], want.as_bytes());
    assert_eq!(&sets[0][16..19], &[0xd2, 0x41, 0x0e]);
    assert_eq!(&sets[0][19 + 4..19 + 6], &[0x00, 0x05]);
}

#[tokio::test]
async fn op_trim_line_out_exact_bytes() {
    let h = synced().await;
    let before = h.fake.dev().writes.len();
    let level = TrimLevel { whole: 9, fract: 0 };
    h.handle
        .op(Op::Trim {
            id: TrimId::LineOut,
            ch: Some(0),
            level,
        })
        .await
        .unwrap();
    sleep_ms(60).await;
    let sets = set_reports(&h.fake, before);
    assert_eq!(sets.len(), 1);
    let mut rec = fixture_state().trims.line_out.clone();
    rec.levels[0] = level;
    let want = Set::Trim(TrimId::LineOut, rec.clone()).report().unwrap();
    assert_eq!(&sets[0], want.as_bytes());
    assert_eq!(
        &sets[0][16..21],
        &[0xcc, 0x42, 0x01, rec.control.as_u8(), 0x09]
    );
}

#[tokio::test]
async fn monitor_trim_refused() {
    let h = synced().await;
    let r = h
        .handle
        .op(Op::Trim {
            id: TrimId::Monitor,
            ch: None,
            level: TrimLevel::default(),
        })
        .await;
    assert!(matches!(r, Err(Error::Unsupported(_))));
}

#[tokio::test]
async fn coalescing_latest_wins() {
    let h = synced().await;
    let before = h.fake.dev().writes.len();
    for i in 0..100u8 {
        h.handle.op(mixer_level(m3(), 1, i % 90)).await.unwrap();
    }
    h.handle.op(mixer_level(m3(), 1, 42)).await.unwrap();
    sleep_ms(400).await;
    let sets: Vec<Set> = set_reports(&h.fake, before)
        .iter()
        .map(|r| Set::decode(r).unwrap())
        .collect();
    assert!(
        !sets.is_empty() && sets.len() <= 10,
        "{} sends for 101 moves",
        sets.len()
    );
    match sets.last().unwrap() {
        Set::MixerChannel(m, 1, c) => {
            assert_eq!(*m, m3());
            assert_eq!(c.level, atten(42));
        }
        s => panic!("unexpected {s:?}"),
    }
    assert_eq!(h.fake.dev().state.mixers[3].channels[0].level, atten(42));
    assert_eq!(h.handle.state().mixers[3].channels[0].level, atten(42));
}

#[tokio::test]
async fn verify_reads_back_and_confirms() {
    let mut h = synced().await;
    let before = h.fake.dev().gets.len();
    h.handle.op(mixer_level(m3(), 4, 12)).await.unwrap();
    let mism = wait_event(&mut h.events, Duration::from_millis(500), |e| {
        matches!(e, Event::Mismatch { .. })
    })
    .await;
    assert!(mism.is_none(), "{mism:?}");
    let gets = h.fake.dev().gets[before..].to_vec();
    assert_eq!(gets, vec![Get::Mixer(m3())], "one covering read-back");
}

#[tokio::test]
async fn verify_mismatch_takes_device_value() {
    let mut h = synced().await;
    h.fake.dev().apply_sets = false;
    h.handle.op(mixer_level(m3(), 1, 30)).await.unwrap();
    assert_eq!(h.handle.state().mixers[3].channels[0].level, atten(30));
    let e = wait_event(&mut h.events, Duration::from_secs(2), |e| {
        matches!(e, Event::Mismatch { .. })
    })
    .await
    .expect("mismatch event");
    let Event::Mismatch { path, ours, device } = e else {
        unreachable!()
    };
    assert_eq!(path, "/mixers/3");
    assert_eq!(ours["channels"][0]["level"], 30);
    let orig = fixture_state().mixers[3].channels[0].level;
    assert_eq!(device["channels"][0]["level"], orig.get());
    assert_eq!(h.handle.state().mixers[3].channels[0].level, orig);
}

#[tokio::test]
async fn status_backed_verify_mismatch() {
    let mut h = synced().await;
    h.fake.dev().apply_sets = false;
    h.handle
        .op(Op::Volume {
            id: VolumeId::Line,
            atten: atten(7),
        })
        .await
        .unwrap();
    assert_eq!(h.handle.state().status.line.atten, atten(7));
    let e = wait_event(&mut h.events, Duration::from_secs(2), |e| {
        matches!(e, Event::Mismatch { .. })
    })
    .await
    .expect("mismatch event");
    let Event::Mismatch { path, .. } = e else {
        unreachable!()
    };
    assert_eq!(path, "/status");
    sleep_ms(30).await;
    assert_eq!(
        h.handle.state().status.line.atten,
        fixture_state().status.line.atten
    );
}

#[tokio::test]
async fn status_backed_op_reaches_device() {
    let mut h = synced().await;
    h.handle
        .op(Op::Volume {
            id: VolumeId::Line,
            atten: atten(7),
        })
        .await
        .unwrap();
    let m = wait_event(&mut h.events, Duration::from_millis(500), |e| {
        matches!(e, Event::Mismatch { .. })
    })
    .await;
    assert!(m.is_none(), "{m:?}");
    assert_eq!(h.fake.dev().state.status.line.atten, atten(7));
    assert_eq!(h.handle.state().status.line.atten, atten(7));
}

#[tokio::test]
async fn hang_detection_stops_writes() {
    let mut h = synced().await;
    h.fake.dev().answer_gets = false;
    // The resync's first get times out twice.
    let handle = h.handle.clone();
    let r = tokio::spawn(async move { handle.op(Op::Resync).await });
    let e = wait_event(&mut h.events, Duration::from_secs(3), |e| {
        matches!(
            e,
            Event::Health {
                health: Health::Hung,
                ..
            }
        )
    })
    .await;
    assert!(e.is_some(), "Hung reported");
    assert!(matches!(r.await.unwrap(), Err(Error::Hung)));
    let n = h.fake.dev().writes.len();
    let gets_after_sync = h.fake.dev().gets.len();
    assert!(gets_after_sync >= 2);
    let r = h.handle.op(mixer_level(m3(), 1, 5)).await;
    assert!(matches!(r, Err(Error::Hung)));
    sleep_ms(400).await;
    assert_eq!(h.fake.dev().writes.len(), n, "no writes once hung");
    assert_eq!(h.handle.health().health, Health::Hung);
}

async fn wait_stalled(h: &mut Harness) {
    let e = wait_event(&mut h.events, Duration::from_secs(3), |e| {
        matches!(
            e,
            Event::Health {
                health: Health::Stalled,
                ..
            }
        )
    })
    .await;
    assert!(e.is_some(), "Stalled reported");
}

#[tokio::test]
async fn cyclic_gap_reports_without_reset() {
    let mut h = synced().await;
    h.fake.dev().cyclic = false;
    wait_stalled(&mut h).await;
    // Settings are refused, not queued, while nothing can be sent.
    let r = h.handle.op(mixer_level(m3(), 1, 5)).await;
    assert!(matches!(r, Err(Error::Stalled)), "{r:?}");
    assert!(!h.handle.health().health.writable());
    // No automatic reset: it would cut a running USB audio stream.
    tokio::time::sleep(Duration::from_millis(800)).await;
    assert_eq!(h.fake.dev().resets, 0);
    // Cyclic reports coming back on their own resync the state.
    h.fake.dev().cyclic = true;
    let e = wait_event(&mut h.events, Duration::from_secs(5), |e| {
        matches!(e, Event::Synced(_))
    })
    .await;
    assert!(e.is_some(), "resynced once cyclic is back");
    assert_eq!(h.fake.dev().resets, 0);
    assert_eq!(h.handle.health().health, Health::Ok);
}

#[tokio::test]
async fn reset_op_resets_and_resyncs() {
    let mut h = synced().await;
    {
        let mut d = h.fake.dev();
        d.cyclic = false;
        d.cyclic_after_reset = true;
    }
    wait_stalled(&mut h).await;
    tokio::time::timeout(
        Duration::from_secs(5),
        h.handle.op(Op::Reset { force: false }),
    )
    .await
    .expect("answered after the resync")
    .unwrap();
    assert_eq!(h.fake.dev().resets, 1);
    assert_eq!(h.handle.health().health, Health::Ok);
}

#[tokio::test]
async fn reset_refused_while_usb_audio_streams() {
    let fake = Fake::new();
    let dir = tempfile::tempdir().unwrap();
    let opts = SessionOpts {
        audio_busy: || Some("playback running (card 2)".into()),
        ..opts(&dir)
    };
    let (h, _rx) = Session::start(fake.transport(), opts).await.unwrap();
    h.wait_synced().await.unwrap();
    let r = h.op(Op::Reset { force: false }).await;
    assert!(matches!(r, Err(Error::ResetBlocked(_))), "{r:?}");
    assert_eq!(fake.dev().resets, 0);
    h.op(Op::Reset { force: true }).await.unwrap();
    assert_eq!(fake.dev().resets, 1);
}

#[tokio::test]
async fn startup_reset_refused_while_usb_audio_streams() {
    let fake = Fake::new();
    fake.dev().cyclic = false;
    let dir = tempfile::tempdir().unwrap();
    let opts = SessionOpts {
        audio_busy: || Some("capture running (card 2)".into()),
        ..opts(&dir)
    };
    let r = Session::start(fake.transport(), opts).await;
    assert!(matches!(r, Err(Error::ResetBlocked(_))));
    assert_eq!(fake.dev().resets, 0);
}

#[tokio::test]
async fn resync_on_device_updated_edge() {
    let mut h = synced().await;
    {
        let mut d = h.fake.dev();
        d.state.mixers[3].channels[1].level = atten(33);
        d.state.status.device_updated = true;
        d.state.status.current_preset = 2;
    }
    let e = wait_event(&mut h.events, Duration::from_secs(5), |e| {
        matches!(e, Event::Synced(_))
    })
    .await
    .expect("resync");
    let Event::Synced(s) = e else { unreachable!() };
    assert_eq!(s.mixers[3].channels[1].level, atten(33));
    // While the flag stays set, no further resyncs.
    let again = wait_event(&mut h.events, Duration::from_millis(400), |e| {
        matches!(e, Event::Synced(_))
    })
    .await;
    assert!(again.is_none());
}

#[tokio::test]
async fn our_preset_recall_resyncs_once() {
    let mut h = synced().await;
    let gets0 = h.fake.dev().gets.len();
    h.handle
        .op(Op::PresetRecall {
            slot: PresetSlot::new(5).unwrap(),
        })
        .await
        .unwrap();
    assert!(
        wait_event(&mut h.events, Duration::from_secs(5), |e| matches!(
            e,
            Event::Synced(_)
        ))
        .await
        .is_some()
    );
    let d = h.fake.dev();
    assert_eq!(d.sets, vec![Set::PresetRecall(PresetSlot::new(5).unwrap())]);
    assert!(d.gets.len() > gets0);
}

#[tokio::test]
async fn afx_remove_sends_bypass_enable() {
    let mut h = synced().await;
    let eq0 = AfxSlot {
        ty: AfxType::Equalizer,
        inst: 0,
    };
    let comp0 = AfxSlot {
        ty: AfxType::Compressor,
        inst: 0,
    };
    h.handle
        .op(Op::AfxChain {
            chain: 15,
            slots: vec![eq0, comp0],
        })
        .await
        .unwrap();
    h.handle
        .op(Op::AfxChain {
            chain: 15,
            slots: vec![eq0],
        })
        .await
        .unwrap();
    sleep_ms(100).await;
    let sets = h.fake.sets();
    let mut want_slots = [None; AFX_SLOTS];
    want_slots[0] = Some(eq0);
    let i = sets
        .iter()
        .position(|s| {
            *s == Set::AfxOrder {
                chain: 15,
                slots: want_slots,
            }
        })
        .expect("order set");
    assert_eq!(
        sets[i + 1],
        Set::AfxBypass {
            ty: AfxType::Compressor,
            inst: 0,
            enabled: true
        }
    );
    assert!(h.fake.dev().state.afx.configs[&AfxType::Compressor][0].enabled);
    assert!(h.handle.state().afx.configs[&AfxType::Compressor][0].enabled);
    let m = wait_event(&mut h.events, Duration::from_millis(400), |e| {
        matches!(e, Event::Mismatch { .. })
    })
    .await;
    assert!(m.is_none(), "{m:?}");
}

#[tokio::test]
async fn meters_and_status_events() {
    let fake = Fake::new();
    fake.dev().meters.bank_a[3] = 12;
    let mut h = synced_with(fake).await;
    let e = wait_event(&mut h.events, Duration::from_secs(1), |e| {
        matches!(e, Event::Meters { .. })
    })
    .await
    .expect("meters");
    let Event::Meters { meters, .. } = e else {
        unreachable!()
    };
    assert_eq!(meters.bank_a[3], 12);
    h.fake.dev().state.status.brightness = 77;
    let e = wait_event(
        &mut h.events,
        Duration::from_secs(1),
        |e| matches!(e, Event::Status(s) if s.brightness == 77),
    )
    .await;
    assert!(e.is_some());
}

#[tokio::test]
async fn restore_through_session_round_trip() {
    let h = synced().await;
    let snap = h.handle.snapshot().unwrap();
    h.handle.op(mixer_level(m3(), 1, 30)).await.unwrap();
    h.handle
        .op(Op::Route {
            bank: OutputBank::MixCh4,
            ch: 0,
            sources: vec![ADAT8],
        })
        .await
        .unwrap();
    sleep_ms(50).await;
    let dry = h.handle.restore(&snap, true).await.unwrap();
    assert_eq!(dry.len(), 2, "{dry:?}");
    let ops = h.handle.restore(&snap, false).await.unwrap();
    assert_eq!(ops, dry);
    sleep_ms(300).await;
    let d = h.fake.dev();
    assert_eq!(d.state.mixers, snap.state.mixers);
    assert_eq!(d.state.routing, snap.state.routing);
    drop(d);
    assert!(plan_restore(&h.handle.state(), &snap.state).is_empty());
}

// ---------------------------------------------------------------------------
// Pure: build and plan

#[test]
fn build_refuses_what_the_cache_lacks() {
    let fx = fixture_state(); // no AFX configs read
    let r = build_sets(
        &fx,
        &Op::AfxEqBand {
            inst: 0,
            band: 1,
            freq: Some(100),
            q: None,
            gain: None,
            ftype: None,
        },
    );
    assert!(matches!(r, Err(Error::NotSynced(_))), "{r:?}");
    let r = build_sets(&fx, &mixer_level(m3(), 33, 0));
    assert!(matches!(r, Err(Error::Invalid(_))));
}

fn apply_ops(state: &DeviceState, ops: &[Op]) -> DeviceState {
    let mut s = state.clone();
    for op in ops {
        for set in build_sets(&s, op).unwrap() {
            apply_set(&mut s, &set).unwrap();
        }
    }
    s
}

#[test]
fn plan_restore_round_trip() {
    let current = expected_synced(&fixture_state());
    assert!(plan_restore(&current, &current).is_empty());

    let eq0 = AfxSlot {
        ty: AfxType::Equalizer,
        inst: 0,
    };
    let comp1 = AfxSlot {
        ty: AfxType::Compressor,
        inst: 1,
    };
    let target = apply_ops(
        &current,
        &[
            mixer_level(m3(), 1, 30),
            Op::Mixer {
                mixer: m3(),
                ch: 0,
                level: Some(atten(20)),
                pan: None,
                mute: Some(true),
                solo: None,
                send: None,
            },
            Op::Route {
                bank: OutputBank::MixCh4,
                ch: 3,
                sources: vec![ADAT8, ADAT8],
            },
            Op::Trim {
                id: TrimId::LineOut,
                ch: Some(0),
                level: TrimLevel { whole: 9, fract: 0 },
            },
            Op::StereoLink {
                target: LinkTarget::Mixer {
                    mixer: m3(),
                    pair: 4,
                },
                linked: true,
            },
            Op::AfxChain {
                chain: 15,
                slots: vec![eq0, comp1],
            },
            Op::AfxEqBand {
                inst: 0,
                band: 2,
                freq: Some(2500),
                q: Some(150),
                gain: Some(-300),
                ftype: None,
            },
            Op::AfxBypass {
                ty: AfxType::Compressor,
                inst: 1,
                enabled: false,
            },
        ],
    );
    let plan = plan_restore(&current, &target);
    assert_eq!(plan.len(), 8, "{plan:#?}");
    let after = apply_ops(&current, &plan);
    assert!(plan_restore(&after, &target).is_empty());
    assert_eq!(after.mixers, target.mixers);
    assert_eq!(after.routing, target.routing);
    assert_eq!(after.trims, target.trims);
    assert_eq!(after.afx, target.afx);
    assert_eq!(after.mixer_links, target.mixer_links);

    // And back.
    let back = plan_restore(&target, &current);
    let restored = apply_ops(&target, &back);
    assert_eq!(restored.mixers, current.mixers);
    assert_eq!(restored.routing, current.routing);
    assert_eq!(restored.afx.order, current.afx.order);
    assert!(plan_restore(&restored, &current).is_empty());
}

#[test]
fn plan_restore_moves_afx_instance_between_chains() {
    let base = expected_synced(&fixture_state());
    let eq0 = AfxSlot {
        ty: AfxType::Equalizer,
        inst: 0,
    };
    let a = apply_ops(
        &base,
        &[Op::AfxChain {
            chain: 14,
            slots: vec![eq0],
        }],
    );
    let b = apply_ops(
        &base,
        &[Op::AfxChain {
            chain: 15,
            slots: vec![eq0],
        }],
    );
    let plan = plan_restore(&a, &b);
    // Chain 14 is emptied before chain 15 takes the instance.
    assert_eq!(
        plan,
        vec![
            Op::AfxChain {
                chain: 14,
                slots: vec![]
            },
            Op::AfxChain {
                chain: 15,
                slots: vec![eq0]
            },
        ]
    );
    let after = apply_ops(&a, &plan);
    assert_eq!(after.afx.order, b.afx.order);
    assert!(after.afx.configs[&AfxType::Equalizer][0].enabled);
}

#[test]
fn master_pan_and_send_are_refused() {
    let fx = expected_synced(&fixture_state());
    let master = |pan: Option<Pan>, send: Option<Atten>| Op::Mixer {
        mixer: m3(),
        ch: 0,
        level: None,
        pan,
        mute: None,
        solo: None,
        send,
    };
    let r = build_sets(&fx, &master(Some(Pan::new(10).unwrap()), None));
    assert!(matches!(r, Err(Error::Invalid(_))), "{r:?}");
    let r = build_sets(&fx, &master(None, Some(atten(20))));
    assert!(matches!(r, Err(Error::Invalid(_))), "{r:?}");
    // The device's own values pass (restores carry them).
    let m = fx.mixers[m3().get() as usize].master;
    build_sets(&fx, &master(Some(m.pan), Some(m.send))).unwrap();
}
