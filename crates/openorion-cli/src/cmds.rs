//! Command implementations.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use openorion_ipc::{Health, Op, linked_partner};
use openorion_proto::names::*;
use openorion_proto::*;
use serde_json::{Value, json};

use crate::backend::Daemon;
use crate::backend::{Backend, Direct};
use crate::parse::{self as p, fmt_bool, fmt_level, fmt_pan, fmt_volume, table};
use crate::{
    AfxCmd, ChainAction, Cmd, Fail, MixerCmd, Out, PresetCmd, ReverbCmd, RouteCmd, SmuxWhich,
    SnapCmd, TrimTarget, VolTarget,
};

pub struct Global {
    pub json: bool,
    pub socket: Option<PathBuf>,
    pub direct: bool,
    pub sim: Option<PathBuf>,
    pub lock: Option<PathBuf>,
    pub yes: bool,
}

pub struct Ctx {
    g: Global,
    be: Option<Box<dyn Backend>>,
    want_meters: bool,
    /// Accept a daemon whose device is unhealthy (`health`).
    tolerant: bool,
}

type R = Result<Option<Out>, Fail>;

fn out(text: String, json: Value) -> R {
    Ok(Some(Out {
        text,
        json,
        code: 0,
    }))
}

/// Output for a command that ran but did not reach its goal (exit 1).
fn out_failed(text: String, json: Value) -> R {
    Ok(Some(Out {
        text,
        json,
        code: 1,
    }))
}

fn ops_json(ops: &[Op]) -> Value {
    json!({"ok": true, "ops": ops})
}

fn op_text(op: &Op) -> String {
    serde_json::to_string(op).unwrap_or_default()
}

impl Ctx {
    pub fn new(g: Global) -> Ctx {
        Ctx {
            g,
            be: None,
            want_meters: false,
            tolerant: false,
        }
    }

    fn be(&mut self) -> Result<&mut Box<dyn Backend>, Fail> {
        if self.be.is_none() {
            let b: Box<dyn Backend> = if self.g.direct {
                Box::new(Direct::open(self.g.sim.as_deref(), self.g.lock.clone())?)
            } else {
                let path = self
                    .g
                    .socket
                    .clone()
                    .unwrap_or_else(openorion_ipc::socket_path);
                Box::new(Daemon::connect(&path, self.want_meters, self.tolerant)?)
            };
            self.be = Some(b);
        }
        Ok(self.be.as_mut().unwrap())
    }

    fn state(&mut self) -> Result<DeviceState, Fail> {
        Ok(self.be()?.state()?)
    }

    /// Refuse a disruptive op unless confirmed (--yes or a y/N prompt).
    fn confirm(&self, ops: &[Op]) -> Result<(), Fail> {
        let risky: Vec<&Op> = ops
            .iter()
            .filter(|o| o.risk() == Risk::Disruptive)
            .collect();
        if risky.is_empty() || self.g.yes {
            return Ok(());
        }
        let list: Vec<String> = risky.iter().map(|o| op_text(o)).collect();
        if crate::interactive() && !self.g.json {
            eprintln!("disruptive operation(s):");
            for l in &list {
                eprintln!("  {l}");
            }
            if crate::ask("continue?", false) {
                return Ok(());
            }
            return Err(Fail::Refused("aborted".into()));
        }
        Err(Fail::Refused(format!(
            "refusing disruptive operation without --yes: {}",
            list.join(", ")
        )))
    }

    /// Confirm and send. If an op fails after others went out, the error
    /// says which were applied.
    fn send(&mut self, ops: &[Op]) -> Result<(), Fail> {
        self.confirm(ops)?;
        let be = self.be()?;
        for (i, op) in ops.iter().enumerate() {
            if let Err(e) = be.op(op.clone()) {
                if i == 0 {
                    return Err(e.into());
                }
                let done: Vec<String> = ops[..i].iter().map(op_text).collect();
                return Err(Fail::Error(format!(
                    "{e} (after {i} of {} ops were applied: {})",
                    ops.len(),
                    done.join(", ")
                )));
            }
        }
        Ok(())
    }

    pub fn finish(&mut self) -> Vec<String> {
        match self.be.as_mut() {
            Some(b) => b.finish(),
            None => Vec::new(),
        }
    }
}

pub fn run(ctx: &mut Ctx, cmd: Cmd) -> R {
    match cmd {
        Cmd::Status => status(ctx),
        Cmd::Meters { bank, count } => meters(ctx, &bank, count),
        Cmd::Route { sub } => match sub {
            RouteCmd::Show { bank } => route_show(ctx, bank.as_deref()),
            RouteCmd::Set {
                output,
                source,
                count,
            } => route_set(ctx, &output, &source, count),
        },
        Cmd::Mixer { sub } => match sub {
            MixerCmd::Show { mixer } => mixer_show(ctx, &mixer),
            MixerCmd::Set {
                mixer,
                ch,
                level,
                pan,
                mute,
                solo,
                send,
                link,
                no_link_follow,
            } => mixer_set(
                ctx,
                &mixer,
                &ch,
                MixArgs {
                    level,
                    pan,
                    mute,
                    solo,
                    send,
                    link,
                    follow: !no_link_follow,
                },
            ),
        },
        Cmd::Volume { target, db } => volume(ctx, target, db.as_deref()),
        Cmd::Mute { target, state } => mute(ctx, target, state.as_deref()),
        Cmd::Clock {
            source,
            lock_timeout,
            no_revert,
        } => clock(ctx, source.as_deref(), lock_timeout, no_revert),
        Cmd::Rate { rate: r } => rate(ctx, r.as_deref()),
        Cmd::Trim { target, dbu, ch } => trim(ctx, target, dbu.as_deref(), ch.as_deref()),
        Cmd::Osc {
            freq,
            freq_l,
            freq_r,
            level,
            mute_l,
            mute_r,
        } => osc(ctx, [freq, freq_l, freq_r, level, mute_l, mute_r]),
        Cmd::Reverb { sub } => match sub {
            ReverbCmd::Show => reverb_show(ctx),
            ReverbCmd::Set {
                mixer,
                room,
                color,
                predelay,
                density,
                early,
                late_delay,
                richness,
                time,
                level,
                on,
            } => reverb_set(
                ctx,
                &mixer,
                [
                    room, color, predelay, density, early, late_delay, richness, time, level,
                ],
                on.as_deref(),
            ),
        },
        Cmd::Afx { sub } => afx(ctx, sub),
        Cmd::Preset { sub } => {
            let (slot, save) = match sub {
                PresetCmd::Save { slot } => (slot, true),
                PresetCmd::Recall { slot } => (slot, false),
            };
            let slot = PresetSlot::new(slot).map_err(|e| e.to_string())?;
            let op = if save {
                Op::PresetSave { slot }
            } else {
                Op::PresetRecall { slot }
            };
            simple(ctx, op, |_| {
                format!(
                    "preset {} {}\n",
                    if save { "saved to" } else { "recalled from" },
                    slot.get()
                )
            })
        }
        Cmd::Snapshot { sub } => snapshot(ctx, sub),
        Cmd::Power { state } => match state {
            None => {
                let st = ctx.state()?;
                let on = st.status.power_on;
                out(
                    format!("power  {}\n", if on { "on" } else { "standby" }),
                    json!({"power_on": on}),
                )
            }
            Some(s) => {
                let on = match s.to_ascii_lowercase().as_str() {
                    "on" => true,
                    "standby" | "off" => false,
                    _ => return Err(format!("power {s:?}: on or standby").into()),
                };
                simple(ctx, Op::Power { on }, |_| {
                    format!("power {}\n", if on { "on" } else { "standby" })
                })
            }
        },
        Cmd::Resync => simple(ctx, Op::Resync, |_| "resynced\n".into()),
        Cmd::Health { recheck } => health(ctx, recheck),
        Cmd::Reset { force } => simple(ctx, Op::Reset { force }, |_| {
            "reset done; resynced\n".into()
        }),
        Cmd::Brightness { value } => match value {
            None => {
                let b = ctx.state()?.status.brightness;
                out(format!("brightness  {b}\n"), json!({"brightness": b}))
            }
            Some(v) => {
                let value = Brightness::new(v).map_err(|e| e.to_string())?;
                simple(ctx, Op::Brightness { value }, |_| {
                    format!("brightness {v}\n")
                })
            }
        },
        Cmd::Smux { which, state } => match (which, state) {
            (Some(w), Some(s)) => {
                let on = p::on_off(&s)?;
                let id = match w {
                    SmuxWhich::In => SmuxId::MadiIn,
                    SmuxWhich::Out => SmuxId::MadiOut,
                };
                simple(ctx, Op::Smux { id, on }, |_| {
                    format!(
                        "smux {} {}\n",
                        if w == SmuxWhich::In { "in" } else { "out" },
                        fmt_bool(on)
                    )
                })
            }
            (None, None) | (Some(_), None) => {
                let st = ctx.state()?.status;
                out(
                    table(&[
                        vec!["smux in".into(), fmt_bool(st.smux_in).into()],
                        vec!["smux out".into(), fmt_bool(st.smux_out).into()],
                    ]),
                    json!({"smux_in": st.smux_in, "smux_out": st.smux_out}),
                )
            }
            (None, Some(_)) => Err("usage: openorion smux in|out on|off".into()),
        },
        Cmd::Src { state } => match state {
            None => {
                let on = ctx.state()?.status.spdif_src;
                out(
                    format!("s/pdif src  {}\n", fmt_bool(on)),
                    json!({"spdif_src": on}),
                )
            }
            Some(s) => {
                let on = p::on_off(&s)?;
                simple(ctx, Op::SpdifSrc { on }, |_| {
                    format!("s/pdif src {}\n", fmt_bool(on))
                })
            }
        },
        Cmd::Raw { i_know, hex } => raw(ctx, i_know, &hex.join(" ")),
    }
}

