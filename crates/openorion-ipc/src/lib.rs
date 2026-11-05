//! IPC between openoriond and its clients (DESIGN.md sec. 7).
//!
//! Wire: `$XDG_RUNTIME_DIR/openorion.sock`, one JSON object per line,
//! tagged by `"t"`. These types are the WP0 contract; `client.rs` is WP7.

use std::path::PathBuf;

use openorion_proto::*;
use serde::{Deserialize, Serialize};

pub mod client;
mod diff;

pub use diff::{apply_diff, set_pointer};

/// Protocol version sent in `hello`. 2 added `remote`. The daemon answers
/// `hello` with the lower of its own and the client's version, and does not
/// relay `remote` messages to clients that said hello with v1.
pub const IPC_VERSION: u32 = 2;

/// First protocol version with `ClientMsg::Remote` / `DaemonMsg::Remote`.
pub const REMOTE_SINCE: u32 = 2;

/// Largest `remote` message (serialized `msg`, bytes) the daemon relays.
pub const REMOTE_MAX_BYTES: usize = 64 * 1024;

/// `$XDG_RUNTIME_DIR/openorion.sock` (falls back to `/tmp/openorion-$UID.sock`).
pub fn socket_path() -> PathBuf {
    runtime_dir().join("openorion.sock")
}

/// `$XDG_RUNTIME_DIR/openorion.lock`, held by the one live Session.
pub fn lock_path() -> PathBuf {
    runtime_dir().join("openorion.lock")
}

fn runtime_dir() -> PathBuf {
    match std::env::var_os("XDG_RUNTIME_DIR") {
        Some(d) if !d.is_empty() => PathBuf::from(d),
        _ => std::env::temp_dir(),
    }
}

// ---------------------------------------------------------------------------
// Operations: semantic, never raw bytes. The core builds the full set
// message from its cache (DESIGN.md 6.4).

/// A semantic operation. Optional fields mean "keep the current value".
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Op {
    Power {
        on: bool,
    },
    SampleRate {
        rate: SampleRate,
    },
    ClockSource {
        source: ClockSource,
    },
    Volume {
        id: VolumeId,
        atten: Atten,
    },
    Mute {
        id: VolumeId,
        mute: bool,
    },
    PresetSave {
        slot: PresetSlot,
    },
    PresetRecall {
        slot: PresetSlot,
    },
    Oscillator {
        osc: Osc,
    },
    Brightness {
        value: Brightness,
    },
    /// `ch` None = every channel. Monitor ignores `ch` (status-backed).
    Trim {
        id: TrimId,
        ch: Option<u8>,
        level: TrimLevel,
    },
    TrimControl {
        id: TrimId,
        control: TrimControl,
    },
    SpdifSrc {
        on: bool,
    },
    Smux {
        id: SmuxId,
        on: bool,
    },
    MeterSource {
        bank: MeterBank,
        src: u8,
    },
    /// ch 0 = master, 1..=32.
    Mixer {
        mixer: MixerId,
        ch: u8,
        level: Option<Atten>,
        pan: Option<Pan>,
        mute: Option<bool>,
        solo: Option<bool>,
        send: Option<Atten>,
    },
    /// Route `sources` to consecutive outputs starting at 0-based `ch`.
    Route {
        bank: OutputBank,
        ch: u8,
        sources: Vec<Source>,
    },
    StereoLink {
        target: LinkTarget,
        linked: bool,
    },
    AfxEqBand {
        inst: u8,
        band: u8,
        freq: Option<u16>,
        q: Option<u16>,
        gain: Option<i16>,
        ftype: Option<EqFilter>,
    },
    AfxCompressor {
        inst: u8,
        params: CompressorParams,
    },
    /// Raw model fields in get order (`names::afx_fields`); None = keep.
    AfxRaw {
        ty: AfxType,
        inst: u8,
        fields: Vec<Option<i32>>,
    },
    /// Replace a chain's slots (packed from slot 0).
    AfxChain {
        chain: u8,
        slots: Vec<AfxSlot>,
    },
    AfxBypass {
        ty: AfxType,
        inst: u8,
        enabled: bool,
    },
    /// Reverb of the slot assigned to `params.mixer`.
    Reverb {
        params: Reverb,
    },
    /// Re-read everything from the device.
    Resync,
    /// Check that the control channel answers (two gets in a row), then
    /// resync. Reads only, so it never disturbs audio. Also the soft
    /// recovery from `Hung`: if the device answers again, the session
    /// returns to `Ok`; if not, it stays `Hung`.
    Recheck,
    /// USB reset of the Orion, then a resync. Refused while its USB audio
    /// streams unless `force` (the reset cuts the stream).
    Reset {
        #[serde(default)]
        force: bool,
    },
}

