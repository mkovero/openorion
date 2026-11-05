//! Port naming from the routing state (DESIGN.md sec. 9 item 1).
//!
//! Pure logic, no JACK: which source feeds each capture channel
//! (`USB REC n`), where each playback channel (`USB PLAY n`) goes, and
//! how JACK ports of the Orion's ALSA card map to channel numbers.

use openorion_proto::names::{output_bank_name, source_name};
use openorion_proto::{DeviceState, OutputBank, RoutingBank, Source, SourceGroup};

/// USB channels per direction at USB3 (32 per routing bank, two banks).
pub const USB_CHANNELS: u32 = 64;

/// Direction of a JACK port as seen from the host.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Dir {
    /// Orion → host (`USB REC n`); a JACK output port of the system client.
    Capture,
    /// Host → Orion (`USB PLAY n`); a JACK input port of the system client.
    Playback,
}

/// Names for one channel.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChannelName {
    /// Shown in patchbays via `http://jackaudio.org/metadata/pretty-name`.
    pub pretty: String,
    /// A short alias-friendly form (no spaces), e.g. `rec01-ADAT_IN_1`.
    pub alias: String,
}

fn bank(state: &DeviceState, b: OutputBank) -> Option<&RoutingBank> {
    state
        .routing
        .get(b as usize)
        .filter(|r| r.bank == b)
        .or_else(|| state.routing.iter().find(|r| r.bank == b))
}

/// The routing bank and 0-based index behind `USB REC n` (1-based n).
fn rec_slot(n: u32) -> Option<(OutputBank, usize)> {
    match n {
        1..=32 => Some((OutputBank::UsbRec1, (n - 1) as usize)),
        33..=64 => Some((OutputBank::UsbRec33, (n - 33) as usize)),
        _ => None,
    }
}

/// The routing source `USB PLAY n` (1-based n).
fn play_source(n: u32) -> Option<Source> {
    match n {
        1..=32 => Some(Source::Input {
            group: SourceGroup::UsbPlay1,
            chan: (n - 1) as u8,
        }),
        33..=64 => Some(Source::Input {
            group: SourceGroup::UsbPlay33,
            chan: (n - 33) as u8,
        }),
        _ => None,
    }
}

/// "LINE OUT 3", "USB REC 40", "MIX CH2 5", "MONITOR L".
pub fn output_name(b: OutputBank, idx: usize) -> String {
    match b {
        OutputBank::UsbRec1 => format!("USB REC {}", idx + 1),
        OutputBank::UsbRec33 => format!("USB REC {}", idx + 33),
        OutputBank::MadiOut1 => format!("MADI OUT {}", idx + 1),
        OutputBank::MadiOut33 => format!("MADI OUT {}", idx + 33),
        OutputBank::Monitor => match idx {
            0 => "MONITOR L".into(),
            1 => "MONITOR R".into(),
            _ => format!("MONITOR {}", idx + 1),
        },
        _ => format!("{} {}", output_bank_name(b), idx + 1),
    }
}

/// What feeds `USB REC n`; None if n is out of range or the bank is
/// missing from the state.
pub fn capture_source(state: &DeviceState, n: u32) -> Option<Source> {
    let (b, i) = rec_slot(n)?;
    bank(state, b)?.sources.get(i).copied()
}

/// Every output `USB PLAY n` is routed to, in bank order.
pub fn playback_destinations(state: &DeviceState, n: u32) -> Vec<(OutputBank, usize)> {
    let Some(src) = play_source(n) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for b in OutputBank::ALL {
        if let Some(rb) = bank(state, *b) {
            for (i, s) in rb.sources.iter().enumerate() {
                if *s == src {
                    out.push((*b, i));
                }
            }
        }
    }
    out
}

fn slug(s: &str) -> String {
    s.chars()
        .map(|c| match c {
            'A'..='Z' | 'a'..='z' | '0'..='9' | '-' => c,
            _ => '_',
        })
        .collect()
}

/// How many destinations a playback pretty name lists before "+k".
const MAX_DESTS: usize = 3;

/// Name of capture channel n (1-based).
pub fn capture_name(state: &DeviceState, n: u32) -> ChannelName {
    let pretty = match capture_source(state, n) {
        Some(s @ Source::Input { .. }) => source_name(s),
        Some(Source::Mute) => format!("USB REC {n} (muted)"),
        None => format!("USB REC {n}"),
    };
    ChannelName {
        alias: format!("openorion:rec{n:02}-{}", slug(&pretty)),
        pretty,
    }
}

