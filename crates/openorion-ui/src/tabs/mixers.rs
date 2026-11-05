//! Mixers 1-4: full-height channel strips (pan and send knobs, M/S,
//! fader with its meter on one dB scale) in linkable pairs, a master
//! strip pinned on the right, and a collapsible reverb panel.

use egui::{RichText, Ui, Vec2};
use openorion_ipc::Op;
use openorion_proto::{
    Atten, LinkTarget, MeterBank, MixerChannel, MixerId, OutputBank, Pan, Reverb, Source,
};

use openorion_control::{ChField, MasterField, Param, ReverbField as RvField};

use super::card;
use crate::labels::{atten_label, pan_label, source_name};
use crate::learn::Learnable;
use crate::meters::PeakHold;
use crate::state::AppState;
use crate::theme;
use crate::widgets::{dim, fader, mini_knob, mono, strip_toggle, toggle};

#[derive(Default)]
pub struct View {
    mixer: u8,
    /// Mixer the meter bank was last pointed at on this visit of the tab
    /// (None = not yet / tab left).
    followed: Option<u8>,
}

impl View {
    /// The tab was left: the next visit may point the meters again.
    pub fn reset_follow(&mut self) {
        self.followed = None;
    }
}

/// "Meters follow tab": the device meters one mixer at a time, so on
/// entering the tab or switching mixers point `MeterBank::Mixer` at the
/// visible mixer. Edge-triggered: one silent `MeterSource` write per
/// navigation, never per frame, and none when the toggle is off.
pub fn follow_meters(st: &mut AppState, view: &mut View) -> Option<Op> {
    if !st.meters_follow_tab || !st.ready() || view.followed == Some(view.mixer) {
        return None;
    }
    view.followed = Some(view.mixer);
    (st.device.status.meter_sources[MeterBank::Mixer as usize] != view.mixer).then_some(
        Op::MeterSource {
            bank: MeterBank::Mixer,
            src: view.mixer,
        },
    )
}

const STRIP_W: f32 = 44.0;
const MASTER_W: f32 = 60.0;
/// Gap between channel pairs.
const PAIR_GAP: f32 = 6.0;
/// Everything in a strip except the fader (header, knobs, buttons,
/// value, link row, spacing).
const STRIP_FIXED_H: f32 = 250.0;
const FADER_MIN_H: f32 = 150.0;
/// Fader floor: level ≥ 95 is -inf.
const FADER_MIN: f32 = -95.0;

/// dB shown on a fader for an attenuation (-95 = -inf).
pub fn atten_to_db(a: Atten) -> f32 {
    -(a.get().min(95) as f32)
}

pub fn db_to_atten(db: f32) -> Atten {
    Atten::new((-db).round().clamp(0.0, 95.0) as u8).expect("clamped")
}

fn mixer_op(m: MixerId, ch: u8) -> Op {
    Op::Mixer {
        mixer: m,
        ch,
        level: None,
        pan: None,
        mute: None,
        solo: None,
        send: None,
    }
}

/// Strip header: channel number and the routed source, up to two lines
/// (full name on hover).
fn strip_header(ui: &mut Ui, title: &str, label: Option<&str>, width: f32) {
    ui.label(
        RichText::new(title)
            .monospace()
            .strong()
            .color(theme::ACCENT),
    );
    let (text, color, hover) = match label {
        Some(l) => (l.to_owned(), theme::TEXT, l.to_owned()),
        None => (
            "—".to_owned(),
            theme::TEXT_DIM,
            "no input routed".to_owned(),
        ),
    };
    let mut job =
        egui::text::LayoutJob::simple(text, egui::FontId::proportional(10.0), color, width - 4.0);
    job.wrap.max_rows = 2;
    job.halign = egui::Align::Center;
    let galley = ui.fonts_mut(|f| f.layout_job(job));
    let (rect, resp) = ui.allocate_exact_size(Vec2::new(width, 26.0), egui::Sense::hover());
    ui.painter()
        .galley(egui::pos2(rect.center().x, rect.top()), galley, theme::TEXT);
    resp.on_hover_text(hover);
}

