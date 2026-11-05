//! The JACK side (feature `jack`): port discovery, pretty-names and
//! aliases, the rate guard loop and the MIDI input.
//!
//! JACK callbacks never talk to the server or openoriond; they forward events
//! over a channel (and MIDI bytes over a lock-free ring buffer) to the main
//! loop, which does the work.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use jack::{
    AsyncClient, Client, ClientOptions, ClientStatus, Control, Frames, MidiIn, NotificationHandler,
    Port, PortFlags, PortId, ProcessHandler, ProcessScope, Property, RingBuffer, RingBufferReader,
    RingBufferWriter,
};
use openorion_ipc::client::Client as Ipc;
use openorion_ipc::{DaemonMsg, Op};
use openorion_proto::{DeviceState, SampleRate};

use crate::doctor::{JackProbe, find_card, parse_cards};
use crate::midimap::{MidiMap, Target, op_for};
use crate::naming::{ChannelName, OrionPort, PortInfo, PortMatch, discover, plan};
use crate::rate::{RateAction, check};
use crate::track::Tracker;
use crate::{info, warn};

/// JACK metadata key for human-readable port names.
pub const PRETTY_NAME: &str = "http://jackaudio.org/metadata/pretty-name";

/// Prefix of the aliases openorion-jack sets (so it can find its own).
pub const ALIAS_PREFIX: &str = "openorion:";

#[derive(Debug, Clone)]
pub struct RunOpts {
    pub socket: PathBuf,
    pub client_name: String,
    pub device_match: String,
    /// JACK client whose ports count as the Orion's if nothing matched.
    pub client_fallback: Option<String>,
    pub follow_jack: bool,
    pub aliases: bool,
    pub midi_map: Option<MidiMap>,
    /// Apply names once and exit (names stay).
    pub once: bool,
    /// `/proc` root, for finding the ALSA card number.
    pub proc_root: PathBuf,
}

// ---------------------------------------------------------------------------
// Discovery and naming

/// Physical audio ports (all audio ports if no physical one matches),
/// filtered to the Orion's.
pub fn discover_ports(client: &Client, m: &PortMatch, fallback: Option<&str>) -> Vec<OrionPort> {
    let collect = |flags: PortFlags| -> Vec<PortInfo> {
        client
            .ports(None, Some("audio"), flags)
            .into_iter()
            .filter_map(|name| {
                let p = client.port_by_name(&name)?;
                Some(PortInfo {
                    aliases: p.aliases().unwrap_or_default(),
                    is_output: p.flags().contains(PortFlags::IS_OUTPUT),
                    name,
                })
            })
            .collect()
    };
    let physical = collect(PortFlags::IS_PHYSICAL);
    let found = discover(&physical, m, fallback);
    if !found.is_empty() {
        return found;
    }
    discover(&collect(PortFlags::empty()), m, None)
}

/// The matcher for `opts`: the device needle plus the ALSA card's
/// `hw:<n>` names when /proc lists it.
pub fn port_match(device_match: &str, proc_root: &std::path::Path) -> PortMatch {
    let m = PortMatch::new(device_match);
    let cards = std::fs::read_to_string(proc_root.join("asound/cards"))
        .map(|t| parse_cards(&t))
        .unwrap_or_default();
    match find_card(&cards, device_match) {
        Some(c) => m.with_alsa_card(c.index, &c.id),
        None => m,
    }
}

fn port_uuid(port: &Port<jack::Unowned>) -> jack::jack_sys::jack_uuid_t {
    // SAFETY: `raw()` is a live port handle owned by the server.
    unsafe { jack::jack_sys::jack_port_uuid(port.raw()) }
}

/// Applies names to ports and remembers what it changed, to restore it.
#[derive(Default)]
pub struct Namer {
    applied: HashMap<String, ChannelName>,
    /// The pretty-name each port had before we touched it.
    original: HashMap<String, Option<Property>>,
    pub aliases: bool,
    warned_meta: bool,
    warned_alias: bool,
}

impl Namer {
    pub fn new(aliases: bool) -> Self {
        Namer {
            aliases,
            ..Default::default()
        }
    }

