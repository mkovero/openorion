//! Session against the real simulator (openorion-sim) in real time, with
//! default timings: full sync, a silent op, read-back.

use std::time::Duration;

use openorion_core::{Session, SessionOpts};
use openorion_ipc::Op;
use openorion_proto::*;
use openorion_sim::{SimClock, SimTransport};

#[tokio::test(flavor = "multi_thread")]
async fn syncs_and_sets_against_sim() {
    let sim = SimTransport::fixture()
        .unwrap()
        .with_clock(SimClock::RealTime);
    let dev = sim.handle();
    let dir = tempfile::tempdir().unwrap();
    let opts = SessionOpts {
        lock_path: dir.path().join("openorion.lock"),
        ..SessionOpts::default()
    };
    let (h, _rx) = Session::start(Box::new(sim), opts).await.unwrap();
    tokio::time::timeout(Duration::from_secs(10), h.wait_synced())
        .await
        .expect("sync within 10 s")
        .unwrap();
    let ours = h.state();
    let theirs = dev.state();
    assert_eq!(ours.mixers, theirs.mixers);
    assert_eq!(ours.routing, theirs.routing);
    assert_eq!(ours.trims, theirs.trims);
    assert_eq!(ours.afx.order, theirs.afx.order);

    // Mixer 4 ch1 level: silent (nothing routed to or from mixer 4).
    let m4 = MixerId::new(3).unwrap();
    h.op(Op::Mixer {
        mixer: m4,
        ch: 1,
        level: Some(Atten::new(30).unwrap()),
        pan: None,
        mute: None,
        solo: None,
        send: None,
    })
    .await
    .unwrap();
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(dev.state().mixers[3].channels[0].level.get(), 30);
    assert_eq!(h.state().mixers[3].channels[0].level.get(), 30);
}
