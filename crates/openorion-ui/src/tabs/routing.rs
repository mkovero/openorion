//! Routing: outputs grouped by bank, laid out in columns so a whole bank
//! fits on screen; each output's source is a chip coloured by source kind.
//! Clicking a chip opens a searchable source picker. Clicking output
//! numbers selects them (shift = range); the picker then routes
//! consecutive sources to the selection.

use std::collections::BTreeSet;

use egui::{RichText, Ui};
use openorion_ipc::Op;
use openorion_proto::{OutputBank, Source, SourceGroup};

use crate::labels::{output_bank_name, source_group_name, source_name};
use crate::state::AppState;
use crate::theme;
use crate::widgets::{chip, dim};

pub struct View {
    bank: OutputBank,
    selected: BTreeSet<u8>,
    /// Last output clicked without shift: the start of a shift range.
    anchor: Option<u8>,
    picker: Option<Picker>,
}

struct Picker {
    /// Output the picker was opened for.
    ch: u8,
    /// Route the selection consecutively instead of one row.
    multi: bool,
    filter: String,
}

impl Default for View {
    fn default() -> Self {
        View {
            bank: OutputBank::LineOut,
            selected: BTreeSet::new(),
            anchor: None,
            picker: None,
        }
    }
}

/// Ops that route consecutive sources starting at `start` to the given
/// outputs (sorted, 0-based). Runs of adjacent outputs become one op. Stops
/// when the source group runs out of channels. MUTE mutes every output.
pub fn plan_routes(bank: OutputBank, outputs: &BTreeSet<u8>, start: Source) -> Vec<Op> {
    let mut ops: Vec<Op> = Vec::new();
    let mut next = start;
    let mut last_ch: Option<u8> = None;
    for &ch in outputs.iter().filter(|&&c| (c as usize) < bank.size()) {
        let src = next;
        match &mut next {
            Source::Mute => {}
            Source::Input { group, chan } => {
                if *chan >= group.size() {
                    break;
                }
                *chan += 1;
            }
        }
        match (ops.last_mut(), last_ch) {
            (Some(Op::Route { sources, .. }), Some(prev)) if prev + 1 == ch => sources.push(src),
            _ => ops.push(Op::Route {
                bank,
                ch,
                sources: vec![src],
            }),
        }
        last_ch = Some(ch);
    }
    ops
}

/// All selectable sources with their display names.
fn all_sources() -> Vec<(Source, String)> {
    let mut v = vec![(Source::Mute, "MUTE".to_owned())];
    for &g in SourceGroup::ALL {
        for chan in 0..g.size() {
            let s = Source::Input { group: g, chan };
            v.push((s, source_name(s)));
        }
    }
    v
}

fn bank_list(ui: &mut Ui, st: &AppState, view: &mut View) {
    ui.vertical(|ui| {
        ui.set_width(190.0);
        for &b in OutputBank::ALL {
            let routed = st.device.routing[b as usize]
                .sources
                .iter()
                .filter(|s| **s != Source::Mute)
                .count();
            let text = RichText::new(format!("{:<15}{routed:>3}", output_bank_name(b))).monospace();
            let text = if b.is_physical() { text.strong() } else { text };
            if ui
                .add_sized(
                    [180.0, 28.0],
                    egui::Button::selectable(view.bank == b, text),
                )
                .clicked()
                && view.bank != b
            {
                view.bank = b;
                view.selected.clear();
                view.anchor = None;
                view.picker = None;
            }
        }
        ui.add_space(6.0);
        ui.label(dim("bold = physical outputs\n(changes need confirmation)").small());
    });
}

/// Colour key for the source chips.
fn legend(ui: &mut Ui) {
    use SourceGroup::*;
    ui.spacing_mut().item_spacing.x = 4.0;
    for (g, name) in [
        (LineIn, "line"),
        (UsbPlay1, "usb"),
        (MadiIn1, "madi"),
        (AdatIn, "adat"),
        (SpdifIn, "s/pdif"),
        (Hdx1Play, "hdx"),
        (AfxOut, "afx"),
        (Mix1, "mix"),
        (Oscillator, "osc"),
    ] {
        let fill = theme::source_fill(Source::Input { group: g, chan: 0 }).expect("input");
        let (r, _) = ui.allocate_exact_size(egui::vec2(10.0, 10.0), egui::Sense::hover());
        ui.painter().circle_filled(r.center(), 4.5, fill);
        ui.label(dim(name).small());
        ui.add_space(6.0);
    }
}

/// Width of one output cell (number + chip) and the gap between columns.
const NUM_W: f32 = 34.0;
const CHIP_W: f32 = 170.0;
const COL_GAP: f32 = 28.0;

/// Rows per column for `n` outputs in `cols` columns: whole blocks of 8
/// (the device's channel grouping), at least 8.
pub fn rows_per_column(n: usize, cols: usize) -> usize {
    n.div_ceil(cols.max(1)).div_ceil(8).max(1) * 8
}

