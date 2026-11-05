//! Naming against the real dump (fixtures/dump1.json).

use std::collections::BTreeMap;
use std::path::PathBuf;

use openorion_jack::naming::*;
use openorion_proto::decode::state_from_dump;
use openorion_proto::{DeviceState, OutputBank, Source, SourceGroup};

fn hex(s: &str) -> Vec<u8> {
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
        .collect()
}

fn dump1() -> DeviceState {
    let p = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/dump1.json");
    let v: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(p).unwrap()).unwrap();
    let mut replies = BTreeMap::new();
    for (k, val) in v["replies"].as_object().unwrap() {
        let (a, b) = k.split_once('/').unwrap();
        let bytes = val.as_str().map(hex).unwrap_or_default();
        replies.insert((a.parse().unwrap(), b.parse().unwrap()), bytes);
    }
    let status = v["cyclic"]["0x73"].as_str().map(hex).unwrap_or_default();
    state_from_dump(&replies, &status).unwrap()
}

#[test]
fn dump1_capture_names() {
    // dump1: USB REC 1-32: 1-2 <- ADAT IN 1-2; 10 <- ADAT IN 1;
    //        USB REC 33-64: 1 <- ADAT IN 1.
    let st = dump1();
    assert_eq!(capture_name(&st, 1).pretty, "ADAT IN 1");
    assert_eq!(capture_name(&st, 2).pretty, "ADAT IN 2");
    assert_eq!(capture_name(&st, 3).pretty, "USB REC 3 (muted)");
    assert_eq!(capture_name(&st, 10).pretty, "ADAT IN 1");
    assert_eq!(capture_name(&st, 33).pretty, "ADAT IN 1");
    assert_eq!(capture_name(&st, 64).pretty, "USB REC 64 (muted)");
}

#[test]
fn dump1_playback_names_follow_routing() {
    let mut st = dump1();
    // Nothing in dump1 plays from USB.
    for n in 1..=64 {
        assert!(playback_name(&st, n).pretty.ends_with("(unrouted)"), "{n}");
    }
    // Route USB PLAY 1-2 to the monitor and USB PLAY 40 to LINE OUT 17.
    let mon = st
        .routing
        .iter_mut()
        .find(|r| r.bank == OutputBank::Monitor)
        .unwrap();
    for i in 0..2 {
        mon.sources[i] = Source::Input {
            group: SourceGroup::UsbPlay1,
            chan: i as u8,
        };
    }
    let line = st
        .routing
        .iter_mut()
        .find(|r| r.bank == OutputBank::LineOut)
        .unwrap();
    line.sources[16] = Source::Input {
        group: SourceGroup::UsbPlay33,
        chan: 7,
    };
    assert_eq!(playback_name(&st, 1).pretty, "MONITOR L");
    assert_eq!(playback_name(&st, 2).pretty, "MONITOR R");
    assert_eq!(playback_name(&st, 40).pretty, "LINE OUT 17");

    let ports = discover(
        &[
            PortInfo {
                name: "system:capture_1".into(),
                aliases: vec!["alsa_pcm:hw:Orion32HD:out1".into()],
                is_output: true,
            },
            PortInfo {
                name: "system:playback_40".into(),
                aliases: vec!["alsa_pcm:hw:Orion32HD:in40".into()],
                is_output: false,
            },
        ],
        &PortMatch::new("Orion32"),
        None,
    );
    let p = plan(&st, &ports);
    assert_eq!(p[0].0, "system:capture_1");
    assert_eq!(p[0].1.pretty, "ADAT IN 1");
    assert_eq!(p[1].1.pretty, "LINE OUT 17");
}
