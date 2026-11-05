//! Dark, calm studio theme: neutral greys, one accent, monospace values,
//! large hit targets (DESIGN.md 8.2).

use egui::{Color32, CornerRadius, FontFamily, FontId, Margin, Stroke, TextStyle, Vec2};
use openorion_proto::{Source, SourceGroup};

pub const BG: Color32 = Color32::from_rgb(0x14, 0x16, 0x19);
pub const PANEL: Color32 = Color32::from_rgb(0x1b, 0x1e, 0x22);
pub const RAISED: Color32 = Color32::from_rgb(0x25, 0x29, 0x2e);
pub const HOVER: Color32 = Color32::from_rgb(0x30, 0x35, 0x3b);
pub const LINE: Color32 = Color32::from_rgb(0x34, 0x39, 0x40);
pub const TEXT: Color32 = Color32::from_rgb(0xd4, 0xd7, 0xdc);
pub const TEXT_DIM: Color32 = Color32::from_rgb(0x84, 0x8b, 0x94);
/// The one accent (calm teal).
pub const ACCENT: Color32 = Color32::from_rgb(0x4f, 0xb3, 0xa9);
pub const ACCENT_DIM: Color32 = Color32::from_rgb(0x2a, 0x5c, 0x57);
/// Alarm only: clip, hung, unlocked, disruptive.
pub const RED: Color32 = Color32::from_rgb(0xe0, 0x55, 0x4f);
/// Channel mute "on": a quieter red than the alarm colour, since many
/// channels sit muted.
pub const MUTE: Color32 = Color32::from_rgb(0x7a, 0x36, 0x33);
/// Solo "on".
pub const SOLO: Color32 = Color32::from_rgb(0xd6, 0xb2, 0x4c);
/// Lamp "off" / unused meter background.
pub const METER_BG: Color32 = Color32::from_rgb(0x0e, 0x10, 0x12);

/// Chip fill for a routing source: one muted hue per kind of source, so
/// routing patterns are visible at a glance. None = MUTE (hollow chip).
pub fn source_fill(s: Source) -> Option<Color32> {
    use SourceGroup::*;
    let Source::Input { group, .. } = s else {
        return None;
    };
    Some(match group {
        LineIn => Color32::from_rgb(0x2f, 0x52, 0x7a),
        UsbPlay1 | UsbPlay33 => Color32::from_rgb(0x55, 0x45, 0x7e),
        MadiIn1 | MadiIn33 => Color32::from_rgb(0x74, 0x58, 0x28),
        Hdx1Play | Hdx2Play => Color32::from_rgb(0x3e, 0x4e, 0x5c),
        AdatIn => ACCENT_DIM,
        SpdifIn => Color32::from_rgb(0x55, 0x5e, 0x2e),
        AfxOut => Color32::from_rgb(0x74, 0x3a, 0x5c),
        Mix1 | Mix2 | Mix3 | Mix4 => Color32::from_rgb(0x2e, 0x62, 0x3c),
        Oscillator => Color32::from_rgb(0x7c, 0x40, 0x2c),
    })
}

pub fn apply(ctx: &egui::Context) {
    ctx.set_visuals(visuals());
    ctx.all_styles_mut(|style| {
        style.visuals = visuals();
        let s = &mut style.spacing;
        s.item_spacing = Vec2::new(8.0, 6.0);
        s.button_padding = Vec2::new(10.0, 5.0);
        s.interact_size = Vec2::new(40.0, 28.0);
        s.slider_width = 160.0;
        s.slider_rail_height = 6.0;
        s.window_margin = Margin::same(14);
        s.combo_width = 140.0;
        use FontFamily::{Monospace, Proportional};
        style.text_styles = [
            (TextStyle::Small, FontId::new(11.0, Proportional)),
            (TextStyle::Body, FontId::new(14.0, Proportional)),
            (TextStyle::Button, FontId::new(14.0, Proportional)),
            (TextStyle::Heading, FontId::new(18.0, Proportional)),
            (TextStyle::Monospace, FontId::new(13.0, Monospace)),
        ]
        .into();
    });
}

fn visuals() -> egui::Visuals {
    let mut v = egui::Visuals::dark();
    v.panel_fill = PANEL;
    v.window_fill = PANEL;
    v.extreme_bg_color = METER_BG;
    v.faint_bg_color = Color32::from_rgb(0x1f, 0x22, 0x27);
    v.window_stroke = Stroke::new(1.0, LINE);
    v.window_corner_radius = CornerRadius::same(8);
    v.override_text_color = None;
    v.hyperlink_color = ACCENT;
    v.selection.bg_fill = ACCENT_DIM;
    v.selection.stroke = Stroke::new(1.0, ACCENT);
    v.error_fg_color = RED;
    v.warn_fg_color = RED;
    let r = CornerRadius::same(5);
    let w = &mut v.widgets;
    w.noninteractive.bg_fill = PANEL;
    w.noninteractive.weak_bg_fill = PANEL;
    w.noninteractive.bg_stroke = Stroke::new(1.0, LINE);
    w.noninteractive.fg_stroke = Stroke::new(1.0, TEXT);
    w.noninteractive.corner_radius = r;
    w.inactive.bg_fill = RAISED;
    w.inactive.weak_bg_fill = RAISED;
    w.inactive.bg_stroke = Stroke::NONE;
    w.inactive.fg_stroke = Stroke::new(1.0, TEXT);
    w.inactive.corner_radius = r;
    w.hovered.bg_fill = HOVER;
    w.hovered.weak_bg_fill = HOVER;
    w.hovered.bg_stroke = Stroke::new(1.0, ACCENT_DIM);
    w.hovered.fg_stroke = Stroke::new(1.5, TEXT);
    w.hovered.corner_radius = r;
    w.active.bg_fill = ACCENT_DIM;
    w.active.weak_bg_fill = ACCENT_DIM;
    w.active.bg_stroke = Stroke::new(1.0, ACCENT);
    w.active.fg_stroke = Stroke::new(2.0, TEXT);
    w.active.corner_radius = r;
    w.open.bg_fill = RAISED;
    w.open.weak_bg_fill = RAISED;
    w.open.corner_radius = r;
    v
}
