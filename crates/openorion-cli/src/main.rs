//! `openorion` CLI (DESIGN.md sec. 8.1). WP8.
//!
//! Talks to openoriond over its socket by default; `--direct` runs a Session
//! in-process instead (hidraw, or `--sim DUMP`). Channels, mixers, AFX
//! instances and bands are 1-based in text; `--json` prints the model
//! (0-based) as JSON.

mod backend;
mod cmds;
mod diff;
mod parse;

use std::io::{IsTerminal, Write};
use std::path::PathBuf;

use clap::{Parser, Subcommand, ValueEnum};

#[derive(Parser, Debug)]
#[command(
    name = "openorion",
    version,
    about = "Control the Antelope Orion32 HD (through openoriond, or --direct)",
    after_help = "Channels, mixers, AFX instances and EQ bands are 1-based. Levels are dB of \
attenuation written as negative numbers (-30, 0, -inf).\n\
Disruptive operations (clock, rate, power, presets, routing to physical outputs, \
restores that include them) need --yes, or a y/N answer on a terminal."
)]
struct Cli {
    /// Machine-readable JSON output.
    #[arg(long, global = true)]
    json: bool,
    /// openoriond socket [default: $XDG_RUNTIME_DIR/openorion.sock].
    #[arg(long, global = true, value_name = "PATH")]
    socket: Option<PathBuf>,
    /// No daemon: run a session in this process (hidraw, or --sim).
    #[arg(long, global = true)]
    direct: bool,
    /// With --direct: simulate a device seeded from an orionread dump or
    /// an `openorion snapshot save` file.
    #[arg(long, global = true, value_name = "DUMP", requires = "direct")]
    sim: Option<PathBuf>,
    /// With --direct: session lock [default: $XDG_RUNTIME_DIR/openorion.lock].
    #[arg(long, global = true, value_name = "PATH")]
    lock: Option<PathBuf>,
    /// Confirm disruptive operations.
    #[arg(long, short = 'y', global = true)]
    yes: bool,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand, Debug)]
enum Cmd {
    /// Clock, rate, lock, volumes, preset, health.
    Status,
    /// Live text meters (Ctrl-C to stop).
    Meters {
        /// a, b, mixer or all.
        #[arg(long, default_value = "all")]
        bank: String,
        /// Stop after this many frames.
        #[arg(long)]
        count: Option<u32>,
    },
    /// Routing matrix.
    Route {
        #[command(subcommand)]
        sub: RouteCmd,
    },
    /// Mixers 1-4.
    Mixer {
        #[command(subcommand)]
        sub: MixerCmd,
    },
    /// Show or set the main / line-out volume (dB, e.g. -20 or -inf).
    Volume {
        target: Option<VolTarget>,
        #[arg(allow_hyphen_values = true)]
        db: Option<String>,
    },
    /// Show or set the main / line-out mute.
    Mute {
        target: Option<VolTarget>,
        /// on|off
        state: Option<String>,
    },
    /// Show or set the clock source (disruptive). External sources are
    /// checked for lock; without lock the previous source is restored.
    Clock {
        source: Option<String>,
        /// Seconds to wait for lock on an external source.
        #[arg(long, default_value_t = 10.0)]
        lock_timeout: f64,
        /// Keep the new source even if it does not lock.
        #[arg(long)]
        no_revert: bool,
    },
    /// Show or set the sample rate (disruptive), e.g. 96k, 44.1k.
    Rate { rate: Option<String> },
    /// Show or set trims (dBu, 10..24).
    Trim {
        target: Option<TrimTarget>,
        #[arg(allow_hyphen_values = true)]
        dbu: Option<String>,
        /// One channel (1-32); default all.
        #[arg(long)]
        ch: Option<String>,
    },
    /// Show or set the oscillator.
    Osc {
        /// 1k or 440 (both sides).
        #[arg(long)]
        freq: Option<String>,
        #[arg(long)]
        freq_l: Option<String>,
        #[arg(long)]
        freq_r: Option<String>,
        /// 0, -6, -12 or -18 dBFS.
        #[arg(long, allow_hyphen_values = true)]
        level: Option<String>,
        #[arg(long)]
        mute_l: Option<String>,
        #[arg(long)]
        mute_r: Option<String>,
    },
    /// Mixer reverb.
    Reverb {
        #[command(subcommand)]
        sub: ReverbCmd,
    },
    /// AFX chains and instances.
    Afx {
        #[command(subcommand)]
        sub: AfxCmd,
    },
    /// Device preset slots 1-5 (disruptive).
    Preset {
        #[command(subcommand)]
        sub: PresetCmd,
    },
    /// Host-side snapshots of the whole device state.
    Snapshot {
        #[command(subcommand)]
        sub: SnapCmd,
    },
    /// Show or set power: on|standby (disruptive).
    Power { state: Option<String> },
    /// Re-read the whole state from the device.
    Resync,
    /// Control-channel health, whether settings are accepted, and what to
    /// do about a fault. Exit code 1 unless healthy.
    Health {
        /// Ask the device again: two reads in a row, then a resync. Reads
        /// only, so audio is not disturbed. Recovers a transient hang.
        #[arg(long)]
        recheck: bool,
    },
    /// USB reset of the Orion, then a resync (disruptive). Refused while
    /// its USB audio streams, since the reset cuts the stream; --force
    /// resets anyway. When the control channel hangs, only a power cycle helps.
    Reset {
        #[arg(long)]
        force: bool,
    },
    /// Show or set the front-panel brightness (0-100).
    Brightness { value: Option<u8> },
    /// Show or set S-MUX: `smux in|out on|off`.
    Smux {
        which: Option<SmuxWhich>,
        state: Option<String>,
    },
    /// Show or set the S/PDIF sample-rate converter: on|off.
    Src { state: Option<String> },
    /// Send a raw set payload (hex, `byte0 = pid | nparams << 6 ...`).
    ///
    /// Only whitelisted set pids are accepted, and the payload must be
    /// canonical (it is decoded with Set::decode and must re-encode to the
    /// same bytes). The daemon only takes semantic ops, so the decoded set
    /// is mapped back to an op and sent like any other command: the core
    /// rebuilds the message from its cache, which can differ from the given
    /// bytes in fields the op does not carry. Payloads that do not decode
    /// are refused. Disruptive sets still need --yes.
    Raw {
        /// Required: acknowledge you know what the bytes do.
        #[arg(long = "i-know")]
        i_know: bool,
        /// Hex bytes, spaces allowed.
        #[arg(required = true, num_args = 1..)]
        hex: Vec<String>,
    },
}

