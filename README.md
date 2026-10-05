# st25r391x

Synchronous Rust 2021 I2C userspace library for ST25R3916/7. Version 0.1.0,
MIT, directly using i2cdev 0.6.2. Own a LinuxI2CDevice at address 0x50:

```rust,no_run
use i2cdev::linux::LinuxI2CDevice;
use st25r391x::{St25r391x, Settings, Technology, I2C_ADDRESS};
use std::time::Duration;
let mut reader = St25r391x::new(LinuxI2CDevice::new("/dev/i2c-1", I2C_ADDRESS)?);
reader.initialize(Settings::default(), Duration::from_secs(1))?;
let tag = reader.discover(Technology::St25tb, Duration::from_secs(1))?;
reader.shutdown(Duration::from_millis(100))?;
# Ok::<(), Box<dyn std::error::Error>>(())
```

`probe(timeout)` reads and validates the identity before initialization. It does
not reset, configure or shut down the chip, and does not poison the reader on
failure. Dropping a fresh probe candidate also performs no bus I/O. This allows
the caller to distinguish a reader at a shared address before choosing its driver.

Check actual board VDD before initialization: defaults select 3.3 V supply.
Settings expose supply, RF drive/modulation, per-technology receivers and
correlators, optional 3916 AAT DAC values, polling and programming hold.
Initialization resets, verifies identity, starts oscillator, adjusts regulators
and verifies field off. `calibrate_regulators` explicitly recalibrates with field
shutdown. AAT values are manual controls, not an automatic antenna optimizer.
3916/3917 share the identity code; the caller must know which chip/board it owns.

Capabilities:

- NFC-A: REQA/WUPA, bounded bit anticollision, 4/7/10 byte UID cascades,
  BCC, SELECT/SAK consistency (88h is UID data at CL3), RATS/ATS including
  the valid TL-only ATS for ISO-DEP-capable tags.
- NFC-B: one-slot REQB/WUPB, PUPI, ATQB and ATTRIB at 106 kbit/s,
  FSDI 8 and CID 0. ST25TB: RESET_TO_INVENTORY before INITIATE, chip-ID
  SELECT and GET_UID, allowing discovery/selection after an earlier activation.
- `discover` activates one tag; `select` verifies an expected UID/PUPI. Fields
  stay on after successful discovery/selection. A collisions choose zero unless
  selecting a known UID; B/TB collisions are reported, not enumerated.
- Byte and bit exchange, hardware CRC and NFC-A parity controls, TX-only,
  RX-only (empty TX), status errors and a 512-byte FIFO limit including RX CRC.
  Arrays preserve air byte order; partial bytes use low bits; disabling NFC-A
  RX parity puts parity bits in the raw received bit stream and disables CRC.

NFC-F and ISO15693 return Unsupported before bus mutation. No ISO-DEP chaining,
APDU, Crypto1, NFC-DEP, authentication, NDEF or application stack is provided.
Nab-hardware owns polling/events, applications, cancellation and D-Bus. No
/dev/nfc0 ABI or NabOS integration is included.

Each operation creates one monotonic deadline shared by all its steps, including
anticollision. It bounds driver-controlled polling/waits, **not a hard wall-clock
limit**: blocking I2C ioctl duration depends on the kernel/adapter timeout.
Mandatory programming and non-chainable calibration intervals, plus a fresh
cleanup budget, can extend an errored operation beyond its deadline. Hardware
NRT is separately configurable in FrameOptions; tag-specific FWI/application
waiting extensions require caller handling through raw exchange.

Register/FIFO reads are one combined write + repeated START + read ioctl, final
STOP. Bank B prefix FB shares the same write transaction. IRQ reads clear flags;
FIFO reads consume bytes. Neither is retried, including EAGAIN. Only ordinary
repeatable register reads retry EAGAIN; writes/commands/exchanges never replay.
If an ordinary-read retry reaches its deadline, ErrorKind::Deadline retains
its last Linux error in deadline_source and Error::source(), preserving errno
and error variant. Short message counts are errors.
Errors preserve Linux errno, stage, possible
changes, TX progress, field certainty and any cleanup error. Failed RF operations
poison the reader; only successful initialize clears poison. Explicit shutdown
waits required RF holds, STOPs activity, disables oscillator/transmitter and
verifies status; Drop is only best effort and can block.

Raw ST25TB opcode 09 is protected before and after the transmit ioctl, even if
it fails ambiguously or the deadline expires. Ambiguous NFC-B/ST25TB TX protection
covers ten-bit characters including CRC, SOF/EOF and maximum supported interchar
guards at fc/128, then adds the field hold; the bound is rearmed after ioctl returns.
TX-only waits for EOF and the hold
before stopping reception. Default general frame hold is 10 ms; callers must set
larger maximum programming times for other families. No software can guarantee
power through process kill, unplugging, another owner, or adapter failure.
Exclusive chip ownership is required; do not bind a kernel driver concurrently.

Run `cargo run --example detect` for detection, or `cargo run --example
st25tb_block -- read` to read selected ST25TB block 7. These examples do not write
by default. Physical qualification is **pending: no ST25R3916/7 hardware available**.

For reversible write qualification, use an owner-identified sacrificial
ST25TB512-AT with unlocked ordinary EEPROM block 7. Save its UID and original
block bytes externally with the read example, remove other tags, then explicitly
run `cargo run --example st25tb_block --
--qualify-write-ST25TB512-AT-sacrificial-block7`. The example toggles one bit,
reads back, restores and verifies the original. On an error, do not automatically
retry: diagnose progress/field state, reinitialize, select the same UID and inspect
block 7 before restoring manually. Never use counters 5/6 or OTP/system area 255
for this procedure; UID alone cannot identify model or prove unlock status.

Software validation: scripted exact I2C transactions, identity/reset, deadlines,
poisoning/cleanup, CRC/parity, FIFO boundaries, bit framing and protocol selection.
CI runs fmt, clippy, tests, examples, package, ARMv6
arm-unknown-linux-gnueabihf and ARM64 aarch64-unknown-linux-gnu checks.
CI is configured, not asserted to have run remotely. Hardware checklist pending:
logic-analyzer repeated START/bank selector, clear-on-read IRQ behavior, receiver
packing under partial-bit collisions, supply/antenna tuning, A/B/TB tags, RF guard
and programming hold on success/error/timeout, and verified shutdown.

[PROVENANCE.md](PROVENANCE.md) records exact specification revisions, previous
GPL C exposure and unresolved hardware/protocol qualification assumptions.
Import the tested code by its immutable Git revision:

```toml
[dependencies]
st25r391x = { git = "https://github.com/guilhem/st25r391x", rev = "a6086572f247975a9f2c98ba59c305febe8336a4" }
i2cdev = "=0.6.2"
```

Recorded checks and remaining physical qualification are in
[VALIDATION.md](VALIDATION.md). The small `integration/dual-reader` consumer in
[the CR14 repository](https://github.com/guilhem/cr14/tree/codex/i2c-userspace/integration/dual-reader)
imports both libraries together without opening a bus. This first delivery uses
`codex/i2c-userspace`, with a parentless root commit. Future Rust pull requests
use this new history as their base; the former GPL history retains its license.
