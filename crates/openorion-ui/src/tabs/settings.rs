//! Settings: clock and rate, oscillator, brightness, S-MUX, SRC, power,
//! device info.

use egui::{RichText, Ui};
use openorion_ipc::Op;
use openorion_proto::{Brightness, OscFreq, OscLevel, SmuxId};

use super::card;
use crate::labels::{osc_freq_name, osc_level_name};
use crate::state::AppState;
use crate::theme;
use crate::topbar::{clock_combo, rate_combo};
use crate::widgets::{dim, lamp, mono, toggle};

/// "2018-04-24" for a Unix time (UTC).
pub fn unix_date(t: u32) -> String {
    // Civil-from-days (H. Hinnant).
    let z = (t / 86_400) as i64 + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + if m <= 2 { 1 } else { 0 };
    format!("{y:04}-{m:02}-{d:02}")
}

fn clock(ui: &mut Ui, st: &mut AppState) {
    card(ui, "CLOCK", |ui| {
        egui::Grid::new("clock-grid")
            .num_columns(2)
            .spacing([14.0, 8.0])
            .show(ui, |ui| {
                ui.label(dim("source"));
                clock_combo(ui, st);
                ui.end_row();
                ui.label(dim("sample rate"));
                rate_combo(ui, st);
                ui.end_row();
                ui.label(dim("external"));
                ui.label(mono(format!("{} Hz", st.device.status.external_clock_hz)));
                ui.end_row();
                ui.label(dim("flags"));
                let f = st.device.status.clock;
                ui.horizontal_wrapped(|ui| {
                    for (on, name) in [
                        (f.locked, "LOCK"),
                        (f.oven, "OVEN"),
                        (f.atomic, "ATOMIC"),
                        (f.usb, "USB"),
                        (f.madi, "MADI"),
                        (f.adat, "ADAT"),
                        (f.spdif, "S/PDIF"),
                        (f.dlink, "DLINK"),
                    ] {
                        lamp(ui, theme::ACCENT, on, name);
                    }
                });
                ui.end_row();
            });
    });
}

fn oscillator(ui: &mut Ui, st: &mut AppState) {
    let osc = st.device.status.oscillator;
    let mut n = osc;
    card(ui, "OSCILLATOR", |ui| {
        egui::Grid::new("osc-grid")
            .num_columns(3)
            .spacing([14.0, 8.0])
            .show(ui, |ui| {
                for (name, freq, mute) in [
                    ("L", &mut n.freq_l, &mut n.mute_l),
                    ("R", &mut n.freq_r, &mut n.mute_r),
                ] {
                    ui.label(dim(name));
                    ui.horizontal(|ui| {
                        for &f in OscFreq::ALL {
                            ui.selectable_value(freq, f, osc_freq_name(f));
                        }
                    });
                    if toggle(ui, *mute, "MUTE", true).clicked() {
                        *mute = !*mute;
                    }
                    ui.end_row();
                }
                ui.label(dim("level"));
                ui.horizontal(|ui| {
                    for &l in OscLevel::ALL {
                        ui.selectable_value(&mut n.level, l, osc_level_name(l));
                    }
                });
                ui.end_row();
            });
        ui.label(dim("Audible only where OSCILLATOR is routed.").small());
    });
    if n != osc {
        st.submit(Op::Oscillator { osc: n });
    }
}

fn panel(ui: &mut Ui, st: &mut AppState) {
    let s = st.device.status.clone();
    card(ui, "FRONT PANEL / DIGITAL I/O", |ui| {
        egui::Grid::new("panel-grid")
            .num_columns(2)
            .spacing([14.0, 8.0])
            .show(ui, |ui| {
                ui.label(dim("brightness"));
                let mut b = s.brightness;
                if ui.add(egui::Slider::new(&mut b, 0..=100)).changed()
                    && let Ok(value) = Brightness::new(b)
                {
                    st.submit(Op::Brightness { value });
                }
                ui.end_row();
                ui.label(dim("S-MUX"));
                ui.horizontal(|ui| {
                    if toggle(ui, s.smux_in, "MADI IN", false).clicked() {
                        st.submit(Op::Smux {
                            id: SmuxId::MadiIn,
                            on: !s.smux_in,
                        });
                    }
                    if toggle(ui, s.smux_out, "MADI OUT", false).clicked() {
                        st.submit(Op::Smux {
                            id: SmuxId::MadiOut,
                            on: !s.smux_out,
                        });
                    }
                });
                ui.end_row();
                ui.label(dim("S/PDIF SRC"));
                if toggle(
                    ui,
                    s.spdif_src,
                    if s.spdif_src { "ON" } else { "OFF" },
                    false,
                )
                .clicked()
                {
                    st.submit(Op::SpdifSrc { on: !s.spdif_src });
                }
                ui.end_row();
                ui.label(dim("power"));
                ui.horizontal(|ui| {
                    if toggle(ui, s.power_on, "ON", false).clicked() && !s.power_on {
                        st.submit(Op::Power { on: true });
                    }
                    if toggle(ui, !s.power_on, "STANDBY", true).clicked() && s.power_on {
                        st.submit(Op::Power { on: false });
                    }
                });
                ui.end_row();
            });
    });
}

fn device(ui: &mut Ui, st: &mut AppState) {
    let d = st.device.clone();
    card(ui, "DEVICE", |ui| {
        egui::Grid::new("info-grid")
            .num_columns(2)
            .spacing([14.0, 4.0])
            .show(ui, |ui| {
                let mut row = |k: &str, v: String| {
                    ui.label(dim(k));
                    ui.label(mono(v));
                    ui.end_row();
                };
                row("name", d.info.passport.name.clone());
                row("serial", d.info.passport.serial.clone());
                row("hardware", d.info.passport.hw.clone());
                row("built", unix_date(d.info.passport.birth));
                row(
                    "firmware",
                    format!("{}.{}", d.info.firmware.major, d.info.firmware.minor),
                );
                row("registered", d.info.registered.to_string());
                row("usb mode", d.status.usb_mode.to_string());
                row("pull mode", format!("0x{:x}", d.status.pull_mode));
                row("device updated", d.status.device_updated.to_string());
            });
        ui.add_space(6.0);
        ui.horizontal(|ui| {
            if ui.button("Resync from device").clicked() {
                st.submit(Op::Resync);
            }
            if ui
                .button("Recheck control channel")
                .on_hover_text("Two reads in a row, then a resync; recovers a transient hang")
                .clicked()
            {
                st.submit(Op::Recheck);
            }
        });
        ui.label(
            RichText::new("Firmware, licensing and factory reset are deliberately absent.")
                .small()
                .color(theme::TEXT_DIM),
        );
    });
}

pub fn show(ui: &mut Ui, st: &mut AppState) {
    egui::ScrollArea::vertical()
        .auto_shrink([false, false])
        .show(ui, |ui| {
            ui.horizontal_top(|ui| {
                ui.vertical(|ui| {
                    clock(ui, st);
                    ui.add_space(8.0);
                    oscillator(ui, st);
                });
                ui.vertical(|ui| {
                    panel(ui, st);
                    ui.add_space(8.0);
                    device(ui, st);
                });
            });
        });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dates() {
        assert_eq!(unix_date(0), "1970-01-01");
        assert_eq!(unix_date(1_524_573_936), "2018-04-24");
        assert_eq!(unix_date(951_782_400), "2000-02-29");
    }
}
