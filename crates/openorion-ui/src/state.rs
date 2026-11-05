//! Data side of the UI: device state, meters, the op log and the send
//! queue.
//!
//! Widgets never mutate `device` directly. They call `AppState::submit`
//! with an `openorion_ipc::Op`; disruptive ops wait for confirmation first.
//! A committed op is mirrored into the local copy (`apply_local`,
//! optimistic), logged in `outbox` and queued for openoriond in `sendq`
//! (continuous controls throttled to ~30 Hz per control, see `Throttle`).
//! The daemon's `State`/`Status`/`Diff`/`Mismatch` are authoritative and
//! overwrite the local copy (`apply_daemon`); ops still held back by the
//! throttle are re-applied on top so a drag does not jump back.

use std::collections::HashMap;

use openorion_ipc::client::DaemonInfo;
use openorion_ipc::{DaemonMsg, Health, Op, apply_diff, linked_partner};
use openorion_proto::*;

use crate::bridge::{Incoming, Outgoing};
use crate::learn::{LearnUi, RemoteView};
use crate::meters::{MeterHolds, MeterSim};

/// Minimum spacing of sends per continuous control (s): ~30 Hz.
pub const SEND_INTERVAL: f64 = 1.0 / 30.0;
/// Frame-time jitter tolerated by the throttle (s), so a 60 Hz UI really
/// sends every second frame.
const SLACK: f64 = 0.002;
/// Clock change: how long to wait for LOCK before asking keep / revert (s).
pub const LOCK_WAIT: f64 = 10.0;
/// Keep / revert countdown; reverts when it runs out (s).
pub const REVERT_COUNTDOWN: f64 = 10.0;
/// Meters older than this fall back to the floor (s).
const METER_STALE: f64 = 0.5;
/// Log entries kept for the outbox panel.
const LOG_MAX: usize = 300;

/// One op the UI committed (outbox / send log).
#[derive(Debug, Clone, PartialEq)]
pub struct OutboxEntry {
    pub seq: u64,
    pub op: Op,
    pub risk: Risk,
    /// How many edits were merged into this entry (slider drags).
    pub merged: u32,
}

