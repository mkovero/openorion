//! A scripted fake transport for the session tests (the simulator, WP5,
//! is written in parallel). It answers gets from a `DeviceState` built
//! from `fixtures/dump1.json`, applies sets, emits cyclic 0x73/0x83 every
//! 4 ms (alternating), and has switches for the failure modes.
#![allow(dead_code)]

use std::collections::{BTreeMap, VecDeque};
use std::io;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use openorion_core::{Event, Session, SessionHandle, SessionOpts, Timings};
use openorion_hid::Transport;
use openorion_proto::codec::{ID_GET, ID_SET};
use openorion_proto::decode::{
    apply_set, encode_afx_meters, encode_reply, encode_status, state_from_dump,
};
use openorion_proto::*;
use tokio::sync::broadcast;

pub fn fixture(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../fixtures")
        .join(name)
}

fn hex(s: &str) -> Vec<u8> {
    let s: String = s.chars().filter(|c| !c.is_whitespace()).collect();
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
        .collect()
}

/// The device state of `fixtures/dump1.json`.
pub fn fixture_state() -> DeviceState {
    let text = std::fs::read_to_string(fixture("dump1.json")).unwrap();
    let v: serde_json::Value = serde_json::from_str(&text).unwrap();
    let mut replies = BTreeMap::new();
    for (k, val) in v["replies"].as_object().unwrap() {
        let (a, b) = k.split_once('/').unwrap();
        let bytes = val.as_str().map(hex).unwrap_or_default();
        replies.insert((a.parse().unwrap(), b.parse().unwrap()), bytes);
    }
    let st = v["cyclic"]["0x73"].as_str().map(hex).unwrap_or_default();
    state_from_dump(&replies, &st).unwrap()
}

/// What the fake device is and does. Tests poke these fields.
pub struct Device {
    pub state: DeviceState,
    pub meters: Meters,
    pub afx_meters: AfxMeters,
    /// Every report written, in order.
    pub writes: Vec<[u8; REPORT_LEN]>,
    pub gets: Vec<Get>,
    pub sets: Vec<Set>,
    /// False: gets go unanswered (hung control channel).
    pub answer_gets: bool,
    /// Some(n): answer n more gets, then hang ("first only" after a
    /// reset of a poisoned device).
    pub answer_budget: Option<u32>,
    /// Gets left that are answered with 0x61 (still booting).
    pub unknown_left: u32,
    /// False: sets are recorded but not applied (verify mismatch).
    pub apply_sets: bool,
    /// Cyclic reports flowing.
    pub cyclic: bool,
    /// What `cyclic` becomes after a reset.
    pub cyclic_after_reset: bool,
    pub resets: usize,
    /// Read error (device gone).
    pub gone: bool,
}

impl Device {
    pub fn writes_len(&self) -> usize {
        self.writes.len()
    }
}

#[derive(Clone)]
pub struct Fake(pub Arc<Mutex<Device>>);

impl Fake {
    pub fn new() -> Fake {
        Fake(Arc::new(Mutex::new(Device {
            state: fixture_state(),
            meters: Meters::default(),
            afx_meters: AfxMeters::default(),
            writes: Vec::new(),
            gets: Vec::new(),
            sets: Vec::new(),
            answer_gets: true,
            answer_budget: None,
            unknown_left: 0,
            apply_sets: true,
            cyclic: true,
            cyclic_after_reset: true,
            resets: 0,
            gone: false,
        })))
    }

    pub fn dev(&self) -> MutexGuard<'_, Device> {
        self.0.lock().unwrap()
    }

    pub fn transport(&self) -> Box<dyn Transport> {
        Box::new(FakeTransport {
            dev: self.0.clone(),
            out: VecDeque::new(),
            next_cyclic: Instant::now(),
            afx_phase: false,
        })
    }

    pub fn sets(&self) -> Vec<Set> {
        self.dev().sets.clone()
    }
}

pub struct FakeTransport {
    dev: Arc<Mutex<Device>>,
    out: VecDeque<[u8; REPORT_LEN]>,
    next_cyclic: Instant,
    afx_phase: bool,
}

fn le32(r: &[u8], o: usize) -> u32 {
    u32::from_le_bytes(r[o..o + 4].try_into().unwrap())
}

