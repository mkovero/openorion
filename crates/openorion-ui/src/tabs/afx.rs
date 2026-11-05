//! AFX: 16 chains (labelled by their AFX IN source) x 8 slots, and an
//! editor for the selected slot: ClearQ (curve + bands), PowerFFC (curve +
//! fields), other models as raw values. Bypass, chain links, meters.

use std::f32::consts::PI;

use egui::{Align2, FontId, Pos2, RichText, Sense, Stroke, Ui, Vec2};
use openorion_ipc::Op;
use openorion_proto::{
    AFX_CHAINS, AFX_SLOTS, AfxConfig, AfxParams, AfxSlot, AfxState, AfxType, CompressorParams,
    EqBand, EqFilter, EqParams, LinkTarget, OutputBank, Source,
};

use openorion_control::{CompField, EqField, Param};

use super::card;
use crate::labels::{afx_type_name, source_name};
use crate::learn::Learnable;
use crate::state::AppState;
use crate::theme;
use crate::widgets::{dim, meter_bar, mono, toggle};

#[derive(Default)]
pub struct View {
    /// (chain, slot)
    pub sel: Option<(usize, usize)>,
}

// ---------------------------------------------------------------------------
// Pure helpers

/// Lowest instance of `ty` that no chain uses, below the device's count.
pub fn free_instance(afx: &AfxState, ty: AfxType) -> Option<u8> {
    let count = afx.instances.get(&ty).copied().unwrap_or(0);
    (0..count).find(|i| {
        !afx.order
            .iter()
            .flatten()
            .flatten()
            .any(|s| s.ty == ty && s.inst == *i)
    })
}

/// Occupied slots of a chain, packed.
pub fn chain_slots(afx: &AfxState, chain: usize) -> Vec<AfxSlot> {
    afx.order[chain].iter().flatten().copied().collect()
}

/// RBJ biquad magnitude in dB of one band at `f` Hz (sample rate `fs`).
pub fn band_db(b: &EqBand, f: f32, fs: f32) -> f32 {
    let q = if b.q == 0 { 0.707 } else { b.q as f32 / 100.0 }.max(0.1);
    let f0 = (b.freq as f32).clamp(10.0, fs * 0.49);
    let a = 10f32.powf(b.gain as f32 / 100.0 / 40.0);
    let w0 = 2.0 * PI * f0 / fs;
    let (cs, sn) = (w0.cos(), w0.sin());
    let alpha = sn / (2.0 * q);
    let sa = 2.0 * a.sqrt() * alpha;
    let (b0, b1, b2, a0, a1, a2) = match b.ftype {
        EqFilter::Peak => (
            1.0 + alpha * a,
            -2.0 * cs,
            1.0 - alpha * a,
            1.0 + alpha / a,
            -2.0 * cs,
            1.0 - alpha / a,
        ),
        EqFilter::LowShelf => (
            a * ((a + 1.0) - (a - 1.0) * cs + sa),
            2.0 * a * ((a - 1.0) - (a + 1.0) * cs),
            a * ((a + 1.0) - (a - 1.0) * cs - sa),
            (a + 1.0) + (a - 1.0) * cs + sa,
            -2.0 * ((a - 1.0) + (a + 1.0) * cs),
            (a + 1.0) + (a - 1.0) * cs - sa,
        ),
        EqFilter::HighShelf => (
            a * ((a + 1.0) + (a - 1.0) * cs + sa),
            -2.0 * a * ((a - 1.0) + (a + 1.0) * cs),
            a * ((a + 1.0) + (a - 1.0) * cs - sa),
            (a + 1.0) - (a - 1.0) * cs + sa,
            2.0 * ((a - 1.0) - (a + 1.0) * cs),
            (a + 1.0) - (a - 1.0) * cs - sa,
        ),
        EqFilter::Lpf => {
            let k = (1.0 - cs) / 2.0;
            (k, 1.0 - cs, k, 1.0 + alpha, -2.0 * cs, 1.0 - alpha)
        }
        EqFilter::Hpf => {
            let k = (1.0 + cs) / 2.0;
            (k, -(1.0 + cs), k, 1.0 + alpha, -2.0 * cs, 1.0 - alpha)
        }
    };
    let w = 2.0 * PI * f / fs;
    let mag2 = |c0: f32, c1: f32, c2: f32| {
        let re = c0 + c1 * w.cos() + c2 * (2.0 * w).cos();
        let im = c1 * w.sin() + c2 * (2.0 * w).sin();
        re * re + im * im
    };
    10.0 * (mag2(b0, b1, b2) / mag2(a0, a1, a2)).log10()
}

