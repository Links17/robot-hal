#![forbid(unsafe_code)]

use async_trait::async_trait;
use seeed_hal_core::{ErrorCategory, HalError, HalResult, ResourceDescriptor, ResourceSelector};
use std::time::Duration;

pub use seeed_hal_core::{
    CapabilityId, CapabilitySet, Endpoint, IdentityQuality, ResourceId, ResourceProperties,
    TransportKind,
};

pub const MAX_GPIO_EVENTS: usize = 1024;
pub const DEFAULT_GPIO_EVENT_CAPACITY: usize = 256;
pub const GPIO_LINES_CAPABILITY: &str = "gpio.lines/v1";
pub const GPIO_EDGES_CAPABILITY: &str = "gpio.edges/v1";

pub fn gpio_lines_capability() -> CapabilityId {
    CapabilityId::parse(GPIO_LINES_CAPABILITY).expect("static GPIO capability is valid")
}

pub fn gpio_edges_capability() -> CapabilityId {
    CapabilityId::parse(GPIO_EDGES_CAPABILITY).expect("static GPIO capability is valid")
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum GpioBias {
    Disabled,
    PullUp,
    PullDown,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum GpioDrive {
    PushPull,
    OpenDrain,
    OpenSource,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum GpioDirection {
    Input,
    Output,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct GpioLineConfig {
    direction: GpioDirection,
    active_low: bool,
    bias: GpioBias,
    drive: Option<GpioDrive>,
    initial_value: Option<bool>,
}

impl GpioLineConfig {
    pub const fn input(active_low: bool, bias: GpioBias) -> HalResult<Self> {
        Ok(Self {
            direction: GpioDirection::Input,
            active_low,
            bias,
            drive: None,
            initial_value: None,
        })
    }

    pub const fn output(
        active_low: bool,
        initial_value: bool,
        drive: GpioDrive,
    ) -> HalResult<Self> {
        Ok(Self {
            direction: GpioDirection::Output,
            active_low,
            bias: GpioBias::Disabled,
            drive: Some(drive),
            initial_value: Some(initial_value),
        })
    }

    pub const fn direction(self) -> GpioDirection {
        self.direction
    }
    pub const fn active_low(self) -> bool {
        self.active_low
    }
    pub const fn bias(self) -> GpioBias {
        self.bias
    }
    pub const fn drive(self) -> Option<GpioDrive> {
        self.drive
    }
    pub const fn initial_value(self) -> Option<bool> {
        self.initial_value
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct EdgeMask(u8);

impl EdgeMask {
    pub const RISING: Self = Self(0b01);
    pub const FALLING: Self = Self(0b10);
    pub const BOTH: Self = Self(0b11);

    pub const fn empty() -> Self {
        Self(0)
    }
    pub const fn contains(self, edge: GpioEdge) -> bool {
        self.0
            & match edge {
                GpioEdge::Rising => Self::RISING.0,
                GpioEdge::Falling => Self::FALLING.0,
            }
            != 0
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum GpioEdge {
    Rising,
    Falling,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct GpioEdgeRequest {
    edges: EdgeMask,
    capacity: usize,
}

impl GpioEdgeRequest {
    pub fn new(edges: EdgeMask, capacity: usize) -> HalResult<Self> {
        if edges.0 == 0 || capacity == 0 || capacity > MAX_GPIO_EVENTS {
            return Err(HalError::new(
                "gpio.edge_request.invalid",
                ErrorCategory::InvalidArgument,
                "gpio.edge_request",
                false,
                "edge request must select edges and use a bounded non-zero capacity",
            )?);
        }
        Ok(Self { edges, capacity })
    }

    pub const fn edges(self) -> EdgeMask {
        self.edges
    }
    pub const fn capacity(self) -> usize {
        self.capacity
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct GpioEdgeEvent {
    edge: GpioEdge,
    monotonic_ns: u64,
    sequence: u64,
}

impl GpioEdgeEvent {
    pub const fn new(edge: GpioEdge, monotonic_ns: u64, sequence: u64) -> Self {
        Self {
            edge,
            monotonic_ns,
            sequence,
        }
    }
    pub const fn edge(self) -> GpioEdge {
        self.edge
    }
    pub const fn monotonic_ns(self) -> u64 {
        self.monotonic_ns
    }
    pub const fn sequence(self) -> u64 {
        self.sequence
    }
}

#[async_trait]
pub trait GpioAdapter: Send + Sync {
    fn adapter_name(&self) -> &'static str;
    async fn enumerate(&self) -> HalResult<Vec<ResourceDescriptor>>;
    async fn open(
        &self,
        selector: &ResourceSelector,
        lines: &[u32],
        config: GpioLineConfig,
    ) -> HalResult<Box<dyn GpioLineSession>>;
}

#[async_trait]
pub trait GpioLineSession: Send {
    fn descriptor(&self) -> &ResourceDescriptor;
    fn lines(&self) -> &[u32];
    fn config(&self) -> GpioLineConfig;
    async fn read(&mut self) -> HalResult<Vec<bool>>;
    async fn write(&mut self, values: &[bool]) -> HalResult<()>;
    /// Returns edge events in monotonic order. Implementations use oldest-drop
    /// queues and report lag with a structured result before later events.
    async fn next_edge(
        &mut self,
        request: GpioEdgeRequest,
        timeout: Duration,
    ) -> HalResult<Option<GpioEdgeEvent>>;
    async fn close(&mut self) -> HalResult<()>;
}
