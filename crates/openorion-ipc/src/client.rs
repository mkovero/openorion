//! Blocking client: std `UnixStream` plus a reader thread delivering
//! `DaemonMsg` on an mpsc channel (DESIGN.md sec. 7).
//!
//! Requests that carry an id (`op`, `snapshot`, `restore`) wait for their
//! answer; every other message that arrives meanwhile is queued and handed
//! out by `recv`/`try_recv`, so no event is lost. The UI uses `send_op`
//! (no wait) and polls `try_recv` each frame.

use std::collections::VecDeque;
use std::io::{self, BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, TryRecvError};
use std::time::{Duration, Instant};

use openorion_proto::{DeviceState, Snapshot};

use crate::{ClientMsg, DaemonMsg, IPC_VERSION, Op};

/// How long a request waits for its answer.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Debug, thiserror::Error)]
pub enum ClientError {
    #[error("cannot connect to openoriond at {path}: {source} (is openoriond running?)")]
    Connect {
        path: String,
        #[source]
        source: io::Error,
    },
    #[error("ipc i/o: {0}")]
    Io(#[from] io::Error),
    #[error("openoriond closed the connection")]
    Closed,
    #[error("no answer from openoriond within {0:?}")]
    Timeout(Duration),
    #[error("ipc version mismatch: daemon {daemon}, client {client}")]
    Version { daemon: u32, client: u32 },
    /// The daemon refused or failed the request.
    #[error("{0}")]
    Daemon(String),
    #[error("unexpected reply: {0}")]
    Unexpected(String),
}

/// Daemon identity from the `hello` exchange.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DaemonInfo {
    pub version: u32,
    pub daemon: String,
    pub sim: bool,
}

pub struct Client {
    stream: UnixStream,
    rx: Receiver<Result<DaemonMsg, String>>,
    pending: VecDeque<DaemonMsg>,
    next_id: u64,
    info: DaemonInfo,
    timeout: Duration,
}

impl Client {
    /// Connect to the default socket (`socket_path()`).
    pub fn connect_default() -> Result<Client, ClientError> {
        Client::connect(&crate::socket_path())
    }

    /// Connect and exchange `hello`.
    pub fn connect(path: &Path) -> Result<Client, ClientError> {
        let stream = UnixStream::connect(path).map_err(|source| ClientError::Connect {
            path: path.display().to_string(),
            source,
        })?;
        let read = stream.try_clone()?;
        let (tx, rx) = mpsc::channel();
        std::thread::Builder::new()
            .name("openorion-ipc-reader".into())
            .spawn(move || reader(read, tx))?;
        let mut c = Client {
            stream,
            rx,
            pending: VecDeque::new(),
            next_id: 1,
            info: DaemonInfo {
                version: 0,
                daemon: String::new(),
                sim: false,
            },
            timeout: DEFAULT_TIMEOUT,
        };
        c.send(&ClientMsg::Hello { v: IPC_VERSION })?;
        c.info = c.wait_for(|m| match m {
            DaemonMsg::Hello { v, daemon, sim } => Some(Ok(DaemonInfo {
                version: *v,
                daemon: daemon.clone(),
                sim: *sim,
            })),
            _ => None,
        })?;
        if c.info.version != IPC_VERSION {
            return Err(ClientError::Version {
                daemon: c.info.version,
                client: IPC_VERSION,
            });
        }
        Ok(c)
    }

    pub fn info(&self) -> &DaemonInfo {
        &self.info
    }

    pub fn set_timeout(&mut self, t: Duration) {
        self.timeout = t;
    }

    /// Send one message, no waiting.
    pub fn send(&mut self, msg: &ClientMsg) -> Result<(), ClientError> {
        let mut line = serde_json::to_vec(msg).map_err(io::Error::other)?;
        line.push(b'\n');
        self.stream.write_all(&line)?;
        Ok(())
    }

    /// Start receiving state events (and meters if asked). The daemon
    /// answers with the current `health` and, once synced, `state`.
    pub fn subscribe(&mut self, meters: bool) -> Result<(), ClientError> {
        self.send(&ClientMsg::Subscribe { meters })
    }

    /// Fetch the full state (waits; errors if the daemon is not synced).
    pub fn get_state(&mut self) -> Result<DeviceState, ClientError> {
        self.send(&ClientMsg::GetState)?;
        self.wait_for(|m| match m {
            DaemonMsg::State { state } => Some(Ok((**state).clone())),
            DaemonMsg::Result {
                id: 0,
                ok: false,
                error,
            } => Some(Err(ClientError::Daemon(
                error.clone().unwrap_or_else(|| "get_state failed".into()),
            ))),
            _ => None,
        })
    }

    /// Queue an op without waiting; returns its id. The `result` arrives
    /// through `recv`/`try_recv`.
    pub fn send_op(&mut self, op: Op) -> Result<u64, ClientError> {
        let id = self.take_id();
        self.send(&ClientMsg::Op { id, op })?;
        Ok(id)
    }

