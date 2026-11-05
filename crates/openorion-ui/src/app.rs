//! The eframe app: panels, tabs, outbox, the confirm and clock keep /
//! revert dialogs, toasts and the daemon connection banner.

use egui::{Color32, RichText, Ui};
use openorion_ipc::Health;
use openorion_proto::Risk;

use crate::bridge::Bridge;
use crate::state::{AppState, Conn, ToastKind};
use crate::tabs;
use crate::theme;
use crate::topbar;
use crate::widgets::{dim, mono};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tab {
    Overview,
    Routing,
    Mixers,
    Trims,
    Afx,
    Settings,
    Snapshots,
    Remote,
}

impl Tab {
    const ALL: [Tab; 8] = [
        Tab::Overview,
        Tab::Routing,
        Tab::Mixers,
        Tab::Trims,
        Tab::Afx,
        Tab::Settings,
        Tab::Snapshots,
        Tab::Remote,
    ];

    fn title(self) -> &'static str {
        match self {
            Tab::Overview => "Overview",
            Tab::Routing => "Routing",
            Tab::Mixers => "Mixers",
            Tab::Trims => "Trims",
            Tab::Afx => "AFX",
            Tab::Settings => "Settings",
            Tab::Snapshots => "Snapshots",
            Tab::Remote => "Remote",
        }
    }
}

pub struct OrionApp {
    pub state: AppState,
    tab: Tab,
    routing: tabs::routing::View,
    mixers: tabs::mixers::View,
    pub afx: tabs::afx::View,
    snapshots: tabs::snapshots::View,
    show_outbox: bool,
    /// Live mode: the openoriond connection. None = offline (demo / file).
    bridge: Option<Bridge>,
    /// Dev aid: `ORION_UI_SHOTS=DIR` saves one PPM per tab, then quits.
    shots: Option<Shots>,
}

struct Shots {
    dir: std::path::PathBuf,
    idx: usize,
    frames: u32,
    requested: bool,
}

impl OrionApp {
    pub fn new(state: AppState, bridge: Option<Bridge>) -> Self {
        let live = bridge.is_some();
        OrionApp {
            state,
            tab: Tab::Overview,
            routing: Default::default(),
            mixers: Default::default(),
            afx: Default::default(),
            snapshots: Default::default(),
            show_outbox: !live,
            bridge,
            shots: std::env::var_os("ORION_UI_SHOTS").map(|d| Shots {
                dir: d.into(),
                idx: 0,
                frames: 0,
                requested: false,
            }),
        }
    }

