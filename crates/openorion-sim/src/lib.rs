//! Simulated Orion32 HD (DESIGN.md sec. 10). WP5.
//!
//! `SimTransport` implements `openorion_hid::Transport` from a `DeviceState`
//! (usually `fixtures/dump1.json`). It answers gets, applies sets, emits
//! the 0x73/0x83 cyclic reports with synthetic meters and models the
//! quirks in protocol-spec.md sec. 1 and 5:
//!
//! - get reply ~28 ms after the request; a set changes state ~25 ms later
//!   and is never answered; ext2 17 or an unknown get → 0x61;
//! - 0x73 every 8 ms, 0x83 ~4 ms after it;
//! - a write that is not 320 bytes (only possible through `write_raw`)
//!   poisons the control channel until `power_cycle()`;
//! - a recall sets `device_updated` and `current_preset`; any other set
//!   clears `current_preset` (via `openorion_proto::decode::apply_set`, which
//!   also disables an AFX instance removed from its chain).
//!
//! Time is either virtual (`SimClock::Virtual`, the default: `read`
//! advances it by the time it would have waited, so tests are exact and
//! instant) or real (`SimClock::RealTime`, for the daemon and UI demo).
//! No threads are spawned; everything is generated lazily in `read`.
//!
//! `SimHandle` is a cheap clone of the shared simulator, kept by tests or
//! the daemon after the transport has moved into the core: fault
//! injection, the write log and the device state.

mod meters;

use std::collections::BTreeMap;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use openorion_hid::Transport;
use openorion_proto::codec::{HEADER_LEN, ID_GET, ID_SET, ID_UNKNOWN_CMD};
use openorion_proto::decode::{
    AFX_CONFIG_ENTRIES, apply_set, default_afx_config, encode_afx_meters, encode_reply,
    encode_status,
};
use openorion_proto::{AfxType, DeviceState, Get, REPORT_LEN, Report, Set};

pub use meters::{afx_meters_at, meters_at};

/// Cyclic report period (0x73 and 0x83 each).
pub const CYCLIC_PERIOD: Duration = Duration::from_millis(8);
/// Offset of each 0x83 after its 0x73.
pub const AFX_METER_OFFSET: Duration = Duration::from_millis(4);
/// Default request → reply latency.
pub const REPLY_LATENCY: Duration = Duration::from_millis(28);
/// Default set → state change latency.
pub const SET_LATENCY: Duration = Duration::from_millis(25);
/// After a reset: the all-zero report comes this much later, the first
/// 0x73 one `CYCLIC_PERIOD` later.
pub const RESET_ZERO_DELAY: Duration = Duration::from_millis(1);
/// Cyclic reports older than this are dropped when nobody reads (hidraw
/// keeps a bounded queue). Replies are never dropped.
pub const CYCLIC_BACKLOG: Duration = Duration::from_millis(128);
/// Longest single sleep in real-time mode, so handle changes are seen.
const REAL_SLEEP_MAX: Duration = Duration::from_millis(4);
/// Preset slots 1..=5.
pub const PRESET_SLOTS: usize = 5;

/// Path of a file in the repository's `fixtures/` directory.
pub fn fixture_path(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../fixtures")
        .join(name)
}

/// How sim time advances.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SimClock {
    /// Starts at 0; `read(timeout)` jumps to the next event or by the
    /// whole timeout. `SimHandle::advance` moves it by hand.
    #[default]
    Virtual,
    /// Wall clock; `read` sleeps.
    RealTime,
}

/// One write the simulator received, in order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WriteRecord {
    /// Sim time of the write.
    pub at: Duration,
    /// The report as written (320 bytes unless it came from `write_raw`).
    pub bytes: Vec<u8>,
}

impl WriteRecord {
    fn id(&self) -> Option<u32> {
        (self.bytes.len() >= 4).then(|| le32(&self.bytes, 0))
    }

    /// The set this write carried, if it was a valid 320-byte set.
    pub fn set(&self) -> Option<Set> {
        let r: &[u8; REPORT_LEN] = self.bytes.as_slice().try_into().ok()?;
        (self.id() == Some(ID_SET)).then(|| Set::decode(r).ok())?
    }