    /// Send an op and wait until the daemon accepted it.
    pub fn op(&mut self, op: Op) -> Result<(), ClientError> {
        let id = self.send_op(op)?;
        self.wait_result(id).map(|_| ())
    }

    pub fn snapshot(&mut self) -> Result<Snapshot, ClientError> {
        let id = self.take_id();
        self.send(&ClientMsg::Snapshot { id })?;
        self.wait_for(|m| match m {
            DaemonMsg::Snapshot { id: i, snapshot } if *i == id => Some(Ok((**snapshot).clone())),
            DaemonMsg::Result {
                id: i,
                ok: false,
                error,
            } if *i == id => Some(Err(daemon_err(error))),
            _ => None,
        })
    }

    /// Restore a snapshot (or only plan it, on `dry_run`). Returns the ops.
    pub fn restore(&mut self, snapshot: Snapshot, dry_run: bool) -> Result<Vec<Op>, ClientError> {
        let id = self.take_id();
        self.send(&ClientMsg::Restore {
            id,
            snapshot: Box::new(snapshot),
            dry_run,
        })?;
        self.wait_for(|m| match m {
            DaemonMsg::RestorePlan { id: i, ops, .. } if *i == id => Some(Ok(ops.clone())),
            DaemonMsg::Result {
                id: i,
                ok: false,
                error,
            } if *i == id => Some(Err(daemon_err(error))),
            _ => None,
        })
    }

    /// Send an opaque `remote` message for the other subscribed clients
    /// (no wait). Their `remote` messages arrive through `recv`/`try_recv`
    /// as `DaemonMsg::Remote` once this client has subscribed. A refusal
    /// (too large) comes back as a `result` with id 0.
    pub fn send_remote(&mut self, msg: serde_json::Value) -> Result<(), ClientError> {
        self.send(&ClientMsg::Remote { msg })
    }

    /// Next message, waiting up to `timeout`. `Ok(None)` on timeout.
    pub fn recv(&mut self, timeout: Duration) -> Result<Option<DaemonMsg>, ClientError> {
        if let Some(m) = self.pending.pop_front() {
            return Ok(Some(m));
        }
        match self.rx.recv_timeout(timeout) {
            Ok(m) => m.map(Some).map_err(ClientError::Unexpected),
            Err(RecvTimeoutError::Timeout) => Ok(None),
            Err(RecvTimeoutError::Disconnected) => Err(ClientError::Closed),
        }
    }

    /// Next message if one is ready.
    pub fn try_recv(&mut self) -> Result<Option<DaemonMsg>, ClientError> {
        if let Some(m) = self.pending.pop_front() {
            return Ok(Some(m));
        }
        match self.rx.try_recv() {
            Ok(m) => m.map(Some).map_err(ClientError::Unexpected),
            Err(TryRecvError::Empty) => Ok(None),
            Err(TryRecvError::Disconnected) => Err(ClientError::Closed),
        }
    }

    fn take_id(&mut self) -> u64 {
        let id = self.next_id;
        self.next_id += 1;
        id
    }

    fn wait_result(&mut self, id: u64) -> Result<(), ClientError> {
        self.wait_for(|m| match m {
            DaemonMsg::Result { id: i, ok, error } if *i == id => {
                Some(if *ok { Ok(()) } else { Err(daemon_err(error)) })
            }
            _ => None,
        })
    }

    /// Wait for the first message `pick` accepts; queue the others.
    fn wait_for<T>(
        &mut self,
        mut pick: impl FnMut(&DaemonMsg) -> Option<Result<T, ClientError>>,
    ) -> Result<T, ClientError> {
        let deadline = Instant::now() + self.timeout;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            let m = match self.rx.recv_timeout(left) {
                Ok(m) => m.map_err(ClientError::Unexpected)?,
                Err(RecvTimeoutError::Timeout) => return Err(ClientError::Timeout(self.timeout)),
                Err(RecvTimeoutError::Disconnected) => return Err(ClientError::Closed),
            };
            match pick(&m) {
                Some(r) => return r,
                None => self.pending.push_back(m),
            }
        }
    }
}

impl Drop for Client {
    fn drop(&mut self) {
        // Ends the reader thread too (its read returns EOF).
        let _ = self.stream.shutdown(std::net::Shutdown::Both);
    }
}

fn daemon_err(e: &Option<String>) -> ClientError {
    ClientError::Daemon(e.clone().unwrap_or_else(|| "request failed".into()))
}

fn reader(stream: UnixStream, tx: mpsc::Sender<Result<DaemonMsg, String>>) {
    for line in BufReader::new(stream).lines() {
        let Ok(line) = line else { break };
        if line.trim().is_empty() {
            continue;
        }
        let msg = serde_json::from_str::<DaemonMsg>(&line)
            .map_err(|e| format!("bad message from openoriond: {e}: {line}"));
        if tx.send(msg).is_err() {
            break;
        }
    }
}
