#![forbid(unsafe_code)]

use async_trait::async_trait;
use bytes::Bytes;
use seeed_hal_core::{ErrorCategory, HalError, HalResult, ResourceDescriptor, ResourceSelector};
use std::time::Duration;

pub use seeed_hal_core::{
    CapabilityId, CapabilitySet, Endpoint, IdentityQuality, ResourceId, ResourceProperties,
    TransportKind,
};

pub const MAX_USB_TRANSFER_BYTES: usize = 16 * 1024;
pub const MAX_USB_PENDING_TRANSFERS: usize = 64;
pub const MAX_USB_INTERFACE_NUMBER: usize = u8::MAX as usize;

pub const USB_CONTROL_CAPABILITY: &str = "usb.control/v1";
pub const USB_BULK_CAPABILITY: &str = "usb.bulk/v1";
pub const USB_INTERRUPT_CAPABILITY: &str = "usb.interrupt/v1";

pub fn usb_control_capability() -> CapabilityId {
    CapabilityId::parse(USB_CONTROL_CAPABILITY).expect("static USB capability is valid")
}

pub fn usb_bulk_capability() -> CapabilityId {
    CapabilityId::parse(USB_BULK_CAPABILITY).expect("static USB capability is valid")
}

pub fn usb_interrupt_capability() -> CapabilityId {
    CapabilityId::parse(USB_INTERRUPT_CAPABILITY).expect("static USB capability is valid")
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct UsbInterfaceClaim(u8);

impl UsbInterfaceClaim {
    pub fn new(interface: usize) -> HalResult<Self> {
        if interface > MAX_USB_INTERFACE_NUMBER {
            return Err(HalError::new(
                "usb.interface.invalid",
                ErrorCategory::InvalidArgument,
                "usb.interface_claim",
                false,
                "USB interface number is outside the u8 range",
            )?);
        }
        Ok(Self(interface as u8))
    }

    pub const fn number(self) -> u8 {
        self.0
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum UsbTransfer {
    ControlOut {
        request_type: u8,
        request: u8,
        value: u16,
        index: u16,
        data: Bytes,
    },
    ControlIn {
        request_type: u8,
        request: u8,
        value: u16,
        index: u16,
        max_bytes: usize,
    },
    BulkOut {
        endpoint: u8,
        data: Bytes,
    },
    BulkIn {
        endpoint: u8,
        max_bytes: usize,
    },
    InterruptOut {
        endpoint: u8,
        data: Bytes,
    },
    InterruptIn {
        endpoint: u8,
        max_bytes: usize,
    },
}

impl UsbTransfer {
    pub fn control_out(
        request_type: u8,
        request: u8,
        value: u16,
        index: u16,
        data: Bytes,
    ) -> HalResult<Self> {
        let transfer = Self::ControlOut {
            request_type,
            request,
            value,
            index,
            data,
        };
        transfer.validate()?;
        Ok(transfer)
    }

    pub fn control_in(
        request_type: u8,
        request: u8,
        value: u16,
        index: u16,
        max_bytes: usize,
    ) -> HalResult<Self> {
        let transfer = Self::ControlIn {
            request_type,
            request,
            value,
            index,
            max_bytes,
        };
        transfer.validate()?;
        Ok(transfer)
    }

    pub fn bulk_out(endpoint: u8, data: Bytes) -> HalResult<Self> {
        let transfer = Self::BulkOut { endpoint, data };
        transfer.validate()?;
        Ok(transfer)
    }

    pub fn bulk_in(endpoint: u8, max_bytes: usize) -> HalResult<Self> {
        let transfer = Self::BulkIn {
            endpoint,
            max_bytes,
        };
        transfer.validate()?;
        Ok(transfer)
    }

    pub fn interrupt_out(endpoint: u8, data: Bytes) -> HalResult<Self> {
        let transfer = Self::InterruptOut { endpoint, data };
        transfer.validate()?;
        Ok(transfer)
    }

    pub fn interrupt_in(endpoint: u8, max_bytes: usize) -> HalResult<Self> {
        let transfer = Self::InterruptIn {
            endpoint,
            max_bytes,
        };
        transfer.validate()?;
        Ok(transfer)
    }

    pub fn validate(&self) -> HalResult<()> {
        match self {
            Self::ControlOut {
                request_type, data, ..
            } => {
                validate_control_direction(*request_type, false)?;
                validate_size(data.len())
            }
            Self::ControlIn {
                request_type,
                max_bytes,
                ..
            } => {
                validate_control_direction(*request_type, true)?;
                validate_size(*max_bytes)
            }
            Self::BulkOut { endpoint, data } | Self::InterruptOut { endpoint, data } => {
                validate_endpoint(*endpoint, false)?;
                validate_size(data.len())
            }
            Self::BulkIn {
                endpoint,
                max_bytes,
            }
            | Self::InterruptIn {
                endpoint,
                max_bytes,
            } => {
                validate_endpoint(*endpoint, true)?;
                validate_size(*max_bytes)
            }
        }
    }
}

fn validate_control_direction(request_type: u8, input: bool) -> HalResult<()> {
    if ((request_type & 0x80) != 0) != input {
        return Err(invalid_transfer(
            "control request direction does not match transfer",
        ));
    }
    Ok(())
}

fn validate_endpoint(endpoint: u8, input: bool) -> HalResult<()> {
    if endpoint & 0x0f == 0 || ((endpoint & 0x80) != 0) != input {
        return Err(invalid_transfer("endpoint direction or number is invalid"));
    }
    Ok(())
}

fn validate_size(size: usize) -> HalResult<()> {
    if size > MAX_USB_TRANSFER_BYTES {
        return Err(invalid_transfer("transfer payload exceeds the bound"));
    }
    Ok(())
}

fn invalid_transfer(message: &'static str) -> HalError {
    HalError::new(
        "usb.transfer.invalid",
        ErrorCategory::InvalidArgument,
        "usb.transfer",
        false,
        message,
    )
    .expect("static USB transfer error metadata is valid")
}

#[async_trait]
pub trait UsbAdapter: Send + Sync {
    fn adapter_name(&self) -> &'static str;

    async fn enumerate(&self) -> HalResult<Vec<ResourceDescriptor>>;

    async fn open(
        &self,
        selector: &ResourceSelector,
        claim: UsbInterfaceClaim,
    ) -> HalResult<Box<dyn UsbInterfaceSession>>;
}

#[async_trait]
pub trait UsbInterfaceSession: Send {
    fn descriptor(&self) -> &ResourceDescriptor;

    fn interface_claim(&self) -> UsbInterfaceClaim;

    /// Executes one validated transfer. Implementations must bound any
    /// admission queue and return `runtime.queue.full` rather than wait forever.
    async fn transfer(&mut self, transfer: UsbTransfer, timeout: Duration) -> HalResult<Bytes>;

    async fn close(&mut self) -> HalResult<()>;
}
