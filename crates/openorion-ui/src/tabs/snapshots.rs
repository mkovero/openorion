//! Snapshots: device presets 1-5 and host snapshot files (save, load,
//! diff vs. live, restore). Restore goes through openoriond
//! (`ClientMsg::Restore`): a dry run shows the plan, then Apply sends it.

use std::path::PathBuf;

use egui::{RichText, Ui};
use openorion_ipc::Op;
use openorion_proto::{DeviceState, PresetSlot, Risk, Snapshot};
use serde_json::Value;

use super::card;
use crate::demo::parse_state;
use crate::state::{AppState, RestoreState};
use crate::theme;
use crate::widgets::{dim, mono, toggle};

/// One differing leaf: JSON pointer, current value, file value.
#[derive(Debug, Clone, PartialEq)]
pub struct DiffLine {
    pub path: String,
    pub ours: Value,
    pub theirs: Value,
}

/// Leaf-level differences between two JSON values (JSON-pointer paths, the
/// same form as `DaemonMsg::Diff`).
pub fn json_diff(a: &Value, b: &Value) -> Vec<DiffLine> {
    fn walk(a: &Value, b: &Value, path: &mut String, out: &mut Vec<DiffLine>) {
        match (a, b) {
            (Value::Object(x), Value::Object(y)) => {
                let keys: std::collections::BTreeSet<&String> = x.keys().chain(y.keys()).collect();
                for k in keys {
                    let len = path.len();
                    path.push('/');
                    path.push_str(&k.replace('~', "~0").replace('/', "~1"));
                    walk(
                        x.get(k).unwrap_or(&Value::Null),
                        y.get(k).unwrap_or(&Value::Null),
                        path,
                        out,
                    );
                    path.truncate(len);
                }
            }
            (Value::Array(x), Value::Array(y)) if x.len() == y.len() => {
                for (i, (p, q)) in x.iter().zip(y).enumerate() {
                    let len = path.len();
                    path.push_str(&format!("/{i}"));
                    walk(p, q, path, out);
                    path.truncate(len);
                }
            }
            _ if a == b => {}
            _ => out.push(DiffLine {
                path: path.clone(),
                ours: a.clone(),
                theirs: b.clone(),
            }),
        }
    }
    let mut out = Vec::new();
    walk(a, b, &mut String::new(), &mut out);
    out
}

pub fn state_diff(ours: &DeviceState, theirs: &DeviceState) -> Vec<DiffLine> {
    match (serde_json::to_value(ours), serde_json::to_value(theirs)) {
        (Ok(a), Ok(b)) => json_diff(&a, &b),
        _ => Vec::new(),
    }
}

pub struct View {
    path: String,
    loaded: Option<(String, DeviceState)>,
    message: String,
}

impl Default for View {
    fn default() -> Self {
        View {
            path: "orion-snapshot.json".to_owned(),
            loaded: None,
            message: String::new(),
        }
    }
}

fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

fn presets(ui: &mut Ui, st: &mut AppState) {
    card(ui, "DEVICE PRESETS", |ui| {
        let cur = st.device.status.current_preset;
        ui.label(dim(if cur == 0 {
            "current state: modified (no preset)".to_owned()
        } else {
            format!("current state: preset {cur}")
        }));
        egui::Grid::new("preset-grid")
            .spacing([10.0, 6.0])
            .show(ui, |ui| {
                for n in 1..=5u8 {
                    let slot = PresetSlot::new(n).expect("1..=5");
                    toggle(ui, cur == n, &format!(" {n} "), false);
                    if ui.button("Recall").clicked() {
                        st.submit(Op::PresetRecall { slot });
                    }
                    if ui.button("Save").clicked() {
                        st.submit(Op::PresetSave { slot });
                    }
                    ui.end_row();
                }
            });
    });
}

