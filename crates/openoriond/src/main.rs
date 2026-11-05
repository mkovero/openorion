//! openoriond: owns the Orion and serves clients (DESIGN.md sec. 7).
//!
//! A supervisor opens the device (or the simulator with `--sim`), runs one
//! `Session` and restarts it when the device goes away. Clients talk
//! newline-delimited JSON (`openorion_ipc::{ClientMsg, DaemonMsg}`) on a unix
//! socket. Session events are fanned out to every subscribed client, so
//! clients see each other's changes.
//!
//! `remote` messages (opaque client-to-client JSON, e.g. openorion-remote
//! and the UI) go through their own fan-out: each is relayed to every other
//! subscribed client that said hello with v2 or later; the sender does not
//! get its own message back.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

mod afx_keep;

use clap::Parser;
use openorion_core::{Error as CoreError, Event, Session, SessionHandle, SessionOpts};
use openorion_hid::{HidrawTransport, Transport};
use openorion_ipc::{ClientMsg, DaemonMsg, Health, IPC_VERSION, REMOTE_MAX_BYTES, REMOTE_SINCE};
use openorion_sim::{SimClock, SimTransport};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{broadcast, mpsc};

/// Pause before reopening after the device went away or failed to start.
const RETRY: Duration = Duration::from_secs(5);
/// Fan-out buffer per client; a client that lags gets a fresh `state`.
const FANOUT: usize = 512;
/// Queued outgoing lines per client before it is dropped as stuck.
const CLIENT_QUEUE: usize = 1024;
/// Relay buffer for `remote` messages; a lagging client loses the oldest.
const REMOTE_FANOUT: usize = 256;

#[derive(Parser, Debug)]
#[command(
    name = "openoriond",
    version,
    about = "openorion daemon: owns the Orion32 HD"
)]
struct Args {
    /// Run on the simulator seeded from this dump (e.g. fixtures/dump1.json).
    #[arg(long, value_name = "DUMP")]
    sim: Option<PathBuf>,
    /// Stay in the foreground (the default; kept for the systemd unit).
    #[arg(long)]
    foreground: bool,
    /// Socket path [default: $XDG_RUNTIME_DIR/openorion.sock].
    #[arg(long)]
    socket: Option<PathBuf>,
    /// Session lock path [default: $XDG_RUNTIME_DIR/openorion.lock].
    #[arg(long)]
    lock: Option<PathBuf>,
    /// Don't put AFX settings back after a power cycle.
    #[arg(long)]
    no_afx_restore: bool,
    /// Where the AFX settings are kept for that [default:
    /// $XDG_STATE_HOME/openorion/afx.json; off with --sim unless given].
    #[arg(long, value_name = "FILE")]
    afx_state: Option<PathBuf>,
}

/// State shared by the supervisor and the client tasks.
struct Daemon {
    sim: bool,
    session: Mutex<Option<SessionHandle>>,
    /// Last health, for clients that subscribe later.
    health: Mutex<(Health, Option<String>)>,
    events: broadcast::Sender<DaemonMsg>,
    /// `remote` relay: (sender's connection id, message).
    remote: broadcast::Sender<(u64, serde_json::Value)>,
    next_conn: AtomicU64,
}

impl Daemon {
    fn session(&self) -> Option<SessionHandle> {
        self.session.lock().unwrap().clone()
    }

    fn set_health(&self, health: Health, detail: Option<String>) {
        let mut h = self.health.lock().unwrap();
        if *h == (health, detail.clone()) {
            return;
        }
        *h = (health, detail.clone());
        let _ = self.events.send(DaemonMsg::Health { health, detail });
    }

    fn health_msg(&self) -> DaemonMsg {
        let (health, detail) = self.health.lock().unwrap().clone();
        DaemonMsg::Health { health, detail }
    }

    /// The full state if a session has synced.
    fn state_msg(&self) -> Option<DaemonMsg> {
        let s = self.session()?;
        s.is_synced().then(|| DaemonMsg::State {
            state: Box::new(s.state()),
        })
    }
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .with_writer(std::io::stderr)
        .with_ansi(std::io::IsTerminal::is_terminal(&std::io::stderr()))
        .init();
    let args = Args::parse();
    if let Err(e) = run(args).await {
        tracing::error!("{e}");
        std::process::exit(1);
    }
}

