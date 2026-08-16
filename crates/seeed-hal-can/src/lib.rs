#![forbid(unsafe_code)]

mod adapter;
mod config;
mod filter;
mod frame;

pub use adapter::{CanActiveConfig, CanAdapter, CanBusState, CanBusStatus, CanChannel};
pub use config::{
    CanBitTiming, CanConfigureConfig, CanLinkExpectation, CanMode, CanOpenConfig,
};
pub use filter::{CanFilter, CanFilterSet, CanFrameClasses, CanIdFormat};
pub use frame::{
    CanBatchSendError, CanErrorClass, CanFrame, CanId, CanTimestamp, CanTimestampSource,
    ReceivedCanFrame,
};
pub use seeed_hal_core::{
    CapabilityId, CapabilitySet, Endpoint, IdentityQuality, LeaseMode, ResourceDescriptor,
    ResourceId, ResourceProperties, ResourceSelector, TransportKind,
};

pub const MAX_CLASSIC_DATA_BYTES: usize = 8;
pub const MAX_FD_DATA_BYTES: usize = 64;
pub const MAX_CAN_FILTERS: usize = 64;
pub const MAX_CAN_BATCH_FRAMES: usize = 64;
/// Maximum number of diagnostic classes carried by one CAN error frame.
///
/// The bound is the number of stable wire error-class values. It keeps the
/// public model and every protocol adapter bounded without deduplicating or
/// otherwise normalizing caller-provided class order.
pub const MAX_CAN_ERROR_CLASSES: usize = 10;
pub const DEFAULT_CAN_RX_CAPACITY: usize = 256;
pub const DEFAULT_CAN_TX_CAPACITY: usize = 64;

pub const CAN_CLASSIC_CAPABILITY: &str = "can.classic/v1";
pub const CAN_FD_CAPABILITY: &str = "can.fd/v1";
pub const CAN_CONFIGURE_CAPABILITY: &str = "can.configure/v1";
pub const CAN_ERROR_FRAMES_CAPABILITY: &str = "can.error-frames/v1";
pub const CAN_RX_TIMESTAMP_CAPABILITY: &str = "can.rx-timestamp/v1";

pub fn can_classic_capability() -> CapabilityId {
    CapabilityId::parse(CAN_CLASSIC_CAPABILITY).expect("static CAN capability is valid")
}

pub fn can_fd_capability() -> CapabilityId {
    CapabilityId::parse(CAN_FD_CAPABILITY).expect("static CAN capability is valid")
}

pub fn can_configure_capability() -> CapabilityId {
    CapabilityId::parse(CAN_CONFIGURE_CAPABILITY).expect("static CAN capability is valid")
}

pub fn can_error_frames_capability() -> CapabilityId {
    CapabilityId::parse(CAN_ERROR_FRAMES_CAPABILITY).expect("static CAN capability is valid")
}

pub fn can_rx_timestamp_capability() -> CapabilityId {
    CapabilityId::parse(CAN_RX_TIMESTAMP_CAPABILITY).expect("static CAN capability is valid")
}
