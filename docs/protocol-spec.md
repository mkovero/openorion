# Orion32 HD control protocol: implementer's spec

The wire format the Rust tool implements, in one place. Every item is tagged:

- **[wire]**: confirmed on the device by observing USB traffic and the
  device's replies
- **[static]**: found by interoperability analysis of the vendor software,
  not yet confirmed on the wire
- **[unknown]**

Only what an independent implementation needs is documented here.

## 1. Transport (Linux)

| item | value | tag |
|---|---|---|
| device | USB `23e5:a210`, interface 3 (vendor HID, no report IDs), EP 0x02 OUT / 0x82 IN | wire |
| node | `/dev/hidrawN` whose `device/uevent` has `HID_ID=0003:000023E5:0000A210` | wire |
| write | `0x00` (report-number byte, stripped by the kernel) + **exactly 320 bytes**. The message is zero-padded to 320 | wire |
| read | each `read()` returns one 320-byte report | wire |
| poison | **Any write that is not 320 bytes poisons the control channel.** Later requests go unanswered ("first only" after a reset). Only a power cycle clears it. Make other sizes impossible to send | wire |
| wake | If no cyclic report arrives within ~200 ms of opening, do one `USBDEVFS_RESET` (ioctl `0x5514`) on `/dev/bus/usb/BBB/DDD`. Expect one all-zero report, then cyclic reports | wire |
| hang | Cyclic flowing but requests unanswered (two consecutive timeouts) = hung or poisoned. A reset gives at most one answer; the user must power-cycle | wire |
| unknown cmd | Right after a power cycle the first requests may come back as **0x61** (`HID_ACK_UNKNOWN_CMD`) while the device boots; later ones are answered. Once, a 0x61 echoed our header shifted by 2 bytes (the request parser lost alignment) | wire |
| flood | Unpaced sets at ~1000/s starved the cyclic reports; a USB reset at that point was followed by a hang that only a power cycle cleared. Pace writes; do not reset automatically | wire |
| one owner | hidraw delivers every IN report to every open fd. Only one process may send requests; the others would steal or confuse replies | design |
| latency | reply ~28 ms after request; back-to-back requests at 4 ms spacing all answered | wire |
| keepalive | none required | wire |
| standby | after `set_power 0`, gets are still answered and cyclic keeps flowing | wire |

## 2. Message header

Four little-endian u32: `id, len, ext2, ext3`, then the payload.

| id | dir | use | tag |
|---|---|---|---|
| 0x74 | out | get; `len` = 16; ext2/ext3 select what | wire |
| 0x75 | in | get reply; echoes ext2 and ext3; `len` is always 16 (meaningless); body runs to the end of the report, zero-padded | wire |
| 0x70 | out | set; `len` = 16 + payload length; ext2 = ext3 = 0. **No reply** | wire |
| 0x73 | in | cyclic status + meters, every 8 ms; `len` = 0 | wire |
| 0x83 | in | cyclic AFX meters, every 8 ms, ~4 ms after 0x73 | wire |
| 0x00 | in | all-zero report (after reset, while clock unlocked). Ignore | wire |
| 0x61 | in | HID_ACK_UNKNOWN_CMD | wire |
| 0x1F75 | in | segment of a larger reply (`total, chunk_len, offset` in the header). Never seen: every known reply fits in 320 bytes. Handle it anyway | static |

Match a reply to its request by `id == 0x75 && ext2 == req.ext2 && ext3 == req.ext3`.
Keep only one request in flight. Timeout 500 ms.

## 3. Set payload encoding

`byte0 = pid | nparams << 6`. If the field bytes total n < 4: `nparams = n-1`
and the fields start at byte 1. Otherwise `nparams = 3`, `byte1 = n`, and the
fields start at byte 2. Multi-byte fields are little-endian. Bitfields are
packed LSB-first within a byte. **[wire]** (every set below checks out)

## 4. Gets (0x74)

