#![deny(unsafe_op_in_unsafe_fn)]

pub mod identity;
mod session;

use async_trait::async_trait;
use seeed_hal_core::{
    CapabilitySet, Endpoint, ErrorCategory, HalError, HalResult, ResourceDescriptor,
    ResourceProperties, ResourceSelector, TransportKind, resolve_resource,
};
use seeed_hal_serial::{SerialAdapter, SerialConfig, SerialSession, serial_bytes_capability};
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

        let descriptors = self.enumerate().await?;
        let descriptor = resolve_resource(
            &descriptors,
            selector,
            &serial_bytes_capability(),
            "serial.open",
        )?
        .clone();

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
        CapabilitySet::new(vec![serial_bytes_capability()]),
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
    let description = error.to_string();
    let (name, category, retryable) = match error.kind() {
        serialport::ErrorKind::NoDevice => no_device_decision(operation, &description),
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
        format!(
            "serialport error kind={:?} raw_os_error=unavailable: {}",
            error.kind(),
            description
        ),
    )
}

pub(crate) fn map_io_error(operation: &'static str, error: std::io::Error) -> HalError {
    let (name, category, retryable) = io_error_decision(error.kind());
    let raw_os_error = error
        .raw_os_error()
        .map(|code| code.to_string())
        .unwrap_or_else(|| "none".to_owned());
    hal_error(
        name,
        category,
        operation,
        retryable,
        format!(
            "io error kind={:?} raw_os_error={}: {}",
            error.kind(),
            raw_os_error,
            error
        ),
    )
}

fn no_device_decision(
    operation: &'static str,
    description: &str,
) -> (&'static str, ErrorCategory, bool) {
    let normalized = description.to_ascii_lowercase();

    if normalized.contains("busy") || normalized.contains("lock") {
        return ("runtime.transport.busy", ErrorCategory::Conflict, true);
    }

    if normalized.contains("access is denied") || normalized.contains("permission denied") {
        return (
            "runtime.transport.permission_denied",
            ErrorCategory::Conflict,
            false,
        );
    }

    if operation == "serial.open" || operation == "serial.enumerate" {
        return ("runtime.resource.not_found", ErrorCategory::NotFound, false);
    }

    (
        "runtime.transport.disconnected",
        ErrorCategory::Unavailable,
        true,
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

pub(crate) fn internal(operation: &'static str, debug_message: impl Into<String>) -> HalError {
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

#[cfg(test)]
mod tests {
    use std::io;

    use super::*;

    #[test]
    fn serialport_no_device_from_exclusive_lock_maps_to_busy() {
        let error = serialport::Error::new(
            serialport::ErrorKind::NoDevice,
            "Unable to acquire exclusive lock on serial port",
        );

        let error = map_serialport_error("serial.open", error);

        assert_eq!(error.name().as_str(), "runtime.transport.busy");
    }

    #[test]
    fn serialport_no_device_from_access_denied_maps_to_permission_denied() {
        let error = serialport::Error::new(serialport::ErrorKind::NoDevice, "Access is denied.");

        let error = map_serialport_error("serial.open", error);

        assert_eq!(error.name().as_str(), "runtime.transport.permission_denied");
    }

    #[test]
    fn serialport_no_device_without_busy_or_permission_signal_maps_to_not_found() {
        let error = serialport::Error::new(serialport::ErrorKind::NoDevice, "No such file");

        let error = map_serialport_error("serial.open", error);

        assert_eq!(error.name().as_str(), "runtime.resource.not_found");
    }

    #[test]
    fn io_error_diagnostics_preserve_raw_os_error_code() {
        let io_error = io::Error::from_raw_os_error(13);

        let error = map_io_error("serial.open", io_error);

        assert!(error.debug_message().contains("raw_os_error=13"));
    }
}
