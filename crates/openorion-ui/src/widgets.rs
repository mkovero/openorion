//! Small custom widgets: meters, knobs, fader, lamp, source chip.

use egui::{
    Align2, Color32, CornerRadius, FontId, Pos2, Rect, Response, RichText, Sense, Stroke,
    StrokeKind, Ui, Vec2,
};

use openorion_proto::METER_FLOOR;

use crate::meters::{
    METER_RANGE_DB, PeakHold, db_fraction, is_clip, meter_fraction, meter_label, raw_to_db,
};
use crate::theme;

/// Monospace value text.
pub fn mono(s: impl Into<String>) -> RichText {
    RichText::new(s.into()).monospace()
}

pub fn dim(s: impl Into<String>) -> RichText {
    RichText::new(s.into()).color(theme::TEXT_DIM)
}

/// A thin vertical meter: fill = level, line = peak hold, red at 0 dB.
pub fn meter_bar(ui: &mut Ui, raw: u8, hold: PeakHold, size: Vec2) -> Response {
    let (rect, resp) = ui.allocate_exact_size(size, Sense::hover());
    if ui.is_rect_visible(rect) {
        let p = ui.painter();
        p.rect_filled(rect, CornerRadius::same(1), theme::METER_BG);
        let h = rect.height() * meter_fraction(raw);
        if h > 0.0 {
            let fill = Rect::from_min_max(Pos2::new(rect.left(), rect.bottom() - h), rect.max);
            p.rect_filled(fill, CornerRadius::same(1), theme::ACCENT);
        }
        // Clip cap: the top 3 px turn red while the hold is at 0 dB.
        if hold.clipped() || is_clip(raw) {
            let cap = Rect::from_min_size(rect.min, Vec2::new(rect.width(), 3.0));
            p.rect_filled(cap, CornerRadius::ZERO, theme::RED);
        }
        let hf = meter_fraction(hold.raw);
        if hf > 0.0 && !hold.clipped() {
            let y = rect.bottom() - rect.height() * hf;
            p.line_segment(
                [Pos2::new(rect.left(), y), Pos2::new(rect.right(), y)],
                Stroke::new(1.5, theme::TEXT),
            );
        }
    }
    resp.on_hover_text(format!(
        "{} dB (peak {})",
        meter_label(raw),
        meter_label(hold.raw)
    ))
}

/// Meter colour for a level in dB: accent up to -18, amber to -6, red
/// above.
fn zone_color(db: f32) -> Color32 {
    if db > -6.0 {
        theme::RED
    } else if db > -18.0 {
        theme::SOLO
    } else {
        theme::ACCENT
    }
}

/// Scale marks of a meter bank (dB).
const BANK_TICKS: [f32; 9] = [0.0, -6.0, -12.0, -18.0, -24.0, -36.0, -48.0, -60.0, -96.0];