async fn run(args: Args) -> Result<(), String> {
    let socket = args
        .socket
        .clone()
        .unwrap_or_else(openorion_ipc::socket_path);
    let lock = args.lock.clone().unwrap_or_else(openorion_ipc::lock_path);
    if let Some(dump) = &args.sim {
        // Fail early on a bad dump rather than in the retry loop.
        SimTransport::from_dump(dump).map_err(|e| format!("--sim {}: {e}", dump.display()))?;
    }
    let listener = bind(&socket)?;
    tracing::info!(socket = %socket.display(), sim = args.sim.is_some(), "openoriond listening");

    let (events, _) = broadcast::channel(FANOUT);
    let daemon = Arc::new(Daemon {
        sim: args.sim.is_some(),
        session: Mutex::new(None),
        health: Mutex::new((Health::Disconnected, None)),
        events,
        remote: broadcast::channel(REMOTE_FANOUT).0,
        next_conn: AtomicU64::new(1),
    });

    let afx_state = if args.no_afx_restore {
        None
    } else if args.afx_state.is_some() {
        args.afx_state.clone()
    } else if args.sim.is_none() {
        afx_keep::default_path()
    } else {
        None
    };
    let sup = tokio::spawn(supervise(daemon.clone(), args.sim.clone(), lock, afx_state));
    let accept = tokio::spawn(accept_loop(listener, daemon.clone()));

    let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        .map_err(|e| e.to_string())?;
    let result = tokio::select! {
        _ = tokio::signal::ctrl_c() => Ok(()),
        _ = term.recv() => Ok(()),
        r = sup => match r {
            Ok(Err(e)) => Err(e),
            _ => Ok(()),
        },
    };
    tracing::info!("shutting down");
    accept.abort();
    if let Some(s) = daemon.session() {
        s.shutdown();
        // Let the session task release the transport and the lock.
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let _ = std::fs::remove_file(&socket);
    result
}

/// Bind the socket; replace a stale one, refuse a live one.
fn bind(path: &Path) -> Result<UnixListener, String> {
    if path.exists() {
        if std::os::unix::net::UnixStream::connect(path).is_ok() {
            return Err(format!(
                "another openoriond is already listening on {}",
                path.display()
            ));
        }
        std::fs::remove_file(path)
            .map_err(|e| format!("cannot remove stale socket {}: {e}", path.display()))?;
    }
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    }
    let l = UnixListener::bind(path).map_err(|e| format!("bind {}: {e}", path.display()))?;
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
        .map_err(|e| format!("chmod {}: {e}", path.display()))?;
    Ok(l)
}

fn open_transport(sim: &Option<PathBuf>) -> Result<Box<dyn Transport>, String> {
    match sim {
        Some(dump) => SimTransport::from_dump(dump)
            .map(|t| Box::new(t.with_clock(SimClock::RealTime)) as Box<dyn Transport>)
            .map_err(|e| format!("simulator: {e}")),
        None => HidrawTransport::open()
            .map(|t| Box::new(t) as Box<dyn Transport>)
            .map_err(|e| e.to_string()),
    }
}

/// Open → run a session → when it ends, wait and reopen. Returns only on a
/// fatal error (the lock is held by someone else).
async fn supervise(
    d: Arc<Daemon>,
    sim: Option<PathBuf>,
    lock: PathBuf,
    afx_state: Option<PathBuf>,
) -> Result<(), String> {
    loop {
        let transport = match open_transport(&sim) {
            Ok(t) => t,
            Err(e) => {
                d.set_health(Health::Disconnected, Some(e));
                tokio::time::sleep(RETRY).await;
                continue;
            }
        };
        d.set_health(Health::Syncing, None);
        let opts = SessionOpts {
            lock_path: lock.clone(),
            audio_busy: if sim.is_some() {
                || None
            } else {
                openorion_hid::usb_audio_busy
            },
            ..SessionOpts::default()
        };
        let (handle, rx) = match Session::start(transport, opts).await {
            Ok(x) => x,
            Err(e @ CoreError::Locked(_)) => return Err(e.to_string()),
            Err(e) => {
                let h = match e {
                    CoreError::NoCyclic | CoreError::ResetBlocked(_) => Health::NoCyclic,
                    CoreError::Hung => Health::Hung,
                    _ => Health::Disconnected,
                };
                d.set_health(h, Some(e.to_string()));
                tokio::time::sleep(RETRY).await;
                continue;
            }
        };
        tracing::info!("session started");
        *d.session.lock().unwrap() = Some(handle.clone());
        let keep = afx_state
            .clone()
            .map(|p| tokio::spawn(afx_keep::run(handle.clone(), p, d.events.clone())));
        forward(&d, &handle, rx).await;
        if let Some(k) = keep {
            k.abort();
        }
        *d.session.lock().unwrap() = None;
        let h = handle.health();
        tracing::warn!(health = ?h.health, detail = ?h.detail, "session ended");
        // The task has stopped: whatever it last said, the device is gone
        // for us until the reopen succeeds.
        let health = if h.health == Health::Ok || h.health == Health::Syncing {
            Health::Disconnected
        } else {
            h.health
        };
        d.set_health(health, h.detail.clone());
        tokio::time::sleep(RETRY).await;
    }
}