#[derive(Subcommand, Debug)]
enum RouteCmd {
    /// Show one bank, or all.
    Show { bank: Option<String> },
    /// Route SRC[:CH] to BANK[:CH], e.g. `route set "MIX CH4:1" "ADAT IN:1" --count 2`.
    Set {
        output: String,
        source: String,
        /// Consecutive channels.
        #[arg(long, default_value_t = 1)]
        count: usize,
    },
}

#[derive(Subcommand, Debug)]
enum MixerCmd {
    /// Show mixer M (1-4).
    Show { mixer: String },
    /// Set channel CH (1-32 or master) of mixer M.
    Set {
        mixer: String,
        ch: String,
        /// dB, e.g. -30, 0 or -inf.
        #[arg(long, allow_hyphen_values = true)]
        level: Option<String>,
        /// -30..+30, C, L12, R12.
        #[arg(long, allow_hyphen_values = true)]
        pan: Option<String>,
        #[arg(long)]
        mute: Option<String>,
        #[arg(long)]
        solo: Option<String>,
        /// Reverb send, dB.
        #[arg(long, allow_hyphen_values = true)]
        send: Option<String>,
        /// Stereo-link the channel's pair: on|off.
        #[arg(long)]
        link: Option<String>,
        /// Do not mirror level/mute/solo/send to the linked partner.
        #[arg(long)]
        no_link_follow: bool,
    },
}

#[derive(Subcommand, Debug)]
enum ReverbCmd {
    Show,
    /// Set the reverb of mixer M's slot (values 0-255 as on the wire).
    Set {
        #[arg(long, default_value = "1")]
        mixer: String,
        #[arg(long)]
        room: Option<u8>,
        #[arg(long)]
        color: Option<u8>,
        #[arg(long)]
        predelay: Option<u8>,
        #[arg(long)]
        density: Option<u8>,
        #[arg(long)]
        early: Option<u8>,
        #[arg(long)]
        late_delay: Option<u8>,
        #[arg(long)]
        richness: Option<u8>,
        #[arg(long)]
        time: Option<u8>,
        #[arg(long)]
        level: Option<u8>,
        #[arg(long)]
        on: Option<String>,
    },
}

#[derive(Subcommand, Debug)]
enum AfxCmd {
    /// Chains, or one instance's parameters (`afx show ClearQ:1`).
    Show { slot: Option<String> },
    /// Edit chain CH (1-16): add TYPE[:INST] | remove TYPE[:INST] | clear | set TYPE:INST...
    Chain {
        chain: String,
        action: ChainAction,
        items: Vec<String>,
    },
    /// ClearQ instance INST (1-16), band BAND (1-5).
    Eq {
        inst: String,
        band: String,
        /// Hz.
        #[arg(long)]
        freq: Option<u16>,
        #[arg(long)]
        q: Option<f64>,
        /// dB, -24..+12.
        #[arg(long, allow_hyphen_values = true)]
        gain: Option<f64>,
        /// lowshelf, highshelf, peak, lpf, hpf.
        #[arg(long = "type")]
        ftype: Option<String>,
    },
    /// PowerFFC instance INST (1-16).
    Comp {
        inst: String,
        /// ms.
        #[arg(long)]
        attack: Option<f64>,
        /// ms.
        #[arg(long)]
        release: Option<f64>,
        /// e.g. 4 for 4:1.
        #[arg(long)]
        ratio: Option<f64>,
        /// Makeup gain, dB.
        #[arg(long)]
        gain: Option<f64>,
        /// dBFS (e.g. -20).
        #[arg(long, allow_hyphen_values = true)]
        threshold: Option<f64>,
        /// dB.
        #[arg(long)]
        knee: Option<u8>,
        /// "peak" or RMS window.
        #[arg(long)]
        taw: Option<String>,
        #[arg(long)]
        linked: Option<u8>,
    },
    /// Bypass TYPE:INST on|off.
    Bypass { slot: String, state: String },
    /// Set raw model fields: `afx raw VEQ-1A:1 name=value ...`.
    Raw {
        slot: String,
        #[arg(allow_hyphen_values = true)]
        assigns: Vec<String>,
    },
    /// Stereo-link the chain pair containing CH: on|off.
    Link { chain: String, state: String },
}

