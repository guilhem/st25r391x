# Implementation provenance

This is original Rust code written from the chip and tag documentation below.
It is not a translation of the former C Linux driver. Prior exposure exists:
the GPL C fork `guilhem/st25r391x`, based on `pguyot/st25r391x`, was inspected
at committed main SHA `ac5c3229d46f813f3495213f022f4922f465950d` to inventory
implemented behavior. This work is **not claimed to be clean-room**. No C
source, identifiers beyond hardware/protocol terminology, comments, or tables
were imported. Rust history starts from an independent MIT orphan; the root
LICENSE is retained. License/provenance assertions do not constitute hardware proof.

Contribution record: Guilhem selected the hardware/protocol scope, independent
library approach, MIT licensing for the newly generated work, API constraints
and qualification requirements. Codex generated the original Rust implementation,
tests, examples and documentation from specifications, after earlier exposure to
GPL histories solely for behavior inventory. Guilhem/parent owns integration and
final dependency revision pins. Existing GPL source roots and their copyright and
license rights remain unchanged; this MIT license covers this independently
written history, not the earlier C works. See the GNU FAQ on
[translations](https://www.gnu.org/licenses/gpl-faq.html#TranslateCode): translating
existing covered code does not remove its license obligations. No such translation
is asserted or intended here.

Specification references consulted on 2026-10-05:

- ST [ST25R3916](https://www.st.com/resource/en/datasheet/st25r3916.pdf)
  and [ST25R3917](https://www.st.com/resource/en/datasheet/st25r3917.pdf):
  both resolve to DS12484 **Rev 8, May 2023**, combined ST25R3916/7 datasheet.
  §4.3.4/table 11 and figures 21–27: I2C address, modes, combined reads,
  bank B until STOP. §4.4/table 13: commands, oscillator requirement,
  STOP semantics and non-chainable commands; §4.4.10: reg_s toggle and
  maximum 5 ms adjustment. §4.5.1–.17: supply, modulation, parity and analog
  receiver encodings; §4.5.22–.24: NRT; §4.5.34–.43: IRQ, FIFO, collision,
  TX byte/bit count; §4.5.46–.57: AAT, drive and regulator controls;
  §4.5.62/.80: status and identity. Physical PDF pages 50–58, 60–66,
  71–86, 90–92, 98–105, 108–121, 125, 135 (printed numbering).
- ST [ST25TB512-AT](https://www.st.com/resource/en/datasheet/st25tb512-at.pdf),
  **DS11456 Rev 9, April 2026**: §4 EEPROM/counters/OTP restrictions,
  §8.1/.4/.7/.8/.9 INITIATE, SELECT, READ_BLOCK, WRITE_BLOCK, GET_UID;
  §10/table 13 programming timings. UID and 32-bit block data transmitted least
  significant byte first; CRC low byte first. Programming hold is conservative
  10 ms (covers the 7 ms maximum counter programming interval).
- NXP [AN10834](https://www.nxp.com/docs/en/application-note/AN10834.pdf),
  **Rev 4.2, 10 August 2021**, UID handling and NFC-A cascade selection.
- NXP [AN10833](https://www.nxp.com/docs/en/application-note/AN10833.pdf),
  **Rev 3.9, 15 December 2025**, ATQA/SAK interpretation; product hints
  are not authentication or identification guarantees.

NFC-B commands use the ISO/IEC 14443-3 Type B framing described by the chip
and the standard REQB/ATQB/ATTRIB wire protocol. No paywalled ISO edition
was obtained here: official protocol conformance remains a qualification gap,
particularly FWI timing and multi-tag slot selection. The implemented choice is
106 kbit/s, one slot, CID 0, FSDI 8, no bit-rate negotiation.

Important qualification uncertainty: DS12484 does not comprehensively illustrate
FIFO packing of all partially transmitted NFC-A collision responses. The port
interprets response bits LSB first, collision counters relative to received data,
and advances the NVB prefix. Scripted tests verify this assumption; analyzer
traces with colliding real tags must confirm it. No connected ST25R3916/7 was
available. Scripted transport tests prove software transactions, not analog RF,
tag interoperability, regulator calibration quality, or kernel timing.
