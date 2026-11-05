//! Argument parsing (text → model) and display helpers (model → text).
//! Channels, mixers, instances and bands are 1-based in text.

use openorion_proto::names::*;
use openorion_proto::*;

/// Upper-case, ':' '_' '-' as spaces, single spaces.
fn norm(s: &str) -> String {
    s.trim()
        .to_ascii_uppercase()
        .replace([':', '_'], " ")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

/// Lower-case alphanumerics only ("S/PDIF" → "spdif").
fn squash(s: &str) -> String {
    s.chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .map(|c| c.to_ascii_lowercase())
        .collect()
}

pub fn on_off(s: &str) -> Result<bool, String> {
    match s.to_ascii_lowercase().as_str() {
        "on" | "true" | "yes" | "1" => Ok(true),
        "off" | "false" | "no" | "0" => Ok(false),
        _ => Err(format!("expected on|off, got {s:?}")),
    }
}

/// A level in dB as attenuation: "-30" → 30, "0" → 0, "-inf" → 96.
pub fn atten_db(s: &str) -> Result<Atten, String> {
    let l = s.trim().to_ascii_lowercase();
    if matches!(l.as_str(), "-inf" | "inf" | "-∞" | "off") {
        return Ok(Atten::NEG_INF);
    }
    let v: f64 = l
        .trim_end_matches("db")
        .trim()
        .parse()
        .map_err(|_| format!("bad level {s:?} (dB, e.g. -30, 0 or -inf)"))?;
    if v > 0.0 {
        return Err(format!(
            "level {s} dB: levels are attenuation, 0 dB at most (e.g. -30 for 30 dB down)"
        ));
    }
    let a = (-v).round();
    if a > 96.0 {
        return Err(format!("level {s} dB: below -96 dB (use -inf)"));
    }
    Atten::new(a as u8).map_err(|e| e.to_string())
}

/// Pan -30..+30 ("C", "L12", "R12" also accepted) → wire value.
pub fn pan(s: &str) -> Result<Pan, String> {
    let u = s.trim().to_ascii_uppercase();
    let v: i32 = if u == "C" || u == "CENTER" || u == "CENTRE" {
        0
    } else if let Some(r) = u.strip_prefix('L') {
        -r.parse::<i32>().map_err(|_| format!("bad pan {s:?}"))?
    } else if let Some(r) = u.strip_prefix('R') {
        r.parse::<i32>().map_err(|_| format!("bad pan {s:?}"))?
    } else {
        u.trim_start_matches('+')
            .parse()
            .map_err(|_| format!("bad pan {s:?} (-30..+30, C, L12, R12)"))?
    };
    if !(-30..=30).contains(&v) {
        return Err(format!("pan {v} out of range -30..+30"));
    }
    Pan::new((v + 32) as u8).map_err(|e| e.to_string())
}

/// Mixer 1..=4 → MixerId 0..=3.
pub fn mixer(s: &str) -> Result<MixerId, String> {
    let n: u8 = squash(s)
        .trim_start_matches("mixer")
        .trim_start_matches("mix")
        .parse()
        .map_err(|_| format!("bad mixer {s:?} (1..4)"))?;
    if !(1..=4).contains(&n) {
        return Err(format!("mixer {n} out of range 1..4"));
    }
    MixerId::new(n - 1).map_err(|e| e.to_string())
}

/// Mixer channel: "master"/"m"/"0" → 0, else 1..=32.
pub fn mixer_ch(s: &str) -> Result<u8, String> {
    let l = s.trim().to_ascii_lowercase();
    if l == "master" || l == "m" || l == "0" {
        return Ok(0);
    }
    let n: u8 = l
        .parse()
        .map_err(|_| format!("bad channel {s:?} (1..32 or master)"))?;
    if !(1..=MIXER_CHANNELS as u8).contains(&n) {
        return Err(format!("channel {n} out of range 1..32"));
    }
    Ok(n)
}

/// 1-based number in `1..=max` → 0-based.
pub fn one_based(s: &str, what: &str, max: usize) -> Result<u8, String> {
    let n: usize = s
        .trim()
        .parse()
        .map_err(|_| format!("bad {what} {s:?} (1..{max})"))?;
    if !(1..=max).contains(&n) {
        return Err(format!("{what} {n} out of range 1..{max}"));
    }
    Ok((n - 1) as u8)
}

/// A bank name without channel: "MIX CH4", "line out", "usb rec 33-64",
/// "spdif out".
pub fn bank(s: &str) -> Result<OutputBank, String> {
    let n = norm(s);
    let q = squash(s);
    OutputBank::ALL
        .iter()
        .copied()
        .find(|&b| norm(output_bank_name(b)) == n || squash(output_bank_name(b)) == q)
        .or(match q.as_str() {
            "usbrec" => Some(OutputBank::UsbRec1),
            "madiout" => Some(OutputBank::MadiOut1),
            "afx" => Some(OutputBank::AfxIn),
            "mon" => Some(OutputBank::Monitor),
            _ => None,
        })
        .ok_or_else(|| format!("unknown output bank {s:?} (e.g. \"LINE OUT\", \"MIX CH4\")"))
}

/// "BANK[:CH]" → (bank, 0-based ch). Without a channel: channel 1.
pub fn output(s: &str) -> Result<(OutputBank, u8), String> {
    if let Some(x) = parse_output(s) {
        return Ok(x);
    }
    bank(s).map(|b| (b, 0)).map_err(|_| {
        format!("unknown output {s:?} (e.g. \"MIX CH4:1\", \"LINE OUT 3\", \"USB REC 40\")")
    })
}

pub fn source(s: &str) -> Result<Source, String> {
    if let Some(x) = parse_source(s) {
        return Ok(x);
    }
    // A group name without channel means channel 1.
    let q = squash(s);
    SourceGroup::ALL
        .iter()
        .copied()
        .find(|&g| squash(source_group_name(g)) == q)
        .map(|group| Source::Input { group, chan: 0 })
        .ok_or_else(|| format!("unknown source {s:?} (e.g. \"ADAT IN:1\", \"MIX L/R1 1\", MUTE)"))
}

pub fn clock_source(s: &str) -> Result<ClockSource, String> {
    let q = squash(s);
    let alias = match q.as_str() {
        "internal" | "int" | "oven" | "atomic" => Some(ClockSource::Internal),
        "wc" | "wordclock" => Some(ClockSource::WordClock),
        _ => None,
    };
    alias
        .or_else(|| {
            ClockSource::ALL
                .iter()
                .copied()
                .find(|&c| squash(clock_source_name(c)) == q || squash(&format!("{c:?}")) == q)
        })
        .ok_or_else(|| {
            let all: Vec<_> = ClockSource::ALL
                .iter()
                .map(|&c| clock_source_name(c))
                .collect();
            format!("unknown clock source {s:?} ({})", all.join(", "))
        })
}

pub fn sample_rate(s: &str) -> Result<SampleRate, String> {
    let l = s.trim().to_ascii_lowercase();
    let l = l.trim_end_matches("hz");
    let hz = if let Some(k) = l.strip_suffix('k') {
        k.parse::<f64>().ok().map(|v| (v * 1000.0).round() as u32)
    } else {
        l.parse::<f64>()
            .ok()
            .map(|v| if v < 1000.0 { v * 1000.0 } else { v }.round() as u32)
    };
    hz.and_then(|hz| {
        SampleRate::from_hz(hz).or_else(|| {
            SampleRate::ALL
                .iter()
                .copied()
                .find(|r| r.hz() / 100 == hz / 100)
        })
    })
    .ok_or_else(|| format!("unknown sample rate {s:?} (32k 44.1k 48k 88.2k 96k 176.4k 192k)"))
}

pub fn afx_type(s: &str) -> Result<AfxType, String> {
    let q = squash(s);
    let alias = match q.as_str() {
        "eq" => Some(AfxType::Equalizer),
        "comp" => Some(AfxType::Compressor),
        _ => None,
    };
    let t = alias
        .or_else(|| {
            AfxType::ALL
                .iter()
                .copied()
                .find(|&t| squash(afx_type_name(t)) == q || squash(&format!("{t:?}")) == q)
        })
        .ok_or_else(|| format!("unknown AFX type {s:?} (e.g. ClearQ, PowerFFC, eq, comp)"))?;
    if !t.on_orion() {
        return Err(format!(
            "{} is not available on the Orion",
            afx_type_name(t)
        ));
    }
    Ok(t)
}

/// "TYPE:INST" or "TYPE INST" (INST 1-based) → (type, Some(0-based)).
pub fn afx_slot(s: &str) -> Result<(AfxType, Option<u8>), String> {
    let t = s.trim();
    // A bare name first: some names end in digits ("BAE-1073").
    if let Ok(ty) = afx_type(t) {
        return Ok((ty, None));
    }
    if let Some((a, b)) = t.rsplit_once([':', ' ', '#'])
        && let Ok(n) = b.trim().parse::<usize>()
    {
        let ty = afx_type(a)?;
        let max = afx_max_instances(ty) as usize;
        return Ok((ty, Some(one_based(&n.to_string(), "instance", max)?)));
    }
    Ok((afx_type(t)?, None))
}

pub fn eq_filter(s: &str) -> Result<EqFilter, String> {
    let q = squash(s);
    EqFilter::ALL
        .iter()
        .copied()
        .find(|&f| squash(&format!("{f:?}")) == q)
        .or(match q.as_str() {
            "ls" | "lowshelf" | "lshelf" => Some(EqFilter::LowShelf),
            "hs" | "highshelf" | "hshelf" => Some(EqFilter::HighShelf),
            "bell" | "pk" => Some(EqFilter::Peak),
            "lp" | "lowpass" => Some(EqFilter::Lpf),
            "hp" | "highpass" => Some(EqFilter::Hpf),
            _ => None,
        })
        .ok_or_else(|| format!("unknown EQ filter {s:?} (lowshelf highshelf peak lpf hpf)"))
}

pub fn osc_freq(s: &str) -> Result<OscFreq, String> {
    match squash(s).as_str() {
        "1k" | "1000" | "1khz" | "1000hz" => Ok(OscFreq::Hz1k),
        "440" | "440hz" => Ok(OscFreq::Hz440),
        _ => Err(format!("oscillator frequency {s:?}: 1k or 440")),
    }
}

pub fn osc_level(s: &str) -> Result<OscLevel, String> {
    match s.trim().trim_end_matches("dB").trim() {
        "0" => Ok(OscLevel::Db0),
        "-6" => Ok(OscLevel::DbM6),
        "-12" => Ok(OscLevel::DbM12),
        "-18" => Ok(OscLevel::DbM18),
        _ => Err(format!("oscillator level {s:?}: 0, -6, -12 or -18")),
    }
}

/// dBu 10..=24 (hundredths kept) → trim level.
pub fn trim_dbu(s: &str) -> Result<TrimLevel, String> {
    let v: f64 = s
        .trim()
        .trim_end_matches("dBu")
        .trim()
        .parse()
        .map_err(|_| format!("bad trim {s:?} (dBu, 10..24)"))?;
    if !(10.0..=24.0).contains(&v) {
        return Err(format!("trim {v} dBu out of range 10..24"));
    }
    let h = ((24.0 - v) * 100.0).round() as u32;
    Ok(TrimLevel {
        whole: (h / 100) as u8,
        fract: (h % 100) as u8,
    })
}

pub fn hex(s: &str) -> Result<Vec<u8>, String> {
    let t: String = s
        .chars()
        .filter(|c| !c.is_whitespace() && *c != ',' && *c != ':')
        .collect();
    let t = t.strip_prefix("0x").unwrap_or(&t);
    if !t.len().is_multiple_of(2) || t.is_empty() {
        return Err(format!("bad hex: {} digits", t.len()));
    }
    (0..t.len())
        .step_by(2)
        .map(|i| {
            u8::from_str_radix(&t[i..i + 2], 16).map_err(|_| format!("bad hex {:?}", &t[i..i + 2]))
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Display

/// Mixer level / send: "-30", "0", "-inf".
pub fn fmt_level(a: Atten) -> String {
    if a.is_neg_inf() {
        "-inf".into()
    } else if a.get() == 0 {
        "0".into()
    } else {
        format!("-{}", a.get())
    }
}

/// Volume: only 96 is silence.
pub fn fmt_volume(a: Atten) -> String {
    if a == Atten::NEG_INF {
        "-inf dB".into()
    } else if a.get() == 0 {
        "0 dB".into()
    } else {
        format!("-{} dB", a.get())
    }
}

pub fn fmt_pan(p: Pan) -> String {
    match p.display() {
        0 => "C".into(),
        d if d < 0 => format!("L{}", -d),
        d => format!("R{d}"),
    }
}

pub fn fmt_bool(b: bool) -> &'static str {
    if b { "on" } else { "off" }
}

/// Output display name with absolute channel numbering ("USB REC 40").
pub fn out_name(b: OutputBank, ch: usize) -> String {
    match b {
        OutputBank::UsbRec1 => format!("USB REC {}", ch + 1),
        OutputBank::UsbRec33 => format!("USB REC {}", ch + 33),
        OutputBank::MadiOut1 => format!("MADI OUT {}", ch + 1),
        OutputBank::MadiOut33 => format!("MADI OUT {}", ch + 33),
        _ => format!("{} {}", output_bank_name(b), ch + 1),
    }
}

fn src_parts(s: Source) -> (String, usize) {
    match s {
        Source::Mute => ("MUTE".into(), 0),
        Source::Input { group, chan } => {
            let c = chan as usize;
            match group {
                SourceGroup::UsbPlay1 => ("USB PLAY".into(), c + 1),
                SourceGroup::UsbPlay33 => ("USB PLAY".into(), c + 33),
                SourceGroup::MadiIn1 => ("MADI IN".into(), c + 1),
                SourceGroup::MadiIn33 => ("MADI IN".into(), c + 33),
                g => (source_group_name(g).into(), c + 1),
            }
        }
    }
}

/// Source display name with absolute channel numbering ("USB PLAY 40").
pub fn src_name(s: Source) -> String {
    match src_parts(s) {
        (n, 0) => n,
        (n, c) => format!("{n} {c}"),
    }
}

/// "LINE OUT 1-2" style range label.
pub fn out_range(b: OutputBank, a: usize, z: usize) -> String {
    if a == z {
        return out_name(b, a);
    }
    let first = out_name(b, a);
    let last = out_name(b, z);
    let n = last.rsplit(' ').next().unwrap_or("");
    format!("{first}-{n}")
}

pub fn src_range(s: Source, n: usize) -> String {
    match src_parts(s) {
        (name, 0) => name,
        (name, c) if n == 1 => format!("{name} {c}"),
        (name, c) => format!("{name} {c}-{}", c + n - 1),
    }
}

/// Pad columns to equal width.
pub fn table(rows: &[Vec<String>]) -> String {
    let cols = rows.iter().map(|r| r.len()).max().unwrap_or(0);
    let mut w = vec![0; cols];
    for r in rows {
        for (i, c) in r.iter().enumerate() {
            w[i] = w[i].max(c.chars().count());
        }
    }
    let mut out = String::new();
    for r in rows {
        let mut line = String::new();
        for (i, c) in r.iter().enumerate() {
            if i + 1 == r.len() {
                line.push_str(c);
            } else {
                line.push_str(c);
                line.extend(std::iter::repeat_n(' ', w[i] - c.chars().count() + 2));
            }
        }
        out.push_str(line.trim_end());
        out.push('\n');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn levels() {
        assert_eq!(atten_db("-30").unwrap().get(), 30);
        assert_eq!(atten_db("0").unwrap().get(), 0);
        assert_eq!(atten_db("-inf").unwrap(), Atten::NEG_INF);
        assert!(atten_db("6").is_err());
        assert!(atten_db("-97").is_err());
        assert_eq!(fmt_level(Atten::new(30).unwrap()), "-30");
        assert_eq!(fmt_level(Atten::new(95).unwrap()), "-inf");
    }

    #[test]
    fn pans() {
        assert_eq!(pan("-30").unwrap().get(), 2);
        assert_eq!(pan("+30").unwrap().get(), 62);
        assert_eq!(pan("C").unwrap(), Pan::CENTER);
        assert_eq!(pan("L5").unwrap().display(), -5);
        assert!(pan("31").is_err());
        assert_eq!(fmt_pan(Pan::new(40).unwrap()), "R8");
    }

    #[test]
    fn names() {
        assert_eq!(bank("mix ch4").unwrap(), OutputBank::MixCh4);
        assert_eq!(bank("S/PDIF OUT").unwrap(), OutputBank::SpdifOut);
        assert_eq!(bank("spdif out").unwrap(), OutputBank::SpdifOut);
        assert_eq!(output("MIX CH4:1").unwrap(), (OutputBank::MixCh4, 0));
        assert_eq!(output("MIX CH4").unwrap(), (OutputBank::MixCh4, 0));
        assert_eq!(
            source("ADAT IN:1").unwrap(),
            Source::Input {
                group: SourceGroup::AdatIn,
                chan: 0
            }
        );
        assert_eq!(clock_source("oven").unwrap(), ClockSource::Internal);
        assert_eq!(clock_source("S/PDIF").unwrap(), ClockSource::Spdif);
        assert_eq!(clock_source("madi2x").unwrap(), ClockSource::Madi2x);
        assert_eq!(sample_rate("44.1k").unwrap(), SampleRate::R44k1);
        assert_eq!(sample_rate("96000").unwrap(), SampleRate::R96k);
        assert_eq!(sample_rate("88.2").unwrap(), SampleRate::R88k2);
        assert_eq!(afx_type("clearq").unwrap(), AfxType::Equalizer);
        assert_eq!(
            afx_slot("PowerFFC:3").unwrap(),
            (AfxType::Compressor, Some(2))
        );
        assert!(afx_type("autowah").is_err());
        assert_eq!(out_name(OutputBank::UsbRec33, 7), "USB REC 40");
        assert_eq!(parse_output("USB REC 40"), Some((OutputBank::UsbRec33, 7)));
    }

    #[test]
    fn trims_and_hex() {
        let t = trim_dbu("14").unwrap();
        assert_eq!((t.whole, t.fract), (10, 0));
        assert_eq!(trim_dbu("14.5").unwrap().dbu(), 14.5);
        assert!(trim_dbu("25").is_err());
        assert_eq!(hex("cc 42 01").unwrap(), vec![0xcc, 0x42, 0x01]);
        assert!(hex("c").is_err());
    }
}
