//! `openorion` end to end on the simulator: `--direct --sim` for most tests,
//! plus round trips through openoriond `--sim` when its binary is built.
//! Writes use mixer 4 and MIX CH4 (bank 14), silent on real hardware too.

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

use openorion_proto::*;
use serde_json::Value;

fn fixture() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/dump1.json")
}

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_openorion")
}

fn run(base: &[&str], args: &[&str]) -> Output {
    Command::new(bin())
        .args(base)
        .args(args)
        .stdin(Stdio::null())
        .output()
        .unwrap()
}

/// `openorion --direct --sim SEED --lock TMP args…`.
struct Direct {
    dir: tempfile::TempDir,
    seed: PathBuf,
}

impl Direct {
    fn new() -> Direct {
        Direct {
            dir: tempfile::tempdir().unwrap(),
            seed: fixture(),
        }
    }

    fn path(&self, name: &str) -> PathBuf {
        self.dir.path().join(name)
    }

    fn run(&self, args: &[&str]) -> Output {
        let seed = self.seed.to_str().unwrap().to_string();
        let lock = self.path("openorion.lock").to_str().unwrap().to_string();
        run(&["--direct", "--sim", &seed, "--lock", &lock], args)
    }

    fn ok(&self, args: &[&str]) -> String {
        let o = self.run(args);
        assert!(
            o.status.success(),
            "openorion {args:?} failed: {}",
            String::from_utf8_lossy(&o.stderr)
        );
        String::from_utf8(o.stdout).unwrap()
    }

    fn json(&self, args: &[&str]) -> Value {
        let mut a = vec!["--json"];
        a.extend_from_slice(args);
        serde_json::from_str(&self.ok(&a)).expect("--json output parses")
    }
}

fn stderr(o: &Output) -> String {
    String::from_utf8_lossy(&o.stderr).into_owned()
}

#[test]
fn status_text_and_json() {
    let d = Direct::new();
    let t = d.ok(&["status"]);
    assert!(t.contains("rate"), "{t}");
    assert!(t.contains("96k"), "{t}");
    assert!(t.contains("direct (sim)"), "{t}");
    let j = d.json(&["status"]);
    assert_eq!(j["status"]["sample_rate"], "R96k");
    assert_eq!(j["health"], "ok");
}

#[test]
fn route_show_and_set() {
    let d = Direct::new();
    let t = d.ok(&["route", "show", "MONITOR"]);
    assert!(t.contains("MONITOR 1-2"), "{t}");
    assert!(t.contains("MIX L/R1 1-2"), "{t}");
    let t = d.ok(&["route", "set", "MIX CH4:1", "ADAT IN:1", "--count", "2"]);
    assert!(t.contains("MIX CH4 1-2"), "{t}");
    assert!(t.contains("ADAT IN 1-2"), "{t}");
    let j = d.json(&["route", "set", "MIX CH4:3", "mute"]);
    assert_eq!(j["ops"][0]["op"], "route");
    assert_eq!(j["ops"][0]["bank"], "MixCh4");
    assert_eq!(j["ops"][0]["ch"], 2);
    // Doesn't fit.
    let o = d.run(&["route", "set", "MIX CH4:32", "ADAT IN:1", "--count", "2"]);
    assert!(!o.status.success());
}

#[test]
fn mixer_set_mirrors_linked_partner() {
    let d = Direct::new();
    let j = d.json(&[
        "mixer", "set", "4", "3", "--level", "-30", "--pan", "-12", "--link", "on",
    ]);
    let ops = j["ops"].as_array().unwrap();
    assert_eq!(ops.len(), 3, "{j}");
    assert_eq!(ops[0]["op"], "stereo_link");
    assert_eq!(ops[1]["ch"], 3);
    assert_eq!(ops[2]["ch"], 4);
    assert_eq!(ops[2]["level"], 30);
    assert_eq!(ops[2]["pan"], Value::Null, "pan does not follow");
    let ch = j["channels"].as_array().unwrap();
    assert_eq!(ch[0]["channel"]["level"], 30);
    assert_eq!(ch[0]["channel"]["pan"], 20);
    assert_eq!(ch[1]["channel"]["level"], 30);

    // Mixer 1 ch 1-2 are linked in the fixture; --no-link-follow sends one op.
    let j = d.json(&["mixer", "set", "1", "1", "--mute", "on", "--no-link-follow"]);
    assert_eq!(j["ops"].as_array().unwrap().len(), 1);
    let j = d.json(&["mixer", "set", "1", "2", "--mute", "on"]);
    assert_eq!(j["ops"].as_array().unwrap().len(), 2);
    assert_eq!(j["ops"][1]["ch"], 1);

    let t = d.ok(&["mixer", "show", "4"]);
    assert!(t.contains("MIXER 4"), "{t}");
    assert!(
        d.run(&["mixer", "set", "4", "1", "--level", "6"])
            .status
            .code()
            == Some(1)
    );
}

