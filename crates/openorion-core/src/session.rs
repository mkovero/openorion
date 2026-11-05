//! The session task (DESIGN.md sec. 6).
//!
//! One tokio task owns the request queue and the state cache. The
//! transport lives on a dedicated I/O thread (`io.rs`) because its calls
//! block; the task sends it writes and receives every incoming report.

use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use openorion_hid::Transport;
use openorion_ipc::{Health, Op};
use openorion_proto::decode::{GetReply, apply_reply, apply_set, decode_reply, empty_state};
use openorion_proto::*;
use serde_json::Value;
use tokio::sync::{broadcast, mpsc, oneshot, watch};
use tokio::time::Instant;

use crate::Error;
use crate::build::{self, Cover, Key, build_sets, cover, key, overlay, region, set_path};
use crate::io::{Io, IoCmd, IoEvent};
use crate::lock;
use crate::restore::plan_restore;

/// Timing constants. `Default` is the design (DESIGN.md sec. 6); tests may
/// shorten them.
#[derive(Debug, Clone)]
pub struct Timings {
    /// Wait for the first cyclic report after open.
    pub startup_cyclic: Duration,
    /// Wait for cyclic reports after a reset.
    pub reset_cyclic: Duration,
    /// Get reply timeout (one retry).
    pub request_timeout: Duration,
    /// Cyclic silence that counts as "no cyclic".
    pub cyclic_gap: Duration,
    /// Minimum spacing of sends per continuous key.
    pub coalesce: Duration,
    /// Debounce after the last set to a key before reading it back.
    pub verify_delay: Duration,
    /// Meter event period (30 Hz).
    pub meter_period: Duration,
    /// Delay between our preset recall and the resync.
    pub recall_settle: Duration,
    /// Transport read timeout on the I/O thread (bounds write latency).
    pub read_poll: Duration,
    /// Minimum spacing of any two writes to the device. Unpaced bursts
    /// (~1000 sets/s) were followed by a control-channel hang on hardware.
    pub write_gap: Duration,
    /// Idle time after which the control channel is probed with one get,
    /// so a hang shows before the next setting is lost, not after.
    pub probe_idle: Duration,
    /// Pause before asking again after a 0x61 answer to a get the device
    /// always knows (it answers 0x61 while still booting).
    pub unknown_backoff: Duration,
}

impl Default for Timings {
    fn default() -> Self {
        Timings {
            startup_cyclic: Duration::from_millis(200),
            reset_cyclic: Duration::from_secs(2),
            request_timeout: Duration::from_millis(500),
            cyclic_gap: Duration::from_secs(1),
            coalesce: Duration::from_millis(20),
            verify_delay: Duration::from_millis(150),
            meter_period: Duration::from_millis(33),
            recall_settle: Duration::from_millis(300),
            read_poll: Duration::from_millis(5),
            write_gap: Duration::from_millis(5),
            probe_idle: Duration::from_secs(5),
            unknown_backoff: Duration::from_millis(500),
        }
    }
}

#[derive(Debug, Clone)]
pub struct SessionOpts {
    pub timings: Timings,
    /// Default: `openorion_ipc::lock_path()`.
    pub lock_path: PathBuf,
    /// Read back after sets (DESIGN.md 6.6). Default true.
    pub verify: bool,
    /// Whether the Orion's USB audio is streaming (why, if so). A USB reset
    /// cuts the stream, so resets are refused while it runs unless forced.
    /// Default: `openorion_hid::usb_audio_busy`.
    pub audio_busy: fn() -> Option<String>,
}

impl Default for SessionOpts {
    fn default() -> Self {
        SessionOpts {
            timings: Timings::default(),
            lock_path: openorion_ipc::lock_path(),
            verify: true,
            audio_busy: openorion_hid::usb_audio_busy,
        }
    }
}

/// What the session publishes. Maps onto `openorion_ipc::DaemonMsg`.
#[derive(Debug, Clone, PartialEq)]
pub enum Event {
    /// A full sync finished; the whole cached state.
    Synced(Box<DeviceState>),
    /// Non-meter status changed (cache view: device status plus our
    /// not-yet-confirmed status-backed sets).
    Status(Box<Status>),
    /// 30 Hz; per channel the minimum raw (= loudest) of the window.
    Meters {
        meters: Box<Meters>,
        afx: Box<AfxMeters>,
    },
    Health {
        health: Health,
        detail: Option<String>,
    },
    /// Read-back disagreed with the cache; the cache took `device`.
    /// `path` is a JSON pointer into `DeviceState`.
    Mismatch {
        path: String,
        ours: Value,
        device: Value,
    },
    /// An op changed the cache (optimistically). JSON pointer + new value.
    Diff { path: String, value: Value },
}

/// Health as a watchable value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HealthInfo {
    pub health: Health,
    pub detail: Option<String>,
    /// Completed full syncs.
    pub syncs: u64,
    /// False once the session task stopped or gave up.
    pub alive: bool,
}

struct Shared {
    state: DeviceState,
    synced: bool,
}

enum Cmd {
    Op(Op, oneshot::Sender<Result<(), Error>>),
    Shutdown,
}

/// Cheap to clone; every clone talks to the same session. The session
/// stops when every handle is dropped or `shutdown` is called.
#[derive(Clone)]
pub struct SessionHandle {
    cmd: mpsc::UnboundedSender<Cmd>,
    shared: Arc<Mutex<Shared>>,
    health: watch::Receiver<HealthInfo>,
    events: broadcast::Sender<Event>,
}

pub struct Session;

