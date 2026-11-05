//! Remote control in the UI: what openorion-remote announced, learn mode
//! (VJ-style: Learn on, click a control, move a MIDI/OSC control), and the
//! `Learnable` extension that tags each control with its `Param`.
//!
//! The UI never talks to openorion-remote directly: `RemoteMsg`s go
//! through openoriond as opaque `remote` messages (`Outgoing::Remote` /
//! `Incoming::Remote`). The remote's `Announce` is the source of truth;
//! the UI only patches its copy optimistically on `Learned` until the next
//! `Announce` replaces it.

use std::collections::HashMap;

use egui::{Align2, FontId, Response, Stroke, StrokeKind, Ui, Vec2};
use openorion_control::{
    ANY_DEVICE, MapOpts, Mappings, OscTarget, Param, PortInfo, RemoteMsg, Source,
};

use crate::bridge::Outgoing;
use crate::state::{AppState, ToastKind};
use crate::theme;

/// How long a device / source lamp stays lit after `Activity` (s).
pub const BLINK: f64 = 0.3;

/// The remote's last `Announce`, plus activity seen since.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct RemoteView {
    /// OSC listen address; None = OSC off.
    pub listen: Option<String>,
    pub midi_inputs: Vec<PortInfo>,
    pub midi_outputs: Vec<PortInfo>,
    pub osc_targets: Vec<OscTarget>,
    pub mappings: Mappings,
    /// The remote is in a learn session.
    pub learning: bool,
    /// Last `Activity` per device key (`device_key`).
    pub device_seen: HashMap<String, f64>,
    /// Last `Activity` per source (`Source` display string).
    pub source_seen: HashMap<String, f64>,
    /// OSC sender hosts seen, with the last time.
    pub osc_senders: Vec<(String, f64)>,
    /// Built-in demo view (`--demo`): nothing is listening.
    pub demo: bool,
}

impl RemoteView {
    /// Mapped sources of `param`, as badge text. The device's short name is
    /// added when mappings come from more than one device.
    pub fn badges(&self, param: &Param) -> Vec<String> {
        let multi = {
            let mut keys: Vec<String> = self
                .mappings
                .iter()
                .map(|m| device_key(&m.source))
                .collect();
            keys.sort();
            keys.dedup();
            keys.len() > 1
        };
        self.mappings
            .for_param(param)
            .map(|m| {
                let b = m.source.badge();
                let dev = match &m.source {
                    Source::Midi { device, .. } if device != ANY_DEVICE => Some(short(device)),
                    Source::Osc { from: Some(h), .. } => Some(short(h)),
                    _ => None,
                };
                match dev {
                    Some(d) if multi => format!("{d} {b}"),
                    _ => b,
                }
            })
            .collect()
    }

    /// The device lamp is lit.
    pub fn device_active(&self, key: &str, now: f64) -> bool {
        self.device_seen.get(key).is_some_and(|&t| now - t < BLINK)
    }

    pub fn source_active(&self, s: &Source, now: f64) -> bool {
        self.source_seen
            .get(&s.to_string())
            .is_some_and(|&t| now - t < BLINK)
    }
}

/// Learn mode on the UI side.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct LearnUi {
    /// The control the next moved source binds to.
    pub selected: Option<Param>,
    /// Learn only from this device (MIDI client name or OSC host).
    pub filter: Option<String>,
    /// An `Announce` confirmed the remote is learning. Until then a
    /// `learning: false` may predate our `LearnStart` and is ignored.
    pub confirmed: bool,
}

/// Activity key of a source's device: the MIDI client name, the OSC
/// sender host, or `OSC` for any sender.
pub fn device_key(s: &Source) -> String {
    match s {
        Source::Midi { device, .. } => device.clone(),
        Source::Osc { from: Some(h), .. } => h.clone(),
        Source::Osc { from: None, .. } => "OSC".into(),
    }
}

/// First word of a device name, at most 8 characters (`nanoKONTROL2`
/// → `nanoKONT`).
fn short(name: &str) -> String {
    let w = name
        .split(|c: char| c.is_whitespace() || c == ':')
        .find(|w| !w.is_empty())
        .unwrap_or(name);
    w.chars().take(8).collect()
}

