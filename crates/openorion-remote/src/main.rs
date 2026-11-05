//! openorion-remote: drive openorion from MIDI controllers and OSC, and
//! send values and meters back out.
//!
//! An openoriond client like openorion-jack. It keeps a mirror of the
//! device state, turns controller input into ops (`engine.rs`), and talks
//! to the UI's learn mode through the daemon's `remote` relay. MIDI is ALSA
//! sequencer through midir (`midi.rs`); OSC is UDP.
//!
//! Config: `$XDG_CONFIG_HOME/openorion/remote.json` (`openorion_control::RemoteConfig`).

mod engine;
mod grid;
mod midi;

use std::net::{SocketAddr, ToSocketAddrs};
use std::path::PathBuf;
use std::time::{Duration, Instant};

use clap::Parser;
use openorion_control::{OSC_PREFIX, Param, RemoteConfig};
use openorion_ipc::{ClientMsg, DaemonMsg, IPC_VERSION, REMOTE_SINCE};
use rosc::{OscPacket, decoder, encoder};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UdpSocket, UnixStream};
use tokio::sync::mpsc;

use engine::{Engine, METER_BANKS, OSC_METERS, OSC_SYNC, Out, Target};
use midi::{MidiCmd, MidiEvent};

#[derive(Parser, Debug)]
#[command(
    name = "openorion-remote",
    version,
    about = "MIDI and OSC control for openorion (an openoriond client)"
)]
struct Args {
    /// Config file [default: $XDG_CONFIG_HOME/openorion/remote.json].
    #[arg(long, value_name = "PATH")]
    config: Option<PathBuf>,
    /// OSC listen address, overrides the config ("" = OSC off).
    #[arg(long, value_name = "ADDR")]
    osc_listen: Option<String>,
    /// openoriond socket [default: $XDG_RUNTIME_DIR/openorion.sock].
    #[arg(long, value_name = "PATH")]
    socket: Option<PathBuf>,
    /// Print the OSC address space and exit.
    #[arg(long)]
    help_osc: bool,
    /// List the MIDI ports and whether the config enables them, and exit.
    #[arg(long)]
    list_midi: bool,
    /// Run without MIDI (no ALSA sequencer needed).
    #[arg(long)]
    no_midi: bool,
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
    if args.help_osc {
        print_osc();
        return;
    }
    if let Err(e) = run(args).await {
        tracing::error!("{e}");
        std::process::exit(1);
    }
}

fn print_osc() {
    println!("OSC input (UDP, default 127.0.0.1:9000) and output to targets.");
    println!("Values are normalised 0..1; toggles take 0/1 (int, float or bool).\n");
    println!("Parameters ({OSC_PREFIX}<path> f; no argument = reply with the value):");
    for l in Param::namespace() {
        println!("  {OSC_PREFIX}{l}");
    }
    println!("\nControl:");
    println!("  {OSC_SYNC}    (no args) send every value to the targets");
    println!("\nOutput to targets:");
    println!("  {OSC_PREFIX}<path> f    on every change (targets with \"values\": true)");
    for b in METER_BANKS {
        println!("  {OSC_METERS}{b} f...    dBFS, -96 = floor (targets with \"meters\": true)");
    }
    println!("\nAny other address can be mapped in learn mode.");
}

fn config_path(args: &Args) -> Result<PathBuf, String> {
    args.config
        .clone()
        .or_else(openorion_control::default_path)
        .ok_or_else(|| "no config path: set --config, XDG_CONFIG_HOME or HOME".into())
}

fn list_midi(cfg: &RemoteConfig) -> Result<(), String> {
    let (ins, outs) = midi::list()?;
    println!("inputs:");
    for (p, n) in &ins {
        let on = cfg.midi.input_active(n);
        println!("  [{}] {n}  ({})", if on { "x" } else { " " }, p.id());
    }
    println!("outputs:");
    for (p, n) in &outs {
        let on = cfg.midi.output_active(n);
        println!("  [{}] {n}  ({})", if on { "x" } else { " " }, p.id());
    }
    Ok(())
}