    /// Set names for `ports`; only ports whose name changed are touched.
    /// Returns how many ports were updated.
    pub fn apply(&mut self, client: &Client, state: &DeviceState, ports: &[OrionPort]) -> usize {
        let mut n = 0;
        for (port_name, name) in plan(state, ports) {
            if self.applied.get(&port_name) == Some(&name) {
                continue;
            }
            let Some(mut port) = client.port_by_name(&port_name) else {
                continue;
            };
            let uuid = port_uuid(&port);
            self.original
                .entry(port_name.clone())
                .or_insert_with(|| client.property_get(uuid, PRETTY_NAME));
            if let Err(e) =
                client.property_set(uuid, PRETTY_NAME, &Property::new(&name.pretty, None))
                && !self.warned_meta
            {
                warn(&format!(
                    "cannot set JACK metadata pretty-name on {port_name}: {e:?} \
                     (server without metadata support?)"
                ));
                self.warned_meta = true;
            }
            if self.aliases {
                for a in port.aliases().unwrap_or_default() {
                    if a.starts_with(ALIAS_PREFIX) {
                        let _ = port.unset_alias(&a);
                    }
                }
                if let Err(e) = port.set_alias(&name.alias)
                    && !self.warned_alias
                {
                    warn(&format!(
                        "cannot set alias on {port_name}: {e:?} (both alias slots in use?)"
                    ));
                    self.warned_alias = true;
                }
            }
            self.applied.insert(port_name, name);
            n += 1;
        }
        n
    }

    /// Forget ports that went away.
    pub fn retain(&mut self, ports: &[OrionPort]) {
        self.applied
            .retain(|k, _| ports.iter().any(|p| &p.name == k));
    }

    /// Put back the original pretty-names and drop our aliases.
    pub fn restore(&mut self, client: &Client) {
        for (port_name, orig) in self.original.drain() {
            let Some(mut port) = client.port_by_name(&port_name) else {
                continue;
            };
            let uuid = port_uuid(&port);
            let _ = match orig {
                Some(p) => client.property_set(uuid, PRETTY_NAME, &p),
                None => client.property_remove(uuid, PRETTY_NAME),
            };
            if self.aliases {
                for a in port.aliases().unwrap_or_default() {
                    if a.starts_with(ALIAS_PREFIX) {
                        let _ = port.unset_alias(&a);
                    }
                }
            }
        }
        self.applied.clear();
    }
}

/// Doctor's JACK check: connect without starting a server, report.
pub fn probe(device_match: &str, proc_root: &std::path::Path, fallback: Option<&str>) -> JackProbe {
    let client = match Client::new("openorion-jack-doctor", ClientOptions::NO_START_SERVER) {
        Ok((c, _)) => c,
        Err(e) => return JackProbe::Unreachable(format!("{e:?}")),
    };
    let ports = discover_ports(&client, &port_match(device_match, proc_root), fallback);
    let cap = ports
        .iter()
        .filter(|p| p.dir == crate::naming::Dir::Capture)
        .count();
    JackProbe::Running {
        rate: client.sample_rate() as u32,
        buffer: client.buffer_size(),
        orion_ports: (cap, ports.len() - cap),
        physical_ports: client
            .ports(None, Some("audio"), PortFlags::IS_PHYSICAL)
            .len(),
    }
}

// ---------------------------------------------------------------------------
// JACK client

enum JackEvent {
    SampleRate(u32),
    Ports,
    Shutdown(String),
}

struct Notif {
    tx: mpsc::Sender<JackEvent>,
}

impl NotificationHandler for Notif {
    unsafe fn shutdown(&mut self, _status: ClientStatus, reason: &str) {
        let _ = self.tx.send(JackEvent::Shutdown(reason.to_string()));
    }

    fn sample_rate(&mut self, _: &Client, srate: Frames) -> Control {
        let _ = self.tx.send(JackEvent::SampleRate(srate));
        Control::Continue
    }

    fn port_registration(&mut self, _: &Client, _id: PortId, _reg: bool) {
        let _ = self.tx.send(JackEvent::Ports);
    }

    fn ports_connected(&mut self, _: &Client, _a: PortId, _b: PortId, _c: bool) {}
}

struct Proc {
    midi: Option<Port<MidiIn>>,
    out: RingBufferWriter,
}

impl ProcessHandler for Proc {
    fn process(&mut self, _: &Client, ps: &ProcessScope) -> Control {
        if let Some(p) = &self.midi {
            for ev in p.iter(ps) {
                // CC only: 3 bytes, status 0xBn. Real-time safe.
                if ev.bytes.len() == 3 && ev.bytes[0] & 0xF0 == 0xB0 && self.out.space() >= 3 {
                    self.out.write_buffer(ev.bytes);
                }
            }
        }
        Control::Continue
    }
}

struct Jack {
    client: AsyncClient<Notif, Proc>,
    rx: mpsc::Receiver<JackEvent>,
    midi_rx: RingBufferReader,
}

fn open_jack(name: &str, midi: bool) -> Result<Jack, String> {
    let (client, _status) =
        Client::new(name, ClientOptions::NO_START_SERVER).map_err(|e| format!("{e:?}"))?;
    let port = if midi {
        Some(
            client
                .register_port("midi_in", MidiIn::default())
                .map_err(|e| format!("{e:?}"))?,
        )
    } else {
        None
    };
    let (midi_rx, out) = RingBuffer::new(4096)
        .map_err(|e| format!("{e:?}"))?
        .into_reader_writer();
    let (tx, rx) = mpsc::channel();
    let client = client
        .activate_async(Notif { tx }, Proc { midi: port, out })
        .map_err(|e| format!("{e:?}"))?;
    Ok(Jack {
        client,
        rx,
        midi_rx,
    })
}

