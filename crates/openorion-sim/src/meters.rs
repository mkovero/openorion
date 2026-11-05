//! Synthetic meters: a slow sine on a few channels, the rest at the floor.

use std::f64::consts::TAU;
use std::time::Duration;

use openorion_proto::{AfxMeters, METER_FLOOR, Meters};

/// Sine period of the synthetic signal.
const PERIOD_S: f64 = 3.0;

/// Raw meter value (dB below full scale) of a sine "signal" at `t`.
/// Ranges about 6..42.
fn level(t: Duration, phase: f64) -> u8 {
    let x = (TAU * t.as_secs_f64() / PERIOD_S + phase).sin();
    (24.0 + 18.0 * x).round().clamp(0.0, METER_FLOOR as f64) as u8
}

/// 0x73 meters at sim time `t`. Silent (floor) while powered off.
pub fn meters_at(t: Duration, power_on: bool) -> Meters {
    let mut m = Meters::default();
    if !power_on {
        return m;
    }
    m.bank_a[0] = level(t, 0.0);
    m.bank_a[1] = level(t, 0.4);
    m.bank_b[0] = level(t, 1.0);
    m.bank_b[1] = level(t, 1.3);
    m.bank_b[4] = level(t, 2.5);
    m.mixer[0] = level(t, 0.2);
    m.mixer[1] = level(t, 0.6);
    m.reverb_in = [level(t, 0.2), level(t, 0.6)];
    m.reverb_out = [
        level(t, 0.9).saturating_add(10),
        level(t, 1.2).saturating_add(10),
    ];
    m
}

/// 0x83 AFX meters at sim time `t`: chain 0 slots 0-1 and chain 1 slot 0
/// carry signal; chain 0 slot 1 shows a little gain reduction.
pub fn afx_meters_at(t: Duration, power_on: bool) -> AfxMeters {
    let mut m = AfxMeters::default();
    if !power_on {
        return m;
    }
    m.peak[0][0] = level(t, 0.0);
    m.peak[0][1] = level(t, 0.3);
    m.peak[1][0] = level(t, 0.4);
    let gr = (6.0 + 6.0 * (TAU * t.as_secs_f64() / PERIOD_S).sin()).round() as u8;
    m.gain_reduction[0][1] = gr;
    m
}