/// Human label for a param ("Mixer 4 · ch 1 · level").
pub fn param_label(p: &Param) -> String {
    let path = p.to_string();
    let mut parts: Vec<String> = Vec::new();
    let mut it = path.split('/').peekable();
    while let Some(w) = it.next() {
        let s = match (w, it.peek()) {
            ("mixer" | "reverb", Some(n)) if n.parse::<u32>().is_ok() => {
                let n = it.next().unwrap_or_default();
                format!("{} {n}", capital(w))
            }
            ("ch" | "band", Some(n)) if n.parse::<u32>().is_ok() => {
                let n = it.next().unwrap_or_default();
                format!("{w} {n}")
            }
            _ => w.replace('_', " "),
        };
        parts.push(s);
    }
    parts.join(" · ")
}

fn capital(s: &str) -> String {
    let mut c = s.chars();
    c.next()
        .map(|f| f.to_uppercase().chain(c).collect())
        .unwrap_or_default()
}

// ---------------------------------------------------------------------------
// AppState: remote messages in and out

impl AppState {
    fn remote_send(&mut self, m: RemoteMsg) {
        if let Ok(v) = serde_json::to_value(&m) {
            self.sendq.push(Outgoing::Remote(v));
        }
    }

    /// Ask the remote to announce itself (on connect).
    pub fn remote_query(&mut self) {
        self.remote_send(RemoteMsg::Query);
    }

    /// Learn can start: a remote has announced itself.
    pub fn can_learn(&self) -> bool {
        self.remote.is_some()
    }

    pub fn learning(&self) -> bool {
        self.learn.is_some()
    }

    pub fn learn_start(&mut self) {
        if self.learn.is_some() || !self.can_learn() {
            return;
        }
        self.learn = Some(LearnUi::default());
        self.remote_send(RemoteMsg::LearnStart);
    }

    /// Leave learn mode: `save` writes the remote's config, else it
    /// reverts this session's binds.
    pub fn learn_stop(&mut self, save: bool) {
        if self.learn.take().is_some() {
            self.remote_send(RemoteMsg::LearnStop { save });
        }
    }

    pub fn learn_select(&mut self, param: Option<Param>) {
        let Some(l) = &mut self.learn else { return };
        if l.selected == param {
            return;
        }
        l.selected = param;
        self.remote_send(RemoteMsg::LearnSelect { param });
    }

    pub fn learn_filter(&mut self, device: Option<String>) {
        let Some(l) = &mut self.learn else { return };
        if l.filter == device {
            return;
        }
        l.filter = device.clone();
        self.remote_send(RemoteMsg::LearnFilter { device });
    }

    pub fn unbind_param(&mut self, param: Param) {
        if let Some(r) = &mut self.remote {
            r.mappings.unbind_param(&param);
        }
        self.remote_send(RemoteMsg::Unbind {
            param: Some(param),
            source: None,
        });
    }

    pub fn unbind_source(&mut self, source: Source) {
        if let Some(r) = &mut self.remote {
            r.mappings.unbind_source(&source);
        }
        self.remote_send(RemoteMsg::Unbind {
            param: None,
            source: Some(source),
        });
    }

    pub fn set_map_opts(&mut self, source: Source, opts: MapOpts) {
        if let Some(r) = &mut self.remote {
            r.mappings.set_opts(&source, opts);
        }
        self.remote_send(RemoteMsg::SetOpts { source, opts });
    }

    /// Enable / disable a MIDI input (`input`) or output by name.
    pub fn set_midi_port(&mut self, input: bool, name: String, enabled: bool) {
        if let Some(r) = &mut self.remote {
            let ports = if input {
                &mut r.midi_inputs
            } else {
                &mut r.midi_outputs
            };
            if let Some(p) = ports.iter_mut().find(|p| p.name == name) {
                p.enabled = enabled;
            }
        }
        let v = Some((name, enabled));
        self.remote_send(if input {
            RemoteMsg::SetDevices {
                input: v,
                output: None,
            }
        } else {
            RemoteMsg::SetDevices {
                input: None,
                output: v,
            }
        });
    }

    /// The daemon went away: the remote's presence is unknown until it
    /// announces itself again.
    pub(crate) fn remote_lost(&mut self) {
        if self.remote.as_ref().is_some_and(|r| !r.demo) {
            self.remote = None;
            self.learn = None;
        }
    }

