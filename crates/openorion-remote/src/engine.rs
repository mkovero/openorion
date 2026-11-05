//! The remote's core: MIDI / OSC events in, ops / feedback / relay
//! messages out. No I/O; the shell in `main.rs` feeds it and carries out
//! what it returns, so every rule here is unit-tested.

use std::collections::{HashMap, HashSet};
use std::net::SocketAddr;
use std::time::{Duration, Instant};

use openorion_control::{
    Kind, Mapping, Mappings, MidiConfig, MidiCtl, MidiMsg, Mode, OSC_PREFIX, OscTarget, Param,
    PickupState, PortInfo, RemoteConfig, RemoteMsg, Source, midi_feedback, norm14, port_matches,
    rel7,
};
use openorion_ipc::{DaemonMsg, Op, apply_diff, linked_partner};
use openorion_proto::{AfxMeters, DeviceState, Meters};
use rosc::{OscMessage, OscType};

use crate::grid::GridState;

/// Ops for one parameter go out at most this often (latest wins).
pub const THROTTLE: Duration = Duration::from_millis(33);
/// MIDI feedback is not sent to a control that moved this recently.
pub const ECHO_WINDOW: Duration = Duration::from_millis(150);
/// At most one `Activity` per source per this.
pub const ACTIVITY_EVERY: Duration = Duration::from_millis(100);
/// A value we sent counts as the current one this long (until the
/// device's diff has come back), for relative encoders and toggles.
pub const IN_FLIGHT: Duration = Duration::from_millis(400);
/// The virtual port's name; also the device name of its events.
pub const VIRTUAL_DEVICE: &str = "openorion";
/// OSC: send every value to the targets.
pub const OSC_SYNC: &str = "/orion/sync";
/// OSC: meter prefix (`/orion/meters/<bank> f…`).
pub const OSC_METERS: &str = "/orion/meters/";
/// Meter banks sent, in order.
pub const METER_BANKS: &[&str] = &[
    "bank_a",
    "bank_b",
    "mixer",
    "reverb_in",
    "reverb_out",
    "afx",
];

/// An OSC target, resolved.
#[derive(Debug, Clone)]
pub struct Target {
    pub addr: SocketAddr,
    pub cfg: OscTarget,
}

/// What the shell must do.
#[derive(Debug, Clone, PartialEq)]
pub enum Out {
    /// Send to the daemon.
    Op(Op),
    /// Relay to the UI.
    Remote(RemoteMsg),
    /// `reply` = an answer to a sender (send from the listen socket).
    Osc {
        target: SocketAddr,
        msg: OscMessage,
        reply: bool,
    },
    /// Feedback for the controls of input `device` (`*` = every paired
    /// output, `VIRTUAL_DEVICE` = the virtual port).
    Midi { device: String, bytes: Vec<u8> },
    /// Write the config file.
    Save(RemoteConfig),
    /// Ports to open changed.
    MidiPorts(MidiConfig),
}

struct Learn {
    /// Mappings and MIDI settings at `LearnStart`, for `LearnStop{save:false}`.
    mappings: Mappings,
    midi: MidiConfig,
    selected: Option<Param>,
}

/// Which parameters a state change can touch.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Group {
    All,
    Nothing,
    Status,
    /// Mixer index; channel 0 = master, 1..=32, None = whole mixer.
    Mixer(u8, Option<u8>),
    MixerLinks,
    Trims,
    Reverb,
    Afx,
}

impl Group {
    fn of(path: &str) -> Group {
        let p: Vec<&str> = path.split('/').skip(1).collect();
        match p.as_slice() {
            [] | [""] => Group::All,
            ["status", ..] => Group::Status,
            ["mixers", m, rest @ ..] => {
                let Ok(m) = m.parse::<u8>() else {
                    return Group::All;
                };
                match rest {
                    ["channels", c, ..] => match c.parse::<u8>() {
                        Ok(c) => Group::Mixer(m, Some(c + 1)),
                        Err(_) => Group::Mixer(m, None),
                    },
                    ["master", ..] => Group::Mixer(m, Some(0)),
                    _ => Group::Mixer(m, None),
                }
            }
            ["mixer_links", ..] => Group::MixerLinks,
            ["trims", ..] => Group::Trims,
            ["reverb", ..] => Group::Reverb,
            ["afx", ..] => Group::Afx,
            ["info" | "routing" | "hdx_delays", ..] => Group::Nothing,
            _ => Group::All,
        }
    }

