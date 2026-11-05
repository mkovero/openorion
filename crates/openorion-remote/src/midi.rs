//! ALSA sequencer MIDI through midir, on one thread that owns every port.
//! It rescans the ports every `RESCAN` (hotplug: an enabled input that
//! reappears is opened again), sends input to the engine and writes
//! feedback.
//!
//! A device is an ALSA client name. Feedback for an input goes to the
//! outputs of the same client, or to `midi.out_ports[device]` when set.
//! Those outputs are opened together with the input; an output gets its
//! init bytes (`MidiConfig::init_for`) first, then the engine is told the
//! device is `Ready` so it can send the current values.

use std::collections::{BTreeSet, HashMap};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use midir::os::unix::{VirtualInput, VirtualOutput};
use midir::{
    Ignore, MidiInput, MidiInputConnection, MidiInputPort, MidiOutput, MidiOutputConnection,
    MidiOutputPort,
};
use openorion_control::{ANY_DEVICE, MidiConfig, port_matches};

use crate::engine::VIRTUAL_DEVICE;

/// ALSA client name of our connections (and of the virtual port).
pub const CLIENT: &str = "openorion-remote";
/// How often the port list is re-read.
pub const RESCAN: Duration = Duration::from_secs(2);
const SEQ: &str = "/dev/snd/seq";
/// Pause after init bytes, so the device has switched mode before the
/// first feedback reaches it.
const INIT_SETTLE: Duration = Duration::from_millis(100);

pub enum MidiCmd {
    Config(MidiConfig),
    Send { device: String, bytes: Vec<u8> },
}

pub enum MidiEvent {
    In {
        device: String,
        bytes: Vec<u8>,
    },
    Ports {
        inputs: Vec<String>,
        outputs: Vec<String>,
    },
    /// Input `device` was opened, with its feedback outputs (if any).
    Ready {
        device: String,
    },
}

/// `"Client:Port c:p"` (midir's ALSA port name) → `"Client"`.
pub fn client_name(port_name: &str) -> &str {
    let s = port_name.rsplit_once(' ').map_or(port_name, |(a, _)| a);
    s.rsplit_once(':').map_or(s, |(a, _)| a)
}

/// Every input and output port, as (port, client name).
/// Input and output ports with their client names.
pub type PortList = (Vec<(MidiInputPort, String)>, Vec<(MidiOutputPort, String)>);

pub fn list() -> Result<PortList, String> {
    if !std::path::Path::new(SEQ).exists() {
        return Err(format!("{SEQ} missing (no ALSA sequencer)"));
    }
    let i = MidiInput::new(CLIENT).map_err(|e| e.to_string())?;
    let o = MidiOutput::new(CLIENT).map_err(|e| e.to_string())?;
    let ins = i
        .ports()
        .into_iter()
        .filter_map(|p| {
            let n = i.port_name(&p).ok()?;
            Some((p, client_name(&n).to_owned()))
        })
        .collect();
    let outs = o
        .ports()
        .into_iter()
        .filter_map(|p| {
            let n = o.port_name(&p).ok()?;
            Some((p, client_name(&n).to_owned()))
        })
        .collect();
    Ok((ins, outs))
}

/// Sorted unique client names, without the system and our own.
fn client_set<P>(v: &[(P, String)]) -> Vec<String> {
    v.iter()
        .map(|(_, n)| n)
        .filter(|n| !openorion_control::is_system_port(n))
        .cloned()
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}

struct Midi {
    cfg: MidiConfig,
    events: tokio::sync::mpsc::UnboundedSender<MidiEvent>,
    /// Open inputs by port id: (client name, connection).
    ins: HashMap<String, (String, MidiInputConnection<()>)>,
    /// Open outputs by port id.
    outs: HashMap<String, (String, MidiOutputConnection)>,
    /// Outputs from the last scan.
    out_ports: Vec<(MidiOutputPort, String)>,
    virt_in: Option<MidiInputConnection<()>>,
    virt_out: Option<MidiOutputConnection>,
    reported: Option<(Vec<String>, Vec<String>)>,
}

