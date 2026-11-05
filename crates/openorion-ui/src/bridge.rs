//! Bridge between the egui thread and openoriond.
//!
//! The blocking `openorion_ipc::client::Client` lives on a background thread.
//! The UI sends `Outgoing` through a channel and polls `Incoming` each
//! frame; the thread calls the waker (egui `request_repaint`) whenever it
//! delivers something. When openoriond is not reachable the thread retries
//! every `RETRY` and reports `Incoming::Down`.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender, TryRecvError};
use std::time::{Duration, Instant};

use openorion_ipc::client::{Client, DaemonInfo};
use openorion_ipc::{ClientMsg, DaemonMsg, Op};
use openorion_proto::Snapshot;

use crate::state::{AppState, RESTORE_ID_BASE};

/// Reconnect interval while openoriond is down.
pub const RETRY: Duration = Duration::from_secs(2);
/// How long one receive waits before the command queue is checked again.
const POLL: Duration = Duration::from_millis(8);

/// UI → daemon.
#[derive(Debug, Clone, PartialEq)]
pub enum Outgoing {
    Op(Op),
    Restore {
        snapshot: Box<Snapshot>,
        dry_run: bool,
    },
    /// Opaque message for other clients (openorion-remote), relayed by
    /// the daemon.
    Remote(serde_json::Value),
}

/// Daemon → UI.
#[derive(Debug, Clone, PartialEq)]
pub enum Incoming {
    Connecting,
    Connected(DaemonInfo),
    /// Not connected; the text is the last error.
    Down(String),
    /// A request could not be sent.
    SendFailed(String),
    /// Every daemon message except `remote`.
    Msg(DaemonMsg),
    /// A `remote` message from another client.
    Remote(serde_json::Value),
}

impl From<DaemonMsg> for Incoming {
    fn from(m: DaemonMsg) -> Incoming {
        match m {
            DaemonMsg::Remote { msg } => Incoming::Remote(msg),
            m => Incoming::Msg(m),
        }
    }
}

enum Cmd {
    Out(Outgoing),
    Quit,
}

pub type Waker = Arc<dyn Fn() + Send + Sync>;

/// Reconnect bookkeeping (pure, for tests).
#[derive(Debug, Clone, PartialEq)]
pub struct Reconnect {
    pub interval: Duration,
    next: Option<Instant>,
    pub connected: bool,
    /// Failed attempts since the last success.
    pub failures: u32,
}

impl Reconnect {
    pub fn new(interval: Duration) -> Self {
        Reconnect {
            interval,
            next: None,
            connected: false,
            failures: 0,
        }
    }

    /// Should a connect attempt be made now?
    pub fn due(&self, now: Instant) -> bool {
        !self.connected && self.next.is_none_or(|t| now >= t)
    }

    /// Time until the next attempt.
    pub fn wait(&self, now: Instant) -> Duration {
        match self.next {
            Some(t) if !self.connected => t.saturating_duration_since(now),
            _ => Duration::ZERO,
        }
    }

    pub fn on_connected(&mut self) {
        self.connected = true;
        self.failures = 0;
        self.next = None;
    }

    pub fn on_failed(&mut self, now: Instant) {
        self.connected = false;
        self.failures += 1;
        self.next = Some(now + self.interval);
    }

    /// An established connection dropped: retry at once (the daemon may
    /// just have restarted), then back to the interval.
    pub fn on_lost(&mut self, now: Instant) {
        self.connected = false;
        self.next = Some(now);
    }
}

pub struct Bridge {
    tx: Sender<Cmd>,
    rx: Receiver<Incoming>,
}

impl Bridge {
    pub fn spawn(path: PathBuf, wake: Waker) -> Bridge {
        let (cmd_tx, cmd_rx) = mpsc::channel();
        let (in_tx, in_rx) = mpsc::channel();
        std::thread::Builder::new()
            .name("openorion-ui-bridge".into())
            .spawn(move || run(path, cmd_rx, in_tx, wake))
            .expect("spawn bridge thread");
        Bridge {
            tx: cmd_tx,
            rx: in_rx,
        }
    }

    pub fn send(&self, o: Outgoing) {
        let _ = self.tx.send(Cmd::Out(o));
    }

    pub fn try_recv(&self) -> Option<Incoming> {
        self.rx.try_recv().ok()
    }

    /// Wait up to `t` for the next message (tests).
    #[cfg(test)]
    pub fn recv_timeout(&self, t: Duration) -> Option<Incoming> {
        self.rx.recv_timeout(t).ok()
    }

    /// Exchange everything pending with `st`: incoming messages are
    /// applied, the send queue is drained. Returns how many messages
    /// arrived.
    pub fn pump(&self, st: &mut AppState) -> usize {
        let mut n = 0;
        while let Some(m) = self.try_recv() {
            st.apply_incoming(m);
            n += 1;
        }
        for o in st.sendq.drain(..) {
            self.send(o);
        }
        n
    }
}

