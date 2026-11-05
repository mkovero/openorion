//! Passive sine watcher (feature `jack`): reads the capture ports and
//! checks every sample of a steady sine, so a long-running load (e.g.
//! Reaper playing tone generators through a device loopback) can be left
//! running and audited. It only reads: no output ports, nothing written.
//!
//! A sine obeys x[n] = 2cos(w) x[n-1] - x[n-2]. Any dropped, repeated or
//! zeroed sample breaks that, so each break counts as a glitch.
//!
//!   cargo run -p openorion-jack --features jack --example sinewatch -- 997 64 [port-prefix]
//!
//! Prints a line per minute, plus one per channel per minute with glitches.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use jack::{
    AudioIn, Client, ClientOptions, Control, NotificationHandler, Port, ProcessHandler,
    ProcessScope,
};

/// Residual that counts as a glitch (about -80 dBFS; 24-bit rounding
/// leaves ~1e-6).
const TOL: f32 = 1e-4;
/// A channel below this peak counts as silent.
const SILENT: f32 = 1e-3;

struct Chan {
    x1: f32,
    x2: f32,
    warm: u32,
}

struct Shared {
    glitches: Vec<AtomicU64>,
    silent_cycles: Vec<AtomicU64>,
    /// Peak of the last cycle per channel, as f32 bits.
    peak: Vec<AtomicU64>,
    xruns: AtomicU64,
    cycles: AtomicU64,
    max_cycle_us: AtomicU64,
}

struct Proc {
    ins: Vec<Port<AudioIn>>,
    ch: Vec<Chan>,
    c: f32,
    s: Arc<Shared>,
}

impl ProcessHandler for Proc {
    fn process(&mut self, _: &Client, ps: &ProcessScope) -> Control {
        let t0 = Instant::now();
        for (i, p) in self.ins.iter().enumerate() {
            let st = &mut self.ch[i];
            let mut bad = 0u64;
            let mut peak = 0f32;
            for &x in p.as_slice(ps) {
                peak = peak.max(x.abs());
                if st.warm >= 2 {
                    if (x - (self.c * st.x1 - st.x2)).abs() > TOL {
                        bad += 1;
                        st.warm = 0;
                    }
                } else {
                    st.warm += 1;
                }
                st.x2 = st.x1;
                st.x1 = x;
            }
            if peak < SILENT {
                self.s.silent_cycles[i].fetch_add(1, Ordering::Relaxed);
                st.warm = 0;
            } else if bad > 0 {
                self.s.glitches[i].fetch_add(1, Ordering::Relaxed);
            }
            self.s.peak[i].store(peak.to_bits() as u64, Ordering::Relaxed);
        }
        self.s.cycles.fetch_add(1, Ordering::Relaxed);
        self.s
            .max_cycle_us
            .fetch_max(t0.elapsed().as_micros() as u64, Ordering::Relaxed);
        Control::Continue
    }
}

struct Notif(Arc<Shared>);

impl NotificationHandler for Notif {
    fn xrun(&mut self, _: &Client) -> Control {
        self.0.xruns.fetch_add(1, Ordering::Relaxed);
        Control::Continue
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().collect();
    let freq: f64 = args.get(1).map_or(Ok(997.0), |a| a.parse())?;
    let nch: usize = args.get(2).map_or(Ok(64), |a| a.parse())?;
    let prefix = args
        .get(3)
        .cloned()
        .unwrap_or_else(|| "system:capture_".into());

    let (client, _) = Client::new("openorion-sinewatch", ClientOptions::NO_START_SERVER)?;
    let rate = client.sample_rate() as f64;
    let period = client.buffer_size();
    let mut ins = Vec::new();
    for i in 1..=nch {
        ins.push(client.register_port(&format!("in_{i:02}"), AudioIn::default())?);
    }
    let s = Arc::new(Shared {
        glitches: (0..nch).map(|_| AtomicU64::new(0)).collect(),
        silent_cycles: (0..nch).map(|_| AtomicU64::new(0)).collect(),
        peak: (0..nch).map(|_| AtomicU64::new(0)).collect(),
        xruns: AtomicU64::new(0),
        cycles: AtomicU64::new(0),
        max_cycle_us: AtomicU64::new(0),
    });
    let proc = Proc {
        ins,
        ch: (0..nch)
            .map(|_| Chan {
                x1: 0.0,
                x2: 0.0,
                warm: 0,
            })
            .collect(),
        c: (2.0 * (2.0 * std::f64::consts::PI * freq / rate).cos()) as f32,
        s: s.clone(),
    };
    let name = client.name().to_string();
    let active = client.activate_async(Notif(s.clone()), proc)?;
    for i in 1..=nch {
        active
            .as_client()
            .connect_ports_by_name(&format!("{prefix}{i}"), &format!("{name}:in_{i:02}"))?;
    }
    println!("watching {nch} ch at {freq} Hz, {rate} Hz, period {period}; a line per minute");

    let start = Instant::now();
    let take = |a: &AtomicU64| a.swap(0, Ordering::Relaxed);
    let (mut tot_glitch, mut tot_xrun) = (0u64, 0u64);
    loop {
        std::thread::sleep(Duration::from_secs(60));
        let mut bad = Vec::new();
        let mut silent = Vec::new();
        let (mut pmin, mut pmax) = (f32::MAX, 0f32);
        for i in 0..nch {
            let g = take(&s.glitches[i]);
            if g > 0 {
                bad.push(format!("{}:{g}", i + 1));
            }
            tot_glitch += g;
            if take(&s.silent_cycles[i]) > 0 {
                silent.push((i + 1).to_string());
            }
            let p = f32::from_bits(s.peak[i].load(Ordering::Relaxed) as u32);
            pmin = pmin.min(p);
            pmax = pmax.max(p);
        }
        let x = take(&s.xruns);
        tot_xrun += x;
        println!(
            "{:>6} min  cycles {}  xruns {x} (total {tot_xrun})  glitched ch {} (glitch cycles total {tot_glitch})  \
             peak {:.1}..{:.1} dBFS  max cycle {} us  dsp {:.1}%{}{}",
            start.elapsed().as_secs() / 60,
            take(&s.cycles),
            bad.len(),
            20.0 * pmin.max(1e-9).log10(),
            20.0 * pmax.max(1e-9).log10(),
            take(&s.max_cycle_us),
            active.as_client().cpu_load(),
            if bad.is_empty() {
                String::new()
            } else {
                format!("  glitched ch:cycles {}", bad.join(" "))
            },
            if silent.is_empty() {
                String::new()
            } else {
                format!("  silent ch {}", silent.join(","))
            },
        );
    }
}
