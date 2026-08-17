#![forbid(unsafe_code)]

pub mod identity;

use async_trait::async_trait;
use bytes::Bytes;
use seeed_hal_core::{
    CapabilitySet, Endpoint, ErrorCategory, HalError, HalResult, ResourceDescriptor,
    ResourceProperties, ResourceSelector, TransportKind, resolve_resource,
};
use seeed_hal_usb::{
    UsbAdapter, UsbInterfaceClaim, UsbInterfaceSession, UsbTransfer, usb_bulk_capability,
    usb_control_capability, usb_interrupt_capability,
};
use std::{collections::BTreeMap, time::Duration};

#[cfg(any(target_os = "macos", target_os = "linux", target_os = "windows"))]
use nusb::{
    MaybeFuture,
    transfer::{Buffer, Bulk, ControlIn, ControlOut, ControlType, In, Interrupt, Out, Recipient},
};

#[derive(Clone, Copy, Debug, Default)]
pub struct NusbAdapter;

impl NusbAdapter {
    #[must_use]
    pub const fn new() -> Self {
        Self
    }
}

#[async_trait]
impl UsbAdapter for NusbAdapter {
    fn adapter_name(&self) -> &'static str {
        "nusb"
    }

    async fn enumerate(&self) -> HalResult<Vec<ResourceDescriptor>> {
        #[cfg(any(target_os = "macos", target_os = "linux", target_os = "windows"))]
        {
            tokio::task::spawn_blocking(enumerate_sync)
                .await
                .map_err(|error| worker_failed("usb.enumerate", error))?
        }
        #[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
        {
            Err(unavailable("usb.enumerate"))
        }
    }

    async fn open(
        &self,
        selector: &ResourceSelector,
        claim: UsbInterfaceClaim,
    ) -> HalResult<Box<dyn UsbInterfaceSession>> {
        #[cfg(any(target_os = "macos", target_os = "linux", target_os = "windows"))]
        {
            open_sync(selector, claim)
        }
        #[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
        {
            let _ = claim;
            Err(unavailable("usb.open").with_resource_id(selector.id().clone()))
        }
    }
}

#[cfg(any(target_os = "macos", target_os = "linux", target_os = "windows"))]
fn enumerate_sync() -> HalResult<Vec<ResourceDescriptor>> {
    nusb::list_devices()
        .wait()
        .map_err(|error| platform_error("usb.enumerate", error))?
        .map(descriptor_from_device)
        .collect()
}

#[cfg(any(target_os = "macos", target_os = "linux", target_os = "windows"))]
fn descriptor_from_device(device: nusb::DeviceInfo) -> HalResult<ResourceDescriptor> {
    let topology = format!(
        "{}-{}",
        device.bus_id(),
        device
            .port_chain()
            .iter()
            .map(u8::to_string)
            .collect::<Vec<_>>()
            .join(".")
    );
    let identity = identity::identity_from_metadata(&identity::UsbDeviceMetadata {
        vendor_id: device.vendor_id(),
        product_id: device.product_id(),
        serial_number: device.serial_number().map(ToOwned::to_owned),
        topology,
    })?;
    let endpoint = format!("{}/{}", device.bus_id(), device.device_address());
    let mut properties = BTreeMap::new();
    properties.insert("adapter".to_owned(), "nusb".to_owned());
    properties.insert("endpoint".to_owned(), endpoint.clone());
    properties.insert("usb.vid".to_owned(), format!("{:04x}", device.vendor_id()));
    properties.insert("usb.pid".to_owned(), format!("{:04x}", device.product_id()));
    properties.insert("usb.bus_id".to_owned(), device.bus_id().to_owned());
    properties.insert(
        "usb.port_chain".to_owned(),
        device
            .port_chain()
            .iter()
            .map(u8::to_string)
            .collect::<Vec<_>>()
            .join("."),
    );
    properties.insert(
        "usb.device_address".to_owned(),
        device.device_address().to_string(),
    );
    Ok(ResourceDescriptor::new(
        identity.id,
        Endpoint::new(endpoint)?,
        identity.quality,
        TransportKind::Usb,
        ResourceProperties::new(properties),
        CapabilitySet::new(vec![
            usb_control_capability(),
            usb_bulk_capability(),
            usb_interrupt_capability(),
        ]),
    ))
}