async fn run(args: Args) -> Result<(), String> {
    let path = config_path(&args)?;
    let mut cfg = RemoteConfig::load(&path).map_err(|e| e.to_string())?;
    if args.list_midi {
        return list_midi(&cfg);
    }
    if let Some(l) = &args.osc_listen {
        cfg.osc.listen = l.clone();
    }
    let socket = args
        .socket
        .clone()
        .unwrap_or_else(openorion_ipc::socket_path);

    let udp = if cfg.osc.listen.is_empty() {
        None
    } else {
        let s = UdpSocket::bind(&cfg.osc.listen)
            .await
            .map_err(|e| format!("OSC listen {}: {e}", cfg.osc.listen))?;
        tracing::info!(addr = %s.local_addr().map_err(|e| e.to_string())?, "OSC listening");
        Some(s)
    };
    let listen = match &udp {
        Some(s) => Some(s.local_addr().map_err(|e| e.to_string())?.to_string()),
        None => None,
    };
    let mut targets = Vec::new();
    for t in &cfg.osc.targets {
        match t.addr.to_socket_addrs().ok().and_then(|mut a| a.next()) {
            Some(addr) => targets.push(Target {
                addr,
                cfg: t.clone(),
            }),
            None => tracing::warn!(addr = %t.addr, "OSC target does not resolve; skipped"),
        }
    }

    let (midi_tx, mut midi_rx) = mpsc::unbounded_channel();
    let midi = if args.no_midi {
        None
    } else {
        match midi::spawn(cfg.midi.clone(), midi_tx.clone()) {
            Ok(tx) => Some(tx),
            Err(e) => {
                tracing::warn!("MIDI off: {e}");
                None
            }
        }
    };

    let mut engine = Engine::new(cfg, listen, targets);
    let (ev_tx, mut ev_rx) = mpsc::unbounded_channel();
    let (out_tx, out_rx) = mpsc::unbounded_channel();
    tokio::spawn(daemon_link(socket, engine.wants_meters(), ev_tx, out_rx));

    let mut send = Sender {
        udp: udp.as_ref(),
        v4: None,
        v6: None,
        daemon: out_tx,
        midi,
        config: path,
        next_id: 1,
    };
    let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        .map_err(|e| e.to_string())?;
    let mut buf = vec![0u8; 65536];
    let mut last_refusal: Option<String> = None;
    loop {
        let deadline = engine
            .next_deadline()
            .unwrap_or_else(|| Instant::now() + Duration::from_secs(3600));
        let outs = tokio::select! {
            Some(ev) = ev_rx.recv() => match ev {
                Link::Up => engine.connected(),
                Link::Down => {
                    engine.disconnected();
                    Vec::new()
                }
                Link::Msg(m) => {
                    if let DaemonMsg::Result { ok: false, id, error } = &m {
                        // A controller sends many ops; say each reason once.
                        if *error != last_refusal {
                            tracing::warn!(id, "op refused: {}", error.as_deref().unwrap_or("?"));
                            last_refusal = error.clone();
                        }
                    } else if matches!(m, DaemonMsg::Result { ok: true, .. }) {
                        last_refusal = None;
                    }
                    engine.daemon(m, Instant::now())
                }
            },
            r = recv_osc(udp.as_ref(), &mut buf) => match r {
                Ok((n, from)) => osc_packets(&buf[..n])
                    .into_iter()
                    .flat_map(|m| engine.osc_in(from, m, Instant::now()))
                    .collect(),
                Err(e) => {
                    tracing::debug!("OSC receive: {e}");
                    Vec::new()
                }
            },
            Some(ev) = midi_rx.recv() => match ev {
                MidiEvent::In { device, bytes } => engine.midi_in(&device, &bytes, Instant::now()),
                MidiEvent::Ports { inputs, outputs } => engine.ports(inputs, outputs),
                MidiEvent::Ready { device } => engine.midi_ready(&device),
            },
            _ = tokio::time::sleep_until(deadline.into()) => engine.tick(Instant::now()),
            _ = tokio::signal::ctrl_c() => break,
            _ = term.recv() => break,
        };
        for o in outs {
            send.out(o).await;
        }
    }
    tracing::info!("shutting down");
    Ok(())
}

async fn recv_osc(udp: Option<&UdpSocket>, buf: &mut [u8]) -> std::io::Result<(usize, SocketAddr)> {
    match udp {
        Some(s) => s.recv_from(buf).await,
        None => std::future::pending().await,
    }
}

/// The messages of a packet, bundles flattened.
fn osc_packets(b: &[u8]) -> Vec<rosc::OscMessage> {
    fn walk(p: OscPacket, v: &mut Vec<rosc::OscMessage>) {
        match p {
            OscPacket::Message(m) => v.push(m),
            OscPacket::Bundle(b) => b.content.into_iter().for_each(|p| walk(p, v)),
        }
    }
    let mut v = Vec::new();
    match decoder::decode_udp(b) {
        Ok((_, p)) => walk(p, &mut v),
        Err(e) => tracing::debug!("bad OSC packet: {e}"),
    }
    v
}

struct Sender<'a> {
    udp: Option<&'a UdpSocket>,
    v4: Option<UdpSocket>,
    v6: Option<UdpSocket>,
    daemon: mpsc::UnboundedSender<ClientMsg>,
    midi: Option<std::sync::mpsc::Sender<MidiCmd>>,
    config: PathBuf,
    next_id: u64,
}

