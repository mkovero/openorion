//! USB loopback stress test (feature `jack`).
//!
//! Plays a counter on every JACK playback port and checks it comes back on
//! the matching capture port. Needs a device-side loopback: route
//! USB REC n from USB PLAY n (silent; no physical output involved).
//!
//! Each sample carries `(channel + 1) << 14 | frame_counter` (21 bits, exact
//! in 24-bit PCM; never 0, so silence stays distinguishable), so the checker sees wrong channels, dropped or repeated
//! frames and corrupted samples separately.
//!
//!   cargo run -p openorion-jack --features jack --example loopback -- 300 64

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use jack::{
    AudioIn, AudioOut, Client, ClientOptions, Control, NotificationHandler, Port, ProcessHandler,
    ProcessScope,
};

const CNT_BITS: u32 = 14;
const CNT_MASK: u32 = (1 << CNT_BITS) - 1;
const SCALE: f32 = 8_388_608.0; // 2^23

#[derive(Default)]
struct Stats {
    cycles: AtomicU64,
    frames_ok: AtomicU64,
    /// Sample from the wrong channel, or not a counter value.
    corrupt: AtomicU64,
    /// Counter jumped (dropped or repeated frames).
    jumps: AtomicU64,
    /// Silence after the channel had locked.
    dropouts: AtomicU64,
    xruns: AtomicU64,
    lat_min: AtomicU64,
    lat_max: AtomicU64,
    max_cycle_us: AtomicU64,
}

struct Proc {
    outs: Vec<Port<AudioOut>>,
    ins: Vec<Port<AudioIn>>,
    /// Next expected counter per capture channel (None until locked).
    expect: Vec<Option<u32>>,
    tx: u32,
    stats: Arc<Stats>,
}

impl ProcessHandler for Proc {
    fn process(&mut self, _: &Client, ps: &ProcessScope) -> Control {
        let t0 = Instant::now();
        let n = ps.n_frames();
        let s = &self.stats;
        for (ch, p) in self.outs.iter_mut().enumerate() {
            let buf = p.as_mut_slice(ps);
            for (i, x) in buf.iter_mut().enumerate() {
                let v = (ch as u32 + 1) << CNT_BITS | (self.tx.wrapping_add(i as u32) & CNT_MASK);
                *x = v as f32 / SCALE;
            }
        }
        let (mut ok, mut corrupt, mut jumps, mut drop) = (0u64, 0u64, 0u64, 0u64);
        for (ch, p) in self.ins.iter().enumerate() {
            let exp = &mut self.expect[ch];
            for (i, x) in p.as_slice(ps).iter().enumerate() {
                let v = (x * SCALE).round() as i64;
                if v == 0 {
                    if exp.is_some() {
                        drop += 1;
                        *exp = None;
                    }
                    continue;
                }
                if v < 0 || (v as u32) >> CNT_BITS != ch as u32 + 1 {
                    corrupt += 1;
                    *exp = None;
                    continue;
                }
                let cnt = v as u32 & CNT_MASK;
                match *exp {
                    Some(e) if e == cnt => ok += 1,
                    Some(_) => jumps += 1,
                    None if ch == 0 => {
                        // Round trip, frames: sent counter minus received.
                        let lat =
                            (self.tx.wrapping_add(i as u32).wrapping_sub(cnt) & CNT_MASK) as u64;
                        s.lat_min.fetch_min(lat, Ordering::Relaxed);
                        s.lat_max.fetch_max(lat, Ordering::Relaxed);
                    }
                    None => {}
                }
                *exp = Some((cnt + 1) & CNT_MASK);
            }
        }
        self.tx = self.tx.wrapping_add(n);
        s.cycles.fetch_add(1, Ordering::Relaxed);
        s.frames_ok.fetch_add(ok, Ordering::Relaxed);
        s.corrupt.fetch_add(corrupt, Ordering::Relaxed);
        s.jumps.fetch_add(jumps, Ordering::Relaxed);
        s.dropouts.fetch_add(drop, Ordering::Relaxed);
        s.max_cycle_us
            .fetch_max(t0.elapsed().as_micros() as u64, Ordering::Relaxed);
        Control::Continue
    }
}

struct Notif(Arc<Stats>);

impl NotificationHandler for Notif {
    fn xrun(&mut self, _: &Client) -> Control {
        self.0.xruns.fetch_add(1, Ordering::Relaxed);
        Control::Continue
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().collect();
    let secs: u64 = args.get(1).map_or(Ok(60), |a| a.parse())?;
    let nch: usize = args.get(2).map_or(Ok(64), |a| a.parse())?;

    let (client, _) = Client::new("openorion-loopback", ClientOptions::NO_START_SERVER)?;
    let rate = client.sample_rate();
    let period = client.buffer_size();
    let mut outs = Vec::new();
    let mut ins = Vec::new();
    for i in 1..=nch {
        outs.push(client.register_port(&format!("out_{i:02}"), AudioOut::default())?);
        ins.push(client.register_port(&format!("in_{i:02}"), AudioIn::default())?);
    }
    let stats = Arc::new(Stats {
        lat_min: AtomicU64::new(u64::MAX),
        ..Default::default()
    });
    let proc = Proc {
        outs,
        ins,
        expect: vec![None; nch],
        tx: 0,
        stats: stats.clone(),
    };
    let active = client.activate_async(Notif(stats.clone()), proc)?;
    let c = active.as_client();
    for i in 1..=nch {
        c.connect_ports_by_name(
            &format!("openorion-loopback:out_{i:02}"),
            &format!("system:playback_{i}"),
        )?;
        c.connect_ports_by_name(
            &format!("system:capture_{i}"),
            &format!("openorion-loopback:in_{i:02}"),
        )?;
    }
    println!("{nch} ch, {rate} Hz, period {period}, {secs} s");

    let start = Instant::now();
    let get = |a: &AtomicU64| a.load(Ordering::Relaxed);
    let report = |label: &str| {
        let lat = match get(&stats.lat_min) {
            u64::MAX => "-".to_string(),
            m => format!("{m}..{}", get(&stats.lat_max)),
        };
        println!(
            "{label:>6} ok {:>12}  corrupt {}  jumps {}  dropouts {}  xruns {}  rt {lat} fr  max cycle {} us  dsp {:.1}%",
            get(&stats.frames_ok),
            get(&stats.corrupt),
            get(&stats.jumps),
            get(&stats.dropouts),
            get(&stats.xruns),
            get(&stats.max_cycle_us),
            c.cpu_load(),
        );
    };
    while start.elapsed() < Duration::from_secs(secs) {
        std::thread::sleep(
            Duration::from_secs(10).min(Duration::from_secs(secs).saturating_sub(start.elapsed())),
        );
        report(&format!("{}s", start.elapsed().as_secs()));
    }
    let expected = get(&stats.cycles) * period as u64 * nch as u64;
    report("final");
    println!(
        "frames checked {} of {} sent",
        get(&stats.frames_ok),
        expected
    );
    active.deactivate()?;
    Ok(())
}