impl Session {
    /// Take the lock, start the I/O thread, make sure cyclic reports flow
    /// (one reset if not), then start the session task, which runs a full
    /// sync and publishes `Event::Synced`. Returns once the device is
    /// talking; use `SessionHandle::wait_synced` for the first sync.
    pub async fn start(
        transport: Box<dyn Transport>,
        opts: SessionOpts,
    ) -> Result<(SessionHandle, broadcast::Receiver<Event>), Error> {
        let lock = lock::acquire(&opts.lock_path)?;
        let t = opts.timings.clone();
        let mut io = Io::spawn(transport, t.read_poll);

        let mut first = wait_cyclic(&mut io, t.startup_cyclic).await?;
        let mut reset_done = false;
        if first.is_none() {
            if let Some(why) = (opts.audio_busy)() {
                return Err(Error::ResetBlocked(why));
            }
            tracing::info!("no cyclic reports after open; resetting the device once");
            io.send(IoCmd::Reset);
            wait_reset(&mut io).await?;
            reset_done = true;
            first = wait_cyclic(&mut io, t.reset_cyclic).await?;
            if first.is_none() {
                return Err(Error::NoCyclic);
            }
        }
        if reset_done {
            tracing::info!("cyclic reports after reset");
        }

        let mut state = empty_state();
        let mut device_status = None;
        if let Some(Some(s)) = first {
            state.status = (*s).clone();
            device_status = Some(s);
        }
        let shared = Arc::new(Mutex::new(Shared {
            state,
            synced: false,
        }));
        let (events, rx) = broadcast::channel(1024);
        let (health_tx, health_rx) = watch::channel(HealthInfo {
            health: Health::Syncing,
            detail: None,
            syncs: 0,
            alive: true,
        });
        let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
        let now = Instant::now();
        let mut task = Task {
            t,
            verify: opts.verify,
            audio_busy: opts.audio_busy,
            io,
            cmd_rx,
            shared: shared.clone(),
            events: events.clone(),
            health_tx,
            _lock: lock,
            queue: VecDeque::new(),
            in_flight: None,
            entries: HashMap::new(),
            last_cyclic: now,
            device_status,
            published_status: None,
            sync: None,
            sync_waiters: Vec::new(),
            resync_at: None,
            our_save: None,
            meter_acc: None,
            afx_acc: None,
            last_afx: AfxMeters::default(),
            next_meter: now,
            last_write: None,
            hold_until: None,
            last_answer: now,
            unknown_streak: 0,
            recheck: 0,
            watch_next: 0,
            mode: Mode::Running,
            finished: false,
        };
        task.start_sync();
        tokio::spawn(task.run());
        Ok((
            SessionHandle {
                cmd: cmd_tx,
                shared,
                health: health_rx,
                events,
            },
            rx,
        ))
    }
}

/// Wait for a cyclic report. `Ok(Some(status))`: cyclic seen (the status
/// if it was a 0x73). `Ok(None)`: timed out.
async fn wait_cyclic(io: &mut Io, within: Duration) -> Result<Option<Option<Box<Status>>>, Error> {
    let deadline = Instant::now() + within;
    loop {
        match tokio::time::timeout_at(deadline, io.rx.recv()).await {
            Err(_) => return Ok(None),
            Ok(None) => return Err(Error::Disconnected),
            Ok(Some(IoEvent::Error(e))) => return Err(Error::Io(e)),
            Ok(Some(IoEvent::ResetDone(_))) => {}
            Ok(Some(IoEvent::Report(r))) => match parse(&r) {
                Incoming::Status(s, _) => return Ok(Some(Some(s))),
                Incoming::AfxMeters(_) => return Ok(Some(None)),
                _ => {}
            },
        }
    }
}

async fn wait_reset(io: &mut Io) -> Result<(), Error> {
    loop {
        match io.rx.recv().await {
            None => return Err(Error::Disconnected),
            Some(IoEvent::Error(e)) => return Err(Error::Io(e)),
            Some(IoEvent::ResetDone(r)) => return r.map_err(|e| Error::Io(format!("reset: {e}"))),
            Some(IoEvent::Report(_)) => {}
        }
    }
}

impl SessionHandle {
    /// Apply a semantic op: build the full set(s) from the cache, update
    /// the cache optimistically and queue the send. Returns once queued.
    /// `Op::Resync` returns when the resync finished.
    pub async fn op(&self, op: Op) -> Result<(), Error> {
        let (tx, rx) = oneshot::channel();
        self.cmd.send(Cmd::Op(op, tx)).map_err(|_| Error::Closed)?;
        rx.await.map_err(|_| Error::Closed)?
    }

    /// The cached state (meaningful once `is_synced`).
    pub fn state(&self) -> DeviceState {
        lock_shared(&self.shared).state.clone()
    }

    pub fn is_synced(&self) -> bool {
        lock_shared(&self.shared).synced
    }

