# openorion software design, v1

Implementation overview for the Linux Orion32 HD controller. The protocol
facts and their provenance are recorded in [protocol-spec.md](protocol-spec.md).
Rust types and command-line `--help` are authoritative for exact API shapes
and options.

## 1. Goals and non-goals

Goals
- Control every confirmed feature of the Orion32 HD from Linux with no
  vendor software: clock/rate, routing, 4 mixers, trims, volumes/mutes,
  oscillator, brightness, S-MUX/SRC, presets, reverb, AFX (EQ, compressor,
  chains, bypass; vintage models as raw knobs), meters.
- One daemon owns the device. A CLI and a GUI are clients. Several clients
  can run at once and see each other's changes live.
- Robust transport: never sends a non-320-byte write, detects and handles
  the wake/hang states, re-syncs after device-side changes (front-panel
  knob, preset recall).
- Host-side snapshots (JSON) = unlimited presets, diffable, restorable.
- **JACK-first audio workflow**: audio runs through JACK (jackd with the
  ALSA backend, or PipeWire-JACK) on top of the kernel's snd-usb-audio,
  which already works. openorion adds JACK integration (sec. 9) and no ALSA
  mixer controls.

Outside the project scope
- Firmware-update, bootloader, flash, licensing writes, `set_config_feature`, factory reset or
  0x76 command. These are **not representable** in the code (sec. 4.3).
- HDX compensation and set_predefined_preset (untested or unknown).
- A userspace audio driver. The audio path stays in the kernel.
- The vendor-manager TCP transport.
- macOS/Windows.

## 2. Architecture

```
            ┌──────────── openorion (CLI) ──────────┐
            │  ┌──────── openorion-ui (egui) ─────┐ │   ┌─ openorion-jack (JACK client)
            │  │                              │ │   │
            ▼  ▼          unix socket, JSON lines    ▼
          ┌────────────────────── openoriond ───────────────────────┐
          │ ipc server ── Session (openorion-core) ── Transport     │
          │                 state cache, request queue,         │
          │                 sync, coalescing, verify            │
          └──────────────────────┬──────────────────────────────┘
                                 │ Transport trait
                    ┌────────────┴────────────┐
             HidrawTransport (openorion-hid)   SimTransport (openorion-sim)
                    │                          (tests, UI dev, demo)
             /dev/hidrawN + USBDEVFS_RESET
```

- `openorion-proto` is pure (no I/O): types, encode, decode. Everything depends
  on it.
- `openorion-core::Session` is the only place that sequences requests. The
  daemon embeds it. `openorion --direct` embeds it too when no daemon is running
  (it takes the same lock).
- Every client talks semantic operations ("mixer 4 ch 1 level -30 dB"), never
  raw bytes. A raw-bytes escape hatch exists only in the CLI, behind
  `openorion raw --i-know`, and still uses the pid whitelist.

## 3. Repository layout

```
Cargo.toml                 workspace; edition 2024; MSRV 1.95
crates/
  openorion-proto/             pure protocol: model, codec, snapshot serde
  openorion-hid/               transport: Linux hidraw (discovery, reset)
  openorion-sim/               simulated device implementing Transport
  openorion-core/              Session: cache, queue, sync, events
  openorion-ipc/               IPC message types + blocking client
  openoriond/                  daemon binary
  openorion-cli/               `openorion` binary
  openorion-ui/                `openorion-ui` binary (eframe/egui)
  openorion-jack/              `openorion-jack` binary (optional, feature-gated build)
  openorion-control/           mappable params, MIDI/OSC mappings, remote config (pure)
  openorion-remote/            `openorion-remote` binary: OSC + ALSA MIDI bridge
fixtures/                  real device replies for tests
packaging/                 udev rule, systemd user units, desktop file
```

