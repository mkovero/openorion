//! LED level meters on a controller's button grid (e.g. the APC40 mkII
//! clip-launch pads): one column per bound fader, lit bottom-up, the top
//! row held after a clip. Extra columns follow one fader each (the APC's
//! scene-launch column follows the master fader).
//!
//! A column's level is the loudest point of its channel's path: the
//! mixer meter (when the device meters that mixer) and, for a channel fed
//! by an AFX chain, every slot of that chain. A limiter at the end of a
//! chain keeps the mixer meter below full scale, so the chain's own slots
//! are what show a clip at its input. A master column reads whichever
//! meter bank (A/B) shows the mixer's L/R bus, directly or through an
//! output routed from it; with neither it stays dark.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use openorion_control::{ChField, Mappings, MasterField, MeterGrid, MidiCtl, Param, Source};
use openorion_proto::{
    AFX_CHAINS, AfxMeters, DeviceState, MIXER_CHANNELS, MeterBank, Meters, OutputBank,
    Source as Route, SourceGroup,
};

/// AFX meters at or below this many dB under full scale read as empty.
const AFX_FLOOR: u8 = 60;

pub struct GridState {
    pub grid: MeterGrid,
    /// Last velocity sent per note (missing = unknown, sent next time).
    sent: HashMap<u8, u8>,
    /// Loudest raw value per column since the last update.
    acc: Vec<u8>,
    clip_until: Vec<Option<Instant>>,
    due: Option<Instant>,
}

impl GridState {
    pub fn new(grid: MeterGrid) -> GridState {
        let n = grid.notes.len() + grid.extra.len();
        GridState {
            grid,
            sent: HashMap::new(),
            acc: vec![u8::MAX; n],
            clip_until: vec![None; n],
            due: None,
        }
    }

    /// The controller may show something else now: send every LED again.
    pub fn forget(&mut self) {
        self.sent.clear();
    }

    /// `note` was changed by the controller itself (a press).
    pub fn forget_note(&mut self, ch: u8, note: u8) {
        if ch == self.grid.ch {
            self.sent.remove(&note);
        }
    }

    /// Feed one meter frame; returns the note messages to send.
    pub fn frame(
        &mut self,
        device: &str,
        mappings: &Mappings,
        state: &DeviceState,
        m: &Meters,
        afx: &AfxMeters,
        now: Instant,
    ) -> Vec<Vec<u8>> {
        let cols = columns(&self.grid, device, mappings);
        for (c, (_, col)) in cols.iter().enumerate() {
            let raw = match *col {
                Some(Target::Ch(mixer, ch)) => level(state, m, afx, mixer, ch),
                Some(Target::Master(mixer)) => master_level(state, m, mixer),
                None => continue,
            };
            self.acc[c] = self.acc[c].min(raw);
        }
        if self.due.is_some_and(|d| now < d) {
            return Vec::new();
        }
        let period = Duration::from_secs_f64(1.0 / self.grid.hz.max(1) as f64);
        self.due = Some(now + period);
        let hold = Duration::from_millis(self.grid.clip_hold_ms as u64);
        let mut out = Vec::new();
        for (c, (notes, col)) in cols.iter().enumerate() {
            let raw = std::mem::replace(&mut self.acc[c], u8::MAX);
            if col.is_some() && raw == 0 {
                self.clip_until[c] = Some(now + hold);
            }
            let clipped = col.is_some() && self.clip_until[c].is_some_and(|t| now < t);
            let db = if col.is_some() && raw < AFX_FLOOR {
                Some(-(raw as i16))
            } else {
                None
            };
            for (r, &note) in notes.iter().enumerate() {
                if bound(mappings, device, self.grid.ch, note) {
                    continue;
                }
                let thr = self.grid.rows_db[r] as i16;
                let lit = if thr >= 0 {
                    clipped
                } else {
                    db.is_some_and(|d| d >= thr)
                };
                let vel = if lit { self.grid.colors[r] } else { 0 };
                if self.sent.get(&note) != Some(&vel) {
                    self.sent.insert(note, vel);
                    out.push(vec![0x90 | self.grid.ch, note, vel]);
                }
            }
        }
        out
    }
}

