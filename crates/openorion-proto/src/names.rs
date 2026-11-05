//! Display names and the AFX field tables. Bodies are WP1.
//!
//! Names follow the vendor panel (protocol-spec.md sec. 8 and 10). Channels are shown 1-based.

use crate::model::*;

/// "LINE IN", "USB PLAY 1-32", … (oriondecode.py `INPUTS`).
pub fn source_group_name(g: SourceGroup) -> &'static str {
    match g {
        SourceGroup::LineIn => "LINE IN",
        SourceGroup::UsbPlay1 => "USB PLAY 1-32",
        SourceGroup::UsbPlay33 => "USB PLAY 33-64",
        SourceGroup::MadiIn1 => "MADI IN 1-32",
        SourceGroup::MadiIn33 => "MADI IN 33-64",
        SourceGroup::Hdx1Play => "HDX 1 PLAY",
        SourceGroup::Hdx2Play => "HDX 2 PLAY",
        SourceGroup::AdatIn => "ADAT IN",
        SourceGroup::SpdifIn => "S/PDIF IN",
        SourceGroup::AfxOut => "AFX OUT",
        SourceGroup::Mix1 => "MIX L/R1",
        SourceGroup::Mix2 => "MIX L/R2",
        SourceGroup::Mix3 => "MIX L/R3",
        SourceGroup::Mix4 => "MIX L/R4",
        SourceGroup::Oscillator => "OSCILLATOR",
    }
}

/// "LINE OUT", "MONITOR", … (oriondecode.py `OUTPUTS`).
pub fn output_bank_name(b: OutputBank) -> &'static str {
    match b {
        OutputBank::LineOut => "LINE OUT",
        OutputBank::Monitor => "MONITOR",
        OutputBank::UsbRec1 => "USB REC 1-32",
        OutputBank::UsbRec33 => "USB REC 33-64",
        OutputBank::MadiOut1 => "MADI OUT 1-32",
        OutputBank::MadiOut33 => "MADI OUT 33-64",
        OutputBank::Hdx1Rec => "HDX 1 REC",
        OutputBank::Hdx2Rec => "HDX 2 REC",
        OutputBank::AdatOut => "ADAT OUT",
        OutputBank::SpdifOut => "S/PDIF OUT",
        OutputBank::AfxIn => "AFX IN",
        OutputBank::MixCh1 => "MIX CH1",
        OutputBank::MixCh2 => "MIX CH2",
        OutputBank::MixCh3 => "MIX CH3",
        OutputBank::MixCh4 => "MIX CH4",
    }
}

/// "ADAT IN 3", "MUTE".
pub fn source_name(s: Source) -> String {
    match s {
        Source::Mute => "MUTE".to_string(),
        Source::Input { group, chan } => {
            format!("{} {}", source_group_name(group), chan as u32 + 1)
        }
    }
}

