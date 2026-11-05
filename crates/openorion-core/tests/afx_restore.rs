//! Power-up AFX recovery: which instances `afx_power_up_target` puts back.

mod support;

use openorion_core::{afx_power_up_target, plan_restore};
use openorion_ipc::Op;
use openorion_proto::decode::default_afx_config;
use openorion_proto::*;

/// The fixture with every EQ and compressor instance at factory defaults,
/// as after a power cycle.
fn fixture_state() -> DeviceState {
    let mut s = support::fixture_state();
    for ty in [AfxType::Equalizer, AfxType::Compressor] {
        s.afx
            .configs
            .insert(ty, vec![default_afx_config(ty).unwrap(); 16]);
    }
    s
}

fn eq_mut(s: &mut DeviceState, inst: usize) -> &mut AfxConfig {
    &mut s.afx.configs.get_mut(&AfxType::Equalizer).unwrap()[inst]
}

fn comp_mut(s: &mut DeviceState, inst: usize) -> &mut CompressorParams {
    match &mut s.afx.configs.get_mut(&AfxType::Compressor).unwrap()[inst].params {
        AfxParams::Compressor(p) => p,
        p => panic!("{p:?}"),
    }
}

#[test]
fn only_instances_back_at_factory_defaults_are_restored() {
    let current = fixture_state();
    let mut saved = current.clone();
    // Set before the power cycle: an EQ boost, an enabled EQ that came up
    // bypassed, and a compressor change.
    if let AfxParams::Eq(eq) = &mut eq_mut(&mut saved, 3).params {
        eq.bands[2].gain = 600;
    }
    comp_mut(&mut saved, 1).threshold = 30;

    let mut now = current.clone();
    eq_mut(&mut now, 5).enabled = false;
    // Changed elsewhere while we were away: not defaults, so left alone.
    comp_mut(&mut now, 1).threshold = 40;

    let (target, picked) = afx_power_up_target(&now, &saved.afx.configs);
    assert_eq!(
        picked,
        vec![(AfxType::Equalizer, 3), (AfxType::Equalizer, 5)],
        "compressor 1 holds non-default values: kept"
    );
    let ops = plan_restore(&now, &target);
    assert_eq!(ops.len(), 2, "{ops:?}");
    assert!(ops.iter().any(|o| matches!(
        o,
        Op::AfxEqBand {
            inst: 3,
            band: 2,
            gain: Some(600),
            ..
        }
    )));
    assert!(ops.iter().any(|o| matches!(
        o,
        Op::AfxBypass {
            ty: AfxType::Equalizer,
            inst: 5,
            enabled: true
        }
    )));
}

#[test]
fn nothing_to_do_when_the_device_kept_its_settings() {
    let s = fixture_state();
    let (_, picked) = afx_power_up_target(&s, &s.afx.configs);
    assert!(picked.is_empty());
}