/// A bank of meters filling `size`: dB gridlines across all bars with
/// labels on both sides, bars coloured by zone with a peak-hold line, the
/// maximum since the last reset above each bar (click the numbers to reset
/// `peaks`), channel numbers below. `peaks` holds raw maxima and is
/// updated here. Hover a bar for its name and values.
pub fn meter_bank(
    ui: &mut Ui,
    vals: &[u8],
    holds: &[PeakHold],
    peaks: &mut [u8],
    names: &dyn Fn(usize) -> String,
    size: Vec2,
) -> Response {
    let (rect, resp) = ui.allocate_exact_size(size, Sense::click());
    let n = vals.len().max(1);
    for (p, &v) in peaks.iter_mut().zip(vals) {
        *p = (*p).min(v);
    }
    let scale_w = 26.0;
    let head_h = 18.0;
    let foot_h = 16.0;
    let bars = Rect::from_min_max(
        Pos2::new(rect.left() + scale_w, rect.top() + head_h),
        Pos2::new(rect.right() - scale_w, rect.bottom() - foot_h),
    );
    let cell_w = bars.width() / n as f32;
    let bar_w = (cell_w * 0.62).clamp(4.0, 34.0);
    let y_of = |db: f32| bars.bottom() - bars.height() * db_fraction(db);
    let cell = |i: usize| {
        let cx = bars.left() + cell_w * (i as f32 + 0.5);
        Rect::from_min_max(
            Pos2::new(cx - bar_w / 2.0, bars.top()),
            Pos2::new(cx + bar_w / 2.0, bars.bottom()),
        )
    };

    if ui.is_rect_visible(rect) {
        let p = ui.painter();
        for t in BANK_TICKS {
            let y = y_of(t);
            p.line_segment(
                [Pos2::new(bars.left(), y), Pos2::new(bars.right(), y)],
                Stroke::new(1.0, theme::LINE.gamma_multiply(0.6)),
            );
            let label = format!("{}", t as i32);
            for (x, align) in [
                (bars.left() - 5.0, Align2::RIGHT_CENTER),
                (bars.right() + 5.0, Align2::LEFT_CENTER),
            ] {
                p.text(
                    Pos2::new(x, y),
                    align,
                    &label,
                    FontId::monospace(9.0),
                    theme::TEXT_DIM,
                );
            }
        }
        let font = FontId::monospace(if cell_w >= 30.0 { 10.0 } else { 8.5 });
        for (i, (&v, h)) in vals.iter().zip(holds).enumerate() {
            let r = cell(i);
            p.rect_filled(
                r,
                CornerRadius::same(2),
                theme::METER_BG.gamma_multiply(0.9),
            );
            if let Some(db) = raw_to_db(v) {
                // Fill zone by zone so each keeps its colour.
                let mut lo = -METER_RANGE_DB;
                for hi in [-18.0, -6.0, 0.0] {
                    if db > lo {
                        let top = y_of(db.min(hi));
                        let bot = y_of(lo);
                        if bot > top {
                            p.rect_filled(
                                Rect::from_min_max(
                                    Pos2::new(r.left(), top),
                                    Pos2::new(r.right(), bot),
                                ),
                                CornerRadius::ZERO,
                                zone_color(hi - 0.5),
                            );
                        }
                    }
                    lo = hi;
                }
            }
            if let Some(hdb) = raw_to_db(h.raw).filter(|d| *d > -METER_RANGE_DB) {
                let y = y_of(hdb);
                p.line_segment(
                    [Pos2::new(r.left(), y), Pos2::new(r.right(), y)],
                    Stroke::new(2.0, zone_color(hdb)),
                );
            }
            // Max since reset.
            let pk = peaks.get(i).copied().unwrap_or(METER_FLOOR);
            let top = Pos2::new(r.center().x, rect.top() + head_h / 2.0);
            if is_clip(pk) {
                let b =
                    Rect::from_center_size(top, Vec2::new(cell_w.min(40.0) - 2.0, head_h - 4.0));
                p.rect_filled(b, CornerRadius::same(3), theme::RED);
                p.text(top, Align2::CENTER_CENTER, "CLIP", font.clone(), theme::BG);
            } else if let Some(db) = raw_to_db(pk) {
                p.text(
                    top,
                    Align2::CENTER_CENTER,
                    format!("{}", db as i32),
                    font.clone(),
                    if db > -6.0 { theme::RED } else { theme::TEXT },
                );
            }
            p.text(
                Pos2::new(r.center().x, rect.bottom() - foot_h / 2.0),
                Align2::CENTER_CENTER,
                format!("{}", i + 1),
                font.clone(),
                theme::TEXT_DIM,
            );
        }
    }

    // Click on the peak numbers resets them.
    let head = Rect::from_min_max(rect.min, Pos2::new(rect.right(), rect.top() + head_h));
    if resp.clicked()
        && resp
            .interact_pointer_pos()
            .is_some_and(|pos| head.contains(pos))
    {
        peaks.fill(METER_FLOOR);
    }
    let hovered = resp
        .hover_pos()
        .filter(|pos| bars.x_range().contains(pos.x))
        .map(|pos| (((pos.x - bars.left()) / cell_w) as usize).min(n - 1));
    match hovered {
        Some(i) if i < vals.len() => resp.on_hover_text(format!(
            "{}\nnow {} dB · hold {} · max {}\nclick the numbers on top to reset maxima",
            names(i),
            meter_label(vals[i]),
            meter_label(holds[i].raw),
            meter_label(peaks.get(i).copied().unwrap_or(METER_FLOOR)),
        )),
        _ => resp.on_hover_text("click the numbers on top to reset maxima"),
    }
}

