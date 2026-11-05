//! Trims: line in / line out per channel (ALL or INDIVIDUAL), monitor trim.

use egui::{RichText, Ui};
use openorion_ipc::Op;
use openorion_proto::{TrimControl, TrimId, TrimLevel, TrimRecord};

use openorion_control::Param;

use super::card;
use crate::labels::{trim_control_name, trim_id_name};
use crate::learn::Learnable;
use crate::state::AppState;
use crate::theme;
use crate::widgets::{dim, mono};

/// Slider range in dBu (spec sec. 6: the panel shows 10..24).
pub const DBU_MIN: f32 = 10.0;
pub const DBU_MAX: f32 = 24.0;

/// Whole-dB trim for a dBu value (fract left at 0).
pub fn dbu_to_trim(dbu: f32) -> TrimLevel {
    TrimLevel {
        whole: (24.0 - dbu.round()).clamp(0.0, 24.0 - DBU_MIN) as u8,
        fract: 0,
    }
}

fn record(ui: &mut Ui, st: &mut AppState, id: TrimId) {
    let rec: TrimRecord = match id {
        TrimId::LineOut => st.device.trims.line_out.clone(),
        _ => st.device.trims.line_in.clone(),
    };
    card(ui, trim_id_name(id), |ui| {
        ui.horizontal(|ui| {
            ui.label(dim("control"));
            for &c in TrimControl::ALL {
                if ui
                    .add(egui::Button::selectable(
                        rec.control == c,
                        trim_control_name(c),
                    ))
                    .clicked()
                    && rec.control != c
                {
                    st.submit(Op::TrimControl { id, control: c });
                }
            }
        });
        ui.add_space(4.0);
        match rec.control {
            TrimControl::All => {
                let mut dbu = rec.levels[0].dbu();
                ui.horizontal(|ui| {
                    ui.label(dim("all channels"));
                    let r = ui.add(
                        egui::Slider::new(&mut dbu, DBU_MIN..=DBU_MAX)
                            .step_by(1.0)
                            .fixed_decimals(0)
                            .suffix(" dBu"),
                    );
                    let r = r.learnable(ui, st, Param::Trim { id, ch: None });
                    if r.changed() {
                        st.submit(Op::Trim {
                            id,
                            ch: None,
                            level: dbu_to_trim(dbu),
                        });
                    }
                });
            }
            TrimControl::Individual => {
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = 2.0;
                    for (i, l) in rec.levels.iter().enumerate() {
                        ui.vertical(|ui| {
                            ui.set_width(30.0);
                            ui.spacing_mut().slider_width = 120.0;
                            let mut dbu = l.dbu();
                            let r = ui
                                .add_sized(
                                    [24.0, 120.0],
                                    egui::Slider::new(&mut dbu, DBU_MIN..=DBU_MAX)
                                        .vertical()
                                        .step_by(1.0)
                                        .show_value(false),
                                )
                                .on_hover_text(format!("ch {}: {:.0} dBu", i + 1, l.dbu()))
                                .learnable(
                                    ui,
                                    st,
                                    Param::Trim {
                                        id,
                                        ch: Some(i as u8),
                                    },
                                );
                            if r.changed() {
                                st.submit(Op::Trim {
                                    id,
                                    ch: Some(i as u8),
                                    level: dbu_to_trim(dbu),
                                });
                            }
                            ui.label(mono(format!("{:.0}", l.dbu())).size(11.0));
                            ui.label(
                                RichText::new(format!("{}", i + 1))
                                    .size(9.0)
                                    .color(theme::TEXT_DIM),
                            );
                        });
                    }
                });
                ui.label(dim("dBu per channel").small());
            }
        }
    });
}

pub fn show(ui: &mut Ui, st: &mut AppState) {
    egui::ScrollArea::vertical()
        .auto_shrink([false, false])
        .show(ui, |ui| {
            record(ui, st, TrimId::LineIn);
            ui.add_space(8.0);
            record(ui, st, TrimId::LineOut);
            ui.add_space(8.0);
            card(ui, "MONITOR", |ui| {
                let mut dbu = 24.0 - st.device.status.monitor_trim as f32;
                ui.horizontal(|ui| {
                    let r = ui.add(
                        egui::Slider::new(&mut dbu, DBU_MIN..=DBU_MAX)
                            .step_by(1.0)
                            .fixed_decimals(0)
                            .suffix(" dBu"),
                    );
                    if r.changed() {
                        st.submit(Op::Trim {
                            id: TrimId::Monitor,
                            ch: None,
                            level: dbu_to_trim(dbu),
                        });
                    }
                    ui.label(dim("monitor trim set is unconfirmed on the wire (O3)"));
                });
            });
        });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trim_mapping() {
        assert_eq!(dbu_to_trim(16.0).whole, 8);
        assert_eq!(dbu_to_trim(24.0).whole, 0);
        assert_eq!(dbu_to_trim(10.0).whole, 14);
        assert_eq!(dbu_to_trim(3.0).whole, 14);
        assert!((dbu_to_trim(15.0).dbu() - 15.0).abs() < 1e-6);
    }
}