/// Upper-case, ':' and '_' as spaces, single spaces.
fn normalize(s: &str) -> String {
    s.trim()
        .to_ascii_uppercase()
        .replace([':', '_'], " ")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

/// Match `"<prefix> <n>"` and return n.
fn suffix_number(s: &str, prefix: &str) -> Option<u32> {
    s.strip_prefix(prefix)?.strip_prefix(' ')?.parse().ok()
}

/// Shared parser: exact names (longest first), then aliases that span
/// two 32-channel halves ("USB PLAY 40") or drop punctuation ("SPDIF IN").
/// Returns (item, 0-based channel).
fn parse_named<T: Copy>(
    s: &str,
    items: &[T],
    name: fn(T) -> &'static str,
    size: fn(T) -> usize,
    halves: &[(&str, T, T)],
) -> Option<(T, u8)> {
    let n = normalize(s);
    let mut sorted: Vec<T> = items.to_vec();
    sorted.sort_by_key(|&t| std::cmp::Reverse(name(t).len()));
    for &t in &sorted {
        let full = name(t);
        let alt = full.replace('/', "");
        for prefix in [full, alt.as_str()] {
            if let Some(ch) = suffix_number(&n, prefix)
                && (1..=size(t) as u32).contains(&ch)
            {
                return Some((t, (ch - 1) as u8));
            }
        }
    }
    for &(prefix, lo, hi) in halves {
        if let Some(ch) = suffix_number(&n, prefix) {
            return match ch {
                1..=32 => Some((lo, (ch - 1) as u8)),
                33..=64 => Some((hi, (ch - 33) as u8)),
                _ => None,
            };
        }
    }
    None
}

/// Parse "ADAT IN:3", "adat in 3", "mute" (case-insensitive, 1-based).
/// Also accepts "USB PLAY 40" / "MADI IN 40" (1..=64 across both halves),
/// "SPDIF IN 1" and "OSC 1".
pub fn parse_source(s: &str) -> Option<Source> {
    let n = normalize(s);
    if n == "MUTE" || n == "-" {
        return Some(Source::Mute);
    }
    let n = match n.strip_prefix("OSC ") {
        Some(rest) => format!("OSCILLATOR {rest}"),
        None => n,
    };
    parse_named(
        &n,
        SourceGroup::ALL,
        source_group_name,
        |g| g.size() as usize,
        &[
            ("USB PLAY", SourceGroup::UsbPlay1, SourceGroup::UsbPlay33),
            ("MADI IN", SourceGroup::MadiIn1, SourceGroup::MadiIn33),
        ],
    )
    .map(|(group, chan)| Source::Input { group, chan })
}

/// Parse "MIX CH4:1", "line out 17" → (bank, 0-based channel). Also
/// accepts "USB REC 40" / "MADI OUT 40" and "SPDIF OUT 1".
pub fn parse_output(s: &str) -> Option<(OutputBank, u8)> {
    parse_named(
        s,
        OutputBank::ALL,
        output_bank_name,
        |b| b.size(),
        &[
            ("USB REC", OutputBank::UsbRec1, OutputBank::UsbRec33),
            ("MADI OUT", OutputBank::MadiOut1, OutputBank::MadiOut33),
        ],
    )
}

/// Panel name: "ClearQ", "PowerFFC", "BAE-1073", …
pub fn afx_type_name(t: AfxType) -> &'static str {
    use AfxType::*;
    match t {
        Equalizer => "ClearQ",
        Compressor => "PowerFFC",
        GuitarAmp => "Guitar Amp",
        GuitarCab => "Guitar Cab",
        PultecMeq => "VMEQ-5",
        PultecEqp => "VEQ-1A",
        Neve1073 => "BAE-1073",
        Api550 => "VEQ-55A",
        Uad1176 => "FET-A76",
        PultecHlf => "VEQ-HLF",
        Helios69 => "UK-69",
        NeumannPev => "NEU-PEV",
        LangPeq2 => "Lang-PEQ2",
        NeumannW492 => "NEU-W492",
        SslPink => "VEQ-4K PINK",
        SslOrange => "VEQ-4K ORANGE",
        SslBlack => "VEQ-4K BLACK",
        SslBrown => "VEQ-4K BROWN",
        NeumannW495 => "NEU-W495",
        Studer089 => "VEQ-STU 089",
        Studer169 => "VEQ-STU 169",
        Studer900 => "VEQ-STU 900",
        Neve1023 => "BAE-1023",
        Neve1084 => "BAE-1084",
        Harrison32c => "VEQ-HA32C",
        Deesser => "De-esser",
        Api550b => "VEQ-55B",
        Dbx903 => "X903",
        GyratecX => "Gyratec X",
        Liverpool => "Liverpool",
        Tube176 => "Tube 176",
        Dbx160 => "VCA160",
        Staylevin => "Stay-Levin",
        Altec436c => "ALT-436C",
        Summit100a => "SMT-100A",
        Uad1178 => "FET-A78",
        Ba6a => "BA-6A",
        Powergate => "PowerGate",
        Tubechild670 => "Tubechild670",
        Impresser => "Impresser",
        GyratecIx => "Gyratec IX",
        TelefunkenV76 => "V76",
        Bae1073mp => "BAE-1073MP",
        Tape => "Tape",
        ChandlerRd47 => "RD47",
        Ba31 => "BA-31",
        Bae10dcf => "BAE10DCF",
        Expander => "Expander",
        AntaresAutotune => "Auto-Tune",
        AntelopeTremolo => "Tremolo",
        SpaceFlanger => "Space Flanger",
        Mvchorus => "MV Chorus",
        Autowah => "Auto-Wah",
        Distortion => "Distortion",
        PitchShifter => "Pitch Shifter",
        Opto2a => "Opto-2A",
    }
}

