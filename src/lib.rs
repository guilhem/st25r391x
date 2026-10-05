//! Synchronous Linux userspace I²C driver for ST25R3916/7, without an IRQ pin.
//! See README and PROVENANCE for RF qualification, ownership and timing limits.
mod protocols;
mod types;
use i2cdev::{
    core::{I2CMessage, I2CTransfer},
    linux::{LinuxI2CError, LinuxI2CMessage},
};
use std::{
    thread,
    time::{Duration, Instant},
};
pub use types::*;

pub const I2C_ADDRESS: u16 = 0x50;
pub const FIFO_CAPACITY: usize = 512;

/// Narrow scripted-test seam. Production uses i2cdev directly: one write message
/// or write + repeated START + read, and a final STOP. Returns message count.
/// Implementations must not retry internally or manufacture partial success.
pub trait Device {
    fn transfer(
        &mut self,
        write: &[u8],
        read: Option<&mut [u8]>,
    ) -> std::result::Result<u32, LinuxI2CError>;
}
impl<T> Device for T
where
    T: for<'a> I2CTransfer<'a, Message = LinuxI2CMessage<'a>, Error = LinuxI2CError>,
{
    fn transfer(
        &mut self,
        write: &[u8],
        read: Option<&mut [u8]>,
    ) -> std::result::Result<u32, LinuxI2CError> {
        if let Some(read) = read {
            let mut messages = [
                LinuxI2CMessage::write(write).with_address(I2C_ADDRESS),
                LinuxI2CMessage::read(read).with_address(I2C_ADDRESS),
            ];
            I2CTransfer::transfer(self, &mut messages)
        } else {
            I2CTransfer::transfer(
                self,
                &mut [LinuxI2CMessage::write(write).with_address(I2C_ADDRESS)],
            )
        }
    }
}

/// Own this reader and its address exclusively. All RF operations use &mut self.
/// No initialization, bus access or RF mutation occurs in `new`.
pub struct St25r391x<D: Device> {
    device: D,
    settings: Settings,
    initialized: bool,
    poisoned: bool,
    field: FieldState,
    technology: Option<Technology>,
    irqs: [u8; 4],
    stage: Stage,
    changed: bool,
    transmission: Transmission,
    /// Neither cleanup nor reset may interrupt these intervals.
    hold_until: Instant,
    busy_until: Instant,
}
impl<D: Device> St25r391x<D> {
    pub fn new(device: D) -> Self {
        let now = Instant::now();
        Self {
            device,
            settings: Settings::default(),
            initialized: false,
            poisoned: false,
            field: FieldState::Unknown,
            technology: None,
            irqs: [0; 4],
            stage: Stage::Validation,
            changed: false,
            transmission: Transmission::NotStarted,
            hold_until: now,
            busy_until: now,
        }
    }
    pub fn field_state(&self) -> FieldState {
        self.field
    }
    pub fn is_poisoned(&self) -> bool {
        self.poisoned
    }
    /// Identify the chip without reset, configuration, RF changes or cleanup.
    /// A fresh reader can be discarded after this probe without Drop bus I/O.
    pub fn probe(&mut self, timeout: Duration) -> Result<Identity> {
        self.stage = Stage::Identity;
        self.changed = false;
        self.transmission = Transmission::NotStarted;
        let deadline = self.deadline(timeout)?;
        self.identity(deadline)
    }