#[derive(ValueEnum, Clone, Copy, Debug, PartialEq, Eq)]
enum ChainAction {
    Add,
    Remove,
    Clear,
    Set,
}

#[derive(Subcommand, Debug)]
enum PresetCmd {
    Save { slot: u8 },
    Recall { slot: u8 },
}

#[derive(Subcommand, Debug)]
enum SnapCmd {
    /// Save the live state to FILE.
    Save { file: PathBuf },
    /// Diff FILE against the live state, or against FILE2.
    Diff {
        file: PathBuf,
        file2: Option<PathBuf>,
    },
    /// Restore FILE (only the differences are sent).
    Restore {
        file: PathBuf,
        #[arg(long)]
        dry_run: bool,
    },
}

#[derive(ValueEnum, Clone, Copy, Debug, PartialEq, Eq)]
enum VolTarget {
    Main,
    Line,
}

#[derive(ValueEnum, Clone, Copy, Debug, PartialEq, Eq)]
enum TrimTarget {
    LineIn,
    LineOut,
    Monitor,
}

#[derive(ValueEnum, Clone, Copy, Debug, PartialEq, Eq)]
enum SmuxWhich {
    In,
    Out,
}

/// Why a command failed. Refusals (unconfirmed disruptive ops, raw
/// payloads that are not allowed) exit 2, errors exit 1.
#[derive(Debug)]
pub enum Fail {
    Error(String),
    Refused(String),
}

impl From<String> for Fail {
    fn from(s: String) -> Fail {
        Fail::Error(s)
    }
}

impl From<&str> for Fail {
    fn from(s: &str) -> Fail {
        Fail::Error(s.to_string())
    }
}

/// A command's result: text for humans, JSON for `--json`.
pub struct Out {
    pub text: String,
    pub json: serde_json::Value,
    /// Exit code; non-zero when the command ran but did not reach its goal
    /// (e.g. the clock did not lock).
    pub code: i32,
}

fn main() {
    let cli = Cli::parse();
    let json = cli.json;
    let mut ctx = cmds::Ctx::new(cmds::Global {
        json: cli.json,
        socket: cli.socket.clone(),
        direct: cli.direct,
        sim: cli.sim.clone(),
        lock: cli.lock.clone(),
        yes: cli.yes,
    });
    let r = cmds::run(&mut ctx, cli.cmd);
    let warnings = ctx.finish();
    for w in &warnings {
        eprintln!("openorion: warning: {w}");
    }
    let mut stdout = std::io::stdout();
    match r {
        Ok(Some(out)) => {
            if json {
                let _ = writeln!(stdout, "{}", out.json);
            } else {
                let _ = write!(stdout, "{}", out.text);
            }
            if out.code != 0 {
                let _ = stdout.flush();
                std::process::exit(out.code);
            }
        }
        Ok(None) => {}
        Err(f) => {
            let (msg, code) = match f {
                Fail::Error(m) => (m, 1),
                Fail::Refused(m) => (m, 2),
            };
            if json {
                let _ = writeln!(
                    stdout,
                    "{}",
                    serde_json::json!({"ok": false, "error": msg, "refused": code == 2})
                );
            }
            eprintln!("openorion: {msg}");
            std::process::exit(code);
        }
    }
}

/// True when we may ask the user on the terminal.
pub fn interactive() -> bool {
    std::io::stdin().is_terminal() && std::io::stderr().is_terminal()
}

/// Ask a yes/no question on the terminal.
pub fn ask(question: &str, default_yes: bool) -> bool {
    eprint!(
        "{question} {} ",
        if default_yes { "[Y/n]" } else { "[y/N]" }
    );
    let _ = std::io::stderr().flush();
    let mut line = String::new();
    if std::io::stdin().read_line(&mut line).is_err() {
        return false;
    }
    match line.trim().to_ascii_lowercase().as_str() {
        "" => default_yes,
        "y" | "yes" => true,
        _ => false,
    }
}