/// Send one op and report.
fn simple(ctx: &mut Ctx, op: Op, text: impl FnOnce(&Op) -> String) -> R {
    ctx.send(std::slice::from_ref(&op))?;
    let t = text(&op);
    out(t, ops_json(&[op]))
}

// ---------------------------------------------------------------------------
// status, meters

fn status(ctx: &mut Ctx) -> R {
    let be = ctx.be()?;
    let st = be.state()?;
    let (health, detail) = be.health();
    let via = be.describe();
    let s = &st.status;
    let i = &st.info;
    let clock = s
        .clock_source
        .map(clock_source_name)
        .unwrap_or("unknown")
        .to_string();
    let rate = s.sample_rate.map(sample_rate_name).unwrap_or("unknown");
    let mut ext = String::new();
    if s.external_clock_hz != 0 {
        ext = format!("  (external {} Hz)", s.external_clock_hz);
    }
    let rows = vec![
        vec![
            "device".into(),
            format!(
                "{} {}  fw {}.{}",
                i.passport.name, i.passport.serial, i.firmware.major, i.firmware.minor
            ),
        ],
        vec![
            "health".into(),
            format!(
                "{health:?}{}  via {via}",
                detail
                    .as_deref()
                    .map(|d| format!(" ({d})"))
                    .unwrap_or_default()
            )
            .to_lowercase(),
        ],
        vec![
            "power".into(),
            if s.power_on { "on" } else { "standby" }.into(),
        ],
        vec![
            "clock".into(),
            format!(
                "{clock}  {}{ext}",
                // The lock bit reads 0 on the internal clock (dump1).
                if s.clock_source == Some(ClockSource::Internal) {
                    "internal"
                } else if s.clock_locked() {
                    "locked"
                } else {
                    "NOT LOCKED"
                }
            ),
        ],
        vec!["rate".into(), rate.into()],
        vec![
            "preset".into(),
            if s.current_preset == 0 {
                "-".into()
            } else {
                s.current_preset.to_string()
            },
        ],
        vec![
            "main".into(),
            format!(
                "{}{}",
                fmt_volume(s.main.atten),
                if s.main.mute { "  muted" } else { "" }
            ),
        ],
        vec![
            "line".into(),
            format!(
                "{}{}",
                fmt_volume(s.line.atten),
                if s.line.mute { "  muted" } else { "" }
            ),
        ],
        vec![
            "monitor trim".into(),
            format!("{} dBu", 24i32 - s.monitor_trim as i32),
        ],
        vec!["brightness".into(), s.brightness.to_string()],
    ];
    out(
        table(&rows),
        json!({
            "health": health, "detail": detail, "via": via,
            "info": st.info, "status": st.status,
        }),
    )
}