Dependencies (keep the list short; ask before adding heavy ones): `thiserror`,
`serde`/`serde_json`, `tokio` (rt-multi-thread, net, sync, time, macros;
daemon and core), `tracing` + `tracing-subscriber`, `clap` (derive), `libc` (ioctl, sysfs), `eframe`/`egui` (UI), `jack` crate (openorion-jack),
`rosc` + `midir` (openorion-remote), `proptest` (dev). No libusb or hidapi: the transport uses hidraw files, polling and ioctls.

## 4. openorion-proto

### 4.1 Model (`model.rs`)

Plain data with serde. Indices are 0-based in code; display names in
`names.rs`.

The model in [`model.rs`](../crates/openorion-proto/src/model.rs) includes
`DeviceState`, routing banks, mixers, trims, status, meters and AFX state.
Newtypes validate indices and values. AFX raw parameter values use
`Vec<i32>` to represent signed and multi-byte fields; their layouts are in
[`names.rs`](../crates/openorion-proto/src/names.rs).

### 4.2 Codec (`codec.rs`)

[`codec.rs`](../crates/openorion-proto/src/codec.rs) defines `Report`,
`Get`, `Set`, request encoding and incoming-report parsing. `Get::report`
constructs a read request; `Set::report` and `Set::payload` return results
because encoding validates inputs. Incoming replies are decoded by
[`decode.rs`](../crates/openorion-proto/src/decode.rs).

### 4.3 Safety by construction

- `Report` has no public constructor from arbitrary bytes, except
  `Report::raw_checked(payload)`, which accepts only whitelisted set pids
  (3,4,5,6,7,8,9,10,11,12,13,14,16,17,18,19,20,21,24,26)
  and checks payload framing. The `Get` API excludes ext2 17 requests.
  The CLI also decodes raw input to a known set and converts it to semantic
  operations before sending it. There is no variant for pid 1, 0x76, 0x06 or any id other than
  0x70/0x74.
- A unit test enumerates every `Set`/`Get` variant and asserts the id, the
  pid and that the length is 320.
- `Set::class()` returns `Risk::{Silent, Audible, Disruptive}`. Clients must
  confirm Disruptive (Power, SampleRate, ClockSource, PresetRecall,
  PresetSave, Routing to a physical output bank). The daemon does not
  enforce this; it is UX policy.

### 4.4 Tests

Golden tests from every example in protocol-spec.md sec. 5 and from
`fixtures/s1-decoded-sets.tsv` (sets captured on the wire). Decoder tests
on `fixtures/dump1.json` (expected values from an independent decoder,
saved as `fixtures/dump1.decoded.txt`). Proptest round-trip
`decode(encode(x)) == x` for all set-able state.

## 5. openorion-hid (Transport)

```rust
pub trait Transport: Send {
    fn write(&mut self, r: &Report) -> io::Result<()>;          // always 321 bytes to hidraw
    fn read(&mut self, timeout: Duration) -> io::Result<Option<[u8; 320]>>;
    fn reset(&mut self) -> io::Result<()>;                       // USBDEVFS_RESET, then reopen
}
```

- Discovery: scan `/sys/class/hidraw/*/device/uevent` for
  `HID_ID=0003:000023E5:0000A210`. The USB node comes from the parent
  `busnum`/`devnum`. Report "not found" vs "permission denied" clearly.
- `reset()`: open `/dev/bus/usb/BBB/DDD` O_RDWR, ioctl 0x5514, close, wait
  for the hidraw node to come back (poll sysfs every 100 ms, up to 5 s),
  reopen. **Resets only this device.** Never touch the controller or any
  other device.
- Reads are blocking with `poll()` and a timeout. The core runs them on a
  dedicated thread.
- Permissions: `packaging/60-openorion.rules` gives the `audio` group (or
  uaccess) rw on the hidraw node and on the USB device node (needed for the
  reset).

## 6. openorion-core (Session)

One tokio task owns the queue and cache. A dedicated I/O thread owns the
Transport, receiving writes through a channel and delivering incoming
reports to the task. Responsibilities:

