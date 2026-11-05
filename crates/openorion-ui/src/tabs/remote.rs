//! Remote: openorion-remote status (OSC listen address and targets), MIDI
//! ports with enable boxes and activity lamps, and the mapping table.
//! Everything here edits the remote through relayed `RemoteMsg`s; its next
//! `Announce` is what is shown.

use egui::{RichText, Ui};
use openorion_control::{MapOpts, Mode, PortInfo, Takeover};

use super::card;
use crate::learn::{RemoteView, device_key, param_label};
use crate::state::AppState;
use crate::theme;
use crate::widgets::{dim, lamp, mono};

fn mode_name(m: Mode) -> &'static str {
    match m {
        Mode::Absolute => "absolute",
        Mode::Relative => "relative",
        Mode::Toggle => "toggle",
    }
}

fn takeover_name(t: Takeover) -> &'static str {
    match t {
        Takeover::Jump => "jump",
        Takeover::Pickup => "pickup",
    }
}

fn status(ui: &mut Ui, r: &RemoteView) {
    card(ui, "OPENORION-REMOTE", |ui| {
        egui::Grid::new("remote-status")
            .num_columns(2)
            .spacing([14.0, 6.0])
            .show(ui, |ui| {
                ui.label(dim("state"));
                if r.demo {
                    ui.label(dim("demo view: nothing is listening"));
                } else {
                    lamp(ui, theme::ACCENT, true, "running");
                }
                ui.end_row();
                ui.label(dim("OSC listen"));
                ui.label(mono(r.listen.as_deref().unwrap_or("off")));
                ui.end_row();
                ui.label(dim("learn"));
                ui.label(if r.learning { "session open" } else { "off" });
                ui.end_row();
            });
        ui.add_space(6.0);
        ui.label(RichText::new("OSC targets").strong().color(theme::TEXT_DIM));
        if r.osc_targets.is_empty() {
            ui.label(dim("none (add them to remote.json)"));
            return;
        }
        egui::Grid::new("osc-targets")
            .num_columns(3)
            .spacing([14.0, 4.0])
            .striped(true)
            .show(ui, |ui| {
                ui.label(dim("address"));
                ui.label(dim("values"));
                ui.label(dim("meters"));
                ui.end_row();
                for t in &r.osc_targets {
                    ui.label(mono(&t.addr));
                    ui.label(if t.values { "yes" } else { "no" });
                    ui.label(if t.meters {
                        format!("{} Hz", t.meter_hz)
                    } else {
                        "no".into()
                    });
                    ui.end_row();
                }
            });
        ui.label(dim("Targets are read-only here; edit remote.json.").small());
    });
}

/// One port list; returns a (name, enabled) toggle.
fn ports(
    ui: &mut Ui,
    id: &str,
    list: &[PortInfo],
    r: &RemoteView,
    now: f64,
) -> Option<(String, bool)> {
    let mut out = None;
    if list.is_empty() {
        ui.label(dim("none found"));
        return None;
    }
    egui::Grid::new(id)
        .num_columns(2)
        .spacing([10.0, 4.0])
        .show(ui, |ui| {
            for p in list {
                let mut on = p.enabled;
                if ui.checkbox(&mut on, &p.name).changed() {
                    out = Some((p.name.clone(), on));
                }
                let lit = r.device_active(&p.name, now);
                lamp(ui, theme::ACCENT, lit, "");
                ui.end_row();
            }
        });
    out
}

fn devices(ui: &mut Ui, st: &mut AppState, r: &RemoteView) {
    let now = st.now();
    let mut change = None;
    card(ui, "DEVICES", |ui| {
        ui.horizontal_top(|ui| {
            ui.vertical(|ui| {
                ui.label(RichText::new("MIDI in").strong().color(theme::TEXT_DIM));
                if let Some(c) = ports(ui, "midi-in", &r.midi_inputs, r, now) {
                    change = Some((true, c));
                }
            });
            ui.add_space(24.0);
            ui.vertical(|ui| {
                ui.label(RichText::new("MIDI out").strong().color(theme::TEXT_DIM));
                if let Some(c) = ports(ui, "midi-out", &r.midi_outputs, r, now) {
                    change = Some((false, c));
                }
            });
            ui.add_space(24.0);
            ui.vertical(|ui| {
                ui.label(RichText::new("OSC senders").strong().color(theme::TEXT_DIM));
                if r.osc_senders.is_empty() {
                    ui.label(dim("none seen yet"));
                }
                for (h, _) in &r.osc_senders {
                    lamp(ui, theme::ACCENT, r.device_active(h, now), h);
                }
            });
        });
    });
    if let Some((input, (name, on))) = change {
        st.set_midi_port(input, name, on);
    }
}