/// Knob input: drag up/down (shift = fine), scroll, double-click = reset.
/// Marks the response changed when the value moved.
fn knob_input(ui: &Ui, resp: &mut Response, value: &mut f32, min: f32, max: f32, reset: f32) {
    let old = *value;
    if resp.dragged() {
        // Accumulate in memory: callers quantize, and per-frame deltas are
        // often smaller than one step.
        let id = resp.id;
        if resp.drag_started() {
            ui.data_mut(|d| d.insert_temp(id, *value));
        }
        let d = -resp.drag_delta().y;
        let fine = if ui.input(|i| i.modifiers.shift) {
            0.1
        } else {
            1.0
        };
        let acc = ui.data(|m| m.get_temp::<f32>(id)).unwrap_or(*value);
        let acc = (acc + d * (max - min) / 200.0 * fine).clamp(min, max);
        ui.data_mut(|m| m.insert_temp(id, acc));
        *value = acc;
    }
    if resp.hovered() {
        let scroll = ui.input(|i| i.smooth_scroll_delta.y);
        if scroll != 0.0 {
            *value += scroll.signum() * (max - min) / 96.0;
        }
    }
    if resp.double_clicked() {
        *value = reset;
    }
    *value = value.clamp(min, max);
    if *value != old {
        resp.mark_changed();
    }
}

/// Points on the knob arc between two fractions of its sweep.
fn knob_arc(c: Pos2, r: f32, from: f32, to: f32) -> Vec<Pos2> {
    let a0 = std::f32::consts::PI * 0.75;
    let sweep = std::f32::consts::PI * 1.5;
    let n = 32;
    (0..=n)
        .map(|i| {
            let a = a0 + sweep * (from + (to - from) * i as f32 / n as f32);
            c + Vec2::angled(a) * r
        })
        .collect()
}

/// A rotary knob. Drag up/down (or scroll) to change, double-click to reset.
pub fn knob(
    ui: &mut Ui,
    value: &mut f32,
    min: f32,
    max: f32,
    reset: f32,
    diameter: f32,
) -> Response {
    let (rect, mut resp) = ui.allocate_exact_size(Vec2::splat(diameter), Sense::click_and_drag());
    knob_input(ui, &mut resp, value, min, max, reset);
    if ui.is_rect_visible(rect) {
        let p = ui.painter();
        let c = rect.center();
        let r = diameter / 2.0 - 3.0;
        let frac = (*value - min) / (max - min);
        p.add(egui::Shape::line(
            knob_arc(c, r, 0.0, 1.0),
            Stroke::new(3.0, theme::RAISED),
        ));
        if frac > 0.001 {
            p.add(egui::Shape::line(
                knob_arc(c, r, 0.0, frac),
                Stroke::new(3.0, theme::ACCENT),
            ));
        }
        let fill = if resp.hovered() || resp.dragged() {
            theme::HOVER
        } else {
            theme::RAISED
        };
        p.circle_filled(c, r - 5.0, fill);
        let a = std::f32::consts::PI * (0.75 + 1.5 * frac);
        p.line_segment(
            [
                c + Vec2::angled(a) * (r - 13.0),
                c + Vec2::angled(a) * (r - 6.0),
            ],
            Stroke::new(2.0, theme::TEXT),
        );
    }
    resp
}