    /// `(ext2, ext3)` if this write was a 320-byte get.
    pub fn get_ext(&self) -> Option<(u32, u32)> {
        (self.bytes.len() == REPORT_LEN && self.id() == Some(ID_GET))
            .then(|| (le32(&self.bytes, 8), le32(&self.bytes, 12)))
    }
}

/// Cyclic stream state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Cyclic {
    Flowing,
    /// Asleep: nothing at all until `reset()`.
    WaitReset,
    /// Unplugged: nothing until `replug()`; `reset()` fails.
    Dropped,
}

#[derive(Debug)]
enum Event {
    /// A ready report (all-zero after reset, 0x61).
    Out(Box<[u8; REPORT_LEN]>),
    /// Answer this get from the state at fire time.
    Reply(Get),
    /// Apply this set (silent).
    Apply(Set),
}

struct Sim {
    clock: SimClock,
    start: Instant,
    vnow: Duration,
    state: DeviceState,
    presets: [DeviceState; PRESET_SLOTS],
    /// (due, seq, event); kept sorted by (due, seq).
    queue: Vec<(Duration, u64, Event)>,
    seq: u64,
    next_status: Duration,
    next_afx: Duration,
    cyclic: Cyclic,
    /// Control channel stuck (hung or poisoned): requests are accepted
    /// and dropped, sets not applied. Cleared by `power_cycle`.
    stuck: bool,
    /// Stuck because of a bad write (as opposed to `hang_after`).
    poisoned: bool,
    /// While stuck: requests still answered (a reset gives one).
    stuck_answers: u32,
    /// `hang_after(n)`: answers left before the channel hangs.
    hang_budget: Option<u64>,
    /// `booting(n)`: requests left that get 0x61 (as right after power-up).
    unknown_left: u32,
    answered: u64,
    reply_latency: Duration,
    set_latency: Duration,
    panic_on_bad_write: bool,
    writes: Vec<WriteRecord>,
    rejected: Vec<String>,
}

fn le32(b: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(b[at..at + 4].try_into().unwrap())
}

fn unknown_cmd(ext2: u32, ext3: u32) -> [u8; REPORT_LEN] {
    let mut r = [0u8; REPORT_LEN];
    r[0..4].copy_from_slice(&ID_UNKNOWN_CMD.to_le_bytes());
    r[4..8].copy_from_slice(&(HEADER_LEN as u32).to_le_bytes());
    r[8..12].copy_from_slice(&ext2.to_le_bytes());
    r[12..16].copy_from_slice(&ext3.to_le_bytes());
    r
}

/// Give every on-Orion AFX type a full config list (factory defaults where
/// missing), as the device has. A dump without 0x07 replies has none, and
/// `apply_set` only disables a removed instance whose config exists.
fn fill_afx_configs(state: &mut DeviceState) {
    for &ty in AfxType::ALL.iter().filter(|t| t.on_orion()) {
        let Some(dflt) = default_afx_config(ty) else {
            continue;
        };
        let v = state.afx.configs.entry(ty).or_default();
        if v.len() < AFX_CONFIG_ENTRIES {
            v.resize(AFX_CONFIG_ENTRIES, dflt);
        }
    }
}

impl Sim {
    fn new(mut state: DeviceState, clock: SimClock) -> Sim {
        fill_afx_configs(&mut state);
        Sim {
            clock,
            start: Instant::now(),
            vnow: Duration::ZERO,
            presets: std::array::from_fn(|_| state.clone()),
            state,
            queue: Vec::new(),
            seq: 0,
            next_status: Duration::ZERO,
            next_afx: AFX_METER_OFFSET,
            cyclic: Cyclic::Flowing,
            stuck: false,
            poisoned: false,
            stuck_answers: 0,
            hang_budget: None,
            unknown_left: 0,
            answered: 0,
            reply_latency: REPLY_LATENCY,
            set_latency: SET_LATENCY,
            panic_on_bad_write: true,
            writes: Vec::new(),
            rejected: Vec::new(),
        }
    }

    fn now(&self) -> Duration {
        match self.clock {
            SimClock::Virtual => self.vnow,
            SimClock::RealTime => self.start.elapsed(),
        }
    }

    fn push(&mut self, due: Duration, ev: Event) {
        let seq = self.seq;
        self.seq += 1;
        let i = self
            .queue
            .partition_point(|(d, s, _)| (*d, *s) <= (due, seq));
        self.queue.insert(i, (due, seq, ev));
    }

