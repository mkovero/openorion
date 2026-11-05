//! Control-channel health: idle probe, recheck (soft recovery), 0x61 while
//! booting, and what happens to settings across a hang.

mod support;

use std::time::Duration;

use openorion_core::{Error, Event, Session};
use openorion_ipc::{Advice, Health, Op};
use openorion_proto::*;
use support::*;

fn level(v: u8) -> Op {
    Op::Mixer {
        mixer: MixerId::new(3).unwrap(),
        ch: 1,
        level: Some(Atten::new(v).unwrap()),
        pan: None,
        mute: None,
        solo: None,
        send: None,
    }
}

async fn wait_health(h: &mut Harness, want: Health, within_ms: u64) -> bool {
    wait_event(
        &mut h.events,
        Duration::from_millis(within_ms),
        |e| matches!(e, Event::Health { health, .. } if *health == want),
    )
    .await
    .is_some()
}

#[tokio::test]
async fn idle_probe_keeps_checking_the_channel() {
    let h = synced().await;
    let before = h.fake.dev().gets.len();
    sleep_ms(1300).await;
    assert!(h.fake.dev().gets.len() >= before + 2, "probed while idle");
    assert!(h.fake.dev().sets.is_empty(), "probes are reads");
    assert_eq!(h.handle.health().health, Health::Ok);
}

/// A session whose idle re-read comes round quickly.
async fn fast_watch() -> Harness {
    let fake = Fake::new();
    let dir = tempfile::tempdir().unwrap();
    let mut o = opts(&dir);
    o.timings.probe_idle = Duration::from_millis(10);
    let (handle, events) = Session::start(fake.transport(), o).await.unwrap();
    handle.wait_synced().await.unwrap();
    Harness {
        fake,
        handle,
        events: events.resubscribe(),
        _dir: dir,
    }
}

#[tokio::test]
async fn idle_rereads_cover_every_block_without_false_resyncs() {
    let h = fast_watch().await;
    let syncs = h.handle.health().syncs;
    let before = h.fake.dev().gets.len();
    sleep_ms(2500).await;
    let d = h.fake.dev();
    let seen: std::collections::HashSet<Get> = d.gets[before..].iter().copied().collect();
    for g in [
        Get::Firmware,
        Get::Routing(OutputBank::LineOut),
        Get::Mixer(MixerId::new(3).unwrap()),
        Get::MixerLinks,
        Get::AfxOrder,
        Get::AfxInstances,
        Get::AfxConfigs(AfxType::Equalizer, 1),
        Get::AfxConfigs(AfxType::Compressor, 0),
        Get::Reverb,
        Get::Trims,
        Get::HdxDelays,
    ] {
        assert!(seen.contains(&g), "{g:?} re-read");
    }
    assert!(!seen.contains(&Get::Passport), "device info is fixed");
    assert!(d.sets.is_empty(), "reads only");
    drop(d);
    assert_eq!(
        h.handle.health().syncs,
        syncs,
        "an unchanged device is not resynced"
    );
}

#[tokio::test]
async fn change_behind_our_back_triggers_a_resync() {
    let h = fast_watch().await;
    let syncs = h.handle.health().syncs;
    // As if the front panel or another client changed it.
    h.fake.dev().state.mixers[3].channels[4].level = Atten::new(17).unwrap();
    let mut resynced = false;
    for _ in 0..60 {
        sleep_ms(50).await;
        if h.handle.health().syncs > syncs {
            resynced = true;
            break;
        }
    }
    assert!(resynced, "full resync");
    assert_eq!(
        h.handle.state().mixers[3].channels[4].level,
        Atten::new(17).unwrap(),
        "cache follows the device"
    );
    assert!(h.fake.dev().sets.is_empty());
}

