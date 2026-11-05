//! Always-visible top bar: clock + LOCK, rate, volumes, presets, health,
//! JACK.

use egui::{Color32, RichText, Ui};
use openorion_ipc::{Health, Op};
use openorion_proto::{Atten, ClockSource, PresetSlot, SampleRate, Status, VolumeId};

use openorion_control::Param;

use crate::labels::{clock_source_name, sample_rate_name, volume_label};
use crate::learn::Learnable;
use crate::state::AppState;
use crate::theme;
use crate::widgets::{dim, knob, lamp, mono, toggle};

/// What the LOCK lamp shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LockLamp {
    /// External source, locked.
    Locked,
    /// Internal (Oven/Atomic) clock: nothing to lock to.
    Internal,
    /// External source without lock: alarm.
    Unlocked,
}

pub fn lock_lamp(st: &Status) -> LockLamp {
    match (st.clock_locked(), st.clock_source) {
        (true, _) => LockLamp::Locked,
        (false, Some(ClockSource::Internal)) => LockLamp::Internal,
        _ => LockLamp::Unlocked,
    }
}

/// Clock source picker. Selecting submits a (disruptive) op.
pub fn clock_combo(ui: &mut Ui, st: &mut AppState) {
    let cur = st.device.status.clock_source;
    let text = cur.map_or("?", clock_source_name);
    egui::ComboBox::from_id_salt("clock-source")
        .selected_text(RichText::new(text).monospace())
        .width(120.0)
        .show_ui(ui, |ui| {
            for &c in ClockSource::ALL {
                let label = RichText::new(clock_source_name(c)).monospace();
                let r = ui.selectable_label(cur == Some(c), label);
                if r.clicked() && cur != Some(c) {
                    st.submit(Op::ClockSource { source: c });
                }
            }
        });
}

pub fn rate_combo(ui: &mut Ui, st: &mut AppState) {
    let cur = st.device.status.sample_rate;
    egui::ComboBox::from_id_salt("sample-rate")
        .selected_text(RichText::new(cur.map_or("?", sample_rate_name)).monospace())
        .width(80.0)
        .show_ui(ui, |ui| {
            for &r in SampleRate::ALL {
                if ui
                    .selectable_label(
                        cur == Some(r),
                        RichText::new(sample_rate_name(r)).monospace(),
                    )
                    .clicked()
                    && cur != Some(r)
                {
                    st.submit(Op::SampleRate { rate: r });
                }
            }
        });
}

/// Knob + value + mute for one volume (0..-96 dB, 96 = -inf).
fn volume(ui: &mut Ui, st: &mut AppState, id: VolumeId, name: &str) {
    let v = match id {
        VolumeId::Main => st.device.status.main,
        VolumeId::Line => st.device.status.line,
    };
    ui.vertical(|ui| {
        ui.spacing_mut().item_spacing.y = 1.0;
        ui.label(RichText::new(name).small().color(theme::TEXT_DIM));
        ui.horizontal(|ui| {
            let mut db = -(v.atten.get() as f32);
            let r = knob(ui, &mut db, -96.0, 0.0, 0.0, 40.0)
                .on_hover_text("drag or scroll; double-click = 0 dB; shift = fine")
                .learnable(ui, st, Param::Volume(id));
            if r.changed()
                && let Ok(a) = Atten::new((-db).round() as u8)
                && a != v.atten
            {
                st.submit(Op::Volume { id, atten: a });
            }
            ui.vertical(|ui| {
                ui.label(mono(format!("{:>4} dB", volume_label(v.atten))));
                if toggle(ui, v.mute, "MUTE", true)
                    .learnable(ui, st, Param::Mute(id))
                    .clicked()
                {
                    st.submit(Op::Mute { id, mute: !v.mute });
                }
            });
        });
    });
}

fn health_dot(ui: &mut Ui, st: &mut AppState) {
    let h = st.health;
    let (col, text) = match h {
        Health::Ok => (theme::ACCENT, "OK"),
        Health::Syncing => (theme::TEXT_DIM, "SYNCING"),
        Health::Stalled => (theme::RED, "STALLED"),
        Health::Hung => (theme::RED, "HUNG: power-cycle the Orion"),
        Health::NoCyclic => (theme::RED, "NO CYCLIC"),
        Health::Disconnected => (theme::RED, "DISCONNECTED"),
    };
    let advice = h.advice().text();
    let r = lamp(ui, col, true, text);
    if !advice.is_empty() {
        r.on_hover_text(advice);
    }
    // Reads only: safe to offer whenever settings are refused.
    if matches!(h, Health::Stalled | Health::Hung)
        && ui
            .small_button("RECHECK")
            .on_hover_text("Ask the device again (reads only; audio is not touched)")
            .clicked()
    {
        st.submit(Op::Recheck);
    }
}