/// Learn tag of a strip control (`ch` 0 = master; the master has no pan,
/// solo or send).
fn strip_param(m: MixerId, ch: u8, f: ChField) -> Param {
    if ch == 0 {
        let field = match f {
            ChField::Mute => MasterField::Mute,
            _ => MasterField::Level,
        };
        Param::Master { mixer: m, field }
    } else {
        Param::Mixer {
            mixer: m,
            ch,
            field: f,
        }
    }
}

/// One strip. `ch` 0 = master. `label` None = no input. Returns ops.
#[allow(clippy::too_many_arguments)]
fn strip(
    ui: &mut Ui,
    st: &mut AppState,
    m: MixerId,
    ch: u8,
    c: MixerChannel,
    label: Option<&str>,
    meter: Option<(u8, PeakHold)>,
    fader_h: f32,
) -> Vec<Op> {
    let mut ops = Vec::new();
    let mut op = |f: &dyn Fn(&mut Op)| {
        let mut o = mixer_op(m, ch);
        f(&mut o);
        ops.push(o);
    };
    let w = if ch == 0 { MASTER_W } else { STRIP_W };
    ui.allocate_ui_with_layout(
        Vec2::new(w, fader_h + STRIP_FIXED_H),
        egui::Layout::top_down(egui::Align::Center),
        |ui| {
            ui.set_width(w);
            ui.spacing_mut().item_spacing.y = 3.0;
            // Narrow strips: buttons must not grow past their min size.
            ui.spacing_mut().button_padding = Vec2::new(2.0, 2.0);
            let title = if ch == 0 {
                "MASTER".to_owned()
            } else {
                ch.to_string()
            };
            strip_header(ui, &title, label, w);

            // Pan (channels only: the device keeps the master centred).
            if ch != 0 {
                let mut p = c.pan.display() as f32;
                let r = mini_knob(ui, &mut p, (-30.0, 30.0), 0.0, 30.0, true)
                    .on_hover_text("pan: drag or scroll, double-click = centre")
                    .learnable(ui, st, strip_param(m, ch, ChField::Pan));
                let np = (p.round() as i32 + 32) as u8;
                if r.changed()
                    && np != c.pan.get()
                    && let Ok(np) = Pan::new(np)
                {
                    op(&|o| {
                        if let Op::Mixer { pan, .. } = o {
                            *pan = Some(np)
                        }
                    });
                }
                ui.label(mono(pan_label(c.pan)).size(10.0).color(theme::TEXT_DIM));
            } else {
                // Keep the faders aligned with the channel strips.
                ui.add_space(30.0 + 3.0 + 14.0);
            }

            // Reverb send (channels only).
            if ch != 0 {
                let mut sdb = atten_to_db(c.send);
                let r = mini_knob(ui, &mut sdb, (FADER_MIN, 0.0), FADER_MIN, 26.0, false)
                    .on_hover_text("reverb send: drag or scroll, double-click = off")
                    .learnable(ui, st, strip_param(m, ch, ChField::Send));
                if r.changed() {
                    let a = db_to_atten(sdb);
                    if a != c.send {
                        op(&|o| {
                            if let Op::Mixer { send, .. } = o {
                                *send = Some(a)
                            }
                        });
                    }
                }
                ui.label(
                    mono(format!("snd {}", atten_label(c.send)))
                        .size(9.5)
                        .color(theme::TEXT_DIM),
                );
            } else {
                ui.add_space(26.0 + 3.0 + 14.0);
            }

            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 3.0;
                let bw = if ch == 0 { w - 6.0 } else { (w - 6.0) / 2.0 };
                if ch != 0 {
                    ui.add_space(1.0);
                }
                if strip_toggle(ui, c.mute, "M", theme::MUTE, theme::TEXT, bw)
                    .on_hover_text("mute")
                    .learnable(ui, st, strip_param(m, ch, ChField::Mute))
                    .clicked()
                {
                    op(&|o| {
                        if let Op::Mixer { mute, .. } = o {
                            *mute = Some(!c.mute)
                        }
                    });
                }
                if ch != 0
                    && strip_toggle(ui, c.solo, "S", theme::SOLO, theme::BG, bw)
                        .on_hover_text("solo")
                        .learnable(ui, st, strip_param(m, ch, ChField::Solo))
                        .clicked()
                {
                    op(&|o| {
                        if let Op::Mixer { solo, .. } = o {
                            *solo = Some(!c.solo)
                        }
                    });
                }
            });

            let mut db = atten_to_db(c.level);
            let r = fader(
                ui,
                &mut db,
                meter,
                Vec2::new(w - 4.0, fader_h),
                ch % 2 == 1 || ch == 0,
            )
            .learnable(ui, st, strip_param(m, ch, ChField::Level));
            if r.changed() {
                let a = db_to_atten(db);
                if a != c.level {
                    op(&|o| {
                        if let Op::Mixer { level, .. } = o {
                            *level = Some(a)
                        }
                    });
                }
            }
            ui.label(mono(atten_label(c.level)).strong());
        },
    );
    ops
}