    fn tab_bar(&mut self, ui: &mut Ui) {
        ui.horizontal(|ui| {
            for t in Tab::ALL {
                let sel = self.tab == t;
                let text = RichText::new(t.title()).size(15.0);
                let text = if sel {
                    text.color(theme::ACCENT).strong()
                } else {
                    text
                };
                if ui
                    .add(
                        egui::Button::new(text)
                            .frame(sel)
                            .min_size(egui::vec2(84.0, 30.0)),
                    )
                    .clicked()
                {
                    self.tab = t;
                }
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let n = self.state.outbox.len();
                ui.toggle_value(&mut self.show_outbox, format!("Sent ({n})"));
                ui.separator();
                topbar::learn_controls(ui, &mut self.state);
            });
        });
    }

    fn outbox(&mut self, ui: &mut Ui) {
        let st = &mut self.state;
        ui.horizontal(|ui| {
            ui.label(
                RichText::new("OUTBOX")
                    .strong()
                    .small()
                    .color(theme::TEXT_DIM),
            );
            ui.label(dim(if st.is_live() {
                "ops sent to openoriond (newest last; slider drags merged)"
            } else {
                "offline: edits only change the local copy"
            }));
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.button("Clear").clicked() {
                    st.outbox.clear();
                }
                if ui.button("Copy JSON").clicked() {
                    let ops: Vec<_> = st.outbox.iter().map(|e| &e.op).collect();
                    if let Ok(s) = serde_json::to_string_pretty(&ops) {
                        ui.ctx().copy_text(s);
                    }
                }
                ui.separator();
                if st.is_live() {
                    ui.label(dim(format!("state: {}", st.origin)));
                    return;
                }
                egui::ComboBox::from_id_salt("health-preview")
                    .selected_text(format!("health: {:?}", st.health))
                    .show_ui(ui, |ui| {
                        for h in [
                            Health::Ok,
                            Health::Syncing,
                            Health::Stalled,
                            Health::Hung,
                            Health::NoCyclic,
                            Health::Disconnected,
                        ] {
                            ui.selectable_value(&mut st.health, h, format!("{h:?}"));
                        }
                    });
                ui.checkbox(&mut st.fake_meters, "fake meters");
                ui.label(dim(format!("state: {}", st.origin)));
            });
        });
        ui.separator();
        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .stick_to_bottom(true)
            .show(ui, |ui| {
                if st.outbox.is_empty() {
                    ui.label(dim("(empty)"));
                }
                for e in &st.outbox {
                    ui.horizontal(|ui| {
                        ui.label(mono(format!("#{:<4}", e.seq)).color(theme::TEXT_DIM));
                        let (txt, col) = risk_style(e.risk);
                        ui.label(mono(format!("{txt:<10}")).color(col));
                        ui.label(crate::ops::describe(&e.op));
                        if e.merged > 0 {
                            ui.label(dim(format!("(x{})", e.merged + 1)));
                        }
                        let json = serde_json::to_string(&e.op).unwrap_or_default();
                        ui.label(mono(json).color(theme::TEXT_DIM).size(11.0));
                    });
                }
            });
    }

    fn confirm_dialog(&mut self, ctx: &egui::Context) {
        if self.state.pending.is_empty() {
            return;
        }
        let mut decision = None;
        let resp = egui::Modal::new(egui::Id::new("confirm-op")).show(ctx, |ui| {
            ui.set_max_width(460.0);
            ui.heading("Confirm");
            ui.add_space(6.0);
            for op in &self.state.pending {
                ui.label(RichText::new(crate::ops::describe(op)).strong());
                for w in crate::ops::warnings(op) {
                    ui.label(RichText::new(w).color(theme::TEXT_DIM));
                }
            }
            ui.add_space(10.0);
            ui.horizontal(|ui| {
                if ui
                    .add(egui::Button::new("Cancel").min_size(egui::vec2(100.0, 32.0)))
                    .clicked()
                {
                    decision = Some(false);
                }
                let go = egui::Button::new(RichText::new("Do it").strong().color(Color32::WHITE))
                    .fill(theme::RED)
                    .min_size(egui::vec2(100.0, 32.0));
                if ui.add(go).clicked() {
                    decision = Some(true);
                }
            });
        });
        if resp.should_close() && decision.is_none() {
            decision = Some(false);
        }
        match decision {
            Some(true) => self.state.confirm(),
            Some(false) => self.state.cancel(),
            None => {}
        }
    }
}

impl OrionApp {
    /// Keep / revert after a clock change that did not lock.
    fn clock_dialog(&mut self, ctx: &egui::Context) {
        let Some((target, prev, left)) = self.state.clock_prompt() else {
            return;
        };
        use openorion_proto::names::clock_source_name;
        let mut keep = false;
        let mut revert = false;
        egui::Modal::new(egui::Id::new("clock-revert")).show(ctx, |ui| {
            ui.set_max_width(460.0);
            ui.heading("No clock lock");
            ui.add_space(6.0);
            ui.label(format!(
                "{} has not locked after {:.0} s.",
                clock_source_name(target),
                crate::state::LOCK_WAIT
            ));
            match prev {
                Some(p) if p != target => ui.label(
                    RichText::new(format!(
                        "Reverting to {} in {:.0} s.",
                        clock_source_name(p),
                        left.ceil()
                    ))
                    .strong()
                    .color(theme::RED),
                ),
                _ => ui.label(dim("The previous source is unknown; nothing to revert to.")),
            };
            ui.add_space(10.0);
            ui.horizontal(|ui| {
                if ui
                    .add(egui::Button::new("Keep").min_size(egui::vec2(100.0, 32.0)))
                    .clicked()
                {
                    keep = true;
                }
                if prev.is_some_and(|p| p != target) {
                    let b = egui::Button::new(
                        RichText::new("Revert now").strong().color(Color32::WHITE),
                    )
                    .fill(theme::ACCENT_DIM)
                    .min_size(egui::vec2(100.0, 32.0));
                    if ui.add(b).clicked() {
                        revert = true;
                    }
                }
            });
        });
        if keep {
            self.state.clock_keep();
        } else if revert {
            self.state.clock_revert();
        }
        // Repaint for the countdown.
        ctx.request_repaint_after(std::time::Duration::from_millis(250));
    }