// ---------------------------------------------------------------------------
// Main loop

static STOP: AtomicBool = AtomicBool::new(false);

extern "C" fn on_signal(_: libc::c_int) {
    STOP.store(true, Ordering::SeqCst);
}

fn install_signals() {
    // SAFETY: the handler only stores to an atomic.
    unsafe {
        libc::signal(libc::SIGINT, on_signal as *const () as libc::sighandler_t);
        libc::signal(libc::SIGTERM, on_signal as *const () as libc::sighandler_t);
    }
}

const RETRY: Duration = Duration::from_secs(2);

struct Guard {
    follow: bool,
    /// JACK started or changed rate and the Orion has not been compared
    /// since: `--follow-jack` may act.
    pending_follow: bool,
    requested: Option<SampleRate>,
    rate_op: Option<u64>,
    last_msg: Option<String>,
}

impl Guard {
    fn run(&mut self, jack_hz: u32, state: Option<&DeviceState>, ipc: Option<&mut Ipc>) {
        let Some(st) = state else { return };
        let Some(orion) = st.status.sample_rate else {
            return;
        };
        let follow = self.follow && self.pending_follow;
        self.pending_follow = false;
        if self.requested == Some(orion) {
            self.requested = None;
        }
        match check(jack_hz, Some(orion), follow, self.requested) {
            RateAction::Ok => {
                if self.last_msg.take().is_some() {
                    info(&format!("sample rates agree ({jack_hz} Hz)"));
                }
            }
            RateAction::Warn(m) => {
                if self.last_msg.as_deref() != Some(m.as_str()) {
                    warn(&m);
                    self.last_msg = Some(m);
                }
            }
            RateAction::SetOrion(r, m) => {
                warn(&m);
                self.last_msg = Some(m);
                if let Some(ipc) = ipc {
                    match ipc.send_op(Op::SampleRate { rate: r }) {
                        Ok(id) => {
                            self.requested = Some(r);
                            self.rate_op = Some(id);
                        }
                        Err(e) => warn(&format!("cannot ask openoriond for the rate change: {e}")),
                    }
                }
            }
        }
    }
}

