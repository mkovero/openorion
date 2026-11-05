//! Keep a local copy of the parts of `DeviceState` openorion-jack uses
//! (routing and status), fed by daemon messages.

use openorion_ipc::DaemonMsg;
use openorion_proto::{DeviceState, RoutingBank, Status};

/// What a message changed.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Changed {
    pub routing: bool,
    pub status: bool,
}

#[derive(Debug, Default)]
pub struct Tracker {
    pub state: Option<DeviceState>,
}

impl Tracker {
    /// Apply one message. Diffs before the first full state are ignored
    /// (the daemon sends `state` once synced).
    pub fn apply(&mut self, msg: &DaemonMsg) -> Changed {
        match msg {
            DaemonMsg::State { state } => {
                let old = self.state.replace((**state).clone());
                match old {
                    None => Changed {
                        routing: true,
                        status: true,
                    },
                    Some(o) => Changed {
                        routing: o.routing != state.routing,
                        status: o.status != state.status,
                    },
                }
            }
            DaemonMsg::Status { status } => self.set_status((**status).clone()),
            DaemonMsg::Diff { path, value } => self.apply_diff(path, value),
            _ => Changed::default(),
        }
    }

    fn set_status(&mut self, s: Status) -> Changed {
        let Some(st) = self.state.as_mut() else {
            return Changed::default();
        };
        let status = st.status != s;
        st.status = s;
        Changed {
            routing: false,
            status,
        }
    }

    fn apply_diff(&mut self, path: &str, value: &serde_json::Value) -> Changed {
        if path == "/status" {
            if let Ok(s) = serde_json::from_value::<Status>(value.clone()) {
                return self.set_status(s);
            }
            return Changed::default();
        }
        let Some(rest) = path.strip_prefix("/routing/") else {
            return Changed::default();
        };
        let Some(st) = self.state.as_mut() else {
            return Changed::default();
        };
        let Ok(rb) = serde_json::from_value::<RoutingBank>(value.clone()) else {
            return Changed::default();
        };
        let slot = rest
            .parse::<usize>()
            .ok()
            .filter(|&i| st.routing.get(i).is_some_and(|r| r.bank == rb.bank))
            .or_else(|| st.routing.iter().position(|r| r.bank == rb.bank));
        match slot {
            Some(i) if st.routing[i] != rb => {
                st.routing[i] = rb;
                Changed {
                    routing: true,
                    status: false,
                }
            }
            Some(_) => Changed::default(),
            None => {
                st.routing.push(rb);
                Changed {
                    routing: true,
                    status: false,
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use openorion_proto::decode::empty_state;
    use openorion_proto::{OutputBank, SampleRate, Source, SourceGroup};

    #[test]
    fn state_then_diffs() {
        let mut t = Tracker::default();
        let diff = |rb: &RoutingBank| DaemonMsg::Diff {
            path: format!("/routing/{}", rb.bank as usize),
            value: serde_json::to_value(rb).unwrap(),
        };
        let mut rb = RoutingBank::all_muted(OutputBank::UsbRec1);
        rb.sources[0] = Source::Input {
            group: SourceGroup::AdatIn,
            chan: 0,
        };
        // Before state: ignored.
        assert_eq!(t.apply(&diff(&rb)), Changed::default());
        let st = empty_state();
        let c = t.apply(&DaemonMsg::State {
            state: Box::new(st.clone()),
        });
        assert!(c.routing && c.status);
        assert!(t.apply(&diff(&rb)).routing);
        assert!(!t.apply(&diff(&rb)).routing, "same value: no change");
        let got = t.state.as_ref().unwrap();
        assert_eq!(
            got.routing[OutputBank::UsbRec1 as usize].sources[0],
            rb.sources[0]
        );

        let mut s = st.status.clone();
        s.sample_rate = Some(SampleRate::R96k);
        let c = t.apply(&DaemonMsg::Diff {
            path: "/status".into(),
            value: serde_json::to_value(&s).unwrap(),
        });
        assert!(c.status && !c.routing);
        assert_eq!(
            t.state.as_ref().unwrap().status.sample_rate,
            Some(SampleRate::R96k)
        );
        let c = t.apply(&DaemonMsg::Status {
            status: Box::new(s),
        });
        assert!(!c.status);
    }
}