    fn identity(&mut self, deadline: Instant) -> Result<Identity> {
        let raw = self.reg(0x3f, deadline)?;
        let identity = Identity {
            type_code: raw >> 3,
            revision: raw & 7,
        };
        if identity.type_code != 5 {
            return Err(self.error(ErrorKind::UnexpectedIdentity(raw)));
        }
        Ok(identity)
    }
    fn error(&self, kind: ErrorKind) -> Error {
        Error {
            kind,
            stage: self.stage,
            possible_changes: self.changed,
            transmission: self.transmission,
            field: self.field,
            cleanup: None,
            deadline_source: None,
        }
    }
    fn start(&mut self, timeout: Duration) -> Result<Instant> {
        self.stage = Stage::Validation;
        self.changed = false;
        self.transmission = Transmission::NotStarted;
        if self.poisoned {
            return Err(self.error(ErrorKind::Poisoned));
        }
        if !self.initialized {
            return Err(self.error(ErrorKind::NotInitialized));
        }
        self.deadline(timeout)
    }
    fn deadline(&self, timeout: Duration) -> Result<Instant> {
        if timeout.is_zero() || timeout > Duration::from_secs(60) {
            return Err(self.error(ErrorKind::InvalidArgument(
                "operation timeout must be >0 and <=60 seconds",
            )));
        }
        Ok(Instant::now() + timeout)
    }
    fn check(&self, deadline: Instant) -> Result<()> {
        if Instant::now() >= deadline {
            Err(self.error(ErrorKind::Deadline))
        } else {
            Ok(())
        }
    }
    fn pause(&self, deadline: Instant) -> Result<()> {
        self.check(deadline)?;
        thread::sleep(
            self.settings
                .poll_interval
                .min(deadline.saturating_duration_since(Instant::now())),
        );
        self.check(deadline)
    }
    fn wait_protected(&self) {
        let until = self.hold_until.max(self.busy_until);
        while let Some(left) = until.checked_duration_since(Instant::now()) {
            thread::sleep(left);
        }
    }
    fn write(&mut self, bytes: &[u8], deadline: Instant) -> Result<()> {
        self.check(deadline)?;
        self.changed = true;
        match self.device.transfer(bytes, None) {
            Ok(1) => self.check(deadline),
            Ok(completed) => Err(self.error(ErrorKind::ShortTransfer {
                expected: 1,
                completed,
            })),
            Err(e) => Err(self.error(ErrorKind::Transport(e))),
        }
    }
    // Ordinary registers alone are repeatable. IRQ/FIFO failures can consume data.
    fn read(
        &mut self,
        prefix: &[u8],
        out: &mut [u8],
        repeatable: bool,
        deadline: Instant,
    ) -> Result<()> {
        let mut last_error = None;
        loop {
            if let Err(mut expired) = self.check(deadline) {
                expired.deadline_source = last_error;
                return Err(expired);
            }
            if !repeatable {
                self.changed = true;
            }
            match self.device.transfer(prefix, Some(out)) {
                Ok(2) => {
                    return self.check(deadline).map_err(|mut expired| {
                        expired.deadline_source = last_error;
                        expired
                    })
                }
                Ok(completed) => {
                    return Err(self.error(ErrorKind::ShortTransfer {
                        expected: 2,
                        completed,
                    }))
                }
                Err(e) if repeatable && temporarily_unavailable(&e) => {
                    if let Err(mut expired) = self.pause(deadline) {
                        expired.deadline_source = Some(e);
                        return Err(expired);
                    }
                    last_error = Some(e);
                }
                Err(e) => return Err(self.error(ErrorKind::Transport(e))),
            }
        }
    }
    fn reg(&mut self, address: u8, deadline: Instant) -> Result<u8> {
        let mut data = [0];
        self.read(&[0x40 | address], &mut data, true, deadline)?;
        Ok(data[0])
    }
    fn set_reg(&mut self, address: u8, value: u8, deadline: Instant) -> Result<()> {
        self.write(&[address, value], deadline)
    }
    fn verify(&mut self, address: u8, expected: u8, deadline: Instant) -> Result<()> {
        let actual = self.reg(address, deadline)?;
        if actual == expected {
            Ok(())
        } else {
            Err(self.error(ErrorKind::Verification {
                register: address,
                expected,
                actual,
            }))
        }
    }
    fn bank_b(&mut self, address: u8, bytes: &[u8], deadline: Instant) -> Result<()> {
        let mut data = Vec::with_capacity(bytes.len() + 2);
        data.extend_from_slice(&[0xfb, address]);
        data.extend_from_slice(bytes);
        self.write(&data, deadline)
    }
    fn command(&mut self, command: u8, deadline: Instant) -> Result<()> {
        self.write(&[command], deadline)
    }
    fn irq_update(&mut self, deadline: Instant) -> Result<()> {
        let mut flags = [0; 4];
        self.read(&[0x5a], &mut flags, false, deadline)?;
        for (cached, fresh) in self.irqs.iter_mut().zip(flags) {
            *cached |= fresh;
        }
        Ok(())
    }
    fn irq_clear(&mut self, deadline: Instant) -> Result<()> {
        self.irq_update(deadline)?;
        self.irqs = [0; 4];
        Ok(())
    }
    fn wait_irq(&mut self, index: usize, mask: u8, deadline: Instant) -> Result<u8> {
        loop {
            self.check(deadline)?;
            let flags = self.irqs[index] & mask;
            if flags != 0 {
                self.irqs[index] &= !flags;
                return Ok(flags);
            }
            self.irq_update(deadline)?;
            if self.irqs[index] & mask == 0 {
                self.pause(deadline)?;
            }
        }
    }
    fn failure<T>(&mut self, mut error: Error) -> Result<T> {
        self.poisoned = true;
        // Recovery is never an exchange retry. Preserve RF power for programming.
        self.wait_protected();
        let deadline = Instant::now() + self.settings.cleanup_timeout;
        if let Err(cleanup) = self.shutdown_inner(deadline) {
            error.cleanup = Some(Box::new(cleanup));
        }
        error.field = self.field;
        Err(error)
    }
    /// Resets the IC and clears poisoning only after complete, verified success.
    /// Protected RF programming/busy intervals are honored before reset.
    pub fn initialize(&mut self, settings: Settings, timeout: Duration) -> Result<Identity> {
        self.stage = Stage::Validation;
        self.changed = false;
        self.transmission = Transmission::NotStarted;
        validate_settings(&settings).map_err(|s| self.error(ErrorKind::InvalidArgument(s)))?;
        let deadline = self.deadline(timeout)?;
        self.wait_protected();
        self.settings = settings;
        self.initialized = false;
        self.poisoned = true;
        let result = self.initialize_inner(deadline);
        match result {
            Ok(id) => {
                self.initialized = true;
                self.poisoned = false;
                Ok(id)
            }
            Err(e) => self.failure(e),
        }
    }
    fn initialize_inner(&mut self, deadline: Instant) -> Result<Identity> {
        self.stage = Stage::Reset;
        self.field = FieldState::Unknown;
        self.command(0xc0, deadline)?;
        // Operation control is reset only by power-up (DS12484 table 21).
        self.set_reg(0x02, 0, deadline)?;
        self.verify(0x02, 0, deadline)?;
        self.field = FieldState::Off;
        self.technology = None;
        self.irqs = [0; 4];
        self.stage = Stage::Identity;
        let identity = self.identity(deadline)?;
        self.stage = Stage::Configure;
        // MCU clock disabled, low-frequency output disabled; supply set by caller.
        self.set_reg(0x00, 0x07, deadline)?;
        self.set_reg(
            0x01,
            if self.settings.supply_3v { 0x84 } else { 4 },
            deadline,
        )?;
        if let Some(tune) = self.settings.antenna_tuning {
            self.set_reg(
                0x01,
                if self.settings.supply_3v { 0xa4 } else { 0x24 },
                deadline,
            )?;
            self.write(&[0x26, tune[0], tune[1]], deadline)?;
        }
        self.write(&[0x16, 0xff, 0xff, 0xff, 0xff], deadline)?;
        self.oscillator(deadline)?;
        self.adjust_regulators(deadline)?;
        self.shutdown_inner(deadline)?;
        Ok(identity)
    }
    fn oscillator(&mut self, deadline: Instant) -> Result<()> {
        self.stage = Stage::Oscillator;
        self.irq_clear(deadline)?;
        self.set_reg(0x02, 0x81, deadline)?;
        self.wait_irq(0, 0x80, deadline)?;
        if self.reg(0x31, deadline)? & 0x10 == 0 {
            return Err(self.error(ErrorKind::InvalidResponse("oscillator not stable")));
        }
        Ok(())
    }
    fn adjust_regulators(&mut self, deadline: Instant) -> Result<()> {
        self.stage = Stage::Regulators;
        self.set_reg(0x2c, 0x80, deadline)?;
        self.set_reg(0x2c, 0x00, deadline)?;
        self.irq_clear(deadline)?;
        self.check(deadline)?;
        // Non-chainable command: no I2C accesses until its specified maximum.
        self.busy_until = Instant::now() + Duration::from_millis(5);
        let result = self.command(0xd6, deadline);
        self.busy_until = Instant::now() + Duration::from_millis(5);
        self.wait_protected();
        result?;
        self.wait_irq(1, 0x80, deadline)?;
        Ok(())
    }
    /// Re-run the regulator adjustment, preserving any programming hold first.
    /// This shuts down the field and returns with the chip verified off.
    pub fn calibrate_regulators(&mut self, timeout: Duration) -> Result<()> {
        let deadline = self.start(timeout)?;
        self.wait_protected();
        let result = (|| {
            self.shutdown_inner(deadline)?;
            self.oscillator(deadline)?;
            self.adjust_regulators(deadline)?;
            self.shutdown_inner(deadline)
        })();
        match result {
            Ok(()) => Ok(()),
            Err(e) => self.failure(e),
        }
    }
    /// Select an RF technology and enable a field after collision avoidance.
    /// Idempotent only if the same technology is already active.
    pub fn field_on(&mut self, technology: Technology, timeout: Duration) -> Result<()> {
        let deadline = self.start(timeout)?;
        supported(technology).map_err(|kind| self.error(kind))?;
        let result = self.field_on_inner(technology, deadline);
        match result {
            Ok(()) => Ok(()),
            Err(e) => self.failure(e),
        }
    }
    fn field_on_inner(&mut self, technology: Technology, deadline: Instant) -> Result<()> {
        self.check(deadline)?;
        if self.field == FieldState::On && self.technology == Some(technology) {
            return Ok(());
        }
        if self.field != FieldState::Off {
            self.wait_protected();
            self.shutdown_inner(deadline)?;
        }
        self.oscillator(deadline)?;
        self.adjust_regulators(deadline)?;
        self.stage = Stage::Configure;
        let is_a = technology == Technology::NfcA;
        self.write(&[0x03, if is_a { 0x08 } else { 0x14 }, 0x00], deadline)?;
        self.set_reg(0x05, 0, deadline)?;
        self.write(&[0x06, 0, 0], deadline)?;
        self.set_reg(0x0a, 0, deadline)?;
        self.set_reg(
            0x28,
            self.settings.modulation << 4 | self.settings.driver_resistance,
            deadline,
        )?;
        let receiver = if is_a {
            self.settings.receiver_a
        } else {
            self.settings.receiver_b
        };
        self.write(
            &[0x0b, receiver[0], receiver[1], receiver[2], receiver[3]],
            deadline,
        )?;
        let correlator = if is_a {
            self.settings.correlator_a
        } else {
            self.settings.correlator_b
        };
        self.bank_b(0x0c, &correlator, deadline)?;
        self.command(0xd5, deadline)?;
        self.bank_b(0x15, &[33], deadline)?;
        self.stage = Stage::Field;
        self.irq_clear(deadline)?;
        self.field = FieldState::Unknown;
        self.command(0xc8, deadline)?;
        let flags = self.wait_irq(1, 0x06, deadline)?;
        if flags & 0x04 != 0 {
            return Err(self.error(ErrorKind::Collision));
        }
        // CAT asserts after chip guard time. Also check actual enable bits.
        self.set_reg(0x02, 0xc9, deadline)?;
        self.verify(0x02, 0xc9, deadline)?;
        self.field = FieldState::On;
        self.technology = Some(technology);
        Ok(())
    }
    /// Wait for protected RF intervals, then explicitly verify the chip off.
    /// `timeout` starts AFTER that mandatory wait; it cannot cut programming.
    /// On success poisoning remains until successful initialize.
    pub fn shutdown(&mut self, timeout: Duration) -> Result<()> {
        self.stage = Stage::Validation;
        self.deadline(timeout)?;
        self.wait_protected();
        let deadline = Instant::now() + timeout;
        match self.shutdown_inner(deadline) {
            Ok(()) => Ok(()),
            Err(e) => {
                self.poisoned = true;
                Err(e)
            }
        }
    }
    fn shutdown_inner(&mut self, deadline: Instant) -> Result<()> {
        self.stage = Stage::Shutdown;
        self.field = FieldState::Unknown;
        self.command(0xc2, deadline)?;
        self.set_reg(0x02, 0, deadline)?;
        self.verify(0x02, 0, deadline)?;
        if self.reg(0x31, deadline)? & 0x20 != 0 {
            return Err(self.error(ErrorKind::InvalidResponse("transmitter still active")));
        }
        self.field = FieldState::Off;
        self.technology = None;
        self.irqs = [0; 4];
        Ok(())
    }
    /// Byte-oriented raw exchange. RX includes CRC when enabled.
    pub fn exchange(
        &mut self,
        tx: &[u8],
        rx: &mut [u8],
        options: FrameOptions,
        timeout: Duration,
    ) -> Result<FrameResult> {
        self.exchange_bits(tx, tx.len().saturating_mul(8), rx, options, timeout)
    }
    /// Partial bytes use their low bits. Zero bits means receive-only.
    pub fn exchange_bits(
        &mut self,
        tx: &[u8],
        bits: usize,
        rx: &mut [u8],
        options: FrameOptions,
        timeout: Duration,
    ) -> Result<FrameResult> {
        let deadline = self.start(timeout)?;
        self.validate_frame(tx, bits, &options)?;
        if self.field != FieldState::On {
            return Err(self.error(ErrorKind::FieldOff));
        }
        let result = self.frame(tx, bits, rx, options, None, false, deadline);
        match result {
            Ok(frame) => Ok(frame),
            Err(e) => self.failure(e),
        }
    }
    fn validate_frame(&self, tx: &[u8], bits: usize, options: &FrameOptions) -> Result<()> {
        if options.tx_only && bits == 0 {
            return Err(self.error(ErrorKind::InvalidArgument("receive-only cannot be TX-only")));
        }
        if self.technology != Some(Technology::NfcA) && (!options.tx_parity || !options.rx_parity) {
            return Err(self.error(ErrorKind::InvalidArgument(
                "parity controls apply to NFC-A only",
            )));
        }
        if !options.tx_parity && options.tx_crc || !options.rx_parity && options.rx_crc {
            return Err(self.error(ErrorKind::InvalidArgument(
                "raw parity requires CRC disabled",
            )));
        }
        let bytes = bits.div_ceil(8);
        if bits > FIFO_CAPACITY * 8 || bytes > tx.len() {
            return Err(self.error(ErrorKind::InvalidArgument(
                "TX bit count exceeds buffer or FIFO",
            )));
        }
        if !bits.is_multiple_of(8) && self.technology != Some(Technology::NfcA) {
            return Err(self.error(ErrorKind::InvalidArgument("partial frames require NFC-A")));
        }
        if !bits.is_multiple_of(8) && options.tx_crc {
            return Err(self.error(ErrorKind::InvalidArgument("CRC requires byte-aligned TX")));
        }
        if options.response_timeout.is_zero()
            || options.response_timeout > Duration::from_secs(19)
            || options.field_hold > Duration::from_secs(60)
        {
            return Err(self.error(ErrorKind::InvalidArgument(
                "response timeout must be >0 and <=19s; hold <=60s",
            )));
        }
        Ok(())
    }
    fn nrt(&mut self, timeout: Duration, deadline: Instant) -> Result<()> {
        // ceil(timeout * 13.56MHz / step), without floats or zero-disable wrap.
        let cycles = timeout
            .as_nanos()
            .saturating_mul(13_560_000)
            .div_ceil(1_000_000_000);
        let (step, ticks) = if cycles.div_ceil(64) <= 65535 {
            (0, cycles.div_ceil(64))
        } else {
            (1, cycles.div_ceil(4096))
        };
        if ticks == 0 || ticks > 65535 {
            return Err(self.error(ErrorKind::InvalidArgument("NRT out of range")));
        }
        self.set_reg(0x12, step, deadline)?;
        self.write(&[0x10, (ticks >> 8) as u8, ticks as u8], deadline)
    }
    fn transmit_airtime(&self, bits: usize, options: &FrameOptions) -> Duration {
        let crc_bytes = if options.tx_crc { 2 } else { 0 };
        let characters = bits.div_ceil(8) + crc_bytes;
        let etu = if matches!(self.technology, Some(Technology::NfcB | Technology::St25tb)) {
            // DS11456 §3.1: start + eight data + stop = ten ETU per byte,
            // including CRC. DS12484 §4.5.7: max SOF 14, EOF 11, EGT 6 ETU.
            // Bound all supported settings, although field_on programs EGT=0.
            characters * 10 + characters.saturating_sub(1) * 6 + 14 + 11
        } else {
            // NFC-A parity applies to every complete byte, including CRC.
            // Reserve 64 ETU for framing/short commands and partial bytes.
            bits + crc_bytes * 8 + if options.tx_parity { characters } else { 0 } + 64
        };
        // The configured 106 kbit/s rate is exactly fc/128 (not 106000 Hz).
        Duration::from_nanos((etu as u64 * 128 * 1_000_000_000).div_ceil(13_560_000))
    }
    #[allow(clippy::too_many_arguments)] // Wire payload, framing mode and shared deadline.
    fn frame(
        &mut self,
        tx: &[u8],
        bits: usize,
        rx: &mut [u8],
        options: FrameOptions,
        special: Option<u8>,
        anticollision: bool,
        deadline: Instant,
    ) -> Result<FrameResult> {
        self.check(deadline)?;
        self.wait_protected();
        self.check(deadline)?;
        self.stage = Stage::PrepareFrame;
        self.command(0xc2, deadline)?;
        self.irqs = [0; 4];
        self.command(0xdb, deadline)?;
        self.set_reg(
            0x05,
            (if options.tx_parity { 0 } else { 0x80 })
                | (if options.rx_parity { 0 } else { 0x40 })
                | u8::from(anticollision),
            deadline,
        )?;
        self.set_reg(0x0a, if options.rx_crc { 0 } else { 0x80 }, deadline)?;
        self.nrt(options.response_timeout, deadline)?;
        let transmitting = bits != 0 || special.is_some();
        if bits != 0 && special.is_none() {
            self.write(&[0x22, (bits >> 8) as u8, bits as u8], deadline)?;
            let mut fifo = Vec::with_capacity(bits.div_ceil(8) + 1);
            fifo.push(0x80);
            fifo.extend_from_slice(&tx[..bits.div_ceil(8)]);
            self.write(&fifo, deadline)?;
        }
        self.irq_clear(deadline)?;
        if transmitting {
            self.stage = Stage::Transmit;
            self.check(deadline)?;
            // Start protection BEFORE ioctl: an errno does not prove no TX.
            let programming = self.technology == Some(Technology::St25tb)
                && bits >= 8
                && tx.first() == Some(&0x09);
            let hold = if programming {
                options.field_hold.max(self.settings.programming_hold)
            } else {
                options.field_hold
            };
            let air = self.transmit_airtime(bits, &options);
            self.hold_until = Instant::now() + air + hold;
            self.transmission = Transmission::PossiblyStarted;
            let sent = self.command(
                special.unwrap_or(if options.tx_crc { 0xc4 } else { 0xc5 }),
                deadline,
            );
            self.hold_until = Instant::now() + air + hold;
            sent?;
            self.wait_irq(0, 0x08, deadline)?;
            self.transmission = Transmission::Completed;
            // Completed TX confirms EOF; keep required post-TX power interval.
            self.hold_until = Instant::now() + hold;
            self.fifo_status(deadline)?;
        } else {
            self.command(0xd1, deadline)?;
            self.command(0xe3, deadline)?;
        }
        if options.tx_only {
            self.wait_protected();
            self.check(deadline)?;
            self.command(0xc2, deadline)?;
            return Ok(FrameResult {
                bytes: 0,
                bits: 0,
                outcome: Outcome::Transmitted,
            });
        }
        self.stage = Stage::Receive;
        loop {
            self.check(deadline)?;
            self.irq_update(deadline)?;
            self.frame_errors(&options, anticollision)?;
            if self.irqs[0] & 0x10 != 0 {
                break;
            }
            if self.irqs[1] & 0x40 != 0 {
                self.wait_protected();
                self.command(0xc2, deadline)?;
                if options.allow_no_response {
                    return Ok(FrameResult {
                        bytes: 0,
                        bits: 0,
                        outcome: Outcome::NoResponse,
                    });
                }
                return Err(self.error(ErrorKind::NoResponse));
            }
            self.pause(deadline)?;
        }
        let result = self.fifo_read(rx, &options, deadline)?;
        // Do not clear collision status: NFC-A consumes it before next command.
        self.command(0xe8, deadline)?;
        Ok(result)
    }
    fn frame_errors(&self, options: &FrameOptions, anticollision: bool) -> Result<()> {
        let errors = self.irqs[2];
        let kind = if errors & 0x80 != 0 && options.rx_crc {
            Some(ErrorKind::Crc)
        } else if errors & 0x40 != 0 && options.rx_parity {
            Some(ErrorKind::Parity)
        } else if errors & 0x30 != 0 {
            Some(ErrorKind::Framing)
        } else if self.irqs[0] & 0x04 != 0 && !anticollision {
            Some(ErrorKind::Collision)
        } else {
            None
        };
        if let Some(kind) = kind {
            Err(self.error(kind))
        } else {
            Ok(())
        }
    }
    fn fifo_status(&mut self, deadline: Instant) -> Result<[u8; 2]> {
        let mut status = [0; 2];
        self.read(&[0x5e], &mut status, true, deadline)?;
        let bytes = usize::from(status[0]) | (usize::from(status[1] & 0xc0) << 2);
        if bytes > FIFO_CAPACITY || status[1] & 0x10 != 0 {
            return Err(self.error(ErrorKind::FifoOverflow));
        }
        if status[1] & 0x20 != 0 {
            return Err(self.error(ErrorKind::FifoUnderflow));
        }
        Ok(status)
    }
    fn fifo_read(
        &mut self,
        rx: &mut [u8],
        options: &FrameOptions,
        deadline: Instant,
    ) -> Result<FrameResult> {
        let status = self.fifo_status(deadline)?;
        let bytes = usize::from(status[0]) | (usize::from(status[1] & 0xc0) << 2);
        let last_bits = usize::from((status[1] >> 1) & 7);
        if status[1] & 1 != 0 && options.rx_parity {
            return Err(self.error(ErrorKind::Parity));
        }
        if bytes == 0 || options.rx_crc && (bytes < 2 || last_bits != 0) {
            return Err(self.error(ErrorKind::InvalidResponse("empty or incomplete CRC frame")));
        }
        if bytes > rx.len() {
            return Err(self.error(ErrorKind::BufferTooSmall {
                needed: bytes,
                available: rx.len(),
            }));
        }
        self.read(&[0x9f], &mut rx[..bytes], false, deadline)?;
        let bits = if last_bits == 0 {
            bytes * 8
        } else {
            (bytes - 1) * 8 + last_bits
        };
        Ok(FrameResult {
            bytes,
            bits,
            outcome: Outcome::Received,
        })
    }
}
impl<D: Device> Drop for St25r391x<D> {
    fn drop(&mut self) {
        // Best effort only. Explicit shutdown is the only reportable guarantee.
        if self.field != FieldState::Off && (self.initialized || self.poisoned) {
            self.wait_protected();
            let _ = self.shutdown_inner(Instant::now() + self.settings.cleanup_timeout);
        }
    }
}
fn temporarily_unavailable(error: &LinuxI2CError) -> bool {
    match error {
        LinuxI2CError::Errno(n) => *n == 11,
        LinuxI2CError::Io(e) => e.kind() == std::io::ErrorKind::WouldBlock,
    }
}
fn supported(technology: Technology) -> std::result::Result<(), ErrorKind> {
    match technology {
        Technology::NfcA | Technology::NfcB | Technology::St25tb => Ok(()),
        _ => Err(ErrorKind::Unsupported(technology)),
    }
}
fn validate_settings(s: &Settings) -> std::result::Result<(), &'static str> {
    if s.driver_resistance > 14 || s.modulation > 15 {
        return Err("invalid TX drive/modulation encoding");
    }
    if s.poll_interval.is_zero()
        || s.poll_interval > Duration::from_millis(10)
        || s.cleanup_timeout < Duration::from_millis(10)
        || s.cleanup_timeout > Duration::from_secs(60)
        || s.programming_hold < Duration::from_millis(10)
        || s.programming_hold > Duration::from_secs(60)
    {
        return Err("invalid poll, cleanup or programming duration");
    }
    if s.receiver_a[2] & 3 != 0
        || s.receiver_b[2] & 3 != 0
        || s.receiver_a[3] >> 4 > 10
        || s.receiver_a[3] & 15 > 10
        || s.receiver_b[3] >> 4 > 10
        || s.receiver_b[3] & 15 > 10
    {
        return Err("receiver LF mode or invalid gain encoding");
    }
    Ok(())
}
#[cfg(test)]
mod tests;