#[tokio::test]
async fn idle_probe_finds_a_hang_before_any_setting() {
    let mut h = synced().await;
    h.fake.dev().answer_gets = false;
    assert!(
        wait_health(&mut h, Health::Hung, 2000).await,
        "hang found while idle"
    );
    assert!(h.fake.dev().sets.is_empty());
    let health = h.handle.health();
    assert_eq!(health.health.advice(), Advice::PowerCycle);
    assert!(!health.health.writable());
    assert!(matches!(h.handle.op(level(5)).await, Err(Error::Hung)));
}

#[tokio::test]
async fn recheck_recovers_a_transient_hang() {
    let mut h = synced().await;
    h.fake.dev().answer_gets = false;
    assert!(wait_health(&mut h, Health::Hung, 2000).await);
    h.fake.dev().answer_gets = true;
    tokio::time::timeout(Duration::from_secs(5), h.handle.op(Op::Recheck))
        .await
        .expect("answered")
        .expect("recovered");
    assert_eq!(h.handle.health().health, Health::Ok);
    assert_eq!(h.fake.dev().resets, 0, "no USB reset");
    h.handle.op(level(7)).await.unwrap();
    sleep_ms(300).await;
    assert!(h.fake.dev().sets.iter().any(|s| matches!(
        s,
        Set::MixerChannel(m, 1, c) if m.get() == 3 && c.level == Atten::new(7).unwrap()
    )));
}

#[tokio::test]
async fn recheck_spots_a_device_that_answers_only_once() {
    let mut h = synced().await;
    h.fake.dev().answer_gets = false;
    assert!(wait_health(&mut h, Health::Hung, 2000).await);
    {
        let mut d = h.fake.dev();
        d.answer_gets = true;
        d.answer_budget = Some(1);
    }
    let r = tokio::time::timeout(Duration::from_secs(5), h.handle.op(Op::Recheck))
        .await
        .expect("answered");
    assert!(matches!(r, Err(Error::Hung)), "{r:?}");
    assert_eq!(h.handle.health().health, Health::Hung);
}

#[tokio::test]
async fn recheck_on_a_healthy_device_resyncs() {
    let h = synced().await;
    h.handle.op(Op::Recheck).await.unwrap();
    assert_eq!(h.handle.health().health, Health::Ok);
    assert!(h.handle.health().syncs >= 2);
}

#[tokio::test]
async fn unknown_command_while_booting_is_retried() {
    let fake = Fake::new();
    fake.dev().unknown_left = 2;
    let h = synced_with(fake).await;
    assert_eq!(h.handle.health().health, Health::Ok);
}

#[tokio::test]
async fn unknown_command_that_persists_is_a_hang() {
    let fake = Fake::new();
    fake.dev().unknown_left = 1000;
    let dir = tempfile::tempdir().unwrap();
    let (handle, _rx) = Session::start(fake.transport(), opts(&dir)).await.unwrap();
    let r = tokio::time::timeout(Duration::from_secs(5), handle.wait_synced())
        .await
        .expect("does not wait forever");
    assert!(matches!(r, Err(Error::Hung)), "{r:?}");
    assert_eq!(handle.health().health.advice(), Advice::PowerCycle);
}

/// A setting whose read-back was cut off by a hang must not block that
/// key forever after recovery.
#[tokio::test]
async fn settings_work_again_after_hang_and_recheck() {
    let mut h = synced().await;
    h.fake.dev().answer_gets = false;
    h.handle.op(level(9)).await.unwrap();
    assert!(wait_health(&mut h, Health::Hung, 2000).await);
    h.fake.dev().answer_gets = true;
    h.handle.op(Op::Recheck).await.unwrap();
    let n = h.fake.dev().sets.len();
    h.handle.op(level(11)).await.unwrap();
    sleep_ms(300).await;
    let d = h.fake.dev();
    assert!(d.sets.len() > n, "key writable again");
    // The cache follows the device, not a stale overlay.
    drop(d);
    assert_eq!(
        h.handle.state().mixers[3].channels[0].level,
        Atten::new(11).unwrap()
    );
}