    fn covers(self, p: &Param) -> bool {
        match self {
            Group::All => true,
            Group::Nothing => false,
            Group::Status => matches!(p, Param::Volume(_) | Param::Mute(_)),
            Group::Mixer(m, ch) => match *p {
                Param::Mixer { mixer, ch: c, .. } => mixer.get() == m && ch.is_none_or(|x| x == c),
                Param::Master { mixer, .. } => mixer.get() == m && ch.is_none_or(|x| x == 0),
                _ => false,
            },
            Group::MixerLinks => matches!(p, Param::MixerLink { .. }),
            Group::Trims => matches!(p, Param::Trim { .. }),
            Group::Reverb => matches!(p, Param::Reverb { .. }),
            Group::Afx => matches!(
                p,
                Param::Eq { .. }
                    | Param::Comp { .. }
                    | Param::AfxRaw { .. }
                    | Param::Bypass { .. }
                    | Param::AfxLink { .. }
            ),
        }
    }
}

pub struct Engine {
    cfg: RemoteConfig,
    mappings: Mappings,
    listen: Option<String>,
    targets: Vec<Target>,
    meter_due: Vec<Option<Instant>>,
    state: Option<DeviceState>,
    learn: Option<Learn>,
    learn_filter: Option<String>,
    inputs: Vec<String>,
    outputs: Vec<String>,
    pickup: HashMap<Source, PickupState>,
    /// Last input per mapping source (echo suppression, pickup).
    last_input: HashMap<Source, Instant>,
    activity: HashMap<Source, Instant>,
    /// Last 14-bit MSB per (device, channel, MSB controller).
    cc14_msb: HashMap<(String, u8, u8), u8>,
    /// Throttled values waiting for their slot.
    held: HashMap<Param, f32>,
    last_op: HashMap<Param, (Instant, f32)>,
    /// Last value seen per watched parameter.
    cache: HashMap<Param, f32>,
    /// Every parameter the state holds (only kept when a target wants values).
    avail: Vec<Param>,
    watch: Vec<Param>,
    /// LED meter grids per bound MIDI device.
    grids: HashMap<String, GridState>,
}

/// A numeric OSC argument as f32.
fn osc_num(a: &OscType) -> Option<f32> {
    match *a {
        OscType::Float(f) => Some(f),
        OscType::Double(d) => Some(d as f32),
        OscType::Int(i) => Some(i as f32),
        OscType::Long(l) => Some(l as f32),
        OscType::Bool(b) => Some(if b { 1.0 } else { 0.0 }),
        _ => None,
    }
    .filter(|v| v.is_finite())
}

fn elapsed(now: Instant, t: Instant) -> Duration {
    now.saturating_duration_since(t)
}

impl Engine {
    /// `listen` is the bound OSC address (None = OSC off), `targets` the
    /// resolved `cfg.osc.targets`.
    pub fn new(cfg: RemoteConfig, listen: Option<String>, targets: Vec<Target>) -> Engine {
        let mappings = Mappings(cfg.mappings.clone());
        let n = targets.len();
        let mut e = Engine {
            cfg,
            mappings,
            listen,
            targets,
            meter_due: vec![None; n],
            state: None,
            learn: None,
            learn_filter: None,
            inputs: Vec::new(),
            outputs: Vec::new(),
            pickup: HashMap::new(),
            last_input: HashMap::new(),
            activity: HashMap::new(),
            cc14_msb: HashMap::new(),
            held: HashMap::new(),
            last_op: HashMap::new(),
            cache: HashMap::new(),
            avail: Vec::new(),
            watch: Vec::new(),
            grids: HashMap::new(),
        };
        e.rewatch();
        e
    }

    /// Whether a target wants meters (subscribe with meters).
    pub fn wants_meters(&self) -> bool {
        self.targets.iter().any(|t| t.cfg.meters) || !self.grids.is_empty()
    }

