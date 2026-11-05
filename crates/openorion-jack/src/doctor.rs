//! `openorion-jack doctor` (DESIGN.md sec. 9 item 4). Read-only: parses
//! `/proc/asound` and `/sys/bus/usb/devices`, asks JACK and openoriond.
//! Roots are parameters so tests run against fake trees.

use std::fmt;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use openorion_ipc::client::Client;
use openorion_ipc::{DaemonMsg, Health};
use openorion_proto::SampleRate;

/// Antelope's USB vendor id and the Orion32 HD product id.
pub const USB_VENDOR: &str = "23e5";
pub const USB_PRODUCT: &str = "a210";

// ---------------------------------------------------------------------------
// /proc/asound/cards

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AlsaCard {
    pub index: u32,
    /// The bracketed id (`hw:<id>` works as a device name).
    pub id: String,
    pub driver: String,
    pub name: String,
    pub longname: String,
}

/// Parse `/proc/asound/cards`:
/// ```text
///  2 [Orion32HD      ]: USB-Audio - Orion32 HD
///                       Antelope Audio Orion32 HD at usb-0000:00:14.0-2, super speed
/// ```
pub fn parse_cards(text: &str) -> Vec<AlsaCard> {
    let mut out: Vec<AlsaCard> = Vec::new();
    for line in text.lines() {
        let t = line.trim_start();
        let head = t.split_once('[').and_then(|(idx, rest)| {
            let index: u32 = idx.trim().parse().ok()?;
            let (id, rest) = rest.split_once(']')?;
            let rest = rest.trim_start().strip_prefix(':')?;
            let (driver, name) = rest.split_once(" - ").unwrap_or((rest, ""));
            Some(AlsaCard {
                index,
                id: id.trim().to_string(),
                driver: driver.trim().to_string(),
                name: name.trim().to_string(),
                longname: String::new(),
            })
        });
        match head {
            Some(c) => out.push(c),
            None => {
                if let Some(c) = out.last_mut()
                    && c.longname.is_empty()
                    && !t.is_empty()
                {
                    c.longname = t.trim().to_string();
                }
            }
        }
    }
    out
}

fn contains_ci(hay: &str, needle: &str) -> bool {
    hay.to_ascii_lowercase()
        .contains(&needle.to_ascii_lowercase())
}

/// The first card whose id, name or long name contains `needle`.
pub fn find_card<'a>(cards: &'a [AlsaCard], needle: &str) -> Option<&'a AlsaCard> {
    cards.iter().find(|c| {
        contains_ci(&c.id, needle)
            || contains_ci(&c.name, needle)
            || contains_ci(&c.longname, needle)
    })
}

// ---------------------------------------------------------------------------
// /proc/asound/cardN/stream0

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct StreamInfo {
    pub playback_channels: Option<u32>,
    pub capture_channels: Option<u32>,
    /// Union of the listed rates, sorted.
    pub rates: Vec<u32>,
}

/// Parse a snd-usb-audio `stream0` file: max `Channels:` per direction and
/// the `Rates:` lines.
pub fn parse_stream(text: &str) -> StreamInfo {
    let mut info = StreamInfo::default();
    let mut section: Option<bool> = None; // Some(true) = playback
    for line in text.lines() {
        let t = line.trim();
        if t == "Playback:" {
            section = Some(true);
        } else if t == "Capture:" {
            section = Some(false);
        } else if let Some(v) = t.strip_prefix("Channels:") {
            if let (Some(p), Ok(n)) = (section, v.trim().parse::<u32>()) {
                let slot = if p {
                    &mut info.playback_channels
                } else {
                    &mut info.capture_channels
                };
                *slot = Some(slot.map_or(n, |o| o.max(n)));
            }
        } else if let Some(v) = t.strip_prefix("Rates:") {
            for r in v.split(',') {
                if let Ok(r) = r.trim().parse::<u32>()
                    && !info.rates.contains(&r)
                {
                    info.rates.push(r);
                }
            }
        }
    }
    info.rates.sort_unstable();
    info
}

// ---------------------------------------------------------------------------
// sysfs