const BARS: [char; 9] = [' ', '▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];

fn meter_line(label: &str, src: &str, v: &[u8]) -> String {
    let bars: String = v
        .iter()
        .map(|&r| {
            let lvl = 60u32.saturating_sub(r.min(60) as u32);
            BARS[(lvl * 8).div_ceil(60) as usize]
        })
        .collect();
    let peak = v.iter().copied().min().unwrap_or(METER_FLOOR);
    let pk = if peak >= 60 {
        "  -inf".to_string()
    } else {
        format!("{:>6}", format!("-{peak}"))
    };
    let mut s = format!("{label:<6} {src:<14} |{bars}| {pk} dB\n");
    // Numeric rows, 16 channels each.
    for (half, chunk) in v.chunks(16).enumerate() {
        let nums: Vec<String> = chunk
            .iter()
            .map(|&r| {
                if r >= 60 {
                    "   .".into()
                } else {
                    format!("{:>4}", -(r as i32))
                }
            })
            .collect();
        s.push_str(&format!(
            "       {:>2}-{:<2} {}\n",
            half * 16 + 1,
            half * 16 + chunk.len(),
            nums.join("")
        ));
    }
    s
}

fn meters(ctx: &mut Ctx, bank: &str, count: Option<u32>) -> R {
    let which = bank.to_ascii_lowercase();
    if !matches!(which.as_str(), "a" | "b" | "mixer" | "all") {
        return Err(format!("--bank {bank:?}: a, b, mixer or all").into());
    }
    ctx.want_meters = true;
    let json = ctx.g.json;
    let be = ctx.be()?;
    let st = be.state()?;
    let srcs = st.status.meter_sources;
    let name = |i: usize| meter_source_name(srcs[i]).unwrap_or("?").to_string();
    let tty = std::io::IsTerminal::is_terminal(&std::io::stdout());
    let mut printed = 0u32;
    let mut last = Instant::now() - Duration::from_secs(1);
    let mut stdout = std::io::stdout();
    loop {
        if count.is_some_and(|c| printed >= c) {
            break;
        }
        let Some((m, _afx)) = be.meters(Duration::from_secs(3))? else {
            return Err("no meter data from the device within 3 s".into());
        };
        if last.elapsed() < Duration::from_millis(100) {
            continue;
        }
        last = Instant::now();
        printed += 1;
        let r = if json {
            let mut v = json!({});
            if which == "a" || which == "all" {
                v["bank_a"] = json!(m.bank_a);
            }
            if which == "b" || which == "all" {
                v["bank_b"] = json!(m.bank_b);
            }
            if which == "mixer" || which == "all" {
                v["mixer"] = json!(m.mixer);
                v["reverb_in"] = json!(m.reverb_in);
                v["reverb_out"] = json!(m.reverb_out);
            }
            writeln!(stdout, "{v}")
        } else {
            let mut s = String::new();
            if tty {
                s.push_str("\x1b[H\x1b[2J");
            }
            if which == "a" || which == "all" {
                s.push_str(&meter_line("A", &name(0), &m.bank_a));
            }
            if which == "b" || which == "all" {
                s.push_str(&meter_line("B", &name(1), &m.bank_b));
            }
            if which == "mixer" || which == "all" {
                s.push_str(&meter_line(
                    "MIXER",
                    &format!("MIXER {}", srcs[2] as u32 + 1),
                    &m.mixer,
                ));
            }
            if !tty {
                s.push('\n');
            }
            write!(stdout, "{s}")
        };
        if r.is_err() {
            break; // stdout closed
        }
        let _ = stdout.flush();
    }
    Ok(None)
}

// ---------------------------------------------------------------------------
// routing

fn bank_sources(st: &DeviceState, b: OutputBank) -> Vec<Source> {
    st.routing
        .iter()
        .find(|r| r.bank == b)
        .map(|r| r.sources.clone())
        .unwrap_or_default()
}

fn follows(prev: Source, next: Source) -> bool {
    match (prev, next) {
        (Source::Mute, Source::Mute) => true,
        (
            Source::Input {
                group: g1,
                chan: c1,
            },
            Source::Input {
                group: g2,
                chan: c2,
            },
        ) => g1 == g2 && c2 == c1 + 1,
        _ => false,
    }
}

/// Runs of consecutive outputs fed by consecutive sources, in `range`.
fn route_runs(b: OutputBank, src: &[Source], range: std::ops::Range<usize>) -> Vec<Vec<String>> {
    let mut rows = Vec::new();
    let mut i = range.start;
    while i < range.end.min(src.len()) {
        let mut j = i;
        while j + 1 < range.end.min(src.len()) && follows(src[j], src[j + 1]) {
            j += 1;
        }
        let n = if src[i] == Source::Mute { 1 } else { j - i + 1 };
        rows.push(vec![
            p::out_range(b, i, j),
            "<-".into(),
            p::src_range(src[i], n),
        ]);
        i = j + 1;
    }
    rows
}

fn route_show(ctx: &mut Ctx, bank: Option<&str>) -> R {
    let banks: Vec<OutputBank> = match bank {
        Some(b) => vec![p::bank(b)?],
        None => OutputBank::ALL.to_vec(),
    };
    let st = ctx.state()?;
    let mut rows = Vec::new();
    let mut j = serde_json::Map::new();
    for b in banks {
        let src = bank_sources(&st, b);
        rows.extend(route_runs(b, &src, 0..b.size()));
        j.insert(output_bank_name(b).to_string(), json!(src));
    }
    out(table(&rows), Value::Object(j))
}

fn route_set(ctx: &mut Ctx, output: &str, source: &str, count: usize) -> R {
    let (bank, ch) = p::output(output)?;
    let src = p::source(source)?;
    if count == 0 {
        return Err("--count must be at least 1".into());
    }
    if ch as usize + count > bank.size() {
        return Err(format!(
            "{} has {} channels; {} from channel {} does not fit",
            output_bank_name(bank),
            bank.size(),
            count,
            ch + 1
        )
        .into());
    }
    let sources: Vec<Source> = match src {
        Source::Mute => vec![Source::Mute; count],
        Source::Input { group, chan } => {
            if chan as usize + count > group.size() as usize {
                return Err(format!(
                    "{} has {} channels; {} from channel {} does not fit",
                    source_group_name(group),
                    group.size(),
                    count,
                    chan + 1
                )
                .into());
            }
            (0..count as u8)
                .map(|k| Source::Input {
                    group,
                    chan: chan + k,
                })
                .collect()
        }
    };
    let op = Op::Route { bank, ch, sources };
    ctx.send(std::slice::from_ref(&op))?;
    let st = ctx.state()?;
    let src = bank_sources(&st, bank);
    let rows = route_runs(bank, &src, ch as usize..ch as usize + count);
    out(table(&rows), ops_json(&[op]))
}

// ---------------------------------------------------------------------------
// mixer

fn mixer_rows(st: &DeviceState, m: MixerId, chans: &[u8]) -> (Vec<Vec<String>>, Vec<Value>) {
    let mx = &st.mixers[m.get() as usize];
    let bank = OutputBank::ALL[OutputBank::MixCh1 as usize + m.get() as usize];
    let src = bank_sources(st, bank);
    let mut rows = vec![
        [
            "CH", "INPUT", "LEVEL", "PAN", "MUTE", "SOLO", "SEND", "LINK",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect::<Vec<_>>(),
    ];
    let mut js = Vec::new();
    for &ch in chans {
        let (c, input, link) = if ch == 0 {
            (mx.master, String::new(), false)
        } else {
            let i = ch as usize - 1;
            (
                mx.channels[i],
                src.get(i).map(|&s| p::src_name(s)).unwrap_or_default(),
                st.mixer_links[m.get() as usize][i / 2],
            )
        };
        rows.push(vec![
            if ch == 0 { "M".into() } else { ch.to_string() },
            input,
            fmt_level(c.level),
            fmt_pan(c.pan),
            if c.mute { "M" } else { "." }.into(),
            if c.solo { "S" } else { "." }.into(),
            fmt_level(c.send),
            if link {
                let a = (ch - 1) / 2 * 2 + 1;
                format!("{a}-{}", a + 1)
            } else {
                String::new()
            },
        ]);
        js.push(json!({"ch": ch, "channel": c, "linked": link}));
    }
    (rows, js)
}

fn mixer_show(ctx: &mut Ctx, mixer: &str) -> R {
    let m = p::mixer(mixer)?;
    let st = ctx.state()?;
    let chans: Vec<u8> = (1..=MIXER_CHANNELS as u8).chain([0]).collect();
    let (rows, _) = mixer_rows(&st, m, &chans);
    out(
        format!("MIXER {}\n{}", m.get() + 1, table(&rows)),
        json!({
            "mixer": m.get(),
            "mixer_state": st.mixers[m.get() as usize],
            "links": st.mixer_links[m.get() as usize],
        }),
    )
}

struct MixArgs {
    level: Option<String>,
    pan: Option<String>,
    mute: Option<String>,
    solo: Option<String>,
    send: Option<String>,
    link: Option<String>,
    follow: bool,
}

fn mixer_set(ctx: &mut Ctx, mixer: &str, ch: &str, a: MixArgs) -> R {
    let m = p::mixer(mixer)?;
    let ch = p::mixer_ch(ch)?;
    let level = a.level.as_deref().map(p::atten_db).transpose()?;
    let pan = a.pan.as_deref().map(p::pan).transpose()?;
    let mute = a.mute.as_deref().map(p::on_off).transpose()?;
    let solo = a.solo.as_deref().map(p::on_off).transpose()?;
    let send = a.send.as_deref().map(p::atten_db).transpose()?;
    let link = a.link.as_deref().map(p::on_off).transpose()?;
    if [
        level.is_some(),
        pan.is_some(),
        mute.is_some(),
        solo.is_some(),
        send.is_some(),
        link.is_some(),
    ]
    .iter()
    .all(|x| !x)
    {
        return Err("nothing to set (--level, --pan, --mute, --solo, --send, --link)".into());
    }
    let mut st = ctx.state()?;
    let mut ops = Vec::new();
    if let Some(linked) = link {
        if ch == 0 {
            return Err("the master has no stereo link".into());
        }
        let pair = (ch - 1) / 2;
        ops.push(Op::StereoLink {
            target: LinkTarget::Mixer { mixer: m, pair },
            linked,
        });
        st.mixer_links[m.get() as usize][pair as usize] = linked;
    }
    if level.is_some() || pan.is_some() || mute.is_some() || solo.is_some() || send.is_some() {
        let op = Op::Mixer {
            mixer: m,
            ch,
            level,
            pan,
            mute,
            solo,
            send,
        };
        let partner = if a.follow {
            linked_partner(&op, &st)
        } else {
            None
        };
        ops.push(op);
        ops.extend(partner);
    }
    ctx.send(&ops)?;
    let st = ctx.state()?;
    let mut chans = vec![ch];
    for op in &ops {
        if let Op::Mixer { ch: c, .. } = op
            && !chans.contains(c)
        {
            chans.push(*c);
        }
    }
    let (rows, js) = mixer_rows(&st, m, &chans);
    out(
        format!("MIXER {}\n{}", m.get() + 1, table(&rows)),
        json!({"ok": true, "ops": ops, "channels": js}),
    )
}

// ---------------------------------------------------------------------------
// volume, mute, clock, rate

fn vol_id(t: VolTarget) -> VolumeId {
    match t {
        VolTarget::Main => VolumeId::Main,
        VolTarget::Line => VolumeId::Line,
    }
}

fn volumes(st: &Status) -> R {
    out(
        table(&[
            vec![
                "main".into(),
                fmt_volume(st.main.atten),
                if st.main.mute { "muted" } else { "" }.into(),
            ],
            vec![
                "line".into(),
                fmt_volume(st.line.atten),
                if st.line.mute { "muted" } else { "" }.into(),
            ],
        ]),
        json!({"main": st.main, "line": st.line}),
    )
}

fn volume(ctx: &mut Ctx, target: Option<VolTarget>, db: Option<&str>) -> R {
    match (target, db) {
        (Some(t), Some(db)) => {
            let atten = p::atten_db(db)?;
            simple(
                ctx,
                Op::Volume {
                    id: vol_id(t),
                    atten,
                },
                |_| format!("{t:?} volume {}\n", fmt_volume(atten)).to_lowercase(),
            )
        }
        _ => volumes(&ctx.state()?.status),
    }
}

fn mute(ctx: &mut Ctx, target: Option<VolTarget>, state: Option<&str>) -> R {
    match (target, state) {
        (Some(t), Some(s)) => {
            let mute = p::on_off(s)?;
            simple(
                ctx,
                Op::Mute {
                    id: vol_id(t),
                    mute,
                },
                |_| format!("{t:?} mute {}\n", fmt_bool(mute)).to_lowercase(),
            )
        }
        _ => volumes(&ctx.state()?.status),
    }
}

fn clock_show(s: &Status) -> R {
    let name = s.clock_source.map(clock_source_name).unwrap_or("unknown");
    let f = &s.clock;
    let seen: Vec<&str> = [
        (f.usb, "USB"),
        (f.madi, "MADI"),
        (f.adat, "ADAT"),
        (f.spdif, "S/PDIF"),
        (f.dlink, "D-Link"),
    ]
    .iter()
    .filter(|(b, _)| *b)
    .map(|(_, n)| *n)
    .collect();
    out(
        table(&[
            vec!["source".into(), name.into()],
            vec!["locked".into(), if f.locked { "yes" } else { "NO" }.into()],
            vec![
                "external".into(),
                if s.external_clock_hz == 0 {
                    "-".into()
                } else {
                    format!("{} Hz", s.external_clock_hz)
                },
            ],
            vec![
                "signals".into(),
                if seen.is_empty() {
                    "-".into()
                } else {
                    seen.join(" ")
                },
            ],
        ]),
        json!({"source": s.clock_source, "clock": s.clock, "external_clock_hz": s.external_clock_hz}),
    )
}

fn clock(ctx: &mut Ctx, source: Option<&str>, lock_timeout: f64, no_revert: bool) -> R {
    let Some(source) = source else {
        return clock_show(&ctx.state()?.status);
    };
    if !(0.0..=MAX_LOCK_TIMEOUT).contains(&lock_timeout) {
        return Err(format!("--lock-timeout must be 0..{MAX_LOCK_TIMEOUT} s").into());
    }
    let target = p::clock_source(source)?;
    let before = ctx.state()?.status.clock_source;
    let op = Op::ClockSource { source: target };
    ctx.send(std::slice::from_ref(&op))?;
    let mut ops = vec![op];
    let name = clock_source_name(target);
    if !target.is_external() {
        return out(format!("clock {name}\n"), ops_json(&ops));
    }
    let st = settle_clock(ctx, target, before, lock_timeout, no_revert, &mut ops)?;
    if st.locked {
        return out(
            format!("clock {name}  locked\n"),
            json!({"ok": true, "ops": ops, "locked": true}),
        );
    }
    out_failed(
        format!(
            "clock {name}  NOT LOCKED{}\n",
            if st.reverted { " (reverted)" } else { "" }
        ),
        json!({"ok": false, "ops": ops, "locked": false, "reverted": st.reverted}),
    )
}

const MAX_LOCK_TIMEOUT: f64 = 3600.0;
/// Lock wait after a clock change that came from `restore` or `raw`.
const DEFAULT_LOCK_TIMEOUT: f64 = 10.0;

struct ClockSettle {
    locked: bool,
    reverted: bool,
}

static INTERRUPTED: AtomicBool = AtomicBool::new(false);

extern "C" fn on_sigint(_: libc::c_int) {
    INTERRUPTED.store(true, Ordering::SeqCst);
}

/// While alive, Ctrl-C sets `INTERRUPTED` instead of ending the process,
/// so a clock change can still be reverted.
struct SigintGuard(libc::sighandler_t);

impl SigintGuard {
    fn install() -> SigintGuard {
        INTERRUPTED.store(false, Ordering::SeqCst);
        let handler = on_sigint as extern "C" fn(libc::c_int);
        // SAFETY: the handler only stores to an atomic, which is
        // async-signal-safe; the old handler is restored on drop.
        let old = unsafe { libc::signal(libc::SIGINT, handler as libc::sighandler_t) };
        SigintGuard(old)
    }
}

impl Drop for SigintGuard {
    fn drop(&mut self) {
        // SAFETY: puts back the handler that `install` replaced.
        unsafe { libc::signal(libc::SIGINT, self.0) };
    }
}

/// After switching to the external source `target`: wait for lock, and
/// without lock go back to `before` unless `no_revert`. A failed wait
/// (daemon gone, device hung) or Ctrl-C also reverts. Revert ops are
/// appended to `ops`. Errors from the wait are returned after the revert.
fn settle_clock(
    ctx: &mut Ctx,
    target: ClockSource,
    before: Option<ClockSource>,
    timeout: f64,
    no_revert: bool,
    ops: &mut Vec<Op>,
) -> Result<ClockSettle, Fail> {
    let name = clock_source_name(target);
    let deadline = Instant::now() + Duration::from_secs_f64(timeout);
    let waited = {
        let _guard = SigintGuard::install();
        loop {
            match ctx.state() {
                Err(e) => break Err(e),
                Ok(st) if st.status.clock_source == Some(target) && st.status.clock_locked() => {
                    break Ok(true);
                }
                Ok(_) => {}
            }
            if INTERRUPTED.load(Ordering::SeqCst) || Instant::now() >= deadline {
                break Ok(false);
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    };
    let interrupted = INTERRUPTED.load(Ordering::SeqCst);
    match &waited {
        Ok(true) => {
            return Ok(ClockSettle {
                locked: true,
                reverted: false,
            });
        }
        Ok(false) if interrupted => eprintln!("openorion: interrupted before {name} locked"),
        Ok(false) => eprintln!("openorion: warning: {name} did not lock within {timeout} s"),
        Err(Fail::Error(e) | Fail::Refused(e)) => {
            eprintln!("openorion: waiting for {name} to lock failed: {e}")
        }
    }
    let mut reverted = false;
    match before {
        None => {
            eprintln!("openorion: warning: the previous clock source is unknown; not reverting")
        }
        Some(prev) if prev != target && !no_revert => {
            let ask = waited.is_ok() && !interrupted && crate::interactive() && !ctx.g.json;
            let revert =
                !ask || crate::ask(&format!("revert to {}?", clock_source_name(prev)), true);
            if revert {
                let back = Op::ClockSource { source: prev };
                match ctx
                    .be()
                    .and_then(|b| b.op(back.clone()).map_err(Fail::from))
                {
                    Ok(()) => {
                        ops.push(back);
                        reverted = true;
                        eprintln!(
                            "openorion: reverted the clock to {}",
                            clock_source_name(prev)
                        );
                    }
                    Err(Fail::Error(e) | Fail::Refused(e)) => eprintln!(
                        "openorion: could not revert ({e}); set it with `openorion clock {}`",
                        clock_source_name(prev)
                    ),
                }
            }
        }
        Some(_) => {}
    }
    waited?;
    if interrupted {
        return Err("interrupted".into());
    }
    Ok(ClockSettle {
        locked: false,
        reverted,
    })
}

/// The external clock source among `ops`, if they change it.
fn external_clock_target(ops: &[Op]) -> Option<ClockSource> {
    ops.iter().rev().find_map(|o| match o {
        Op::ClockSource { source } if source.is_external() => Some(*source),
        _ => None,
    })
}

fn health(ctx: &mut Ctx, recheck: bool) -> R {
    ctx.tolerant = true;
    let recheck_err = if recheck {
        ctx.be()?.op(Op::Recheck).err()
    } else {
        None
    };
    let be = ctx.be()?;
    let (health, detail) = be.health();
    let via = be.describe();
    let advice = health.advice();
    let mut rows = vec![
        vec!["health".into(), format!("{health:?}").to_lowercase()],
        vec![
            "settings".into(),
            if health.writable() {
                "accepted"
            } else {
                "refused"
            }
            .into(),
        ],
        vec!["via".into(), via.clone()],
    ];
    if let Some(d) = &detail {
        rows.push(vec!["detail".into(), d.clone()]);
    }
    if !advice.text().is_empty() {
        rows.push(vec!["advice".into(), advice.text().into()]);
    }
    if let Some(e) = &recheck_err {
        rows.push(vec!["recheck".into(), format!("failed: {e}")]);
    } else if recheck {
        rows.push(vec!["recheck".into(), "ok".into()]);
    }
    let j = json!({
        "ok": health == Health::Ok,
        "health": health,
        "writable": health.writable(),
        "detail": detail,
        "advice": advice,
        "advice_text": advice.text(),
        "via": via,
        "recheck": recheck.then(|| recheck_err.is_none()),
    });
    if health == Health::Ok {
        out(table(&rows), j)
    } else {
        out_failed(table(&rows), j)
    }
}

fn rate(ctx: &mut Ctx, rate: Option<&str>) -> R {
    match rate {
        None => {
            let r = ctx.state()?.status.sample_rate;
            out(
                format!("rate  {}\n", r.map(sample_rate_name).unwrap_or("unknown")),
                json!({"sample_rate": r, "hz": r.map(|r| r.hz())}),
            )
        }
        Some(s) => {
            let rate = p::sample_rate(s)?;
            simple(ctx, Op::SampleRate { rate }, |_| {
                format!("rate {}\n", sample_rate_name(rate))
            })
        }
    }
}

// ---------------------------------------------------------------------------
// trims, oscillator

fn fmt_dbu(t: TrimLevel) -> String {
    let v = t.dbu();
    if v.fract() == 0.0 {
        format!("{v:.0}")
    } else {
        format!("{v:.2}")
    }
}

fn trim(ctx: &mut Ctx, target: Option<TrimTarget>, dbu: Option<&str>, ch: Option<&str>) -> R {
    let Some(dbu) = dbu else {
        let st = ctx.state()?;
        let t = &st.trims;
        let mut rows = vec![vec![
            "CH".to_string(),
            format!("LINE IN ({:?})", t.line_in.control).to_uppercase(),
            format!("LINE OUT ({:?})", t.line_out.control).to_uppercase(),
        ]];
        for i in 0..TRIM_CHANNELS {
            rows.push(vec![
                (i + 1).to_string(),
                fmt_dbu(t.line_in.levels[i]),
                fmt_dbu(t.line_out.levels[i]),
            ]);
        }
        let mon = 24 - st.status.monitor_trim as i32;
        let only = |x: TrimTarget| target.is_none_or(|t| t == x);
        let mut text = String::new();
        if only(TrimTarget::LineIn) || only(TrimTarget::LineOut) {
            text.push_str(&table(&rows));
        }
        if only(TrimTarget::Monitor) {
            text.push_str(&format!("monitor  {mon} dBu\n"));
        }
        return out(text, json!({"trims": st.trims, "monitor_dbu": mon}));
    };
    let Some(target) = target else {
        return Err("usage: openorion trim line-in|line-out|monitor DBU [--ch N]".into());
    };
    let level = p::trim_dbu(dbu)?;
    let id = match target {
        TrimTarget::LineIn => TrimId::LineIn,
        TrimTarget::LineOut => TrimId::LineOut,
        TrimTarget::Monitor => TrimId::Monitor,
    };
    let ch = match ch {
        Some(c) if target != TrimTarget::Monitor => {
            Some(p::one_based(c, "channel", TRIM_CHANNELS)?)
        }
        Some(_) => return Err("the monitor trim has no channels".into()),
        None => None,
    };
    simple(ctx, Op::Trim { id, ch, level }, |_| {
        format!(
            "trim {:?}{} {} dBu\n",
            id,
            ch.map(|c| format!(" ch {}", c + 1)).unwrap_or_default(),
            fmt_dbu(level)
        )
        .to_lowercase()
    })
}

fn osc_name(o: &Osc) -> Vec<Vec<String>> {
    let f = |x: OscFreq| match x {
        OscFreq::Hz1k => "1k",
        OscFreq::Hz440 => "440",
    };
    let l = match o.level {
        OscLevel::Db0 => "0",
        OscLevel::DbM6 => "-6",
        OscLevel::DbM12 => "-12",
        OscLevel::DbM18 => "-18",
    };
    vec![
        vec![
            "freq".into(),
            format!("L {}  R {}", f(o.freq_l), f(o.freq_r)),
        ],
        vec!["level".into(), format!("{l} dBFS")],
        vec![
            "mute".into(),
            format!("L {}  R {}", fmt_bool(o.mute_l), fmt_bool(o.mute_r)),
        ],
    ]
}

fn osc(ctx: &mut Ctx, a: [Option<String>; 6]) -> R {
    let mut o = ctx.state()?.status.oscillator;
    if a.iter().all(|x| x.is_none()) {
        return out(table(&osc_name(&o)), json!({"oscillator": o}));
    }
    let [freq, fl, fr, level, ml, mr] = a;
    if let Some(f) = freq {
        let f = p::osc_freq(&f)?;
        o.freq_l = f;
        o.freq_r = f;
    }
    if let Some(f) = fl {
        o.freq_l = p::osc_freq(&f)?;
    }
    if let Some(f) = fr {
        o.freq_r = p::osc_freq(&f)?;
    }
    if let Some(l) = level {
        o.level = p::osc_level(&l)?;
    }
    if let Some(m) = ml {
        o.mute_l = p::on_off(&m)?;
    }
    if let Some(m) = mr {
        o.mute_r = p::on_off(&m)?;
    }
    simple(ctx, Op::Oscillator { osc: o }, |_| table(&osc_name(&o)))
}

// ---------------------------------------------------------------------------
// reverb

const REVERB_FIELDS: [&str; 9] = [
    "room",
    "color",
    "predelay",
    "density",
    "early",
    "late-delay",
    "richness",
    "time",
    "level",
];

fn reverb_vals(r: &Reverb) -> [u8; 9] {
    [
        r.room_size,
        r.color,
        r.predelay,
        r.density,
        r.early_ref_gain,
        r.late_ref_delay,
        r.richness,
        r.reverb_time,
        r.reverb_level,
    ]
}

fn reverb_table(slots: &[Option<Reverb>]) -> String {
    let mut rows = vec![
        ["SLOT", "MIXER", "ON"]
            .iter()
            .map(|s| s.to_string())
            .chain(REVERB_FIELDS.iter().map(|s| s.to_uppercase()))
            .collect::<Vec<_>>(),
    ];
    for (i, r) in slots.iter().enumerate() {
        let mut row = vec![(i + 1).to_string()];
        match r {
            None => row.push("unused".into()),
            Some(r) => {
                row.push((r.mixer.get() + 1).to_string());
                row.push(fmt_bool(r.on).into());
                row.extend(reverb_vals(r).iter().map(|v| v.to_string()));
            }
        }
        rows.push(row);
    }
    table(&rows)
}

fn reverb_show(ctx: &mut Ctx) -> R {
    let st = ctx.state()?;
    out(reverb_table(&st.reverb), json!({"reverb": st.reverb}))
}

fn reverb_set(ctx: &mut Ctx, mixer: &str, v: [Option<u8>; 9], on: Option<&str>) -> R {
    let m = p::mixer(mixer)?;
    let on = on.map(p::on_off).transpose()?;
    if v.iter().all(|x| x.is_none()) && on.is_none() {
        return Err("nothing to set (--room, --color, ..., --on)".into());
    }
    let st = ctx.state()?;
    let Some(mut r) = st.reverb.iter().flatten().find(|r| r.mixer == m).copied() else {
        return Err(format!("no reverb slot is assigned to mixer {}", m.get() + 1).into());
    };
    let fields: [&mut u8; 9] = [
        &mut r.room_size,
        &mut r.color,
        &mut r.predelay,
        &mut r.density,
        &mut r.early_ref_gain,
        &mut r.late_ref_delay,
        &mut r.richness,
        &mut r.reverb_time,
        &mut r.reverb_level,
    ];
    for (f, x) in fields.into_iter().zip(v) {
        if let Some(x) = x {
            *f = x;
        }
    }
    if let Some(on) = on {
        r.on = on;
    }
    simple(ctx, Op::Reverb { params: r }, |_| reverb_table(&[Some(r)]))
}

// ---------------------------------------------------------------------------
// AFX

fn slot_name(s: AfxSlot) -> String {
    format!("{}:{}", afx_type_name(s.ty), s.inst + 1)
}

fn afx_config(st: &DeviceState, ty: AfxType, inst: u8) -> Option<&AfxConfig> {
    st.afx.configs.get(&ty).and_then(|v| v.get(inst as usize))
}

fn afx_chains(st: &DeviceState) -> String {
    let mut rows = Vec::new();
    for (c, slots) in st.afx.order.iter().enumerate() {
        let items: Vec<String> = slots
            .iter()
            .flatten()
            .map(|&s| {
                let bypassed = afx_config(st, s.ty, s.inst).is_some_and(|c| !c.enabled);
                format!(
                    "{}{}",
                    slot_name(s),
                    if bypassed { " (bypassed)" } else { "" }
                )
            })
            .collect();
        let link = if st.afx.links[c / 2] {
            format!("{}-{}", c / 2 * 2 + 1, c / 2 * 2 + 2)
        } else {
            String::new()
        };
        rows.push(vec![
            format!("AFX {}", c + 1),
            link,
            if items.is_empty() {
                "-".into()
            } else {
                items.join(" > ")
            },
        ]);
    }
    table(&rows)
}

fn afx_params_text(ty: AfxType, cfg: &AfxConfig) -> String {
    let mut s = format!(
        "enabled  {}\n",
        if cfg.enabled { "yes" } else { "no (bypassed)" }
    );
    match &cfg.params {
        AfxParams::Eq(eq) => {
            let mut rows = vec![
                ["BAND", "TYPE", "FREQ", "Q", "GAIN"]
                    .iter()
                    .map(|s| s.to_string())
                    .collect::<Vec<_>>(),
            ];
            for (i, b) in eq.bands.iter().enumerate() {
                rows.push(vec![
                    (i + 1).to_string(),
                    format!("{:?}", b.ftype),
                    format!("{} Hz", b.freq),
                    format!("{:.2}", b.q as f64 / 100.0),
                    format!("{:+.1} dB", b.gain as f64 / 100.0),
                ]);
            }
            s.push_str(&table(&rows));
        }
        AfxParams::Compressor(c) => {
            s.push_str(&table(&[
                vec!["attack".into(), format!("{} ms", c.attack as f64 / 1000.0)],
                vec![
                    "release".into(),
                    format!("{} ms", c.release as f64 / 1000.0),
                ],
                vec![
                    "taw".into(),
                    if c.taw == 65535 {
                        "peak".into()
                    } else {
                        format!("{}", c.taw as f64 / 100.0)
                    },
                ],
                vec!["ratio".into(), format!("{}:1", c.ratio as f64 / 100.0)],
                vec!["gain".into(), format!("{} dB", c.gain as f64 / 100.0)],
                vec!["threshold".into(), format!("-{} dBFS", c.threshold)],
                vec!["knee".into(), format!("{} dB", c.knee)],
                vec!["linked".into(), c.linked.to_string()],
            ]));
        }
        AfxParams::Raw(r) => {
            let names = afx_fields(ty).unwrap_or(&[]);
            let rows: Vec<Vec<String>> = r
                .fields
                .iter()
                .enumerate()
                .map(|(i, v)| {
                    let f = names.get(i);
                    vec![
                        f.map(|f| f.name.to_string()).unwrap_or(format!("#{i}")),
                        v.to_string(),
                        f.map(|f| format!("({}..{})", f.min, f.max))
                            .unwrap_or_default(),
                    ]
                })
                .collect();
            s.push_str(&table(&rows));
        }
    }
    s
}

fn need_inst(slot: &str) -> Result<(AfxType, u8), Fail> {
    match p::afx_slot(slot)? {
        (ty, Some(i)) => Ok((ty, i)),
        (_, None) => Err(format!("{slot:?}: give an instance, e.g. ClearQ:1").into()),
    }
}

fn afx(ctx: &mut Ctx, sub: AfxCmd) -> R {
    match sub {
        AfxCmd::Show { slot: None } => {
            let st = ctx.state()?;
            out(afx_chains(&st), json!({"afx": st.afx}))
        }
        AfxCmd::Show { slot: Some(slot) } => {
            let (ty, inst) = need_inst(&slot)?;
            let st = ctx.state()?;
            let cfg = afx_config(&st, ty, inst).cloned().ok_or_else(|| {
                format!(
                    "no config for {} in the cache",
                    slot_name(AfxSlot { ty, inst })
                )
            })?;
            out(
                format!(
                    "{}\n{}",
                    slot_name(AfxSlot { ty, inst }),
                    afx_params_text(ty, &cfg)
                ),
                json!({"ty": ty, "inst": inst, "config": cfg}),
            )
        }
        AfxCmd::Chain {
            chain,
            action,
            items,
        } => afx_chain(ctx, &chain, action, &items),
        AfxCmd::Eq {
            inst,
            band,
            freq,
            q,
            gain,
            ftype,
        } => {
            let inst = p::one_based(&inst, "instance", 16)?;
            let band = p::one_based(&band, "band", EQ_BANDS)?;
            let q = q.map(|q| (q * 100.0).round() as u16);
            let gain = match gain {
                Some(g) if !(-24.0..=12.0).contains(&g) => {
                    return Err(format!("gain {g} dB out of range -24..+12").into());
                }
                g => g.map(|g| (g * 100.0).round() as i16),
            };
            let ftype = ftype.as_deref().map(p::eq_filter).transpose()?;
            if freq.is_none() && q.is_none() && gain.is_none() && ftype.is_none() {
                return Err("nothing to set (--freq, --q, --gain, --type)".into());
            }
            let op = Op::AfxEqBand {
                inst,
                band,
                freq,
                q,
                gain,
                ftype,
            };
            ctx.send(std::slice::from_ref(&op))?;
            let st = ctx.state()?;
            let text = afx_config(&st, AfxType::Equalizer, inst)
                .map(|c| afx_params_text(AfxType::Equalizer, c))
                .unwrap_or_default();
            out(format!("ClearQ:{}\n{text}", inst + 1), ops_json(&[op]))
        }
        AfxCmd::Comp {
            inst,
            attack,
            release,
            ratio,
            gain,
            threshold,
            knee,
            taw,
            linked,
        } => {
            let inst = p::one_based(&inst, "instance", 16)?;
            let st = ctx.state()?;
            let mut c = match afx_config(&st, AfxType::Compressor, inst)
                .cloned()
                .or_else(|| decode::default_afx_config(AfxType::Compressor))
                .map(|c| c.params)
            {
                Some(AfxParams::Compressor(c)) => c,
                _ => return Err("no PowerFFC config in the cache".into()),
            };
            if let Some(v) = attack {
                c.attack = (v * 1000.0).round() as u32;
            }
            if let Some(v) = release {
                c.release = (v * 1000.0).round() as u32;
            }
            if let Some(v) = ratio {
                c.ratio = (v * 100.0).round() as u16;
            }
            if let Some(v) = gain {
                c.gain = (v * 100.0).round() as u16;
            }
            if let Some(v) = threshold {
                c.threshold = v.abs().round() as u8;
            }
            if let Some(v) = knee {
                c.knee = v;
            }
            if let Some(v) = taw {
                c.taw = if v.eq_ignore_ascii_case("peak") {
                    65535
                } else {
                    let x: f64 = v
                        .parse()
                        .map_err(|_| format!("--taw {v:?}: peak or a number"))?;
                    (x * 100.0).round() as u16
                };
            }
            if let Some(v) = linked {
                c.linked = v;
            }
            let op = Op::AfxCompressor { inst, params: c };
            ctx.send(std::slice::from_ref(&op))?;
            let cfg = AfxConfig {
                enabled: afx_config(&st, AfxType::Compressor, inst).is_none_or(|c| c.enabled),
                params: AfxParams::Compressor(c),
            };
            out(
                format!(
                    "PowerFFC:{}\n{}",
                    inst + 1,
                    afx_params_text(AfxType::Compressor, &cfg)
                ),
                ops_json(&[op]),
            )
        }
        AfxCmd::Bypass { slot, state } => {
            let (ty, inst) = need_inst(&slot)?;
            let bypass = p::on_off(&state)?;
            simple(
                ctx,
                Op::AfxBypass {
                    ty,
                    inst,
                    enabled: !bypass,
                },
                |_| {
                    format!(
                        "{} {}\n",
                        slot_name(AfxSlot { ty, inst }),
                        if bypass { "bypassed" } else { "active" }
                    )
                },
            )
        }
        AfxCmd::Raw { slot, assigns } => {
            let (ty, inst) = need_inst(&slot)?;
            let table_ = afx_fields(ty).ok_or_else(|| {
                format!(
                    "{} has dedicated commands (afx eq / afx comp)",
                    afx_type_name(ty)
                )
            })?;
            if assigns.is_empty() {
                let names: Vec<String> = table_
                    .iter()
                    .filter(|f| f.in_set)
                    .map(|f| format!("{} ({}..{})", f.name, f.min, f.max))
                    .collect();
                return Err(format!(
                    "give NAME=VALUE; fields of {}: {}",
                    afx_type_name(ty),
                    names.join(", ")
                )
                .into());
            }
            let mut fields = vec![None; table_.len()];
            for a in &assigns {
                let (n, v) = a
                    .split_once('=')
                    .ok_or_else(|| format!("{a:?}: expected NAME=VALUE"))?;
                let i = table_
                    .iter()
                    .position(|f| f.name.eq_ignore_ascii_case(n.trim()))
                    .ok_or_else(|| format!("{} has no field {n:?}", afx_type_name(ty)))?;
                let f = &table_[i];
                if !f.in_set {
                    return Err(format!("field {} cannot be set", f.name).into());
                }
                let v: i32 = v
                    .trim()
                    .parse()
                    .map_err(|_| format!("{a:?}: value must be an integer"))?;
                if !(f.min..=f.max).contains(&v) {
                    return Err(
                        format!("{} = {v} out of range {}..{}", f.name, f.min, f.max).into(),
                    );
                }
                fields[i] = Some(v);
            }
            simple(ctx, Op::AfxRaw { ty, inst, fields }, |_| {
                format!(
                    "{} {}\n",
                    slot_name(AfxSlot { ty, inst }),
                    assigns.join(" ")
                )
            })
        }
        AfxCmd::Link { chain, state } => {
            let c = p::one_based(&chain, "chain", AFX_CHAINS)?;
            let linked = p::on_off(&state)?;
            let pair = c / 2;
            simple(
                ctx,
                Op::StereoLink {
                    target: LinkTarget::Afx { pair },
                    linked,
                },
                |_| {
                    format!(
                        "AFX {}-{} link {}\n",
                        pair * 2 + 1,
                        pair * 2 + 2,
                        fmt_bool(linked)
                    )
                },
            )
        }
    }
}

fn afx_chain(ctx: &mut Ctx, chain: &str, action: ChainAction, items: &[String]) -> R {
    let c = p::one_based(chain, "chain", AFX_CHAINS)?;
    let st = ctx.state()?;
    let mut slots: Vec<AfxSlot> = st.afx.order[c as usize].iter().flatten().copied().collect();
    let used = |ty: AfxType, slots: &[AfxSlot]| -> Vec<u8> {
        let mut v: Vec<u8> = st
            .afx
            .order
            .iter()
            .enumerate()
            .filter(|(i, _)| *i != c as usize)
            .flat_map(|(_, s)| s.iter().flatten())
            .chain(slots.iter())
            .filter(|s| s.ty == ty)
            .map(|s| s.inst)
            .collect();
        v.sort();
        v
    };
    match action {
        ChainAction::Clear => {
            if !items.is_empty() {
                return Err("clear takes no items".into());
            }
            slots.clear();
        }
        ChainAction::Add | ChainAction::Set => {
            if items.is_empty() {
                return Err("give TYPE[:INST] items, e.g. ClearQ PowerFFC:2".into());
            }
            if action == ChainAction::Set {
                slots.clear();
            }
            for it in items {
                let (ty, inst) = p::afx_slot(it)?;
                let taken = used(ty, &slots);
                let inst = match inst {
                    Some(i) => {
                        if taken.contains(&i) {
                            return Err(format!(
                                "{} is already in use",
                                slot_name(AfxSlot { ty, inst: i })
                            )
                            .into());
                        }
                        i
                    }
                    None => {
                        let max = st
                            .afx
                            .instances
                            .get(&ty)
                            .copied()
                            .unwrap_or(afx_max_instances(ty))
                            .min(afx_max_instances(ty));
                        (0..max)
                            .find(|i| !taken.contains(i))
                            .ok_or_else(|| format!("no free {} instance", afx_type_name(ty)))?
                    }
                };
                slots.push(AfxSlot { ty, inst });
            }
            if slots.len() > AFX_SLOTS {
                return Err(format!("a chain holds at most {AFX_SLOTS} slots").into());
            }
        }
        ChainAction::Remove => {
            if items.is_empty() {
                return Err("give TYPE[:INST] items to remove".into());
            }
            for it in items {
                let (ty, inst) = p::afx_slot(it)?;
                let pos = slots
                    .iter()
                    .position(|s| s.ty == ty && inst.is_none_or(|i| i == s.inst))
                    .ok_or_else(|| format!("{it:?} is not in AFX {}", c + 1))?;
                slots.remove(pos);
            }
        }
    }
    let op = Op::AfxChain { chain: c, slots };
    ctx.send(std::slice::from_ref(&op))?;
    let st = ctx.state()?;
    let text = afx_chains(&st)
        .lines()
        .nth(c as usize)
        .unwrap_or_default()
        .to_string();
    out(format!("{text}\n"), ops_json(&[op]))
}

// ---------------------------------------------------------------------------
// snapshots

fn load_snapshot(path: &Path) -> Result<Snapshot, Fail> {
    let text = std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
    serde_json::from_str(&text)
        .map_err(|e| format!("{}: not a snapshot: {e}", path.display()).into())
}

fn diff_out(a: &DeviceState, b: &DeviceState, la: &str, lb: &str) -> R {
    let av = serde_json::to_value(a).map_err(|e| e.to_string())?;
    let bv = serde_json::to_value(b).map_err(|e| e.to_string())?;
    let changes = crate::diff::diff(&av, &bv);
    let mut text = String::new();
    if changes.is_empty() {
        text.push_str("no differences\n");
    } else {
        let mut rows = vec![vec![
            "PATH".to_string(),
            la.to_uppercase(),
            lb.to_uppercase(),
        ]];
        for c in &changes {
            rows.push(vec![
                c.path.clone(),
                crate::diff::show(&c.a),
                crate::diff::show(&c.b),
            ]);
        }
        text = table(&rows);
    }
    let js: Vec<Value> = changes
        .iter()
        .map(|c| json!({"path": c.path, la: c.a, lb: c.b}))
        .collect();
    out(text, json!({"changes": js}))
}

fn snapshot(ctx: &mut Ctx, sub: SnapCmd) -> R {
    match sub {
        SnapCmd::Save { file } => {
            let snap = ctx.be()?.snapshot()?;
            let text = serde_json::to_string_pretty(&snap).map_err(|e| e.to_string())?;
            std::fs::write(&file, text + "\n").map_err(|e| format!("{}: {e}", file.display()))?;
            out(
                format!("saved {}\n", file.display()),
                json!({"ok": true, "saved": file, "taken": snap.taken}),
            )
        }
        SnapCmd::Diff { file, file2 } => {
            let a = load_snapshot(&file)?;
            match file2 {
                Some(f2) => {
                    let b = load_snapshot(&f2)?;
                    diff_out(&a.state, &b.state, "a", "b")
                }
                None => {
                    let live = ctx.state()?;
                    diff_out(&a.state, &live, "file", "live")
                }
            }
        }
        SnapCmd::Restore { file, dry_run } => {
            let snap = load_snapshot(&file)?;
            let plan = ctx.be()?.restore(snap, true)?;
            if !dry_run && !plan.is_empty() {
                // Apply exactly the plan that was confirmed.
                let before = ctx.state()?.status.clock_source;
                ctx.send(&plan)?;
                let mut ops = plan;
                let text = format!(
                    "restored {} ({} op{})\n",
                    file.display(),
                    ops.len(),
                    if ops.len() == 1 { "" } else { "s" }
                );
                if let Some(target) = external_clock_target(&ops) {
                    let st =
                        settle_clock(ctx, target, before, DEFAULT_LOCK_TIMEOUT, false, &mut ops)?;
                    if !st.locked {
                        return out_failed(
                            format!("{text}clock {}  NOT LOCKED\n", clock_source_name(target)),
                            json!({"ok": false, "applied": true, "ops": ops, "locked": false,
                                   "reverted": st.reverted}),
                        );
                    }
                }
                return out(text, json!({"ok": true, "applied": true, "ops": ops}));
            }
            let mut text = String::new();
            if plan.is_empty() {
                text.push_str("nothing to restore\n");
            } else {
                for op in &plan {
                    let risk = op.risk();
                    text.push_str(&format!(
                        "{:<10} {}\n",
                        format!("{risk:?}").to_lowercase(),
                        op_text(op)
                    ));
                }
            }
            out(text, json!({"ok": true, "applied": false, "ops": plan}))
        }
    }
}

// ---------------------------------------------------------------------------
// raw

/// Map a decoded set back to the semantic op(s) the core accepts.
pub fn set_to_ops(set: Set) -> Result<Vec<Op>, String> {
    Ok(match set {
        Set::Power(on) => vec![Op::Power { on }],
        Set::SampleRate(rate) => vec![Op::SampleRate { rate }],
        Set::ClockSource(source) => vec![Op::ClockSource { source }],
        Set::Volume(id, atten) => vec![Op::Volume { id, atten }],
        Set::Mute(id, mute) => vec![Op::Mute { id, mute }],
        Set::PresetSave(slot) => vec![Op::PresetSave { slot }],
        Set::PresetRecall(slot) => vec![Op::PresetRecall { slot }],
        Set::Oscillator(osc) => vec![Op::Oscillator { osc }],
        Set::Brightness(value) => vec![Op::Brightness { value }],
        Set::Trim(id, rec) => {
            let mut ops = vec![Op::TrimControl {
                id,
                control: rec.control,
            }];
            if rec.levels.iter().all(|l| *l == rec.levels[0]) {
                ops.push(Op::Trim {
                    id,
                    ch: None,
                    level: rec.levels[0],
                });
            } else {
                for (i, l) in rec.levels.iter().enumerate() {
                    ops.push(Op::Trim {
                        id,
                        ch: Some(i as u8),
                        level: *l,
                    });
                }
            }
            ops
        }
        Set::SpdifSrc(on) => vec![Op::SpdifSrc { on }],
        Set::Smux(id, on) => vec![Op::Smux { id, on }],
        Set::MeterSource(bank, src) => vec![Op::MeterSource { bank, src }],
        Set::MixerChannel(mixer, ch, c) => vec![Op::Mixer {
            mixer,
            ch,
            level: Some(c.level),
            pan: Some(c.pan),
            mute: Some(c.mute),
            solo: Some(c.solo),
            send: Some(c.send),
        }],
        Set::Routing(bank, mut sources) => {
            sources.truncate(bank.size());
            vec![Op::Route {
                bank,
                ch: 0,
                sources,
            }]
        }
        Set::StereoLink(target, linked) => vec![Op::StereoLink { target, linked }],
        Set::AfxEq { inst, band, value } => vec![Op::AfxEqBand {
            inst,
            band,
            freq: Some(value.freq),
            q: Some(value.q),
            gain: Some(value.gain),
            ftype: Some(value.ftype),
        }],
        Set::AfxCompressor { inst, params } => vec![Op::AfxCompressor { inst, params }],
        Set::AfxRaw { inst, raw } => {
            let table =
                afx_fields(raw.ty).ok_or_else(|| format!("{:?} is not a raw AFX model", raw.ty))?;
            let fields: Vec<Option<i32>> = if raw.fields.len() == table.len() {
                table
                    .iter()
                    .zip(&raw.fields)
                    .map(|(f, v)| f.in_set.then_some(*v))
                    .collect()
            } else {
                let mut it = raw.fields.iter();
                table
                    .iter()
                    .map(|f| if f.in_set { it.next().copied() } else { None })
                    .collect()
            };
            vec![Op::AfxRaw {
                ty: raw.ty,
                inst,
                fields,
            }]
        }
        Set::AfxOrder { chain, slots } => {
            let packed: Vec<AfxSlot> = slots.iter().flatten().copied().collect();
            let n = slots.iter().take_while(|s| s.is_some()).count();
            if n != packed.len() {
                return Err(
                    "AFX order with gaps between slots: the chain op packs slots from slot 1"
                        .into(),
                );
            }
            vec![Op::AfxChain {
                chain,
                slots: packed,
            }]
        }
        Set::AfxBypass { ty, inst, enabled } => vec![Op::AfxBypass { ty, inst, enabled }],
        Set::Reverb(params) => vec![Op::Reverb { params }],
    })
}

fn raw(ctx: &mut Ctx, i_know: bool, hex: &str) -> R {
    if !i_know {
        return Err(Fail::Refused(
            "raw sets need --i-know (see `openorion raw --help`)".into(),
        ));
    }
    let bytes = p::hex(hex)?;
    let report = Report::raw_checked(&bytes).map_err(|e| Fail::Refused(e.to_string()))?;
    let set = Set::decode(report.as_bytes())
        .map_err(|e| Fail::Refused(format!("payload does not decode to a known set: {e}")))?;
    let mut ops = set_to_ops(set).map_err(Fail::Refused)?;
    let before = ctx.state()?.status.clock_source;
    ctx.send(&ops)?;
    let text: String = ops
        .iter()
        .map(|o| format!("sent {}\n", op_text(o)))
        .collect();
    if let Some(target) = external_clock_target(&ops) {
        let st = settle_clock(ctx, target, before, DEFAULT_LOCK_TIMEOUT, false, &mut ops)?;
        if !st.locked {
            return out_failed(
                format!("{text}clock {}  NOT LOCKED\n", clock_source_name(target)),
                json!({"ok": false, "ops": ops, "locked": false, "reverted": st.reverted}),
            );
        }
    }
    out(text, ops_json(&ops))
}