/// Name of playback channel n (1-based).
pub fn playback_name(state: &DeviceState, n: u32) -> ChannelName {
    let dests = playback_destinations(state, n);
    let pretty = if dests.is_empty() {
        format!("USB PLAY {n} (unrouted)")
    } else {
        let mut parts: Vec<String> = dests
            .iter()
            .take(MAX_DESTS)
            .map(|&(b, i)| output_name(b, i))
            .collect();
        if dests.len() > MAX_DESTS {
            parts.push(format!("+{}", dests.len() - MAX_DESTS));
        }
        parts.join(", ")
    };
    ChannelName {
        alias: format!("openorion:play{n:02}-{}", slug(&pretty)),
        pretty,
    }
}

/// Name of channel n in direction `dir`.
pub fn channel_name(state: &DeviceState, dir: Dir, n: u32) -> ChannelName {
    match dir {
        Dir::Capture => capture_name(state, n),
        Dir::Playback => playback_name(state, n),
    }
}

// ---------------------------------------------------------------------------
// Port discovery

/// How to recognise the Orion's ports.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PortMatch {
    /// Case-insensitive substrings; a port matches when its full name or
    /// one of its aliases contains any of them.
    pub needles: Vec<String>,
}

impl PortMatch {
    pub fn new(primary: &str) -> Self {
        PortMatch {
            needles: vec![primary.to_string()],
        }
    }

    /// Also accept `hw:<card>` style aliases of the ALSA card (the JACK
    /// ALSA backend names aliases after the device string it was given,
    /// e.g. `alsa_pcm:hw:2:out1` or `alsa_pcm:hw:Orion32HD:out1`).
    pub fn with_alsa_card(mut self, index: u32, id: &str) -> Self {
        self.needles.push(format!("hw:{index}:"));
        self.needles.push(format!("hw:{index},"));
        if !id.is_empty() {
            self.needles.push(format!("hw:{id}:"));
            self.needles.push(format!("hw:{id},"));
        }
        self
    }

    pub fn matches(&self, name: &str, aliases: &[String]) -> bool {
        let hit = |s: &str| {
            let s = s.to_ascii_lowercase();
            self.needles
                .iter()
                .any(|n| !n.is_empty() && s.contains(&n.to_ascii_lowercase()))
        };
        hit(name) || aliases.iter().any(|a| hit(a))
    }
}

/// 1-based channel number from a port's short name: `capture_3` → 3,
/// `playback_12` → 12, PipeWire `capture_AUX0` → 1 (AUX is 0-based).
pub fn channel_of(short_name: &str) -> Option<u32> {
    let digits: String = short_name
        .chars()
        .rev()
        .take_while(|c| c.is_ascii_digit())
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    if digits.is_empty() {
        return None;
    }
    let v: u32 = digits.parse().ok()?;
    let head = &short_name[..short_name.len() - digits.len()];
    let n = if head.to_ascii_uppercase().ends_with("AUX") {
        v + 1
    } else {
        v
    };
    (1..=USB_CHANNELS).contains(&n).then_some(n)
}

/// A JACK port as discovery sees it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PortInfo {
    pub name: String,
    pub aliases: Vec<String>,
    /// JACK output port (data flows out of it into JACK) = capture.
    pub is_output: bool,
}

/// One Orion port with its channel.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OrionPort {
    pub name: String,
    pub dir: Dir,
    pub n: u32,
}

/// Pick the Orion's ports out of the physical audio ports. With
/// `client_fallback` (e.g. "system"), ports of that JACK client count too
/// when nothing matched by name or alias.
pub fn discover(
    ports: &[PortInfo],
    m: &PortMatch,
    client_fallback: Option<&str>,
) -> Vec<OrionPort> {
    let pick = |p: &PortInfo| -> Option<OrionPort> {
        let short = p.name.rsplit(':').next().unwrap_or(&p.name);
        Some(OrionPort {
            name: p.name.clone(),
            dir: if p.is_output {
                Dir::Capture
            } else {
                Dir::Playback
            },
            n: channel_of(short)?,
        })
    };
    let mut out: Vec<OrionPort> = ports
        .iter()
        .filter(|p| m.matches(&p.name, &p.aliases))
        .filter_map(pick)
        .collect();
    if out.is_empty()
        && let Some(c) = client_fallback
    {
        let prefix = format!("{c}:");
        out = ports
            .iter()
            .filter(|p| p.name.starts_with(&prefix))
            .filter_map(pick)
            .collect();
    }
    out.sort_by(|a, b| (a.dir, a.n, &a.name).cmp(&(b.dir, b.n, &b.name)));
    out
}

