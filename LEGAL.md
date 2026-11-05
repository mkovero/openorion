# Legal notes

This is not legal advice. It records how the project was made and what it
deliberately does not do.

## Independence

openorion is an independent program written to interoperate with
hardware its authors own. It is not affiliated with, endorsed by or
supported by Antelope Audio. Trademarks are used only to identify the
hardware it works with.

The project's purpose is to let owners operate their audio interface on
Linux without needing a Windows or macOS computer for device control.
It implements the control protocol independently; audio processing and
the onboard effects remain on the device.

## How the protocol was established

- **Observation.** Most of the protocol was established by watching the
  USB traffic between a computer and the authors' own device and testing
  how the device responds. Items found this way are tagged **[wire]** in
  `docs/protocol-spec.md`.
- **Interoperability analysis.** Some details come from analysing how the
  vendor's software talks to the device, solely to make this independent
  program interoperate with the hardware. They are tagged **[static]**.
- **What is published.** The protocol documentation describes message
  formats, field layouts and values used for interoperability, rather
  than reproducing the vendor's implementation.

## Legal context

[Directive 2009/24/EC, Articles 5(3), 6 and 8(2)](https://eur-lex.europa.eu/legal-content/EN/ALL/?uri=CELEX%3A32009L0024)
provides for studying a program during authorized use and, subject to
conditions, decompilation necessary for interoperability. Those conditions
include a right to use the software, information not already readily
available, and analysis limited to what interoperability requires.
Disclosure of information obtained under Article 6 is also limited to
what is necessary for interoperability. Contractual provisions contrary
to these exceptions are null and void under Article 8(2).

In [C-406/10, *SAS Institute*](https://eur-lex.europa.eu/legal-content/EN/TXT/PDF/?uri=celex%3A62010CJ0406),
the CJEU distinguished program functionality, programming languages and
data-file formats from expression protected by copyright in a program.
This does not authorize copying protected code or documentation.

These rules support independent interoperability work; they are not a
blanket exemption for every method of analysis or publication. Their
application depends on the facts and applicable national law.

## What this repository does not contain

- No code, binaries, firmware, images, sounds, manuals or other files of
  the vendor, and no decompiled or disassembled vendor code.
- No screenshots of the vendor's software.

## What the software does not do

The protocol encoder permits only defined read requests and an allowlist
of setting commands. It excludes firmware-update, bootloader and flash
commands, licensing or assignment writes, configuration-feature writes,
and factory reset (`crates/openorion-proto`; see `tests/safety.rs`).

The project is intended to control hardware and features the user is
entitled to use. It does not implement an activation service or issue
licenses. These command restrictions are not a claim that the project
has verified how every firmware version enforces effect entitlements.
Features intended to evade activation or licensing restrictions are
outside the project's scope.

## Contributions

See [CONTRIBUTING.md](CONTRIBUTING.md). Contributions that would break any
of the above are not accepted.