/// A small strip knob. `bipolar` draws the value arc from the centre
/// (pan) instead of from the minimum.
pub fn mini_knob(
    ui: &mut Ui,
    value: &mut f32,
    (min, max): (f32, f32),
    reset: f32,
    diameter: f32,
    bipolar: bool,
) -> Response {
    let (rect, mut resp) = ui.allocate_exact_size(Vec2::splat(diameter), Sense::click_and_drag());
    knob_input(ui, &mut resp, value, min, max, reset);
    if ui.is_rect_visible(rect) {
        let p = ui.painter();
        let c = rect.center();
        let r = diameter / 2.0 - 2.0;
        let frac = (*value - min) / (max - min);
        p.add(egui::Shape::line(
            knob_arc(c, r, 0.0, 1.0),
            Stroke::new(2.5, theme::RAISED),
        ));
        let origin = if bipolar { 0.5 } else { 0.0 };
        if (frac - origin).abs() > 0.005 {
            p.add(egui::Shape::line(
                knob_arc(c, r, origin.min(frac), origin.max(frac)),
                Stroke::new(2.5, theme::ACCENT),
            ));
        }
        let fill = if resp.hovered() || resp.dragged() {
            theme::HOVER
        } else {
            theme::RAISED
        };
        p.circle_filled(c, r - 4.0, fill);
        let a = std::f32::consts::PI * (0.75 + 1.5 * frac);
        p.line_segment(
            [
                c + Vec2::angled(a) * (r * 0.2),
                c + Vec2::angled(a) * (r - 4.0),
            ],
            Stroke::new(2.0, theme::TEXT),
        );
    }
    resp
}

/// Fader taper: (dB, position 0..=1). More travel near 0 dB, the bottom
/// stretch compressed; -95 is -inf.
const TAPER: [(f32, f32); 7] = [
    (-95.0, 0.0),
    (-60.0, 0.10),
    (-40.0, 0.24),
    (-24.0, 0.44),
    (-12.0, 0.66),
    (-6.0, 0.82),
    (0.0, 1.0),
];

/// Fader position 0..=1 for a dB value.
pub fn fader_pos(db: f32) -> f32 {
    let db = db.clamp(TAPER[0].0, 0.0);
    TAPER
        .windows(2)
        .find(|w| db <= w[1].0)
        .map(|w| {
            let ((d0, p0), (d1, p1)) = (w[0], w[1]);
            p0 + (db - d0) / (d1 - d0) * (p1 - p0)
        })
        .unwrap_or(1.0)
}

/// dB for a fader position 0..=1 (inverse of `fader_pos`).
pub fn fader_db(pos: f32) -> f32 {
    let pos = pos.clamp(0.0, 1.0);
    TAPER
        .windows(2)
        .find(|w| pos <= w[1].1)
        .map(|w| {
            let ((d0, p0), (d1, p1)) = (w[0], w[1]);
            d0 + (pos - p0) / (p1 - p0) * (d1 - d0)
        })
        .unwrap_or(0.0)
}