    /// A relayed `remote` message. Unparsable ones are ignored.
    pub fn apply_remote(&mut self, v: serde_json::Value) {
        let Ok(m) = serde_json::from_value::<RemoteMsg>(v) else {
            return;
        };
        let now = self.now();
        match m {
            RemoteMsg::Announce {
                listen,
                midi_inputs,
                midi_outputs,
                osc_targets,
                mappings,
                learning,
            } => {
                let old = self.remote.take().unwrap_or_default();
                self.remote = Some(RemoteView {
                    listen,
                    midi_inputs,
                    midi_outputs,
                    osc_targets,
                    mappings: Mappings(mappings),
                    learning,
                    demo: false,
                    ..old
                });
                match (&mut self.learn, learning) {
                    (None, true) => {
                        // Reconnected into a running session.
                        self.learn = Some(LearnUi {
                            confirmed: true,
                            ..Default::default()
                        });
                    }
                    (Some(l), true) => l.confirmed = true,
                    (Some(l), false) if l.confirmed => {
                        // Ended elsewhere (another UI, or the remote restarted).
                        self.learn = None;
                        self.toast(ToastKind::Info, "Learn mode ended by openorion-remote");
                    }
                    _ => {}
                }
            }
            RemoteMsg::Learned { param, source } => {
                self.stamp(&source, now);
                if let Some(r) = &mut self.remote {
                    r.mappings.bind(source.clone(), param);
                }
                self.toast(
                    ToastKind::Info,
                    format!("Bound {source} → {}", param_label(&param)),
                );
            }
            RemoteMsg::Activity { source } => self.stamp(&source, now),
            // UI → remote messages from another UI: nothing to do.
            _ => {}
        }
    }