    fn values_out(&self) -> bool {
        self.targets.iter().any(|t| t.cfg.values)
    }

    #[cfg(test)]
    pub fn learning(&self) -> bool {
        self.learn.is_some()
    }

    #[cfg(test)]
    pub fn mappings(&self) -> &Mappings {
        &self.mappings
    }

    #[cfg(test)]
    pub fn state(&self) -> Option<&DeviceState> {
        self.state.as_ref()
    }

    /// The config as it stands (mappings included).
    pub fn config(&self) -> RemoteConfig {
        RemoteConfig {
            mappings: self.mappings.0.clone(),
            ..self.cfg.clone()
        }
    }

    pub fn announce(&self) -> RemoteMsg {
        RemoteMsg::Announce {
            listen: self.listen.clone(),
            midi_inputs: self
                .inputs
                .iter()
                .map(|n| PortInfo {
                    name: n.clone(),
                    enabled: self.cfg.midi.input_active(n),
                })
                .collect(),
            midi_outputs: self
                .outputs
                .iter()
                .map(|n| PortInfo {
                    name: n.clone(),
                    enabled: self.cfg.midi.output_active(n),
                })
                .collect(),
            osc_targets: self.targets.iter().map(|t| t.cfg.clone()).collect(),
            mappings: self.mappings.0.clone(),
            learning: self.learn.is_some(),
        }
    }

    // -----------------------------------------------------------------------
    // Connection and daemon messages

    /// Connected to the daemon (after hello).
    pub fn connected(&mut self) -> Vec<Out> {
        vec![Out::Remote(self.announce())]
    }

    /// Lost the daemon: forget the state; pending ops are dropped.
    pub fn disconnected(&mut self) {
        self.state = None;
        self.held.clear();
        self.last_op.clear();
        self.cache.clear();
        self.avail.clear();
        self.rewatch();
    }

    pub fn daemon(&mut self, msg: DaemonMsg, now: Instant) -> Vec<Out> {
        let mut out = Vec::new();
        match msg {
            DaemonMsg::State { state } => {
                let first = self.cache.is_empty();
                self.avail = if self.values_out() {
                    Param::available(&state)
                } else {
                    Vec::new()
                };
                self.state = Some(*state);
                self.rewatch();
                self.changed(Group::All, !first, now, &mut out);
            }
            DaemonMsg::Diff { path, value } => {
                let Some(s) = &mut self.state else {
                    return out;
                };
                if let Err(e) = apply_diff(s, &path, &value) {
                    tracing::warn!(path, "diff: {e}");
                    return out;
                }
                self.changed(Group::of(&path), true, now, &mut out);
            }
            // Status from the device itself (front-panel knobs, meter
            // sources): not sent as a diff.
            DaemonMsg::Status { status } => {
                let Some(s) = &mut self.state else {
                    return out;
                };
                s.status = *status;
                self.changed(Group::Status, true, now, &mut out);
            }
            DaemonMsg::Meters { meters, afx } => self.meters(&meters, &afx, now, &mut out),
            DaemonMsg::Remote { msg } => match serde_json::from_value::<RemoteMsg>(msg) {
                Ok(m) => return self.remote(m),
                Err(e) => tracing::debug!("remote message: {e}"),
            },
            _ => {}
        }
        out
    }

    /// The MIDI ports seen now (ALSA client names).
    pub fn ports(&mut self, inputs: Vec<String>, outputs: Vec<String>) -> Vec<Out> {
        if inputs == self.inputs && outputs == self.outputs {
            return Vec::new();
        }
        self.inputs = inputs;
        self.outputs = outputs;
        vec![Out::Remote(self.announce())]
    }

    // -----------------------------------------------------------------------
    // UI messages