impl Op {
    /// UX risk (DESIGN.md 4.3). Clients confirm `Disruptive`.
    pub fn risk(&self) -> Risk {
        match self {
            Op::Power { .. }
            | Op::SampleRate { .. }
            | Op::ClockSource { .. }
            | Op::PresetSave { .. }
            | Op::PresetRecall { .. }
            | Op::Reset { .. } => Risk::Disruptive,
            Op::Route { bank, .. } if bank.is_physical() => Risk::Disruptive,
            Op::Volume { .. }
            | Op::Mute { .. }
            | Op::Oscillator { .. }
            | Op::Trim { .. }
            | Op::TrimControl { .. }
            | Op::Route { .. } => Risk::Audible,
            _ => Risk::Silent,
        }
    }
}

/// For an op on one side of a stereo link, the matching op for the other
/// side; None when there is no linked partner. The core sends ops as
/// given, so clients that want panel-like linking send both.
///
/// - Mixer channel pair: level, mute, solo and send follow; pan does not
///   (a linked pair is usually panned apart). Not the master.
/// - AFX chain pair: an instance's parameters and bypass go to the
///   instance of the same model in the partner chain (the one in the same
///   slot, else the first of that model there).
pub fn linked_partner(op: &Op, state: &DeviceState) -> Option<Op> {
    match op {
        Op::Mixer { .. } => mixer_partner(op, state),
        _ => afx_partner(op, &state.afx),
    }
}

fn afx_partner(op: &Op, afx: &AfxState) -> Option<Op> {
    let (ty, inst) = match op {
        Op::AfxEqBand { inst, .. } => (AfxType::Equalizer, *inst),
        Op::AfxCompressor { inst, .. } => (AfxType::Compressor, *inst),
        Op::AfxRaw { ty, inst, .. } | Op::AfxBypass { ty, inst, .. } => (*ty, *inst),
        _ => return None,
    };
    let me = AfxSlot { ty, inst };
    let (chain, slot) = afx.order.iter().enumerate().find_map(|(c, slots)| {
        let s = slots.iter().position(|x| *x == Some(me))?;
        Some((c, s))
    })?;
    if !afx.links.get(chain / 2).copied().unwrap_or(false) {
        return None;
    }
    let other = afx.order.get(chain ^ 1)?;
    let partner = match other[slot] {
        Some(s) if s.ty == ty => s,
        _ => other.iter().flatten().find(|s| s.ty == ty).copied()?,
    };
    if partner.inst == inst {
        return None;
    }
    let mut p = op.clone();
    match &mut p {
        Op::AfxEqBand { inst, .. }
        | Op::AfxCompressor { inst, .. }
        | Op::AfxRaw { inst, .. }
        | Op::AfxBypass { inst, .. } => *inst = partner.inst,
        _ => unreachable!(),
    }
    Some(p)
}

fn mixer_partner(op: &Op, state: &DeviceState) -> Option<Op> {
    let Op::Mixer {
        mixer,
        ch,
        level,
        pan: _,
        mute,
        solo,
        send,
    } = op
    else {
        return None;
    };
    if *ch == 0 || *ch as usize > MIXER_CHANNELS {
        return None;
    }
    let pair = (*ch - 1) / 2;
    if !state.mixer_links[mixer.get() as usize][pair as usize] {
        return None;
    }
    if level.is_none() && mute.is_none() && solo.is_none() && send.is_none() {
        return None;
    }
    let partner = if *ch % 2 == 1 { *ch + 1 } else { *ch - 1 };
    Some(Op::Mixer {
        mixer: *mixer,
        ch: partner,
        level: *level,
        pan: None,
        mute: *mute,
        solo: *solo,
        send: *send,
    })
}

