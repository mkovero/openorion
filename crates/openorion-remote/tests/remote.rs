//! openorion-remote against openoriond `--sim` (no MIDI, no hardware): OSC
//! in → op → diff, value feedback to an OSC target, and learn through the
//! daemon's `remote` relay. Skipped when the openoriond binary is not built
//! (`cargo build -p openoriond`, or test the workspace).

use std::net::{SocketAddr, UdpSocket};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use openorion_ipc::DaemonMsg;
use openorion_ipc::client::Client;
use openorion_proto::{DeviceState, MixerChannel};
use rosc::{OscMessage, OscPacket, OscType, decoder, encoder};
use serde_json::{Value, json};

struct Proc(Child);

impl Drop for Proc {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn openoriond() -> Option<PathBuf> {
    let p = Path::new(env!("CARGO_BIN_EXE_openorion-remote"))
        .parent()?
        .join("openoriond");
    p.exists().then_some(p)
}

struct Rig {
    _daemon: Proc,
    _remote: Proc,
    client: Client,
    state: DeviceState,
    /// The remote's OSC listen address.
    listen: SocketAddr,
    /// Our OSC target socket.
    target: UdpSocket,
    config: PathBuf,
    _dir: tempfile::TempDir,
}

fn wait_for<T>(c: &mut Client, what: &str, mut f: impl FnMut(&DaemonMsg) -> Option<T>) -> T {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        if let Some(m) = c.recv(Duration::from_millis(100)).unwrap()
            && let Some(v) = f(&m)
        {
            return v;
        }
    }
}

fn remote_t(m: &DaemonMsg) -> Option<(String, Value)> {
    match m {
        DaemonMsg::Remote { msg } => Some((msg["t"].as_str()?.to_owned(), msg.clone())),
        _ => None,
    }
}

fn start() -> Option<Rig> {
    let Some(daemon) = openoriond() else {
        eprintln!("openoriond not built; skipping (cargo build -p openoriond)");
        return None;
    };
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("openorion.sock");
    let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/dump1.json");
    let d = Proc(
        Command::new(daemon)
            .arg("--sim")
            .arg(fixture)
            .arg("--socket")
            .arg(&socket)
            .arg("--lock")
            .arg(dir.path().join("openorion.lock"))
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap(),
    );
    let deadline = Instant::now() + Duration::from_secs(10);
    while !socket.exists() {
        assert!(Instant::now() < deadline, "no daemon socket");
        std::thread::sleep(Duration::from_millis(20));
    }

    let target = UdpSocket::bind("127.0.0.1:0").unwrap();
    target
        .set_read_timeout(Some(Duration::from_millis(200)))
        .unwrap();
    let config = dir.path().join("remote.json");
    let cfg = json!({
        "osc": {
            "listen": "127.0.0.1:0",
            "targets": [{"addr": target.local_addr().unwrap().to_string(), "values": true}]
        }
    });
    std::fs::write(&config, cfg.to_string()).unwrap();
    let r = Proc(
        Command::new(env!("CARGO_BIN_EXE_openorion-remote"))
            .arg("--config")
            .arg(&config)
            .arg("--socket")
            .arg(&socket)
            .arg("--no-midi")
            .env("XDG_CONFIG_HOME", dir.path())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap(),
    );

    let mut client = Client::connect(&socket).unwrap();
    client.subscribe(false).unwrap();
    let state = wait_for(&mut client, "state", |m| match m {
        DaemonMsg::State { state } => Some((**state).clone()),
        _ => None,
    });
    // Ask until the remote is connected and answers.
    let deadline = Instant::now() + Duration::from_secs(10);
    let listen = loop {
        assert!(Instant::now() < deadline, "no announce from the remote");
        client.send_remote(json!({"t": "query"})).unwrap();
        let until = Instant::now() + Duration::from_millis(300);
        let mut got = None;
        while got.is_none() && Instant::now() < until {
            if let Some(m) = client.recv(Duration::from_millis(50)).unwrap()
                && let Some((t, v)) = remote_t(&m)
                && t == "announce"
            {
                got = v["listen"].as_str().map(|s| s.parse().unwrap());
            }
        }
        if let Some(l) = got {
            break l;
        }
    };
    Some(Rig {
        _daemon: d,
        _remote: r,
        client,
        state,
        listen,
        target,
        config,
        _dir: dir,
    })
}

fn send_osc(to: SocketAddr, addr: &str, args: Vec<OscType>) {
    let s = UdpSocket::bind("127.0.0.1:0").unwrap();
    let b = encoder::encode(&OscPacket::Message(OscMessage {
        addr: addr.into(),
        args,
    }))
    .unwrap();
    s.send_to(&b, to).unwrap();
}

/// Wait for an OSC message to `addr` on the target socket.
fn recv_osc(s: &UdpSocket, addr: &str) -> OscMessage {
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut buf = [0u8; 4096];
    loop {
        assert!(Instant::now() < deadline, "no OSC {addr} at the target");
        let Ok((n, _)) = s.recv_from(&mut buf) else {
            continue;
        };
        if let Ok((_, OscPacket::Message(m))) = decoder::decode_udp(&buf[..n])
            && m.addr == addr
        {
            return m;
        }
    }
}

#[test]
fn osc_sets_and_feeds_back() {
    let Some(mut r) = start() else { return };
    // Mixer 4 channel 1 (silent): pick a level it is not at now.
    let cur = r.state.mixers[3].channels[0].level.get();
    let (norm, want) = if cur == 72 { (0.5, 48) } else { (0.25, 72) };
    send_osc(
        r.listen,
        "/orion/mixer/4/ch/1/level",
        vec![OscType::Float(norm)],
    );
    wait_for(&mut r.client, "the level diff", |m| match m {
        DaemonMsg::Diff { path, value } if path == "/mixers/3/channels/0" => {
            let c: MixerChannel = serde_json::from_value(value.clone()).ok()?;
            (c.level.get() == want).then_some(())
        }
        _ => None,
    });
    let fb = recv_osc(&r.target, "/orion/mixer/4/ch/1/level");
    assert_eq!(fb.args, vec![OscType::Float(norm)]);
}

#[test]
fn learn_through_the_relay() {
    let Some(mut r) = start() else { return };
    let c = &mut r.client;
    c.send_remote(json!({"t": "learn_start"})).unwrap();
    wait_for(c, "learning announce", |m| {
        let (t, v) = remote_t(m)?;
        (t == "announce" && v["learning"] == true).then_some(())
    });
    c.send_remote(json!({"t": "learn_select", "param": "mixer/4/ch/2/level"}))
        .unwrap();
    // The select has no answer; give the relay a moment to deliver it.
    std::thread::sleep(Duration::from_millis(200));
    send_osc(r.listen, "/my/fader", vec![OscType::Float(0.3)]);
    let learned = wait_for(c, "learned", |m| {
        let (t, v) = remote_t(m)?;
        (t == "learned").then_some(v)
    });
    assert_eq!(learned["param"], "mixer/4/ch/2/level");
    assert_eq!(learned["source"]["addr"], "/my/fader");
    c.send_remote(json!({"t": "learn_stop", "save": true}))
        .unwrap();
    wait_for(c, "announce after stop", |m| {
        let (t, v) = remote_t(m)?;
        (t == "announce" && v["learning"] == false).then_some(())
    });
    let saved: Value = serde_json::from_str(&std::fs::read_to_string(&r.config).unwrap()).unwrap();
    assert_eq!(saved["mappings"][0]["param"], "mixer/4/ch/2/level");
    assert_eq!(saved["osc"]["listen"], "127.0.0.1:0", "config kept");
}