    pub fn remote(&mut self, msg: RemoteMsg) -> Vec<Out> {
        let mut out = Vec::new();
        match msg {
            RemoteMsg::Query => {}
            RemoteMsg::LearnStart => {
                if self.learn.is_none() {
                    self.learn = Some(Learn {
                        mappings: self.mappings.clone(),
                        midi: self.cfg.midi.clone(),
                        selected: None,
                    });
                    self.held.clear();
                }
            }
            RemoteMsg::LearnSelect { param } => {
                if let Some(l) = &mut self.learn {
                    l.selected = param;
                }
                return out;
            }
            RemoteMsg::LearnStop { save } => {
                let Some(l) = self.learn.take() else {
                    out.push(Out::Remote(self.announce()));
                    return out;
                };
                if save {
                    out.push(Out::Save(self.config()));
                    self.rewatch();
                } else {
                    let before = std::mem::replace(&mut self.mappings, l.mappings);
                    if self.cfg.midi != l.midi {
                        self.cfg.midi = l.midi;
                        out.push(Out::MidiPorts(self.cfg.midi.clone()));
                    }
                    self.pickup.clear();
                    self.rewatch();
                    self.relight(&before, &mut out);
                }
            }
            RemoteMsg::Unbind { param, source } => {
                let before = self.mappings.clone();
                let mut n = 0;
                if let Some(p) = param {
                    n += self.mappings.unbind_param(&p);
                }
                if let Some(s) = source
                    && self.mappings.unbind_source(&s).is_some()
                {
                    self.pickup.remove(&s);
                    n += 1;
                }
                if n == 0 {
                    return out;
                }
                self.rewatch();
                self.relight(&before, &mut out);
                self.persist(&mut out);
            }
            RemoteMsg::SetOpts { source, opts } => {
                if !self.mappings.set_opts(&source, opts) {
                    return out;
                }
                self.pickup.remove(&source);
                self.persist(&mut out);
            }
            RemoteMsg::SetDevices { input, output } => {
                let before = self.cfg.midi.clone();
                if let Some((name, on)) = input {
                    self.cfg.midi.set_input_known(&name, on, &self.inputs);
                }
                if let Some((name, on)) = output {
                    self.cfg.midi.set_output(&name, on);
                }
                if self.cfg.midi == before {
                    return out;
                }
                out.push(Out::MidiPorts(self.cfg.midi.clone()));
                self.persist(&mut out);
            }
            RemoteMsg::LearnFilter { device } => {
                self.learn_filter = device.filter(|d| !d.is_empty());
                return out;
            }
            RemoteMsg::Announce { .. } | RemoteMsg::Learned { .. } | RemoteMsg::Activity { .. } => {
                return out;
            }
        }
        out.push(Out::Remote(self.announce()));
        out
    }

    /// Outside a learn session, edits are written at once.
    fn persist(&self, out: &mut Vec<Out>) {
        if self.learn.is_none() {
            out.push(Out::Save(self.config()));
        }
    }

    // -----------------------------------------------------------------------
    // Controller input

    /// MIDI `device` was (re)opened: show it every mapped value, so LEDs
    /// and rings start out right. Wildcard mappings answer on every device.
    pub fn midi_ready(&mut self, device: &str) -> Vec<Out> {
        let mut out = Vec::new();
        if let Some(g) = self.grids.get_mut(device) {
            g.forget();
        }
        for m in self.mappings.iter() {
            let Some(d) = m.source.device() else { continue };
            if d != device && d != openorion_control::ANY_DEVICE {
                continue;
            }
            if let Some(v) = self.cache.get(&m.param)
                && let Some(bytes) = m.midi_feedback(*v)
            {
                out.push(Out::Midi {
                    device: device.to_owned(),
                    bytes,
                });
            }
        }
        out
    }

    /// A button with no binding: many controllers light it locally when
    /// pressed, so answer with LED off. Knobs are left alone (their ring
    /// shows the encoder's own position).
    fn dark_if_unbound(&mut self, device: &str, msg: &MidiMsg, ev: &Source, out: &mut Vec<Out>) {
        if let MidiCtl::Note(n) = msg.ctl()
            && self.mappings.lookup(ev).is_none()
        {
            // A meter pad: the next frame puts its LED back.
            if let Some(g) = self.grids.get_mut(device) {
                g.forget_note(msg.ch(), n);
            }
            out.push(Out::Midi {
                device: device.to_owned(),
                bytes: midi_feedback(msg.ch(), msg.ctl(), 0.0),
            });
        }
    }

