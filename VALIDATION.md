# Validation evidence

Recorded on 2026-10-05 for the code at
`a6086572f247975a9f2c98ba59c305febe8336a4`. This is software evidence;
physical ST25R3916/7 hardware was unavailable, confirmed by the owner.

- Rust 2021; formatting, strict Clippy, 28 scripted tests, runnable example
  builds and verified Cargo packaging passed locally.
- Official Rust 1.99.0 checked all targets (library, tests and examples) for
  ARMv6 `arm-unknown-linux-gnueabihf` with `target-cpu=arm1176jzf-s`, and
  ARM64 `aarch64-unknown-linux-gnu`. These checks do not execute ARM tests.
- The independent read-only implementation review found five defects. Changes
  `ae95095` and `a608657` fix Type B transmission safety bounds, ST25TB
  reselection, a cascade-level-three UID byte, minimum ATS length and deadline
  errors preserving Linux errno. Each has a runnable regression. The successful
  read that crosses a retry deadline also retains the last Linux error.
- Post-fix independent confirmation returned `ship` with no remaining findings
  for this code and CR14 `f134e898`. All 49 tests, formatting, strict Clippy and
  examples were independently rerun.
- A small consumer imports this crate and CR14 with the same concrete
  `LinuxI2CDevice`; native construction checks open no physical bus.

Detection, removal, A/B/ST25TB interoperability, actual IRQ clearing and FIFO
partial-collision packing, supply/antenna settings, physical RF timing, verified
field shutdown and reversible EEPROM write/readback/restoration remain pending.
Follow the tag-specific procedure in README only with an independently identified
sacrificial test tag and ordinary unlocked EEPROM region. No physical tag memory
was written by this implementation's validation.
