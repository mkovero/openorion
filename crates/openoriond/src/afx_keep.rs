//! Keep the AFX settings across power cycles.
//!
//! A power cycle resets the Orion's AFX configs to factory defaults while
//! routing and mixers survive (findings); the vendor software re-sends
//! them on connect. The daemon keeps a copy of the last good configs in a
//! state file and, right after each (re)connect, puts back every instance
//! that now holds factory defaults where the copy has something else
//! (`openorion_core::afx_power_up_target`). Instances with other values
//! are left alone. On by default; `--no-afx-restore` turns it off.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use openorion_core::{SessionHandle, afx_power_up_target, plan_restore};
use openorion_ipc::{DaemonMsg, Health};
use openorion_proto::{AfxConfig, AfxType, DeviceState};
use serde::{Deserialize, Serialize};

/// How often the copy is refreshed from the session cache.
const SAVE_EVERY: Duration = Duration::from_secs(10);

#[derive(Serialize, Deserialize)]
struct Saved {
    version: u32,
    /// The unit the configs belong to (never restored onto another).
    serial: String,
    configs: BTreeMap<AfxType, Vec<AfxConfig>>,
}

/// `$STATE_DIRECTORY/afx.json` under systemd (`StateDirectory=`), else
/// `$XDG_STATE_HOME/openorion/afx.json` (default `~/.local/state`).
pub fn default_path() -> Option<PathBuf> {
    if let Some(d) = std::env::var_os("STATE_DIRECTORY").filter(|v| !v.is_empty()) {
        // May list several directories, colon-separated; ours is the only one.
        let first = d
            .to_string_lossy()
            .split(':')
            .next()
            .unwrap_or("")
            .to_owned();
        return Some(PathBuf::from(first).join("afx.json"));
    }
    let base = std::env::var_os("XDG_STATE_HOME")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/state")))?;
    Some(base.join("openorion/afx.json"))
}

fn load(path: &Path) -> Option<Saved> {
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return None,
        Err(e) => {
            tracing::warn!("afx keep: {}: {e}", path.display());
            return None;
        }
    };
    match serde_json::from_str::<Saved>(&text) {
        Ok(s) if s.version == 1 => Some(s),
        Ok(s) => {
            tracing::warn!(
                "afx keep: {}: version {} unknown",
                path.display(),
                s.version
            );
            None
        }
        Err(e) => {
            tracing::warn!("afx keep: {}: {e}", path.display());
            None
        }
    }
}

fn store(path: &Path, saved: &Saved) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_vec_pretty(saved)?)?;
    std::fs::rename(&tmp, path)
}

fn snapshot(st: &DeviceState) -> Saved {
    Saved {
        version: 1,
        serial: st.info.passport.serial.clone(),
        configs: st.afx.configs.clone(),
    }
}

/// Runs for one session: restore after the first sync, then keep the copy
/// current. Ends when the session does (the caller aborts it).
pub async fn run(
    handle: SessionHandle,
    path: PathBuf,
    events: tokio::sync::broadcast::Sender<DaemonMsg>,
) {
    if handle.wait_synced().await.is_err() {
        return;
    }
    restore(&handle, &path, &events).await;
    let mut last: Option<BTreeMap<AfxType, Vec<AfxConfig>>> = None;
    loop {
        let h = handle.health();
        if !h.alive {
            return;
        }
        if h.health == Health::Ok {
            let st = handle.state();
            if !st.afx.configs.is_empty() && last.as_ref() != Some(&st.afx.configs) {
                match store(&path, &snapshot(&st)) {
                    Ok(()) => last = Some(st.afx.configs),
                    Err(e) => tracing::warn!("afx keep: saving {}: {e}", path.display()),
                }
            }
        }
        tokio::time::sleep(SAVE_EVERY).await;
    }
}

async fn restore(
    handle: &SessionHandle,
    path: &Path,
    events: &tokio::sync::broadcast::Sender<DaemonMsg>,
) {
    let Some(saved) = load(path) else {
        return;
    };
    let current = handle.state();
    if saved.serial != current.info.passport.serial {
        tracing::info!("afx keep: saved configs belong to another unit; not restoring");
        return;
    }
    let (target, picked) = afx_power_up_target(&current, &saved.configs);
    if picked.is_empty() {
        return;
    }
    let ops = plan_restore(&current, &target);
    tracing::info!(
        instances = ?picked,
        ops = ops.len(),
        "afx keep: AFX settings at factory defaults (power cycle?); restoring"
    );
    let n = ops.len();
    for op in ops {
        if let Err(e) = handle.op(op).await {
            tracing::warn!("afx keep: restore stopped: {e}");
            let _ = events.send(DaemonMsg::Notice {
                text: format!("Restoring AFX settings after power-up failed: {e}"),
            });
            return;
        }
    }
    let _ = events.send(DaemonMsg::Notice {
        text: format!(
            "Restored AFX settings after power-up ({} instance{}, {n} changes)",
            picked.len(),
            if picked.len() == 1 { "" } else { "s" }
        ),
    });
}