    /// After the bindings changed from `before`: controls that lost their
    /// binding go dark, newly bound ones show their parameter's value.
    fn relight(&self, before: &Mappings, out: &mut Vec<Out>) {
        for old in before.iter() {
            if let Source::Midi { device, ch, ctl } = &old.source
                && self.mappings.get(&old.source).is_none()
            {
                out.push(Out::Midi {
                    device: device.clone(),
                    bytes: midi_feedback(ch.unwrap_or(0), *ctl, 0.0),
                });
            }
        }
        for m in self.mappings.iter() {
            if before.get(&m.source).is_some_and(|o| o.param == m.param) {
                continue;
            }
            if let (Some(device), Some(v)) = (m.source.device(), self.cache.get(&m.param))
                && let Some(bytes) = m.midi_feedback(*v)
            {
                out.push(Out::Midi {
                    device: device.to_owned(),
                    bytes,
                });
            }
        }
    }

    pub fn midi_in(&mut self, device: &str, bytes: &[u8], now: Instant) -> Vec<Out> {
        let mut out = Vec::new();
        let Some(msg) = MidiMsg::parse(bytes) else {
            return out;
        };
        let ev = msg.source(device);
        self.activity(&ev, now, &mut out);
        if self.learn.is_some() {
            self.learn_midi(device, &msg, ev.clone(), &mut out);
            self.dark_if_unbound(device, &msg, &ev, &mut out);
            return out;
        }
        let Some(m) = self.mappings.lookup(&ev).cloned() else {
            self.dark_if_unbound(device, &msg, &ev, &mut out);
            return out;
        };
        self.last_input.insert(m.source.clone(), now);
        let ctl = match m.source {
            Source::Midi { ctl, .. } => Some(ctl),
            Source::Osc { .. } => None,
        };
        let raw = match (m.opts.mode, msg, ctl) {
            (Mode::Relative, MidiMsg::Cc { value, .. }, _) => rel7(value) as f32 * m.param.tick(),
            (Mode::Relative, _, _) => return out,
            (_, MidiMsg::Cc { ch, num, value }, Some(MidiCtl::Cc14(n))) => {
                let key = (device.to_owned(), ch, n);
                if num == n {
                    self.cc14_msb.insert(key, value);
                    norm14(value, 0)
                } else {
                    let Some(&msb) = self.cc14_msb.get(&key) else {
                        return out;
                    };
                    norm14(msb, value)
                }
            }
            _ => msg.norm(),
        };
        self.drive(&m, raw, now, &mut out);
        out
    }

    fn learn_midi(&mut self, device: &str, msg: &MidiMsg, ev: Source, out: &mut Vec<Out>) {
        let Some(param) = self.learn.as_ref().and_then(|l| l.selected) else {
            return;
        };
        if self
            .learn_filter
            .as_ref()
            .is_some_and(|f| !port_matches(f, device))
        {
            return;
        }
        // The second half of a 14-bit controller whose MSB was just bound
        // to this parameter: make it a 14-bit binding.
        if let MidiMsg::Cc { ch, num, .. } = *msg
            && (32..64).contains(&num)
        {
            let msb = Source::Midi {
                device: device.to_owned(),
                ch: Some(ch),
                ctl: MidiCtl::Cc(num - 32),
            };
            if self.mappings.get(&msb).is_some_and(|m| m.param == param) {
                self.mappings.unbind_source(&msb);
                let c14 = Source::Midi {
                    device: device.to_owned(),
                    ch: Some(ch),
                    ctl: MidiCtl::Cc14(num - 32),
                };
                self.bind(c14, param, out);
                return;
            }
        }
        self.bind(ev, param, out);
    }

    fn bind(&mut self, source: Source, param: Param, out: &mut Vec<Out>) {
        if self
            .mappings
            .lookup(&source)
            .is_some_and(|m| m.param == param)
        {
            return;
        }
        let before = self.mappings.clone();
        self.mappings.bind(source.clone(), param);
        self.pickup.remove(&source);
        self.rewatch();
        self.relight(&before, out);
        out.push(Out::Remote(RemoteMsg::Learned { param, source }));
        out.push(Out::Remote(self.announce()));
    }