#[derive(Debug, Clone, PartialEq)]
pub struct UsbDevice {
    pub path: PathBuf,
    /// The `speed` attribute in Mb/s (480 = USB2 high speed, 5000+ = USB3).
    pub speed_mbps: Option<f64>,
}

impl UsbDevice {
    pub fn is_usb3(&self) -> Option<bool> {
        self.speed_mbps.map(|s| s >= 5000.0)
    }
}

fn read_trim(p: &Path) -> Option<String> {
    std::fs::read_to_string(p)
        .ok()
        .map(|s| s.trim().to_string())
}

/// Find the Orion under `<sys_root>/bus/usb/devices` (read-only).
pub fn find_usb_device(sys_root: &Path) -> Vec<UsbDevice> {
    let dir = sys_root.join("bus/usb/devices");
    let Ok(rd) = std::fs::read_dir(&dir) else {
        return Vec::new();
    };
    let mut out: Vec<UsbDevice> = rd
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            read_trim(&p.join("idVendor")).is_some_and(|v| v.eq_ignore_ascii_case(USB_VENDOR))
                && read_trim(&p.join("idProduct"))
                    .is_some_and(|v| v.eq_ignore_ascii_case(USB_PRODUCT))
        })
        .map(|path| UsbDevice {
            speed_mbps: read_trim(&path.join("speed")).and_then(|s| s.parse().ok()),
            path,
        })
        .collect();
    out.sort_by(|a, b| a.path.cmp(&b.path));
    out
}

// ---------------------------------------------------------------------------
// Report

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Level {
    Ok,
    Info,
    Skip,
    Warn,
    Fail,
}

impl fmt::Display for Level {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Level::Ok => "  ok",
            Level::Info => "info",
            Level::Skip => "skip",
            Level::Warn => "WARN",
            Level::Fail => "FAIL",
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Check {
    pub level: Level,
    pub title: String,
    pub detail: String,
}

impl fmt::Display for Check {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "[{}] {}", self.level, self.title)?;
        for l in self.detail.lines() {
            write!(f, "\n       {l}")?;
        }
        Ok(())
    }
}

/// What the JACK probe found (filled by the `jack` feature code).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum JackProbe {
    /// Built without the `jack` feature.
    NotBuilt,
    Unreachable(String),
    Running {
        rate: u32,
        buffer: u32,
        /// Orion ports found (capture, playback).
        orion_ports: (usize, usize),
        /// Physical audio port count, for the "nothing matched" hint.
        physical_ports: usize,
    },
}

/// What the daemon probe found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DaemonProbe {
    Unreachable(String),
    Up {
        sim: bool,
        health: Option<Health>,
        detail: Option<String>,
        rate: Option<SampleRate>,
    },
}

/// Ask openoriond for health and the sample rate (waits up to `wait`).
pub fn probe_daemon(socket: &Path, wait: Duration) -> DaemonProbe {
    let mut c = match Client::connect(socket) {
        Ok(c) => c,
        Err(e) => return DaemonProbe::Unreachable(e.to_string()),
    };
    let sim = c.info().sim;
    if let Err(e) = c.subscribe(false) {
        return DaemonProbe::Unreachable(e.to_string());
    }
    let (mut health, mut detail, mut rate) = (None, None, None);
    let deadline = Instant::now() + wait;
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            break;
        }
        match c.recv(left) {
            Ok(Some(DaemonMsg::Health {
                health: h,
                detail: d,
            })) => {
                health = Some(h);
                detail = d;
                if h != Health::Ok && h != Health::Syncing {
                    break;
                }
            }
            Ok(Some(DaemonMsg::State { state })) => {
                rate = state.status.sample_rate;
                break;
            }
            Ok(Some(_)) => {}
            Ok(None) | Err(_) => break,
        }
    }
    DaemonProbe::Up {
        sim,
        health,
        detail,
        rate,
    }
}

pub struct DoctorInput {
    pub proc_root: PathBuf,
    pub sys_root: PathBuf,
    /// Card match, e.g. "Orion32".
    pub needle: String,
    pub jack: JackProbe,
    pub daemon: DaemonProbe,
}