/// Whole-EQ response in dB at `f`.
pub fn eq_db(eq: &EqParams, f: f32, fs: f32) -> f32 {
    eq.bands.iter().map(|b| band_db(b, f, fs)).sum()
}

/// Static compressor curve: output dBFS for input dBFS (soft knee,
/// makeup included).
pub fn comp_out_db(c: &CompressorParams, x: f32) -> f32 {
    let t = -(c.threshold as f32);
    let r = (c.ratio as f32 / 100.0).max(1.0);
    let w = c.knee as f32;
    let makeup = c.gain as f32 / 100.0;
    let d = x - t;
    let y = if 2.0 * d < -w {
        x
    } else if w > 0.0 && 2.0 * d.abs() <= w {
        x + (1.0 / r - 1.0) * (d + w / 2.0).powi(2) / (2.0 * w)
    } else {
        t + d / r
    };
    y + makeup
}

// ---------------------------------------------------------------------------
// Chain grid

fn chain_label(st: &AppState, chain: usize) -> String {
    let s = st.device.routing[OutputBank::AfxIn as usize]
        .sources
        .get(chain)
        .copied()
        .unwrap_or(Source::Mute);
    if s == Source::Mute {
        "—".to_owned()
    } else {
        source_name(s)
    }
}

fn grid(ui: &mut Ui, st: &mut AppState, view: &mut View) {
    let mut ops = Vec::new();
    egui::Grid::new("afx-grid")
        .spacing([6.0, 4.0])
        .striped(true)
        .show(ui, |ui| {
            ui.label(dim("chain"));
            ui.label(dim("input"));
            ui.label(dim("link"));
            for s in 0..AFX_SLOTS {
                ui.label(dim(format!("slot {}", s + 1)));
            }
            ui.label("");
            ui.end_row();
            for chain in 0..AFX_CHAINS {
                ui.label(mono(format!("AFX {:>2}", chain + 1)));
                ui.add_sized(
                    [110.0, 24.0],
                    egui::Label::new(RichText::new(chain_label(st, chain)).small()).truncate(),
                );
                if chain % 2 == 0 {
                    let pair = (chain / 2) as u8;
                    let linked = st.device.afx.links[pair as usize];
                    if toggle(ui, linked, "LINK", false)
                        .on_hover_text(format!("link chains {}-{}", chain + 1, chain + 2))
                        .learnable(ui, st, Param::AfxLink { pair })
                        .clicked()
                    {
                        ops.push(Op::StereoLink {
                            target: LinkTarget::Afx { pair },
                            linked: !linked,
                        });
                    }
                } else {
                    ui.label("");
                }
                for slot in 0..AFX_SLOTS {
                    let cell = st.device.afx.order[chain][slot];
                    ui.horizontal(|ui| {
                        ui.spacing_mut().item_spacing.x = 2.0;
                        let sel = view.sel == Some((chain, slot));
                        match cell {
                            Some(s) => {
                                let enabled = config(&st.device.afx, s).is_none_or(|c| c.enabled);
                                let mut t = RichText::new(afx_type_name(s.ty)).small();
                                if !enabled {
                                    t = t.color(theme::TEXT_DIM).strikethrough();
                                }
                                if ui
                                    .add_sized(
                                        [96.0, 26.0],
                                        egui::Button::new(t).fill(if sel {
                                            theme::ACCENT_DIM
                                        } else {
                                            theme::RAISED
                                        }),
                                    )
                                    .on_hover_text(format!(
                                        "{} #{}",
                                        afx_type_name(s.ty),
                                        s.inst + 1
                                    ))
                                    .clicked()
                                {
                                    view.sel = Some((chain, slot));
                                }
                                meter_bar(
                                    ui,
                                    st.afx_meters.peak[chain][slot],
                                    st.holds.afx[chain][slot],
                                    Vec2::new(4.0, 26.0),
                                );
                            }
                            None => {
                                ui.add_sized([96.0, 26.0], egui::Label::new(dim("·")));
                                ui.add_space(4.0);
                            }
                        }
                    });
                }
                let used = chain_slots(&st.device.afx, chain);
                ui.add_enabled_ui(used.len() < AFX_SLOTS, |ui| {
                    ui.menu_button("+", |ui| {
                        egui::ScrollArea::vertical()
                            .max_height(360.0)
                            .show(ui, |ui| {
                                for &ty in AfxType::ALL.iter().filter(|t| t.on_orion()) {
                                    let free = free_instance(&st.device.afx, ty);
                                    let r = ui.add_enabled(
                                        free.is_some(),
                                        egui::Button::new(afx_type_name(ty)),
                                    );
                                    if r.clicked()
                                        && let Some(inst) = free
                                    {
                                        let mut slots = used.clone();
                                        slots.push(AfxSlot { ty, inst });
                                        ops.push(Op::AfxChain {
                                            chain: chain as u8,
                                            slots,
                                        });
                                        ui.close();
                                    }
                                }
                            });
                    })
                    .response
                    .on_hover_text("add a model to this chain");
                });
                ui.end_row();
            }
        });
    st.submit_all(ops);
}