    pub fn osc_in(&mut self, from: SocketAddr, msg: OscMessage, now: Instant) -> Vec<Out> {
        let mut out = Vec::new();
        let addr = msg.addr;
        let nums: Vec<Option<f32>> = msg.args.iter().map(osc_num).collect();
        if nums.is_empty() {
            if addr == OSC_SYNC {
                self.sync(&mut out);
            } else if let (Some(p), Some(s)) = (Param::from_osc(&addr), &self.state)
                && let Some(v) = p.read(s)
            {
                out.push(Out::Osc {
                    target: from,
                    msg: value_msg(&p, v),
                    reply: true,
                });
            }
            return out;
        }
        let host = from.ip().to_string();
        let ev = |arg| Source::Osc {
            from: Some(host.clone()),
            addr: addr.clone(),
            arg,
        };
        let Some(first) = nums.iter().position(Option::is_some) else {
            return out;
        };
        self.activity(&ev(first), now, &mut out);
        if self.learn.is_some() {
            let Some(param) = self.learn.as_ref().and_then(|l| l.selected) else {
                return out;
            };
            if self
                .learn_filter
                .as_ref()
                .is_some_and(|f| !port_matches(f, &host))
            {
                return out;
            }
            let src = Source::Osc {
                from: None,
                addr: addr.clone(),
                arg: first,
            };
            self.bind(src, param, &mut out);
            return out;
        }
        if addr.starts_with(OSC_PREFIX) && !addr.starts_with(OSC_METERS) {
            if let Some(p) = Param::from_osc(&addr)
                && let Some(v) = nums[first]
                && p.kind() != Kind::Trigger
            {
                self.push(p, v.clamp(0.0, 1.0), now, &mut out);
            }
            return out;
        }
        for (i, v) in nums.iter().enumerate() {
            let Some(v) = *v else { continue };
            let Some(m) = self.mappings.lookup(&ev(i)).cloned() else {
                continue;
            };
            self.last_input.insert(m.source.clone(), now);
            self.drive(&m, v, now, &mut out);
        }
        out
    }

    fn activity(&mut self, ev: &Source, now: Instant, out: &mut Vec<Out>) {
        if self
            .activity
            .get(ev)
            .is_some_and(|t| elapsed(now, *t) < ACTIVITY_EVERY)
        {
            return;
        }
        if self.activity.len() > 4096 {
            self.activity
                .retain(|_, t| elapsed(now, *t) < Duration::from_secs(10));
        }
        self.activity.insert(ev.clone(), now);
        out.push(Out::Remote(RemoteMsg::Activity { source: ev.clone() }));
    }

    // -----------------------------------------------------------------------
    // Ops

    /// The parameter's value as we know it: a held or just-sent value
    /// before the device's.
    fn current(&self, p: &Param, now: Instant) -> Option<f32> {
        if let Some(v) = self.held.get(p) {
            return Some(*v);
        }
        if let Some((t, v)) = self.last_op.get(p)
            && elapsed(now, *t) < IN_FLIGHT
        {
            return Some(*v);
        }
        self.state.as_ref().and_then(|s| p.read(s))
    }

    fn drive(&mut self, m: &Mapping, raw: f32, now: Instant, out: &mut Vec<Out>) {
        let current = self.current(&m.param, now);
        let st = self
            .pickup
            .entry(m.source.clone())
            .or_insert_with(|| PickupState::for_param(&m.param));
        if let Some(v) = m.opts.apply(raw, current, st) {
            self.push(m.param, v, now, out);
        }
    }

    /// Latest-wins throttle per parameter.
    fn push(&mut self, p: Param, norm: f32, now: Instant, out: &mut Vec<Out>) {
        if let Some(h) = self.held.get_mut(&p) {
            *h = norm;
            return;
        }
        match self.last_op.get(&p) {
            Some((t, _)) if elapsed(now, *t) < THROTTLE => {
                self.held.insert(p, norm);
            }
            _ => self.send(p, norm, now, out),
        }
    }