const METER_SOURCES: [&str; 29] = [
    "LINE IN",
    "USB PLAY 1-32",
    "USB PLAY 33-64",
    "MADI IN 1-32",
    "MADI IN 33-64",
    "HDX 1 PLAY",
    "HDX 2 PLAY",
    "ADAT IN",
    "S/PDIF IN",
    "AFX OUT",
    "MIX L/R1",
    "MIX L/R2",
    "MIX L/R3",
    "MIX L/R4",
    "LINE OUT",
    "MONITOR",
    "USB REC 1-32",
    "USB REC 33-64",
    "MADI OUT 1-32",
    "MADI OUT 33-64",
    "HDX 1 REC",
    "HDX 2 REC",
    "ADAT OUT",
    "S/PDIF OUT",
    "AFX IN",
    "MIX CH1",
    "MIX CH2",
    "MIX CH3",
    "MIX CH4",
];

/// Meter bank A/B source 0..=28 (spec sec. 8).
pub fn meter_source_name(src: u8) -> Option<&'static str> {
    METER_SOURCES.get(src as usize).copied()
}

/// "Oven/Atomic", "W.C.", …
pub fn clock_source_name(c: ClockSource) -> &'static str {
    match c {
        ClockSource::Internal => "Oven/Atomic",
        ClockSource::WordClock => "W.C.",
        ClockSource::Spdif => "S/PDIF",
        ClockSource::Adat => "ADAT",
        ClockSource::Adat2x => "ADAT 2x",
        ClockSource::Adat4x => "ADAT 4x",
        ClockSource::Madi => "MADI",
        ClockSource::Madi2x => "MADI 2x",
        ClockSource::Madi4x => "MADI 4x",
        ClockSource::Hdx => "HDX",
        ClockSource::Usb => "USB",
    }
}

/// "96k", "44.1k", …
pub fn sample_rate_name(r: SampleRate) -> &'static str {
    match r {
        SampleRate::R32k => "32k",
        SampleRate::R44k1 => "44.1k",
        SampleRate::R48k => "48k",
        SampleRate::R88k2 => "88.2k",
        SampleRate::R96k => "96k",
        SampleRate::R176k4 => "176.4k",
        SampleRate::R192k => "192k",
    }
}

/// Instances of `t` on the device: 12 for PowerGate (get 12/0 on the
/// wire), 16 for everything else. Instance ids are `0..this`.
pub fn afx_max_instances(t: AfxType) -> u8 {
    match t {
        AfxType::Powergate => 12,
        _ => 16,
    }
}

// ---------------------------------------------------------------------------
// AFX field tables (protocol-spec.md sec. 10)

/// u8 field.
const fn u(name: &'static str, min: i32, max: i32, default: i32) -> AfxField {
    AfxField {
        name,
        width: 1,
        signed: false,
        min,
        max,
        default,
        in_set: true,
    }
}

/// s8 field (two's complement on the wire, also for the u8-typed fields
/// that carry negative values: W495 gains, ALT-436C output, PEV high_gain).
const fn s(name: &'static str, min: i32, max: i32, default: i32) -> AfxField {
    AfxField {
        name,
        width: 1,
        signed: true,
        min,
        max,
        default,
        in_set: true,
    }
}

/// u16 field.
const fn w(name: &'static str, min: i32, max: i32, default: i32) -> AfxField {
    AfxField {
        name,
        width: 2,
        signed: false,
        min,
        max,
        default,
        in_set: true,
    }
}

/// 0/1 switch.
const fn sw(name: &'static str, default: i32) -> AfxField {
    u(name, 0, 1, default)
}

/// 0..100 pot.
const fn pot(name: &'static str, default: i32) -> AfxField {
    u(name, 0, 100, default)
}

/// `level` (AfxLevelPot, s8 -24..+12 dB).
const LEVEL: AfxField = s("level", -24, 12, 0);
/// Vintage compressors' trailing per-slot link, part of the set.
const LINKED: AfxField = sw("linked", 0);