fn config(afx: &AfxState, s: AfxSlot) -> Option<&AfxConfig> {
    afx.configs.get(&s.ty).and_then(|v| v.get(s.inst as usize))
}

// ---------------------------------------------------------------------------
// Editors

const PLOT_F_MIN: f32 = 20.0;
const PLOT_F_MAX: f32 = 20_000.0;
const PLOT_DB: f32 = 24.0;

fn eq_plot(ui: &mut Ui, eq: &EqParams, fs: f32) {
    let (rect, _) = ui.allocate_exact_size(Vec2::new(520.0, 180.0), Sense::hover());
    let p = ui.painter_at(rect);
    p.rect_filled(rect, 6, theme::METER_BG);
    let fx = |f: f32| {
        rect.left() + rect.width() * (f / PLOT_F_MIN).ln() / (PLOT_F_MAX / PLOT_F_MIN).ln()
    };
    let dy =
        |db: f32| rect.center().y - db.clamp(-PLOT_DB, PLOT_DB) / PLOT_DB * rect.height() / 2.0;
    for f in [100.0, 1000.0, 10_000.0] {
        let x = fx(f);
        p.line_segment(
            [Pos2::new(x, rect.top()), Pos2::new(x, rect.bottom())],
            Stroke::new(1.0, theme::LINE),
        );
        let t = if f >= 1000.0 {
            format!("{}k", f / 1000.0)
        } else {
            format!("{f}")
        };
        p.text(
            Pos2::new(x + 3.0, rect.bottom() - 3.0),
            Align2::LEFT_BOTTOM,
            t,
            FontId::monospace(9.0),
            theme::TEXT_DIM,
        );
    }
    for db in [-12.0, 0.0, 12.0] {
        let y = dy(db);
        p.line_segment(
            [Pos2::new(rect.left(), y), Pos2::new(rect.right(), y)],
            Stroke::new(1.0, theme::LINE),
        );
        p.text(
            Pos2::new(rect.left() + 3.0, y - 1.0),
            Align2::LEFT_BOTTOM,
            format!("{db:+}"),
            FontId::monospace(9.0),
            theme::TEXT_DIM,
        );
    }
    let n = 200;
    let pts: Vec<Pos2> = (0..=n)
        .map(|i| {
            let f = PLOT_F_MIN * (PLOT_F_MAX / PLOT_F_MIN).powf(i as f32 / n as f32);
            Pos2::new(fx(f), dy(eq_db(eq, f, fs)))
        })
        .collect();
    p.add(egui::Shape::line(pts, Stroke::new(2.0, theme::ACCENT)));
    for (i, b) in eq.bands.iter().enumerate() {
        let c = Pos2::new(fx(b.freq as f32), dy(eq_db(eq, b.freq as f32, fs)));
        p.circle_filled(c, 5.0, theme::ACCENT_DIM);
        p.text(
            c,
            Align2::CENTER_CENTER,
            format!("{}", i + 1),
            FontId::monospace(9.0),
            theme::TEXT,
        );
    }
}

