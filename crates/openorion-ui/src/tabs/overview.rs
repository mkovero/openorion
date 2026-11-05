//! Overview: meter banks A and B (full width, with max-peak readouts) and
//! their source pickers; monitor, reverb meters and device info below.

use egui::{RichText, Ui, Vec2};
use openorion_ipc::Op;
use openorion_proto::{METER_FLOOR, MeterBank, OutputBank, SourceGroup, VolumeId};

use super::card;
use crate::labels::{
    METER_SOURCE_MAX, meter_channel_name, meter_source_channels, meter_source_name, source_name,
    volume_label,
};
use crate::learn::Learnable;
use crate::meters::PeakHold;
use crate::state::AppState;
use crate::theme;
use crate::widgets::{chip, dim, meter_bank, mono, toggle};

/// Height of the row under the meter banks (monitor, reverb, device).
const LOWER_H: f32 = 250.0;
const BANK_MIN_H: f32 = 200.0;
const BANK_MAX_H: f32 = 460.0;

/// Max-since-reset values for one meter display, kept in egui memory and
/// reset when the source changes.
fn peaks_for(
    ui: &Ui,
    key: impl std::hash::Hash + std::fmt::Debug,
    src: u8,
    n: usize,
) -> (egui::Id, Vec<u8>) {
    let id = egui::Id::new(("meter-peaks", key));
    let stored: Option<(u8, Vec<u8>)> = ui.data(|d| d.get_temp(id));
    let v = match stored {
        Some((s, v)) if s == src && v.len() == n => v,
        _ => vec![METER_FLOOR; n],
    };
    (id, v)
}

/// Meters to draw for a source: ADAT is limited by the rate
/// (`adat_channels`, 8 at 96k).
fn visible_channels(st: &AppState, src: u8) -> usize {
    let n = meter_source_channels(src).min(32);
    if src == SourceGroup::AdatIn as u8 || src == 14 + OutputBank::AdatOut as u8 {
        n.min(st.device.status.adat_channels.max(1) as usize)
    } else {
        n
    }
}

fn bank(ui: &mut Ui, st: &mut AppState, bank: MeterBank, height: f32) {
    let idx = bank as usize;
    let src = st.device.status.meter_sources[idx];
    let title = match bank {
        MeterBank::A => "METER BANK A",
        _ => "METER BANK B",
    };
    card(ui, title, |ui| {
        ui.horizontal(|ui| {
            ui.label(dim("source"));
            egui::ComboBox::from_id_salt(("meter-src", idx))
                .selected_text(RichText::new(meter_source_name(src).unwrap_or("?")).monospace())
                .width(160.0)
                .show_ui(ui, |ui| {
                    for s in 0..=METER_SOURCE_MAX {
                        let name = meter_source_name(s).unwrap_or("?");
                        if ui
                            .selectable_label(s == src, RichText::new(name).monospace())
                            .clicked()
                            && s != src
                        {
                            st.submit(Op::MeterSource { bank, src: s });
                        }
                    }
                });
            ui.label(dim("numbers on top = max since reset (click to reset)").small());
        });
        let n = visible_channels(st, src);
        let (vals, holds): (&[u8], &[PeakHold]) = match bank {
            MeterBank::A => (&st.meters.bank_a[..n], &st.holds.bank_a[..n]),
            _ => (&st.meters.bank_b[..n], &st.holds.bank_b[..n]),
        };
        let (id, mut peaks) = peaks_for(ui, idx, src, n);
        let w = ui.available_width();
        meter_bank(
            ui,
            vals,
            holds,
            &mut peaks,
            &|i| meter_channel_name(src, i),
            Vec2::new(w, height),
        );
        ui.data_mut(|d| d.insert_temp(id, (src, peaks)));
    });
}