/// Click on output `ch`: toggle it, or with shift select the range from
/// the anchor.
fn click_output(view: &mut View, ch: u8, shift: bool) {
    match (shift, view.anchor) {
        (true, Some(a)) => {
            let (lo, hi) = (a.min(ch), a.max(ch));
            view.selected.extend(lo..=hi);
        }
        _ => {
            if !view.selected.remove(&ch) {
                view.selected.insert(ch);
            }
            view.anchor = Some(ch);
        }
    }
}

fn rows(ui: &mut Ui, st: &mut AppState, view: &mut View) {
    let bank = view.bank;
    let sources = st.device.routing[bank as usize].sources.clone();
    ui.vertical(|ui| {
        ui.horizontal(|ui| {
            ui.heading(output_bank_name(bank));
            ui.label(dim(format!("{} outputs", bank.size())));
            ui.separator();
            ui.label(dim(format!("{} selected", view.selected.len())));
            if ui.button("Select all").clicked() {
                view.selected = (0..bank.size() as u8).collect();
            }
            if ui
                .add_enabled(!view.selected.is_empty(), egui::Button::new("Select none"))
                .clicked()
            {
                view.selected.clear();
            }
            let any = !view.selected.is_empty();
            if ui
                .add_enabled(any, egui::Button::new("Route consecutively…"))
                .clicked()
            {
                view.picker = Some(Picker {
                    ch: *view.selected.first().unwrap_or(&0),
                    multi: true,
                    filter: String::new(),
                });
            }
            if ui
                .add_enabled(any, egui::Button::new("Mute selected"))
                .clicked()
            {
                st.submit_all(plan_routes(bank, &view.selected, Source::Mute));
            }
        });
        ui.horizontal(|ui| {
            ui.label(
                dim("click output numbers to select, shift-click for a range; click a source to change it")
                    .small(),
            );
            ui.separator();
            legend(ui);
        });
        ui.separator();

        let cell_w = NUM_W + 6.0 + CHIP_W + COL_GAP;
        let cols = ((ui.available_width() / cell_w).floor() as usize).clamp(1, 4);
        let per_col = rows_per_column(sources.len(), cols);
        let shift = ui.input(|i| i.modifiers.shift);
        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .show(ui, |ui| {
                ui.horizontal_top(|ui| {
                    ui.spacing_mut().item_spacing.x = COL_GAP;
                    for (c, col) in sources.chunks(per_col).enumerate() {
                        ui.vertical(|ui| {
                            ui.spacing_mut().item_spacing = egui::vec2(6.0, 5.0);
                            for (j, s) in col.iter().enumerate() {
                                let ch = (c * per_col + j) as u8;
                                if j > 0 && j % 8 == 0 {
                                    ui.add_space(6.0);
                                }
                                ui.horizontal(|ui| {
                                    let sel = view.selected.contains(&ch);
                                    let num = RichText::new(format!("{:>2}", ch + 1))
                                        .monospace()
                                        .color(if sel { theme::TEXT } else { theme::TEXT_DIM });
                                    let r = ui
                                        .add(
                                            egui::Button::new(num)
                                                .fill(if sel {
                                                    theme::ACCENT_DIM
                                                } else {
                                                    egui::Color32::TRANSPARENT
                                                })
                                                .min_size(egui::vec2(NUM_W, 24.0)),
                                        )
                                        .on_hover_text(format!(
                                            "{} {}: click to select, shift-click for a range",
                                            output_bank_name(bank),
                                            ch + 1
                                        ));
                                    if r.clicked() {
                                        click_output(view, ch, shift);
                                    }
                                    let name = source_name(*s);
                                    if chip(ui, &name, theme::source_fill(*s), CHIP_W, sel)
                                        .on_hover_text(format!(
                                            "{} {} ← {name}",
                                            output_bank_name(bank),
                                            ch + 1
                                        ))
                                        .clicked()
                                    {
                                        view.picker = Some(Picker {
                                            ch,
                                            multi: false,
                                            filter: String::new(),
                                        });
                                    }
                                });
                            }
                        });
                    }
                });
            });
    });
}