    /// Skip cyclic reports nobody read for longer than `CYCLIC_BACKLOG`.
    fn catch_up(&mut self, now: Duration) {
        if self.cyclic != Cyclic::Flowing {
            return;
        }
        let Some(oldest) = now.checked_sub(CYCLIC_BACKLOG) else {
            return;
        };
        let p = CYCLIC_PERIOD.as_nanos();
        for next in [&mut self.next_status, &mut self.next_afx] {
            if *next < oldest {
                let k = (oldest - *next).as_nanos().div_ceil(p);
                *next += Duration::from_nanos((k * p) as u64);
            }
        }
    }

    /// Due time of the next event, if any.
    fn next_due(&self) -> Option<Duration> {
        let q = self.queue.first().map(|(d, _, _)| *d);
        let c = (self.cyclic == Cyclic::Flowing).then(|| self.next_status.min(self.next_afx));
        match (q, c) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        }
    }

    /// Fire the earliest event. Returns the report it produced, if any.
    fn fire(&mut self) -> Option<[u8; REPORT_LEN]> {
        let q = self.queue.first().map(|(d, _, _)| *d);
        let flowing = self.cyclic == Cyclic::Flowing;
        let cyc = flowing.then(|| self.next_status.min(self.next_afx));
        let take_queue = match (q, cyc) {
            (Some(a), Some(b)) => a <= b,
            (Some(_), None) => true,
            (None, Some(_)) => false,
            (None, None) => return None,
        };
        if take_queue {
            let (_, _, ev) = self.queue.remove(0);
            return match ev {
                Event::Out(r) => Some(*r),
                Event::Reply(g) => Some(
                    encode_reply(&g, &self.state)
                        .unwrap_or_else(|_| unknown_cmd(g.ext2(), g.ext3())),
                ),
                Event::Apply(s) => {
                    self.apply(&s);
                    None
                }
            };
        }
        let power = self.state.status.power_on;
        if self.next_status <= self.next_afx {
            let t = self.next_status;
            self.next_status += CYCLIC_PERIOD;
            Some(encode_status(&self.state.status, &meters_at(t, power)))
        } else {
            let t = self.next_afx;
            self.next_afx += CYCLIC_PERIOD;
            Some(encode_afx_meters(&afx_meters_at(t, power)))
        }
    }

    /// Apply every pending set due by `now` (no reports are produced).
    fn apply_due_sets(&mut self, now: Duration) {
        while let Some(i) = self
            .queue
            .iter()
            .position(|(d, _, e)| *d <= now && matches!(e, Event::Apply(_)))
        {
            if let (_, _, Event::Apply(s)) = self.queue.remove(i) {
                self.apply(&s);
            }
        }
    }

    fn apply(&mut self, set: &Set) {
        match set {
            Set::PresetSave(slot) => {
                self.presets[slot.get() as usize - 1] = self.state.clone();
                self.state.status.current_preset = slot.get();
            }
            Set::PresetRecall(slot) => {
                let p = &self.presets[slot.get() as usize - 1];
                let s = &mut self.state;
                s.routing = p.routing.clone();
                s.mixers = p.mixers.clone();
                s.mixer_links = p.mixer_links;
                s.trims = p.trims.clone();
                s.reverb = p.reverb;
                s.afx = p.afx.clone();
                s.hdx_delays = p.hdx_delays;
                s.status.current_preset = slot.get();
                s.status.device_updated = true;
            }
            _ => {
                if let Err(e) = apply_set(&mut self.state, set) {
                    self.rejected.push(format!("apply {set:?}: {e}"));
                }
            }
        }
    }

    /// Whether the device answers a request now (and consume budgets).
    fn may_answer(&mut self) -> bool {
        if self.cyclic != Cyclic::Flowing {
            return false;
        }
        if self.stuck {
            if self.stuck_answers == 0 {
                return false;
            }
            self.stuck_answers -= 1;
        }
        true
    }

    fn answered_one(&mut self) {
        self.answered += 1;
        if let Some(b) = &mut self.hang_budget {
            *b = b.saturating_sub(1);
            if *b == 0 {
                self.hang_budget = None;
                self.stuck = true;
            }
        }
    }

    /// Handle one 320-byte report from the host.
    fn receive(&mut self, r: &[u8; REPORT_LEN]) {
        let now = self.now();
        self.writes.push(WriteRecord {
            at: now,
            bytes: r.to_vec(),
        });
        let id = le32(r, 0);
        let (ext2, ext3) = (le32(r, 8), le32(r, 12));
        if id == ID_SET {
            if self.cyclic != Cyclic::Flowing || (self.stuck && self.stuck_answers == 0) {
                return;
            }
            match Set::decode(r) {
                Ok(s) => self.push(now + self.set_latency, Event::Apply(s)),
                Err(e) => self.rejected.push(format!("set decode: {e}")),
            }
            return;
        }
        if !self.may_answer() {
            return;
        }
        let due = now + self.reply_latency;
        match (id, Get::from_ext(ext2, ext3)) {
            _ if self.unknown_left > 0 => {
                self.unknown_left -= 1;
                self.push(due, Event::Out(Box::new(unknown_cmd(ext2, ext3))))
            }
            (ID_GET, Some(g)) if ext2 != 17 => self.push(due, Event::Reply(g)),
            _ => self.push(due, Event::Out(Box::new(unknown_cmd(ext2, ext3)))),
        }
        self.answered_one();
    }

    fn reset(&mut self) -> io::Result<()> {
        if self.cyclic == Cyclic::Dropped {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                "simulated Orion unplugged",
            ));
        }
        let now = self.now();
        self.queue.retain(|(_, _, e)| matches!(e, Event::Apply(_)));
        self.push(
            now + RESET_ZERO_DELAY,
            Event::Out(Box::new([0; REPORT_LEN])),
        );
        self.cyclic = Cyclic::Flowing;
        self.next_status = now + RESET_ZERO_DELAY + CYCLIC_PERIOD;
        self.next_afx = self.next_status + AFX_METER_OFFSET;
        if self.stuck {
            self.stuck_answers = 1;
        }
        Ok(())
    }
}