/// LEARN toggle, Cancel and the device filter (right-to-left layout; in
/// the tab bar row, which has the room).
pub fn learn_controls(ui: &mut Ui, st: &mut AppState) {
    let learning = st.learning();
    let can = st.can_learn();
    let r = ui
        .add_enabled_ui(can || learning, |ui| toggle(ui, learning, "LEARN", false))
        .inner;
    let r = if can {
        r.on_hover_text(
            "Learn on: click a control, then move a MIDI/OSC control to bind it. \
             Learn off saves; Cancel reverts. Nothing is written to the device \
             while learning.",
        )
    } else {
        r.on_disabled_hover_text("openorion-remote not running")
    };
    if r.clicked() {
        if learning {
            st.learn_stop(true);
        } else {
            st.learn_start();
        }
    }
    if !learning {
        return;
    }
    if ui
        .button("Cancel")
        .on_hover_text("leave learn mode and drop this session's binds")
        .clicked()
    {
        st.learn_stop(false);
        return;
    }
    let cur = st.learn.as_ref().and_then(|l| l.filter.clone());
    let mut choices: Vec<String> = Vec::new();
    if let Some(r) = &st.remote {
        choices.extend(
            r.midi_inputs
                .iter()
                .filter(|p| p.enabled)
                .map(|p| p.name.clone()),
        );
        choices.extend(r.osc_senders.iter().map(|(h, _)| h.clone()));
    }
    let mut pick = None;
    egui::ComboBox::from_id_salt("learn-filter")
        .selected_text(cur.clone().unwrap_or_else(|| "any device".into()))
        .width(130.0)
        .show_ui(ui, |ui| {
            if ui.selectable_label(cur.is_none(), "any device").clicked() {
                pick = Some(None);
            }
            for c in choices {
                if ui.selectable_label(cur.as_ref() == Some(&c), &c).clicked() {
                    pick = Some(Some(c));
                }
            }
        })
        .response
        .on_hover_text("learn only from this device");
    if let Some(p) = pick {
        st.learn_filter(p);
    }
}

/// Learn mode hint under the bar.
fn learn_hint(ui: &mut Ui, st: &mut AppState) {
    let Some(l) = &st.learn else { return };
    let text = match &l.selected {
        None => "LEARN: click a control, then move a MIDI/OSC control. Esc clears the selection."
            .to_owned(),
        Some(p) => format!(
            "LEARN: {} selected; move a MIDI/OSC control to bind it (Esc clears).",
            crate::learn::param_label(p)
        ),
    };
    ui.label(RichText::new(text).small().color(theme::ACCENT));
}

pub fn show(ui: &mut Ui, st: &mut AppState) {
    show_bar(ui, st);
    learn_hint(ui, st);
}

fn show_bar(ui: &mut Ui, st: &mut AppState) {
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 14.0;
        ui.label(
            RichText::new("ORION32 HD")
                .strong()
                .size(16.0)
                .color(theme::ACCENT),
        );
        if !st.device.status.power_on {
            ui.label(RichText::new("STANDBY").strong().color(theme::RED));
        }
        ui.separator();

        ui.vertical(|ui| {
            ui.label(RichText::new("CLOCK").small().color(theme::TEXT_DIM));
            ui.horizontal(|ui| {
                clock_combo(ui, st);
                match lock_lamp(&st.device.status) {
                    LockLamp::Locked => lamp(ui, theme::ACCENT, true, "LOCK"),
                    LockLamp::Internal => lamp(ui, theme::ACCENT_DIM, true, "INT"),
                    LockLamp::Unlocked => lamp(ui, theme::RED, true, "UNLOCKED"),
                };
            });
        });
        ui.vertical(|ui| {
            ui.label(RichText::new("RATE").small().color(theme::TEXT_DIM));
            rate_combo(ui, st);
        });
        ui.separator();
        volume(ui, st, VolumeId::Main, "MAIN");
        volume(ui, st, VolumeId::Line, "LINE");
        ui.separator();

        ui.vertical(|ui| {
            let cur = st.device.status.current_preset;
            ui.label(
                RichText::new(if cur == 0 {
                    "PRESET  (modified)"
                } else {
                    "PRESET"
                })
                .small()
                .color(theme::TEXT_DIM),
            );
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 4.0;
                for n in 1..=5u8 {
                    let r = toggle(ui, cur == n, &n.to_string(), false)
                        .on_hover_text("click: recall; right-click: save");
                    let slot = PresetSlot::new(n).expect("1..=5");
                    if r.clicked() {
                        st.submit(Op::PresetRecall { slot });
                    }
                    r.context_menu(|ui| {
                        if ui
                            .button(format!("Save current state to preset {n}"))
                            .clicked()
                        {
                            st.submit(Op::PresetSave { slot });
                            ui.close();
                        }
                    });
                }
            });
        });

        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            match &st.jack {
                Some(s) => lamp(ui, theme::ACCENT, true, &format!("JACK {s}")),
                None => lamp(ui, Color32::TRANSPARENT, false, "JACK —"),
            };
            health_dot(ui, st);
            ui.label(dim(format!(
                "fw {}.{}",
                st.device.info.firmware.major, st.device.info.firmware.minor
            )));
        });
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::demo::demo_state;

    #[test]
    fn lamp_states() {
        let mut s = demo_state().status;
        assert_eq!(lock_lamp(&s), LockLamp::Internal);
        s.clock_source = Some(ClockSource::WordClock);
        assert_eq!(lock_lamp(&s), LockLamp::Unlocked);
        s.clock.locked = true;
        assert_eq!(lock_lamp(&s), LockLamp::Locked);
        // USB clock: the lock bit stays 0, the usb flag counts.
        s.clock.locked = false;
        s.clock_source = Some(ClockSource::Usb);
        assert_eq!(lock_lamp(&s), LockLamp::Unlocked);
        s.clock.usb = true;
        assert_eq!(lock_lamp(&s), LockLamp::Locked);
    }
}