    fn send(&mut self, p: Param, norm: f32, now: Instant, out: &mut Vec<Out>) {
        let Some(state) = &self.state else {
            return;
        };
        let Some(op) = p.to_op(norm, state) else {
            return;
        };
        let partner = linked_partner(&op, state);
        out.push(Out::Op(op));
        if let Some(op) = partner {
            out.push(Out::Op(op));
        }
        self.last_op.insert(p, (now, norm));
    }

    /// Send held values whose slot has come.
    pub fn tick(&mut self, now: Instant) -> Vec<Out> {
        let mut out = Vec::new();
        let due: Vec<Param> = self
            .held
            .keys()
            .filter(|p| {
                self.last_op
                    .get(p)
                    .is_none_or(|(t, _)| elapsed(now, *t) >= THROTTLE)
            })
            .copied()
            .collect();
        for p in due {
            if let Some(v) = self.held.remove(&p) {
                self.send(p, v, now, &mut out);
            }
        }
        out
    }

    /// When `tick` has work next.
    pub fn next_deadline(&self) -> Option<Instant> {
        self.held
            .keys()
            .filter_map(|p| self.last_op.get(p).map(|(t, _)| *t + THROTTLE))
            .min()
    }

    // -----------------------------------------------------------------------
    // Feedback

    /// Recompute the watched parameters: the mapped ones, plus every
    /// available one when a target wants values.
    fn rewatch(&mut self) {
        let mut set: HashSet<Param> = self.mappings.iter().map(|m| m.param).collect();
        set.extend(self.avail.iter().copied());
        let mut w: Vec<Param> = set.into_iter().collect();
        w.sort();
        // New entries start from the current value, without feedback.
        if let Some(s) = &self.state {
            for p in &w {
                if !self.cache.contains_key(p)
                    && let Some(v) = p.read(s)
                {
                    self.cache.insert(*p, v);
                }
            }
        }
        self.cache.retain(|p, _| w.binary_search(p).is_ok());
        self.watch = w;
        self.regrid();
    }

    /// One meter grid per MIDI device a mapping names, when the device has
    /// a grid (config or built in). Existing grids keep their LED state.
    fn regrid(&mut self) {
        let mut devices: Vec<String> = self
            .mappings
            .iter()
            .filter_map(|m| m.source.device())
            .filter(|d| *d != openorion_control::ANY_DEVICE && *d != VIRTUAL_DEVICE)
            .map(str::to_owned)
            .collect();
        devices.sort();
        devices.dedup();
        let mut grids = HashMap::new();
        for d in devices {
            let Some(grid) = self.cfg.midi.grid_for(&d) else {
                continue;
            };
            let g = match self.grids.remove(&d) {
                Some(g) if g.grid == grid => g,
                _ => GridState::new(grid),
            };
            grids.insert(d, g);
        }
        self.grids = grids;
    }

    /// Feedback for watched parameters in `g` whose value changed. With
    /// `osc` false (the first state), only MIDI feedback goes out; OSC
    /// targets ask with `/orion/sync`.
    fn changed(&mut self, g: Group, osc: bool, now: Instant, out: &mut Vec<Out>) {
        let Some(s) = &self.state else { return };
        let mut changed = Vec::new();
        for p in self.watch.iter().filter(|p| g.covers(p)) {
            match p.read(s) {
                None => {
                    self.cache.remove(p);
                }
                Some(v) => {
                    let old = self.cache.insert(*p, v);
                    if old.is_none_or(|o| (o - v).abs() > 1e-6) {
                        changed.push((*p, v));
                    }
                }
            }
        }
        // `rewatch` filled the cache for the first state: everything mapped
        // gets its MIDI feedback once.
        if !osc {
            changed = self
                .mappings
                .iter()
                .filter_map(|m| Some((m.param, *self.cache.get(&m.param)?)))
                .collect();
            changed.sort_by_key(|c| c.0);
            changed.dedup_by(|a, b| a.0 == b.0);
        }
        for (p, v) in changed {
            self.feedback(p, v, osc, now, out);
        }
    }