/// Simulated device implementing `Transport`. `Send`; the shared state is
/// reachable through `handle()`.
pub struct SimTransport {
    inner: Arc<Mutex<Sim>>,
}

/// Shared control of a `SimTransport`: fault injection and inspection.
/// Cheap to clone; stays valid after the transport is moved.
#[derive(Clone)]
pub struct SimHandle {
    inner: Arc<Mutex<Sim>>,
}

fn lock(m: &Mutex<Sim>) -> MutexGuard<'_, Sim> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

fn hex(s: &str) -> io::Result<Vec<u8>> {
    let s: String = s.chars().filter(|c| !c.is_whitespace()).collect();
    if !s.len().is_multiple_of(2) {
        return Err(bad_data(format!("odd-length hex ({} chars)", s.len())));
    }
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).map_err(|e| bad_data(e.to_string())))
        .collect()
}

fn bad_data(msg: String) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, msg)
}

/// Parse an `orionread.py` dump (`fixtures/dump1.json` format) into a
/// `DeviceState`.
pub fn state_from_dump_json(text: &str) -> io::Result<DeviceState> {
    let v: serde_json::Value =
        serde_json::from_str(text).map_err(|e| bad_data(format!("dump json: {e}")))?;
    let mut replies = BTreeMap::new();
    if let Some(obj) = v["replies"].as_object() {
        for (k, val) in obj {
            let (a, b) = k
                .split_once('/')
                .ok_or_else(|| bad_data(format!("dump key {k:?}")))?;
            let key = (
                a.parse().map_err(|_| bad_data(format!("dump key {k:?}")))?,
                b.parse().map_err(|_| bad_data(format!("dump key {k:?}")))?,
            );
            let bytes = match val.as_str() {
                Some(s) => hex(s)?,
                None => Vec::new(),
            };
            replies.insert(key, bytes);
        }
    }
    let status = match v["cyclic"]["0x73"].as_str() {
        Some(s) => hex(s)?,
        None => Vec::new(),
    };
    openorion_proto::decode::state_from_dump(&replies, &status)
        .map_err(|e| bad_data(format!("dump: {e}")))
}

impl SimTransport {
    /// A simulator in virtual time with this device state. Missing AFX
    /// config lists are filled with factory defaults.
    pub fn from_state(state: DeviceState) -> SimTransport {
        SimTransport {
            inner: Arc::new(Mutex::new(Sim::new(state, SimClock::Virtual))),
        }
    }