fn eq_editor(ui: &mut Ui, st: &mut AppState, inst: u8, eq: &EqParams, fs: f32) -> Vec<Op> {
    let tag = |band: usize, field: EqField| Param::Eq {
        inst,
        band: band as u8,
        field,
    };
    let mut ops = Vec::new();
    eq_plot(ui, eq, fs);
    ui.label(dim("curve is an RBJ-biquad approximation").small());
    let band_op = |band: usize| Op::AfxEqBand {
        inst,
        band: band as u8,
        freq: None,
        q: None,
        gain: None,
        ftype: None,
    };
    egui::Grid::new("eq-bands")
        .spacing([18.0, 6.0])
        .show(ui, |ui| {
            ui.label("");
            for i in 0..eq.bands.len() {
                ui.label(RichText::new(format!("BAND {}", i + 1)).strong());
            }
            ui.end_row();

            ui.label(dim("freq Hz"));
            for (i, b) in eq.bands.iter().enumerate() {
                let range = match i {
                    0 | 1 => 20..=800,
                    2 => 125..=8000,
                    _ => 400..=20000,
                };
                let mut f = b.freq;
                if ui
                    .add(egui::DragValue::new(&mut f).range(range).speed(5.0))
                    .learnable(ui, st, tag(i, EqField::Freq))
                    .changed()
                {
                    let mut o = band_op(i);
                    if let Op::AfxEqBand { freq, .. } = &mut o {
                        *freq = Some(f);
                    }
                    ops.push(o);
                }
            }
            ui.end_row();

            ui.label(dim("gain dB"));
            for (i, b) in eq.bands.iter().enumerate() {
                let cut = matches!(b.ftype, EqFilter::Lpf | EqFilter::Hpf);
                let mut g = b.gain as f32 / 100.0;
                let r = ui.add_enabled(
                    !cut,
                    egui::DragValue::new(&mut g)
                        .range(-24.0..=12.0)
                        .speed(0.1)
                        .fixed_decimals(1),
                );
                let r = if cut {
                    r
                } else {
                    r.learnable(ui, st, tag(i, EqField::Gain))
                };
                if r.changed() {
                    let mut o = band_op(i);
                    if let Op::AfxEqBand { gain, .. } = &mut o {
                        *gain = Some((g * 100.0).round() as i16);
                    }
                    ops.push(o);
                }
            }
            ui.end_row();

            ui.label(dim("Q"));
            for (i, b) in eq.bands.iter().enumerate() {
                if i == 0 || i == 4 {
                    ui.label(dim("—"));
                    continue;
                }
                let mut q = b.q as f32 / 100.0;
                if ui
                    .add(
                        egui::DragValue::new(&mut q)
                            .range(0.5..=18.0)
                            .speed(0.02)
                            .fixed_decimals(2),
                    )
                    .learnable(ui, st, tag(i, EqField::Q))
                    .changed()
                {
                    let mut o = band_op(i);
                    if let Op::AfxEqBand { q: qq, .. } = &mut o {
                        *qq = Some((q * 100.0).round() as u16);
                    }
                    ops.push(o);
                }
            }
            ui.end_row();

            ui.label(dim("type"));
            for (i, b) in eq.bands.iter().enumerate() {
                let choices: &[(EqFilter, &str)] = match i {
                    0 => &[(EqFilter::LowShelf, "SHELF"), (EqFilter::Hpf, "HPF")],
                    4 => &[(EqFilter::HighShelf, "SHELF"), (EqFilter::Lpf, "LPF")],
                    _ => &[(EqFilter::Peak, "PEAK")],
                };
                ui.horizontal(|ui| {
                    for &(ft, name) in choices {
                        if ui
                            .add(egui::Button::selectable(b.ftype == ft, name))
                            .clicked()
                            && b.ftype != ft
                        {
                            let mut o = band_op(i);
                            if let Op::AfxEqBand { ftype, gain, .. } = &mut o {
                                *ftype = Some(ft);
                                if matches!(ft, EqFilter::Lpf | EqFilter::Hpf) {
                                    *gain = Some(0);
                                }
                            }
                            ops.push(o);
                        }
                    }
                });
            }
            ui.end_row();
        });
    ops
}