/// Start the MIDI thread. Fails when there is no ALSA sequencer.
pub fn spawn(
    cfg: MidiConfig,
    events: tokio::sync::mpsc::UnboundedSender<MidiEvent>,
) -> Result<mpsc::Sender<MidiCmd>, String> {
    list()?;
    let (tx, rx) = mpsc::channel();
    std::thread::Builder::new()
        .name("openorion-midi".into())
        .spawn(move || {
            let mut m = Midi {
                cfg,
                events,
                ins: HashMap::new(),
                outs: HashMap::new(),
                out_ports: Vec::new(),
                virt_in: None,
                virt_out: None,
                reported: None,
            };
            m.open_virtual();
            m.run(rx);
        })
        .map_err(|e| e.to_string())?;
    Ok(tx)
}

fn input(
    events: tokio::sync::mpsc::UnboundedSender<MidiEvent>,
    device: String,
) -> impl FnMut(u64, &[u8], &mut ()) + Send + 'static {
    move |_, bytes, _| {
        let _ = events.send(MidiEvent::In {
            device: device.clone(),
            bytes: bytes.to_vec(),
        });
    }
}

impl Midi {
    fn open_virtual(&mut self) {
        if !self.cfg.virtual_port {
            return;
        }
        match MidiInput::new(CLIENT) {
            Ok(mut i) => {
                i.ignore(Ignore::All);
                let cb = input(self.events.clone(), VIRTUAL_DEVICE.into());
                match i.create_virtual(VIRTUAL_DEVICE, cb, ()) {
                    Ok(c) => self.virt_in = Some(c),
                    Err(e) => tracing::warn!("virtual MIDI input: {e}"),
                }
            }
            Err(e) => tracing::warn!("virtual MIDI input: {e}"),
        }
        match MidiOutput::new(CLIENT)
            .map_err(|e| e.to_string())
            .and_then(|o| o.create_virtual(VIRTUAL_DEVICE).map_err(|e| e.to_string()))
        {
            Ok(c) => self.virt_out = Some(c),
            Err(e) => tracing::warn!("virtual MIDI output: {e}"),
        }
    }

    fn run(&mut self, rx: mpsc::Receiver<MidiCmd>) {
        let mut next = Instant::now();
        loop {
            if Instant::now() >= next {
                self.scan();
                next = Instant::now() + RESCAN;
            }
            match rx.recv_timeout(next.saturating_duration_since(Instant::now())) {
                Ok(MidiCmd::Config(c)) => {
                    self.cfg = c;
                    next = Instant::now();
                }
                Ok(MidiCmd::Send { device, bytes }) => self.send(&device, &bytes),
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => return,
            }
        }
    }

    fn scan(&mut self) {
        let (ins, outs) = match list() {
            Ok(l) => l,
            Err(e) => {
                tracing::warn!("MIDI ports: {e}");
                return;
            }
        };
        // Close inputs that went away or were disabled.
        self.ins.retain(|id, (name, _)| {
            let keep =
                ins.iter().any(|(p, n)| p.id() == *id && n == name) && self.cfg.input_active(name);
            if !keep {
                tracing::info!(device = %name, "MIDI input closed");
            }
            keep
        });
        let mut opened_now = Vec::new();
        for (p, name) in &ins {
            if self.ins.contains_key(&p.id()) || !self.cfg.input_active(name) {
                continue;
            }
            let opened = MidiInput::new(CLIENT)
                .map_err(|e| e.to_string())
                .and_then(|mut i| {
                    i.ignore(Ignore::All);
                    let cb = input(self.events.clone(), name.clone());
                    i.connect(p, "openorion-in", cb, ())
                        .map_err(|e| e.to_string())
                });
            match opened {
                Ok(c) => {
                    tracing::info!(device = %name, "MIDI input opened");
                    self.ins.insert(p.id(), (name.clone(), c));
                    opened_now.push(name.clone());
                }
                Err(e) => tracing::warn!(device = %name, "MIDI input: {e}"),
            }
        }
        self.outs.retain(|id, (name, _)| {
            outs.iter().any(|(p, n)| p.id() == *id && n == name) && self.cfg.output_active(name)
        });
        let inames = client_set(&ins);
        let onames = client_set(&outs);
        self.out_ports = outs;
        for device in opened_now {
            for (p, name) in self.outputs_for(&device) {
                self.ensure_out(&p, &name);
            }
            let _ = self.events.send(MidiEvent::Ready { device });
        }
        let now = (inames, onames);
        if self.reported.as_ref() != Some(&now) {
            self.reported = Some(now.clone());
            let _ = self.events.send(MidiEvent::Ports {
                inputs: now.0,
                outputs: now.1,
            });
        }
    }

