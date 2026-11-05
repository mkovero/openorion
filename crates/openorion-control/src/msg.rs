//! `RemoteMsg`: what the UI and openorion-remote say to each other. The
//! daemon relays these without reading them, so the UI keeps its single
//! connection. Tagged by `"t"`, like the IPC messages.

use serde::{Deserialize, Serialize};

use crate::config::OscTarget;
use crate::mapping::{MapOpts, Mapping, Source};
use crate::param::Param;

/// A MIDI port the remote sees, and whether it is in use.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PortInfo {
    /// ALSA client (device) name.
    pub name: String,
    pub enabled: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "t", rename_all = "snake_case")]
pub enum RemoteMsg {
    /// Remote → UI: everything the Remote tab shows. Sent on connect, on
    /// `Query`, and after every change.
    Announce {
        /// OSC listen address; None = OSC off.
        #[serde(default)]
        listen: Option<String>,
        #[serde(default)]
        midi_inputs: Vec<PortInfo>,
        #[serde(default)]
        midi_outputs: Vec<PortInfo>,
        #[serde(default)]
        osc_targets: Vec<OscTarget>,
        #[serde(default)]
        mappings: Vec<Mapping>,
        #[serde(default)]
        learning: bool,
    },
    /// UI → remote: please `Announce`.
    Query,
    /// UI → remote: enter learn mode (mappings are edited in memory).
    LearnStart,
    /// UI → remote: the control that the next moved source binds to;
    /// None clears the selection.
    LearnSelect { param: Option<Param> },
    /// UI → remote: leave learn mode, writing the config (`save`) or
    /// reverting to it.
    LearnStop { save: bool },
    /// Remote → UI: `source` is now bound to `param`.
    Learned { param: Param, source: Source },
    /// UI → remote: drop every mapping of `param`, and/or the mapping of
    /// `source`.
    Unbind {
        #[serde(default)]
        param: Option<Param>,
        #[serde(default)]
        source: Option<Source>,
    },
    /// UI → remote: new options for the mapping of `source`.
    SetOpts { source: Source, opts: MapOpts },
    /// UI → remote: enable or disable a MIDI input / output by name.
    SetDevices {
        #[serde(default)]
        input: Option<(String, bool)>,
        #[serde(default)]
        output: Option<(String, bool)>,
    },
    /// Remote → UI: `source` sent something (rate-limited; the activity
    /// blink).
    Activity { source: Source },
    /// UI → remote: learn only from this MIDI device; None = any.
    LearnFilter {
        #[serde(default)]
        device: Option<String>,
    },
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mapping::MidiCtl;
    use openorion_proto::VolumeId;

    #[test]
    fn wire_shape() {
        let m = RemoteMsg::LearnSelect {
            param: Some(Param::Volume(VolumeId::Main)),
        };
        let s = serde_json::to_string(&m).unwrap();
        assert_eq!(s, r#"{"t":"learn_select","param":"volume/main"}"#);
        assert_eq!(serde_json::from_str::<RemoteMsg>(&s).unwrap(), m);
        assert_eq!(
            serde_json::from_str::<RemoteMsg>(r#"{"t":"query"}"#).unwrap(),
            RemoteMsg::Query
        );
        assert_eq!(
            serde_json::from_str::<RemoteMsg>(r#"{"t":"announce"}"#).unwrap(),
            RemoteMsg::Announce {
                listen: None,
                midi_inputs: vec![],
                midi_outputs: vec![],
                osc_targets: vec![],
                mappings: vec![],
                learning: false
            }
        );
        let all = [
            RemoteMsg::LearnStart,
            RemoteMsg::LearnStop { save: true },
            RemoteMsg::Learned {
                param: Param::Mute(VolumeId::Line),
                source: Source::Midi {
                    device: "pad".into(),
                    ch: Some(9),
                    ctl: MidiCtl::Note(36),
                },
            },
            RemoteMsg::Unbind {
                param: None,
                source: Some(Source::Osc {
                    from: None,
                    addr: "/a".into(),
                    arg: 1,
                }),
            },
            RemoteMsg::SetOpts {
                source: Source::Osc {
                    from: Some("tablet".into()),
                    addr: "/a".into(),
                    arg: 0,
                },
                opts: MapOpts::default(),
            },
            RemoteMsg::SetDevices {
                input: Some(("nano".into(), true)),
                output: None,
            },
            RemoteMsg::Activity {
                source: Source::Midi {
                    device: "*".into(),
                    ch: None,
                    ctl: MidiCtl::PitchBend,
                },
            },
            RemoteMsg::LearnFilter { device: None },
        ];
        for m in all {
            let s = serde_json::to_string(&m).unwrap();
            assert_eq!(serde_json::from_str::<RemoteMsg>(&s).unwrap(), m, "{s}");
        }
    }
}
