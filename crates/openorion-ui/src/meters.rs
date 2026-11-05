//! Meter scaling, peak hold and a synthetic meter source (no device).
//!
//! Raw meter values are dB below full scale: 0 = clip, 96 = floor (0x73);
//! AFX meters (0x83) use the same scale. The bars show the whole 0..-96 dB
//! range on a taper: most of the height for the top 24 dB, the quiet end
//! compressed, so both programme levels and the noise floor are visible.

use openorion_proto::{AFX_CHAINS, AFX_SLOTS, AfxMeters, METER_FLOOR, Meters};

/// Displayed range in dB (bottom of the bar).
pub const METER_RANGE_DB: f32 = 96.0;

/// Meter taper: (dB, bar position 0..=1).
const TAPER: [(f32, f32); 10] = [
    (-96.0, 0.0),
    (-72.0, 0.08),
    (-60.0, 0.16),
    (-48.0, 0.27),
    (-36.0, 0.40),
    (-24.0, 0.56),
    (-18.0, 0.65),
    (-12.0, 0.76),
    (-6.0, 0.88),
    (0.0, 1.0),
];
/// Peak hold time in seconds.
pub const PEAK_HOLD_SECS: f64 = 1.5;

/// dBFS for a raw meter value; None at the floor (silence).
pub fn raw_to_db(raw: u8) -> Option<f32> {
    (raw < METER_FLOOR).then(|| -(raw as f32))
}

/// Bar fill 0..=1 for a raw value (0 dB = 1, the floor = 0).
pub fn meter_fraction(raw: u8) -> f32 {
    db_fraction(-(raw as f32))
}

/// Bar position 0..=1 for a dB value (scale ticks, fills).
pub fn db_fraction(db: f32) -> f32 {
    let db = db.clamp(-METER_RANGE_DB, 0.0);
    TAPER
        .windows(2)
        .find(|w| db <= w[1].0)
        .map(|w| {
            let ((d0, p0), (d1, p1)) = (w[0], w[1]);
            p0 + (db - d0) / (d1 - d0) * (p1 - p0)
        })
        .unwrap_or(1.0)
}

pub fn is_clip(raw: u8) -> bool {
    raw == 0
}

/// "-12", "-inf", "CLIP".
pub fn meter_label(raw: u8) -> String {
    match raw_to_db(raw) {
        None => "-inf".to_owned(),
        Some(_) if is_clip(raw) => "CLIP".to_owned(),
        Some(db) => format!("{db:.0}"),
    }
}

/// Peak hold for one meter: keeps the loudest raw value (smallest number)
/// for `PEAK_HOLD_SECS`, then follows the signal.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PeakHold {
    pub raw: u8,
    pub since: f64,
}

impl Default for PeakHold {
    fn default() -> Self {
        PeakHold {
            raw: METER_FLOOR,
            since: f64::NEG_INFINITY,
        }
    }
}

impl PeakHold {
    /// Feed the current value at time `now` (seconds); returns the held value.
    pub fn update(&mut self, raw: u8, now: f64) -> u8 {
        if raw <= self.raw || now - self.since >= PEAK_HOLD_SECS {
            self.raw = raw;
            self.since = now;
        }
        self.raw
    }

    pub fn clipped(&self) -> bool {
        is_clip(self.raw)
    }
}

/// Peak holds for every meter the UI draws.
#[derive(Debug, Clone)]
pub struct MeterHolds {
    pub bank_a: [PeakHold; 32],
    pub bank_b: [PeakHold; 32],
    pub mixer: [PeakHold; 32],
    pub reverb_in: [PeakHold; 2],
    pub reverb_out: [PeakHold; 2],
    pub afx: [[PeakHold; AFX_SLOTS]; AFX_CHAINS],
}

impl Default for MeterHolds {
    fn default() -> Self {
        let h = PeakHold::default();
        MeterHolds {
            bank_a: [h; 32],
            bank_b: [h; 32],
            mixer: [h; 32],
            reverb_in: [h; 2],
            reverb_out: [h; 2],
            afx: [[h; AFX_SLOTS]; AFX_CHAINS],
        }
    }
}

impl MeterHolds {
    pub fn update(&mut self, m: &Meters, afx: &AfxMeters, now: f64) {
        fn feed(holds: &mut [PeakHold], vals: &[u8], now: f64) {
            for (h, &v) in holds.iter_mut().zip(vals) {
                h.update(v, now);
            }
        }
        feed(&mut self.bank_a, &m.bank_a, now);
        feed(&mut self.bank_b, &m.bank_b, now);
        feed(&mut self.mixer, &m.mixer, now);
        feed(&mut self.reverb_in, &m.reverb_in, now);
        feed(&mut self.reverb_out, &m.reverb_out, now);
        for (h, v) in self.afx.iter_mut().zip(&afx.peak) {
            feed(h, v, now);
        }
    }
}

/// Deterministic synthetic meters for the UI shell. Some channels are
/// silent, the rest move like music; bank A ch 1 clips briefly every 7 s
/// so the clip state is visible.
#[derive(Debug, Clone, Default)]
pub struct MeterSim {
    rng: u32,
}

impl MeterSim {
    pub fn new() -> Self {
        MeterSim { rng: 0x1234_5678 }
    }