/// A channel fader with its meter on one shared dB scale: tick labels on
/// the left, the meter, then the fader track and cap. Drag the cap (shift
/// = fine), scroll for 1 dB steps, double-click for 0 dB. `db` is in
/// -95..=0 (-95 = -inf). `labels` = draw the dB numbers (off for the
/// second strip of a pair, which shares the first one's scale).
pub fn fader(
    ui: &mut Ui,
    db: &mut f32,
    meter: Option<(u8, PeakHold)>,
    size: Vec2,
    labels: bool,
) -> Response {
    let (rect, mut resp) = ui.allocate_exact_size(size, Sense::click_and_drag());
    let old = *db;
    // Usable travel: keep half a cap of room at both ends.
    let cap_h = 14.0;
    let top = rect.top() + cap_h / 2.0;
    let bottom = rect.bottom() - cap_h / 2.0;
    let travel = bottom - top;
    let y_of = |pos: f32| bottom - travel * pos;
    if resp.dragged() {
        let id = resp.id;
        if resp.drag_started() {
            ui.data_mut(|d| d.insert_temp(id, fader_pos(*db)));
        }
        let fine = if ui.input(|i| i.modifiers.shift) {
            0.15
        } else {
            1.0
        };
        let acc = ui.data(|m| m.get_temp::<f32>(id)).unwrap_or(fader_pos(*db));
        let acc = (acc - resp.drag_delta().y / travel * fine).clamp(0.0, 1.0);
        ui.data_mut(|m| m.insert_temp(id, acc));
        *db = fader_db(acc);
    }
    if resp.hovered() {
        let scroll = ui.input(|i| i.smooth_scroll_delta.y);
        if scroll != 0.0 {
            *db += scroll.signum();
        }
    }
    if resp.double_clicked() {
        *db = 0.0;
    }
    *db = db.clamp(-95.0, 0.0);
    if *db != old {
        resp.mark_changed();
    }

    if ui.is_rect_visible(rect) {
        let p = ui.painter();
        let x0 = rect.left();
        let scale_r = x0 + 15.0;
        let meter_x = x0 + 17.0;
        let meter_w = 7.0;
        let track_x = (meter_x + meter_w + rect.right()) / 2.0;
        for t in [0.0, -6.0, -12.0, -24.0, -40.0, -60.0] {
            let y = y_of(fader_pos(t));
            if labels {
                p.text(
                    Pos2::new(scale_r, y),
                    Align2::RIGHT_CENTER,
                    format!("{}", -t as i32),
                    FontId::monospace(8.5),
                    theme::TEXT_DIM,
                );
            }
            p.line_segment(
                [Pos2::new(track_x - 6.0, y), Pos2::new(track_x + 6.0, y)],
                Stroke::new(1.0, theme::LINE),
            );
        }
        if labels {
            p.text(
                Pos2::new(scale_r, y_of(0.0)),
                Align2::RIGHT_CENTER,
                "-∞",
                FontId::monospace(8.5),
                theme::TEXT_DIM,
            );
        }

        // Meter on the fader's scale (raw = dB below full scale).
        let mrect = Rect::from_min_max(
            Pos2::new(meter_x, top),
            Pos2::new(meter_x + meter_w, bottom),
        );
        p.rect_filled(mrect, CornerRadius::same(1), theme::METER_BG);
        if let Some((raw, hold)) = meter {
            let lvl = fader_pos(-(raw as f32));
            if lvl > 0.0 {
                let fill = Rect::from_min_max(Pos2::new(mrect.left(), y_of(lvl)), mrect.max);
                p.rect_filled(fill, CornerRadius::same(1), zone_color(-(raw as f32)));
            }
            if hold.clipped() || is_clip(raw) {
                let cap = Rect::from_min_size(mrect.min, Vec2::new(meter_w, 3.0));
                p.rect_filled(cap, CornerRadius::ZERO, theme::RED);
            } else {
                let hp = fader_pos(-(hold.raw as f32));
                if hp > 0.0 {
                    let y = y_of(hp);
                    p.line_segment(
                        [Pos2::new(mrect.left(), y), Pos2::new(mrect.right(), y)],
                        Stroke::new(1.5, theme::TEXT),
                    );
                }
            }
        }

        // Track (a plain groove, so it never reads as a meter) and cap.
        let track = Rect::from_center_size(
            Pos2::new(track_x, (top + bottom) / 2.0),
            Vec2::new(3.0, travel),
        );
        p.rect_filled(track, CornerRadius::same(1), theme::METER_BG);
        let cy = y_of(fader_pos(*db));
        let cap = Rect::from_center_size(Pos2::new(track_x, cy), Vec2::new(16.0, cap_h));
        let active = resp.hovered() || resp.dragged();
        p.rect_filled(
            cap,
            CornerRadius::same(3),
            if active { theme::HOVER } else { theme::RAISED },
        );
        p.rect_stroke(
            cap,
            CornerRadius::same(3),
            Stroke::new(1.0, if active { theme::ACCENT } else { theme::LINE }),
            StrokeKind::Inside,
        );
        p.line_segment(
            [
                Pos2::new(cap.left() + 4.0, cy),
                Pos2::new(cap.right() - 4.0, cy),
            ],
            Stroke::new(2.0, theme::ACCENT),
        );
    }
    let tip = match meter {
        Some((raw, hold)) => format!(
            "fader {} dB · meter {} dB (peak {})\ndrag (shift = fine), scroll = 1 dB, double-click = 0 dB",
            if *db <= -95.0 {
                "-inf".to_owned()
            } else {
                format!("{}", *db as i32)
            },
            meter_label(raw),
            meter_label(hold.raw)
        ),
        None => "drag (shift = fine), scroll = 1 dB, double-click = 0 dB".to_owned(),
    };
    resp.on_hover_text(tip)
}

