//! Every parameter against the real set builder and encoder: whatever a
//! controller sends, `to_op` builds sets the device accepts (panel limits
//! included), never a disruptive one, and reading back gives the value.
//! Same idea as openorion-proto/tests/safety.rs, one layer up.

use openorion_control::*;
use openorion_core::build_sets;
use openorion_ipc::Op;
use openorion_proto::decode::apply_set;
use openorion_proto::{DeviceState, Risk};
use openorion_sim::SimTransport;
use proptest::prelude::*;

/// fixtures/dump1.json with factory AFX configs filled in (the sim's seed).
fn fixture() -> DeviceState {
    SimTransport::fixture()
        .expect("fixtures/dump1.json")
        .handle()
        .state()
}

/// Build the op's sets (encoder range checks run here), apply them to
/// `state`. Panics with the parameter on any refusal.
fn send(p: &Param, op: &Op, state: &mut DeviceState) {
    assert_ne!(op.risk(), Risk::Disruptive, "{p}: {op:?}");
    let sets = build_sets(state, op).unwrap_or_else(|e| panic!("{p} → {op:?}: {e}"));
    assert!(!sets.is_empty(), "{p}");
    for s in &sets {
        assert_ne!(s.class(), Risk::Disruptive, "{p}: {s:?}");
        let r = s.report().unwrap_or_else(|e| panic!("{p}: {s:?}: {e}"));
        assert_eq!(r.as_bytes().len(), openorion_proto::REPORT_LEN);
        apply_set(state, s).unwrap_or_else(|e| panic!("{p}: {s:?}: {e}"));
    }
}

/// Read-back tolerance: half a device step plus rounding slack.
fn tolerance(p: &Param) -> f32 {
    match p.steps() {
        Some(n) if n > 1 => 0.5 / (n - 1) as f32 + 0.01,
        _ => 0.0,
    }
}

/// Set `p` to `norm` and read it back. False when `to_op` declined.
fn check(p: &Param, norm: f32, state: &mut DeviceState) -> bool {
    let Some(op) = p.to_op(norm, state) else {
        return false;
    };
    send(p, &op, state);
    let got = p
        .read(state)
        .unwrap_or_else(|| panic!("{p}: no read after set"));
    let want = match p.kind() {
        Kind::Toggle => {
            if norm >= 0.5 {
                1.0
            } else {
                0.0
            }
        }
        _ => norm.clamp(0.0, 1.0),
    };
    assert!(
        (got - want).abs() <= tolerance(p) + 1e-4,
        "{p}: set {norm}, read {got}"
    );
    true
}

#[test]
fn paths_round_trip() {
    let all = Param::all();
    assert!(all.len() > 3000, "{}", all.len());
    let mut seen = std::collections::HashSet::new();
    for p in &all {
        let s = p.to_string();
        assert!(seen.insert(s.clone()), "duplicate path {s}");
        assert_eq!(Param::parse(&s).as_ref(), Ok(p), "{s}");
        assert_eq!(Param::from_osc(&p.osc_addr()), Some(*p));
        let j = serde_json::to_string(p).unwrap();
        assert_eq!(serde_json::from_str::<Param>(&j).unwrap(), *p);
    }
}

#[test]
fn kinds_are_sane() {
    for p in Param::all() {
        match p.kind() {
            Kind::Continuous { min, max } => {
                assert!(min < max, "{p}");
                assert!(p.steps().is_some_and(|n| n > 1), "{p}");
                assert!(p.tick() > 0.0 && p.tick() <= 1.0, "{p}");
            }
            Kind::Toggle => assert_eq!(p.steps(), None, "{p}"),
            Kind::Trigger => panic!("{p}: no trigger params yet"),
        }
    }
}

/// Every available parameter at the ends, the middle and odd values.
#[test]
fn sweep_every_param() {
    let mut st = fixture();
    let params = Param::available(&st);
    assert!(params.len() > 3000, "{}", params.len());
    let mut declined = Vec::new();
    for p in &params {
        for n in [
            0.0,
            1.0,
            0.5,
            0.25,
            0.75,
            1e-6,
            1.0 - 1e-6,
            0.49,
            0.51,
            -3.0,
            7.0,
        ] {
            if !check(p, n, &mut st) {
                declined.push(*p);
            }
        }
    }
    // Only gain on a cut band (HPF/LPF) is refused, as in the panel.
    for p in declined {
        let Param::Eq {
            band,
            field: EqField::Gain,
            ..
        } = p
        else {
            panic!("{p}: to_op declined");
        };
        assert!(band == 0 || band == 4, "{p}");
    }
    // Non-finite input never builds an op.
    for p in &params {
        assert!(p.to_op(f32::NAN, &st).is_none(), "{p}");
        assert!(p.to_op(f32::INFINITY, &st).is_none(), "{p}");
    }
}

#[test]
fn missing_records_give_none() {
    let mut st = fixture();
    st.reverb = [None; 4];
    st.afx.configs.clear();
    let p = Param::parse("reverb/1/room_size").unwrap();
    assert!(p.to_op(0.5, &st).is_none());
    assert!(p.read(&st).is_none());
    let p = Param::parse("afx/compressor/1/ratio").unwrap();
    assert!(p.to_op(0.5, &st).is_none());
    assert!(p.read(&st).is_none());
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(2000))]

    #[test]
    fn any_norm_is_accepted(i in any::<prop::sample::Index>(), n in -0.5f32..1.5f32) {
        thread_local! {
            static P: Vec<Param> = Param::available(&fixture());
        }
        let p = P.with(|v| v[i.index(v.len())]);
        let mut st = fixture();
        check(&p, n, &mut st);
    }
}
