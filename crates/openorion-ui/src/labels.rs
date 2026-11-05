//! Display names used by the UI.
//!
//! The protocol names come from `openorion_proto::names` (re-exported here so
//! the tabs have one import site); this module only adds UI-specific
//! labels (value formatting, trim/oscillator names, meter channel helpers).
//! Channels are shown 1-based.

pub use openorion_proto::names::{
    afx_type_name, clock_source_name, meter_source_name, output_bank_name, sample_rate_name,
    source_group_name, source_name,
};
use openorion_proto::{
    Atten, OscFreq, OscLevel, OutputBank, Pan, SourceGroup, TrimControl, TrimId,
};

/// Highest meter source index for banks A/B.
pub const METER_SOURCE_MAX: u8 = 28;

/// Channels a meter source shows (UI-only helper).
pub fn meter_source_channels(src: u8) -> usize {
    match src {
        0..=13 => SourceGroup::try_from(src).map_or(0, |g| g.size() as usize),
        14..=METER_SOURCE_MAX => OutputBank::try_from(src - 14).map_or(0, |b| b.size()),
        _ => 0,
    }
}

/// Channel label for meter source `src`, channel `i` (0-based).
pub fn meter_channel_name(src: u8, i: usize) -> String {
    match meter_source_name(src) {
        Some(n) => format!("{n} {}", i + 1),
        None => format!("? {}", i + 1),
    }
}

pub fn trim_id_name(t: TrimId) -> &'static str {
    match t {
        TrimId::Monitor => "MONITOR",
        TrimId::LineOut => "LINE OUT",
        TrimId::LineIn => "LINE IN",
    }
}

pub fn trim_control_name(c: TrimControl) -> &'static str {
    match c {
        TrimControl::All => "ALL",
        TrimControl::Individual => "INDIVIDUAL",
    }
}

pub fn osc_freq_name(f: OscFreq) -> &'static str {
    match f {
        OscFreq::Hz1k => "1 kHz",
        OscFreq::Hz440 => "440 Hz",
    }
}

pub fn osc_level_name(l: OscLevel) -> &'static str {
    match l {
        OscLevel::Db0 => "0 dBFS",
        OscLevel::DbM6 => "-6 dBFS",
        OscLevel::DbM12 => "-12 dBFS",
        OscLevel::DbM18 => "-18 dBFS",
    }
}

/// Mixer level / send: "0.0", "-12.0", "-inf" (≥95).
pub fn atten_label(a: Atten) -> String {
    if a.is_neg_inf() {
        "-inf".to_owned()
    } else if a.get() == 0 {
        "0".to_owned()
    } else {
        format!("-{}", a.get())
    }
}

/// Volume: 96 is -inf, everything else is plain dB.
pub fn volume_label(a: Atten) -> String {
    match a.get() {
        96 => "-inf".to_owned(),
        0 => "0".to_owned(),
        n => format!("-{n}"),
    }
}

/// "L30", "C", "R12".
pub fn pan_label(p: Pan) -> String {
    match p.display() {
        0 => "C".to_owned(),
        d if d < 0 => format!("L{}", -d),
        d => format!("R{d}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use openorion_proto::{ClockSource, SampleRate, Source};

    #[test]
    fn sources() {
        assert_eq!(source_name(Source::Mute), "MUTE");
        assert_eq!(
            source_name(Source::Input {
                group: SourceGroup::AdatIn,
                chan: 2
            }),
            "ADAT IN 3"
        );
        for &g in SourceGroup::ALL {
            assert!(!source_group_name(g).is_empty());
        }
        for &b in OutputBank::ALL {
            assert!(!output_bank_name(b).is_empty());
        }
    }

    #[test]
    fn meter_sources() {
        // Spec sec. 8: 0 LINE IN, 7 ADAT IN, 14 LINE OUT, 22 ADAT OUT, 28 MIX CH4.
        assert_eq!(meter_source_name(0), Some("LINE IN"));
        assert_eq!(meter_source_name(7), Some("ADAT IN"));
        assert_eq!(meter_source_name(13), Some("MIX L/R4"));
        assert_eq!(meter_source_name(14), Some("LINE OUT"));
        assert_eq!(meter_source_name(15), Some("MONITOR"));
        assert_eq!(meter_source_name(22), Some("ADAT OUT"));
        assert_eq!(meter_source_name(24), Some("AFX IN"));
        assert_eq!(meter_source_name(28), Some("MIX CH4"));
        assert_eq!(meter_source_name(29), None);
        assert_eq!(meter_source_channels(7), 16);
        assert_eq!(meter_source_channels(15), 2);
        assert_eq!(meter_source_channels(0), 32);
        assert_eq!(meter_source_channels(200), 0);
        assert_eq!(meter_channel_name(22, 0), "ADAT OUT 1");
    }

    #[test]
    fn values() {
        assert_eq!(atten_label(Atten::new(0).unwrap()), "0");
        assert_eq!(atten_label(Atten::new(12).unwrap()), "-12");
        assert_eq!(atten_label(Atten::new(95).unwrap()), "-inf");
        assert_eq!(volume_label(Atten::new(95).unwrap()), "-95");
        assert_eq!(volume_label(Atten::NEG_INF), "-inf");
        assert_eq!(pan_label(Pan::CENTER), "C");
        assert_eq!(pan_label(Pan::new(2).unwrap()), "L30");
        assert_eq!(pan_label(Pan::new(62).unwrap()), "R30");
        assert_eq!(clock_source_name(ClockSource::Internal), "Oven/Atomic");
        assert_eq!(sample_rate_name(SampleRate::R88k2), "88.2k");
    }
}