fn comp_plot(ui: &mut Ui, c: &CompressorParams) {
    let size = 180.0;
    let (rect, _) = ui.allocate_exact_size(Vec2::splat(size), Sense::hover());
    let p = ui.painter_at(rect);
    p.rect_filled(rect, 6, theme::METER_BG);
    let lo = -80.0;
    let map = |x: f32, y: f32| {
        Pos2::new(
            rect.left() + (x - lo) / -lo * size,
            rect.bottom() - ((y - lo) / -lo * size).clamp(0.0, size),
        )
    };
    p.line_segment([map(lo, lo), map(0.0, 0.0)], Stroke::new(1.0, theme::LINE));
    let t = -(c.threshold as f32);
    p.line_segment([map(t, lo), map(t, 0.0)], Stroke::new(1.0, theme::LINE));
    let pts: Vec<Pos2> = (0..=80)
        .map(|i| {
            let x = lo + i as f32;
            map(x, comp_out_db(c, x))
        })
        .collect();
    p.add(egui::Shape::line(pts, Stroke::new(2.0, theme::ACCENT)));
    p.text(
        rect.left_top() + Vec2::new(4.0, 4.0),
        Align2::LEFT_TOP,
        "out",
        FontId::monospace(9.0),
        theme::TEXT_DIM,
    );
    p.text(
        rect.right_bottom() - Vec2::new(4.0, 4.0),
        Align2::RIGHT_BOTTOM,
        "in",
        FontId::monospace(9.0),
        theme::TEXT_DIM,
    );
}

/// Attack / release row: ms DragValue over a µs field.
#[allow(clippy::too_many_arguments)]
fn ms_field(ui: &mut Ui, st: &mut AppState, name: &str, v: &mut u32, lo: f32, hi: f32, tag: Param) {
    ui.label(dim(name));
    let mut ms = *v as f32 / 1000.0;
    if ui
        .add(
            egui::DragValue::new(&mut ms)
                .range(lo..=hi)
                .speed(0.1)
                .fixed_decimals(1)
                .suffix(" ms"),
        )
        .learnable(ui, st, tag)
        .changed()
    {
        *v = (ms * 1000.0).round() as u32;
    }
    ui.end_row();
}

fn comp_editor(ui: &mut Ui, st: &mut AppState, inst: u8, c: &CompressorParams) -> Vec<Op> {
    let mut n = *c;
    let tag = |field: CompField| Param::Comp { inst, field };
    ui.horizontal_top(|ui| {
        comp_plot(ui, c);
        ui.add_space(12.0);
        egui::Grid::new("comp-fields")
            .num_columns(2)
            .spacing([14.0, 6.0])
            .show(ui, |ui| {
                let (attack, release) = (tag(CompField::Attack), tag(CompField::Release));
                ms_field(ui, st, "attack", &mut n.attack, 0.1, 150.0, attack);
                ms_field(ui, st, "release", &mut n.release, 1.0, 600.0, release);

                ui.label(dim("detector"));
                let det = |t: u16| {
                    if t == 65535 {
                        "PEAK".to_owned()
                    } else {
                        format!("RMS {}", t / 100)
                    }
                };
                egui::ComboBox::from_id_salt("comp-taw")
                    .selected_text(det(n.taw))
                    .show_ui(ui, |ui| {
                        for t in [65535u16, 2000, 5000, 10000, 15000, 20000] {
                            ui.selectable_value(&mut n.taw, t, det(t));
                        }
                    })
                    .response
                    .learnable(ui, st, tag(CompField::Detector));
                ui.end_row();

                ui.label(dim("ratio"));
                let mut r = n.ratio as f32 / 100.0;
                if ui
                    .add(
                        egui::DragValue::new(&mut r)
                            .range(1.0..=20.0)
                            .speed(0.05)
                            .fixed_decimals(2)
                            .suffix(" :1"),
                    )
                    .learnable(ui, st, tag(CompField::Ratio))
                    .changed()
                {
                    n.ratio = (r * 100.0).round() as u16;
                }
                ui.end_row();

                ui.label(dim("threshold"));
                let mut t = -(n.threshold as i32);
                if ui
                    .add(egui::DragValue::new(&mut t).range(-80..=0).suffix(" dB"))
                    .learnable(ui, st, tag(CompField::Threshold))
                    .changed()
                {
                    n.threshold = (-t) as u8;
                }
                ui.end_row();

                ui.label(dim("knee"));
                ui.add(
                    egui::DragValue::new(&mut n.knee)
                        .range(0..=30)
                        .suffix(" dB"),
                )
                .learnable(ui, st, tag(CompField::Knee));
                ui.end_row();

                ui.label(dim("makeup"));
                let mut g = n.gain as f32 / 100.0;
                if ui
                    .add(
                        egui::DragValue::new(&mut g)
                            .range(0.0..=24.0)
                            .speed(0.1)
                            .fixed_decimals(1)
                            .suffix(" dB"),
                    )
                    .learnable(ui, st, tag(CompField::Makeup))
                    .changed()
                {
                    n.gain = (g * 100.0).round() as u16;
                }
                ui.end_row();
            });
    });
    if n != *c {
        vec![Op::AfxCompressor { inst, params: n }]
    } else {
        vec![]
    }
}