#[cfg(any(target_os = "macos", target_os = "linux", target_os = "windows"))]
fn open_sync(
    selector: &ResourceSelector,
    claim: UsbInterfaceClaim,
) -> HalResult<Box<dyn UsbInterfaceSession>> {
    let devices = nusb::list_devices()
        .wait()
        .map_err(|error| platform_error("usb.open", error))?
        .map(|device| Ok((descriptor_from_device(device.clone())?, device)))
        .collect::<HalResult<Vec<_>>>()?;
    let descriptors = devices
        .iter()
        .map(|(descriptor, _)| descriptor.clone())
        .collect::<Vec<_>>();
    let descriptor = resolve_resource(
        &descriptors,
        selector,
        &usb_control_capability(),
        "usb.open",
    )?
    .clone();
    let device = devices
        .into_iter()
        .find(|(candidate, _)| candidate.endpoint() == descriptor.endpoint())
        .expect("resolved USB descriptor originated from current enumeration")
        .1
        .open()
        .wait()
        .map_err(|error| {
            platform_error("usb.open", error).with_resource_id(descriptor.id().clone())
        })?;
    let interface = device
        .claim_interface(claim.number())
        .wait()
        .map_err(|error| {
            platform_error("usb.open", error).with_resource_id(descriptor.id().clone())
        })?;
    Ok(Box::new(NusbSession {
        descriptor,
        claim,
        interface,
        closed: false,
    }))
}

#[cfg(any(target_os = "macos", target_os = "linux", target_os = "windows"))]
struct NusbSession {
    descriptor: ResourceDescriptor,
    claim: UsbInterfaceClaim,
    interface: nusb::Interface,
    closed: bool,
}

#[cfg(any(target_os = "macos", target_os = "linux", target_os = "windows"))]
#[async_trait]
impl UsbInterfaceSession for NusbSession {
    fn descriptor(&self) -> &ResourceDescriptor {
        &self.descriptor
    }

    fn interface_claim(&self) -> UsbInterfaceClaim {
        self.claim
    }

    async fn transfer(&mut self, transfer: UsbTransfer, timeout: Duration) -> HalResult<Bytes> {
        if self.closed {
            return Err(session_closed("usb.transfer"));
        }
        transfer.validate()?;
        execute_transfer(&mut self.interface, transfer, timeout)
            .map_err(|error| error.with_resource_id(self.descriptor.id().clone()))
    }

    async fn close(&mut self) -> HalResult<()> {
        self.closed = true;
        Ok(())
    }
}

#[cfg(any(target_os = "macos", target_os = "linux", target_os = "windows"))]
fn execute_transfer(
    interface: &mut nusb::Interface,
    transfer: UsbTransfer,
    timeout: Duration,
) -> HalResult<Bytes> {
    match transfer {
        UsbTransfer::ControlOut {
            request_type,
            request,
            value,
            index,
            data,
        } => {
            interface
                .control_out(
                    ControlOut {
                        control_type: control_type(request_type)?,
                        recipient: recipient(request_type)?,
                        request,
                        value,
                        index,
                        data: &data,
                    },
                    timeout,
                )
                .wait()
                .map_err(|error| transfer_error("usb.transfer", error))?;
            Ok(Bytes::new())
        }
        UsbTransfer::ControlIn {
            request_type,
            request,
            value,
            index,
            max_bytes,
        } => {
            let length = u16::try_from(max_bytes)
                .map_err(|_| invalid("usb.transfer", "control length exceeds u16"))?;
            interface
                .control_in(
                    ControlIn {
                        control_type: control_type(request_type)?,
                        recipient: recipient(request_type)?,
                        request,
                        value,
                        index,
                        length,
                    },
                    timeout,
                )
                .wait()
                .map(Bytes::from)
                .map_err(|error| transfer_error("usb.transfer", error))
        }
        UsbTransfer::BulkOut { endpoint, data } => {
            endpoint_out::<Bulk>(interface, endpoint, data, timeout)
        }
        UsbTransfer::BulkIn {
            endpoint,
            max_bytes,
        } => endpoint_in::<Bulk>(interface, endpoint, max_bytes, timeout),
        UsbTransfer::InterruptOut { endpoint, data } => {
            endpoint_out::<Interrupt>(interface, endpoint, data, timeout)
        }
        UsbTransfer::InterruptIn {
            endpoint,
            max_bytes,
        } => endpoint_in::<Interrupt>(interface, endpoint, max_bytes, timeout),
    }
}