    /// Snapshot of the cache. Errors before the first sync.
    pub fn snapshot(&self) -> Result<Snapshot, Error> {
        let sh = lock_shared(&self.shared);
        if !sh.synced {
            return Err(Error::NotSynced("no full sync yet".into()));
        }
        Ok(Snapshot {
            version: 1,
            taken: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0),
            state: sh.state.clone(),
        })
    }

    /// Plan (and unless `dry_run`, apply) the ops that take the device to
    /// `snapshot`. Returns the plan.
    pub async fn restore(&self, snapshot: &Snapshot, dry_run: bool) -> Result<Vec<Op>, Error> {
        let current = {
            let sh = lock_shared(&self.shared);
            if !sh.synced {
                return Err(Error::NotSynced("no full sync yet".into()));
            }
            sh.state.clone()
        };
        let ops = plan_restore(&current, &snapshot.state);
        if !dry_run {
            for op in &ops {
                self.op(op.clone()).await?;
            }
        }
        Ok(ops)
    }

    pub fn health(&self) -> HealthInfo {
        self.health.borrow().clone()
    }

    /// Watch health changes.
    pub fn health_watch(&self) -> watch::Receiver<HealthInfo> {
        self.health.clone()
    }

    pub fn subscribe(&self) -> broadcast::Receiver<Event> {
        self.events.subscribe()
    }

    /// Wait for the first full sync (returns at once if done).
    pub async fn wait_synced(&self) -> Result<DeviceState, Error> {
        let mut w = self.health.clone();
        loop {
            {
                let h = w.borrow_and_update();
                if h.syncs > 0 {
                    break;
                }
                if !h.alive || matches!(h.health, Health::Hung | Health::NoCyclic) {
                    return Err(health_error(&h));
                }
            }
            if w.changed().await.is_err() {
                return Err(Error::Closed);
            }
        }
        Ok(self.state())
    }

    /// Stop the session (also happens when every handle is dropped).
    pub fn shutdown(&self) {
        let _ = self.cmd.send(Cmd::Shutdown);
    }
}

fn health_error(h: &HealthInfo) -> Error {
    match h.health {
        Health::Hung => Error::Hung,
        Health::NoCyclic => Error::NoCyclic,
        Health::Stalled => Error::Stalled,
        Health::Disconnected => Error::Disconnected,
        _ => Error::Closed,
    }
}

fn lock_shared(m: &Mutex<Shared>) -> MutexGuard<'_, Shared> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

// ---------------------------------------------------------------------------
// Task

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Purpose {
    Sync,
    Verify,
    /// Liveness check (`Op::Recheck`).
    Probe,
    /// Idle re-read of one setting block: liveness, and a resync when the
    /// device changed behind our back (front panel, another client).
    Watch,
}

/// Consecutive 0x61 answers to `get firmware` before the channel counts
/// as hung (a booting device stops answering 0x61 within a few tries).
const UNKNOWN_LIMIT: u32 = 4;

enum Req {
    /// The latest set of a key (looked up when written).
    Write(Key),
    /// An unkeyed set (preset save/recall).
    WriteSet(Set),
    Get(Get, Purpose),
}

struct InFlight {
    get: Get,
    purpose: Purpose,
    deadline: Instant,
    retried: bool,
}

/// Per-key bookkeeping for coalescing and verify. The entry lives from the
/// first set until the read-back confirmed the latest one; while it lives,
/// its set is re-applied on top of fresh device data (the overlay).
struct Entry {
    set: Set,
    seq: u64,
    written_seq: u64,
    queued: bool,
    last_write: Option<Instant>,
    verify_at: Option<Instant>,
    verify_req: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    Running,
    /// Reset sent, waiting for it to finish.
    Resetting,
    /// Reset done, waiting for cyclic reports.
    Recovering(Instant),
    /// Cyclic reports stopped: nothing is sent until they come back or
    /// the user asks for a reset (`Op::Reset`). No automatic reset: it
    /// would cut a running audio stream.
    Stalled,
    /// Control channel hung: nothing is sent.
    Hung,
    /// No cyclic after the recovery reset: nothing is sent.
    GaveUp,
}

struct SyncRun {
    outstanding: usize,
    again: bool,
}

struct Task {
    t: Timings,
    verify: bool,
    audio_busy: fn() -> Option<String>,
    io: Io,
    cmd_rx: mpsc::UnboundedReceiver<Cmd>,
    shared: Arc<Mutex<Shared>>,
    events: broadcast::Sender<Event>,
    health_tx: watch::Sender<HealthInfo>,
    _lock: std::fs::File,
    queue: VecDeque<Req>,
    in_flight: Option<InFlight>,
    entries: HashMap<Key, Entry>,
    last_cyclic: Instant,
    /// Last raw device status (no overlay).
    device_status: Option<Box<Status>>,
    published_status: Option<Status>,
    sync: Option<SyncRun>,
    sync_waiters: Vec<oneshot::Sender<Result<(), Error>>>,
    resync_at: Option<Instant>,
    our_save: Option<(u8, Instant)>,
    meter_acc: Option<Meters>,
    afx_acc: Option<AfxMeters>,
    last_afx: AfxMeters,
    next_meter: Instant,
    /// When the last report went out (writes are paced by `write_gap`).
    last_write: Option<Instant>,
    /// No write before this (backoff after 0x61).
    hold_until: Option<Instant>,
    /// When the device last answered a get.
    last_answer: Instant,
    /// Consecutive 0x61 answers to `get firmware`.
    unknown_streak: u32,
    /// Probes of a running `Op::Recheck` still to be answered.
    recheck: u32,
    /// Position in the idle re-read rotation (`watch_gets`).
    watch_next: usize,
    mode: Mode,
    finished: bool,
}

impl Task {
    async fn run(mut self) {
        loop {
            self.pump();
            if self.finished {
                break;
            }
            let deadline = self.next_deadline();
            tokio::select! {
                c = self.cmd_rx.recv() => match c {
                    None | Some(Cmd::Shutdown) => break,
                    Some(Cmd::Op(op, tx)) => self.on_op(op, tx),
                },
                e = self.io.rx.recv() => match e {
                    None => self.disconnected("I/O thread stopped".into()),
                    Some(ev) => self.on_io(ev),
                },
                _ = tokio::time::sleep_until(deadline) => {}
            }
            if self.finished {
                break;
            }
        }
        for w in self.sync_waiters.drain(..) {
            let _ = w.send(Err(Error::Closed));
        }
        self.health_tx.send_modify(|h| h.alive = false);
        self.io.send(IoCmd::Shutdown);
    }