/// Reverb parameter: label, accessor, UI range, learn tag.
type ReverbField = (&'static str, fn(&mut Reverb) -> &mut u8, u8, u8, RvField);

fn reverb_panel(ui: &mut Ui, st: &mut AppState, m: MixerId) {
    let Some(rv) = st
        .device
        .reverb
        .iter()
        .flatten()
        .find(|r| r.mixer == m)
        .copied()
    else {
        ui.label(dim("No reverb slot is assigned to this mixer."));
        return;
    };
    card(ui, "REVERB", |ui| {
        let mut new = rv;
        ui.horizontal(|ui| {
            if toggle(ui, rv.on, if rv.on { "ON" } else { "OFF" }, false)
                .learnable(
                    ui,
                    st,
                    Param::Reverb {
                        mixer: m,
                        field: RvField::On,
                    },
                )
                .clicked()
            {
                new.on = !rv.on;
            }
            ui.label(dim("density is fixed at 100"));
        });
        egui::Grid::new("reverb-grid")
            .num_columns(4)
            .spacing([16.0, 6.0])
            .show(ui, |ui| {
                let fields: [ReverbField; 8] = [
                    ("room size", |r| &mut r.room_size, 0, 100, RvField::RoomSize),
                    ("color", |r| &mut r.color, 0, 100, RvField::Color),
                    ("predelay", |r| &mut r.predelay, 0, 150, RvField::Predelay),
                    (
                        "early ref",
                        |r| &mut r.early_ref_gain,
                        0,
                        100,
                        RvField::EarlyRefGain,
                    ),
                    (
                        "late delay",
                        |r| &mut r.late_ref_delay,
                        0,
                        150,
                        RvField::LateRefDelay,
                    ),
                    ("richness", |r| &mut r.richness, 0, 100, RvField::Richness),
                    ("time", |r| &mut r.reverb_time, 0, 100, RvField::ReverbTime),
                    (
                        "level",
                        |r| &mut r.reverb_level,
                        1,
                        100,
                        RvField::ReverbLevel,
                    ),
                ];
                for (i, (name, get, lo, hi, field)) in fields.into_iter().enumerate() {
                    ui.label(dim(name));
                    ui.add(egui::Slider::new(get(&mut new), lo..=hi)).learnable(
                        ui,
                        st,
                        Param::Reverb { mixer: m, field },
                    );
                    if i % 2 == 1 {
                        ui.end_row();
                    }
                }
            });
        if new != rv {
            st.submit(Op::Reverb { params: new });
        }
    });
}

pub fn show(ui: &mut Ui, st: &mut AppState, view: &mut View) {
    let metered = st.device.status.meter_sources[MeterBank::Mixer as usize];
    let mut meter_here = false;
    ui.horizontal(|ui| {
        for i in 0..4u8 {
            let r = ui.add(
                egui::Button::selectable(
                    view.mixer == i,
                    RichText::new(format!("MIXER {}", i + 1)).strong(),
                )
                .min_size(Vec2::new(90.0, 30.0)),
            );
            if r.clicked() {
                view.mixer = i;
            }
        }
        ui.separator();
        if ui
            .checkbox(&mut st.meters_follow_tab, "meters follow tab")
            .on_hover_text(
                "The device meters one mixer at a time. When on, switching \
                 mixers here points the meters at the shown mixer (a silent \
                 device setting).",
            )
            .changed()
        {
            // Turning it on points the meters at this mixer right away.
            view.reset_follow();
        }
        if metered != view.mixer {
            ui.label(dim(format!("meters show mixer {}", metered + 1)));
            meter_here = ui
                .button("meter this mixer")
                .on_hover_text("point the device's mixer meters here (silent)")
                .clicked();
        }
    });
    let m = MixerId::new(view.mixer).expect("0..=3");
    if meter_here {
        st.submit(Op::MeterSource {
            bank: MeterBank::Mixer,
            src: view.mixer,
        });
    }
    if let Some(op) = follow_meters(st, view) {
        st.submit(op);
    }
    egui::CollapsingHeader::new(RichText::new("Reverb").strong())
        .id_salt(("reverb", view.mixer))
        .default_open(false)
        .show(ui, |ui| reverb_panel(ui, st, m));
    ui.separator();

    let mixer = st.device.mixers[view.mixer as usize].clone();
    let links = st.device.mixer_links[view.mixer as usize];
    let in_bank = OutputBank::try_from(OutputBank::MixCh1 as u8 + view.mixer).expect("mix bank");
    let inputs = st.device.routing[in_bank as usize].sources.clone();
    // Meters belong to whichever mixer the device meters; never show
    // another mixer's levels on these strips.
    let metering = metered == view.mixer;
    let meters: Vec<Option<(u8, PeakHold)>> = (0..mixer.channels.len())
        .map(|i| metering.then(|| (st.meters.mixer[i], st.holds.mixer[i])))
        .collect();
    let mut ops = Vec::new();

    // Pair frame inner margin + link row are part of STRIP_FIXED_H.
    let fader_h = (ui.available_height() - STRIP_FIXED_H - 24.0).max(FADER_MIN_H);
    let strips_w = (ui.available_width() - MASTER_W - 28.0).max(STRIP_W * 2.0);
    ui.horizontal_top(|ui| {
        ui.allocate_ui(Vec2::new(strips_w, ui.available_height()), |ui| {
            ui.style_mut().spacing.scroll = egui::style::ScrollStyle::solid();
            egui::ScrollArea::horizontal()
                .id_salt("mixer-strips")
                .auto_shrink([false, true])
                .show(ui, |ui| {
                    ui.horizontal_top(|ui| {
                        ui.spacing_mut().item_spacing.x = PAIR_GAP;
                        for (pair, &linked) in links.iter().enumerate() {
                            ops.extend(channel_pair(
                                ui, st, m, pair, linked, &mixer, &inputs, &meters, fader_h,
                            ));
                        }
                    });
                });
        });
        ui.separator();
        egui::Frame::new()
            .fill(ui.visuals().faint_bg_color)
            .corner_radius(6)
            .inner_margin(egui::Margin::symmetric(2, 4))
            .show(ui, |ui| {
                ops.extend(strip(
                    ui,
                    st,
                    m,
                    0,
                    mixer.master,
                    Some(&format!("MIX L/R{}", view.mixer + 1)),
                    None,
                    fader_h,
                ));
            });
    });
    st.submit_all(ops);
}

/// Two strips in a frame plus their LINK button; the frame is outlined
/// in the accent while the pair is linked.
#[allow(clippy::too_many_arguments)]
fn channel_pair(
    ui: &mut Ui,
    st: &mut AppState,
    m: MixerId,
    pair: usize,
    linked: bool,
    mixer: &openorion_proto::Mixer,
    inputs: &[Source],
    meters: &[Option<(u8, PeakHold)>],
    fader_h: f32,
) -> Vec<Op> {
    let mut ops = Vec::new();
    let stroke = if linked {
        egui::Stroke::new(1.0, theme::ACCENT_DIM)
    } else {
        egui::Stroke::NONE
    };
    egui::Frame::new()
        .fill(ui.visuals().faint_bg_color)
        .stroke(stroke)
        .corner_radius(6)
        .inner_margin(egui::Margin::symmetric(2, 4))
        .show(ui, |ui| {
            ui.vertical(|ui| {
                ui.spacing_mut().item_spacing = Vec2::new(2.0, 4.0);
                ui.horizontal_top(|ui| {
                    for i in [2 * pair, 2 * pair + 1] {
                        let src = inputs.get(i).copied().unwrap_or(Source::Mute);
                        let label = (src != Source::Mute).then(|| source_name(src));
                        ops.extend(strip(
                            ui,
                            st,
                            m,
                            i as u8 + 1,
                            mixer.channels[i],
                            label.as_deref(),
                            meters.get(i).copied().flatten(),
                            fader_h,
                        ));
                    }
                });
                let r = strip_toggle(
                    ui,
                    linked,
                    if linked { "LINKED" } else { "LINK" },
                    theme::ACCENT_DIM,
                    theme::TEXT,
                    2.0 * STRIP_W + 2.0,
                )
                .on_hover_text(format!(
                    "stereo-link {}-{}: level, mute, solo and send move together",
                    2 * pair + 1,
                    2 * pair + 2
                ))
                .learnable(
                    ui,
                    st,
                    Param::MixerLink {
                        mixer: m,
                        pair: pair as u8,
                    },
                );
                if r.clicked() {
                    ops.push(Op::StereoLink {
                        target: LinkTarget::Mixer {
                            mixer: m,
                            pair: pair as u8,
                        },
                        linked: !linked,
                    });
                }
            });
        });
    ops
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn meter_follow_is_edge_triggered_and_optional() {
        let mut st = AppState::new(crate::demo::demo_state(), "demo");
        st.meters_follow_tab = true;
        st.device.status.meter_sources[MeterBank::Mixer as usize] = 0;
        let mut v = View {
            mixer: 2,
            followed: None,
        };
        assert!(follow_meters(&mut st, &mut v).is_some());
        // Same visit, same mixer: nothing more, even if the device
        // still reports the old source.
        assert!(follow_meters(&mut st, &mut v).is_none());
        v.reset_follow();
        st.meters_follow_tab = false;
        assert!(follow_meters(&mut st, &mut v).is_none());
        st.meters_follow_tab = true;
        st.device.status.meter_sources[MeterBank::Mixer as usize] = 2;
        assert!(follow_meters(&mut st, &mut v).is_none(), "already there");
    }

    #[test]
    fn fader_mapping() {
        assert_eq!(atten_to_db(Atten::ZERO_DB), 0.0);
        assert_eq!(atten_to_db(Atten::new(12).unwrap()), -12.0);
        assert_eq!(atten_to_db(Atten::NEG_INF), -95.0);
        assert_eq!(db_to_atten(-12.4).get(), 12);
        assert_eq!(db_to_atten(-95.0).get(), 95);
        assert!(db_to_atten(-95.0).is_neg_inf());
        assert_eq!(db_to_atten(3.0).get(), 0);
    }
}