    fn toasts(&mut self, ctx: &egui::Context) {
        if self.state.toasts.is_empty() {
            return;
        }
        egui::Area::new(egui::Id::new("toasts"))
            .anchor(egui::Align2::RIGHT_BOTTOM, egui::vec2(-16.0, -16.0))
            .order(egui::Order::Foreground)
            .interactable(false)
            .show(ctx, |ui| {
                ui.set_max_width(420.0);
                for t in &self.state.toasts {
                    let col = match t.kind {
                        ToastKind::Info => theme::ACCENT,
                        ToastKind::Warn => Color32::from_rgb(0xe0, 0xb0, 0x4f),
                        ToastKind::Error => theme::RED,
                    };
                    egui::Frame::new()
                        .fill(theme::RAISED)
                        .stroke(egui::Stroke::new(1.0, col))
                        .corner_radius(6.0)
                        .inner_margin(egui::Margin::symmetric(12, 8))
                        .show(ui, |ui| {
                            ui.label(RichText::new(&t.text).color(theme::TEXT));
                        });
                    ui.add_space(6.0);
                }
            });
    }

    /// Live mode without a usable connection.
    fn banner(&self, ui: &mut Ui) {
        let (head, detail) = match &self.state.conn {
            Conn::Offline => return,
            Conn::Connected(_) if self.state.synced => return,
            Conn::Connected(_) => (
                "Connected to openoriond; waiting for the device state…".to_owned(),
                self.state
                    .health_detail
                    .clone()
                    .unwrap_or_else(|| format!("device health: {:?}", self.state.health)),
            ),
            Conn::Connecting => (
                "Connecting to openoriond…".to_owned(),
                self.state.origin.clone(),
            ),
            Conn::Down(e) => (
                "Daemon not running".to_owned(),
                format!(
                    "Start it with `openoriond` or `systemctl --user start openoriond`. Retrying every {} s. ({e})",
                    crate::bridge::RETRY.as_secs()
                ),
            ),
        };
        egui::Frame::new()
            .fill(theme::RAISED)
            .stroke(egui::Stroke::new(1.0, theme::RED))
            .inner_margin(egui::Margin::symmetric(12, 8))
            .show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.label(RichText::new(head).strong().color(theme::RED));
                ui.label(dim(detail));
            });
        ui.add_space(8.0);
    }
}

pub fn risk_style(r: Risk) -> (&'static str, Color32) {
    match r {
        Risk::Silent => ("silent", theme::TEXT_DIM),
        Risk::Audible => ("audible", theme::ACCENT),
        Risk::Disruptive => ("DISRUPTIVE", theme::RED),
    }
}

