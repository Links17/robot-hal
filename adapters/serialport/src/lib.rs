#![forbid(unsafe_code)]

pub mod identity;
mod session;

use async_trait::async_trait;
use seeed_hal_core::{
    Endpoint, ErrorCategory, HalError, HalResult, ResourceDescriptor, ResourceProperties,
    ResourceSelector, TransportKind,
};
use seeed_hal_serial::{SerialAdapter, SerialConfig, SerialSession};
use serialport::{SerialPortInfo, SerialPortType};
use std::collections::BTreeMap;

use crate::identity::{SerialIdentity, UsbPortMetadata, identity_from_endpoint};
use crate::session::NativeSerialSession;

#[derive(Clone, Copy, Debug, Default)]
pub struct SerialPortAdapter;

impl SerialPortAdapter {
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl SerialAdapter for SerialPortAdapter {
    fn adapter_name(&self) -> &'static str {
        "serialport"
    }

    async fn enumerate(&self) -> HalResult<Vec<ResourceDescriptor>> {
        let ports = tokio::task::spawn_blocking(serialport::available_ports)
            .await
            .map_err(|error| {
                internal(
                    "serial.enumerate",
                    format!("serialport enumeration task failed: {error}"),
                )
            })?
            .map_err(|error| map_serialport_error("serial.enumerate", error))?;

        ports.into_iter().map(descriptor_from_port).collect()
    }

    async fn open(
        &self,
        selector: &ResourceSelector,
        config: SerialConfig,
    ) -> HalResult<Box<dyn SerialSession>> {
        if selector.transport() != TransportKind::Serial {
            return Err(unsupported_configuration(
                "serial.open",
                "selector transport is not serial",
            ));
        }

        let descriptor = self
            .enumerate()
            .await?
            .into_iter()
            .find(|descriptor| descriptor.selector() == *selector)
            .ok_or_else(|| not_found("serial.open", "selector did not match an enumerated port"))?;

        Ok(Box::new(
            NativeSerialSession::open(descriptor, config).await?,
        ))
    }
}

fn descriptor_from_port(port: SerialPortInfo) -> HalResult<ResourceDescriptor> {
    let endpoint = Endpoint::new(port.port_name.clone())?;
    let identity = identity_from_port(&port)?;
    let properties = properties_from_port(&port);

    Ok(ResourceDescriptor::new(
        identity.id,
        endpoint,
        identity.quality,
        TransportKind::Serial,
        properties,
    ))
}

fn identity_from_port(port: &SerialPortInfo) -> HalResult<SerialIdentity> {
    match &port.port_type {
        SerialPortType::UsbPort(info) => crate::identity::identity_from_usb_metadata(
            &port.port_name,
            &UsbPortMetadata {
                vid: info.vid,
                pid: info.pid,
                serial_number: info.serial_number.clone(),
                manufacturer: info.manufacturer.clone(),
                product: info.product.clone(),
            },
        ),
        SerialPortType::PciPort | SerialPortType::BluetoothPort | SerialPortType::Unknown => {
            identity_from_endpoint(&port.port_name)
        }
    }
}

fn properties_from_port(port: &SerialPortInfo) -> ResourceProperties {
    let mut properties = BTreeMap::new();
    properties.insert("adapter".to_owned(), "serialport".to_owned());
    properties.insert("endpoint".to_owned(), port.port_name.clone());

    match &port.port_type {
        SerialPortType::UsbPort(info) => {
            properties.insert("port_type".to_owned(), "usb".to_owned());
            properties.insert("usb.vid".to_owned(), format!("{:04x}", info.vid));
            properties.insert("usb.pid".to_owned(), format!("{:04x}", info.pid));
            insert_optional(
                &mut properties,
                "usb.serial_number",
                info.serial_number.as_deref(),
            );
            insert_optional(
                &mut properties,
                "usb.manufacturer",
                info.manufacturer.as_deref(),
            );
            insert_optional(&mut properties, "usb.product", info.product.as_deref());
        }
        SerialPortType::PciPort => {
            properties.insert("port_type".to_owned(), "pci".to_owned());
        }
        SerialPortType::BluetoothPort => {
            properties.insert("port_type".to_owned(), "bluetooth".to_owned());
        }
        SerialPortType::Unknown => {
            properties.insert("port_type".to_owned(), "unknown".to_owned());
        }
    }

    ResourceProperties::new(properties)
}