fn raw_editor(ui: &mut Ui, st: &mut AppState, ty: AfxType, inst: u8, fields: &[i32]) -> Vec<Op> {
    let mut changed: Vec<Option<i32>> = vec![None; fields.len()];
    ui.label(dim("Knob positions in get-reply order. Field names and ranges come with openorion_proto::names::afx_fields (WP1)."));
    egui::Grid::new("raw-fields")
        .spacing([14.0, 6.0])
        .show(ui, |ui| {
            for (i, &v) in fields.iter().enumerate() {
                let mut x = v;
                ui.label(dim(format!("field {}", i + 1)));
                if ui
                    .add(egui::DragValue::new(&mut x).range(-128..=1000))
                    .learnable(
                        ui,
                        st,
                        Param::AfxRaw {
                            ty,
                            inst,
                            field: i as u8,
                        },
                    )
                    .changed()
                {
                    changed[i] = Some(x);
                }
                if i % 4 == 3 {
                    ui.end_row();
                }
            }
        });
    if changed.iter().any(Option::is_some) {
        vec![Op::AfxRaw {
            ty,
            inst,
            fields: changed,
        }]
    } else {
        vec![]
    }
}

fn editor(ui: &mut Ui, st: &mut AppState, chain: usize, slot: usize) {
    let Some(s) = st.device.afx.order[chain][slot] else {
        return;
    };
    let fs = st
        .device
        .status
        .sample_rate
        .map_or(96_000.0, |r| r.hz() as f32);
    let cfg = config(&st.device.afx, s).cloned();
    let mut ops = Vec::new();
    card(
        ui,
        &format!("AFX {} · SLOT {}", chain + 1, slot + 1),
        |ui| {
            ui.horizontal(|ui| {
                ui.heading(afx_type_name(s.ty));
                ui.label(mono(format!("#{}", s.inst + 1)).color(theme::TEXT_DIM));
                if let Some(c) = &cfg {
                    let label = if c.enabled { "ACTIVE" } else { "BYPASSED" };
                    if toggle(ui, c.enabled, label, false)
                        .learnable(
                            ui,
                            st,
                            Param::Bypass {
                                ty: s.ty,
                                inst: s.inst,
                            },
                        )
                        .clicked()
                    {
                        ops.push(Op::AfxBypass {
                            ty: s.ty,
                            inst: s.inst,
                            enabled: !c.enabled,
                        });
                    }
                }
                if ui.button("Remove from chain").clicked() {
                    let mut slots = chain_slots(&st.device.afx, chain);
                    slots.retain(|x| *x != s);
                    ops.push(Op::AfxChain {
                        chain: chain as u8,
                        slots,
                    });
                }
                ui.separator();
                ui.label(dim("peak"));
                meter_bar(
                    ui,
                    st.afx_meters.peak[chain][slot],
                    st.holds.afx[chain][slot],
                    Vec2::new(6.0, 26.0),
                );
                ui.label(dim(format!(
                    "GR raw {}",
                    st.afx_meters.gain_reduction[chain][slot]
                )));
            });
            ui.add_space(6.0);
            match cfg.as_ref().map(|c| &c.params) {
                Some(AfxParams::Eq(eq)) => ops.extend(eq_editor(ui, st, s.inst, eq, fs)),
                Some(AfxParams::Compressor(c)) => ops.extend(comp_editor(ui, st, s.inst, c)),
                Some(AfxParams::Raw(r)) => ops.extend(raw_editor(ui, st, s.ty, s.inst, &r.fields)),
                None => {
                    ui.label(dim("No config has been read for this instance."));
                }
            }
        },
    );
    st.submit_all(ops);
}