/// Session events → client fan-out, until the session task stops.
async fn forward(d: &Daemon, handle: &SessionHandle, mut rx: broadcast::Receiver<Event>) {
    let mut alive = handle.health_watch();
    loop {
        tokio::select! {
            ev = rx.recv() => match ev {
                Ok(ev) => {
                    if let Some(m) = event_msg(d, ev) {
                        let _ = d.events.send(m);
                    }
                }
                Err(broadcast::error::RecvError::Lagged(n)) => {
                    tracing::warn!(n, "session events lagged; resending state");
                    if let Some(m) = d.state_msg() {
                        let _ = d.events.send(m);
                    }
                }
                Err(broadcast::error::RecvError::Closed) => return,
            },
            r = alive.changed() => {
                if r.is_err() || !alive.borrow().alive {
                    return;
                }
            }
        }
    }
}

fn event_msg(d: &Daemon, ev: Event) -> Option<DaemonMsg> {
    Some(match ev {
        Event::Synced(state) => DaemonMsg::State { state },
        Event::Status(status) => DaemonMsg::Status { status },
        Event::Meters { meters, afx } => DaemonMsg::Meters { meters, afx },
        Event::Health { health, detail } => {
            // Keeps the cached copy current; set_health broadcasts.
            d.set_health(health, detail);
            return None;
        }
        Event::Mismatch { path, ours, device } => DaemonMsg::Mismatch { path, ours, device },
        Event::Diff { path, value } => DaemonMsg::Diff { path, value },
    })
}