const PULTEC_MEQ: &[AfxField] = &[
    u("low_freq", 0, 4, 0),
    pot("low_gain", 0),
    u("mid_freq", 0, 10, 0),
    pot("mid_gain", 0),
    u("hi_freq", 0, 4, 0),
    pot("hi_gain", 0),
];

const PULTEC_EQP: &[AfxField] = &[
    u("low_freq", 0, 3, 0),
    pot("low_boost", 0),
    pot("low_atten", 0),
    u("high_freq", 0, 6, 0),
    pot("high_boost", 0),
    pot("high_atten", 0),
    pot("bandwidth", 0),
    u("atten_sel", 0, 2, 0),
];

const PULTEC_HLF: &[AfxField] = &[u("low_shelf", 0, 10, 0), u("high_shelf", 0, 10, 10)];

const NEVE_1073: &[AfxField] = &[
    s("gain", -20, 10, 0),
    u("high_shelf", 0, 16, 8),
    u("peak_freq", 0, 6, 0),
    u("peak_gain", 0, 16, 8),
    u("low_freq", 0, 4, 0),
    u("low_gain", 0, 16, 8),
    u("high_pass", 0, 4, 0),
    sw("phase_inv", 0),
];

const NEVE_1023: &[AfxField] = &[
    s("gain", -20, 10, 0),
    u("high_freq", 0, 5, 0),
    pot("high_gain", 50),
    u("peak_freq", 0, 11, 0),
    pot("peak_gain", 50),
    u("low_freq", 0, 4, 0),
    pot("low_gain", 50),
    u("high_pass", 0, 4, 0),
    sw("phase_inv", 0),
];

const NEVE_1084: &[AfxField] = &[
    s("gain", -20, 10, 0),
    u("high_freq", 0, 3, 0),
    pot("high_gain", 50),
    u("peak_freq", 0, 6, 0),
    pot("peak_gain", 50),
    u("low_freq", 0, 4, 0),
    pot("low_gain", 50),
    u("high_pass", 0, 4, 0),
    u("low_pass", 0, 5, 0),
    sw("hi_q", 0),
    sw("phase_inv", 0),
];

const API_550: &[AfxField] = &[
    s("gain", -48, 12, 0),
    u("low_freq", 0, 4, 2),
    u("low_gain", 0, 10, 5),
    u("mid_freq", 0, 4, 2),
    u("mid_gain", 0, 10, 5),
    u("high_freq", 0, 4, 2),
    u("high_gain", 0, 10, 5),
    sw("low_shelf", 0),
    sw("high_shelf", 0),
    sw("bandpass", 0),
];

const API_550B: &[AfxField] = &[
    s("gain", -48, 12, 0),
    u("low_freq", 0, 6, 2),
    u("low_gain", 0, 10, 5),
    u("lmid_freq", 0, 6, 2),
    u("lmid_gain", 0, 10, 5),
    u("hmid_freq", 0, 6, 2),
    u("hmid_gain", 0, 10, 5),
    u("high_freq", 0, 6, 2),
    u("high_gain", 0, 10, 5),
    sw("low_shelf", 0),
    sw("high_shelf", 0),
];

const HELIOS_69: &[AfxField] = &[
    s("bass_freq", -5, 5, 0),
    pot("bass_gain", 0),
    u("high_gain", 0, 10, 5),
    u("mid_freq", 0, 8, 4),
    pot("mid_gain", 0),
    sw("mid_pk", 0),
    LEVEL,
];

const NEUMANN_PEV: &[AfxField] = &[
    s("low_gain", -5, 3, 0),
    u("pres_gain", 0, 4, 0),
    u("pres_freq", 0, 6, 3),
    s("high_gain", -5, 4, 0),
    LEVEL,
];

const NEUMANN_W492: &[AfxField] = &[
    u("low_cut", 0, 2, 1),
    pot("low_gain", 50),
    pot("low_freq", 50),
    sw("low_on", 1),
    pot("lmid_gain", 50),
    pot("lmid_freq", 50),
    sw("lmid_on", 1),
    pot("hmid_gain", 50),
    pot("hmid_freq", 50),
    sw("hmid_on", 1),
    pot("high_gain", 50),
    pot("high_freq", 50),
    sw("high_on", 1),
    LEVEL,
];