    /// Seed from an `orionread.py` dump file (e.g. `fixtures/dump1.json`).
    pub fn from_dump(path: impl AsRef<Path>) -> io::Result<SimTransport> {
        let text = std::fs::read_to_string(path)?;
        Ok(SimTransport::from_state(state_from_dump_json(&text)?))
    }

    /// Seeded from the repository's `fixtures/dump1.json`.
    pub fn fixture() -> io::Result<SimTransport> {
        SimTransport::from_dump(fixture_path("dump1.json"))
    }

    /// Choose the clock (resets sim time to 0).
    pub fn with_clock(self, clock: SimClock) -> SimTransport {
        {
            let mut s = lock(&self.inner);
            s.clock = clock;
            s.start = Instant::now();
            s.vnow = Duration::ZERO;
        }
        self
    }

    pub fn handle(&self) -> SimHandle {
        SimHandle {
            inner: self.inner.clone(),
        }
    }

    /// Test hook: write arbitrary bytes as one report. Anything but 320
    /// bytes poisons the control channel (until `power_cycle`) and then
    /// panics, unless `SimHandle::set_panic_on_bad_write(false)`.
    pub fn write_raw(&mut self, bytes: &[u8]) {
        self.handle().write_raw(bytes)
    }
}

impl Transport for SimTransport {
    fn write(&mut self, r: &Report) -> io::Result<()> {
        // `Report` is always exactly 320 bytes; the guard documents the rule.
        let b: &[u8; REPORT_LEN] = r.as_bytes();
        assert_eq!(b.len(), REPORT_LEN, "non-320 write");
        lock(&self.inner).receive(b);
        Ok(())
    }

    fn read(&mut self, timeout: Duration) -> io::Result<Option<[u8; REPORT_LEN]>> {
        let deadline = lock(&self.inner).now() + timeout;
        loop {
            let mut s = lock(&self.inner);
            let now = s.now();
            s.catch_up(now);
            let next = s.next_due();
            match s.clock {
                SimClock::Virtual => match next {
                    Some(t) if t <= deadline => {
                        s.vnow = s.vnow.max(t);
                        if let Some(r) = s.fire() {
                            return Ok(Some(r));
                        }
                    }
                    _ => {
                        s.vnow = s.vnow.max(deadline);
                        return Ok(None);
                    }
                },
                SimClock::RealTime => {
                    if let Some(t) = next
                        && t <= now
                    {
                        if let Some(r) = s.fire() {
                            return Ok(Some(r));
                        }
                        continue;
                    }
                    if now >= deadline {
                        return Ok(None);
                    }
                    let until = next.unwrap_or(deadline).min(deadline);
                    let nap = (until - now).min(REAL_SLEEP_MAX);
                    drop(s);
                    std::thread::sleep(nap);
                }
            }
        }
    }

    fn reset(&mut self) -> io::Result<()> {
        lock(&self.inner).reset()
    }
}

