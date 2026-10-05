use i2cdev::linux::LinuxI2CError;
use std::{error, fmt, time::Duration};

/// The two ICs share their identity code; it does not distinguish 3916 from 3917.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Identity {
    pub type_code: u8,
    pub revision: u8,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Technology {
    NfcA,
    NfcB,
    St25tb,
    /// Not implemented by this library.
    NfcF,
    /// Not implemented by this library.
    Iso15693,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FieldState {
    Off,
    On,
    /// An operation failed and the field has not been verified off.
    Unknown,
}

/// Progress survives errors: a failed transmit ioctl may still have started RF TX.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Transmission {
    NotStarted,
    PossiblyStarted,
    Completed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stage {
    Validation,
    Reset,
    Identity,
    Configure,
    Oscillator,
    Regulators,
    Field,
    PrepareFrame,
    Transmit,
    Receive,
    Select,
    Shutdown,
}

#[derive(Debug)]
pub enum ErrorKind {
    Transport(LinuxI2CError),
    ShortTransfer {
        expected: u32,
        completed: u32,
    },
    Deadline,
    InvalidArgument(&'static str),
    Unsupported(Technology),
    NotInitialized,
    Poisoned,
    FieldOff,
    UnexpectedIdentity(u8),
    InvalidResponse(&'static str),
    TagMismatch,
    NoResponse,
    Collision,
    Crc,
    Parity,
    Framing,
    FifoOverflow,
    FifoUnderflow,
    BufferTooSmall {
        needed: usize,
        available: usize,
    },
    Verification {
        register: u8,
        expected: u8,
        actual: u8,
    },
}

/// No failed operation is replayed. `cleanup` retains a second failure without
/// replacing the original errno. Successful shutdown does not clear poisoning.
#[derive(Debug)]
pub struct Error {
    pub kind: ErrorKind,
    pub stage: Stage,
    pub possible_changes: bool,
    pub transmission: Transmission,
    pub field: FieldState,
    pub cleanup: Option<Box<Error>>,
}

impl Error {
    pub fn raw_os_error(&self) -> Option<i32> {
        match &self.kind {
            ErrorKind::Transport(LinuxI2CError::Errno(n)) => Some(*n),
            ErrorKind::Transport(LinuxI2CError::Io(e)) => e.raw_os_error(),
            _ => None,
        }
    }
}
impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{:?}: {:?} (TX {:?}, field {:?}, possible changes {})",
            self.stage, self.kind, self.transmission, self.field, self.possible_changes
        )?;
        if let Some(e) = &self.cleanup {
            write!(f, "; cleanup: {e}")?;
        }
        Ok(())
    }
}
impl error::Error for Error {
    fn source(&self) -> Option<&(dyn error::Error + 'static)> {
        match &self.kind {
            ErrorKind::Transport(e) => Some(e),
            _ => None,
        }
    }
}
pub type Result<T> = std::result::Result<T, Error>;

/// Supply and analog controls. Values are register encodings from DS12484,
/// not calibrated claims about a particular antenna. Reserved encodings rejected.
#[derive(Debug, Clone)]
pub struct Settings {
    /// true for 2.4..=3.6 V; false for >3.6..=5.5 V.
    pub supply_3v: bool,
    /// RFO driver resistance encoding, 0..=14; 15 (high impedance) excluded.
    pub driver_resistance: u8,
    /// ASK modulation index encoding, 0..=15.
    pub modulation: u8,
    /// Receiver registers 0B..0E, separately tunable for each RF mode.
    pub receiver_a: [u8; 4],
    pub receiver_b: [u8; 4],
    /// Bank B correlator registers 0C..0D.
    pub correlator_a: [u8; 2],
    pub correlator_b: [u8; 2],
    /// Optional ST25R3916 AAT DAC values. 3917 boards must leave this None.
    pub antenna_tuning: Option<[u8; 2]>,
    /// Polling interval for latched IRQs; not an RF timing guarantee.
    pub poll_interval: Duration,
    /// Fresh budget for verifiable shutdown after a failed operation.
    pub cleanup_timeout: Duration,
    /// Minimum field hold for ST25TB writes (>=10 ms, covers documented 7 ms).
    pub programming_hold: Duration,
}
impl Default for Settings {
    fn default() -> Self {
        Self {
            supply_3v: true,
            driver_resistance: 0,
            modulation: 7,
            // Datasheet reset receiver values; antenna qualification may tune them.
            receiver_a: [0x00, 0x2c, 0xd8, 0x00],
            receiver_b: [0x00, 0x2c, 0xd8, 0x00],
            correlator_a: [0x51, 0x00],
            correlator_b: [0x51, 0x00],
            antenna_tuning: None,
            poll_interval: Duration::from_micros(200),
            cleanup_timeout: Duration::from_millis(100),
            programming_hold: Duration::from_millis(10),
        }
    }
}