impl OrionApp {
    fn shots(&mut self, ctx: &egui::Context) {
        let Some(sh) = &mut self.shots else { return };
        if sh.idx >= Tab::ALL.len() {
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            return;
        }
        self.tab = Tab::ALL[sh.idx];
        if self.tab == Tab::Afx {
            self.afx.sel = Some((0, 0));
        }
        let image = ctx.input(|i| {
            i.events.iter().find_map(|e| match e {
                egui::Event::Screenshot { image, .. } => Some(image.clone()),
                _ => None,
            })
        });
        if let Some(img) = image {
            let mut ppm = format!("P6\n{} {}\n255\n", img.size[0], img.size[1]).into_bytes();
            for p in &img.pixels {
                ppm.extend_from_slice(&[p.r(), p.g(), p.b()]);
            }
            let name = format!("{}-{}.ppm", sh.idx, self.tab.title().to_lowercase());
            if let Err(e) = std::fs::write(sh.dir.join(name), ppm) {
                eprintln!("openorion-ui: screenshot: {e}");
            }
            sh.idx += 1;
            sh.frames = 0;
            sh.requested = false;
            return;
        }
        sh.frames += 1;
        if sh.frames > 45 && !sh.requested {
            sh.requested = true;
            ctx.send_viewport_cmd(egui::ViewportCommand::Screenshot(Default::default()));
        }
    }
}

impl eframe::App for OrionApp {
    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        let now = ctx.input(|i| i.time);
        match &self.bridge {
            Some(b) => {
                b.pump(&mut self.state);
            }
            None => self.state.sendq.clear(),
        }
        self.state.tick(now);
        // Throttled sends that became due this frame go out now.
        if let Some(b) = &self.bridge {
            b.pump(&mut self.state);
        }
        if self.tab != Tab::Mixers {
            self.mixers.reset_follow();
        }
        // Learn mode: Esc clears the selection.
        if self.state.learning() && ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
            self.state.learn_select(None);
        }
        self.shots(ctx);
        // ~30 Hz for the meters.
        ctx.request_repaint_after(std::time::Duration::from_millis(33));
    }

    /// Closing mid-learn would leave the remote in learn mode (controllers
    /// not driving the device), so end it and keep this session's binds.
    fn on_exit(&mut self) {
        if !self.state.learning() {
            return;
        }
        self.state.learn_stop(true);
        if let Some(b) = &self.bridge {
            b.pump(&mut self.state);
            // The bridge thread writes it; no join, so give it a moment.
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
    }

    fn ui(&mut self, ui: &mut Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        let bar = egui::Frame::new()
            .fill(theme::BG)
            .inner_margin(egui::Margin::symmetric(12, 8));
        egui::Panel::top("topbar").frame(bar).show(ui, |ui| {
            let ready = self.state.ready();
            ui.add_enabled_ui(ready, |ui| topbar::show(ui, &mut self.state));
        });
        egui::Panel::top("tabs")
            .frame(
                egui::Frame::new()
                    .fill(theme::PANEL)
                    .inner_margin(egui::Margin::symmetric(12, 4)),
            )
            .show(ui, |ui| self.tab_bar(ui));
        if self.show_outbox {
            egui::Panel::bottom("outbox")
                .resizable(true)
                .default_size(120.0)
                .frame(
                    egui::Frame::new()
                        .fill(theme::BG)
                        .inner_margin(egui::Margin::same(8)),
                )
                .show(ui, |ui| self.outbox(ui));
        }
        egui::CentralPanel::default()
            .frame(
                egui::Frame::new()
                    .fill(theme::PANEL)
                    .inner_margin(egui::Margin::same(12)),
            )
            .show(ui, |ui| {
                self.banner(ui);
                let ready = self.state.ready();
                ui.add_enabled_ui(ready, |ui| match self.tab {
                    Tab::Overview => tabs::overview::show(ui, &mut self.state),
                    Tab::Routing => tabs::routing::show(ui, &mut self.state, &mut self.routing),
                    Tab::Mixers => tabs::mixers::show(ui, &mut self.state, &mut self.mixers),
                    Tab::Trims => tabs::trims::show(ui, &mut self.state),
                    Tab::Afx => tabs::afx::show(ui, &mut self.state, &mut self.afx),
                    Tab::Settings => tabs::settings::show(ui, &mut self.state),
                    Tab::Snapshots => {
                        tabs::snapshots::show(ui, &mut self.state, &mut self.snapshots)
                    }
                    Tab::Remote => tabs::remote::show(ui, &mut self.state),
                });
            });
        self.confirm_dialog(&ctx);
        self.clock_dialog(&ctx);
        self.toasts(&ctx);
    }
}
