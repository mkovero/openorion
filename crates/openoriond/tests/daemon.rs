//! openoriond on `--sim`: hello, state, two clients see each other's ops,
//! snapshot/restore through the socket.

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use openorion_ipc::client::Client;
use openorion_ipc::{DaemonMsg, Health, Op};
use openorion_proto::*;

struct Daemon {
    child: Child,
    socket: PathBuf,
    _dir: tempfile::TempDir,
}

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn fixture() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/dump1.json")
}

fn start() -> Daemon {
    start_with(&[])
}

fn start_with(extra: &[&std::ffi::OsStr]) -> Daemon {
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("openorion.sock");
    let child = Command::new(env!("CARGO_BIN_EXE_openoriond"))
        .args(extra)
        .arg("--sim")
        .arg(fixture())
        .arg("--socket")
        .arg(&socket)
        .arg("--lock")
        .arg(dir.path().join("openorion.lock"))
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    while !socket.exists() {
        assert!(
            Instant::now() < deadline,
            "openoriond did not create its socket"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    Daemon {
        child,
        socket,
        _dir: dir,
    }
}

/// Connect and wait until the daemon has synced (state arrives).
fn synced_client(d: &Daemon) -> (Client, DeviceState) {
    let mut c = Client::connect(&d.socket).unwrap();
    assert!(c.info().sim);
    c.subscribe(false).unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        assert!(Instant::now() < deadline, "no state within 10 s");
        match c.recv(Duration::from_millis(200)).unwrap() {
            Some(DaemonMsg::State { state }) => return (c, *state),
            Some(DaemonMsg::Health {
                health: Health::Hung | Health::NoCyclic,
                detail,
            }) => panic!("sim unhealthy: {detail:?}"),
            _ => {}
        }
    }
}

fn level_op(ch: u8, db: u8) -> Op {
    Op::Mixer {
        mixer: MixerId::new(3).unwrap(),
        ch,
        level: Some(Atten::new(db).unwrap()),
        pan: None,
        mute: None,
        solo: None,
        send: None,
    }
}

#[test]
fn two_clients_see_each_others_ops() {
    let d = start();
    let (mut a, state) = synced_client(&d);
    assert_eq!(
        state.info.firmware,
        Version {
            major: 1,
            minor: 50
        }
    );
    let (mut b, _) = synced_client(&d);

    b.op(level_op(1, 30)).unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut seen = false;
    while !seen {
        assert!(Instant::now() < deadline, "A never saw B's op");
        if let Some(DaemonMsg::Diff { path, value }) = a.recv(Duration::from_millis(100)).unwrap() {
            seen = path.starts_with("/mixers/3") && value.to_string().contains("30");
        }
    }
    // And the other way round, checked through get_state.
    a.op(level_op(2, 12)).unwrap();
    std::thread::sleep(Duration::from_millis(300));
    let s = b.get_state().unwrap();
    assert_eq!(s.mixers[3].channels[0].level.get(), 30);
    assert_eq!(s.mixers[3].channels[1].level.get(), 12);
}

#[test]
fn snapshot_restore_and_errors() {
    let d = start();
    let (mut c, _) = synced_client(&d);
    let snap = c.snapshot().unwrap();
    c.op(level_op(5, 40)).unwrap();
    std::thread::sleep(Duration::from_millis(300));

    let plan = c.restore(snap.clone(), true).unwrap();
    assert_eq!(plan.len(), 1, "{plan:?}");
    assert_eq!(c.get_state().unwrap().mixers[3].channels[4].level.get(), 40);

    c.restore(snap.clone(), false).unwrap();
    std::thread::sleep(Duration::from_millis(500));
    let s = c.get_state().unwrap();
    assert_eq!(s.mixers, snap.state.mixers);
    assert!(c.restore(snap, true).unwrap().is_empty());

    // An invalid op comes back as a daemon error, not a hang.
    let r = c.op(Op::Mixer {
        mixer: MixerId::new(3).unwrap(),
        ch: 40,
        level: None,
        pan: None,
        mute: None,
        solo: None,
        send: None,
    });
    assert!(r.is_err(), "{r:?}");
}

#[test]
fn second_daemon_on_same_socket_refuses() {
    let d = start();
    let out = Command::new(env!("CARGO_BIN_EXE_openoriond"))
        .arg("--sim")
        .arg(fixture())
        .arg("--socket")
        .arg(&d.socket)
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("already listening"));
}

fn eq_gain(s: &DeviceState, inst: usize, band: usize) -> i16 {
    match &s.afx.configs[&AfxType::Equalizer][inst].params {
        AfxParams::Eq(eq) => eq.bands[band].gain,
        p => panic!("{p:?}"),
    }
}

