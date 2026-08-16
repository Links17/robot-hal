use bytes::Bytes;
use seeed_hal_core::{ErrorCategory, HalError, HalResult};
use std::fmt;

use crate::{MAX_CLASSIC_DATA_BYTES, MAX_FD_DATA_BYTES};

fn invalid_frame(message: &'static str) -> HalError {
    HalError::new(
        "can.frame.invalid",
        ErrorCategory::InvalidArgument,
        "can.frame",
        false,
        message,
    )
    .expect("static CAN frame error metadata is valid")
}

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum CanId {
    Standard(u16),
    Extended(u32),
}

impl CanId {
    pub fn standard(value: u16) -> HalResult<Self> {
        if value > 0x7ff {
            return Err(invalid_frame("standard CAN identifier exceeds 11 bits"));
        }
        Ok(Self::Standard(value))
    }

    pub fn extended(value: u32) -> HalResult<Self> {
        if value > 0x1fff_ffff {
            return Err(invalid_frame("extended CAN identifier exceeds 29 bits"));
        }
        Ok(Self::Extended(value))
    }

    pub fn value(self) -> u32 {
        match self {
            Self::Standard(value) => u32::from(value),
            Self::Extended(value) => value,
        }
    }

    pub fn is_standard(self) -> bool {
        matches!(self, Self::Standard(_))
    }

    pub fn is_extended(self) -> bool {
        matches!(self, Self::Extended(_))
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum CanErrorClass {
    TxTimeout,
    LostArbitration,
    Controller,
    Protocol,
    Transceiver,
    NoAcknowledgement,
    BusOff,
    BusError,
    Restarted,
    Other,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CanFrame {
    ClassicData { id: CanId, data: Bytes },
    ClassicRemote { id: CanId, dlc: u8 },
    FdData {
        id: CanId,
        data: Bytes,
        bitrate_switch: bool,
        error_state_indicator: bool,
    },
    Error { classes: Vec<CanErrorClass>, data: Bytes },
}

impl CanFrame {
    pub fn classic_data(id: CanId, data: impl Into<Bytes>) -> HalResult<Self> {
        let data = data.into();
        if data.len() > MAX_CLASSIC_DATA_BYTES {
            return Err(invalid_frame("Classical CAN data exceeds 8 bytes"));
        }
        Ok(Self::ClassicData { id, data })
    }

    pub fn classic_remote(id: CanId, dlc: u8) -> HalResult<Self> {
        if usize::from(dlc) > MAX_CLASSIC_DATA_BYTES {
            return Err(invalid_frame("Classical CAN remote DLC exceeds 8"));
        }
        Ok(Self::ClassicRemote { id, dlc })
    }

    pub fn fd_data(
        id: CanId,
        data: impl Into<Bytes>,
        bitrate_switch: bool,
        error_state_indicator: bool,
    ) -> HalResult<Self> {
        let data = data.into();
        if data.len() > MAX_FD_DATA_BYTES {
            return Err(invalid_frame("CAN FD data exceeds 64 bytes"));
        }
        Ok(Self::FdData {
            id,
            data,
            bitrate_switch,
            error_state_indicator,
        })
    }

    pub fn error(classes: Vec<CanErrorClass>, data: impl Into<Bytes>) -> HalResult<Self> {
        let data = data.into();
        if classes.is_empty() {
            return Err(invalid_frame("CAN error frame must contain a class"));
        }
        if data.len() > MAX_CLASSIC_DATA_BYTES {
            return Err(invalid_frame("CAN error diagnostics exceed 8 bytes"));
        }
        Ok(Self::Error { classes, data })
    }

    pub fn id(&self) -> Option<&CanId> {
        match self {
            Self::ClassicData { id, .. }
            | Self::ClassicRemote { id, .. }
            | Self::FdData { id, .. } => Some(id),
            Self::Error { .. } => None,
        }
    }

    pub fn data(&self) -> &[u8] {
        match self {
            Self::ClassicData { data, .. }
            | Self::FdData { data, .. }
            | Self::Error { data, .. } => data,
            Self::ClassicRemote { .. } => &[],
        }
    }

    pub fn dlc(&self) -> Option<u8> {
        match self {
            Self::ClassicRemote { dlc, .. } => Some(*dlc),
            _ => None,
        }
    }

    pub fn is_error(&self) -> bool {
        matches!(self, Self::Error { .. })
    }

    pub fn is_remote(&self) -> bool {
        matches!(self, Self::ClassicRemote { .. })
    }

    pub fn is_data(&self) -> bool {
        matches!(self, Self::ClassicData { .. } | Self::FdData { .. })
    }

    pub fn bitrate_switch(&self) -> Option<bool> {
        match self {
            Self::FdData { bitrate_switch, .. } => Some(*bitrate_switch),
            _ => None,
        }
    }

    pub fn error_state_indicator(&self) -> Option<bool> {
        match self {
            Self::FdData {
                error_state_indicator,
                ..
            } => Some(*error_state_indicator),
            _ => None,
        }
    }

    pub fn error_classes(&self) -> Option<&[CanErrorClass]> {
        match self {
            Self::Error { classes, .. } => Some(classes),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum CanTimestampSource {
    Hardware,
    Kernel,
    HostMonotonic,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CanTimestamp {
    timestamp_ns: u64,
    source: CanTimestampSource,
    clock_domain: String,
}

impl CanTimestamp {
    pub fn new(
        timestamp_ns: u64,
        source: CanTimestampSource,
        clock_domain: impl Into<String>,
    ) -> HalResult<Self> {
        let clock_domain = clock_domain.into();
        if clock_domain.is_empty() {
            return Err(invalid_frame("CAN timestamp clock domain must not be empty"));
        }
        if clock_domain.len() > 255 {
            return Err(invalid_frame("CAN timestamp clock domain exceeds 255 bytes"));
        }
        if !clock_domain.is_ascii() {
            return Err(invalid_frame("CAN timestamp clock domain must be ASCII"));
        }
        Ok(Self {
            timestamp_ns,
            source,
            clock_domain,
        })
    }

    pub fn timestamp_ns(&self) -> u64 {
        self.timestamp_ns
    }

    pub fn source(&self) -> CanTimestampSource {
        self.source
    }

    pub fn clock_domain(&self) -> &str {
        &self.clock_domain
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReceivedCanFrame {
    frame: CanFrame,
    timestamp: Option<CanTimestamp>,
}

impl ReceivedCanFrame {
    pub fn new(frame: CanFrame, timestamp: Option<CanTimestamp>) -> Self {
        Self { frame, timestamp }
    }

    pub fn frame(&self) -> &CanFrame {
        &self.frame
    }

    pub fn timestamp(&self) -> Option<&CanTimestamp> {
        self.timestamp.as_ref()
    }

    pub fn into_frame(self) -> CanFrame {
        self.frame
    }
}

#[derive(Clone, Eq, PartialEq)]
pub struct CanBatchSendError {
    error: HalError,
    committed: usize,
}

impl CanBatchSendError {
    pub fn new(error: HalError, committed: usize) -> Self {
        Self { error, committed }
    }

    pub fn error(&self) -> &HalError {
        &self.error
    }

    pub fn committed(&self) -> usize {
        self.committed
    }
}

impl fmt::Debug for CanBatchSendError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("CanBatchSendError")
            .field("error", &self.error)
            .field("committed", &self.committed)
            .finish()
    }
}

impl fmt::Display for CanBatchSendError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{} (committed {})", self.error, self.committed)
    }
}

impl std::error::Error for CanBatchSendError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.error)
    }
}