// ---------------------------------------------------------------------------
// Messages

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "t", rename_all = "snake_case")]
pub enum ClientMsg {
    Hello {
        v: u32,
    },
    GetState,
    /// Receive state events; `meters` adds the 30 Hz meter stream.
    Subscribe {
        meters: bool,
    },
    Op {
        id: u64,
        op: Op,
    },
    Snapshot {
        id: u64,
    },
    Restore {
        id: u64,
        snapshot: Box<Snapshot>,
        dry_run: bool,
    },
    /// Opaque client-to-client message (e.g. openorion-remote <-> UI). The
    /// daemon does not interpret `msg`; it relays it as `DaemonMsg::Remote`
    /// to every other subscribed client. Answered only on error (a
    /// `result` with id 0, e.g. when `msg` exceeds `REMOTE_MAX_BYTES`).
    Remote {
        msg: serde_json::Value,
    },
}

/// Connection health (DESIGN.md 6.3).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Health {
    /// The control channel answered recently: settings are accepted.
    Ok,
    /// Reading the device (first sync, resync, recheck).
    Syncing,
    /// Cyclic reports stopped. They may come back by themselves; settings
    /// are refused meanwhile.
    Stalled,
    /// Requests unanswered while cyclic flows: power-cycle the Orion.
    Hung,
    /// No cyclic reports even after a reset.
    NoCyclic,
    /// No device found or it went away.
    Disconnected,
}

/// What the user can do about a health state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Advice {
    None,
    /// Transient: wait a moment.
    Wait,
    /// Wait; if it persists, a USB reset (`openorion reset`, cuts USB
    /// audio) usually brings cyclic reports back.
    WaitThenReset,
    /// Only a power cycle clears it. A USB reset does not cure a hung
    /// control channel and was seen to cause one.
    PowerCycle,
    /// Check the cable, permissions and that the device is on.
    Reconnect,
}

impl Health {
    /// Whether the session accepts settings now. While syncing they are
    /// queued behind the reads.
    pub fn writable(self) -> bool {
        matches!(self, Health::Ok | Health::Syncing)
    }

    pub fn advice(self) -> Advice {
        match self {
            Health::Ok => Advice::None,
            Health::Syncing => Advice::Wait,
            Health::Stalled => Advice::WaitThenReset,
            Health::Hung | Health::NoCyclic => Advice::PowerCycle,
            Health::Disconnected => Advice::Reconnect,
        }
    }
}

impl Advice {
    /// One line for people.
    pub fn text(self) -> &'static str {
        match self {
            Advice::None => "",
            Advice::Wait => "wait a moment",
            Advice::WaitThenReset => {
                "wait; if it persists, run `openorion reset` (interrupts USB audio) \
                 or power-cycle the Orion"
            }
            Advice::PowerCycle => "power-cycle the Orion (a USB reset does not clear this)",
            Advice::Reconnect => "check the cable, power and device permissions",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "t", rename_all = "snake_case")]