#[test]
fn disruptive_needs_yes() {
    let d = Direct::new();
    for args in [
        &["clock", "spdif"][..],
        &["rate", "48k"],
        &["preset", "recall", "1"],
        &["power", "standby"],
        &["route", "set", "LINE OUT 1", "mute"],
    ] {
        let o = d.run(args);
        assert_eq!(o.status.code(), Some(2), "{args:?}");
        assert!(stderr(&o).contains("--yes"), "{args:?}: {}", stderr(&o));
    }
    // Confirmed: the current rate again (no change on the sim).
    d.ok(&["--yes", "rate", "96k"]);
    // Silent ops need no confirmation.
    d.ok(&["brightness", "0"]);
}

#[test]
fn json_everywhere() {
    let d = Direct::new();
    for args in [
        &["route", "show"][..],
        &["mixer", "show", "4"],
        &["volume"],
        &["clock"],
        &["rate"],
        &["trim"],
        &["osc"],
        &["reverb", "show"],
        &["afx", "show"],
        &["smux"],
        &["src"],
        &["power"],
        &["brightness"],
    ] {
        d.json(args);
    }
    let lines = d.ok(&["--json", "meters", "--count", "2"]);
    let frames: Vec<Value> = lines
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    assert_eq!(frames.len(), 2);
    assert_eq!(frames[0]["bank_a"].as_array().unwrap().len(), 32);
    // Errors are JSON too.
    let o = d.run(&["--json", "clock", "spdif"]);
    let j: Value = serde_json::from_slice(&o.stdout).unwrap();
    assert_eq!(j["ok"], false);
    assert_eq!(j["refused"], true);
}

#[test]
fn afx_chain_and_params() {
    let d = Direct::new();
    let t = d.ok(&["afx", "chain", "16", "add", "ClearQ", "PowerFFC"]);
    assert!(t.contains("AFX 16"), "{t}");
    assert!(t.contains("ClearQ:1 > PowerFFC:1"), "{t}");
    let j = d.json(&["afx", "eq", "16", "2", "--gain", "-3.5", "--type", "peak"]);
    assert_eq!(j["ops"][0]["gain"], -350);
    assert_eq!(j["ops"][0]["band"], 1);
    let j = d.json(&["afx", "comp", "16", "--ratio", "4", "--threshold", "-20"]);
    assert_eq!(j["ops"][0]["params"]["ratio"], 400);
    assert_eq!(j["ops"][0]["params"]["threshold"], 20);
    let j = d.json(&["afx", "bypass", "PowerFFC:16", "on"]);
    assert_eq!(j["ops"][0]["enabled"], false);
}

#[test]
fn snapshot_save_diff_restore() {
    let d = Direct::new();
    let a = d.path("a.json");
    let b = d.path("b.json");
    d.ok(&["snapshot", "save", a.to_str().unwrap()]);
    let mut snap: Snapshot = serde_json::from_str(&std::fs::read_to_string(&a).unwrap()).unwrap();
    snap.state.mixers[3].channels[0].level = Atten::new(20).unwrap();
    std::fs::write(&b, serde_json::to_string(&snap).unwrap()).unwrap();

    let t = d.ok(&["snapshot", "diff", a.to_str().unwrap(), b.to_str().unwrap()]);
    assert!(t.contains("/mixers/3/channels/0/level"), "{t}");
    let t = d.ok(&["snapshot", "diff", a.to_str().unwrap()]);
    assert!(t.contains("no differences"), "{t}");
    let j = d.json(&["snapshot", "diff", b.to_str().unwrap()]);
    assert_eq!(j["changes"].as_array().unwrap().len(), 1);
    assert_eq!(j["changes"][0]["file"], 20);
    assert_eq!(j["changes"][0]["live"], 0);

    let j = d.json(&["snapshot", "restore", b.to_str().unwrap(), "--dry-run"]);
    assert_eq!(j["applied"], false);
    assert_eq!(j["ops"][0]["op"], "mixer");
    let j = d.json(&["snapshot", "restore", b.to_str().unwrap()]);
    assert_eq!(j["applied"], true);
    assert_eq!(j["ops"].as_array().unwrap().len(), 1);

    // A sim seeded from the snapshot matches it.
    let d2 = Direct {
        dir: tempfile::tempdir().unwrap(),
        seed: b.clone(),
    };
    let t = d2.ok(&["snapshot", "diff", b.to_str().unwrap()]);
    assert!(t.contains("no differences"), "{t}");
}

fn hex(b: &[u8]) -> String {
    b.iter()
        .map(|x| format!("{x:02x}"))
        .collect::<Vec<_>>()
        .join(" ")
}