/// Run until SIGINT/SIGTERM (or once with `opts.once`).
pub fn run(opts: RunOpts) -> Result<(), String> {
    install_signals();
    let m = port_match(&opts.device_match, &opts.proc_root);
    let mut tracker = Tracker::default();
    let mut namer = Namer::new(opts.aliases);
    let mut guard = Guard {
        follow: opts.follow_jack,
        pending_follow: true,
        requested: None,
        rate_op: None,
        last_msg: None,
    };
    let mut ipc: Option<Ipc> = None;
    let mut jack: Option<Jack> = None;
    let mut ports: Vec<OrionPort> = Vec::new();
    let mut last_ipc_try: Option<Instant> = None;
    let mut last_jack_try: Option<Instant> = None;
    let (mut warned_ipc, mut warned_jack, mut warned_ports) = (false, false, false);
    let mut midi_ops: HashSet<u64> = HashSet::new();
    if opts.follow_jack {
        info("--follow-jack: the Orion's sample rate follows JACK (disruptive)");
    }

    while !STOP.load(Ordering::SeqCst) {
        let now = Instant::now();
        let mut renamed = false;
        let mut recheck_rate = false;

        // (Re)connect openoriond.
        if ipc.is_none() && last_ipc_try.is_none_or(|t| now - t >= RETRY) {
            last_ipc_try = Some(now);
            match Ipc::connect(&opts.socket).and_then(|mut c| c.subscribe(false).map(|_| c)) {
                Ok(c) => {
                    info("connected to openoriond");
                    warned_ipc = false;
                    ipc = Some(c);
                }
                Err(e) => {
                    if !warned_ipc {
                        warn(&format!("{e}; retrying every {}s", RETRY.as_secs()));
                        warned_ipc = true;
                    }
                    if opts.once {
                        return Err(e.to_string());
                    }
                }
            }
        }

        // (Re)connect JACK.
        if jack.is_none() && last_jack_try.is_none_or(|t| now - t >= RETRY) {
            last_jack_try = Some(now);
            match open_jack(&opts.client_name, opts.midi_map.is_some()) {
                Ok(j) => {
                    let c = j.client.as_client();
                    info(&format!(
                        "connected to JACK ({} Hz, {} frames)",
                        c.sample_rate(),
                        c.buffer_size()
                    ));
                    warned_jack = false;
                    warned_ports = false;
                    guard.pending_follow = true;
                    recheck_rate = true;
                    ports.clear();
                    renamed = true;
                    jack = Some(j);
                }
                Err(e) => {
                    if !warned_jack {
                        warn(&format!(
                            "JACK server not reachable ({e}); retrying every {}s",
                            RETRY.as_secs()
                        ));
                        warned_jack = true;
                    }
                    if opts.once {
                        return Err(format!("JACK server not reachable: {e}"));
                    }
                }
            }
        }

        // JACK events.
        let mut lost_jack = None;
        if let Some(j) = &mut jack {
            while let Ok(ev) = j.rx.try_recv() {
                match ev {
                    JackEvent::SampleRate(hz) => {
                        info(&format!("JACK sample rate: {hz} Hz"));
                        guard.pending_follow = true;
                        recheck_rate = true;
                    }
                    JackEvent::Ports => renamed = true,
                    JackEvent::Shutdown(r) => lost_jack = Some(r),
                }
            }
            // MIDI: keep the last value per target, then send.
            if let (Some(map), Some(c)) = (&opts.midi_map, ipc.as_mut()) {
                let mut latest: Vec<(Target, u8)> = Vec::new();
                let mut buf = [0u8; 3];
                while j.midi_rx.space() >= 3 {
                    j.midi_rx.read_buffer(&mut buf);
                    for t in map.lookup(buf[0], buf[1]) {
                        latest.retain(|(x, _)| *x != t);
                        latest.push((t, buf[2]));
                    }
                }
                for (t, v) in latest {
                    match c.send_op(op_for(t, v)) {
                        Ok(id) => {
                            midi_ops.insert(id);
                        }
                        Err(e) => warn(&format!("MIDI op not sent: {e}")),
                    }
                }
            }
        }
        if let Some(r) = lost_jack {
            warn(&format!(
                "JACK shut down ({r}); waiting for it to come back"
            ));
            // The server is gone; dropping the client is all we can do.
            jack = None;
            ports.clear();
            namer = Namer::new(opts.aliases);
            last_jack_try = Some(Instant::now());
        }

        // openoriond messages.
        let mut lost_ipc = false;
        if let Some(c) = &mut ipc {
            let wait = Duration::from_millis(100);
            let mut first = true;
            loop {
                let msg = if first {
                    first = false;
                    c.recv(wait)
                } else {
                    c.try_recv()
                };
                match msg {
                    Ok(Some(msg)) => {
                        let ch = tracker.apply(&msg);
                        renamed |= ch.routing;
                        recheck_rate |= ch.status;
                        match &msg {
                            DaemonMsg::Result { id, ok, error } => {
                                let e = error.clone().unwrap_or_default();
                                if Some(*id) == guard.rate_op {
                                    guard.rate_op = None;
                                    if !ok {
                                        warn(&format!("Orion rate change refused: {e}"));
                                        guard.requested = None;
                                    }
                                } else if midi_ops.remove(id) && !ok {
                                    warn(&format!("MIDI op failed: {e}"));
                                }
                            }
                            DaemonMsg::Health { health, detail } => info(&format!(
                                "openoriond health: {health:?}{}",
                                detail.as_ref().map_or(String::new(), |d| format!(" ({d})"))
                            )),
                            _ => {}
                        }
                    }
                    Ok(None) => break,
                    Err(e) => {
                        warn(&format!(
                            "lost openoriond ({e}); retrying every {}s",
                            RETRY.as_secs()
                        ));
                        lost_ipc = true;
                        break;
                    }
                }
            }
        } else {
            std::thread::sleep(Duration::from_millis(100));
        }
        if lost_ipc {
            ipc = None;
            last_ipc_try = Some(Instant::now());
        }

        // Act.
        if let Some(j) = &jack {
            let c = j.client.as_client();
            if renamed && let Some(st) = &tracker.state {
                ports = discover_ports(c, &m, opts.client_fallback.as_deref());
                namer.retain(&ports);
                if ports.is_empty() {
                    if !warned_ports {
                        warn(&format!(
                            "no JACK ports of the Orion (match \"{}\"); is JACK running \
                             on the Orion? Try --device-match or --client",
                            opts.device_match
                        ));
                        warned_ports = true;
                    }
                } else {
                    warned_ports = false;
                    let n = namer.apply(c, st, &ports);
                    if n > 0 {
                        info(&format!("named {n} of {} Orion port(s)", ports.len()));
                    }
                }
            }
            if recheck_rate {
                guard.run(c.sample_rate(), tracker.state.as_ref(), ipc.as_mut());
            }
            if opts.once && tracker.state.is_some() {
                return Ok(());
            }
        }
    }

    if let Some(j) = jack {
        namer.restore(j.client.as_client());
        info("restored the original port names");
        let _ = j.client.deactivate();
    }
    Ok(())
}