    fn noise(&mut self) -> f32 {
        // xorshift32
        let mut x = self.rng.max(1);
        x ^= x << 13;
        x ^= x >> 17;
        x ^= x << 5;
        self.rng = x;
        (x % 1000) as f32 / 1000.0
    }

    /// One channel's raw value. `seed` picks the character of the channel.
    fn channel(&mut self, t: f64, seed: u32) -> u8 {
        if seed % 5 == 3 {
            return METER_FLOOR; // silent channel
        }
        let f = 0.3 + (seed % 7) as f64 * 0.17;
        let phase = seed as f64 * 1.3;
        let base = -18.0 - (seed % 4) as f32 * 6.0;
        let swing = 10.0 * (t * f + phase).sin() as f32;
        let beat = if ((t * 2.0 + seed as f64 * 0.1).fract()) < 0.15 {
            8.0
        } else {
            0.0
        };
        let db = base + swing + beat + self.noise() * 4.0;
        (-db).clamp(1.0, 95.0) as u8
    }

    pub fn sample(&mut self, t: f64) -> (Meters, AfxMeters) {
        let mut m = Meters::default();
        for i in 0..32 {
            m.bank_a[i] = self.channel(t, i as u32);
            m.bank_b[i] = self.channel(t, i as u32 + 40);
            m.mixer[i] = self.channel(t, i as u32 + 80);
        }
        if (t % 7.0) < 0.12 {
            m.bank_a[0] = 0;
        }
        for i in 0..2 {
            m.reverb_in[i] = self.channel(t, 120 + i as u32);
            m.reverb_out[i] = self.channel(t, 122 + i as u32).saturating_add(6);
        }
        let mut afx = AfxMeters::default();
        for c in 0..AFX_CHAINS {
            for s in 0..AFX_SLOTS {
                let raw = self.channel(t, 200 + (c * AFX_SLOTS + s) as u32);
                afx.peak[c][s] = raw.min(60);
                afx.gain_reduction[c][s] = ((60u8.saturating_sub(raw)) / 6).min(20);
            }
        }
        (m, afx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scaling() {
        assert_eq!(raw_to_db(0), Some(0.0));
        assert_eq!(raw_to_db(12), Some(-12.0));
        assert_eq!(raw_to_db(95), Some(-95.0));
        assert_eq!(raw_to_db(96), None);
        assert_eq!(meter_fraction(0), 1.0);
        assert_eq!(meter_fraction(24), 0.56);
        assert_eq!(meter_fraction(96), 0.0);
        assert!(meter_fraction(90) > 0.0, "the noise floor still shows");
        assert_eq!(db_fraction(-24.0), 0.56);
        assert_eq!(db_fraction(-120.0), 0.0);
        assert_eq!(db_fraction(3.0), 1.0);
        // Monotonic.
        let v: Vec<f32> = (0..=96).map(meter_fraction).collect();
        assert!(v.windows(2).all(|w| w[0] > w[1]));
        assert!(is_clip(0));
        assert!(!is_clip(1));
        assert_eq!(meter_label(0), "CLIP");
        assert_eq!(meter_label(6), "-6");
        assert_eq!(meter_label(96), "-inf");
    }

    #[test]
    fn peak_hold() {
        let mut h = PeakHold::default();
        assert_eq!(h.update(20, 0.0), 20);
        // Quieter signal does not drop the hold within 1.5 s.
        assert_eq!(h.update(40, 1.0), 20);
        assert_eq!(h.update(40, 1.49), 20);
        // Louder signal replaces it immediately and restarts the timer.
        assert_eq!(h.update(10, 1.6), 10);
        assert_eq!(h.update(50, 3.0), 10);
        // After the hold time it follows the signal.
        assert_eq!(h.update(50, 3.2), 50);
        assert_eq!(h.update(0, 3.3), 0);
        assert!(h.clipped());
    }

    #[test]
    fn holds_follow_meters() {
        let mut holds = MeterHolds::default();
        let mut m = Meters::default();
        m.bank_a[3] = 5;
        holds.update(&m, &AfxMeters::default(), 0.0);
        assert_eq!(holds.bank_a[3].raw, 5);
        assert_eq!(holds.bank_a[4].raw, METER_FLOOR);
        m.bank_a[3] = 50;
        holds.update(&m, &AfxMeters::default(), 1.0);
        assert_eq!(holds.bank_a[3].raw, 5);
        holds.update(&m, &AfxMeters::default(), 2.0);
        assert_eq!(holds.bank_a[3].raw, 50);
    }

    #[test]
    fn sim_is_in_range() {
        let mut sim = MeterSim::new();
        let mut saw_clip = false;
        let mut saw_floor = false;
        for i in 0..400 {
            let (m, afx) = sim.sample(i as f64 * 0.033);
            for &v in m.bank_a.iter().chain(&m.bank_b).chain(&m.mixer) {
                assert!(v <= METER_FLOOR);
                saw_floor |= v == METER_FLOOR;
            }
            saw_clip |= m.bank_a[0] == 0;
            assert!(afx.peak.iter().flatten().all(|&v| v <= 60));
        }
        assert!(saw_clip && saw_floor);
    }
}