/// Run every check. Pure apart from reading the two roots.
pub fn run(inp: &DoctorInput) -> Vec<Check> {
    let mut out = Vec::new();
    let mut push = |level, title: String, detail: String| {
        out.push(Check {
            level,
            title,
            detail,
        })
    };

    // ALSA card.
    let cards_path = inp.proc_root.join("asound/cards");
    let cards = std::fs::read_to_string(&cards_path)
        .map(|t| parse_cards(&t))
        .unwrap_or_default();
    let card = find_card(&cards, &inp.needle).cloned();
    match &card {
        Some(c) => push(
            Level::Ok,
            format!("ALSA card {} [{}] ({})", c.index, c.id, c.driver),
            c.longname.clone(),
        ),
        None => push(
            Level::Fail,
            format!("no ALSA card matching \"{}\"", inp.needle),
            format!(
                "{} lists {} card(s). Is the Orion on, connected to this host \
                 and bound to snd-usb-audio?",
                cards_path.display(),
                cards.len()
            ),
        ),
    }
    if let Some(c) = &card
        && c.driver != "USB-Audio"
    {
        push(
            Level::Warn,
            format!("card driver is \"{}\", expected USB-Audio", c.driver),
            String::new(),
        );
    }

    // Channels from stream0.
    let stream = card.as_ref().and_then(|c| {
        std::fs::read_to_string(
            inp.proc_root
                .join(format!("asound/card{}/stream0", c.index)),
        )
        .ok()
    });
    let stream = stream.map(|t| parse_stream(&t));
    if let Some(s) = &stream {
        let fmt_ch = |c: Option<u32>| c.map_or("?".to_string(), |n| n.to_string());
        let full = s.playback_channels == Some(64) && s.capture_channels == Some(64);
        push(
            if full { Level::Ok } else { Level::Warn },
            format!(
                "USB audio channels: {} playback, {} capture",
                fmt_ch(s.playback_channels),
                fmt_ch(s.capture_channels)
            ),
            if full {
                String::new()
            } else {
                "Expected 64 each; fewer usually means a USB2 link or a \
                 high sample rate."
                    .into()
            },
        );
    }

    // USB link speed.
    let devs = find_usb_device(&inp.sys_root);
    match devs.first() {
        None => push(
            Level::Fail,
            format!("no USB device {USB_VENDOR}:{USB_PRODUCT} in sysfs"),
            String::new(),
        ),
        Some(d) => match d.is_usb3() {
            Some(true) => push(
                Level::Ok,
                format!(
                    "USB link {} Mb/s (USB3: 64 channels)",
                    d.speed_mbps.unwrap_or(0.0)
                ),
                d.path.display().to_string(),
            ),
            Some(false) => push(
                Level::Warn,
                format!(
                    "USB link {} Mb/s (USB2: only 8 channels each way)",
                    d.speed_mbps.unwrap_or(0.0)
                ),
                "Use a USB3 port and cable for 64 channels.".into(),
            ),
            None => push(
                Level::Warn,
                "USB link speed unknown".into(),
                d.path.display().to_string(),
            ),
        },
    }

    // JACK.
    let mut jack_rate = None;
    match &inp.jack {
        JackProbe::NotBuilt => push(
            Level::Skip,
            "JACK checks skipped".into(),
            "openorion-jack was built without the `jack` feature.".into(),
        ),
        JackProbe::Unreachable(e) => push(Level::Info, "JACK server not running".into(), e.clone()),
        JackProbe::Running {
            rate,
            buffer,
            orion_ports,
            physical_ports,
        } => {
            jack_rate = Some(*rate);
            push(
                Level::Ok,
                format!("JACK running at {rate} Hz, {buffer} frames"),
                String::new(),
            );
            if orion_ports.0 + orion_ports.1 == 0 {
                push(
                    Level::Warn,
                    "JACK has no ports of the Orion".into(),
                    format!(
                        "{physical_ports} physical audio port(s), none matching. \
                         Start JACK on the Orion (below), or pass \
                         --device-match / --client."
                    ),
                );
            } else {
                push(
                    Level::Ok,
                    format!(
                        "JACK Orion ports: {} capture, {} playback",
                        orion_ports.0, orion_ports.1
                    ),
                    String::new(),
                );
            }
        }
    }

    // Daemon.
    let mut orion_rate = None;
    match &inp.daemon {
        DaemonProbe::Unreachable(e) => push(
            Level::Warn,
            "openoriond not reachable".into(),
            format!("{e}\nPort naming and the rate guard need openoriond."),
        ),
        DaemonProbe::Up {
            sim,
            health,
            detail,
            rate,
        } => {
            orion_rate = *rate;
            let lvl = match health {
                Some(Health::Ok) => Level::Ok,
                Some(Health::Syncing) | None => Level::Info,
                Some(_) => Level::Warn,
            };
            let h = health.map_or("unknown".to_string(), |h| format!("{h:?}").to_lowercase());
            push(
                lvl,
                format!(
                    "openoriond reachable{}, health {h}{}",
                    if *sim { " (simulator)" } else { "" },
                    rate.map_or(String::new(), |r| format!(", Orion at {} Hz", r.hz()))
                ),
                detail.clone().unwrap_or_default(),
            );
        }
    }

    if let (Some(j), Some(o)) = (jack_rate, orion_rate)
        && j != o.hz()
    {
        push(
            Level::Warn,
            format!("rate mismatch: JACK {j} Hz, Orion {} Hz", o.hz()),
            "Restart JACK at the Orion's rate, or run openorion-jack --follow-jack.".into(),
        );
    }

    // Suggestion.
    let rate = orion_rate.map(|r| r.hz()).or(jack_rate).unwrap_or(48_000);
    let dev = card
        .as_ref()
        .map_or("hw:Orion32HD".to_string(), |c| format!("hw:{}", c.id));
    push(
        Level::Info,
        "suggested setup".into(),
        format!(
            "JACK:     jackd -d alsa -d {dev} -r {rate} -p 128 -n 2\n\
             PipeWire: pw-metadata -n settings 0 clock.force-rate {rate}\n          \
             pw-metadata -n settings 0 clock.force-quantum 128\n          \
             (use the card's Pro Audio profile for all channels)"
        ),
    );
    out
}

