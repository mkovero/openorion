//! `openorion-jack` (DESIGN.md sec. 9). WP10.

use std::path::PathBuf;
use std::time::Duration;

use clap::{Parser, Subcommand};
use openorion_jack::DEFAULT_DEVICE_MATCH;
use openorion_jack::doctor::{self, DoctorInput};

#[derive(Parser, Debug)]
#[command(
    name = "openorion-jack",
    version,
    about = "JACK integration for the Orion32 HD: port names that follow \
             routing, sample-rate guard, MIDI control, setup doctor"
)]
struct Cli {
    /// openoriond socket (default: $XDG_RUNTIME_DIR/openorion.sock).
    #[arg(long, global = true)]
    socket: Option<PathBuf>,
    /// Case-insensitive substring identifying the Orion's ALSA card and
    /// JACK ports (port name or alias).
    #[arg(long, global = true, default_value = DEFAULT_DEVICE_MATCH)]
    device_match: String,
    /// JACK client whose physical ports are the Orion's when nothing
    /// matches --device-match (e.g. "system").
    #[arg(long, global = true)]
    client: Option<String>,
    /// /proc root (testing).
    #[arg(long, global = true, default_value = "/proc", hide = true)]
    proc_root: PathBuf,
    /// /sys root (testing).
    #[arg(long, global = true, default_value = "/sys", hide = true)]
    sys_root: PathBuf,

    #[command(subcommand)]
    cmd: Option<Cmd>,

    #[command(flatten)]
    run: RunArgs,
}

#[derive(clap::Args, Debug)]
struct RunArgs {
    /// Set the Orion's sample rate to JACK's when JACK starts or changes
    /// rate. Disruptive: audio on the Orion drops out while it relocks.
    #[arg(long)]
    follow_jack: bool,
    /// Also set port aliases (`openorion:rec01-ADAT_IN_1`) besides the
    /// metadata pretty-names.
    #[arg(long)]
    aliases: bool,
    /// MIDI control map (line format, see `openorion-jack --help-midi`).
    #[arg(long)]
    midi_map: Option<PathBuf>,
    /// Name the ports once and exit (names stay set).
    #[arg(long)]
    once: bool,
    /// JACK client name.
    #[arg(long, default_value = "openorion-jack")]
    name: String,
    /// Print the MIDI map format and exit.
    #[arg(long)]
    help_midi: bool,
}

#[derive(Subcommand, Debug)]
enum Cmd {
    /// Check the setup (read-only): ALSA card, USB link, JACK, openoriond.
    Doctor,
}

const MIDI_HELP: &str = "\
MIDI map: one binding per line, `#` starts a comment.

  cc <channel 1-16 | *> <controller 0-127> <target>

targets:
  mixer <1-4> ch <1-32> level|mute|solo|send
  mixer <1-4> master level|mute|solo|send
  volume main|line        (monitor / line-out volume)
  mute main|line

Levels: CC 127 = 0 dB, 0 = -inf. Switches: CC >= 64 = on.
";

fn main() {
    let cli = Cli::parse();
    let socket = cli
        .socket
        .clone()
        .unwrap_or_else(openorion_ipc::socket_path);
    match cli.cmd {
        Some(Cmd::Doctor) => {
            let checks = doctor::run(&DoctorInput {
                proc_root: cli.proc_root.clone(),
                sys_root: cli.sys_root.clone(),
                needle: cli.device_match.clone(),
                jack: jack_probe(&cli),
                daemon: doctor::probe_daemon(&socket, Duration::from_secs(3)),
            });
            for c in &checks {
                println!("{c}");
            }
            std::process::exit(doctor::exit_code(&checks));
        }
        None if cli.run.help_midi => print!("{MIDI_HELP}"),
        None => std::process::exit(run(&cli, socket)),
    }
}

#[cfg(feature = "jack")]
fn jack_probe(cli: &Cli) -> doctor::JackProbe {
    openorion_jack::jackrun::probe(&cli.device_match, &cli.proc_root, cli.client.as_deref())
}

#[cfg(not(feature = "jack"))]
fn jack_probe(_: &Cli) -> doctor::JackProbe {
    doctor::JackProbe::NotBuilt
}

#[cfg(feature = "jack")]
fn run(cli: &Cli, socket: PathBuf) -> i32 {
    let midi_map = match &cli.run.midi_map {
        None => None,
        Some(p) => {
            let parsed = std::fs::read_to_string(p)
                .map_err(|e| e.to_string())
                .and_then(|t| openorion_jack::midimap::MidiMap::parse(&t));
            match parsed {
                Ok(m) => Some(m),
                Err(e) => {
                    eprintln!("openorion-jack: {}: {e}", p.display());
                    return 2;
                }
            }
        }
    };
    let opts = openorion_jack::jackrun::RunOpts {
        socket,
        client_name: cli.run.name.clone(),
        device_match: cli.device_match.clone(),
        client_fallback: cli.client.clone(),
        follow_jack: cli.run.follow_jack,
        aliases: cli.run.aliases,
        midi_map,
        once: cli.run.once,
        proc_root: cli.proc_root.clone(),
    };
    match openorion_jack::jackrun::run(opts) {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("openorion-jack: {e}");
            1
        }
    }
}

#[cfg(not(feature = "jack"))]
fn run(_: &Cli, _: PathBuf) -> i32 {
    eprintln!(
        "openorion-jack: built without JACK support; rebuild with `--features jack`. \
         `openorion-jack doctor` works without it."
    );
    2
}
