#![forbid(unsafe_code)]

use async_trait::async_trait;
use robot_hal_core::{HalResult, ResourceSelector};
use std::time::Duration;

pub use robot_hal_core::{
    CapabilityId, CapabilitySet, Endpoint, IdentityQuality, ResourceDescriptor, ResourceId,
    ResourceProperties, TransportKind,
};

pub const SERIAL_BYTES_CAPABILITY: &str = "serial.bytes/v1";

pub fn serial_bytes_capability() -> CapabilityId {
    CapabilityId::parse(SERIAL_BYTES_CAPABILITY)
        .expect("the static Serial capability identifier is valid")
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DataBits {
    Five,
    Six,
    Seven,
    Eight,
}

impl Default for DataBits {
    fn default() -> Self {
        Self::Eight
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Parity {
    None,
    Odd,
    Even,
}

impl Default for Parity {
    fn default() -> Self {
        Self::None
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum StopBits {
    One,
    Two,
}

impl Default for StopBits {
    fn default() -> Self {
        Self::One
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FlowControl {
    None,
    Software,
    Hardware,
}

impl Default for FlowControl {
    fn default() -> Self {
        Self::None
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ControlLines {
    pub data_terminal_ready: bool,
    pub request_to_send: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SerialConfig {
    pub baud_rate: u32,
    pub data_bits: DataBits,
    pub parity: Parity,
    pub stop_bits: StopBits,
    pub flow_control: FlowControl,
    pub read_timeout: Duration,
}

impl Default for SerialConfig {
    fn default() -> Self {
        Self {
            baud_rate: 115_200,
            data_bits: DataBits::Eight,
            parity: Parity::None,
            stop_bits: StopBits::One,
            flow_control: FlowControl::None,
            read_timeout: Duration::from_millis(100),
        }
    }
}

#[async_trait]
pub trait SerialAdapter: Send + Sync {
    fn adapter_name(&self) -> &'static str;

    async fn enumerate(&self) -> HalResult<Vec<ResourceDescriptor>>;

    async fn open(
        &self,
        selector: &ResourceSelector,
        config: SerialConfig,
    ) -> HalResult<Box<dyn SerialSession>>;
}

#[async_trait]
pub trait SerialSession: Send {
    fn descriptor(&self) -> &ResourceDescriptor;

    async fn read(&mut self, max_bytes: usize) -> HalResult<bytes::Bytes>;

    /// Admits `bytes` for transmission while preserving the byte order of
    /// successfully admitted writes.
    ///
    /// Implementations must bound any internal admission queue. If that queue
    /// has no capacity, this operation must return an error named
    /// `runtime.queue.full` instead of waiting indefinitely. Queue capacity is
    /// adapter-specific.
    async fn write_all(&mut self, bytes: &[u8]) -> HalResult<()>;

    async fn flush(&mut self) -> HalResult<()>;

    async fn set_control_lines(&mut self, lines: ControlLines) -> HalResult<()>;

    async fn close(&mut self) -> HalResult<()>;
}