/// Exit status: 1 if any check failed.
pub fn exit_code(checks: &[Check]) -> i32 {
    i32::from(checks.iter().any(|c| c.level == Level::Fail))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    const CARDS: &str = " 0 [PCH            ]: HDA-Intel - HDA Intel PCH
                      HDA Intel PCH at 0xf7f10000 irq 32
 2 [Orion32HD      ]: USB-Audio - Orion32 HD
                      Antelope Audio Orion32 HD at usb-0000:00:14.0-2, super speed
";

    const STREAM: &str = "Antelope Audio Orion32 HD at usb-0000:00:14.0-2, super speed : USB Audio

Playback:
  Status: Stop
  Interface 1
    Altset 1
    Format: S32_LE
    Channels: 64
    Endpoint: 0x01 (1 OUT) (ASYNC)
    Rates: 44100, 48000, 88200, 96000
Capture:
  Status: Stop
  Interface 2
    Altset 1
    Format: S32_LE
    Channels: 64
    Endpoint: 0x81 (1 IN) (ASYNC)
    Rates: 44100, 48000, 88200, 96000, 192000
";

    #[test]
    fn cards_and_stream() {
        let cards = parse_cards(CARDS);
        assert_eq!(cards.len(), 2);
        let c = find_card(&cards, "orion32").unwrap();
        assert_eq!(c.index, 2);
        assert_eq!(c.id, "Orion32HD");
        assert_eq!(c.driver, "USB-Audio");
        assert_eq!(c.name, "Orion32 HD");
        assert!(c.longname.ends_with("super speed"));
        assert!(find_card(&cards, "babyface").is_none());

        let s = parse_stream(STREAM);
        assert_eq!(s.playback_channels, Some(64));
        assert_eq!(s.capture_channels, Some(64));
        assert_eq!(s.rates, vec![44100, 48000, 88200, 96000, 192000]);
    }

    fn fake_tree(speed: &str) -> tempfile::TempDir {
        let d = tempfile::tempdir().unwrap();
        let proc_ = d.path().join("proc");
        fs::create_dir_all(proc_.join("asound/card2")).unwrap();
        fs::write(proc_.join("asound/cards"), CARDS).unwrap();
        fs::write(proc_.join("asound/card2/stream0"), STREAM).unwrap();
        let devs = d.path().join("sys/bus/usb/devices");
        for (name, v, p, s) in [
            ("1-1", "8087", "0024", "480"),
            ("2-2", USB_VENDOR, USB_PRODUCT, speed),
        ] {
            fs::create_dir_all(devs.join(name)).unwrap();
            fs::write(devs.join(name).join("idVendor"), format!("{v}\n")).unwrap();
            fs::write(devs.join(name).join("idProduct"), format!("{p}\n")).unwrap();
            fs::write(devs.join(name).join("speed"), format!("{s}\n")).unwrap();
        }
        // An interface entry has no idVendor; must be skipped.
        fs::create_dir_all(devs.join("2-2:1.0")).unwrap();
        d
    }

    fn input(d: &Path, jack: JackProbe, daemon: DaemonProbe) -> DoctorInput {
        DoctorInput {
            proc_root: d.join("proc"),
            sys_root: d.join("sys"),
            needle: "Orion32".into(),
            jack,
            daemon,
        }
    }

    #[test]
    fn doctor_usb3_all_good() {
        let d = fake_tree("5000");
        let devs = find_usb_device(&d.path().join("sys"));
        assert_eq!(devs.len(), 1);
        assert_eq!(devs[0].is_usb3(), Some(true));
        let checks = run(&input(
            d.path(),
            JackProbe::Running {
                rate: 48_000,
                buffer: 128,
                orion_ports: (64, 64),
                physical_ports: 128,
            },
            DaemonProbe::Up {
                sim: false,
                health: Some(Health::Ok),
                detail: None,
                rate: Some(SampleRate::R48k),
            },
        ));
        let text: Vec<String> = checks.iter().map(|c| c.to_string()).collect();
        assert!(
            checks.iter().all(|c| c.level <= Level::Info),
            "{}",
            text.join("\n")
        );
        assert_eq!(exit_code(&checks), 0);
        let all = text.join("\n");
        assert!(all.contains("jackd -d alsa -d hw:Orion32HD -r 48000 -p 128 -n 2"));
        assert!(all.contains("64 playback, 64 capture"));
    }

    #[test]
    fn doctor_usb2_mismatch_no_jack() {
        let d = fake_tree("480");
        let checks = run(&input(
            d.path(),
            JackProbe::NotBuilt,
            DaemonProbe::Up {
                sim: true,
                health: Some(Health::Ok),
                detail: None,
                rate: Some(SampleRate::R96k),
            },
        ));
        let all: String = checks.iter().map(|c| format!("{c}\n")).collect();
        assert!(all.contains("[WARN] USB link 480 Mb/s (USB2"), "{all}");
        assert!(all.contains("[skip] JACK checks skipped"));
        assert!(all.contains("-r 96000"));

        let checks = run(&input(
            d.path(),
            JackProbe::Running {
                rate: 44_100,
                buffer: 256,
                orion_ports: (0, 0),
                physical_ports: 4,
            },
            DaemonProbe::Up {
                sim: false,
                health: Some(Health::Ok),
                detail: None,
                rate: Some(SampleRate::R96k),
            },
        ));
        let all: String = checks.iter().map(|c| format!("{c}\n")).collect();
        assert!(all.contains("rate mismatch: JACK 44100 Hz, Orion 96000 Hz"));
        assert!(all.contains("JACK has no ports of the Orion"));
    }

    #[test]
    fn doctor_nothing_present() {
        let d = tempfile::tempdir().unwrap();
        let checks = run(&input(
            d.path(),
            JackProbe::Unreachable("no server".into()),
            probe_daemon(&d.path().join("missing.sock"), Duration::from_millis(10)),
        ));
        assert_eq!(exit_code(&checks), 1);
        let all: String = checks.iter().map(|c| format!("{c}\n")).collect();
        assert!(all.contains("no ALSA card matching"));
        assert!(all.contains("no USB device 23e5:a210"));
        assert!(all.contains("openoriond not reachable"));
        assert!(all.contains("hw:Orion32HD -r 48000"));
    }
}
