# Contributing

Bug reports, captures of what your own device does, and patches are
welcome. To keep the project clean, every contribution must follow these
rules.

## Do

- Work from observing your **own** device: USB captures (usbmon,
  Wireshark), the device's replies, and its behaviour.
- Describe new protocol facts in `docs/protocol-spec.md` in your own
  words, tagged [wire] (seen on the wire) or [unknown].
- Record how each new protocol fact or fixture was obtained, including
  the relevant device and software versions, and what was sanitized.
  Preserve existing [static] provenance labels; later observation can
  corroborate a fact without erasing its origin. Discuss proposed static
  analysis contributions with the maintainers, including their source
  and why the information is necessary for interoperability. Do not
  submit vendor code or analysis dumps.
- Keep tests on the simulator (`openorion-sim`). Hardware tests stay
  `#[ignore]`d and must leave the device as they found it.

## Don't

- Don't submit code, binaries, firmware, images, sound files, manuals,
  screenshots or text copied from the vendor's software or documentation.
- Don't submit decompiled or disassembled vendor code, or excerpts of it.
- Don't add firmware-update or bootloader commands, activation or
  licensing writes, device-assignment writes or factory reset. Don't
  add features intended to evade activation or licensing restrictions.
- Don't include personal data in captures or fixtures: serial numbers,
  host names, user names, network addresses. Replace serials with zeros.

## Checks

```
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
```

By contributing you agree to license your work under GPL-3.0-or-later.