    /// Feedback for the controls of input `device`.
    fn send(&mut self, device: &str, bytes: &[u8]) {
        if device == VIRTUAL_DEVICE {
            if let Some(c) = &mut self.virt_out {
                let _ = c.send(bytes);
            }
            return;
        }
        for (p, name) in self.outputs_for(device) {
            if !self.ensure_out(&p, &name) {
                continue;
            }
            if let Some((_, c)) = self.outs.get_mut(&p.id())
                && let Err(e) = c.send(bytes)
            {
                tracing::debug!(device = %name, "MIDI send: {e}");
            }
        }
    }

    /// The outputs that get feedback for input `device`.
    fn outputs_for(&self, device: &str) -> Vec<(MidiOutputPort, String)> {
        let open_inputs: Vec<String> = self.ins.values().map(|(n, _)| n.clone()).collect();
        let pattern = self.cfg.out_ports.get(device).cloned();
        self.out_ports
            .iter()
            .filter(|(_, n)| {
                self.cfg.output_active(n)
                    && if device == ANY_DEVICE {
                        open_inputs.iter().any(|i| {
                            self.cfg
                                .out_ports
                                .get(i)
                                .map_or(i == n, |p| port_matches(p, n))
                        })
                    } else {
                        match &pattern {
                            Some(p) => port_matches(p, n),
                            None => n == device,
                        }
                    }
            })
            .cloned()
            .collect()
    }

    /// Open output `p` unless it is open, sending its init bytes first.
    /// False when it cannot be opened.
    fn ensure_out(&mut self, p: &MidiOutputPort, name: &str) -> bool {
        let id = p.id();
        if self.outs.contains_key(&id) {
            return true;
        }
        let c = MidiOutput::new(CLIENT)
            .map_err(|e| e.to_string())
            .and_then(|o| o.connect(p, "openorion-fb").map_err(|e| e.to_string()));
        let mut c = match c {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!(device = %name, "MIDI output: {e}");
                return false;
            }
        };
        tracing::info!(device = %name, "MIDI output opened");
        if let Some(init) = self.cfg.init_for(name) {
            let msgs = openorion_control::split_midi(&init);
            for m in &msgs {
                if let Err(e) = c.send(m) {
                    tracing::warn!(device = %name, "MIDI init: {e}");
                    break;
                }
                // A mode switch (SysEx) needs a moment before more input.
                if m[0] == 0xF0 {
                    std::thread::sleep(INIT_SETTLE);
                }
            }
            tracing::info!(device = %name, messages = msgs.len(), "MIDI init sent");
        }
        self.outs.insert(id, (name.to_owned(), c));
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn client_names() {
        assert_eq!(
            client_name("nanoKONTROL2:nanoKONTROL2 _ CTRL 20:0"),
            "nanoKONTROL2"
        );
        assert_eq!(
            client_name("Midi Through:Midi Through Port-0 14:0"),
            "Midi Through"
        );
        assert_eq!(client_name("plain"), "plain");
    }
}