pub fn show(ui: &mut Ui, st: &mut AppState, view: &mut View) {
    if let Some((c, s)) = view.sel
        && st.device.afx.order[c][s].is_none()
    {
        view.sel = None;
    }
    // Grid on top (scrolls on its own), editor below.
    let grid_h = (ui.available_height() * 0.45).max(160.0);
    egui::ScrollArea::vertical()
        .id_salt("afx-grid")
        .max_height(grid_h)
        .auto_shrink([false, true])
        .show(ui, |ui| grid(ui, st, view));
    ui.separator();
    egui::ScrollArea::vertical()
        .id_salt("afx-editor")
        .auto_shrink([false, false])
        .show(ui, |ui| match view.sel {
            Some((c, s)) => editor(ui, st, c, s),
            None => {
                ui.label(dim("Select a slot to edit it."));
            }
        });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::demo::{default_comp, default_eq, demo_state};

    #[test]
    fn instances() {
        let s = demo_state();
        // Demo uses ClearQ #0 and PowerFFC #0 in chain 1.
        assert_eq!(free_instance(&s.afx, AfxType::Equalizer), Some(1));
        assert_eq!(free_instance(&s.afx, AfxType::Powergate), Some(0));
        assert_eq!(free_instance(&s.afx, AfxType::GuitarAmp), None);
        assert_eq!(chain_slots(&s.afx, 0).len(), 2);
        assert!(chain_slots(&s.afx, 5).is_empty());
    }

    #[test]
    fn eq_curve() {
        let fs = 96_000.0;
        let flat = default_eq();
        for f in [20.0, 100.0, 1000.0, 15000.0] {
            assert!(eq_db(&flat, f, fs).abs() < 0.01, "flat at {f}");
        }
        let mut eq = default_eq();
        eq.bands[2].gain = 600;
        eq.bands[2].q = 100;
        assert!((eq_db(&eq, 1000.0, fs) - 6.0).abs() < 0.1);
        assert!(eq_db(&eq, 50.0, fs).abs() < 0.5);
        let mut hp = default_eq();
        hp.bands[0].ftype = EqFilter::Hpf;
        hp.bands[0].freq = 100;
        assert!(eq_db(&hp, 20.0, fs) < -20.0);
        assert!(eq_db(&hp, 5000.0, fs).abs() < 0.1);
    }

    #[test]
    fn comp_curve() {
        let mut c = default_comp();
        c.threshold = 20;
        c.ratio = 400;
        // Below threshold: unity.
        assert!((comp_out_db(&c, -40.0) + 40.0).abs() < 1e-4);
        // Above: 4:1.
        assert!((comp_out_db(&c, 0.0) + 15.0).abs() < 1e-4);
        c.gain = 600;
        assert!((comp_out_db(&c, 0.0) + 9.0).abs() < 1e-4);
        // Soft knee is continuous at its edges.
        c.gain = 0;
        c.knee = 10;
        let edge = comp_out_db(&c, -25.0);
        assert!((edge + 25.0).abs() < 1e-3);
        let upper = comp_out_db(&c, -15.0);
        assert!((upper - (-20.0 + 5.0 / 4.0)).abs() < 1e-3);
    }
}