impl Drop for Bridge {
    fn drop(&mut self) {
        // The thread exits on its own; no join (a hung connect must not
        // block closing the window).
        let _ = self.tx.send(Cmd::Quit);
    }
}

enum End {
    Quit,
    Lost(String),
}

fn run(path: PathBuf, cmds: Receiver<Cmd>, out: Sender<Incoming>, wake: Waker) {
    let emit = |m: Incoming| {
        let ok = out.send(m).is_ok();
        wake();
        ok
    };
    let mut rc = Reconnect::new(RETRY);
    if !emit(Incoming::Connecting) {
        return;
    }
    loop {
        let now = Instant::now();
        if rc.due(now) {
            match connect(&path) {
                Ok(mut c) => {
                    rc.on_connected();
                    if !emit(Incoming::Connected(c.info().clone())) {
                        return;
                    }
                    match serve(&mut c, &cmds, &emit) {
                        End::Quit => return,
                        End::Lost(e) => {
                            rc.on_lost(Instant::now());
                            if !emit(Incoming::Down(e)) {
                                return;
                            }
                        }
                    }
                }
                Err(e) => {
                    rc.on_failed(now);
                    if !emit(Incoming::Down(e)) {
                        return;
                    }
                }
            }
            continue;
        }
        match cmds.recv_timeout(rc.wait(now)) {
            Ok(Cmd::Quit) | Err(RecvTimeoutError::Disconnected) => return,
            Ok(Cmd::Out(_)) => {
                if !emit(Incoming::SendFailed("not connected to openoriond".into())) {
                    return;
                }
            }
            Err(RecvTimeoutError::Timeout) => {}
        }
    }
}

fn connect(path: &std::path::Path) -> Result<Client, String> {
    let mut c = Client::connect(path).map_err(|e| e.to_string())?;
    c.subscribe(true).map_err(|e| e.to_string())?;
    Ok(c)
}