    fn stamp(&mut self, s: &Source, now: f64) {
        let Some(r) = &mut self.remote else { return };
        r.device_seen.insert(device_key(s), now);
        r.source_seen.insert(s.to_string(), now);
        if let Source::Osc { from: Some(h), .. } = s {
            match r.osc_senders.iter_mut().find(|(x, _)| x == h) {
                Some(e) => e.1 = now,
                None => r.osc_senders.push((h.clone(), now)),
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Tagging controls

/// Tags a control with the `Param` it drives. Outside learn mode this is a
/// passthrough. In learn mode the control is outlined (bright when
/// selected), shows its mapped sources as badges, selects on click or
/// drag, and offers "Clear mapping" on right-click. Params that are not
/// valid (e.g. an unnamed raw AFX field) stay untagged.
pub trait Learnable {
    fn learnable(self, ui: &Ui, st: &mut AppState, param: Param) -> Response;
}

impl Learnable for Response {
    fn learnable(self, ui: &Ui, st: &mut AppState, param: Param) -> Response {
        let Some(learn) = &st.learn else { return self };
        if !param.is_valid() {
            return self;
        }
        let selected = learn.selected == Some(param);
        let badges = st
            .remote
            .as_ref()
            .map(|r| r.badges(&param))
            .unwrap_or_default();

        if ui.is_rect_visible(self.rect) {
            let p = ui.painter();
            let rect = self.rect.expand(1.5);
            if selected {
                let t = ui.input(|i| i.time);
                let a = 0.65 + 0.35 * (t * 5.0).sin() as f32;
                p.rect_filled(rect, 4, theme::ACCENT.gamma_multiply(0.18 * a));
                p.rect_stroke(
                    rect,
                    4,
                    Stroke::new(2.5, theme::ACCENT.gamma_multiply(a)),
                    StrokeKind::Outside,
                );
            } else {
                let col = if self.hovered() {
                    theme::ACCENT
                } else {
                    theme::ACCENT_DIM
                };
                p.rect_stroke(rect, 4, Stroke::new(1.0, col), StrokeKind::Outside);
            }
            if !badges.is_empty() {
                let text = badges.join(" ");
                let galley = p.layout_no_wrap(text, FontId::monospace(9.0), theme::BG);
                let size = galley.size() + Vec2::new(6.0, 2.0);
                let anchor = rect.right_top() + Vec2::new(2.0, -3.0);
                let r = Align2::RIGHT_TOP.anchor_size(anchor, size);
                p.rect_filled(r, 3, if selected { theme::ACCENT } else { theme::SOLO });
                p.galley(r.min + Vec2::new(3.0, 1.0), galley, theme::BG);
            }
        }

        let pressed = self.contains_pointer() && ui.input(|i| i.pointer.primary_pressed());
        if self.clicked() || self.drag_started() || pressed {
            st.learn_select(Some(param));
        }
        let mapped = !badges.is_empty();
        self.context_menu(|ui| {
            ui.label(crate::widgets::dim(param.to_string()));
            if ui
                .add_enabled(mapped, egui::Button::new("Clear mapping"))
                .clicked()
            {
                st.unbind_param(param);
                ui.close();
            }
        });
        let hint = if mapped {
            "learn: click to select, right-click to clear"
        } else {
            "learn: click, then move a MIDI/OSC control"
        };
        self.on_hover_text(format!("{}\n{hint}", param_label(&param)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bridge::Incoming;
    use crate::demo::demo_state;
    use openorion_control::{ChField, MidiCtl};
    use openorion_ipc::Op;
    use openorion_proto::{Atten, MixerId};
    use serde_json::json;

    fn p(ch: u8) -> Param {
        Param::Mixer {
            mixer: MixerId::new(3).unwrap(),
            ch,
            field: ChField::Level,
        }
    }

    fn cc(n: u8) -> Source {
        Source::Midi {
            device: "nanoKONTROL2".into(),
            ch: Some(0),
            ctl: MidiCtl::Cc(n),
        }
    }

    fn remote_msgs(st: &mut AppState) -> Vec<RemoteMsg> {
        st.sendq
            .drain(..)
            .filter_map(|o| match o {
                Outgoing::Remote(v) => serde_json::from_value(v).ok(),
                _ => None,
            })
            .collect()
    }

    fn announce(st: &mut AppState, learning: bool, mappings: serde_json::Value) {
        st.apply_incoming(Incoming::Remote(json!({
            "t": "announce",
            "listen": "127.0.0.1:9000",
            "midi_inputs": [{"name": "nanoKONTROL2", "enabled": true}],
            "mappings": mappings,
            "learning": learning,
        })));
    }

    #[test]
    fn announce_is_parsed_and_unparsable_ignored() {
        let mut st = AppState::new(demo_state(), "test");
        assert!(!st.can_learn());
        st.apply_incoming(Incoming::Remote(json!({"t": "nonsense"})));
        st.apply_incoming(Incoming::Remote(json!(42)));
        assert!(st.remote.is_none());
        announce(
            &mut st,
            false,
            json!([{"source": {"kind": "midi", "device": "nanoKONTROL2", "ch": 1, "ctl": {"cc": 7}},
                    "param": "mixer/4/ch/1/level"}]),
        );
        let r = st.remote.as_ref().unwrap();
        assert_eq!(r.listen.as_deref(), Some("127.0.0.1:9000"));
        assert_eq!(r.midi_inputs.len(), 1);
        assert_eq!(r.mappings.len(), 1);
        assert_eq!(r.badges(&p(1)), vec!["CC7".to_owned()]);
        assert!(st.can_learn());
        assert!(st.learn.is_none());
    }

    #[test]
    fn learn_mode_drops_ops() {
        let mut st = AppState::new(demo_state(), "test");
        announce(&mut st, false, json!([]));
        let before = st.device.clone();
        st.learn_start();
        assert_eq!(remote_msgs(&mut st), vec![RemoteMsg::LearnStart]);
        // A fader drag and a mute click while learning.
        st.submit(Op::Mixer {
            mixer: MixerId::new(3).unwrap(),
            ch: 1,
            level: Some(Atten::new(10).unwrap()),
            pan: None,
            mute: None,
            solo: None,
            send: None,
        });
        st.submit(Op::Mute {
            id: openorion_proto::VolumeId::Main,
            mute: true,
        });
        st.tick(1.0);
        assert!(st.sendq.is_empty(), "nothing queued");
        assert!(st.outbox.is_empty(), "nothing logged");
        assert!(st.pending.is_empty());
        // Widgets render from `device`, so the control snaps back.
        assert_eq!(st.device, before);
        st.learn_stop(true);
        assert_eq!(
            remote_msgs(&mut st),
            vec![RemoteMsg::LearnStop { save: true }]
        );
        st.submit(Op::Mute {
            id: openorion_proto::VolumeId::Main,
            mute: true,
        });
        assert_eq!(st.outbox.len(), 1, "writes work again");
    }

    #[test]
    fn learn_needs_an_announce() {
        let mut st = AppState::new(demo_state(), "test");
        st.learn_start();
        assert!(st.learn.is_none());
        assert!(st.sendq.is_empty());
    }

    #[test]
    fn select_emits_learn_select() {
        let mut st = AppState::new(demo_state(), "test");
        announce(&mut st, false, json!([]));
        st.learn_start();
        st.learn_select(Some(p(1)));
        st.learn_select(Some(p(1))); // unchanged: not resent
        st.learn_filter(Some("nanoKONTROL2".into()));
        st.learn_select(None);
        assert_eq!(
            remote_msgs(&mut st),
            vec![
                RemoteMsg::LearnStart,
                RemoteMsg::LearnSelect { param: Some(p(1)) },
                RemoteMsg::LearnFilter {
                    device: Some("nanoKONTROL2".into())
                },
                RemoteMsg::LearnSelect { param: None },
            ]
        );
        st.learn_stop(false);
        assert_eq!(
            remote_msgs(&mut st),
            vec![RemoteMsg::LearnStop { save: false }]
        );
        // Not learning: selecting does nothing.
        st.learn_select(Some(p(2)));
        assert!(st.sendq.is_empty());
    }

    #[test]
    fn learned_then_announce() {
        let mut st = AppState::new(demo_state(), "test");
        announce(&mut st, false, json!([]));
        st.learn_start();
        st.learn_select(Some(p(1)));
        let learned = RemoteMsg::Learned {
            param: p(1),
            source: cc(7),
        };
        st.apply_incoming(Incoming::Remote(serde_json::to_value(&learned).unwrap()));
        let r = st.remote.as_ref().unwrap();
        assert_eq!(r.mappings.len(), 1, "optimistic bind");
        assert!(r.device_active("nanoKONTROL2", st.now()));
        // Rebinding the same source moves it.
        let learned = RemoteMsg::Learned {
            param: p(2),
            source: cc(7),
        };
        st.apply_incoming(Incoming::Remote(serde_json::to_value(&learned).unwrap()));
        assert_eq!(st.remote.as_ref().unwrap().mappings.0[0].param, p(2));
        // The announce that follows is the truth.
        let m = vec![
            openorion_control::Mapping {
                source: cc(7),
                param: p(2),
                opts: MapOpts::default(),
            },
            openorion_control::Mapping {
                source: cc(8),
                param: p(3),
                opts: MapOpts::default(),
            },
        ];
        announce(&mut st, true, serde_json::to_value(&m).unwrap());
        let r = st.remote.as_ref().unwrap();
        assert_eq!(r.mappings.0, m);
        assert!(st.learn.as_ref().unwrap().confirmed);
        assert!(st.learning(), "still learning");
    }

    #[test]
    fn learning_follows_the_remote() {
        let mut st = AppState::new(demo_state(), "test");
        // Reconnect into a running session: learn mode comes on.
        announce(&mut st, true, json!([]));
        assert!(st.learning());
        // The remote ended it.
        announce(&mut st, false, json!([]));
        assert!(!st.learning());
        // Unconfirmed: a stale learning=false does not end our session.
        st.learn_start();
        announce(&mut st, false, json!([]));
        assert!(st.learning());
    }

    #[test]
    fn query_on_connect_and_forget_on_down() {
        let mut st = AppState::live("sock");
        st.apply_incoming(Incoming::Connected(openorion_ipc::client::DaemonInfo {
            version: 1,
            daemon: "t".into(),
            sim: true,
        }));
        assert_eq!(remote_msgs(&mut st), vec![RemoteMsg::Query]);
        announce(&mut st, true, json!([]));
        assert!(st.learning());
        st.apply_incoming(Incoming::Down("gone".into()));
        assert!(st.remote.is_none());
        assert!(!st.learning());
    }

    #[test]
    fn labels_and_badges() {
        assert_eq!(param_label(&p(1)), "Mixer 4 · ch 1 · level");
        assert_eq!(
            param_label(&Param::parse("afx/equalizer/2/band/3/gain").unwrap()),
            "afx · equalizer · 2 · band 3 · gain"
        );
        let mut r = RemoteView::default();
        r.mappings.bind(cc(7), p(1));
        r.mappings.bind(
            Source::Midi {
                device: "Launch Control XL".into(),
                ch: None,
                ctl: MidiCtl::Note(36),
            },
            p(1),
        );
        assert_eq!(r.badges(&p(1)), vec!["nanoKONT CC7", "Launch N36"]);
    }
}