/// The daemon keeps the AFX configs in its state file and, after a
/// (re)connect, puts back instances that came up at factory defaults.
#[test]
fn afx_settings_come_back_after_power_up() {
    let keep = tempfile::tempdir().unwrap();
    let file = keep.path().join("afx.json");
    let args = [std::ffi::OsStr::new("--afx-state"), file.as_os_str()];

    // First run: the copy is written.
    {
        let d = start_with(&args);
        let (_c, state) = synced_client(&d);
        assert_eq!(eq_gain(&state, 3, 2), 0, "fixture EQ 3 at defaults");
        let deadline = Instant::now() + Duration::from_secs(5);
        while !file.exists() {
            assert!(Instant::now() < deadline, "state file not written");
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    // What we had set before the power cycle: EQ 3 band 2 at +6 dB.
    let mut v: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&file).unwrap()).unwrap();
    v["configs"]["Equalizer"][3]["params"]["bands"][2]["gain"] = 600.into();
    std::fs::write(&file, v.to_string()).unwrap();

    // Second run: the (simulated) device came up with defaults.
    let d = start_with(&args);
    let (mut c, _) = synced_client(&d);
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let s = c.get_state().unwrap();
        if eq_gain(&s, 3, 2) == 600 {
            break;
        }
        assert!(Instant::now() < deadline, "EQ 3 not restored");
        std::thread::sleep(Duration::from_millis(100));
    }
}

#[test]
fn afx_restore_can_be_turned_off() {
    let keep = tempfile::tempdir().unwrap();
    let file = keep.path().join("afx.json");
    let d = start_with(&[
        std::ffi::OsStr::new("--afx-state"),
        file.as_os_str(),
        std::ffi::OsStr::new("--no-afx-restore"),
    ]);
    let _ = synced_client(&d);
    std::thread::sleep(Duration::from_millis(500));
    assert!(!file.exists(), "nothing kept when turned off");
}

/// Next `remote` message, skipping state events.
fn next_remote(c: &mut Client) -> serde_json::Value {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        assert!(Instant::now() < deadline, "no remote message within 5 s");
        if let Some(DaemonMsg::Remote { msg }) = c.recv(Duration::from_millis(100)).unwrap() {
            return msg;
        }
    }
}

/// `remote` is relayed unchanged to the other subscribed clients, not back
/// to the sender and not to v1 clients; oversize messages are refused.
#[test]
fn remote_messages_are_relayed() {
    use std::io::{BufRead, BufReader, Write};

    let d = start();
    let (mut a, _) = synced_client(&d);
    let (mut b, _) = synced_client(&d);

    // A v1 client: hello is answered with v1, and remote is not relayed.
    let mut old = std::os::unix::net::UnixStream::connect(&d.socket).unwrap();
    old.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    old.write_all(b"{\"t\":\"hello\",\"v\":1}\n{\"t\":\"subscribe\",\"meters\":false}\n")
        .unwrap();
    let mut old_lines = BufReader::new(old.try_clone().unwrap()).lines();
    let hello: serde_json::Value =
        serde_json::from_str(&old_lines.next().unwrap().unwrap()).unwrap();
    assert_eq!(hello["t"], "hello");
    assert_eq!(hello["v"], 1);

    a.send_remote(serde_json::json!({"x": 1})).unwrap();
    assert_eq!(next_remote(&mut b), serde_json::json!({"x": 1}));

    // Too large: A gets a refusal, B nothing.
    let big = "y".repeat(openorion_ipc::REMOTE_MAX_BYTES + 1);
    a.send_remote(serde_json::json!({ "big": big })).unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        assert!(
            Instant::now() < deadline,
            "no refusal for the oversize message"
        );
        match a.recv(Duration::from_millis(100)).unwrap() {
            Some(DaemonMsg::Result {
                id: 0,
                ok: false,
                error,
            }) => {
                assert!(error.unwrap().contains("too large"));
                break;
            }
            Some(DaemonMsg::Remote { msg }) => panic!("sender got a remote back: {msg}"),
            _ => {}
        }
    }
    b.send_remote(serde_json::json!({"from": "b"})).unwrap();
    // A sees B's marker and never its own messages.
    assert_eq!(next_remote(&mut a), serde_json::json!({"from": "b"}));
    a.send_remote(serde_json::json!({"x": 2})).unwrap();
    // B: the next relayed message after {"x":1} is {"x":2}; the oversize
    // one was dropped.
    assert_eq!(next_remote(&mut b), serde_json::json!({"x": 2}));

    // The v1 client saw state traffic but no remote.
    std::thread::sleep(Duration::from_millis(200));
    old.set_read_timeout(Some(Duration::from_millis(300)))
        .unwrap();
    while let Some(Ok(line)) = old_lines.next() {
        let m: serde_json::Value = serde_json::from_str(&line).unwrap();
        assert_ne!(m["t"], "remote", "v1 client got {line}");
    }
}