/// All arrays preserve air-protocol byte order. Bits within bytes are LSB first.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NfcATag {
    pub uid: Vec<u8>,
    pub atqa: [u8; 2],
    pub sak: u8,
    /// Full ATS including its TL byte, excluding CRC; empty for non-ISO-DEP tags.
    pub ats: Vec<u8>,
}
impl NfcATag {
    pub fn iso_dep(&self) -> bool {
        self.sak & 0x20 != 0
    }
    pub fn nfc_dep(&self) -> bool {
        self.sak & 0x40 != 0
    }
    /// SAK hints only, not proof of a product or implemented authentication.
    pub fn classic_hint(&self) -> bool {
        self.sak & 0x08 != 0 && self.sak & 0x02 == 0
    }
    pub fn type2_hint(&self) -> bool {
        self.sak == 0
    }
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NfcBTag {
    pub pupi: [u8; 4],
    pub application_data: [u8; 4],
    pub protocol_info: [u8; 3],
    pub cid: u8,
    /// High nibble of ATTRIB response, preserved rather than requiring zero.
    pub mbli: u8,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct St25tbTag {
    pub uid: [u8; 8],
    pub chip_id: u8,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Tag {
    NfcA(NfcATag),
    NfcB(NfcBTag),
    St25tb(St25tbTag),
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TagId {
    NfcA(Vec<u8>),
    NfcB([u8; 4]),
    St25tb([u8; 8]),
}
impl Tag {
    pub fn id(&self) -> TagId {
        match self {
            Self::NfcA(t) => TagId::NfcA(t.uid.clone()),
            Self::NfcB(t) => TagId::NfcB(t.pupi),
            Self::St25tb(t) => TagId::St25tb(t.uid),
        }
    }
}

/// Raw frames are not an ISO-DEP/APDU, Crypto1, NFC-DEP or NDEF stack.
#[derive(Debug, Clone, Copy)]
pub struct FrameOptions {
    pub tx_crc: bool,
    pub rx_crc: bool,
    pub tx_parity: bool,
    pub rx_parity: bool,
    pub tx_only: bool,
    pub allow_no_response: bool,
    /// Hardware NRT interval measured after TX; >0 and <=19 seconds.
    pub response_timeout: Duration,
    /// RF hold after a possibly started TX, including TX-only/error paths.
    /// Set this to the tag's maximum programming time for other tag families.
    pub field_hold: Duration,
}
impl Default for FrameOptions {
    fn default() -> Self {
        Self {
            tx_crc: true,
            rx_crc: true,
            tx_parity: true,
            rx_parity: true,
            tx_only: false,
            allow_no_response: false,
            response_timeout: Duration::from_millis(5),
            field_hold: Duration::from_millis(10),
        }
    }
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    Received,
    Transmitted,
    NoResponse,
}
/// RX bytes contain CRC when rx_crc is enabled; `bits` includes those CRC bits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FrameResult {
    pub bytes: usize,
    pub bits: usize,
    pub outcome: Outcome,
}