fn serve(c: &mut Client, cmds: &Receiver<Cmd>, emit: &impl Fn(Incoming) -> bool) -> End {
    let mut restore_id = RESTORE_ID_BASE;
    loop {
        loop {
            let r = match cmds.try_recv() {
                Ok(Cmd::Out(Outgoing::Op(op))) => c.send_op(op).map(|_| ()),
                Ok(Cmd::Out(Outgoing::Restore { snapshot, dry_run })) => {
                    restore_id += 1;
                    c.send(&ClientMsg::Restore {
                        id: restore_id,
                        snapshot,
                        dry_run,
                    })
                }
                Ok(Cmd::Out(Outgoing::Remote(msg))) => c.send_remote(msg),
                Ok(Cmd::Quit) | Err(TryRecvError::Disconnected) => return End::Quit,
                Err(TryRecvError::Empty) => break,
            };
            if let Err(e) = r {
                return End::Lost(e.to_string());
            }
        }
        match c.recv(POLL) {
            Ok(Some(m)) => {
                if !emit(m.into()) {
                    return End::Quit;
                }
                // Drain what is already there without waiting.
                loop {
                    match c.try_recv() {
                        Ok(Some(m)) => {
                            if !emit(m.into()) {
                                return End::Quit;
                            }
                        }
                        Ok(None) => break,
                        Err(e) => return End::Lost(e.to_string()),
                    }
                }
            }
            Ok(None) => {}
            Err(e) => return End::Lost(e.to_string()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reconnect_state_machine() {
        let t0 = Instant::now();
        let mut rc = Reconnect::new(RETRY);
        assert!(rc.due(t0), "first attempt is immediate");
        rc.on_failed(t0);
        assert!(!rc.due(t0));
        assert_eq!(rc.wait(t0), RETRY);
        assert!(!rc.due(t0 + Duration::from_millis(1999)));
        assert!(rc.due(t0 + RETRY));
        rc.on_failed(t0 + RETRY);
        assert_eq!(rc.failures, 2);
        assert!(!rc.due(t0 + RETRY + Duration::from_secs(1)));
        rc.on_connected();
        assert!(rc.connected);
        assert_eq!(rc.failures, 0);
        assert!(
            !rc.due(t0 + Duration::from_secs(60)),
            "no attempts while up"
        );
        assert_eq!(rc.wait(t0), Duration::ZERO);
        // A dropped connection retries at once, then every RETRY.
        let t1 = t0 + Duration::from_secs(100);
        rc.on_lost(t1);
        assert!(rc.due(t1));
        rc.on_failed(t1);
        assert!(!rc.due(t1 + Duration::from_secs(1)));
        assert!(rc.due(t1 + RETRY));
    }

    /// `target/debug/openoriond` next to this test binary (`target/debug/deps`).
    fn oriond_bin() -> Option<PathBuf> {
        let exe = std::env::current_exe().ok()?;
        let p = exe.parent()?.parent()?.join("openoriond");
        p.is_file().then_some(p)
    }

    struct Daemon(std::process::Child, PathBuf);

    impl Drop for Daemon {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
            let _ = std::fs::remove_dir_all(&self.1);
        }
    }

    /// Pump until `pred` holds or `secs` pass.
    fn pump_until(b: &Bridge, st: &mut AppState, secs: u64, pred: impl Fn(&AppState) -> bool) {
        let end = Instant::now() + Duration::from_secs(secs);
        let t0 = Instant::now();
        while !pred(st) {
            assert!(Instant::now() < end, "timed out; conn {:?}", st.conn);
            if let Some(m) = b.recv_timeout(Duration::from_millis(50)) {
                st.apply_incoming(m);
            }
            st.tick(t0.elapsed().as_secs_f64());
            b.pump(st);
        }
    }

    #[test]
    fn live_against_sim_daemon() {
        let Some(bin) = oriond_bin() else {
            eprintln!("skipping: openoriond not built (cargo build -p openoriond)");
            return;
        };
        let dump = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/dump1.json");
        let dir = std::env::temp_dir().join(format!("openorion-ui-live-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let sock = dir.join("o.sock");
        let child = std::process::Command::new(bin)
            .arg("--sim")
            .arg(&dump)
            .arg("--socket")
            .arg(&sock)
            .arg("--lock")
            .arg(dir.join("o.lock"))
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("start openoriond");
        let _daemon = Daemon(child, dir);

        // The bridge retries until the socket appears.
        let b = Bridge::spawn(sock, Arc::new(|| {}));
        let mut st = AppState::live("test");
        pump_until(&b, &mut st, 20, |s| s.ready());
        assert!(matches!(&st.conn, crate::state::Conn::Connected(i) if i.sim));

        // A silent mixer-4 edit: optimistic locally, then the daemon's
        // Diff agrees.
        let ch = st.device.mixers[3].channels[0];
        let want = if ch.level.get() == 7 { 8 } else { 7 };
        let op = Op::Mixer {
            mixer: openorion_proto::MixerId::new(3).unwrap(),
            ch: 1,
            level: Some(openorion_proto::Atten::new(want).unwrap()),
            pan: None,
            mute: None,
            solo: None,
            send: None,
        };
        st.submit(op);
        assert_eq!(st.device.mixers[3].channels[0].level.get(), want);
        // Make the local copy wrong so only the daemon's Diff can fix it.
        st.device.mixers[3].channels[0].level = ch.level;
        b.pump(&mut st);
        pump_until(&b, &mut st, 10, |s| {
            s.device.mixers[3].channels[0].level.get() == want
        });
        // Restore dry run: a snapshot differing in one mixer-4 field plans
        // exactly that op and changes nothing.
        let mut snap = st.device.clone();
        snap.mixers[3].channels[1].mute = !snap.mixers[3].channels[1].mute;
        st.request_restore(
            openorion_proto::Snapshot {
                version: 1,
                taken: 0,
                state: snap.clone(),
            },
            true,
        );
        b.pump(&mut st);
        pump_until(&b, &mut st, 10, |s| {
            matches!(s.restore, crate::state::RestoreState::Plan { .. })
        });
        let crate::state::RestoreState::Plan { ops, applied } = &st.restore else {
            unreachable!()
        };
        assert!(!applied);
        assert_eq!(ops.len(), 1, "{ops:?}");
        assert_ne!(
            st.device.mixers[3].channels[1].mute,
            snap.mixers[3].channels[1].mute
        );

        // Meters stream in at ~30 Hz.
        let t = Instant::now();
        let mut n = 0;
        while t.elapsed() < Duration::from_millis(500) {
            if let Some(Incoming::Msg(DaemonMsg::Meters { .. })) =
                b.recv_timeout(Duration::from_millis(50))
            {
                n += 1;
            }
        }
        assert!(n >= 5, "meters: {n} in 0.5 s");
    }

    #[test]
    fn bridge_reports_down_without_daemon() {
        let dir =
            std::env::temp_dir().join(format!("openorion-ui-nodaemon-{}", std::process::id()));
        let b = Bridge::spawn(dir.join("none.sock"), Arc::new(|| {}));
        assert_eq!(
            b.recv_timeout(Duration::from_secs(2)),
            Some(Incoming::Connecting)
        );
        match b.recv_timeout(Duration::from_secs(2)) {
            Some(Incoming::Down(e)) => assert!(e.contains("openoriond"), "{e}"),
            other => panic!("expected Down, got {other:?}"),
        }
        // Ops while down are refused, not silently dropped.
        b.send(Outgoing::Op(Op::Resync));
        assert_eq!(
            b.recv_timeout(Duration::from_secs(2)),
            Some(Incoming::SendFailed("not connected to openoriond".into()))
        );
        let mut st = AppState::live("test");
        st.apply_incoming(Incoming::Down("x".into()));
        assert!(!st.ready());
    }
}