1. **Startup.** Open, wait ≤200 ms for cyclic. None → `reset()` once →
   wait ≤2 s. Still none → `Error::NoCyclic`. The startup reset is skipped
   while the Orion's USB audio streams (`/proc/asound/cardN/stream0`
   "Status: Running"): `Error::ResetBlocked`, because a reset cuts the
   stream.
2. **Full sync.** Gets in the panel's order: firmware, passport,
   registration, routing 0..14, mixers 0..3, mixer links, AFX order, AFX
   links, AFX instances, the AFX configs for types that have instances (EQ
   both halves), reverb, trims, HDX delays. About 60 gets × 28 ms ≈ 2 s.
   Publish `Event::Synced(DeviceState)`.
3. **Request queue.** One in flight. Timeout 500 ms, one retry. Writes
   are paced: at least `write_gap` (5 ms) between any two reports out,
   and the I/O thread reads what is ready between writes, so a burst of
   sets cannot starve the cyclic reports.
   **Health.** The device's control channel is fragile (protocol-spec.md
   sec. 1: wrong-size writes poison it, a hang leaves cyclic flowing, a
   reset may answer only once, 0x61 while booting), so the session checks
   it actively instead of waiting for a setting to fail:
   - *Idle probe*: after `probe_idle` (5 s) without an answer, one
     get from a rotation over every setting block a sync reads (plus
     `get firmware`; one full round takes a few minutes). Two unanswered
     tries while cyclic flows → `Health::Hung`. An answer that differs
     from the cache (and no pending write covers it) starts a resync:
     this catches changes from the front panel or another client, and
     recalls after `device_updated` stays set (we never send 0x76).
   - *0x61 to get firmware* (which every firmware knows): back off
     500 ms and ask again (boot window); four in a row → `Hung`.
   - *Cyclic stops* for more than 1 s → `Health::Stalled`; it may come
     back by itself (then a resync). A reset gets no cyclic →
     `Health::NoCyclic`.
   - *Write gate*: `Health::writable()` is true only for `Ok` and
     `Syncing`. In `Stalled`, `Hung` and `NoCyclic` settings are refused
     with an error, not queued; unsent and unconfirmed settings are
     dropped when the session halts, and the resync after recovery reads
     what the device really has.
   - *Advice*: `Health::advice()` says what to do: wait (`Syncing`), wait
     then `openorion reset` (`Stalled`), power-cycle (`Hung`,
     `NoCyclic`; a USB reset does not clear a hung channel and was seen
     to cause one), check the connection (`Disconnected`).
   - *Recheck* (`Op::Recheck`, `openorion health --recheck`, the UI's
     RECHECK button): two `get firmware` in a row, then a resync. Reads
     only, so audio is never touched. From `Hung` it is the soft
     recovery: a device that answers both returns to `Ok`; one that
     answers only the first (poisoned) stays `Hung`.
   **No automatic reset after startup**: on hardware one cut a running
   USB audio stream. `Op::Reset` is refused while the USB audio streams
   unless `force`.
4. **Semantic ops → sets.** `Op` (in openorion-ipc) is high-level, e.g.
   `Op::Mixer` with optional level, pan, mute, solo and send fields. The core builds the full set message from the
   cached state, because most sets carry all fields (mixer channel, routing
   bank, trims, reverb, EQ band). Then it updates the cache optimistically.
5. **Coalescing.** Continuous controls (faders, pans, knobs, volume) are
   keyed by target. Latest wins; at most one send per key per 20 ms.
6. **Verify.** Sets have no ack. After the last set to a key, debounce
   150 ms, then read back the covering get (or watch status for
   status-backed fields). On mismatch: `Event::Mismatch`, and the cache
   takes the device's value.
7. **Status stream.** Decode every 0x73/0x83. Diff the non-meter status
   against the cache; publish `Event::Status` on change. Meters: keep the
   per-channel min raw (= max level) over each 33 ms window and publish
   `Event::Meters` at 30 Hz.