    fn shared(&self) -> MutexGuard<'_, Shared> {
        lock_shared(&self.shared)
    }

    fn emit(&self, e: Event) {
        let _ = self.events.send(e);
    }

    fn set_health(&self, health: Health, detail: Option<String>) {
        let changed = {
            let h = self.health_tx.borrow();
            h.health != health || h.detail != detail
        };
        if changed {
            self.health_tx.send_modify(|h| {
                h.health = health;
                h.detail = detail.clone();
            });
            self.emit(Event::Health { health, detail });
        }
    }

    fn can_send(&self) -> bool {
        self.mode == Mode::Running
    }

    fn write(&mut self, r: Report) {
        self.last_write = Some(Instant::now());
        self.io.send(IoCmd::Write(Box::new(r)));
    }

    /// When the next write may go out.
    fn write_ready_at(&self) -> Option<Instant> {
        let gap = self.last_write.map(|w| w + self.t.write_gap);
        match (gap, self.hold_until) {
            (Some(a), Some(b)) => Some(a.max(b)),
            (a, b) => a.or(b),
        }
    }

    /// Nothing in flight or queued and no sync running.
    fn idle(&self) -> bool {
        self.in_flight.is_none() && self.queue.is_empty() && self.sync.is_none()
    }

    // -- timers and the queue ------------------------------------------------

    fn next_deadline(&self) -> Instant {
        let now = Instant::now();
        let mut d = now + Duration::from_secs(1);
        let mut at = |x: Instant| {
            if x < d {
                d = x;
            }
        };
        at(self.next_meter);
        if let Some(f) = &self.in_flight {
            at(f.deadline);
        }
        if self.mode == Mode::Running {
            at(self.last_cyclic + self.t.cyclic_gap);
            if self.idle() {
                at(self.last_answer + self.t.probe_idle);
            }
            if self.in_flight.is_none()
                && !self.queue.is_empty()
                && let Some(x) = self.write_ready_at()
            {
                at(x);
            }
        }
        if let Mode::Recovering(x) = self.mode {
            at(x);
        }
        if let Some(x) = self.resync_at {
            at(x);
        }
        for e in self.entries.values() {
            if let Some(x) = e.verify_at {
                at(x);
            }
            if !e.queued
                && e.written_seq != e.seq
                && let Some(lw) = e.last_write
            {
                at(lw + self.t.coalesce);
            }
        }
        d.max(now)
    }

    fn pump(&mut self) {
        let now = Instant::now();
        self.check_in_flight(now);
        self.check_cyclic(now);
        if let Some(x) = self.resync_at
            && now >= x
        {
            self.resync_at = None;
            self.start_sync();
        }
        self.check_entries(now);
        self.check_probe(now);
        if now >= self.next_meter {
            self.flush_meters();
            self.next_meter += self.t.meter_period;
            if self.next_meter <= now {
                self.next_meter = now + self.t.meter_period;
            }
        }
        self.drive_queue();
    }

    fn check_in_flight(&mut self, now: Instant) {
        let Some(f) = &mut self.in_flight else {
            return;
        };
        if now < f.deadline {
            return;
        }
        let cyclic_flows = now.duration_since(self.last_cyclic) < self.t.cyclic_gap;
        if !f.retried {
            tracing::debug!("get {:?} timed out; retrying", f.get);
            f.retried = true;
            f.deadline = now + self.t.request_timeout;
            let r = f.get.report();
            self.write(r);
            return;
        }
        if cyclic_flows {
            tracing::warn!("get {:?} unanswered twice while cyclic flows: hung", f.get);
            self.hung("requests unanswered while cyclic reports flow");
        } else {
            // Cyclic stopped too: the gap check reports it.
            f.deadline = now + self.t.request_timeout;
        }
    }

    /// Idle probe: when nothing was answered for a while, re-read the next
    /// setting block. Any answer proves the channel; a changed answer
    /// means the device changed behind our back.
    fn check_probe(&mut self, now: Instant) {
        if self.mode == Mode::Running && self.idle() && now >= self.last_answer + self.t.probe_idle
        {
            let gets = self.watch_gets();
            let g = gets[self.watch_next % gets.len()];
            self.watch_next = self.watch_next.wrapping_add(1);
            self.queue.push_back(Req::Get(g, Purpose::Watch));
        }
    }

    /// The idle re-read rotation: every setting block a sync reads
    /// (device info is fixed and left out).
    fn watch_gets(&self) -> Vec<Get> {
        let mut v = vec![Get::Firmware];
        v.extend(
            Self::sync_gets()
                .into_iter()
                .filter(|g| !matches!(g, Get::Firmware | Get::Passport | Get::Registration)),
        );
        let sh = lock_shared(&self.shared);
        v.extend(Self::afx_config_gets(
            sh.state.afx.instances.iter().map(|(&t, &n)| (t, n)),
        ));
        v
    }

    /// The AFX config gets for the instance counts a device reports.
    fn afx_config_gets(instances: impl IntoIterator<Item = (AfxType, u8)>) -> Vec<Get> {
        let mut gets = Vec::new();
        for (ty, n) in instances {
            if n == 0 || !ty.on_orion() || openorion_proto::decode::default_afx_config(ty).is_none()
            {
                continue;
            }
            if ty == AfxType::Equalizer {
                gets.push(Get::AfxConfigs(ty, 0));
                gets.push(Get::AfxConfigs(ty, 1));
            } else {
                gets.push(Get::AfxConfigs(ty, 0));
            }
        }
        gets
    }

    fn hung(&mut self, why: &str) {
        self.halt(
            Mode::Hung,
            Health::Hung,
            format!("{why}; power-cycle the Orion (a USB reset does not clear this)"),
            || Error::Hung,
        );
    }

    /// Stop sending: hung or gave up. Unsent and unconfirmed settings are
    /// dropped with the queue; after recovery the resync reads what the
    /// device really has.
    fn halt(&mut self, mode: Mode, health: Health, detail: String, err: fn() -> Error) {
        self.mode = mode;
        self.queue.clear();
        self.in_flight = None;
        self.sync = None;
        self.resync_at = None;
        self.hold_until = None;
        self.recheck = 0;
        self.unknown_streak = 0;
        self.entries.clear();
        for w in self.sync_waiters.drain(..) {
            let _ = w.send(Err(err()));
        }
        self.set_health(health, Some(detail));
    }

    fn check_cyclic(&mut self, now: Instant) {
        match self.mode {
            Mode::Running if now.duration_since(self.last_cyclic) > self.t.cyclic_gap => {
                tracing::warn!("cyclic reports stopped; waiting (no automatic reset)");
                self.mode = Mode::Stalled;
                self.set_health(
                    Health::Stalled,
                    Some("cyclic reports stopped; settings are refused until they return".into()),
                );
                self.park_requests();
            }
            Mode::Recovering(deadline) if now >= deadline => self.give_up(),
            _ => {}
        }
    }

    /// Before a reset or while stalled: sync gets restart later, others
    /// are kept.
    fn park_requests(&mut self) {
        if let Some(f) = self.in_flight.take()
            && f.purpose == Purpose::Verify
        {
            self.queue.push_front(Req::Get(f.get, f.purpose));
        }
        self.queue
            .retain(|r| !matches!(r, Req::Get(_, Purpose::Sync)));
        self.sync = None;
    }

    /// User-requested USB reset (`Op::Reset`). The answer goes out when
    /// the resync after it finished (or the reset failed).
    fn start_reset(&mut self, force: bool, tx: oneshot::Sender<Result<(), Error>>) {
        if !force && let Some(why) = (self.audio_busy)() {
            let _ = tx.send(Err(Error::ResetBlocked(why)));
            return;
        }
        if matches!(self.mode, Mode::Resetting | Mode::Recovering(_)) {
            self.sync_waiters.push(tx);
            return;
        }
        tracing::warn!("USB reset requested");
        self.set_health(Health::Syncing, Some("USB reset requested".into()));
        self.park_requests();
        if matches!(self.mode, Mode::Hung | Mode::GaveUp) {
            self.queue.clear();
        }
        self.mode = Mode::Resetting;
        self.sync_waiters.push(tx);
        self.io.send(IoCmd::Reset);
    }

    fn give_up(&mut self) {
        self.halt(
            Mode::GaveUp,
            Health::NoCyclic,
            "no cyclic reports after a reset; power-cycle the Orion".into(),
            || Error::NoCyclic,
        );
    }

    /// `Op::Recheck`: two probes in a row (a poisoned device answers only
    /// the first), then a resync. From `Hung` this is the soft recovery:
    /// reads only, nothing that could disturb audio.
    fn start_recheck(&mut self, tx: oneshot::Sender<Result<(), Error>>) {
        match self.mode {
            Mode::GaveUp => {
                let _ = tx.send(Err(Error::NoCyclic));
                return;
            }
            Mode::Stalled => {
                let _ = tx.send(Err(Error::Stalled));
                return;
            }
            Mode::Resetting | Mode::Recovering(_) => {
                self.sync_waiters.push(tx);
                return;
            }
            Mode::Hung => {
                tracing::info!("recheck: probing the hung control channel");
                self.mode = Mode::Running;
            }
            Mode::Running => {}
        }
        self.set_health(
            Health::Syncing,
            Some("rechecking the control channel".into()),
        );
        self.park_requests();
        self.recheck = 2;
        for _ in 0..2 {
            self.queue
                .push_front(Req::Get(Get::Firmware, Purpose::Probe));
        }
        self.sync_waiters.push(tx);
    }

    /// A verify get failed (0x61 or undecodable): stop waiting for it, so
    /// its sets no longer overlay device data.
    fn verify_failed(&mut self, get: &Get) {
        self.entries
            .retain(|_, e| !(e.verify_req.is_some() && cover(&e.set) == Some(Cover::Get(*get))));
    }

    fn disconnected(&mut self, why: String) {
        tracing::warn!("transport: {why}");
        self.set_health(Health::Disconnected, Some(why));
        for w in self.sync_waiters.drain(..) {
            let _ = w.send(Err(Error::Disconnected));
        }
        self.finished = true;
    }

    fn check_entries(&mut self, now: Instant) {
        let mut verify_status = Vec::new();
        let mut verify_gets = Vec::new();
        let mut drop_keys = Vec::new();
        for (k, e) in self.entries.iter_mut() {
            // Coalesced send due.
            if !e.queued
                && e.written_seq != e.seq
                && e.last_write.is_none_or(|lw| now >= lw + self.t.coalesce)
            {
                self.queue.push_back(Req::Write(*k));
                e.queued = true;
            }
            // Read-back due.
            if let Some(x) = e.verify_at
                && now >= x
                && !e.queued
                && e.written_seq == e.seq
            {
                e.verify_at = None;
                if !self.verify {
                    drop_keys.push(*k);
                    continue;
                }
                match cover(&e.set) {
                    None => drop_keys.push(*k),
                    Some(Cover::Status) => verify_status.push(*k),
                    Some(Cover::Get(g)) => {
                        e.verify_req = Some(e.seq);
                        verify_gets.push(g);
                    }
                }
            }
        }
        for k in drop_keys {
            self.entries.remove(&k);
        }
        for g in verify_gets {
            let pending = self
                .queue
                .iter()
                .any(|r| matches!(r, Req::Get(q, Purpose::Verify) if *q == g))
                || self
                    .in_flight
                    .as_ref()
                    .is_some_and(|f| f.get == g && f.purpose == Purpose::Verify);
            if !pending {
                self.queue.push_back(Req::Get(g, Purpose::Verify));
            }
        }
        if !verify_status.is_empty() {
            self.verify_status(&verify_status);
        }
    }

    fn drive_queue(&mut self) {
        while self.can_send() && self.in_flight.is_none() {
            if self.write_ready_at().is_some_and(|x| Instant::now() < x) {
                return;
            }
            let Some(req) = self.queue.pop_front() else {
                return;
            };
            match req {
                Req::Write(k) => {
                    let Some(e) = self.entries.get_mut(&k) else {
                        continue;
                    };
                    e.queued = false;
                    if e.written_seq == e.seq {
                        continue;
                    }
                    let now = Instant::now();
                    e.written_seq = e.seq;
                    e.last_write = Some(now);
                    e.verify_at = Some(now + self.t.verify_delay);
                    match e.set.report() {
                        Ok(r) => self.write(r),
                        Err(err) => tracing::error!("set {:?} failed to encode: {err}", e.set),
                    }
                }
                Req::WriteSet(s) => match s.report() {
                    Ok(r) => self.write(r),
                    Err(err) => tracing::error!("set {s:?} failed to encode: {err}"),
                },
                Req::Get(g, purpose) => {
                    self.write(g.report());
                    self.in_flight = Some(InFlight {
                        get: g,
                        purpose,
                        deadline: Instant::now() + self.t.request_timeout,
                        retried: false,
                    });
                }
            }
        }
    }

    // -- sync ------------------------------------------------------------------

    fn sync_gets() -> Vec<Get> {
        let mut v = vec![Get::Firmware, Get::Passport, Get::Registration];
        v.extend(OutputBank::ALL.iter().map(|&b| Get::Routing(b)));
        v.extend((0..4).map(|m| Get::Mixer(MixerId::new(m).expect("mixer id"))));
        v.extend([
            Get::MixerLinks,
            Get::AfxOrder,
            Get::AfxLinks,
            // The AFX config gets are inserted after this reply.
            Get::AfxInstances,
            Get::Reverb,
            Get::Trims,
            Get::HdxDelays,
        ]);
        v
    }

    fn start_sync(&mut self) {
        if matches!(self.mode, Mode::Hung | Mode::GaveUp) {
            return;
        }
        if let Some(s) = &mut self.sync {
            s.again = true;
            return;
        }
        self.resync_at = None;
        self.set_health(Health::Syncing, None);
        let gets = Self::sync_gets();
        self.sync = Some(SyncRun {
            outstanding: gets.len(),
            again: false,
        });
        for g in gets {
            self.queue.push_back(Req::Get(g, Purpose::Sync));
        }
    }

    /// Resync trigger from the status stream.
    fn request_resync(&mut self, why: &str) {
        if self.resync_at.is_some() {
            return;
        }
        tracing::info!("resync: {why}");
        self.start_sync();
    }

    fn sync_get_done(&mut self) {
        let Some(s) = &mut self.sync else {
            return;
        };
        s.outstanding = s.outstanding.saturating_sub(1);
        if s.outstanding > 0 {
            return;
        }
        let again = s.again;
        self.sync = None;
        if again {
            self.start_sync();
            return;
        }
        let state = {
            let mut sh = self.shared();
            sh.synced = true;
            sh.state.clone()
        };
        self.health_tx.send_modify(|h| h.syncs += 1);
        self.set_health(Health::Ok, None);
        self.emit(Event::Synced(Box::new(state)));
        for w in self.sync_waiters.drain(..) {
            let _ = w.send(Ok(()));
        }
    }

    // -- incoming --------------------------------------------------------------

    fn on_io(&mut self, ev: IoEvent) {
        match ev {
            IoEvent::Error(e) => self.disconnected(e),
            IoEvent::ResetDone(Ok(())) => {
                if self.mode == Mode::Resetting {
                    self.mode = Mode::Recovering(Instant::now() + self.t.reset_cyclic);
                }
            }
            IoEvent::ResetDone(Err(e)) => {
                tracing::warn!("reset failed: {e}");
                self.give_up();
            }
            IoEvent::Report(r) => self.on_report(&r),
        }
    }

    fn cyclic_seen(&mut self) {
        self.last_cyclic = Instant::now();
        if matches!(self.mode, Mode::Stalled | Mode::Recovering(_)) {
            tracing::info!("cyclic reports back; resyncing");
            self.mode = Mode::Running;
            // Sync gets queued meanwhile would count against the new run.
            self.park_requests();
            self.start_sync();
        }
    }

    fn on_report(&mut self, r: &[u8; REPORT_LEN]) {
        match parse(r) {
            Incoming::Status(s, m) => {
                self.cyclic_seen();
                self.on_status(s);
                self.on_meters(&m);
            }
            Incoming::AfxMeters(m) => {
                self.cyclic_seen();
                if let Some(acc) = &mut self.afx_acc {
                    for (a, b) in acc.peak.iter_mut().zip(m.peak.iter()) {
                        for (x, y) in a.iter_mut().zip(b) {
                            *x = (*x).min(*y);
                        }
                    }
                    acc.gain_reduction = m.gain_reduction;
                } else {
                    self.afx_acc = Some(*m);
                }
            }
            Incoming::Reply { ext2, ext3, body } => {
                let Some(f) = &self.in_flight else {
                    tracing::debug!("unsolicited reply {ext2}/{ext3}");
                    return;
                };
                if f.get.ext2() != ext2 || f.get.ext3() != ext3 {
                    tracing::debug!("stale reply {ext2}/{ext3}");
                    return;
                }
                let f = self.in_flight.take().expect("in flight");
                self.last_answer = Instant::now();
                self.unknown_streak = 0;
                self.on_reply(f.get, f.purpose, &body);
            }
            Incoming::UnknownCmd => {
                let Some(f) = self.in_flight.take() else {
                    return;
                };
                if f.get == Get::Firmware {
                    // Every firmware knows this get: the device is still
                    // booting, or its request parser lost step.
                    self.unknown_streak += 1;
                    if self.unknown_streak >= UNKNOWN_LIMIT {
                        tracing::warn!("get firmware answered 0x61 {UNKNOWN_LIMIT} times");
                        self.hung("the Orion answers requests with 'unknown command' (0x61)");
                        return;
                    }
                    tracing::info!("get firmware answered 0x61; still booting? asking again");
                    self.hold_until = Some(Instant::now() + self.t.unknown_backoff);
                    self.queue.push_front(Req::Get(f.get, f.purpose));
                    return;
                }
                tracing::warn!("device does not know get {:?}", f.get);
                match f.purpose {
                    Purpose::Sync => self.sync_get_done(),
                    Purpose::Verify => self.verify_failed(&f.get),
                    Purpose::Probe | Purpose::Watch => {}
                }
            }
            Incoming::Segment { .. } => tracing::warn!("segmented reply ignored"),
            Incoming::Zero | Incoming::Other(_) => {}
        }
    }

    fn on_reply(&mut self, get: Get, purpose: Purpose, body: &[u8]) {
        if purpose == Purpose::Probe {
            if self.recheck > 0 {
                self.recheck -= 1;
                if self.recheck == 0 {
                    tracing::info!("recheck: control channel answers; resyncing");
                    self.start_sync();
                }
            }
            return;
        }
        match decode_reply(&get, body) {
            Err(e) => {
                tracing::warn!("reply to {get:?} failed to decode: {e}");
                if purpose == Purpose::Verify {
                    self.verify_failed(&get);
                }
            }
            Ok(reply) if purpose == Purpose::Watch => self.on_watch(get, reply),
            Ok(reply) => {
                let instances = match &reply {
                    GetReply::AfxInstances(v) => Some(v.clone()),
                    _ => None,
                };
                let mut sh = lock_shared(&self.shared);
                let before = (purpose == Purpose::Verify).then(|| region(&get, &sh.state));
                apply_reply(&mut sh.state, reply);
                let mut done = Vec::new();
                for (k, e) in &self.entries {
                    if cover(&e.set) != Some(Cover::Get(get)) {
                        continue;
                    }
                    if purpose == Purpose::Verify
                        && e.verify_req == Some(e.seq)
                        && e.written_seq == e.seq
                        && !e.queued
                    {
                        done.push(*k);
                    } else {
                        overlay(&mut sh.state, &e.set);
                    }
                }
                let after = before.as_ref().map(|_| region(&get, &sh.state));
                drop(sh);
                for k in done {
                    self.entries.remove(&k);
                }
                if let (Some((path, ours)), Some((_, device))) = (before, after)
                    && ours != device
                {
                    tracing::warn!("read-back mismatch at {path}");
                    self.emit(Event::Mismatch { path, ours, device });
                }
                if purpose == Purpose::Sync
                    && let Some(v) = instances
                {
                    let gets = Self::afx_config_gets(v);
                    if let Some(s) = &mut self.sync {
                        s.outstanding += gets.len();
                    }
                    for g in gets.into_iter().rev() {
                        self.queue.push_front(Req::Get(g, Purpose::Sync));
                    }
                }
            }
        }
        if purpose == Purpose::Sync {
            self.sync_get_done();
        }
    }

    /// An idle re-read: resync when the device holds something else than
    /// the cache. Blocks with a pending write are skipped (the overlay
    /// differs on purpose until the write is verified).
    fn on_watch(&mut self, get: Get, reply: GetReply) {
        if self
            .entries
            .values()
            .any(|e| cover(&e.set) == Some(Cover::Get(get)))
        {
            return;
        }
        let changed = {
            let sh = lock_shared(&self.shared);
            let mut fresh = sh.state.clone();
            apply_reply(&mut fresh, reply);
            fresh != sh.state
        };
        if changed {
            self.request_resync(&format!("{get:?} changed on the device"));
        }
    }

    fn on_status(&mut self, dev: Box<Status>) {
        let prev = self.device_status.replace(dev.clone());
        if let Some(p) = prev {
            if !p.device_updated && dev.device_updated {
                self.request_resync("device_updated set");
            } else if dev.current_preset != p.current_preset && dev.current_preset != 0 {
                let ours = self.our_save.is_some_and(|(slot, at)| {
                    slot == dev.current_preset && at.elapsed() < Duration::from_secs(5)
                });
                if !ours {
                    self.request_resync("preset changed on the device");
                }
            }
        }
        self.refresh_status();
    }

    /// Cache status = device status + overlay; publish on change.
    fn refresh_status(&mut self) {
        let Some(dev) = self.device_status.clone() else {
            return;
        };
        let st = {
            let mut sh = lock_shared(&self.shared);
            sh.state.status = *dev;
            for e in self.entries.values() {
                if cover(&e.set) == Some(Cover::Status) {
                    overlay(&mut sh.state, &e.set);
                }
            }
            sh.state.status.clone()
        };
        if self.published_status.as_ref() != Some(&st) {
            self.published_status = Some(st.clone());
            self.emit(Event::Status(Box::new(st)));
        }
    }

    fn verify_status(&mut self, keys: &[Key]) {
        let Some(dev) = self.device_status.clone() else {
            return;
        };
        for k in keys {
            let Some(e) = self.entries.remove(k) else {
                continue;
            };
            let mut tmp = self.shared().state.clone();
            tmp.status = (*dev).clone();
            let _ = apply_set(&mut tmp, &e.set);
            tmp.status.current_preset = dev.current_preset;
            if tmp.status != *dev {
                tracing::warn!("status read-back mismatch for {k:?}");
                self.emit(Event::Mismatch {
                    path: "/status".into(),
                    ours: serde_json::to_value(&tmp.status).unwrap_or(Value::Null),
                    device: serde_json::to_value(&*dev).unwrap_or(Value::Null),
                });
            }
        }
        self.refresh_status();
    }

    fn on_meters(&mut self, m: &Meters) {
        match &mut self.meter_acc {
            None => self.meter_acc = Some(m.clone()),
            Some(a) => {
                let min = |x: &mut [u8], y: &[u8]| {
                    for (a, b) in x.iter_mut().zip(y) {
                        *a = (*a).min(*b);
                    }
                };
                min(&mut a.bank_a, &m.bank_a);
                min(&mut a.bank_b, &m.bank_b);
                min(&mut a.mixer, &m.mixer);
                min(&mut a.reverb_in, &m.reverb_in);
                min(&mut a.reverb_out, &m.reverb_out);
            }
        }
    }

    fn flush_meters(&mut self) {
        if let Some(a) = self.afx_acc.take() {
            self.last_afx = a;
        }
        if let Some(m) = self.meter_acc.take() {
            self.emit(Event::Meters {
                meters: Box::new(m),
                afx: Box::new(self.last_afx.clone()),
            });
        }
    }

    // -- ops ---------------------------------------------------------------------

    fn on_op(&mut self, op: Op, tx: oneshot::Sender<Result<(), Error>>) {
        if let Op::Reset { force } = op {
            self.start_reset(force, tx);
            return;
        }
        if op == Op::Recheck {
            self.start_recheck(tx);
            return;
        }
        match self.mode {
            Mode::Hung => {
                let _ = tx.send(Err(Error::Hung));
                return;
            }
            Mode::GaveUp => {
                let _ = tx.send(Err(Error::NoCyclic));
                return;
            }
            // Nothing can be sent or read back; queuing would only pile up.
            Mode::Stalled => {
                let _ = tx.send(Err(Error::Stalled));
                return;
            }
            _ => {}
        }
        if op == Op::Resync {
            self.sync_waiters.push(tx);
            self.start_sync();
            return;
        }
        let _ = tx.send(self.apply_op(&op));
    }

    fn apply_op(&mut self, op: &Op) -> Result<(), Error> {
        let now = Instant::now();
        let mut diffs = Vec::new();
        let sets = {
            let mut sh = lock_shared(&self.shared);
            if !sh.synced {
                return Err(Error::NotSynced("first sync still running".into()));
            }
            let sets = build_sets(&sh.state, op)?;
            // Validate everything before touching the cache.
            let mut tmp = sh.state.clone();
            for s in &sets {
                apply_set(&mut tmp, s)?;
            }
            for s in &sets {
                apply_set(&mut sh.state, s)?;
                if let Some(d) = set_path(s, &sh.state) {
                    diffs.push(d);
                }
            }
            sets
        };
        for (path, value) in diffs {
            self.emit(Event::Diff { path, value });
        }
        for s in sets {
            match &s {
                Set::PresetRecall(_) => self.resync_at = Some(now + self.t.recall_settle),
                Set::PresetSave(slot) => self.our_save = Some((slot.get(), now)),
                _ => {}
            }
            let Some(k) = key(&s) else {
                self.queue.push_back(Req::WriteSet(s));
                continue;
            };
            let cont = build::continuous(k);
            let e = self.entries.entry(k).or_insert_with(|| Entry {
                set: s.clone(),
                seq: 0,
                written_seq: 0,
                queued: false,
                last_write: None,
                verify_at: None,
                verify_req: None,
            });
            e.set = s;
            e.seq += 1;
            e.verify_at = None;
            if !e.queued {
                let ready = !cont || e.last_write.is_none_or(|lw| now >= lw + self.t.coalesce);
                if ready {
                    self.queue.push_back(Req::Write(k));
                    e.queued = true;
                }
            }
        }
        if self
            .entries
            .values()
            .any(|e| cover(&e.set) == Some(Cover::Status))
        {
            self.refresh_status();
        }
        Ok(())
    }
}