const NEUMANN_W495: &[AfxField] = &[
    u("low_freq", 0, 2, 1),
    s("low_gain", -5, 5, 0),
    u("mid_freq", 0, 10, 5),
    s("mid_gain", -5, 5, 0),
    u("mid_q", 0, 2, 1),
    u("high_freq", 0, 2, 1),
    s("high_gain", -5, 5, 0),
    LEVEL,
];

const LANG_PEQ2: &[AfxField] = &[
    u("lf_boost_freq", 0, 7, 3),
    pot("lf_boost_gain", 0),
    u("lf_drop_freq", 0, 3, 1),
    pot("lf_drop_gain", 0),
    pot("hf_bandwidth", 50),
    u("hf_boost_freq", 0, 7, 4),
    pot("hf_boost_gain", 0),
    u("hf_drop_freq", 0, 5, 3),
    pot("hf_drop_gain", 0),
    LEVEL,
];

/// VEQ-4K PINK/ORANGE/BLACK/BROWN share one layout.
const SSL: &[AfxField] = &[
    pot("lf_cut", 0),
    pot("hf_cut", 0),
    pot("hf_gain", 50),
    pot("hf_freq", 50),
    sw("hf_bell", 0),
    pot("hmf_gain", 50),
    pot("hmf_freq", 50),
    pot("hmf_qual", 50),
    pot("lmf_gain", 50),
    pot("lmf_freq", 50),
    pot("lmf_qual", 50),
    sw("lf_bell", 0),
    pot("lf_gain", 50),
    pot("lf_freq", 50),
    LEVEL,
];

const STUDER_900: &[AfxField] = &[
    pot("low_cut", 0),
    pot("hi_cut", 100),
    pot("lf_gain", 50),
    pot("lf_freq", 0),
    sw("lf_switch", 0),
    pot("lmf_gain", 50),
    pot("lmf_freq", 0),
    sw("lmf_switch", 0),
    pot("hmf_gain", 50),
    pot("hmf_freq", 0),
    sw("hmf_switch", 0),
    pot("hf_gain", 50),
    pot("hf_freq", 0),
    sw("hf_switch", 0),
    LEVEL,
];

const STUDER_169: &[AfxField] = &[
    sw("hpf_switch", 0),
    pot("hf_gain", 50),
    pot("mf_gain", 50),
    pot("mf_freq", 0),
    pot("lf_gain", 50),
    LEVEL,
];

const STUDER_089: &[AfxField] = &[
    u("lf_cut", 0, 4, 0),
    u("hf_cut", 0, 4, 0),
    s("lf_shelf", -4, 4, 0),
    s("hf_shelf", -4, 4, 0),
    pot("mf_gain", 50),
    u("mf_freq", 0, 6, 0),
    LEVEL,
];

const HARRISON_32C: &[AfxField] = &[
    sw("low_cut_enable", 1),
    pot("low_cut", 0),
    sw("hi_cut_enable", 1),
    pot("hi_cut", 100),
    pot("lf_gain", 50),
    pot("lf_freq", 0),
    sw("lf_switch", 0),
    pot("lmf_gain", 50),
    pot("lmf_freq", 0),
    pot("hmf_gain", 50),
    pot("hmf_freq", 0),
    pot("hf_gain", 50),
    pot("hf_freq", 0),
    sw("hf_switch", 0),
    LEVEL,
];

/// FET-A76. The panel always sends linked = 0 (protocol-spec.md sec. 10).
const UAD_1176: &[AfxField] = &[
    u("input", 0, 60, 30),
    u("output", 0, 60, 30),
    pot("attack", 66),
    pot("release", 66),
    // bit0 4:1, bit1 8:1, bit2 12:1, bit3 20:1
    u("ratio", 0, 15, 1),
    LINKED,
];

const UAD_1178: &[AfxField] = &[
    u("input", 0, 60, 44),
    u("output", 0, 60, 44),
    pot("attack", 50),
    pot("release", 50),
    u("ratio", 0, 15, 1),
    LINKED,
];

const DBX_903: &[AfxField] = &[
    pot("threshold", 100),
    pot("ratio", 0),
    pot("output", 50),
    LINKED,
];