fn insert_optional(properties: &mut BTreeMap<String, String>, key: &str, value: Option<&str>) {
    if let Some(value) = value.filter(|value| !value.is_empty()) {
        properties.insert(key.to_owned(), value.to_owned());
    }
}

pub(crate) fn map_serialport_error(operation: &'static str, error: serialport::Error) -> HalError {
    let (name, category, retryable) = match error.kind() {
        serialport::ErrorKind::NoDevice => {
            ("runtime.resource.not_found", ErrorCategory::NotFound, false)
        }
        serialport::ErrorKind::InvalidInput => (
            "runtime.transport.unsupported_configuration",
            ErrorCategory::InvalidArgument,
            false,
        ),
        serialport::ErrorKind::Io(kind) => io_error_decision(kind),
        serialport::ErrorKind::Unknown => (
            "runtime.transport.disconnected",
            ErrorCategory::Unavailable,
            true,
        ),
    };

    hal_error(
        name,
        category,
        operation,
        retryable,
        format!("serialport error {:?}: {}", error.kind(), error),
    )
}

pub(crate) fn map_io_error(operation: &'static str, error: std::io::Error) -> HalError {
    let (name, category, retryable) = io_error_decision(error.kind());
    hal_error(
        name,
        category,
        operation,
        retryable,
        format!("io error {:?}: {}", error.kind(), error),
    )
}

fn io_error_decision(kind: std::io::ErrorKind) -> (&'static str, ErrorCategory, bool) {
    match kind {
        std::io::ErrorKind::NotFound => {
            ("runtime.resource.not_found", ErrorCategory::NotFound, false)
        }
        std::io::ErrorKind::PermissionDenied => (
            "runtime.transport.permission_denied",
            ErrorCategory::Conflict,
            false,
        ),
        std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock => (
            "runtime.transport.timeout",
            ErrorCategory::Unavailable,
            true,
        ),
        std::io::ErrorKind::AddrInUse | std::io::ErrorKind::AlreadyExists => {
            ("runtime.transport.busy", ErrorCategory::Conflict, true)
        }
        std::io::ErrorKind::ConnectionAborted
        | std::io::ErrorKind::ConnectionReset
        | std::io::ErrorKind::BrokenPipe
        | std::io::ErrorKind::NotConnected
        | std::io::ErrorKind::UnexpectedEof => (
            "runtime.transport.disconnected",
            ErrorCategory::Unavailable,
            true,
        ),
        std::io::ErrorKind::InvalidInput | std::io::ErrorKind::InvalidData => (
            "runtime.transport.unsupported_configuration",
            ErrorCategory::InvalidArgument,
            false,
        ),
        _ => (
            "runtime.transport.disconnected",
            ErrorCategory::Unavailable,
            true,
        ),
    }
}

pub(crate) fn invalid_argument(
    operation: &'static str,
    debug_message: impl Into<String>,
) -> HalError {
    hal_error(
        "runtime.argument.invalid",
        ErrorCategory::InvalidArgument,
        operation,
        false,
        debug_message,
    )
}

pub(crate) fn not_found(operation: &'static str, debug_message: impl Into<String>) -> HalError {
    hal_error(
        "runtime.resource.not_found",
        ErrorCategory::NotFound,
        operation,
        false,
        debug_message,
    )
}

pub(crate) fn unsupported_configuration(
    operation: &'static str,
    debug_message: impl Into<String>,
) -> HalError {
    hal_error(
        "runtime.transport.unsupported_configuration",
        ErrorCategory::InvalidArgument,
        operation,
        false,
        debug_message,
    )
}

pub(crate) fn timeout(operation: &'static str, debug_message: impl Into<String>) -> HalError {
    hal_error(
        "runtime.transport.timeout",
        ErrorCategory::Unavailable,
        operation,
        true,
        debug_message,
    )
}

pub(crate) fn session_closed(
    operation: &'static str,
    debug_message: impl Into<String>,
) -> HalError {
    hal_error(
        "runtime.session.closed",
        ErrorCategory::Conflict,
        operation,
        false,
        debug_message,
    )
}

fn internal(operation: &'static str, debug_message: impl Into<String>) -> HalError {
    hal_error(
        "runtime.internal",
        ErrorCategory::Internal,
        operation,
        true,
        debug_message,
    )
}

fn hal_error(
    name: &'static str,
    category: ErrorCategory,
    operation: &'static str,
    retryable: bool,
    debug_message: impl Into<String>,
) -> HalError {
    HalError::new(name, category, operation, retryable, debug_message)
        .expect("static serialport adapter error metadata must be valid")
}