impl SimHandle {
    fn sim(&self) -> MutexGuard<'_, Sim> {
        lock(&self.inner)
    }

    /// Current sim time.
    pub fn now(&self) -> Duration {
        self.sim().now()
    }

    /// Virtual clock only: move time forward by `d`, applying sets that
    /// fall due (reports stay queued for `read`). No-op in real time.
    pub fn advance(&self, d: Duration) {
        let mut s = self.sim();
        if s.clock == SimClock::Virtual {
            s.vnow += d;
            let now = s.vnow;
            s.apply_due_sets(now);
        }
    }

    /// The device's current state (sets not yet due are not in it).
    pub fn state(&self) -> DeviceState {
        self.sim().state.clone()
    }

    /// Stored preset slot 1..=5 (initially copies of the seed state).
    pub fn preset(&self, slot: u8) -> Option<DeviceState> {
        let i = (slot as usize).checked_sub(1)?;
        self.sim().presets.get(i).cloned()
    }

    /// Device-side change (front-panel knob, another host): mutates the
    /// state now; the next status or get reflects it.
    pub fn external_change(&self, f: impl FnOnce(&mut DeviceState)) {
        f(&mut self.sim().state)
    }

    /// After `n` more answered requests, accept requests but never answer
    /// (cyclic keeps flowing). `n = 0` hangs at once. Persists across
    /// `reset()` (which gives at most one answer) until `power_cycle()`.
    pub fn hang_after(&self, n: u64) {
        let mut s = self.sim();
        if n == 0 {
            s.hang_budget = None;
            s.stuck = true;
            s.stuck_answers = 0;
        } else {
            s.hang_budget = Some(n);
        }
    }

    /// Still booting: the next `n` requests are answered with 0x61 (seen
    /// right after a power cycle), whatever they ask.
    pub fn booting(&self, n: u32) {
        self.sim().unknown_left = n;
    }

    /// End a hang without a power cycle (a transient hang). Poison stays.
    pub fn unstick(&self) {
        let mut s = self.sim();
        if !s.poisoned {
            s.stuck = false;
            s.stuck_answers = 0;
        }
    }

    /// Asleep: no cyclic and no answers until `reset()`; the reset gives
    /// one all-zero report, then cyclic.
    pub fn no_cyclic_until_reset(&self) {
        let mut s = self.sim();
        s.cyclic = Cyclic::WaitReset;
        s.queue.retain(|(_, _, e)| matches!(e, Event::Apply(_)));
    }

    /// Unplug: cyclic and answers stop; `reset()` fails with `NotFound`
    /// until `replug()`.
    pub fn drop_cyclic(&self) {
        let mut s = self.sim();
        s.cyclic = Cyclic::Dropped;
        s.queue.clear();
    }

    /// Undo `drop_cyclic`: the device is back but asleep (like a fresh
    /// power-up: it needs a `reset()` to start cyclic).
    pub fn replug(&self) {
        let mut s = self.sim();
        if s.cyclic == Cyclic::Dropped {
            s.cyclic = Cyclic::WaitReset;
        }
    }

    /// Power cycle: clears hang and poison and anything pending. Like the
    /// real device after power-up, it stays silent until `reset()`
    /// (protocol-spec.md sec. 1, "wake"). The state and
    /// presets survive.
    pub fn power_cycle(&self) {
        let mut s = self.sim();
        s.stuck = false;
        s.poisoned = false;
        s.stuck_answers = 0;
        s.hang_budget = None;
        s.unknown_left = 0;
        s.queue.clear();
        s.state.status.device_updated = false;
        s.cyclic = Cyclic::WaitReset;
    }

    /// True while the control channel is hung or poisoned.
    pub fn is_stuck(&self) -> bool {
        self.sim().stuck
    }

    /// True after a non-320 write, until `power_cycle()`.
    pub fn is_poisoned(&self) -> bool {
        self.sim().poisoned
    }

    /// Requests answered so far (replies and 0x61).
    pub fn answered(&self) -> u64 {
        self.sim().answered
    }

    /// Default true: `write_raw` with a bad size panics after poisoning.
    pub fn set_panic_on_bad_write(&self, on: bool) {
        self.sim().panic_on_bad_write = on;
    }

    /// Override the reply and set latencies (defaults 28 / 25 ms).
    pub fn set_latencies(&self, reply: Duration, set: Duration) {
        let mut s = self.sim();
        s.reply_latency = reply;
        s.set_latency = set;
    }

    /// Everything written so far, in order.
    pub fn writes(&self) -> Vec<WriteRecord> {
        self.sim().writes.clone()
    }

    /// The decoded sets among `writes()`.
    pub fn sets(&self) -> Vec<Set> {
        self.sim().writes.iter().filter_map(|w| w.set()).collect()
    }

    pub fn clear_writes(&self) {
        self.sim().writes.clear();
    }

    /// Sets the simulator could not decode or apply (should stay empty).
    pub fn rejected(&self) -> Vec<String> {
        self.sim().rejected.clone()
    }

    /// See `SimTransport::write_raw`.
    pub fn write_raw(&self, bytes: &[u8]) {
        let panic = {
            let mut s = self.sim();
            if let Ok(r) = <&[u8; REPORT_LEN]>::try_from(bytes) {
                s.receive(r);
                return;
            }
            let at = s.now();
            s.writes.push(WriteRecord {
                at,
                bytes: bytes.to_vec(),
            });
            s.stuck = true;
            s.poisoned = true;
            s.stuck_answers = 0;
            s.queue.retain(|(_, _, e)| matches!(e, Event::Apply(_)));
            s.panic_on_bad_write
        };
        // Lock released first, so the shared state survives the panic.
        if panic {
            panic!(
                "simulated Orion: {}-byte write (must be {REPORT_LEN}); control channel poisoned",
                bytes.len()
            );
        }
    }
}
