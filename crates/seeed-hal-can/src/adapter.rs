use async_trait::async_trait;
use seeed_hal_core::{HalResult, ResourceDescriptor, ResourceSelector};
use std::time::Duration;

use crate::{CanFrame, CanOpenConfig, ReceivedCanFrame};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CanBusState {
    Active,
    Warning,
    Passive,
    BusOff,
    Stopped,
    Unknown,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CanBusStatus {
    state: CanBusState,
    tx_error_counter: Option<u32>,
    rx_error_counter: Option<u32>,
}

impl CanBusStatus {
    pub fn new(
        state: CanBusState,
        tx_error_counter: Option<u32>,
        rx_error_counter: Option<u32>,
    ) -> Self {
        Self {
            state,
            tx_error_counter,
            rx_error_counter,
        }
    }

    pub fn state(&self) -> CanBusState {
        self.state
    }

    pub fn tx_error_counter(&self) -> Option<u32> {
        self.tx_error_counter
    }

    pub fn rx_error_counter(&self) -> Option<u32> {
        self.rx_error_counter
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CanActiveConfig {
    mode: crate::CanMode,
    nominal: crate::CanBitTiming,
    data: Option<crate::CanBitTiming>,
    listen_only: bool,
    loopback: bool,
    clock_domain: String,
}

impl CanActiveConfig {
    pub fn new(
        mode: crate::CanMode,
        nominal: crate::CanBitTiming,
        data: Option<crate::CanBitTiming>,
        listen_only: bool,
        loopback: bool,
        clock_domain: impl Into<String>,
    ) -> HalResult<Self> {
        let clock_domain = clock_domain.into();
        if clock_domain.is_empty() || clock_domain.len() > 255 || !clock_domain.is_ascii() {
            return Err(seeed_hal_core::HalError::new(
                "can.configuration.invalid",
                seeed_hal_core::ErrorCategory::InvalidArgument,
                "can.active_configuration",
                false,
                "CAN clock domain must be non-empty ASCII of at most 255 bytes",
            )
            .expect("static CAN configuration error metadata is valid"));
        }
        if matches!(mode, crate::CanMode::Classic) && data.is_some()
            || matches!(mode, crate::CanMode::Fd) && data.is_none()
        {
            return Err(seeed_hal_core::HalError::new(
                "can.configuration.invalid",
                seeed_hal_core::ErrorCategory::InvalidArgument,
                "can.active_configuration",
                false,
                "CAN active configuration has incompatible mode and data timing",
            )
            .expect("static CAN configuration error metadata is valid"));
        }
        Ok(Self {
            mode,
            nominal,
            data,
            listen_only,
            loopback,
            clock_domain,
        })
    }

    pub fn mode(&self) -> crate::CanMode {
        self.mode
    }

    pub fn nominal(&self) -> &crate::CanBitTiming {
        &self.nominal
    }

    pub fn data(&self) -> Option<&crate::CanBitTiming> {
        self.data.as_ref()
    }

    pub fn listen_only(&self) -> bool {
        self.listen_only
    }

    pub fn loopback(&self) -> bool {
        self.loopback
    }

    pub fn clock_domain(&self) -> &str {
        &self.clock_domain
    }
}

#[async_trait]
pub trait CanAdapter: Send + Sync {
    fn adapter_name(&self) -> &'static str;

    async fn enumerate(&self) -> HalResult<Vec<ResourceDescriptor>>;

    async fn open(
        &self,
        selector: &ResourceSelector,
        config: &CanOpenConfig,
    ) -> HalResult<Box<dyn CanChannel>>;
}

pub trait CanChannel: Send {
    fn descriptor(&self) -> &ResourceDescriptor;

    fn active_config(&self) -> &CanActiveConfig;

    fn receive(&mut self, timeout: Duration) -> HalResult<Option<ReceivedCanFrame>>;

    fn send(&mut self, frame: &CanFrame) -> HalResult<()>;

    fn bus_status(&mut self) -> HalResult<CanBusStatus>;

    fn close(&mut self) -> HalResult<()>;
}
