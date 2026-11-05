//! The I/O thread. It owns the `Transport` (whose calls block), writes
//! what the session task sends it and forwards every incoming report.

use std::io;
use std::sync::mpsc as std_mpsc;
use std::thread::JoinHandle;
use std::time::Duration;

use openorion_hid::Transport;
use openorion_proto::{REPORT_LEN, Report};
use tokio::sync::mpsc;

pub(crate) enum IoCmd {
    Write(Box<Report>),
    Reset,
    Shutdown,
}

pub(crate) enum IoEvent {
    Report(Box<[u8; REPORT_LEN]>),
    ResetDone(Result<(), String>),
    /// Read or write failed; the thread has stopped.
    Error(String),
}

pub(crate) struct Io {
    pub tx: std_mpsc::Sender<IoCmd>,
    pub rx: mpsc::UnboundedReceiver<IoEvent>,
    pub join: Option<JoinHandle<()>>,
}

impl Io {
    pub fn spawn(mut t: Box<dyn Transport>, poll: Duration) -> Io {
        let (cmd_tx, cmd_rx) = std_mpsc::channel::<IoCmd>();
        let (ev_tx, ev_rx) = mpsc::unbounded_channel::<IoEvent>();
        let join = std::thread::Builder::new()
            .name("orion-io".into())
            .spawn(move || {
                // Forward one report; false when the thread must stop.
                let forward = |r: io::Result<Option<[u8; REPORT_LEN]>>| -> Option<bool> {
                    match r {
                        Ok(Some(r)) => Some(ev_tx.send(IoEvent::Report(Box::new(r))).is_ok()),
                        Ok(None) => None,
                        Err(e) => {
                            let _ = ev_tx.send(IoEvent::Error(format!("read: {e}")));
                            Some(false)
                        }
                    }
                };
                loop {
                    // Drain what is ready before each command, so a burst
                    // of writes never starves the cyclic reports (on
                    // hardware that read as "cyclic stopped").
                    loop {
                        match forward(t.read(Duration::ZERO)) {
                            Some(true) => {}
                            Some(false) => return,
                            None => break,
                        }
                    }
                    match cmd_rx.try_recv() {
                        Ok(IoCmd::Write(r)) => {
                            if let Err(e) = t.write(&r) {
                                let _ = ev_tx.send(IoEvent::Error(format!("write: {e}")));
                                return;
                            }
                        }
                        Ok(IoCmd::Reset) => {
                            let r = t.reset().map_err(|e| e.to_string());
                            if ev_tx.send(IoEvent::ResetDone(r)).is_err() {
                                return;
                            }
                        }
                        Ok(IoCmd::Shutdown) | Err(std_mpsc::TryRecvError::Disconnected) => {
                            return;
                        }
                        Err(std_mpsc::TryRecvError::Empty) => match forward(t.read(poll)) {
                            Some(true) => {}
                            Some(false) => return,
                            None => {
                                if ev_tx.is_closed() {
                                    return;
                                }
                            }
                        },
                    }
                }
            })
            .expect("spawn orion-io thread");
        Io {
            tx: cmd_tx,
            rx: ev_rx,
            join: Some(join),
        }
    }

    pub fn send(&self, c: IoCmd) {
        let _ = self.tx.send(c);
    }
}

impl Drop for Io {
    fn drop(&mut self) {
        let _ = self.tx.send(IoCmd::Shutdown);
        // The thread exits within one read poll; don't block an async
        // context waiting for it.
        self.join.take();
    }
}