/// Pretty names for every discovered port: (port name, names).
pub fn plan(state: &DeviceState, ports: &[OrionPort]) -> Vec<(String, ChannelName)> {
    ports
        .iter()
        .map(|p| (p.name.clone(), channel_name(state, p.dir, p.n)))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use openorion_proto::decode::empty_state;

    fn src(group: SourceGroup, chan: u8) -> Source {
        Source::Input { group, chan }
    }

    fn set(state: &mut DeviceState, b: OutputBank, i: usize, s: Source) {
        let rb = state.routing.iter_mut().find(|r| r.bank == b).unwrap();
        rb.sources[i] = s;
    }

    #[test]
    fn capture_follows_usb_rec() {
        let mut st = empty_state();
        set(&mut st, OutputBank::UsbRec1, 0, src(SourceGroup::AdatIn, 0));
        set(
            &mut st,
            OutputBank::UsbRec33,
            7,
            src(SourceGroup::LineIn, 4),
        );
        assert_eq!(capture_name(&st, 1).pretty, "ADAT IN 1");
        assert_eq!(capture_name(&st, 1).alias, "openorion:rec01-ADAT_IN_1");
        assert_eq!(capture_name(&st, 40).pretty, "LINE IN 5");
        assert_eq!(capture_name(&st, 2).pretty, "USB REC 2 (muted)");
        assert_eq!(capture_name(&st, 65).pretty, "USB REC 65");
    }

    #[test]
    fn playback_lists_destinations() {
        let mut st = empty_state();
        let p1 = src(SourceGroup::UsbPlay1, 0);
        set(&mut st, OutputBank::LineOut, 0, p1);
        set(&mut st, OutputBank::Monitor, 0, p1);
        set(&mut st, OutputBank::MixCh2, 4, p1);
        set(&mut st, OutputBank::UsbRec33, 0, p1);
        set(
            &mut st,
            OutputBank::AdatOut,
            2,
            src(SourceGroup::UsbPlay33, 1),
        );
        assert_eq!(
            playback_name(&st, 1).pretty,
            "LINE OUT 1, MONITOR L, USB REC 33, +1"
        );
        assert_eq!(playback_name(&st, 34).pretty, "ADAT OUT 3");
        assert_eq!(playback_name(&st, 2).pretty, "USB PLAY 2 (unrouted)");
        assert_eq!(
            playback_destinations(&st, 1),
            vec![
                (OutputBank::LineOut, 0),
                (OutputBank::Monitor, 0),
                (OutputBank::UsbRec33, 0),
                (OutputBank::MixCh2, 4)
            ]
        );
    }

    #[test]
    fn channel_numbers() {
        assert_eq!(channel_of("capture_1"), Some(1));
        assert_eq!(channel_of("playback_64"), Some(64));
        assert_eq!(channel_of("capture_AUX0"), Some(1));
        assert_eq!(channel_of("playback_AUX63"), Some(64));
        assert_eq!(channel_of("capture_65"), None);
        assert_eq!(channel_of("capture_FL"), None);
        assert_eq!(channel_of("out0"), None);
    }

    fn port(name: &str, aliases: &[&str], is_output: bool) -> PortInfo {
        PortInfo {
            name: name.into(),
            aliases: aliases.iter().map(|s| s.to_string()).collect(),
            is_output,
        }
    }

    #[test]
    fn discovery_by_alias_card_and_fallback() {
        let ports = vec![
            port("system:capture_2", &["alsa_pcm:hw:Orion32HD:out2"], true),
            port("system:capture_1", &["alsa_pcm:hw:Orion32HD:out1"], true),
            port("system:playback_1", &["alsa_pcm:hw:Orion32HD:in1"], false),
            port("Other:capture_1", &["alsa_pcm:hw:1:out1"], true),
        ];
        let m = PortMatch::new("orion32");
        let got = discover(&ports, &m, None);
        assert_eq!(
            got.iter().map(|p| (p.dir, p.n)).collect::<Vec<_>>(),
            vec![(Dir::Capture, 1), (Dir::Capture, 2), (Dir::Playback, 1)]
        );

        // Device given by number: only the hw:<index> needle matches.
        let ports2 = vec![
            port("system:capture_1", &["alsa_pcm:hw:2:out1"], true),
            port("x:capture_1", &["alsa_pcm:hw:20:out1"], true),
        ];
        assert!(discover(&ports2, &m, None).is_empty());
        let m2 = PortMatch::new("orion32").with_alsa_card(2, "Orion32HD");
        let got = discover(&ports2, &m2, None);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].name, "system:capture_1");

        // PipeWire-JACK: the client name carries the device name.
        let pw = vec![port("Orion32 HD Pro:capture_AUX3", &[], true)];
        assert_eq!(discover(&pw, &m, None)[0].n, 4);

        // Fallback to a named client.
        let plain = vec![port("system:playback_3", &[], false)];
        assert!(discover(&plain, &m, None).is_empty());
        assert_eq!(discover(&plain, &m, Some("system"))[0].n, 3);
    }
}