8. **Resync triggers.** `device_updated` 0→1, `current_preset` changing
   to non-zero without our own recall, after any reset, an idle re-read
   that differs from the cache (item 3), and on the `Resync` op.
9. **Lock.** Take `flock` on `$XDG_RUNTIME_DIR/openorion.lock` so only one
   Session (daemon or `--direct`) exists.
10. **AFX quirks.** When an instance is removed from a chain, send bypass
    `enabled=1` afterwards to keep the factory state (findings).

## 7. IPC (openorion-ipc, openoriond)

- Socket: `$XDG_RUNTIME_DIR/openorion.sock`, newline-delimited JSON,
  `serde(tag = "t")`.
- Message schemas are `ClientMsg` and `DaemonMsg` in
  [`openorion-ipc/src/lib.rs`](../crates/openorion-ipc/src/lib.rs).
  Clients send a versioned hello, state/subscription requests, operations,
  snapshots and restores. Operations, snapshots and restores carry request
  IDs. Replies include state, diffs, meters, results, health and mismatches.
- **Version and remote relay** (IPC v2). `{"t":"remote","msg":<any JSON>}`
  from a client is relayed unchanged as `{"t":"remote","msg":...}` to every
  *other* subscribed client (not back to the sender) that said hello with
  v2 or later; the daemon never interprets `msg`. It carries client-to-client
  traffic such as openorion-remote <-> UI (learn, announce). A `msg` over
  64 KiB serialized (`REMOTE_MAX_BYTES`) is refused with
  `{"t":"result","id":0,"ok":false,"error":"remote message too large ..."}`
  and not relayed. Relay buffer 256 messages; a lagging client loses the
  oldest. The daemon answers `hello` with the lower of its and the client's
  version, so a v1 client keeps working and never sees `remote`.
- `openorion_ipc::apply_diff(&mut DeviceState, path, &value)` applies a
  `diff` (JSON pointer into `DeviceState`) for clients that mirror state.
- `openorion-ipc` provides a **blocking** `Client` (std UnixStream plus a reader
  thread delivering events on an mpsc channel) for the CLI and UI, and the
  serde types for the daemon.
- openoriond: `openoriond [--sim fixtures/dump1.json] [--foreground]`. A systemd
  user unit in packaging. Logs through tracing. `--sim` runs on SimTransport
  so the UI/CLI work without hardware.
- **AFX across power cycles** (`openoriond/src/afx_keep.rs`). A power
  cycle resets the AFX configs to factory defaults while routing and
  mixers survive. The daemon keeps the last good configs (per serial) in
  `$XDG_STATE_HOME/openorion/afx.json`, refreshed every 10 s while
  healthy. Right after each (re)connect and first sync it puts back every
  instance that differs from the copy *and* now holds factory defaults
  (`openorion_core::afx_power_up_target` + `plan_restore`); instances
  with other values (changed elsewhere) are kept. Clients get a
  `DaemonMsg::Notice`. On by default; `--no-afx-restore` turns it off,
  `--afx-state FILE` moves the file (with `--sim` it is off unless given).

## 8. Clients

### 8.1 CLI `openorion`

