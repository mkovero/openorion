//! Against a private dummy JACK server (feature `jack`). Self-skips when
//! `jackd` is missing or will not start. Never touches the default server
//! or audio hardware: the server has a unique name and the dummy backend.

#![cfg(feature = "jack")]

use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use jack::{Client, ClientOptions};
use openorion_jack::jackrun::{ALIAS_PREFIX, Namer, PRETTY_NAME, discover_ports};
use openorion_jack::naming::{Dir, PortMatch};
use openorion_proto::decode::empty_state;
use openorion_proto::{OutputBank, Source, SourceGroup};

struct Server(Child);

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn start(name: &str) -> Option<Server> {
    let child = Command::new("jackd")
        .args(["-r", "-n", name, "-d", "dummy", "-r", "48000", "-p", "256"])
        .args(["-C", "4", "-P", "4"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    Some(Server(child))
}

fn connect(name: &str) -> Option<Client> {
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        if let Ok((c, _)) = Client::new(name, ClientOptions::NO_START_SERVER) {
            return Some(c);
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    None
}

#[test]
fn names_dummy_ports() {
    let server_name = format!("openorion-test-{}", std::process::id());
    let Some(mut server) = start(&server_name) else {
        eprintln!("skip: jackd not available");
        return;
    };
    // SAFETY: the only test in this binary; set before any JACK call.
    unsafe { std::env::set_var("JACK_DEFAULT_SERVER", &server_name) };
    let Some(client) = connect("openorion-jack-test") else {
        let _ = server.0.kill();
        eprintln!("skip: private jackd did not come up");
        return;
    };
    assert_eq!(client.sample_rate(), 48_000);

    // Dummy ports don't mention the Orion: only the client fallback finds them.
    let m = PortMatch::new("Orion32");
    assert!(discover_ports(&client, &m, None).is_empty());
    let ports = discover_ports(&client, &m, Some("system"));
    assert_eq!(ports.iter().filter(|p| p.dir == Dir::Capture).count(), 4);
    assert_eq!(ports.iter().filter(|p| p.dir == Dir::Playback).count(), 4);

    let mut st = empty_state();
    let rec = st
        .routing
        .iter_mut()
        .find(|r| r.bank == OutputBank::UsbRec1)
        .unwrap();
    rec.sources[0] = Source::Input {
        group: SourceGroup::AdatIn,
        chan: 0,
    };

    let mut namer = Namer::new(true);
    assert_eq!(namer.apply(&client, &st, &ports), 8);
    assert_eq!(namer.apply(&client, &st, &ports), 0, "unchanged: no-op");

    let port = client.port_by_name("system:capture_1").unwrap();
    let uuid = unsafe { jack::jack_sys::jack_port_uuid(port.raw()) };
    match client.property_get(uuid, PRETTY_NAME) {
        Some(p) => assert_eq!(p.value(), "ADAT IN 1"),
        None => eprintln!("note: this jackd has no metadata support"),
    }
    let aliases = port.aliases().unwrap();
    assert!(
        aliases.iter().any(|a| a == "openorion:rec01-ADAT_IN_1"),
        "{aliases:?}"
    );

    // Routing change: only the changed port is renamed.
    let rec = st
        .routing
        .iter_mut()
        .find(|r| r.bank == OutputBank::UsbRec1)
        .unwrap();
    rec.sources[0] = Source::Input {
        group: SourceGroup::LineIn,
        chan: 2,
    };
    assert_eq!(namer.apply(&client, &st, &ports), 1);
    let aliases = port.aliases().unwrap();
    assert_eq!(
        aliases
            .iter()
            .filter(|a| a.starts_with(ALIAS_PREFIX))
            .collect::<Vec<_>>(),
        vec!["openorion:rec01-LINE_IN_3"]
    );

    namer.restore(&client);
    assert!(client.property_get(uuid, PRETTY_NAME).is_none());
    assert!(
        !port
            .aliases()
            .unwrap()
            .iter()
            .any(|a| a.starts_with(ALIAS_PREFIX))
    );

    let probe = openorion_jack::jackrun::probe(
        "Orion32",
        std::path::Path::new("/nonexistent"),
        Some("system"),
    );
    assert_eq!(
        probe,
        openorion_jack::doctor::JackProbe::Running {
            rate: 48_000,
            buffer: 256,
            orion_ports: (4, 4),
            physical_ports: 8,
        }
    );
    drop(client);
}