/// Whether a mapping owns `note` (its LED shows the parameter instead).
fn bound(mappings: &Mappings, device: &str, ch: u8, note: u8) -> bool {
    mappings
        .lookup(&Source::Midi {
            device: device.to_owned(),
            ch: Some(ch),
            ctl: MidiCtl::Note(note),
        })
        .is_some()
}

/// What a column meters: mixer index and channel 1..=32, or a mixer's
/// master.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Target {
    Ch(u8, u8),
    Master(u8),
}

/// Columns (notes bottom-up, target) from the fader bindings: the grid's
/// own, then `extra`.
pub fn columns<'a>(
    grid: &'a MeterGrid,
    device: &str,
    mappings: &Mappings,
) -> Vec<(&'a [u8], Option<Target>)> {
    let fader = |ch: u8, cc: u8| {
        mappings.iter().find_map(|m| {
            let Source::Midi {
                device: d,
                ch: Some(c),
                ctl,
            } = &m.source
            else {
                return None;
            };
            let hit = matches!(*ctl, MidiCtl::Cc(n) | MidiCtl::Cc14(n) if n == cc);
            if d != device || *c != ch || !hit {
                return None;
            }
            match m.param {
                Param::Mixer {
                    mixer,
                    ch,
                    field: ChField::Level,
                } => Some(Target::Ch(mixer.get(), ch)),
                Param::Master {
                    mixer,
                    field: MasterField::Level,
                } => Some(Target::Master(mixer.get())),
                _ => None,
            }
        })
    };
    let own = grid
        .notes
        .iter()
        .enumerate()
        .map(|(c, notes)| (notes.as_slice(), fader(c as u8, grid.fader_cc)));
    let extra = grid
        .extra
        .iter()
        .map(|x| (x.notes.as_slice(), fader(x.ch, x.cc)));
    own.chain(extra).collect()
}

/// Loudest raw meter value of mixer `mixer`'s L/R bus, from a meter bank
/// (A/B) showing it: the bus itself (meter source 10 + mixer, L/R at 0/1)
/// or an output bank (source 14 + bank) with outputs routed from it.
/// `u8::MAX` = not metered.
pub fn master_level(state: &DeviceState, m: &Meters, mixer: u8) -> u8 {
    let bus = SourceGroup::ALL[SourceGroup::Mix1 as usize + mixer as usize];
    let mut raw = u8::MAX;
    for (bank, vals) in [(MeterBank::A, &m.bank_a), (MeterBank::B, &m.bank_b)] {
        let src = state.status.meter_sources[bank as usize];
        if src == bus as u8 {
            raw = vals[..2].iter().fold(raw, |r, &v| r.min(v));
        } else if let Some(ob) = src
            .checked_sub(14)
            .and_then(|i| OutputBank::ALL.get(i as usize))
            && let Some(r) = state.routing.iter().find(|r| r.bank == *ob)
        {
            for (i, s) in r.sources.iter().enumerate() {
                if matches!(s, Route::Input { group, .. } if *group == bus)
                    && let Some(&v) = vals.get(i)
                {
                    raw = raw.min(v);
                }
            }
        }
    }
    raw
}

/// Loudest raw meter value (dB below full scale) of mixer channel `ch`
/// and its linked partner; `u8::MAX` = nothing metered.
pub fn level(state: &DeviceState, m: &Meters, afx: &AfxMeters, mixer: u8, ch: u8) -> u8 {
    let mut chans = vec![ch];
    if (1..=MIXER_CHANNELS as u8).contains(&ch)
        && state.mixer_links[mixer as usize][(ch as usize - 1) / 2]
    {
        chans.push(if ch % 2 == 1 { ch + 1 } else { ch - 1 });
    }
    let metered = state.status.meter_sources[MeterBank::Mixer as usize] == mixer;
    let bank = OutputBank::ALL[OutputBank::MixCh1 as usize + mixer as usize];
    let routes = state.routing.iter().find(|r| r.bank == bank);
    let mut raw = u8::MAX;
    for c in chans {
        let i = c as usize - 1;
        if metered && let Some(&v) = m.mixer.get(i) {
            raw = raw.min(v);
        }
        if let Some(Route::Input {
            group: SourceGroup::AfxOut,
            chan,
        }) = routes.and_then(|r| r.sources.get(i)).copied()
            && (chan as usize) < AFX_CHAINS
        {
            for &v in &afx.peak[chan as usize] {
                raw = raw.min(v);
            }
        }
    }
    raw
}