    fn feedback(&mut self, p: Param, v: f32, osc: bool, now: Instant, out: &mut Vec<Out>) {
        if osc {
            for t in self.targets.iter().filter(|t| t.cfg.values) {
                out.push(Out::Osc {
                    target: t.addr,
                    msg: value_msg(&p, v),
                    reply: false,
                });
            }
        }
        let maps: Vec<Mapping> = self.mappings.for_param(&p).cloned().collect();
        for m in maps {
            let quiet = self
                .last_input
                .get(&m.source)
                .is_none_or(|t| elapsed(now, *t) >= ECHO_WINDOW);
            self.pickup
                .entry(m.source.clone())
                .or_insert_with(|| PickupState::for_param(&p))
                .observe(v, quiet);
            // Only an absolute control already shows where the hand left
            // it; a button LED or an encoder ring needs the new state even
            // right after its own input.
            if !quiet && m.opts.mode == Mode::Absolute {
                continue;
            }
            if let (Some(bytes), Some(device)) = (m.midi_feedback(v), m.source.device()) {
                out.push(Out::Midi {
                    device: device.to_owned(),
                    bytes,
                });
            }
        }
    }

    /// `/orion/sync`: every value to every values target.
    fn sync(&mut self, out: &mut Vec<Out>) {
        let Some(s) = &self.state else { return };
        let all = if self.avail.is_empty() {
            Param::available(s)
        } else {
            self.avail.clone()
        };
        for p in all {
            let Some(v) = p.read(s) else { continue };
            for t in self.targets.iter().filter(|t| t.cfg.values) {
                out.push(Out::Osc {
                    target: t.addr,
                    msg: value_msg(&p, v),
                    reply: false,
                });
            }
        }
    }

    // -----------------------------------------------------------------------
    // Meters

    fn meters(&mut self, m: &Meters, afx: &AfxMeters, now: Instant, out: &mut Vec<Out>) {
        if let Some(state) = &self.state {
            for (device, g) in self.grids.iter_mut() {
                for bytes in g.frame(device, &self.mappings, state, m, afx, now) {
                    out.push(Out::Midi {
                        device: device.clone(),
                        bytes,
                    });
                }
            }
        }
        let mut msgs: Option<Vec<OscMessage>> = None;
        for (i, t) in self.targets.iter().enumerate() {
            if !t.cfg.meters || t.cfg.meter_hz == 0 {
                continue;
            }
            let period = Duration::from_secs_f64(1.0 / t.cfg.meter_hz as f64);
            // A little slack so a rate equal to the daemon's is not halved
            // by jitter.
            let slack = Duration::from_millis(2);
            let due = self.meter_due[i];
            if due.is_some_and(|d| now + slack < d) {
                continue;
            }
            self.meter_due[i] = Some(match due {
                Some(d) if now < d + period => d + period,
                _ => now + period,
            });
            let msgs = msgs.get_or_insert_with(|| meter_msgs(m, afx));
            for msg in msgs.iter() {
                out.push(Out::Osc {
                    target: t.addr,
                    msg: msg.clone(),
                    reply: false,
                });
            }
        }
    }
}

/// `/orion/<path> f`.
pub fn value_msg(p: &Param, v: f32) -> OscMessage {
    OscMessage {
        addr: p.osc_addr(),
        args: vec![OscType::Float(v)],
    }
}

/// Raw meter value (dB below full scale, 96 = floor) → dBFS.
pub fn dbfs(raw: u8) -> f32 {
    -(raw.min(openorion_proto::METER_FLOOR) as f32)
}

/// One message per bank in `METER_BANKS`; `afx` is the AFX peaks,
/// chain-major.
pub fn meter_msgs(m: &Meters, afx: &AfxMeters) -> Vec<OscMessage> {
    let bank = |name: &str, v: &mut dyn Iterator<Item = u8>| OscMessage {
        addr: format!("{OSC_METERS}{name}"),
        args: v.map(|r| OscType::Float(dbfs(r))).collect(),
    };
    vec![
        bank("bank_a", &mut m.bank_a.iter().copied()),
        bank("bank_b", &mut m.bank_b.iter().copied()),
        bank("mixer", &mut m.mixer.iter().copied()),
        bank("reverb_in", &mut m.reverb_in.iter().copied()),
        bank("reverb_out", &mut m.reverb_out.iter().copied()),
        bank("afx", &mut afx.peak.iter().flatten().copied()),
    ]
}

#[cfg(test)]
mod tests;