fn monitor(ui: &mut Ui, st: &mut AppState) {
    card(ui, "MONITOR", |ui| {
        let srcs = st.device.routing[OutputBank::Monitor as usize]
            .sources
            .clone();
        egui::Grid::new("monitor-grid")
            .num_columns(2)
            .spacing([12.0, 8.0])
            .show(ui, |ui| {
                for (i, s) in srcs.iter().enumerate() {
                    ui.label(dim(format!("MONITOR {}", i + 1)));
                    chip(ui, &source_name(*s), theme::source_fill(*s), 150.0, false)
                        .on_hover_text("change in the Routing tab");
                    ui.end_row();
                }
                let main = st.device.status.main;
                ui.label(dim("main volume"));
                ui.horizontal(|ui| {
                    ui.label(mono(format!("{} dB", volume_label(main.atten))));
                    if toggle(ui, main.mute, "MUTE", true)
                        .learnable(ui, st, openorion_control::Param::Mute(VolumeId::Main))
                        .clicked()
                    {
                        st.submit(Op::Mute {
                            id: VolumeId::Main,
                            mute: !main.mute,
                        });
                    }
                });
                ui.end_row();
                ui.label(dim("monitor trim"));
                ui.label(mono(format!(
                    "{} dBu",
                    24 - st.device.status.monitor_trim as i32
                )));
                ui.end_row();
            });
    });
}

fn reverb_meters(ui: &mut Ui, st: &AppState) {
    card(ui, "REVERB", |ui| {
        let vals = [
            st.meters.reverb_in[0],
            st.meters.reverb_in[1],
            st.meters.reverb_out[0],
            st.meters.reverb_out[1],
        ];
        let holds = [
            st.holds.reverb_in[0],
            st.holds.reverb_in[1],
            st.holds.reverb_out[0],
            st.holds.reverb_out[1],
        ];
        let (id, mut peaks) = peaks_for(ui, "reverb", 0, 4);
        meter_bank(
            ui,
            &vals,
            &holds,
            &mut peaks,
            &|i| ["in L", "in R", "out L", "out R"][i].to_owned(),
            Vec2::new(200.0, LOWER_H - 70.0),
        );
        ui.data_mut(|d| d.insert_temp(id, (0u8, peaks)));
        ui.label(dim("1-2 in · 3-4 out (scale unconfirmed)").small());
    });
}

fn device(ui: &mut Ui, st: &AppState) {
    let d = &st.device;
    card(ui, "DEVICE", |ui| {
        egui::Grid::new("dev-grid")
            .num_columns(2)
            .spacing([12.0, 4.0])
            .show(ui, |ui| {
                let mut row = |k: &str, v: String| {
                    ui.label(dim(k));
                    ui.label(mono(v));
                    ui.end_row();
                };
                row("name", d.info.passport.name.clone());
                row("serial", d.info.passport.serial.clone());
                row(
                    "firmware",
                    format!("{}.{}", d.info.firmware.major, d.info.firmware.minor),
                );
                row("ext clock", format!("{} Hz", d.status.external_clock_hz));
                row(
                    "ADAT / MADI ch",
                    format!("{} / {}", d.status.adat_channels, d.status.madi_channels),
                );
                row(
                    "power",
                    if d.status.power_on {
                        "on".into()
                    } else {
                        "standby".into()
                    },
                );
            });
    });
}

pub fn show(ui: &mut Ui, st: &mut AppState) {
    // Card chrome (title, source row, margins) around each bank.
    let chrome = 120.0;
    let bank_h =
        ((ui.available_height() - LOWER_H - 2.0 * chrome) / 2.0).clamp(BANK_MIN_H, BANK_MAX_H);
    egui::ScrollArea::vertical()
        .auto_shrink([false, false])
        .show(ui, |ui| {
            bank(ui, st, MeterBank::A, bank_h);
            ui.add_space(8.0);
            bank(ui, st, MeterBank::B, bank_h);
            ui.add_space(8.0);
            ui.horizontal_top(|ui| {
                monitor(ui, st);
                reverb_meters(ui, st);
                device(ui, st);
            });
            if st.fake_meters {
                ui.add_space(4.0);
                ui.label(
                    RichText::new("Offline: meters are synthetic.")
                        .small()
                        .color(theme::TEXT_DIM),
                );
            }
        });
}