#[cfg(any(target_os = "macos", target_os = "linux", target_os = "windows"))]
fn endpoint_out<T: nusb::transfer::BulkOrInterrupt>(
    interface: &nusb::Interface,
    endpoint: u8,
    data: Bytes,
    timeout: Duration,
) -> HalResult<Bytes> {
    let mut endpoint = interface
        .endpoint::<T, Out>(endpoint)
        .map_err(|error| platform_error("usb.transfer", error))?;
    let completion = endpoint.transfer_blocking(Buffer::from(data.to_vec()), timeout);
    completion
        .status
        .map_err(|error| transfer_error("usb.transfer", error))?;
    Ok(Bytes::new())
}

#[cfg(any(target_os = "macos", target_os = "linux", target_os = "windows"))]
fn endpoint_in<T: nusb::transfer::BulkOrInterrupt>(
    interface: &nusb::Interface,
    endpoint: u8,
    max_bytes: usize,
    timeout: Duration,
) -> HalResult<Bytes> {
    let mut endpoint = interface
        .endpoint::<T, In>(endpoint)
        .map_err(|error| platform_error("usb.transfer", error))?;
    let completion = endpoint.transfer_blocking(Buffer::new(max_bytes), timeout);
    completion
        .status
        .map_err(|error| transfer_error("usb.transfer", error))?;
    Ok(Bytes::copy_from_slice(
        &completion.buffer[..completion.actual_len],
    ))
}

#[cfg(any(target_os = "macos", target_os = "linux", target_os = "windows"))]
fn control_type(request_type: u8) -> HalResult<ControlType> {
    match (request_type >> 5) & 0x03 {
        0 => Ok(ControlType::Standard),
        1 => Ok(ControlType::Class),
        2 => Ok(ControlType::Vendor),
        _ => Err(invalid("usb.transfer", "reserved USB control type")),
    }
}

#[cfg(any(target_os = "macos", target_os = "linux", target_os = "windows"))]
fn recipient(request_type: u8) -> HalResult<Recipient> {
    match request_type & 0x1f {
        0 => Ok(Recipient::Device),
        1 => Ok(Recipient::Interface),
        2 => Ok(Recipient::Endpoint),
        3 => Ok(Recipient::Other),
        _ => Err(invalid("usb.transfer", "reserved USB control recipient")),
    }
}

#[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
fn unavailable(operation: &'static str) -> HalError {
    HalError::new(
        "runtime.adapter.unavailable",
        ErrorCategory::Unavailable,
        operation,
        false,
        "native nusb session support is not yet available",
    )
    .expect("static nusb adapter error metadata is valid")
}

#[cfg(any(target_os = "macos", target_os = "linux", target_os = "windows"))]
fn platform_error(operation: &'static str, error: impl std::error::Error) -> HalError {
    HalError::new(
        "runtime.transport.unavailable",
        ErrorCategory::Unavailable,
        operation,
        true,
        format!("nusb platform error: {error}"),
    )
    .expect("static nusb platform error metadata is valid")
}

#[cfg(any(target_os = "macos", target_os = "linux", target_os = "windows"))]
fn transfer_error(operation: &'static str, error: nusb::transfer::TransferError) -> HalError {
    let (name, category, retryable) = match error {
        nusb::transfer::TransferError::Cancelled => (
            "runtime.transport.timeout",
            ErrorCategory::Unavailable,
            true,
        ),
        nusb::transfer::TransferError::Disconnected => (
            "runtime.transport.disconnected",
            ErrorCategory::Unavailable,
            true,
        ),
        nusb::transfer::TransferError::InvalidArgument => (
            "runtime.transport.unsupported_configuration",
            ErrorCategory::InvalidArgument,
            false,
        ),
        _ => (
            "runtime.transport.unavailable",
            ErrorCategory::Unavailable,
            true,
        ),
    };
    HalError::new(
        name,
        category,
        operation,
        retryable,
        format!("nusb transfer error: {error}"),
    )
    .expect("static nusb transfer error metadata is valid")
}

fn invalid(operation: &'static str, message: &'static str) -> HalError {
    HalError::new(
        "usb.transfer.invalid",
        ErrorCategory::InvalidArgument,
        operation,
        false,
        message,
    )
    .expect("static USB invalid error metadata is valid")
}

fn session_closed(operation: &'static str) -> HalError {
    HalError::new(
        "runtime.session.closed",
        ErrorCategory::Conflict,
        operation,
        false,
        "USB session is closed",
    )
    .expect("static USB closed error metadata is valid")
}

fn worker_failed(operation: &'static str, error: tokio::task::JoinError) -> HalError {
    HalError::new(
        "runtime.internal.worker_failed",
        ErrorCategory::Internal,
        operation,
        false,
        format!("nusb worker failed: {error}"),
    )
    .expect("static nusb worker error metadata is valid")
}