/// Where the state comes from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Conn {
    /// `--demo` / `--state FILE`: nothing is sent anywhere.
    Offline,
    /// Live mode, first connect attempt running.
    Connecting,
    /// Live mode, openoriond not reachable (retrying every 2 s).
    Down(String),
    Connected(DaemonInfo),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToastKind {
    Info,
    Warn,
    Error,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Toast {
    pub text: String,
    pub kind: ToastKind,
    pub until: f64,
}

/// A clock-source change waiting for LOCK.
#[derive(Debug, Clone, PartialEq)]
pub struct ClockWatch {
    pub prev: Option<ClockSource>,
    pub target: ClockSource,
    pub started: f64,
    /// Set once LOCK_WAIT passed without lock: the keep / revert dialog is
    /// up and reverts at this time.
    pub revert_at: Option<f64>,
}

/// Snapshot restore progress (Snapshots tab).
#[derive(Debug, Clone, PartialEq, Default)]
pub enum RestoreState {
    #[default]
    Idle,
    /// Dry run or apply sent, waiting for the plan.
    Waiting {
        dry_run: bool,
    },
    Plan {
        ops: Vec<Op>,
        applied: bool,
    },
}

/// Per-control send throttle. A keyed op is sent at once if its key was
/// not sent within `interval`, else held (latest wins) until it is due.
/// An unkeyed op flushes everything held first, so order is kept.
#[derive(Debug, Default)]
pub struct Throttle {
    pub interval: f64,
    last: HashMap<String, f64>,
    held: Vec<(String, Op)>,
}

impl Throttle {
    pub fn new(interval: f64) -> Self {
        Throttle {
            interval,
            ..Default::default()
        }
    }

    /// Ops to send now (possibly none).
    pub fn push(&mut self, op: Op, now: f64) -> Vec<Op> {
        let Some(key) = coalesce_key(&op) else {
            let mut out = self.flush();
            out.push(op);
            return out;
        };
        if let Some(slot) = self.held.iter_mut().find(|(k, _)| *k == key) {
            slot.1 = op;
            return Vec::new();
        }
        match self.last.get(&key) {
            Some(&t) if now - t < self.interval - SLACK => {
                self.held.push((key, op));
                Vec::new()
            }
            _ => {
                self.last.insert(key, now);
                vec![op]
            }
        }
    }

    /// Held ops whose interval has passed.
    pub fn due(&mut self, now: f64) -> Vec<Op> {
        let mut out = Vec::new();
        let mut i = 0;
        while i < self.held.len() {
            let k = &self.held[i].0;
            if self
                .last
                .get(k)
                .is_none_or(|&t| now - t >= self.interval - SLACK)
            {
                let (k, op) = self.held.remove(i);
                self.last.insert(k, now);
                out.push(op);
            } else {
                i += 1;
            }
        }
        out
    }

    /// Everything held, regardless of timing.
    pub fn flush(&mut self) -> Vec<Op> {
        self.held.drain(..).map(|(_, op)| op).collect()
    }

    pub fn held(&self) -> impl Iterator<Item = &Op> {
        self.held.iter().map(|(_, op)| op)
    }
}

pub struct AppState {
    pub device: DeviceState,
    /// The daemon's view without our optimistic edits; `device` falls back
    /// to it when the daemon rejects an op.
    confirmed: DeviceState,
    /// Where `device` came from ("demo", a file path or the socket).
    pub origin: String,
    pub conn: Conn,
    /// Live mode: a full `State` arrived on this connection.
    pub synced: bool,
    pub meters: Meters,
    pub afx_meters: AfxMeters,
    pub holds: MeterHolds,
    /// Synthetic meters (offline mode only).
    pub fake_meters: bool,
    sim: MeterSim,
    last_meters: f64,
    /// Log of committed ops (outbox panel).
    pub outbox: Vec<OutboxEntry>,
    next_seq: u64,
    /// Ready to send; the app drains it into the bridge.
    pub sendq: Vec<Outgoing>,
    throttle: Throttle,
    /// Ops waiting for the confirm dialog (a batch with at least one
    /// disruptive op). Empty = no dialog.
    pub pending: Vec<Op>,
    pub health: Health,
    pub health_detail: Option<String>,
    /// openorion-jack status line; None = not running / unknown.
    pub jack: Option<String>,
    pub toasts: Vec<Toast>,
    pub clock_watch: Option<ClockWatch>,
    pub restore: RestoreState,
    /// Mixers tab: point the device's mixer meter bank at the visible
    /// mixer. This writes `MeterSource` (a silent op) when the tab or the
    /// selected mixer changes; off = never write on navigation.
    pub meters_follow_tab: bool,
    /// openorion-remote as last announced; None = not seen (not running).
    pub remote: Option<RemoteView>,
    /// Learn mode: writes are dropped, clicks select a control to bind.
    pub learn: Option<LearnUi>,
    now: f64,
}

impl AppState {
    /// Offline state (demo or file): nothing is sent.
    pub fn new(device: DeviceState, origin: impl Into<String>) -> Self {
        AppState {
            confirmed: device.clone(),
            device,
            origin: origin.into(),
            conn: Conn::Offline,
            synced: true,
            meters: Meters::default(),
            afx_meters: AfxMeters::default(),
            holds: MeterHolds::default(),
            fake_meters: true,
            sim: MeterSim::new(),
            last_meters: f64::NEG_INFINITY,
            outbox: Vec::new(),
            next_seq: 1,
            sendq: Vec::new(),
            throttle: Throttle::new(SEND_INTERVAL),
            pending: Vec::new(),
            health: Health::Ok,
            health_detail: None,
            jack: None,
            toasts: Vec::new(),
            clock_watch: None,
            restore: RestoreState::Idle,
            meters_follow_tab: true,
            remote: None,
            learn: None,
            now: 0.0,
        }
    }

    /// Live state: empty until openoriond sends `State`.
    pub fn live(origin: impl Into<String>) -> Self {
        let mut s = AppState::new(openorion_proto::decode::empty_state(), origin);
        s.conn = Conn::Connecting;
        s.synced = false;
        s.fake_meters = false;
        s.health = Health::Syncing;
        s
    }

    /// Time of the last `tick` (s).
    pub fn now(&self) -> f64 {
        self.now
    }

    pub fn is_live(&self) -> bool {
        self.conn != Conn::Offline
    }

    /// Controls usable: offline, or connected with a synced state.
    pub fn ready(&self) -> bool {
        match self.conn {
            Conn::Offline => true,
            Conn::Connected(_) => self.synced,
            _ => false,
        }
    }

    /// Advance to time `now` (seconds): meters, throttled sends, the clock
    /// watch and toast expiry.
    pub fn tick(&mut self, now: f64) {
        self.now = now;
        for op in self.throttle.due(now) {
            self.sendq.push(Outgoing::Op(op));
        }
        if self.fake_meters {
            let (m, a) = self.sim.sample(now);
            self.meters = m;
            self.afx_meters = a;
        } else if now - self.last_meters > METER_STALE {
            self.meters = Meters::default();
            self.afx_meters = AfxMeters::default();
        }
        self.holds.update(&self.meters, &self.afx_meters, now);
        self.tick_clock(now);
        self.toasts.retain(|t| t.until > now);
    }

    pub fn toast(&mut self, kind: ToastKind, text: impl Into<String>) {
        let text = text.into();
        let secs = if kind == ToastKind::Info { 3.0 } else { 6.0 };
        self.toasts.retain(|t| t.text != text);
        self.toasts.push(Toast {
            text,
            kind,
            until: self.now + secs,
        });
        if self.toasts.len() > 5 {
            self.toasts.remove(0);
        }
    }

    /// Queue an op. Disruptive ops go to `pending` until confirmed.
    pub fn submit(&mut self, op: Op) {
        self.submit_all(vec![op]);
    }

    /// Queue a batch. If any op is disruptive the whole batch waits for one
    /// confirmation (it replaces an unconfirmed batch). Mixer ops on a
    /// linked pair are mirrored to the partner channel.
    ///
    /// In learn mode every op is dropped (not applied, sent or logged):
    /// clicking a control there selects it for binding, and since widgets
    /// render from `device` each frame, a dragged control snaps back.
    pub fn submit_all(&mut self, ops: Vec<Op>) {
        if self.learn.is_some() {
            return;
        }
        let mut all = Vec::with_capacity(ops.len());
        for op in ops {
            let partner = linked_partner(&op, &self.device);
            all.push(op);
            if let Some(p) = partner
                && !all.contains(&p)
            {
                all.push(p);
            }
        }
        if all.iter().any(|o| o.risk() == Risk::Disruptive) {
            self.pending = all;
        } else {
            for op in all {
                self.commit(op);
            }
        }
    }

    pub fn confirm(&mut self) {
        for op in std::mem::take(&mut self.pending) {
            self.commit(op);
        }
    }

    pub fn cancel(&mut self) {
        self.pending.clear();
    }

    /// Ask openoriond to restore a snapshot (dry run first, then apply).
    pub fn request_restore(&mut self, snapshot: Snapshot, dry_run: bool) {
        self.restore = RestoreState::Waiting { dry_run };
        self.sendq.push(Outgoing::Restore {
            snapshot: Box::new(snapshot),
            dry_run,
        });
    }

    fn commit(&mut self, op: Op) {
        if let Op::ClockSource { source } = op {
            self.clock_watch = Some(ClockWatch {
                prev: self.device.status.clock_source,
                target: source,
                started: self.now,
                revert_at: None,
            });
        }
        self.commit_quiet(op);
    }

    /// Commit without starting a clock watch (used by revert).
    fn commit_quiet(&mut self, op: Op) {
        apply_local(&mut self.device, &op);
        for o in self.throttle.push(op.clone(), self.now) {
            self.sendq.push(Outgoing::Op(o));
        }
        let key = coalesce_key(&op);
        if let (Some(k), Some(last)) = (&key, self.outbox.last_mut())
            && coalesce_key(&last.op).as_ref() == Some(k)
        {
            last.op = op;
            last.merged += 1;
            return;
        }
        let risk = op.risk();
        self.outbox.push(OutboxEntry {
            seq: self.next_seq,
            op,
            risk,
            merged: 0,
        });
        self.next_seq += 1;
        if self.outbox.len() > LOG_MAX {
            self.outbox.drain(..self.outbox.len() - LOG_MAX);
        }
    }

    // -- clock keep / revert ---------------------------------------------------

    fn tick_clock(&mut self, now: f64) {
        let Some(w) = &mut self.clock_watch else {
            return;
        };
        let st = &self.device.status;
        let locked = st.clock_locked() && st.clock_source == Some(w.target);
        if locked || !w.target.is_external() {
            if w.revert_at.is_some() {
                self.clock_watch = None;
                self.toast(ToastKind::Info, "Clock locked");
            } else {
                self.clock_watch = None;
            }
            return;
        }
        let (revert_at, started) = (w.revert_at, w.started);
        match revert_at {
            None if now - started >= LOCK_WAIT => w.revert_at = Some(now + REVERT_COUNTDOWN),
            Some(t) if now >= t => self.clock_revert(),
            _ => {}
        }
    }

    /// The keep / revert dialog is up: (target, prev, seconds left).
    pub fn clock_prompt(&self) -> Option<(ClockSource, Option<ClockSource>, f64)> {
        let w = self.clock_watch.as_ref()?;
        let t = w.revert_at?;
        Some((w.target, w.prev, (t - self.now).max(0.0)))
    }

    pub fn clock_keep(&mut self) {
        self.clock_watch = None;
    }

    pub fn clock_revert(&mut self) {
        let Some(w) = self.clock_watch.take() else {
            return;
        };
        match w.prev {
            Some(prev) if prev != w.target => {
                self.commit_quiet(Op::ClockSource { source: prev });
                self.toast(
                    ToastKind::Warn,
                    format!(
                        "No LOCK on {}: reverted to {}",
                        names::clock_source_name(w.target),
                        names::clock_source_name(prev)
                    ),
                );
            }
            _ => self.toast(ToastKind::Warn, "No previous clock source to revert to"),
        }
    }

    // -- daemon events ---------------------------------------------------------

    /// Apply what the bridge delivered.
    pub fn apply_incoming(&mut self, msg: Incoming) {
        match msg {
            Incoming::Connecting => self.conn = Conn::Connecting,
            Incoming::Connected(info) => {
                self.origin = format!(
                    "openoriond {}{}",
                    info.daemon,
                    if info.sim { " (sim)" } else { "" }
                );
                self.conn = Conn::Connected(info);
                self.synced = false;
                self.health = Health::Syncing;
                // Is openorion-remote there? It answers with `Announce`.
                self.remote_query();
            }
            Incoming::Down(e) => {
                if matches!(self.conn, Conn::Connected(_)) {
                    self.toast(ToastKind::Error, "Lost connection to openoriond");
                }
                self.conn = Conn::Down(e);
                self.synced = false;
                self.health = Health::Disconnected;
                // Whatever was held belongs to the old connection.
                self.throttle.flush();
                self.sendq.clear();
                if matches!(self.restore, RestoreState::Waiting { .. }) {
                    self.restore = RestoreState::Idle;
                }
                self.remote_lost();
            }
            Incoming::SendFailed(e) => self.toast(ToastKind::Error, e),
            Incoming::Msg(m) => self.apply_daemon(m),
            Incoming::Remote(v) => self.apply_remote(v),
        }
    }

    pub fn apply_daemon(&mut self, msg: DaemonMsg) {
        match msg {
            DaemonMsg::Hello { .. } | DaemonMsg::Snapshot { .. } | DaemonMsg::Remote { .. } => {}
            DaemonMsg::State { state } => {
                self.confirmed = (*state).clone();
                self.device = *state;
                self.synced = true;
                self.reapply_held();
            }
            DaemonMsg::Status { status } => {
                self.confirmed.status = (*status).clone();
                self.device.status = *status;
                self.reapply_held();
            }
            DaemonMsg::Diff { path, value } => {
                let _ = apply_diff(&mut self.confirmed, &path, &value);
                if let Err(e) = apply_diff(&mut self.device, &path, &value) {
                    self.toast(ToastKind::Error, format!("bad diff at {path}: {e}"));
                }
                self.reapply_held();
            }
            DaemonMsg::Mismatch { path, device, .. } => {
                // The daemon's cache took the device's value: follow it.
                let _ = apply_diff(&mut self.confirmed, &path, &device);
                let _ = apply_diff(&mut self.device, &path, &device);
                self.toast(
                    ToastKind::Warn,
                    format!("Device read-back differs at {path}; showing the device value"),
                );
            }
            DaemonMsg::Meters { meters, afx } => {
                self.meters = *meters;
                self.afx_meters = *afx;
                self.last_meters = self.now;
            }
            DaemonMsg::Result { ok: true, .. } => {}
            DaemonMsg::Result {
                id,
                ok: false,
                error,
            } => {
                let e = error.unwrap_or_else(|| "request failed".into());
                if matches!(self.restore, RestoreState::Waiting { .. }) && id >= RESTORE_ID_BASE {
                    self.restore = RestoreState::Idle;
                }
                // Drop the optimistic edit the daemon did not take.
                self.device = self.confirmed.clone();
                self.reapply_held();
                self.toast(ToastKind::Error, format!("openoriond: {e}"));
            }
            DaemonMsg::Health { health, detail } => {
                if health != self.health
                    && matches!(health, Health::Stalled | Health::Hung | Health::NoCyclic)
                {
                    self.toast(
                        ToastKind::Error,
                        detail
                            .clone()
                            .unwrap_or_else(|| format!("device {health:?}")),
                    );
                }
                self.health = health;
                self.health_detail = detail;
            }
            DaemonMsg::RestorePlan { ops, applied, .. } => {
                if applied {
                    self.toast(
                        ToastKind::Info,
                        format!("Restored: {} ops applied", ops.len()),
                    );
                }
                self.restore = RestoreState::Plan { ops, applied };
            }
            DaemonMsg::Notice { text } => self.toast(ToastKind::Info, text),
        }
    }

    fn reapply_held(&mut self) {
        let held: Vec<Op> = self.throttle.held().cloned().collect();
        for op in &held {
            apply_local(&mut self.device, op);
        }
    }
}

/// Restore requests use ids from here up so they never collide with op ids
/// (which the client numbers from 1).
pub const RESTORE_ID_BASE: u64 = 1 << 40;

/// Ops that a continuous control (slider, knob) emits many times while
/// dragging. Consecutive ops with the same key replace each other in the
/// outbox. None = never merged.
pub fn coalesce_key(op: &Op) -> Option<String> {
    fn f<T>(o: &Option<T>) -> char {
        if o.is_some() { '1' } else { '0' }
    }
    match op {
        Op::Volume { id, .. } => Some(format!("volume/{id:?}")),
        Op::Brightness { .. } => Some("brightness".into()),
        Op::Trim { id, ch, .. } => Some(format!("trim/{id:?}/{ch:?}")),
        Op::MeterSource { bank, .. } => Some(format!("meter/{bank:?}")),
        Op::Mixer {
            mixer,
            ch,
            level,
            pan,
            mute,
            solo,
            send,
        } if mute.is_none() && solo.is_none() => Some(format!(
            "mixer/{}/{ch}/{}{}{}",
            mixer.get(),
            f(level),
            f(pan),
            f(send)
        )),
        Op::AfxEqBand {
            inst,
            band,
            freq,
            q,
            gain,
            ftype,
        } if ftype.is_none() => Some(format!("eq/{inst}/{band}/{}{}{}", f(freq), f(q), f(gain))),
        Op::AfxCompressor { inst, .. } => Some(format!("comp/{inst}")),
        Op::AfxRaw { ty, inst, fields } => {
            let mask: String = fields.iter().map(f).collect();
            Some(format!("raw/{ty:?}/{inst}/{mask}"))
        }
        Op::Reverb { params } => Some(format!("reverb/{}", params.mixer.get())),
        _ => None,
    }
}

/// Mirror an op into the local state (what the device will report back).
/// Out-of-range indices are ignored.
pub fn apply_local(s: &mut DeviceState, op: &Op) {
    let st = &mut s.status;
    match op {
        Op::Power { on } => st.power_on = *on,
        Op::SampleRate { rate } => st.sample_rate = Some(*rate),
        Op::ClockSource { source } => {
            // Same rule as proto `apply_set`: the old source's lock says
            // nothing about the new one.
            if st.clock_source != Some(*source) {
                st.clock.locked = false;
            }
            st.clock_source = Some(*source);
        }
        Op::Volume { id, atten } => vol(st, *id).atten = *atten,
        Op::Mute { id, mute } => vol(st, *id).mute = *mute,
        Op::PresetSave { slot } | Op::PresetRecall { slot } => st.current_preset = slot.get(),
        Op::Oscillator { osc } => st.oscillator = *osc,
        Op::Brightness { value } => st.brightness = value.get(),
        Op::Trim { id, ch, level } => {
            let rec = match id {
                TrimId::Monitor => {
                    st.monitor_trim = level.whole;
                    return;
                }
                TrimId::LineOut => &mut s.trims.line_out,
                TrimId::LineIn => &mut s.trims.line_in,
            };
            match ch {
                None => rec.levels.fill(*level),
                Some(c) => {
                    if let Some(l) = rec.levels.get_mut(*c as usize) {
                        *l = *level;
                    }
                }
            }
        }
        Op::TrimControl { id, control } => match id {
            TrimId::LineOut => s.trims.line_out.control = *control,
            TrimId::LineIn => s.trims.line_in.control = *control,
            TrimId::Monitor => {}
        },
        Op::SpdifSrc { on } => st.spdif_src = *on,
        Op::Smux { id, on } => match id {
            SmuxId::MadiIn => st.smux_in = *on,
            SmuxId::MadiOut => st.smux_out = *on,
        },
        Op::MeterSource { bank, src } => st.meter_sources[*bank as usize] = *src,
        Op::Mixer {
            mixer,
            ch,
            level,
            pan,
            mute,
            solo,
            send,
        } => {
            let m = &mut s.mixers[mixer.get() as usize];
            let strip = match *ch {
                0 => &mut m.master,
                c => match m.channels.get_mut(c as usize - 1) {
                    Some(x) => x,
                    None => return,
                },
            };
            if let Some(v) = level {
                strip.level = *v;
            }
            if let Some(v) = pan {
                strip.pan = *v;
            }
            if let Some(v) = mute {
                strip.mute = *v;
            }
            if let Some(v) = solo {
                strip.solo = *v;
            }
            if let Some(v) = send {
                strip.send = *v;
            }
        }
        Op::Route { bank, ch, sources } => {
            if let Some(b) = s.routing.get_mut(*bank as usize) {
                for (dst, src) in b.sources.iter_mut().skip(*ch as usize).zip(sources) {
                    *dst = *src;
                }
            }
        }
        Op::StereoLink { target, linked } => match target {
            LinkTarget::Mixer { mixer, pair } => {
                if let Some(l) = s.mixer_links[mixer.get() as usize].get_mut(*pair as usize) {
                    *l = *linked;
                }
            }
            LinkTarget::Afx { pair } => {
                if let Some(l) = s.afx.links.get_mut(*pair as usize) {
                    *l = *linked;
                }
            }
        },
        Op::AfxEqBand {
            inst,
            band,
            freq,
            q,
            gain,
            ftype,
        } => {
            if let Some(AfxParams::Eq(eq)) = afx_params(s, AfxType::Equalizer, *inst)
                && let Some(b) = eq.bands.get_mut(*band as usize)
            {
                if let Some(v) = freq {
                    b.freq = *v;
                }
                if let Some(v) = q {
                    b.q = *v;
                }
                if let Some(v) = gain {
                    b.gain = *v;
                }
                if let Some(v) = ftype {
                    b.ftype = *v;
                }
            }
        }
        Op::AfxCompressor { inst, params } => {
            if let Some(AfxParams::Compressor(c)) = afx_params(s, AfxType::Compressor, *inst) {
                *c = *params;
            }
        }
        Op::AfxRaw { ty, inst, fields } => {
            if let Some(AfxParams::Raw(raw)) = afx_params(s, *ty, *inst) {
                for (dst, v) in raw.fields.iter_mut().zip(fields) {
                    if let Some(v) = v {
                        *dst = *v;
                    }
                }
            }
        }
        Op::AfxChain { chain, slots } => {
            if let Some(order) = s.afx.order.get_mut(*chain as usize) {
                *order = [None; AFX_SLOTS];
                for (dst, slot) in order.iter_mut().zip(slots) {
                    *dst = Some(*slot);
                }
            }
        }
        Op::AfxBypass { ty, inst, enabled } => {
            if let Some(c) = s
                .afx
                .configs
                .get_mut(ty)
                .and_then(|v| v.get_mut(*inst as usize))
            {
                c.enabled = *enabled;
            }
        }
        Op::Reverb { params } => {
            if let Some(slot) = s
                .reverb
                .iter_mut()
                .flatten()
                .find(|r| r.mixer == params.mixer)
            {
                *slot = *params;
            }
        }
        Op::Resync | Op::Recheck | Op::Reset { .. } => {}
    }
}

fn vol(st: &mut Status, id: VolumeId) -> &mut VolumeState {
    match id {
        VolumeId::Main => &mut st.main,
        VolumeId::Line => &mut st.line,
    }
}

fn afx_params(s: &mut DeviceState, ty: AfxType, inst: u8) -> Option<&mut AfxParams> {
    s.afx
        .configs
        .get_mut(&ty)
        .and_then(|v| v.get_mut(inst as usize))
        .map(|c| &mut c.params)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::demo::demo_state;
    use openorion_ipc::client::DaemonInfo;
    use serde_json::Value;

    fn mixer_level(m: u8, ch: u8, l: u8) -> Op {
        Op::Mixer {
            mixer: MixerId::new(m).unwrap(),
            ch,
            level: Some(Atten::new(l).unwrap()),
            pan: None,
            mute: None,
            solo: None,
            send: None,
        }
    }

    fn diff_of(path: &str, s: &DeviceState) -> Value {
        serde_json::to_value(s)
            .unwrap()
            .pointer(path)
            .unwrap()
            .clone()
    }

    #[test]
    fn diff_applies_real_paths() {
        let base = demo_state();
        let mut target = base.clone();
        target.mixers[3].channels[0].level = Atten::new(17).unwrap();
        target.mixers[3].channels[0].mute = true;
        target.mixers[2].master.pan = Pan::new(10).unwrap();
        target.routing[14].sources[5] = Source::Input {
            group: SourceGroup::AdatIn,
            chan: 3,
        };
        target.status.main.atten = Atten::new(30).unwrap();
        target.status.clock.locked = !base.status.clock.locked;
        target.mixer_links[3][0] = !base.mixer_links[3][0];
        target.trims.line_out.levels[2] = TrimLevel { whole: 4, fract: 0 };
        if let AfxParams::Eq(eq) =
            &mut target.afx.configs.get_mut(&AfxType::Equalizer).unwrap()[2].params
        {
            eq.bands[1].gain = -40;
        }
        target.afx.order[5] = [None; AFX_SLOTS];
        let paths = [
            "/mixers/3/channels/0",
            "/mixers/2/master",
            "/routing/14",
            "/status",
            "/mixer_links/3/0",
            "/trims/line_out",
            "/afx/configs/Equalizer/2",
            "/afx/order/5",
            "/reverb",
        ];
        let mut s = base.clone();
        for p in paths {
            apply_diff(&mut s, p, &diff_of(p, &target)).unwrap_or_else(|e| panic!("{p}: {e}"));
        }
        assert_eq!(s, target);
        // Whole-state replace.
        let mut s = base.clone();
        apply_diff(&mut s, "", &serde_json::to_value(&target).unwrap()).unwrap();
        assert_eq!(s, target);
        // Errors leave the state alone.
        let mut s = base.clone();
        assert!(apply_diff(&mut s, "/mixers/9/channels/0", &Value::Null).is_err());
        assert!(apply_diff(&mut s, "/mixers/3/channels/40", &Value::Null).is_err());
        assert!(apply_diff(&mut s, "/status", &serde_json::json!({"x": 1})).is_err());
        assert!(apply_diff(&mut s, "/nope/1/2", &Value::Null).is_err());
        assert_eq!(s, base);
    }

    #[test]
    fn rejected_op_rolls_back_to_the_daemon_state() {
        let mut a = AppState::live("t");
        a.apply_incoming(Incoming::Connected(DaemonInfo {
            version: 1,
            daemon: "x".into(),
            sim: true,
        }));
        a.apply_daemon(DaemonMsg::State {
            state: Box::new(demo_state()),
        });
        let before = a.device.status.main.mute;
        a.submit(Op::Mute {
            id: VolumeId::Main,
            mute: !before,
        });
        assert_eq!(a.device.status.main.mute, !before, "optimistic");
        a.apply_daemon(DaemonMsg::Result {
            id: 1,
            ok: false,
            error: Some("the Orion's control channel hangs".into()),
        });
        assert_eq!(a.device.status.main.mute, before, "rolled back");
    }

    #[test]
    fn daemon_messages_update_state() {
        let mut a = AppState::live("t");
        assert!(!a.ready());
        a.apply_incoming(Incoming::Connected(DaemonInfo {
            version: 1,
            daemon: "x".into(),
            sim: true,
        }));
        assert!(!a.ready(), "connected but no state yet");
        a.apply_daemon(DaemonMsg::State {
            state: Box::new(demo_state()),
        });
        assert!(a.ready());
        let mut st = a.device.status.clone();
        st.current_preset = 4;
        a.apply_daemon(DaemonMsg::Status {
            status: Box::new(st),
        });
        assert_eq!(a.device.status.current_preset, 4);
        a.device.mixer_links[0][0] = true;
        a.apply_daemon(DaemonMsg::Mismatch {
            path: "/mixer_links/0/0".into(),
            ours: Value::Bool(true),
            device: Value::Bool(false),
        });
        assert!(!a.device.mixer_links[0][0]);
        assert_eq!(a.toasts.len(), 1);
        a.apply_daemon(DaemonMsg::Result {
            id: 3,
            ok: false,
            error: Some("boom".into()),
        });
        assert!(a.toasts.iter().any(|t| t.text.contains("boom")));
        let mut m = Meters::default();
        m.mixer[0] = 6;
        a.apply_daemon(DaemonMsg::Meters {
            meters: Box::new(m),
            afx: Box::default(),
        });
        a.tick(0.01);
        assert_eq!(a.meters.mixer[0], 6);
        a.tick(5.0);
        assert_eq!(a.meters.mixer[0], 96, "stale meters drop to the floor");
        a.apply_daemon(DaemonMsg::Health {
            health: Health::Hung,
            detail: Some("power-cycle".into()),
        });
        assert_eq!(a.health, Health::Hung);
        a.apply_incoming(Incoming::Down("gone".into()));
        assert!(!a.ready());
        assert_eq!(a.health, Health::Disconnected);
    }

    fn sent_ops(a: &mut AppState) -> Vec<Op> {
        a.sendq
            .drain(..)
            .filter_map(|o| match o {
                Outgoing::Op(op) => Some(op),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn throttle_limits_continuous_sends() {
        let mut a = AppState::new(demo_state(), "demo");
        // 60 Hz drag for half a second.
        let mut sent = Vec::new();
        for i in 0..30u8 {
            a.tick(i as f64 / 60.0);
            a.submit(mixer_level(3, 1, i));
            sent.extend(sent_ops(&mut a));
        }
        a.tick(1.0);
        sent.extend(sent_ops(&mut a));
        assert!((14..=17).contains(&sent.len()), "{}", sent.len());
        assert_eq!(sent.last(), Some(&mixer_level(3, 1, 29)), "latest wins");
        // Different controls do not hold each other back.
        let mut t = Throttle::new(SEND_INTERVAL);
        assert_eq!(t.push(mixer_level(3, 1, 1), 0.0).len(), 1);
        assert_eq!(t.push(mixer_level(3, 2, 1), 0.0).len(), 1);
        assert!(t.push(mixer_level(3, 1, 2), 0.01).is_empty());
        assert!(t.push(mixer_level(3, 1, 3), 0.02).is_empty());
        assert!(t.due(0.03).is_empty());
        assert_eq!(t.due(0.04), vec![mixer_level(3, 1, 3)]);
        // A discrete op flushes held ones first (order kept).
        assert!(t.push(mixer_level(3, 1, 4), 0.05).is_empty());
        let mute = Op::Mute {
            id: VolumeId::Line,
            mute: true,
        };
        assert_eq!(t.push(mute.clone(), 0.05), vec![mixer_level(3, 1, 4), mute]);
    }

    #[test]
    fn held_ops_survive_daemon_echo() {
        let mut a = AppState::new(demo_state(), "demo");
        a.tick(0.0);
        a.submit(mixer_level(3, 1, 10));
        a.submit(mixer_level(3, 1, 20)); // held
        // The daemon echoes the first value.
        let mut echo = a.device.mixers[3].channels[0];
        echo.level = Atten::new(10).unwrap();
        a.apply_daemon(DaemonMsg::Diff {
            path: "/mixers/3/channels/0".into(),
            value: serde_json::to_value(echo).unwrap(),
        });
        assert_eq!(a.device.mixers[3].channels[0].level.get(), 20);
    }

    #[test]
    fn linked_pairs_mirror() {
        let mut a = AppState::new(demo_state(), "demo");
        a.device.mixer_links[3] = [false; MIXER_CHANNELS / 2];
        a.device.mixer_links[3][1] = true; // ch 3-4
        a.tick(0.0);
        a.submit(mixer_level(3, 4, 12));
        assert_eq!(a.device.mixers[3].channels[2].level.get(), 12);
        assert_eq!(a.device.mixers[3].channels[3].level.get(), 12);
        let ops = sent_ops(&mut a);
        assert_eq!(ops, vec![mixer_level(3, 4, 12), mixer_level(3, 3, 12)]);
        // Pan does not follow; unlinked channels do not either.
        let pan = Op::Mixer {
            mixer: MixerId::new(3).unwrap(),
            ch: 3,
            level: None,
            pan: Some(Pan::new(5).unwrap()),
            mute: None,
            solo: None,
            send: None,
        };
        a.submit(pan.clone());
        assert_eq!(sent_ops(&mut a), vec![pan]);
        a.submit(mixer_level(3, 5, 1));
        assert_eq!(sent_ops(&mut a).len(), 1);
    }

    #[test]
    fn clock_change_reverts_without_lock() {
        let mut a = AppState::new(demo_state(), "demo");
        a.device.status.clock_source = Some(ClockSource::Internal);
        a.device.status.clock.locked = false;
        a.tick(100.0);
        a.submit(Op::ClockSource {
            source: ClockSource::WordClock,
        });
        a.confirm();
        sent_ops(&mut a);
        a.tick(105.0);
        assert!(a.clock_prompt().is_none());
        a.tick(110.0);
        let (target, prev, left) = a.clock_prompt().expect("prompt after 10 s");
        assert_eq!(target, ClockSource::WordClock);
        assert_eq!(prev, Some(ClockSource::Internal));
        assert!((left - REVERT_COUNTDOWN).abs() < 1e-9);
        a.tick(119.0);
        assert!(a.clock_prompt().is_some());
        a.tick(120.0);
        assert!(a.clock_prompt().is_none());
        assert_eq!(
            sent_ops(&mut a),
            vec![Op::ClockSource {
                source: ClockSource::Internal
            }]
        );
        assert_eq!(a.device.status.clock_source, Some(ClockSource::Internal));
        assert!(a.clock_watch.is_none(), "a revert is not watched again");

        // Lock arriving cancels the watch; Keep dismisses the prompt.
        a.submit(Op::ClockSource {
            source: ClockSource::Madi,
        });
        a.confirm();
        a.tick(121.0);
        a.device.status.clock.locked = true;
        a.tick(122.0);
        assert!(a.clock_watch.is_none());
        a.device.status.clock.locked = false;
        a.submit(Op::ClockSource {
            source: ClockSource::Spdif,
        });
        a.confirm();
        a.tick(140.0);
        assert!(a.clock_prompt().is_some());
        a.clock_keep();
        a.tick(200.0);
        assert!(a.clock_watch.is_none());
        assert_eq!(a.device.status.clock_source, Some(ClockSource::Spdif));
        // Internal needs no lock.
        a.submit(Op::ClockSource {
            source: ClockSource::Internal,
        });
        a.confirm();
        a.tick(201.0);
        assert!(a.clock_watch.is_none());
    }

    #[test]
    fn disruptive_waits_for_confirm() {
        let mut a = AppState::new(demo_state(), "demo");
        a.submit(Op::SampleRate {
            rate: SampleRate::R48k,
        });
        assert_eq!(a.pending.len(), 1);
        assert!(a.outbox.is_empty());
        assert_eq!(a.device.status.sample_rate, Some(SampleRate::R96k));
        a.confirm();
        assert!(a.pending.is_empty());
        assert_eq!(a.outbox.len(), 1);
        assert_eq!(a.outbox[0].risk, Risk::Disruptive);
        assert_eq!(a.device.status.sample_rate, Some(SampleRate::R48k));

        a.submit(Op::Power { on: false });
        a.cancel();
        assert!(a.device.status.power_on);
        assert_eq!(a.outbox.len(), 1);
    }

    #[test]
    fn slider_drags_coalesce() {
        let mut a = AppState::new(demo_state(), "demo");
        for l in 0..10 {
            a.submit(mixer_level(3, 1, l));
        }
        assert_eq!(a.outbox.len(), 1);
        assert_eq!(a.outbox[0].merged, 9);
        assert_eq!(a.outbox[0].op, mixer_level(3, 1, 9));
        // A different channel starts a new entry.
        a.submit(mixer_level(3, 2, 5));
        assert_eq!(a.outbox.len(), 2);
        // Discrete ops never merge.
        let mute = Op::Mute {
            id: VolumeId::Line,
            mute: true,
        };
        a.submit(mute.clone());
        a.submit(mute);
        assert_eq!(a.outbox.len(), 4);
        assert_eq!(a.device.mixers[3].channels[0].level.get(), 9);
        assert_eq!(a.device.mixers[3].channels[1].level.get(), 5);
        assert!(a.device.status.line.mute);
    }

    #[test]
    fn apply_local_covers_structures() {
        let mut s = demo_state();
        apply_local(&mut s, &mixer_level(0, 0, 7));
        assert_eq!(s.mixers[0].master.level.get(), 7);
        apply_local(&mut s, &mixer_level(0, 33, 7)); // out of range: ignored
        apply_local(
            &mut s,
            &Op::Route {
                bank: OutputBank::MixCh4,
                ch: 30,
                sources: vec![
                    Source::Input {
                        group: SourceGroup::LineIn,
                        chan: 0,
                    };
                    4
                ],
            },
        );
        let b = &s.routing[OutputBank::MixCh4 as usize].sources;
        assert_eq!(b.len(), 32);
        assert_ne!(b[31], Source::Mute);
        apply_local(
            &mut s,
            &Op::Trim {
                id: TrimId::LineOut,
                ch: None,
                level: TrimLevel { whole: 3, fract: 0 },
            },
        );
        assert!(s.trims.line_out.levels.iter().all(|l| l.whole == 3));
        apply_local(
            &mut s,
            &Op::AfxEqBand {
                inst: 2,
                band: 4,
                freq: Some(12000),
                q: None,
                gain: None,
                ftype: Some(EqFilter::Lpf),
            },
        );
        let AfxParams::Eq(eq) = &s.afx.configs[&AfxType::Equalizer][2].params else {
            panic!()
        };
        assert_eq!(eq.bands[4].freq, 12000);
        assert_eq!(eq.bands[4].ftype, EqFilter::Lpf);
        apply_local(
            &mut s,
            &Op::AfxChain {
                chain: 0,
                slots: vec![],
            },
        );
        assert!(s.afx.order[0].iter().all(Option::is_none));
    }
}