async fn accept_loop(listener: UnixListener, d: Arc<Daemon>) {
    loop {
        match listener.accept().await {
            Ok((stream, _)) => {
                tokio::spawn(client(stream, d.clone()));
            }
            Err(e) => {
                tracing::error!("accept: {e}");
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        }
    }
}

/// One client connection: requests in, replies and subscribed events out.
async fn client(stream: UnixStream, d: Arc<Daemon>) {
    let (read, mut write) = stream.into_split();
    let (out, mut out_rx) = mpsc::channel::<DaemonMsg>(CLIENT_QUEUE);
    let writer = tokio::spawn(async move {
        while let Some(m) = out_rx.recv().await {
            let Ok(mut line) = serde_json::to_vec(&m) else {
                continue;
            };
            line.push(b'\n');
            if write.write_all(&line).await.is_err() {
                break;
            }
        }
    });

    let mut lines = BufReader::new(read).lines();
    let conn = d.next_conn.fetch_add(1, Ordering::Relaxed);
    // Version from the client's hello; a client that never says hello is
    // taken to speak the current one.
    let mut peer_v = IPC_VERSION;
    // (receiver, wants meters) once subscribed.
    let mut sub: Option<(broadcast::Receiver<DaemonMsg>, bool)> = None;
    let mut remote_rx: Option<broadcast::Receiver<(u64, serde_json::Value)>> = None;
    loop {
        tokio::select! {
            line = lines.next_line() => {
                let line = match line {
                    Ok(Some(l)) => l,
                    _ => break,
                };
                if line.trim().is_empty() {
                    continue;
                }
                let msg = match serde_json::from_str::<ClientMsg>(&line) {
                    Ok(m) => m,
                    Err(e) => {
                        let _ = out.try_send(DaemonMsg::Result {
                            id: 0,
                            ok: false,
                            error: Some(format!("bad request: {e}")),
                        });
                        continue;
                    }
                };
                if let ClientMsg::Subscribe { meters } = msg {
                    // Subscribe before taking the snapshot so nothing falls
                    // between the two.
                    sub = Some((d.events.subscribe(), meters));
                    if peer_v >= REMOTE_SINCE {
                        remote_rx = Some(d.remote.subscribe());
                    }
                    if send(&out, d.health_msg()).is_err() {
                        break;
                    }
                    if let Some(s) = d.state_msg()
                        && send(&out, s).is_err()
                    {
                        break;
                    }
                    continue;
                }
                if let ClientMsg::Remote { msg } = msg {
                    // Relay only; no reply unless refused.
                    let size = serde_json::to_vec(&msg).map_or(usize::MAX, |b| b.len());
                    if size > REMOTE_MAX_BYTES {
                        let refusal = DaemonMsg::Result {
                            id: 0,
                            ok: false,
                            error: Some(format!(
                                "remote message too large ({size} bytes, max {REMOTE_MAX_BYTES})"
                            )),
                        };
                        if send(&out, refusal).is_err() {
                            break;
                        }
                    } else {
                        let _ = d.remote.send((conn, msg));
                    }
                    continue;
                }
                if let ClientMsg::Hello { v } = msg {
                    peer_v = v;
                }
                let reply = request(&d, msg).await;
                if send(&out, reply).is_err() {
                    break;
                }
            }
            ev = recv_sub(&mut sub) => {
                let m = match ev {
                    Ok(m) => m,
                    Err(broadcast::error::RecvError::Lagged(_)) => match d.state_msg() {
                        Some(s) => s,
                        None => continue,
                    },
                    Err(broadcast::error::RecvError::Closed) => break,
                };
                let wants_meters = sub.as_ref().is_some_and(|s| s.1);
                if matches!(m, DaemonMsg::Meters { .. }) && !wants_meters {
                    continue;
                }
                if send(&out, m).is_err() {
                    break;
                }
            }
            r = recv_remote(&mut remote_rx) => {
                let msg = match r {
                    Ok((from, _)) if from == conn => continue,
                    Ok((_, msg)) => msg,
                    Err(broadcast::error::RecvError::Lagged(n)) => {
                        tracing::warn!(conn, n, "remote relay lagged; dropped messages");
                        continue;
                    }
                    Err(broadcast::error::RecvError::Closed) => break,
                };
                if send(&out, DaemonMsg::Remote { msg }).is_err() {
                    break;
                }
            }
        }
    }
    drop(out);
    let _ = writer.await;
}

/// Queue a message; Err if the client is gone or stuck (queue full).
fn send(out: &mpsc::Sender<DaemonMsg>, m: DaemonMsg) -> Result<(), ()> {
    out.try_send(m).map_err(|_| ())
}

async fn recv_sub(
    sub: &mut Option<(broadcast::Receiver<DaemonMsg>, bool)>,
) -> Result<DaemonMsg, broadcast::error::RecvError> {
    match sub {
        Some((rx, _)) => rx.recv().await,
        None => std::future::pending().await,
    }
}

async fn recv_remote(
    rx: &mut Option<broadcast::Receiver<(u64, serde_json::Value)>>,
) -> Result<(u64, serde_json::Value), broadcast::error::RecvError> {
    match rx {
        Some(rx) => rx.recv().await,
        None => std::future::pending().await,
    }
}

fn not_ready(d: &Daemon) -> String {
    let (h, detail) = d.health.lock().unwrap().clone();
    match detail {
        Some(x) => format!("device not ready ({h:?}): {x}"),
        None => format!("device not ready ({h:?})"),
    }
}

/// Answer one request (everything but `subscribe`).
async fn request(d: &Daemon, msg: ClientMsg) -> DaemonMsg {
    let fail = |id: u64, e: String| DaemonMsg::Result {
        id,
        ok: false,
        error: Some(e),
    };
    match msg {
        // The lower version of the two, so an older client keeps working.
        ClientMsg::Hello { v } => DaemonMsg::Hello {
            v: v.min(IPC_VERSION),
            daemon: format!("openoriond {}", env!("CARGO_PKG_VERSION")),
            sim: d.sim,
        },
        ClientMsg::GetState => d.state_msg().unwrap_or_else(|| fail(0, not_ready(d))),
        ClientMsg::Subscribe { .. } | ClientMsg::Remote { .. } => {
            unreachable!("handled by the caller")
        }
        ClientMsg::Op { id, op } => {
            let Some(s) = d.session() else {
                return fail(id, not_ready(d));
            };
            tracing::debug!(id, ?op, "op");
            match s.op(op).await {
                Ok(()) => DaemonMsg::Result {
                    id,
                    ok: true,
                    error: None,
                },
                Err(e) => fail(id, e.to_string()),
            }
        }
        ClientMsg::Snapshot { id } => match d.session().map(|s| s.snapshot()) {
            Some(Ok(snapshot)) => DaemonMsg::Snapshot {
                id,
                snapshot: Box::new(snapshot),
            },
            Some(Err(e)) => fail(id, e.to_string()),
            None => fail(id, not_ready(d)),
        },
        ClientMsg::Restore {
            id,
            snapshot,
            dry_run,
        } => {
            let Some(s) = d.session() else {
                return fail(id, not_ready(d));
            };
            match s.restore(&snapshot, dry_run).await {
                Ok(ops) => DaemonMsg::RestorePlan {
                    id,
                    ops,
                    applied: !dry_run,
                },
                Err(e) => fail(id, e.to_string()),
            }
        }
    }
}