const DBX_160: &[AfxField] = &[
    pot("threshold", 100),
    u("compression", 0, 60, 0),
    pot("output", 50),
    LINKED,
];

const LIVERPOOL: &[AfxField] = &[
    pot("input", 50),
    pot("threshold", 50),
    u("attack", 0, 10, 5),
    u("recovery", 0, 10, 0),
    u("output", 0, 5, 2),
    LINKED,
];

const GYRATEC_X: &[AfxField] = &[
    pot("input", 50),
    pot("output", 50),
    pot("attack", 50),
    pot("release", 50),
    pot("ratio", 50),
    pot("threshold", 50),
    LINKED,
];

const TUBE_176: &[AfxField] = &[
    pot("input", 50),
    u("ratio", 0, 3, 0),
    pot("attack", 50),
    pot("release", 50),
    pot("output", 50),
    LINKED,
];

const STAYLEVIN: &[AfxField] = &[
    pot("input", 50),
    pot("output", 50),
    u("release", 0, 4, 2),
    sw("attack", 0),
    LINKED,
];

const ALTEC_436C: &[AfxField] = &[
    pot("input", 50),
    pot("threshold", 50),
    pot("release", 50),
    s("output", -24, 24, 0),
    LINKED,
];

const SUMMIT_100A: &[AfxField] = &[
    pot("output", 50),
    u("attack", 0, 2, 0),
    u("release", 0, 2, 0),
    pot("input", 50),
    LINKED,
];

const BA6A: &[AfxField] = &[
    pot("input", 50),
    pot("output", 50),
    u("attack", 0, 2, 1),
    LINKED,
];

const TUBECHILD670: &[AfxField] = &[
    pot("input", 50),
    pot("threshold", 50),
    u("time_const", 0, 5, 2),
    pot("output", 50),
    LINKED,
];

const IMPRESSER: &[AfxField] = &[
    pot("input", 50),
    pot("output", 50),
    pot("attack", 50),
    pot("release", 50),
    // 1:1, 2:1, 3:1, 4:1, 6:1, 10:1, 20:1, NUKE
    u("ratio", 0, 7, 0),
    u("high_pass_filter", 0, 10, 4),
    u("band_pass_filter", 0, 10, 8),
    // dist2, dist3, none
    u("distortion", 0, 2, 2),
    sw("british_mode", 0),
    LINKED,
];

/// PowerGate: get entries are 10 B (enabled + these), the same fields as
/// the set. There is no trailing get-only `linked`; the
/// hardware has none (fixtures/afx-configs-hw.json).
const POWERGATE: &[AfxField] = &[
    u("threshold", 0, 120, 90),
    u("range", 0, 20, 0),
    w("attack", 1, 750, 100),
    w("decay", 1, 1000, 50),
    w("hold", 0, 5000, 0),
    s("gain", -24, 12, 0),
];

/// Field table for models carried as `AfxRaw` (vintage EQs/compressors,
/// PowerGate), in get-reply order without `enabled`. None for Equalizer,
/// Compressor, and types not on the Orion. See protocol-spec.md sec. 10.
pub fn afx_fields(t: AfxType) -> Option<&'static [AfxField]> {
    use AfxType::*;
    Some(match t {
        PultecMeq => PULTEC_MEQ,
        PultecEqp => PULTEC_EQP,
        PultecHlf => PULTEC_HLF,
        Neve1073 => NEVE_1073,
        Neve1023 => NEVE_1023,
        Neve1084 => NEVE_1084,
        Api550 => API_550,
        Api550b => API_550B,
        Helios69 => HELIOS_69,
        NeumannPev => NEUMANN_PEV,
        NeumannW492 => NEUMANN_W492,
        NeumannW495 => NEUMANN_W495,
        LangPeq2 => LANG_PEQ2,
        SslPink | SslOrange | SslBlack | SslBrown => SSL,
        Studer900 => STUDER_900,
        Studer169 => STUDER_169,
        Studer089 => STUDER_089,
        Harrison32c => HARRISON_32C,
        Uad1176 => UAD_1176,
        Uad1178 => UAD_1178,
        Dbx903 => DBX_903,
        Dbx160 => DBX_160,
        Liverpool => LIVERPOOL,
        GyratecX => GYRATEC_X,
        Tube176 => TUBE_176,
        Staylevin => STAYLEVIN,
        Altec436c => ALTEC_436C,
        Summit100a => SUMMIT_100A,
        Ba6a => BA6A,
        Tubechild670 => TUBECHILD670,
        Impresser => IMPRESSER,
        Powergate => POWERGATE,
        _ => return None,
    })
}