| what | ext2/ext3 | reply body (after the 16-byte header) | tag |
|---|---|---|---|
| firmware | 0/0 | `major u8, minor u8` (1.50 = `01 32`) | wire |
| compat passport | 0/1 | same bytes as firmware | wire |
| passport | 1/0 | `name char[20], serial char[16], hw char[8], birth u32 (unix time)` | wire |
| registration | 2/0 | `u8` (1) | wire |
| routing | 3/bank (0..14) | `bank u8`, then 64 × `(group s8, chan u8)`; only the first `bank size` entries mean anything | wire |
| mixer | 4/m (0..3) | 33 × `(level u8, pan:6 mute:1 solo:1, send u8)`; entry 0 = master | wire |
| AFX configs | 7/AfxType | per-model; see sec. 10. **EQ is the exception: ext3 0 = inst 0-7, ext3 1 = inst 8-15**, 8 × 36 B | wire (EQ, compressor) / static (others) |
| AFX order | 9/0 | 16 chains × 8 slots × `(type u8, inst u8)`; empty slot = (0,0) | wire |
| reverb | 10/0 | 4 slots × 11 B (fields as in set_reverb); unused slot: mixer_id 0xFF | wire |
| mixer links | 11/3 | 64 × `u8`, index `mixer*16 + ch_pair`; 1 = linked (inferred) | wire |
| AFX links | 11/4 | 8 × `u8`, one per chain pair | wire |
| AFX instances | 12/0 | pairs `(type u8, count u8)` until type 0 | wire |
| HDX delays | 13/0 | 2 × `u8[32]` | wire |
| trims | 14/0 | 2 records × 66 B: `trim_id u8, control u8, 32 × (whole u8, fract u8)`: LINE_OUT (1), then LINE_IN (2). Monitor trim is in status | wire |
| **never** | 17/* | licensing / assignment. Do not implement | — |

## 5. Sets (0x70)

| cmd | pid | fields | example | tag |
|---|---|---|---|---|
| power | 3 | `on u8` | `03 00` standby, `03 01` on | wire |
| sample rate | 4 | `idx u8`: 0=32k 1=44.1k 2=48k 3=88.2k 4=96k 5=176.4k 6=192k | `04 03` | wire for 3/4, static for others |
| clock source | 5 | `idx u8`: 0 internal (Oven/Atomic), 1 W.C., 2 S/PDIF, 3 ADAT, 4 ADAT 2x, 5 ADAT 4x, 6 MADI, 7 MADI 2x, 8 MADI 4x, 9 HDX, 10 USB | `05 00` | wire for 0/4 |
| volume | 6 | `id u8` (0 main monitor, 1 line out), `atten u8` (dB, 0..96, 96 = -inf) | `46 01 02` | wire |
| mute | 7 | `id u8, mute u8` | `47 00 01` | wire (id 0) |
| preset save | 8 | `slot u8` 1..5 | `08 05` | wire |
| preset recall | 9 | `slot u8` 1..5 | `09 05` | wire |
| oscillator | 10 | `freq_l:2 freq_r:2 level:2 mute_l:1 mute_r:1`; freq 0=1 kHz 1=440 Hz; level 0/-6/-12/-18 dBFS | `0a a0` (1 kHz, −12 dBFS, R muted) | wire |
| brightness | 11 | `u8` 0..100 (25 read back as 26) | `0b 19` | wire |
| trim | 12 | `trim_id u8` (0 MONITOR, 1 LINE_OUT, 2 LINE_IN), `control u8` (0 ALL, 1 INDIVIDUAL), 32 × `(whole u8, fract u8)`; dBu = 24 − whole − fract/100 (inferred sign for fract) | `cc 42 01 01 09 00 08 00 …` | wire (LINE_OUT) |
| S/PDIF SRC | 13 | `on u8` | `0d 01` | wire |
| S-MUX | 14 | `id u8` (0 MADI in, 1 MADI out), `on u8` | `4e 00 01` | wire |
| meter source | 16 | `bank u8` (0 A, 1 B, 2 mixer), `src u8` (A/B: §8 list; mixer: mixer idx) | `50 02 03` | wire |
| mixer channel | 17 | `mixer u8, ch u8 (0 master, 1..32), level u8, pan:6 mute:1 solo:1, send u8` | `d1 05 03 01 1e 02 60` | wire |
| routing | 18 | `bank u8`, 32 × `(group s8, chan u8)`, unused entries = (0,0) | `d2 41 0e …` | wire |
| stereo link | 19 | `periph u8` (3 mixer, 4 AFX), `id u8` (mixer: `m*16 + (ch-1)/2`; AFX: chain/2), `linked u8` | `93 03 30 01` | wire (mixer) |
| AFX params | 20 | `type u8, inst u8, model fields…` | EQ: `d4 0a 01 0f 02 e8 03 47 00 2c 01 02` | wire (EQ, compressor) |
| AFX order | 21 | `chain u8`, 8 × `(type u8, inst u8)`, packed from slot 0 | `d5 11 0f 01 0f 00…` | wire |
| AFX bypass | 24 | `inst u8, type u8, enabled u8` | `98 0f 01 00` | wire |
| reverb | 26 | `mixer_id, room_size, color, predelay, density, early_ref_gain, late_ref_delay, richness, reverb_time, reverb_level, on` (u8 each) | `da 0b 00 44 0c 00 64 0c 74 19 17 19 00` | wire |
| HDX 27..31 | | not used by the Orion | | static, **don't ship** |
| predefined AFX preset | 25 | unused by the panel; numbering unknown | | **don't ship** |
| **never** | 1 | `set_config_feature` | | — |
| **never** | | 0x76 clear_update_flag, firmware/boot/flash (0x02, 0x06, 0x14, 0x40, 0x50, 0x52, 0xD4…) | | — |

Behaviours **[wire]**:
- A set changes state ~25 ms later. Confirm it by reading back (gets), or
  through status for status-backed fields.
- Removing an AFX instance from its chain sets its `enabled` to 0. Send a
  bypass `…01` after removal to keep it enabled.
- Any set clears status `current_preset` to 0. A recall sets
  `device_updated` (status byte 0 bit 7), which stays set because nobody
  sends 0x76. Treat a 0→1 edge as "full resync"; while it stays 1, use
  preset changes and status diffs instead.
- set_stereo_link changes only the link flag, not pans or levels.

## 6. Value semantics

| value | meaning | tag |
|---|---|---|
| mixer level, send | dB attenuation; 0 = 0 dB; level 95 and send ≥95 = -inf (send 96 seen) | wire (level steps) / static (-inf) |
| pan | 6-bit, 0..63 on the wire, centre 32, shown as value − 32; lower = left (2 = L30, 62 = R30). The panel knob covers 2..62 (the encoder refuses other channel pans) but master writes use 0 | wire (direction by ear) |
| trims | dBu = 24 − whole − fract/100; the encoder keeps to the panel's 10..24 dBu. The UI has two display ranges, 10..24 and 14..20 dBu; which trim uses which is unconfirmed (LINE IN ch20 holds 10 dBu) | wire for the formula |
| meters (0x73, 0x83) | raw = dB below full scale, 1 dB per step, 0 = clip, 96 = floor | wire (0x73 with oscillator); 0x83 floor 96 wire |
| reverb UI ranges | room, color, early, richness, time 0..100; predelay, late 0..150; level 1..100; density fixed 100; raw = UI value | static |
| brightness | 0..100 | wire (approximate) |
| AFX units | EQ: Hz, Q×100, dB×100; compressor: µs, ratio×100, dB×100, threshold −dB. Vintage models: raw knob positions | wire (EQ/comp) / static |

## 7. Status report 0x73 (payload offsets, after the 16-byte header)

| +p | field | tag |
|---|---|---|
| 0 | `preset:3` (0 = none/modified, 1..5), `power_on:1`, `muted:1` (meaning unknown, not mute_main), `usb_mode:2`, `device_updated:1` | wire (preset, power, updated) |
| 1 | flags: oven 0x01, locked 0x02, atomic 0x04, usb 0x08, madi 0x10, adat 0x20, spdif 0x40, dlink 0x80 | wire (locked, atomic, adat) |
| 2 | `rate_idx:4` (as in set sample rate), pull modes in the high bits | wire |
| 3 | sync source idx | wire |
| 4 | oscillator byte (as in set) | wire |
| 5..7 | external clock Hz = `[7] | [6]<<8 | [5]<<16` | wire |
| 8 | smux_in 0x01, smux_out 0x02, spdif_src 0x04 | wire |
| 9 | brightness | wire |
| 10, 11 | adat / madi available channels | static, values 8/32 seen |
| 12 | `main_volume:7, main_mute:1` | wire |
| 13 | `line_volume:7, line_mute:1` | wire |
| 14 | monitor trim (whole; dBu = 24 − v) | wire |
| 17..18 | spdif_gains | unknown |
| 19..21 | usb channels / usb3 max / usb2 max (64/64/8) | static |
| 22..24 | usb/stream status bits | static |
| 25..26 | fw_rev u16 | static |
| 27..55 | HDX / loop sync | static |
| 106..109 | meter bank sources (A, B, mixer, ?) | wire |
| 110..141 | meters bank A (32) | wire |
| 142..173 | meters bank B (32) | wire |
| 174..205 | meters, selected mixer (32 inputs) | wire |
| 222..225 | reverb in L/R, out L/R | static |

## 8. Numbering

Routing sources (`group` in routing entries): 0 LINE IN(32), 1 USB PLAY 1-32,
2 USB PLAY 33-64, 3 MADI IN 1-32, 4 MADI IN 33-64, 5 HDX 1 PLAY, 6 HDX 2 PLAY,
7 ADAT IN(16), 8 S/PDIF IN(2), 9 AFX OUT(16), 10..13 MIX L/R1..4 (2),
14 MUTE (chan 0), 15 OSCILLATOR (2). **[wire]**

Routing banks (outputs): 0 LINE OUT(32), 1 MONITOR(2), 2 USB REC 1-32,
3 USB REC 33-64, 4 MADI OUT 1-32, 5 MADI OUT 33-64, 6 HDX 1 REC, 7 HDX 2 REC,
8 ADAT OUT(16), 9 S/PDIF OUT(2), 10 AFX IN(16), 11..14 MIX CH1..4 (32).
**[wire]**

Meter bank A/B sources: 0..13 = routing sources 0..13, 14 LINE OUT,
15 MONITOR, 16/17 USB REC, 18/19 MADI OUT, 20/21 HDX REC, 22 ADAT OUT,
23 S/PDIF OUT, 24 AFX IN, 25..28 MIX CH1..4. **[wire for 0, 7, 22]**

ADAT channel counts depend on the rate (16 at 1x; `adat_avail_channels` = 8
at 96 kHz). MADI depends on S-MUX. Use the status fields (+10, +11).

## 9. Clock-source safety (not protocol)

- An external clock source can fail to lock, e.g. when the other device
  clocks from the Orion (a loop). The CLI waits for lock and offers to revert interactively, or reverts
  noninteractively, unless `--no-revert` is set. The GUI offers keep/revert
  handling. These are client behaviors, not a firmware guarantee.

## 10. AFX models

- **Model ids**: `AfxType` in `crates/openorion-proto/src/model.rs` (ids 1..59;
  10, 40 and 49 unused). `AfxType::on_orion` lists the models the Orion
  runs; never send the others. [static]
- **Get** (ext2 7, ext3 = model id; EQ split as in sec. 4): one entry per
  instance, indexed by instance id. Each entry is `enabled u8` (1 = not
  bypassed) followed by the set fields. [wire for EQ, compressor, PowerGate]
- **Set**: `type_id u8, inst_id u8`, then the same fields in the same
  order. [wire for EQ, compressor; static for the rest]
- **Field tables**: ClearQ (EQ) and PowerFFC (compressor) are the
  `EqParams` / `CompressorParams` structs in `model.rs`. Every other model
  is carried raw: `afx_fields` in `crates/openorion-proto/src/names.rs` gives
  each field's name, width, signedness, range and default. [static, the
  PowerGate layout wire]
- **Raw values** are control positions, not physical units, except for
  ClearQ, PowerFFC and PowerGate. The mapping to dB/Hz of the vintage
  models is **unknown**. [static]
- `level` fields are s8, -24..+12. Some fields with negative ranges sit in
  u8 slots; decode them as s8 (`AfxField::signed`). [static]
- FET-A76 (id 9): the vendor panel always sends `linked = 0`. Do
  the same. [static]
- **ClearQ panel limits** per band (0..4): freq 20..800, 20..800,
  125..8000, 400..20000, 400..20000 Hz; filter type band 0 low shelf or
  HPF, bands 1-3 peak, band 4 high shelf or LPF; Q×100 50..1800 on
  bands 1-3, bands 0 and 4 have no Q control (the panel sends 0, the
  device default is 71); gain×100 -2400..+1200. [static]
- **PowerFFC panel limits**: `taw` (detector) 65535 = peak, or RMS
  window × 100: 2000, 5000, 10000, 15000, 20000; `ctrl` always 0.
  The encoder refuses anything else. [static]
- **0x83 AFX meters**: `peak[16 chains][8 slots]` then
  `gain_reduction[16][8]`, 256 B, slot k = the k-th effect in the chain's
  order. Peak raw = dB below full scale, 0 = clip, ≥ 60 = floor. Vintage
  compressor GR raw = dB × 4. [peak scale wire; GR static]