The CLI supports device status, routing, mixer controls, AFX, clock/rate,
trims, oscillator, reverb, presets, snapshots, meters and explicit reset.
See `openorion --help` and each subcommand's `--help` for exact syntax.
The [README](../README.md#use) contains examples.

By default it connects to the daemon. `--direct` embeds a session and takes
the same exclusive lock; stop the daemon before direct hardware access.
`--json` selects machine-readable output. Disruptive commands need
confirmation or `--yes`; ordinary audible controls do not. `raw --i-know`
still validates input and sends semantic operations through the session.

### 8.2 GUI `openorion-ui` (eframe/egui)

The egui/eframe GUI connects to the daemon through a background IPC bridge.
It includes overview, routing, mixer, trims, AFX, settings and snapshot
views. Disruptive actions open a confirmation dialog; a clock change that
fails to lock offers keep/revert handling. Ordinary controls apply directly.

`openorion-ui --demo` uses built-in state and synthetic meters.
`--state FILE` loads a DeviceState or Snapshot JSON file. Both modes are
offline: edits affect the local state and outbox, not hardware. The GUI is
built for the host and uses its graphics/windowing libraries.

## 9. JACK integration (openorion-jack)

The priority is a decent straight JACK workflow. Audio stays on
snd-usb-audio; JACK opens it through its ALSA backend, or PipeWire-JACK
does. openorion-jack is a small JACK client plus daemon IPC client:

1. **Port naming.** Using JACK metadata (`http://jackaudio.org/metadata/pretty-name`)
   and aliases, name the Orion's capture ports after what feeds `USB REC n`
   (e.g. "ADAT IN 1") and the playback ports after where `USB PLAY n` is
   routed. Update live on routing changes.
2. **Rate guard.** If the JACK server's rate ≠ the Orion's rate, warn through
   the JACK helper. Optional `--follow-jack` sets the Orion rate when JACK
   starts or changes rate. This helper does not enforce a rate-change
   restriction in the CLI/UI.
3. **MIDI control.** A JACK MIDI input port with a plain-text line map (CC → op),
   for example faders and mutes of mixer N and the monitor volume.
   See `openorion-jack --help-midi` for syntax.
4. **Setup doctor.** `openorion-jack doctor` checks: USB3 link (64 ch, not 8),
   snd-usb-audio card present, the JACK device name, a suggested
   `jackd -d alsa -d hw:Orion32HD -r <rate> -p 128 -n 2` (or PipeWire
   quantum), and daemon connectivity.

## 9a. Remote control (openorion-control, openorion-remote)

External control surfaces: OSC (TouchOSC, VU displays, other software) and
MIDI controllers over ALSA seq. `openorion-remote` is a separate daemon IPC
client, like openorion-jack, so openoriond keeps `RestrictAddressFamilies=AF_UNIX`.

1. **Params.** `openorion_control::Param` names every mappable control by
   a path, which is also its OSC address under `/orion/`:
   `mixer/<m>/ch/<c>/{level,pan,mute,solo,send}`, `mixer/<m>/master/{level,mute}`,
   `volume/{main,line}`, `mute/{main,line}`, `trim/{line_in,line_out}/{<n>,all}`,
   `link/...`, `reverb/<m>/<field>`, `afx/<model>/<inst>/...` (1-based like
   the panel). `to_op(norm, &state)` and `read(&state)` work in 0..1 and
   stay inside the encoder's panel limits. Disruptive ops are never
   mappable. `openorion-remote --help-osc` prints the namespace.
2. **Mappings.** `Source` (MIDI device + channel + CC/14-bit CC/note/pitch
   bend, or OSC address + argument, optionally pinned to a sender) → `Param`
   with options: invert, range, mode (absolute, relative 2's-complement,
   toggle), takeover (jump, pickup). A source maps to one param; a param may
   have several sources. The device is the ALSA client name, so two
   controllers sending the same CC are separate bindings; `*` matches any.
3. **Config.** `$XDG_CONFIG_HOME/openorion/remote.json` (versioned JSON):
   OSC listen address (default `127.0.0.1:9000`), OSC targets
   `{addr, values, meters, meter_hz}`, MIDI inputs/outputs enabled, output
   overrides, the virtual `openorion` port, per-output init bytes
   (`midi.init`, hex SysEx; built-in `KNOWN_INIT` entry for the APC40 mkII,
   whose LEDs ignore the host until it gets its introduction message), and
   the mappings. Saved atomically. When an input opens, its feedback
   outputs open too, get their init, and every mapped value is sent so
   LEDs and rings start out right. Echo suppression applies only to
   absolute-mode controls; a button LED or encoder ring always gets the
   new state. Unbound buttons are answered with LED off (controllers often
   light them locally); a control that loses its binding goes dark, and a
   newly bound one shows its value at once.
4. **Engine.** A pure `Engine` (events in, ops/OSC/MIDI/relay messages out)
   inside a thin I/O shell. Ops are throttled latest-wins per param
   (~30 Hz) and mirrored to linked partners (`linked_partner`: mixer
   channel pairs, and AFX chain pairs to the same model in the partner
   chain). Feedback: the remote mirrors
   `DeviceState` with `apply_diff`; changed params go out as
   `/orion/<path> f` to value targets and as CC/note to the MIDI output
   paired with the source device (echo suppression 150 ms). `/orion/sync`
   sends every value; an address with no argument replies with its value.
   Meters: `/orion/meters/<bank> f...` in dBFS at each target's rate.
   **LED meter grid**: a controller with a button grid (`midi.meter_grid`,
   built in for the APC40 mkII clip pads) shows one column per bound fader
   (CC `fader_cc` on MIDI channel = column), a linked pair as one bar. The
   level is the loudest of the mixer meter (when the device meters that
   mixer) and every slot of the AFX chain feeding the channel, since a
   chain-end limiter hides input clips from the mixer meter. Rows light at
   `rows_db`; the 0 dB row is the clip LED, held `clip_hold_ms`. Pads bound
   by a mapping keep their parameter LED. `extra` columns follow one fader
   each (APC: the scene-launch column follows the master fader); a master
   column reads whichever meter bank (A/B) shows the mixer's L/R bus,
   directly or through an output routed from it. Pad brightness = palette
   colour (`colors`). `midi.init` may hold several messages (SysEx, then
   e.g. APC ring-style CCs); they are sent one by one.
5. **Learn.** VJ-style: LEARN on in the UI, click a control, move a MIDI or
   OSC control, repeat; LEARN off saves (Cancel reverts). While learning the
   UI drops its own writes and the remote sends no ops, so nothing changes
   on the device. UI and remote talk through the daemon's opaque relay
   (sec. 7): `RemoteMsg` (`openorion_control::msg`). The remote announces
   itself (`Announce`); without one the UI disables LEARN. The UI tab
   "Remote" lists devices (enable, activity) and edits mapping options.
6. **JACK MIDI map** (sec. 9 item 3) stays for now; its targets are a
   subset of `Param` and may later move onto the remote.

## 10. Simulator (openorion-sim)

`SimTransport` implements `Transport`:
- seeded from `fixtures/dump1.json` (full replies plus cyclic);
- answers gets from its state and applies sets (decoded with openorion-proto);
- emits 0x73/0x83 every 8 ms with synthetic meters (slow sine on a few
  channels);
- models the quirks: no reply to sets; **panics on a write that is not 320
  bytes**; optional fault injection: `hang_after(n)`, `no_cyclic_until_reset`,
  `device_updated` on recall, AFX-removal disables the instance.

It supports core and daemon integration tests and simulated CLI sessions.
The GUI's offline demo uses its own local demo state.

## 11. Testing and hardware policy

- Run tests on Linux with `cargo test --workspace --locked`. The default
  JACK helper build does not link JACK; enable `--features jack` when
  building its live integration.
- CLI daemon tests skip when the sibling `openoriond` binary is absent.
  Build it first with `cargo build --locked -p openoriond` to exercise that
  path. Daemon integration tests use the simulator.
- No automated hardware suite is currently shipped. The HID probe example accesses real hardware and may reset the device;
  it is not part of routine testing. Hardware checks must be deliberate,
  avoid active audio sessions, and preserve/restore device settings.
- Formatting and lint checks: `cargo fmt --all --check` and
  `cargo clippy --workspace --all-targets --locked -- -D warnings`.
