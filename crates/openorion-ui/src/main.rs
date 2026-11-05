//! openorion-ui: egui control panel for the Orion32 HD (DESIGN.md 8.2).
//!
//! Live mode (default): connects to openoriond over its unix socket, shows the
//! daemon's state and meters and sends edits as `openorion_ipc::Op`s
//! (`bridge.rs`). Offline mode (`--demo` or `--state FILE`): renders a
//! static `DeviceState` with synthetic meters; edits only change the local
//! copy and are listed in the outbox.

mod app;
mod bridge;
mod demo;
mod labels;
mod learn;
mod meters;
mod ops;
mod state;
mod tabs;
mod theme;
mod topbar;
mod widgets;

use std::path::PathBuf;
use std::sync::Arc;

const USAGE: &str = "usage: openorion-ui [--socket PATH] [--demo | --state FILE]

  --socket PATH  openoriond socket (default: $XDG_RUNTIME_DIR/openorion.sock)
  --demo         offline: built-in demo state, nothing is sent
  --state FILE   offline: load a DeviceState (or Snapshot) JSON file";

#[derive(Debug, Clone, PartialEq, Eq)]
enum Mode {
    Live(PathBuf),
    Demo,
    File(PathBuf),
}

fn parse_args(args: impl IntoIterator<Item = String>) -> Result<Mode, String> {
    let mut it = args.into_iter();
    let mut socket = None;
    let mut offline: Option<Mode> = None;
    while let Some(a) = it.next() {
        let m = match a.as_str() {
            "--state" => Mode::File(PathBuf::from(it.next().ok_or("--state needs a FILE")?)),
            s if s.starts_with("--state=") => Mode::File(PathBuf::from(&s["--state=".len()..])),
            "--demo" => Mode::Demo,
            "--socket" => {
                socket = Some(PathBuf::from(it.next().ok_or("--socket needs a PATH")?));
                continue;
            }
            s if s.starts_with("--socket=") => {
                socket = Some(PathBuf::from(&s["--socket=".len()..]));
                continue;
            }
            "-h" | "--help" => return Err(String::new()),
            other => return Err(format!("unknown argument: {other}")),
        };
        if offline.replace(m).is_some() {
            return Err("--demo and --state are exclusive".into());
        }
    }
    match (offline, socket) {
        (Some(_), Some(_)) => Err("--socket is for live mode; drop --demo/--state".into()),
        (Some(m), None) => Ok(m),
        (None, s) => Ok(Mode::Live(s.unwrap_or_else(openorion_ipc::socket_path))),
    }
}

fn main() -> eframe::Result {
    let mode = match parse_args(std::env::args().skip(1)) {
        Ok(m) => m,
        Err(e) => {
            if !e.is_empty() {
                eprintln!("openorion-ui: {e}");
            }
            eprintln!("{USAGE}");
            std::process::exit(if e.is_empty() { 0 } else { 2 });
        }
    };
    let state = match &mode {
        Mode::File(p) => match demo::load_state(p) {
            Ok(s) => state::AppState::new(s, p.display().to_string()),
            Err(e) => {
                eprintln!("openorion-ui: {e}");
                std::process::exit(1);
            }
        },
        Mode::Demo => {
            let mut s = state::AppState::new(demo::demo_state(), "demo");
            s.remote = Some(demo::demo_remote());
            s
        }
        Mode::Live(p) => state::AppState::live(p.display().to_string()),
    };

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("openorion")
            .with_app_id("openorion")
            .with_inner_size([1280.0, 820.0])
            .with_min_inner_size([900.0, 600.0]),
        ..Default::default()
    };
    eframe::run_native(
        "openorion-ui",
        options,
        Box::new(move |cc| {
            theme::apply(&cc.egui_ctx);
            let bridge = match mode {
                Mode::Live(p) => {
                    let ctx = cc.egui_ctx.clone();
                    Some(bridge::Bridge::spawn(
                        p,
                        Arc::new(move || ctx.request_repaint()),
                    ))
                }
                _ => None,
            };
            Ok(Box::new(app::OrionApp::new(state, bridge)))
        }),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn cli() {
        let live = Mode::Live(openorion_ipc::socket_path());
        assert_eq!(parse_args(args(&[])), Ok(live));
        assert_eq!(
            parse_args(args(&["--state", "a.json"])),
            Ok(Mode::File(PathBuf::from("a.json")))
        );
        assert_eq!(
            parse_args(args(&["--state=b.json"])),
            Ok(Mode::File(PathBuf::from("b.json")))
        );
        assert_eq!(parse_args(args(&["--demo"])), Ok(Mode::Demo));
        assert_eq!(
            parse_args(args(&["--socket", "/x.sock"])),
            Ok(Mode::Live(PathBuf::from("/x.sock")))
        );
        assert!(parse_args(args(&["--state"])).is_err());
        assert!(parse_args(args(&["--demo", "--state", "a"])).is_err());
        assert!(parse_args(args(&["--demo", "--socket", "a"])).is_err());
        assert!(parse_args(args(&["--bogus"])).is_err());
    }
}