impl Transport for FakeTransport {
    fn write(&mut self, r: &Report) -> io::Result<()> {
        let b = *r.as_bytes();
        assert_eq!(b.len(), 320, "only 320-byte writes");
        let mut d = self.dev.lock().unwrap();
        d.writes.push(b);
        match le32(&b, 0) {
            ID_GET => {
                let get = Get::from_ext(le32(&b, 8), le32(&b, 12)).expect("known get");
                // EQ halves normalise to (Equalizer, half); keep the request.
                d.gets.push(get);
                if let Some(n) = d.answer_budget {
                    if n == 0 {
                        d.answer_gets = false;
                        d.answer_budget = None;
                    } else {
                        d.answer_budget = Some(n - 1);
                    }
                }
                if d.answer_gets && d.unknown_left > 0 {
                    d.unknown_left -= 1;
                    let mut rep = [0u8; REPORT_LEN];
                    rep[..4].copy_from_slice(&0x61u32.to_le_bytes());
                    self.out.push_back(rep);
                } else if d.answer_gets {
                    let rep = encode_reply(&get, &d.state).expect("encode reply");
                    self.out.push_back(rep);
                }
            }
            ID_SET => {
                let set = Set::decode(&b).expect("valid set");
                if d.apply_sets {
                    apply_set(&mut d.state, &set).expect("apply set");
                }
                d.sets.push(set);
            }
            id => panic!("unexpected write id {id:#x}"),
        }
        Ok(())
    }

    fn read(&mut self, timeout: Duration) -> io::Result<Option<[u8; REPORT_LEN]>> {
        let deadline = Instant::now() + timeout;
        loop {
            if let Some(r) = self.out.pop_front() {
                return Ok(Some(r));
            }
            let now = Instant::now();
            {
                let d = self.dev.lock().unwrap();
                if d.gone {
                    return Err(io::Error::other("device gone"));
                }
                if d.cyclic && now >= self.next_cyclic {
                    self.next_cyclic = now + Duration::from_millis(4);
                    self.afx_phase = !self.afx_phase;
                    let r = if self.afx_phase {
                        encode_status(&d.state.status, &d.meters)
                    } else {
                        encode_afx_meters(&d.afx_meters)
                    };
                    return Ok(Some(r));
                }
            }
            if now >= deadline {
                return Ok(None);
            }
            let until = self.next_cyclic.max(now).min(deadline);
            std::thread::sleep(
                until
                    .saturating_duration_since(now)
                    .max(Duration::from_micros(200)),
            );
        }
    }

    fn reset(&mut self) -> io::Result<()> {
        let mut d = self.dev.lock().unwrap();
        d.resets += 1;
        d.cyclic = d.cyclic_after_reset;
        self.out.clear();
        self.out.push_back([0; REPORT_LEN]);
        Ok(())
    }
}

/// Short but realistic timings for tests.
pub fn timings() -> Timings {
    Timings {
        startup_cyclic: Duration::from_millis(200),
        reset_cyclic: Duration::from_millis(400),
        request_timeout: Duration::from_millis(150),
        cyclic_gap: Duration::from_millis(400),
        coalesce: Duration::from_millis(20),
        verify_delay: Duration::from_millis(150),
        meter_period: Duration::from_millis(33),
        recall_settle: Duration::from_millis(100),
        read_poll: Duration::from_millis(2),
        write_gap: Duration::from_millis(1),
        probe_idle: Duration::from_millis(500),
        unknown_backoff: Duration::from_millis(50),
    }
}

pub struct Harness {
    pub fake: Fake,
    pub handle: SessionHandle,
    pub events: broadcast::Receiver<Event>,
    pub _dir: tempfile::TempDir,
}

pub fn opts(dir: &tempfile::TempDir) -> SessionOpts {
    SessionOpts {
        timings: timings(),
        lock_path: dir.path().join("openorion.lock"),
        verify: true,
        audio_busy: || None,
    }
}

/// Start a session on a fresh fake and wait for the first sync.
pub async fn synced() -> Harness {
    synced_with(Fake::new()).await
}

pub async fn synced_with(fake: Fake) -> Harness {
    let dir = tempfile::tempdir().unwrap();
    let (handle, events) = Session::start(fake.transport(), opts(&dir)).await.unwrap();
    tokio::time::timeout(Duration::from_secs(10), handle.wait_synced())
        .await
        .expect("sync in time")
        .expect("sync ok");
    // Start the test's view after the first sync.
    let events = events.resubscribe();
    Harness {
        fake,
        handle,
        events,
        _dir: dir,
    }
}

/// Wait for the first event matching `f`.
pub async fn wait_event<F: FnMut(&Event) -> bool>(
    rx: &mut broadcast::Receiver<Event>,
    within: Duration,
    mut f: F,
) -> Option<Event> {
    let deadline = tokio::time::Instant::now() + within;
    loop {
        match tokio::time::timeout_at(deadline, rx.recv()).await {
            Err(_) => return None,
            Ok(Err(broadcast::error::RecvError::Lagged(_))) => continue,
            Ok(Err(_)) => return None,
            Ok(Ok(e)) => {
                if f(&e) {
                    return Some(e);
                }
            }
        }
    }
}

pub async fn sleep_ms(ms: u64) {
    tokio::time::sleep(Duration::from_millis(ms)).await;
}
