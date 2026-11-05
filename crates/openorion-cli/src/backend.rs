//! Where ops go: the daemon (default) or an in-process Session (`--direct`).

use std::path::{Path, PathBuf};
use std::time::Duration;
use std::time::Instant;

use openorion_core::{Event, Session, SessionHandle, SessionOpts};
use openorion_hid::{DeviceTransport, Transport};
use openorion_ipc::DaemonMsg;
use openorion_ipc::client::Client;
use openorion_ipc::{Health, Op};
use openorion_proto::{AfxMeters, DeviceState, Meters, Snapshot};
use openorion_sim::{SimClock, SimTransport};
use tokio::sync::broadcast;

/// How long to wait for the first full sync.
const SYNC_TIMEOUT: Duration = Duration::from_secs(15);

pub trait Backend {
    /// Current cached state.
    fn state(&mut self) -> Result<DeviceState, String>;
    /// Send one op (returns once the session accepted it).
    fn op(&mut self, op: Op) -> Result<(), String>;
    fn snapshot(&mut self) -> Result<Snapshot, String>;
    fn restore(&mut self, snap: Snapshot, dry_run: bool) -> Result<Vec<Op>, String>;
    fn health(&mut self) -> (Health, Option<String>);
    /// Next meter frame; None on timeout.
    fn meters(&mut self, timeout: Duration) -> Result<Option<(Meters, AfxMeters)>, String>;
    /// Called before exit: let queued sets reach the device. Returns
    /// warnings (read-back mismatches).
    fn finish(&mut self) -> Vec<String>;
    /// "openoriond" / "openoriond (sim)" / "direct" / "direct (sim)".
    fn describe(&self) -> String;
}

// ---------------------------------------------------------------------------
// Daemon

pub struct Daemon {
    c: Client,
    health: (Health, Option<String>),
    sim: bool,
    meters_on: bool,
}

impl Daemon {
    /// Connect and wait for the daemon's state. With `tolerant`, a daemon
    /// whose device is unhealthy (hung, stalled, no cyclic) is accepted
    /// rather than refused, so `openorion health` can still talk to it.
    pub fn connect(path: &Path, meters: bool, tolerant: bool) -> Result<Daemon, String> {
        let mut c = Client::connect(path).map_err(|e| e.to_string())?;
        let sim = c.info().sim;
        c.subscribe(meters).map_err(|e| e.to_string())?;
        let mut health = (Health::Syncing, None);
        let deadline = Instant::now() + SYNC_TIMEOUT;
        loop {
            if Instant::now() >= deadline {
                return Err(format!(
                    "openoriond has not synced with the device within {SYNC_TIMEOUT:?} (health {:?}{})",
                    health.0,
                    health
                        .1
                        .as_deref()
                        .map(|d| format!(": {d}"))
                        .unwrap_or_default()
                ));
            }
            match c
                .recv(Duration::from_millis(200))
                .map_err(|e| e.to_string())?
            {
                Some(DaemonMsg::State { .. }) => break,
                Some(DaemonMsg::Health { health: h, detail }) => {
                    if tolerant && h != Health::Syncing {
                        health = (h, detail);
                        break;
                    }
                    if matches!(h, Health::Hung | Health::NoCyclic) {
                        return Err(format!(
                            "openoriond reports {h:?}: {}",
                            detail.unwrap_or_else(|| "power-cycle the Orion".into())
                        ));
                    }
                    health = (h, detail);
                }
                _ => {}
            }
        }
        if health.0 == Health::Syncing {
            health.0 = Health::Ok;
        }
        Ok(Daemon {
            c,
            health,
            sim,
            meters_on: meters,
        })
    }

    fn drain(&mut self) {
        while let Ok(Some(m)) = self.c.try_recv() {
            if let DaemonMsg::Health { health, detail } = m {
                self.health = (health, detail);
            }
        }
    }
}

impl Backend for Daemon {
    fn state(&mut self) -> Result<DeviceState, String> {
        self.c.get_state().map_err(|e| e.to_string())
    }

    fn op(&mut self, op: Op) -> Result<(), String> {
        self.c.op(op).map_err(|e| e.to_string())
    }

    fn snapshot(&mut self) -> Result<Snapshot, String> {
        self.c.snapshot().map_err(|e| e.to_string())
    }

    fn restore(&mut self, snap: Snapshot, dry_run: bool) -> Result<Vec<Op>, String> {
        self.c.restore(snap, dry_run).map_err(|e| e.to_string())
    }

    fn health(&mut self) -> (Health, Option<String>) {
        if !self.meters_on {
            self.drain();
        }
        self.health.clone()
    }

    fn meters(&mut self, timeout: Duration) -> Result<Option<(Meters, AfxMeters)>, String> {
        let deadline = Instant::now() + timeout;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Ok(None);
            }
            match self.c.recv(left).map_err(|e| e.to_string())? {
                Some(DaemonMsg::Meters { meters, afx }) => return Ok(Some((*meters, *afx))),
                Some(DaemonMsg::Health { health, detail }) => self.health = (health, detail),
                Some(_) => {}
                None => return Ok(None),
            }
        }
    }

    fn finish(&mut self) -> Vec<String> {
        Vec::new()
    }

    fn describe(&self) -> String {
        if self.sim {
            "openoriond (sim)".into()
        } else {
            "openoriond".into()
        }
    }
}