impl Sender<'_> {
    async fn out(&mut self, o: Out) {
        match o {
            Out::Op(op) => {
                let id = self.next_id;
                self.next_id += 1;
                let _ = self.daemon.send(ClientMsg::Op { id, op });
            }
            Out::Remote(m) => match serde_json::to_value(&m) {
                Ok(msg) => {
                    let _ = self.daemon.send(ClientMsg::Remote { msg });
                }
                Err(e) => tracing::warn!("remote message: {e}"),
            },
            Out::Osc { target, msg, reply } => {
                let Ok(bytes) = encoder::encode(&OscPacket::Message(msg)) else {
                    return;
                };
                let sock = match (reply, self.udp) {
                    (true, Some(s)) => Some(s),
                    _ => self.target_socket(target).await,
                };
                if let Some(s) = sock
                    && let Err(e) = s.send_to(&bytes, target).await
                {
                    tracing::debug!(%target, "OSC send: {e}");
                }
            }
            Out::Midi { device, bytes } => {
                if let Some(m) = &self.midi {
                    let _ = m.send(MidiCmd::Send { device, bytes });
                }
            }
            Out::Save(cfg) => match cfg.save(&self.config) {
                Ok(()) => tracing::info!(path = %self.config.display(), "config saved"),
                Err(e) => tracing::error!("saving config: {e}"),
            },
            Out::MidiPorts(c) => {
                if let Some(m) = &self.midi {
                    let _ = m.send(MidiCmd::Config(c));
                }
            }
        }
    }

    /// An unbound socket of the target's family, made on first use.
    async fn target_socket(&mut self, target: SocketAddr) -> Option<&UdpSocket> {
        let (slot, any) = if target.is_ipv4() {
            (&mut self.v4, "0.0.0.0:0")
        } else {
            (&mut self.v6, "[::]:0")
        };
        if slot.is_none() {
            match UdpSocket::bind(any).await {
                Ok(s) => *slot = Some(s),
                Err(e) => {
                    tracing::warn!("OSC send socket: {e}");
                    return None;
                }
            }
        }
        slot.as_ref()
    }
}

enum Link {
    Up,
    Down,
    Msg(DaemonMsg),
}

/// Keep a connection to openoriond: hello, subscribe, then relay lines both
/// ways. Reconnects with backoff while the daemon is down.
async fn daemon_link(
    socket: PathBuf,
    meters: bool,
    ev: mpsc::UnboundedSender<Link>,
    mut out: mpsc::UnboundedReceiver<ClientMsg>,
) {
    let mut backoff = Duration::from_millis(250);
    let mut warned = false;
    loop {
        match UnixStream::connect(&socket).await {
            Ok(s) => {
                backoff = Duration::from_millis(250);
                warned = false;
                // Whatever was queued while down is stale.
                while out.try_recv().is_ok() {}
                match session(s, meters, &ev, &mut out).await {
                    Ok(()) => tracing::info!("openoriond closed the connection"),
                    Err(e) => tracing::warn!("openoriond: {e}"),
                }
                if ev.send(Link::Down).is_err() {
                    return;
                }
            }
            Err(e) => {
                if !warned {
                    tracing::warn!(socket = %socket.display(), "openoriond not reachable ({e}); retrying");
                    warned = true;
                }
            }
        }
        if ev.is_closed() {
            return;
        }
        tokio::time::sleep(backoff).await;
        backoff = (backoff * 2).min(Duration::from_secs(5));
    }
}

async fn session(
    s: UnixStream,
    meters: bool,
    ev: &mpsc::UnboundedSender<Link>,
    out: &mut mpsc::UnboundedReceiver<ClientMsg>,
) -> Result<(), String> {
    let (r, mut w) = s.into_split();
    let mut lines = BufReader::new(r).lines();
    let write = async |w: &mut tokio::net::unix::OwnedWriteHalf, m: &ClientMsg| {
        let mut b = serde_json::to_vec(m).map_err(|e| e.to_string())?;
        b.push(b'\n');
        w.write_all(&b).await.map_err(|e| e.to_string())
    };
    write(&mut w, &ClientMsg::Hello { v: IPC_VERSION }).await?;
    write(&mut w, &ClientMsg::Subscribe { meters }).await?;
    let mut up = false;
    loop {
        tokio::select! {
            l = lines.next_line() => {
                let Some(l) = l.map_err(|e| e.to_string())? else {
                    return Ok(());
                };
                let m: DaemonMsg = match serde_json::from_str(&l) {
                    Ok(m) => m,
                    Err(e) => {
                        tracing::debug!("daemon message: {e}");
                        continue;
                    }
                };
                if let DaemonMsg::Hello { v, daemon, sim } = &m {
                    tracing::info!(%daemon, sim, v, "connected to openoriond");
                    if *v < REMOTE_SINCE {
                        tracing::warn!("openoriond speaks v{v}: no learn relay to the UI");
                    }
                    if !up {
                        up = true;
                        ev.send(Link::Up).map_err(|e| e.to_string())?;
                    }
                    continue;
                }
                ev.send(Link::Msg(m)).map_err(|e| e.to_string())?;
            }
            m = out.recv() => {
                let Some(m) = m else { return Ok(()) };
                write(&mut w, &m).await?;
            }
        }
    }
}