fn host(ui: &mut Ui, st: &mut AppState, view: &mut View) {
    card(ui, "HOST SNAPSHOTS", |ui| {
        ui.horizontal(|ui| {
            ui.label(dim("file"));
            ui.add(egui::TextEdit::singleline(&mut view.path).desired_width(320.0));
            if ui.button("Save current").clicked() {
                let snap = Snapshot {
                    version: 1,
                    taken: now_unix(),
                    state: st.device.clone(),
                };
                view.message = match serde_json::to_string_pretty(&snap)
                    .map_err(|e| e.to_string())
                    .and_then(|s| std::fs::write(&view.path, s).map_err(|e| e.to_string()))
                {
                    Ok(()) => format!("saved {}", view.path),
                    Err(e) => format!("save failed: {e}"),
                };
            }
            if ui.button("Load & diff").clicked() {
                let p = PathBuf::from(&view.path);
                view.message = match std::fs::read_to_string(&p)
                    .map_err(|e| e.to_string())
                    .and_then(|t| parse_state(&t))
                {
                    Ok(s) => {
                        view.loaded = Some((view.path.clone(), s));
                        st.restore = RestoreState::Idle;
                        format!("loaded {}", view.path)
                    }
                    Err(e) => format!("load failed: {e}"),
                };
            }
        });
        if !view.message.is_empty() {
            ui.label(dim(&view.message));
        }
        if let Some((name, snap)) = &view.loaded {
            let diff = state_diff(&st.device, snap);
            ui.separator();
            let mut request = None;
            ui.horizontal(|ui| {
                ui.label(RichText::new(format!("{} differences vs {name}", diff.len())).strong());
                let can = st.is_live() && st.ready();
                if ui
                    .add_enabled(can, egui::Button::new("Restore… (dry run)"))
                    .on_hover_text("ask openoriond which ops would restore this snapshot")
                    .on_disabled_hover_text("needs a connection to openoriond")
                    .clicked()
                {
                    request = Some(true);
                }
            });
            restore_panel(ui, st, &mut request);
            if let Some(dry_run) = request {
                let snapshot = Snapshot {
                    version: 1,
                    taken: now_unix(),
                    state: snap.clone(),
                };
                st.request_restore(snapshot, dry_run);
            }
            egui::ScrollArea::vertical()
                .max_height(360.0)
                .show(ui, |ui| {
                    egui::Grid::new("diff-grid")
                        .striped(true)
                        .spacing([16.0, 2.0])
                        .show(ui, |ui| {
                            ui.label(dim("path"));
                            ui.label(dim("current"));
                            ui.label(dim("file"));
                            ui.end_row();
                            for d in diff.iter().take(500) {
                                ui.label(mono(&d.path));
                                ui.label(mono(d.ours.to_string()));
                                ui.label(mono(d.theirs.to_string()).color(theme::ACCENT));
                                ui.end_row();
                            }
                        });
                    if diff.len() > 500 {
                        ui.label(dim(format!("… {} more", diff.len() - 500)));
                    }
                });
        }
    });
}

/// Dry-run plan and apply. Sets `request` to `Some(false)` on Apply.
fn restore_panel(ui: &mut Ui, st: &mut AppState, request: &mut Option<bool>) {
    match &st.restore {
        RestoreState::Idle => {}
        RestoreState::Waiting { dry_run } => {
            ui.horizontal(|ui| {
                ui.spinner();
                ui.label(dim(if *dry_run {
                    "asking openoriond for the restore plan…"
                } else {
                    "restoring…"
                }));
            });
        }
        RestoreState::Plan { ops, applied } => {
            let disruptive = ops.iter().filter(|o| o.risk() == Risk::Disruptive).count();
            let mut dismiss = false;
            card(
                ui,
                if *applied { "RESTORED" } else { "RESTORE PLAN" },
                |ui| {
                    if ops.is_empty() {
                        ui.label(dim("nothing to do: the device already matches"));
                    }
                    egui::ScrollArea::vertical()
                        .id_salt("restore-plan")
                        .max_height(200.0)
                        .show(ui, |ui| {
                            for op in ops {
                                ui.horizontal(|ui| {
                                    let (txt, col) = crate::app::risk_style(op.risk());
                                    ui.label(mono(format!("{txt:<10}")).color(col));
                                    ui.label(crate::ops::describe(op));
                                });
                            }
                        });
                    ui.horizontal(|ui| {
                    if !*applied && !ops.is_empty() {
                        let mut text = RichText::new(format!("Apply {} ops", ops.len())).strong();
                        let mut b = egui::Button::new(text.clone());
                        if disruptive > 0 {
                            text = text.color(egui::Color32::WHITE);
                            b = egui::Button::new(text).fill(theme::RED);
                        }
                        if ui.add(b).clicked() {
                            *request = Some(false);
                        }
                    }
                    if ui.button(if *applied { "Dismiss" } else { "Cancel" }).clicked() {
                        dismiss = true;
                    }
                    if !*applied && disruptive > 0 {
                        ui.label(
                            RichText::new(format!(
                                "{disruptive} disruptive op(s): clock, rate, power or physical routing"
                            ))
                            .color(theme::RED),
                        );
                    }
                });
                },
            );
            if dismiss {
                st.restore = RestoreState::Idle;
            }
        }
    }
}

pub fn show(ui: &mut Ui, st: &mut AppState, view: &mut View) {
    egui::ScrollArea::vertical()
        .auto_shrink([false, false])
        .show(ui, |ui| {
            ui.horizontal_top(|ui| {
                presets(ui, st);
                host(ui, st, view);
            });
        });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::demo::demo_state;
    use serde_json::json;

    #[test]
    fn diff_leaves() {
        let a = json!({"a": 1, "b": [1, 2, {"c/d": true}], "e": "x"});
        let b = json!({"a": 1, "b": [1, 3, {"c/d": false}], "f": 2});
        let d = json_diff(&a, &b);
        let paths: Vec<_> = d.iter().map(|l| l.path.as_str()).collect();
        assert_eq!(paths, ["/b/1", "/b/2/c~1d", "/e", "/f"]);
        assert!(json_diff(&a, &a).is_empty());
    }

    #[test]
    fn state_diff_finds_mixer_change() {
        let a = demo_state();
        let mut b = a.clone();
        b.mixers[3].channels[0].mute = true;
        let d = state_diff(&a, &b);
        assert_eq!(d.len(), 1);
        assert_eq!(d[0].path, "/mixers/3/channels/0/mute");
    }
}