/// Bytes of one get-reply entry for `t` (`enabled` included).
pub fn afx_entry_len(t: AfxType) -> Option<usize> {
    match t {
        AfxType::Equalizer => Some(36),
        AfxType::Compressor => Some(19),
        _ => afx_fields(t).map(|f| 1 + f.iter().map(|f| f.width as usize).sum::<usize>()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_orion_raw_model_has_a_table() {
        for &t in AfxType::ALL {
            let raw = t.on_orion() && t != AfxType::Equalizer && t != AfxType::Compressor;
            assert_eq!(afx_fields(t).is_some(), raw, "{t:?}");
            if let Some(tab) = afx_fields(t) {
                for f in tab {
                    assert!(f.min <= f.default && f.default <= f.max, "{t:?} {}", f.name);
                }
                // get-only fields only at the end
                let first_get_only = tab.iter().position(|f| !f.in_set).unwrap_or(tab.len());
                assert!(tab[first_get_only..].iter().all(|f| !f.in_set));
                assert!(afx_entry_len(t).unwrap() * 16 <= 304);
            }
        }
    }

    #[test]
    fn parse_names() {
        let adat3 = Source::Input {
            group: SourceGroup::AdatIn,
            chan: 2,
        };
        assert_eq!(parse_source("ADAT IN:3"), Some(adat3));
        assert_eq!(parse_source("adat in 3"), Some(adat3));
        assert_eq!(parse_source(" Mute "), Some(Source::Mute));
        assert_eq!(parse_source("ADAT IN 17"), None);
        assert_eq!(parse_source("ADAT IN 0"), None);
        assert_eq!(
            parse_source("usb play 40"),
            Some(Source::Input {
                group: SourceGroup::UsbPlay33,
                chan: 7
            })
        );
        assert_eq!(
            parse_source("USB PLAY 1-32:5"),
            Some(Source::Input {
                group: SourceGroup::UsbPlay1,
                chan: 4
            })
        );
        assert_eq!(
            parse_source("mix l/r2 2"),
            Some(Source::Input {
                group: SourceGroup::Mix2,
                chan: 1
            })
        );
        assert_eq!(
            parse_source("spdif in 1"),
            Some(Source::Input {
                group: SourceGroup::SpdifIn,
                chan: 0
            })
        );
        assert_eq!(parse_output("MIX CH4:1"), Some((OutputBank::MixCh4, 0)));
        assert_eq!(parse_output("line out 17"), Some((OutputBank::LineOut, 16)));
        assert_eq!(parse_output("monitor 3"), None);
        assert_eq!(
            parse_output("S/PDIF OUT 2"),
            Some((OutputBank::SpdifOut, 1))
        );
        for &g in SourceGroup::ALL {
            for c in 0..g.size() {
                let s = Source::Input { group: g, chan: c };
                assert_eq!(parse_source(&source_name(s)), Some(s));
            }
        }
        for &b in OutputBank::ALL {
            let n = format!("{} {}", output_bank_name(b), b.size());
            assert_eq!(parse_output(&n), Some((b, b.size() as u8 - 1)));
        }
    }

    #[test]
    fn misc_names() {
        assert_eq!(meter_source_name(0), Some("LINE IN"));
        assert_eq!(meter_source_name(22), Some("ADAT OUT"));
        assert_eq!(meter_source_name(28), Some("MIX CH4"));
        assert_eq!(meter_source_name(29), None);
        assert_eq!(clock_source_name(ClockSource::Internal), "Oven/Atomic");
        assert_eq!(sample_rate_name(SampleRate::R44k1), "44.1k");
        assert_eq!(afx_type_name(AfxType::Neve1073), "BAE-1073");
        assert_eq!(source_name(Source::Mute), "MUTE");
    }
}