#[test]
fn raw_needs_flag_and_whitelist() {
    let d = Direct::new();
    let good = Set::Brightness(Brightness::new(0).unwrap())
        .payload()
        .unwrap();
    // No --i-know.
    let o = d.run(&["raw", &hex(&good)]);
    assert_eq!(o.status.code(), Some(2));
    // Whitelisted, canonical: mapped to an op.
    let j = d.json(&["raw", "--i-know", &hex(&good)]);
    assert_eq!(j["ops"][0]["op"], "brightness");
    // pid 15 (set_config_feature) and pid 1 are not whitelisted.
    for b0 in [0x0f_u8, 0x01, 0x16] {
        let mut bad = good.clone();
        bad[0] = (bad[0] & 0xc0) | b0;
        let o = d.run(&["raw", "--i-know", &hex(&bad)]);
        assert_eq!(o.status.code(), Some(2), "pid {b0}");
        assert!(stderr(&o).contains("not whitelisted"), "{}", stderr(&o));
    }
    // Disruptive sets still need --yes.
    let rate = Set::SampleRate(SampleRate::R96k).payload().unwrap();
    assert_eq!(
        d.run(&["raw", "--i-know", &hex(&rate)]).status.code(),
        Some(2)
    );
}

// ---------------------------------------------------------------------------
// Through openoriond

struct Daemon {
    child: Child,
    socket: PathBuf,
    dir: tempfile::TempDir,
}

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn openoriond() -> Option<PathBuf> {
    let p = Path::new(bin()).parent()?.join("openoriond");
    p.exists().then_some(p)
}

impl Daemon {
    fn start(exe: &Path) -> Daemon {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("openorion.sock");
        let child = Command::new(exe)
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
        Daemon { child, socket, dir }
    }

    fn ok(&self, args: &[&str]) -> String {
        let o = run(&["--socket", self.socket.to_str().unwrap()], args);
        assert!(o.status.success(), "openorion {args:?}: {}", stderr(&o));
        String::from_utf8(o.stdout).unwrap()
    }
}

#[test]
fn daemon_round_trips() {
    let Some(exe) = openoriond() else {
        eprintln!("skipping: openoriond is not built (cargo build -p openoriond)");
        return;
    };
    let d = Daemon::start(&exe);
    let t = d.ok(&["status"]);
    assert!(t.contains("openoriond (sim)"), "{t}");

    let snap = d.dir.path().join("before.json");
    d.ok(&["snapshot", "save", snap.to_str().unwrap()]);

    d.ok(&["route", "set", "MIX CH4:5", "ADAT IN:3", "--count", "2"]);
    let t = d.ok(&["route", "show", "MIX CH4"]);
    assert!(t.contains("MIX CH4 5-6"), "{t}");
    assert!(t.contains("ADAT IN 3-4"), "{t}");

    d.ok(&["mixer", "set", "4", "7", "--level", "-12", "--link", "on"]);
    let j: Value = serde_json::from_str(&d.ok(&["--json", "mixer", "show", "4"])).unwrap();
    assert_eq!(j["mixer_state"]["channels"][6]["level"], 12);
    assert_eq!(j["mixer_state"]["channels"][7]["level"], 12);
    assert_eq!(j["links"][3], true);

    let t = d.ok(&["snapshot", "diff", snap.to_str().unwrap()]);
    assert!(t.contains("/mixers/3/channels/6/level"), "{t}");
    d.ok(&["snapshot", "restore", snap.to_str().unwrap()]);
    let t = d.ok(&["snapshot", "diff", snap.to_str().unwrap()]);
    assert!(t.contains("no differences"), "{t}");
}

#[test]
fn health_reports_and_rechecks() {
    let d = Direct::new();
    let j = d.json(&["health"]);
    assert_eq!(j["health"], "ok");
    assert_eq!(j["writable"], true);
    let j = d.json(&["health", "--recheck"]);
    assert_eq!(j["recheck"], true);
    assert!(d.ok(&["health"]).contains("accepted"));
}

/// The simulator never locks an external source: the CLI reverts and
/// exits 1 instead of reporting success.
#[test]
fn clock_without_lock_reverts_and_fails() {
    let d = Direct::new();
    let o = d.run(&["--yes", "--json", "clock", "spdif", "--lock-timeout", "0.3"]);
    assert_eq!(o.status.code(), Some(1), "{}", stderr(&o));
    let j: Value = serde_json::from_slice(&o.stdout).unwrap();
    assert_eq!(j["ok"], false);
    assert_eq!(j["locked"], false);
    assert_eq!(j["reverted"], true);
    for bad in ["inf", "NaN", "-1"] {
        let o = d.run(&["--yes", "clock", "spdif", &format!("--lock-timeout={bad}")]);
        assert_eq!(o.status.code(), Some(1), "{bad}: {}", stderr(&o));
        assert!(stderr(&o).contains("--lock-timeout"), "{bad}");
    }
}