fn picker(ctx: &egui::Context, st: &mut AppState, view: &mut View) {
    let Some(p) = &mut view.picker else { return };
    let bank = view.bank;
    let current = (!p.multi)
        .then(|| {
            st.device.routing[bank as usize]
                .sources
                .get(p.ch as usize)
                .copied()
        })
        .flatten();
    let title = if p.multi {
        format!(
            "Route {} outputs of {} from…",
            view.selected.len(),
            output_bank_name(bank)
        )
    } else {
        format!("Source for {} {}", output_bank_name(bank), p.ch + 1)
    };
    let mut open = true;
    let mut chosen = None;
    egui::Window::new(title)
        .id(egui::Id::new("source-picker"))
        .open(&mut open)
        .collapsible(false)
        .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
        .default_size([900.0, 760.0])
        .show(ctx, |ui| {
            ui.horizontal(|ui| {
                ui.label("search");
                ui.add(
                    egui::TextEdit::singleline(&mut p.filter)
                        .hint_text("e.g. adat 3, mix, line in 1"),
                )
                .request_focus();
            });
            ui.separator();
            let f = p.filter.to_lowercase();
            let words: Vec<&str> = f.split_whitespace().collect();
            egui::ScrollArea::vertical().show(ui, |ui| {
                let all = all_sources();
                let matching: Vec<&(Source, String)> = all
                    .iter()
                    .filter(|(_, n)| words.iter().all(|w| n.to_lowercase().contains(w)))
                    .collect();
                if matching.iter().any(|(s, _)| *s == Source::Mute)
                    && chip(ui, "MUTE", None, 80.0, current == Some(Source::Mute)).clicked()
                {
                    chosen = Some(Source::Mute);
                }
                // One block per source group: name on the left, channels in
                // rows of 16.
                egui::Grid::new("picker-groups")
                    .num_columns(2)
                    .spacing([12.0, 10.0])
                    .show(ui, |ui| {
                        for &g in SourceGroup::ALL {
                            let chans: Vec<&(Source, String)> = matching
                                .iter()
                                .copied()
                                .filter(|(s, _)| {
                                    matches!(s, Source::Input { group, .. } if *group == g)
                                })
                                .collect();
                            if chans.is_empty() {
                                continue;
                            }
                            ui.label(
                                RichText::new(source_group_name(g))
                                    .small()
                                    .color(theme::TEXT_DIM),
                            );
                            ui.vertical(|ui| {
                                ui.spacing_mut().item_spacing = egui::vec2(6.0, 6.0);
                                for row in chans.chunks(16) {
                                    ui.horizontal(|ui| {
                                        for (s, name) in row {
                                            let Source::Input { chan, .. } = s else {
                                                continue;
                                            };
                                            if chip(
                                                ui,
                                                &format!("{}", chan + 1),
                                                theme::source_fill(*s),
                                                40.0,
                                                current == Some(*s),
                                            )
                                            .on_hover_text(name)
                                            .clicked()
                                            {
                                                chosen = Some(*s);
                                            }
                                        }
                                    });
                                }
                            });
                            ui.end_row();
                        }
                    });
            });
        });
    if let Some(s) = chosen {
        let ops = if p.multi {
            plan_routes(bank, &view.selected, s)
        } else {
            vec![Op::Route {
                bank,
                ch: p.ch,
                sources: vec![s],
            }]
        };
        st.submit_all(ops);
        view.picker = None;
    } else if !open {
        view.picker = None;
    }
}

pub fn show(ui: &mut Ui, st: &mut AppState, view: &mut View) {
    ui.horizontal_top(|ui| {
        bank_list(ui, st, view);
        ui.separator();
        rows(ui, st, view);
    });
    picker(ui.ctx(), st, view);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn adat(chan: u8) -> Source {
        Source::Input {
            group: SourceGroup::AdatIn,
            chan,
        }
    }

    #[test]
    fn consecutive_runs() {
        let sel: BTreeSet<u8> = [0, 1, 2, 5, 6].into_iter().collect();
        let ops = plan_routes(OutputBank::LineOut, &sel, adat(4));
        assert_eq!(
            ops,
            vec![
                Op::Route {
                    bank: OutputBank::LineOut,
                    ch: 0,
                    sources: vec![adat(4), adat(5), adat(6)]
                },
                Op::Route {
                    bank: OutputBank::LineOut,
                    ch: 5,
                    sources: vec![adat(7), adat(8)]
                },
            ]
        );
    }

    #[test]
    fn stops_at_group_end_and_bank_end() {
        // S/PDIF IN has 2 channels: only two outputs get a source.
        let sel: BTreeSet<u8> = (0..5).collect();
        let sp = Source::Input {
            group: SourceGroup::SpdifIn,
            chan: 0,
        };
        let ops = plan_routes(OutputBank::LineOut, &sel, sp);
        assert_eq!(ops.len(), 1);
        let Op::Route { sources, .. } = &ops[0] else {
            panic!()
        };
        assert_eq!(sources.len(), 2);
        // MONITOR has 2 outputs: selection beyond is ignored.
        let ops = plan_routes(OutputBank::Monitor, &sel, Source::Mute);
        let Op::Route { sources, .. } = &ops[0] else {
            panic!()
        };
        assert_eq!(sources, &vec![Source::Mute, Source::Mute]);
    }

    #[test]
    fn columns_hold_whole_blocks_of_eight() {
        assert_eq!(rows_per_column(32, 4), 8);
        assert_eq!(rows_per_column(32, 3), 16);
        assert_eq!(rows_per_column(32, 1), 32);
        assert_eq!(rows_per_column(2, 4), 8);
        assert_eq!(rows_per_column(16, 4), 8);
    }

    #[test]
    fn shift_click_selects_a_range() {
        let mut v = View::default();
        click_output(&mut v, 2, false);
        click_output(&mut v, 6, true);
        assert_eq!(v.selected, (2..=6).collect());
        click_output(&mut v, 4, false);
        assert!(!v.selected.contains(&4));
    }

    #[test]
    fn sources_listed() {
        let all = all_sources();
        let n: usize = SourceGroup::ALL.iter().map(|g| g.size() as usize).sum();
        assert_eq!(all.len(), n + 1);
        assert_eq!(all[0].1, "MUTE");
    }
}