// ---------------------------------------------------------------------------
// Direct

pub struct Direct {
    rt: tokio::runtime::Runtime,
    h: SessionHandle,
    events: broadcast::Receiver<Event>,
    sim: bool,
    dirty: bool,
}

/// `--sim FILE`: an `orionread.py` dump, or a snapshot written by
/// `openorion snapshot save`.
fn sim_transport(path: &Path) -> Result<SimTransport, String> {
    let text = std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let t = match serde_json::from_str::<Snapshot>(&text) {
        Ok(s) => SimTransport::from_state(s.state),
        Err(_) => SimTransport::from_state(
            openorion_sim::state_from_dump_json(&text)
                .map_err(|e| format!("--sim {}: {e}", path.display()))?,
        ),
    };
    Ok(t.with_clock(SimClock::RealTime))
}

impl Direct {
    pub fn open(sim: Option<&Path>, lock: Option<PathBuf>) -> Result<Direct, String> {
        let transport: Box<dyn Transport> = match sim {
            Some(p) => Box::new(sim_transport(p)?),
            None => Box::new(DeviceTransport::open().map_err(|e| e.to_string())?),
        };
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .map_err(|e| format!("tokio runtime: {e}"))?;
        let opts = SessionOpts {
            lock_path: lock.unwrap_or_else(openorion_ipc::lock_path),
            audio_busy: if sim.is_some() {
                || None
            } else {
                openorion_hid::usb_audio_busy
            },
            ..SessionOpts::default()
        };
        let (h, events) = rt.block_on(async {
            let (h, rx) = Session::start(transport, opts)
                .await
                .map_err(|e| e.to_string())?;
            match tokio::time::timeout(SYNC_TIMEOUT, h.wait_synced()).await {
                Err(_) => Err(format!("no full sync within {SYNC_TIMEOUT:?}")),
                Ok(Err(e)) => Err(e.to_string()),
                Ok(Ok(_)) => Ok((h, rx)),
            }
        })?;
        Ok(Direct {
            rt,
            h,
            events,
            sim: sim.is_some(),
            dirty: false,
        })
    }
}

impl Backend for Direct {
    fn state(&mut self) -> Result<DeviceState, String> {
        Ok(self.h.state())
    }

    fn op(&mut self, op: Op) -> Result<(), String> {
        self.dirty = true;
        let h = self.h.clone();
        self.rt.block_on(h.op(op)).map_err(|e| e.to_string())
    }

    fn snapshot(&mut self) -> Result<Snapshot, String> {
        self.h.snapshot().map_err(|e| e.to_string())
    }

    fn restore(&mut self, snap: Snapshot, dry_run: bool) -> Result<Vec<Op>, String> {
        if !dry_run {
            self.dirty = true;
        }
        let h = self.h.clone();
        self.rt
            .block_on(async move { h.restore(&snap, dry_run).await })
            .map_err(|e| e.to_string())
    }

    fn health(&mut self) -> (Health, Option<String>) {
        let h = self.h.health();
        (h.health, h.detail)
    }

    fn meters(&mut self, timeout: Duration) -> Result<Option<(Meters, AfxMeters)>, String> {
        let events = &mut self.events;
        self.rt.block_on(async {
            let deadline = tokio::time::Instant::now() + timeout;
            loop {
                match tokio::time::timeout_at(deadline, events.recv()).await {
                    Err(_) => return Ok(None),
                    Ok(Ok(Event::Meters { meters, afx })) => return Ok(Some((*meters, *afx))),
                    Ok(Ok(_)) | Ok(Err(broadcast::error::RecvError::Lagged(_))) => {}
                    Ok(Err(broadcast::error::RecvError::Closed)) => {
                        return Err("session closed".to_string());
                    }
                }
            }
        })
    }

    fn finish(&mut self) -> Vec<String> {
        let mut warnings = Vec::new();
        if self.dirty {
            // Sets are queued, coalesced (20 ms) and read back after
            // 150 ms; stay until that settles.
            let events = &mut self.events;
            self.rt.block_on(async {
                let deadline = tokio::time::Instant::now() + Duration::from_millis(600);
                loop {
                    match tokio::time::timeout_at(deadline, events.recv()).await {
                        Err(_) | Ok(Err(broadcast::error::RecvError::Closed)) => break,
                        Ok(Ok(Event::Mismatch { path, ours, device })) => warnings.push(format!(
                            "read-back mismatch at {path}: sent {ours}, device has {device}"
                        )),
                        Ok(_) => {}
                    }
                }
            });
        }
        self.h.shutdown();
        self.rt
            .block_on(async { tokio::time::sleep(Duration::from_millis(50)).await });
        warnings
    }

    fn describe(&self) -> String {
        if self.sim {
            "direct (sim)".into()
        } else {
            "direct".into()
        }
    }
}