pub enum DaemonMsg {
    Hello {
        v: u32,
        daemon: String,
        sim: bool,
    },
    State {
        state: Box<DeviceState>,
    },
    Status {
        status: Box<Status>,
    },
    /// A changed part of the state. `path` is a JSON pointer into
    /// `DeviceState` (e.g. `/mixers/3/channels/0`), `value` the new value.
    Diff {
        path: String,
        value: serde_json::Value,
    },
    Meters {
        meters: Box<Meters>,
        afx: Box<AfxMeters>,
    },
    Result {
        id: u64,
        ok: bool,
        error: Option<String>,
    },
    Health {
        health: Health,
        detail: Option<String>,
    },
    /// Read-back disagreed with what we set; the cache took `device`.
    Mismatch {
        path: String,
        ours: serde_json::Value,
        device: serde_json::Value,
    },
    Snapshot {
        id: u64,
        snapshot: Box<Snapshot>,
    },
    /// Restore result: the ops applied (or that would be, on dry run).
    RestorePlan {
        id: u64,
        ops: Vec<Op>,
        applied: bool,
    },
    /// Something the daemon did by itself (e.g. restored AFX settings
    /// after a power cycle), for clients to show.
    Notice {
        text: String,
    },
    /// A `ClientMsg::Remote` from another subscribed client, unchanged.
    Remote {
        msg: serde_json::Value,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn linked_partner_mirrors_all_but_pan() {
        let mut state = openorion_proto::decode::empty_state();
        let m = MixerId::new(0).unwrap();
        state.mixer_links[0][0] = true; // ch 1-2
        let op = Op::Mixer {
            mixer: m,
            ch: 2,
            level: Some(Atten::new(10).unwrap()),
            pan: Some(Pan::new(40).unwrap()),
            mute: None,
            solo: None,
            send: None,
        };
        let p = linked_partner(&op, &state).unwrap();
        assert_eq!(
            p,
            Op::Mixer {
                mixer: m,
                ch: 1,
                level: Some(Atten::new(10).unwrap()),
                pan: None,
                mute: None,
                solo: None,
                send: None,
            }
        );
        // Unlinked pair, master, pan-only: nothing.
        let mut op3 = op.clone();
        if let Op::Mixer { ch, .. } = &mut op3 {
            *ch = 3;
        }
        assert!(linked_partner(&op3, &state).is_none());
        let pan_only = Op::Mixer {
            mixer: m,
            ch: 1,
            level: None,
            pan: Some(Pan::CENTER),
            mute: None,
            solo: None,
            send: None,
        };
        assert!(linked_partner(&pan_only, &state).is_none());
    }

    #[test]
    fn linked_partner_mirrors_afx_chains() {
        let mut state = openorion_proto::decode::empty_state();
        let eq = |inst| {
            Some(AfxSlot {
                ty: AfxType::Equalizer,
                inst,
            })
        };
        let comp = |inst| {
            Some(AfxSlot {
                ty: AfxType::Compressor,
                inst,
            })
        };
        // Chains 3-4 (0-based 2, 3), linked; chain 5 (4) alone.
        state.afx.order[2][0] = eq(2);
        state.afx.order[2][1] = comp(2);
        state.afx.order[3][0] = eq(3);
        state.afx.order[3][1] = comp(3);
        state.afx.order[4][0] = eq(4);
        state.afx.links[1] = true;
        let band = |inst| Op::AfxEqBand {
            inst,
            band: 1,
            freq: None,
            q: None,
            gain: Some(-250),
            ftype: None,
        };
        assert_eq!(linked_partner(&band(2), &state), Some(band(3)));
        assert_eq!(linked_partner(&band(3), &state), Some(band(2)));
        let bypass = Op::AfxBypass {
            ty: AfxType::Compressor,
            inst: 3,
            enabled: false,
        };
        assert_eq!(
            linked_partner(&bypass, &state),
            Some(Op::AfxBypass {
                ty: AfxType::Compressor,
                inst: 2,
                enabled: false,
            })
        );
        // Unlinked chain, instance in no chain, link off: nothing.
        assert!(linked_partner(&band(4), &state).is_none());
        assert!(linked_partner(&band(9), &state).is_none());
        state.afx.links[1] = false;
        assert!(linked_partner(&band(2), &state).is_none());
        // Partner model in another slot still matches.
        state.afx.links[1] = true;
        state.afx.order[3][0] = comp(3);
        state.afx.order[3][1] = eq(3);
        assert_eq!(linked_partner(&band(2), &state), Some(band(3)));
    }

    #[test]
    fn op_json_shape() {
        let msg = ClientMsg::Op {
            id: 7,
            op: Op::Volume {
                id: VolumeId::Line,
                atten: Atten::new(2).unwrap(),
            },
        };
        let s = serde_json::to_string(&msg).unwrap();
        assert_eq!(
            s,
            r#"{"t":"op","id":7,"op":{"op":"volume","id":"Line","atten":2}}"#
        );
        assert_eq!(serde_json::from_str::<ClientMsg>(&s).unwrap(), msg);
    }

    #[test]
    fn remote_json_shape() {
        let msg = ClientMsg::Remote {
            msg: serde_json::json!({"x": 1}),
        };
        let s = serde_json::to_string(&msg).unwrap();
        assert_eq!(s, r#"{"t":"remote","msg":{"x":1}}"#);
        assert_eq!(serde_json::from_str::<ClientMsg>(&s).unwrap(), msg);
        let d = DaemonMsg::Remote {
            msg: serde_json::json!({"x": 1}),
        };
        let s = serde_json::to_string(&d).unwrap();
        assert_eq!(s, r#"{"t":"remote","msg":{"x":1}}"#);
        assert_eq!(serde_json::from_str::<DaemonMsg>(&s).unwrap(), d);
    }
}