fn mappings(ui: &mut Ui, st: &mut AppState, r: &RemoteView) {
    let now = st.now();
    card(ui, "MAPPINGS", |ui| {
        if r.mappings.is_empty() {
            ui.label("No mappings yet.");
            ui.label(dim(
                "Turn LEARN on in the top bar, click a fader, knob or button, then move a \
                 MIDI control or send an OSC message; repeat for more controls. LEARN off \
                 saves, Cancel reverts. Every control also has a fixed OSC address that \
                 works without learning: run `openorion-remote --help-osc` for the list.",
            ));
            return;
        }
        let mut set: Option<(openorion_control::Source, MapOpts)> = None;
        let mut del = None;
        egui::Grid::new("mapping-table")
            .num_columns(8)
            .spacing([12.0, 4.0])
            .striped(true)
            .show(ui, |ui| {
                for h in [
                    "", "source", "control", "mode", "invert", "range", "takeover", "",
                ] {
                    ui.label(dim(h));
                }
                ui.end_row();
                for (i, m) in r.mappings.iter().enumerate() {
                    let mut o = m.opts;
                    lamp(ui, theme::ACCENT, r.source_active(&m.source, now), "");
                    ui.label(mono(m.source.to_string()))
                        .on_hover_text(format!("device: {}", device_key(&m.source)));
                    ui.label(param_label(&m.param))
                        .on_hover_text(format!("/orion/{}", m.param));
                    egui::ComboBox::from_id_salt(("map-mode", i))
                        .selected_text(mode_name(o.mode))
                        .width(90.0)
                        .show_ui(ui, |ui| {
                            for md in [Mode::Absolute, Mode::Relative, Mode::Toggle] {
                                ui.selectable_value(&mut o.mode, md, mode_name(md));
                            }
                        });
                    ui.checkbox(&mut o.invert, "");
                    ui.horizontal(|ui| {
                        ui.add(
                            egui::DragValue::new(&mut o.lo)
                                .range(0.0..=1.0)
                                .speed(0.005)
                                .fixed_decimals(2),
                        )
                        .on_hover_text("low end of the parameter range (0..1)");
                        ui.label(dim("–"));
                        ui.add(
                            egui::DragValue::new(&mut o.hi)
                                .range(0.0..=1.0)
                                .speed(0.005)
                                .fixed_decimals(2),
                        )
                        .on_hover_text("high end; below low reverses the direction");
                    });
                    egui::ComboBox::from_id_salt(("map-takeover", i))
                        .selected_text(takeover_name(o.takeover))
                        .width(80.0)
                        .show_ui(ui, |ui| {
                            for t in [Takeover::Jump, Takeover::Pickup] {
                                ui.selectable_value(&mut o.takeover, t, takeover_name(t));
                            }
                        });
                    if ui
                        .small_button("delete")
                        .on_hover_text("delete this mapping")
                        .clicked()
                    {
                        del = Some(m.source.clone());
                    }
                    ui.end_row();
                    if o != m.opts {
                        set = Some((m.source.clone(), o));
                    }
                }
            });
        if let Some((s, o)) = set {
            st.set_map_opts(s, o);
        }
        if let Some(s) = del {
            st.unbind_source(s);
        }
    });
}

pub fn show(ui: &mut Ui, st: &mut AppState) {
    // A copy: the cards edit `st` (through messages) while showing it.
    let Some(r) = st.remote.clone() else {
        card(ui, "OPENORION-REMOTE", |ui| {
            ui.label(RichText::new("openorion-remote not running").color(theme::TEXT_DIM));
            ui.label(dim(
                "MIDI and OSC control runs in its own process. Start it with \
                 `openorion-remote` (or `systemctl --user start openorion-remote`); \
                 this tab fills in once it announces itself. `openorion-remote \
                 --help-osc` lists the fixed OSC namespace.",
            ));
        });
        return;
    };
    egui::ScrollArea::vertical()
        .auto_shrink([false, false])
        .show(ui, |ui| {
            ui.horizontal_top(|ui| {
                status(ui, &r);
                devices(ui, st, &r);
            });
            ui.add_space(8.0);
            mappings(ui, st, &r);
        });
}
