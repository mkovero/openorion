//! One module per tab (DESIGN.md 8.2). Each takes the `AppState` and,
//! where it has selection state, its own `View`.

pub mod afx;
pub mod mixers;
pub mod overview;
pub mod remote;
pub mod routing;
pub mod settings;
pub mod snapshots;
pub mod trims;

use egui::{RichText, Ui};

use crate::theme;

/// A titled card with a slightly raised background.
pub fn card<R>(ui: &mut Ui, title: &str, add: impl FnOnce(&mut Ui) -> R) -> R {
    egui::Frame::new()
        .fill(theme::RAISED.gamma_multiply(0.55))
        .corner_radius(8)
        .inner_margin(egui::Margin::same(12))
        .show(ui, |ui| {
            // Cards often sit in a horizontal row; keep their content vertical.
            ui.vertical(|ui| {
                ui.label(RichText::new(title).strong().color(theme::TEXT_DIM));
                ui.add_space(4.0);
                add(ui)
            })
            .inner
        })
        .inner
}