/// A compact strip button (mute, solo, link) with its own "on" colours.
pub fn strip_toggle(
    ui: &mut Ui,
    on: bool,
    text: &str,
    on_fill: Color32,
    on_text: Color32,
    width: f32,
) -> Response {
    let (fill, fg) = if on {
        (on_fill, on_text)
    } else {
        (theme::RAISED, theme::TEXT_DIM)
    };
    ui.add(
        egui::Button::new(RichText::new(text).strong().size(12.0).color(fg))
            .fill(fill)
            .min_size(Vec2::new(width, 20.0)),
    )
}

/// A small status lamp with a label.
pub fn lamp(ui: &mut Ui, on: Color32, lit: bool, label: &str) -> Response {
    ui.horizontal(|ui| {
        let (rect, _) = ui.allocate_exact_size(Vec2::splat(12.0), Sense::hover());
        let col = if lit { on } else { theme::METER_BG };
        ui.painter().circle_filled(rect.center(), 5.0, col);
        ui.painter()
            .circle_stroke(rect.center(), 5.0, Stroke::new(1.0, theme::LINE));
        ui.label(RichText::new(label).monospace().color(if lit {
            theme::TEXT
        } else {
            theme::TEXT_DIM
        }));
    })
    .response
}

/// A routing source chip: rounded, clickable, filled with `fill` (None =
/// hollow, for MUTE). `ring` outlines it (the current choice).
pub fn chip(ui: &mut Ui, text: &str, fill: Option<Color32>, width: f32, ring: bool) -> Response {
    let (rect, resp) = ui.allocate_exact_size(Vec2::new(width, 24.0), Sense::click());
    if ui.is_rect_visible(rect) {
        let p = ui.painter();
        let hovered = resp.hovered();
        let r = CornerRadius::same(12);
        match fill {
            None => {
                p.rect_stroke(
                    rect,
                    r,
                    Stroke::new(1.0, if hovered { theme::ACCENT } else { theme::LINE }),
                    StrokeKind::Inside,
                );
            }
            Some(f) => {
                p.rect_filled(rect, r, if hovered { f.gamma_multiply(1.35) } else { f });
            }
        }
        if ring {
            p.rect_stroke(rect, r, Stroke::new(2.0, theme::TEXT), StrokeKind::Inside);
        }
        p.text(
            rect.center(),
            Align2::CENTER_CENTER,
            text,
            FontId::monospace(12.0),
            if fill.is_none() {
                theme::TEXT_DIM
            } else {
                theme::TEXT
            },
        );
    }
    resp
}

/// A toggle button that lights in the accent (or red) when on.
pub fn toggle(ui: &mut Ui, on: bool, text: &str, alarm: bool) -> Response {
    let fill = match (on, alarm) {
        (true, true) => theme::RED,
        (true, false) => theme::ACCENT_DIM,
        _ => theme::RAISED,
    };
    ui.add(
        egui::Button::new(RichText::new(text).strong())
            .fill(fill)
            .min_size(Vec2::new(30.0, 26.0)),
    )
}
