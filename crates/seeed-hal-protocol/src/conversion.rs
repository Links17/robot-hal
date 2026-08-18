use std::collections::BTreeMap;
use std::time::Duration;

use seeed_hal_core::{
    CapabilityId, CapabilitySet, Endpoint, ErrorCategory, ErrorContext, HalError, HalResult,
    IdentityQuality, LeaseId, LeaseMode, LeaseToken, ResourceDescriptor, ResourceId,
    ResourceProperties, ResourceSelector, SessionId, TransportKind,
};
use seeed_hal_gpio::{
    EdgeMask, GpioBias, GpioDirection, GpioDrive, GpioEdge, GpioEdgeEvent, GpioEdgeRequest,
    GpioLineConfig, MAX_GPIO_EVENTS,
};
use seeed_hal_runtime::{RuntimeEvent, RuntimeEventKind};
use seeed_hal_serial::{DataBits, FlowControl, Parity, SerialConfig, StopBits};
use seeed_hal_usb::{MAX_USB_INTERFACE_NUMBER, MAX_USB_TRANSFER_BYTES, UsbTransfer};

use crate::v1;

pub fn invalid_message(debug_message: impl Into<String>) -> HalError {
    HalError::new(
        "runtime.protocol.invalid_message",
        ErrorCategory::InvalidArgument,
        "runtime.protocol.decode",
        false,
        debug_message,
    )
    .expect("static protocol error metadata is valid")
}

pub fn usb_transfer_from_proto(value: v1::UsbTransferRequest) -> HalResult<UsbTransfer> {
    let kind = required_enum::<v1::UsbTransferKind>(value.kind, "usb_transfer.kind")?;
    let byte = |value: u32, field: &'static str| {
        u8::try_from(value).map_err(|_| invalid_message(format!("{field} exceeds u8")))
    };
    let u16_value = |value: u32, field: &'static str| {
        u16::try_from(value).map_err(|_| invalid_message(format!("{field} exceeds u16")))
    };
    let max = usize::try_from(value.max_bytes)
        .map_err(|_| invalid_message("usb_transfer.max_bytes is invalid"))?;
    let data = value.data.into();
    let transfer = match kind {
        v1::UsbTransferKind::ControlOut => {
            if value.endpoint != 0 || value.max_bytes != 0 {
                return Err(invalid_message(
                    "control-out transfer must not set endpoint or max_bytes",
                ));
            }
            UsbTransfer::control_out(
                byte(value.request_type, "usb_transfer.request_type")?,
                byte(value.request, "usb_transfer.request")?,
                u16_value(value.value, "usb_transfer.value")?,
                u16_value(value.index, "usb_transfer.index")?,
                data,
            )
        }
        v1::UsbTransferKind::ControlIn => {
            if value.endpoint != 0 || !data.is_empty() {
                return Err(invalid_message(
                    "control-in transfer must not set endpoint or data",
                ));
            }
            UsbTransfer::control_in(
                byte(value.request_type, "usb_transfer.request_type")?,
                byte(value.request, "usb_transfer.request")?,
                u16_value(value.value, "usb_transfer.value")?,
                u16_value(value.index, "usb_transfer.index")?,
                max,
            )
        }
        v1::UsbTransferKind::BulkOut => {
            if value.request_type != 0
                || value.request != 0
                || value.value != 0
                || value.index != 0
                || value.max_bytes != 0
            {
                return Err(invalid_message(
                    "bulk-out transfer has control or max_bytes fields",
                ));
            }
            UsbTransfer::bulk_out(byte(value.endpoint, "usb_transfer.endpoint")?, data)
        }
        v1::UsbTransferKind::BulkIn => {
            if value.request_type != 0
                || value.request != 0
                || value.value != 0
                || value.index != 0
                || !data.is_empty()
            {
                return Err(invalid_message(
                    "bulk-in transfer has control or data fields",
                ));
            }
            UsbTransfer::bulk_in(byte(value.endpoint, "usb_transfer.endpoint")?, max)
        }
        v1::UsbTransferKind::InterruptOut => {
            if value.request_type != 0
                || value.request != 0
                || value.value != 0
                || value.index != 0
                || value.max_bytes != 0
            {
                return Err(invalid_message(
                    "interrupt-out transfer has control or max_bytes fields",
                ));
            }
            UsbTransfer::interrupt_out(byte(value.endpoint, "usb_transfer.endpoint")?, data)
        }
        v1::UsbTransferKind::InterruptIn => {
            if value.request_type != 0
                || value.request != 0
                || value.value != 0
                || value.index != 0
                || !data.is_empty()
            {
                return Err(invalid_message(
                    "interrupt-in transfer has control or data fields",
                ));
            }
            UsbTransfer::interrupt_in(byte(value.endpoint, "usb_transfer.endpoint")?, max)
        }
        v1::UsbTransferKind::Unspecified => Err(invalid_message("usb_transfer.kind is required")),
    };
    transfer.map_err(|_| invalid_message("usb_transfer violates the public USB transfer bounds"))
}

pub fn usb_transfer_request_from_proto(
    value: v1::UsbTransferRequest,
) -> HalResult<(SessionId, LeaseToken, UsbTransfer, Duration)> {
    let timeout = nonzero_timeout(value.timeout_ms, "usb_transfer.timeout_ms")?;
    let session_id = value.session_id.clone();
    let lease = value.lease.clone();
    let (session, lease) = parse_control_session_lease(session_id, lease, "USB")?;
    let transfer = usb_transfer_from_proto(value)?;
    Ok((session, lease, transfer, timeout))
}

pub fn gpio_config_from_proto(value: v1::GpioLineConfig) -> HalResult<GpioLineConfig> {
    let direction = required_enum::<v1::GpioDirection>(value.direction, "gpio_config.direction")?;
    let bias = match required_enum::<v1::GpioBias>(value.bias, "gpio_config.bias")? {
        v1::GpioBias::Disabled => GpioBias::Disabled,
        v1::GpioBias::PullUp => GpioBias::PullUp,
        v1::GpioBias::PullDown => GpioBias::PullDown,
        v1::GpioBias::Unspecified => return Err(invalid_message("gpio_config.bias is required")),
    };
    match direction {
        v1::GpioDirection::Input => GpioLineConfig::input(value.active_low, bias),
        v1::GpioDirection::Output => {
            let drive = match required_enum::<v1::GpioDrive>(value.drive, "gpio_config.drive")? {
                v1::GpioDrive::PushPull => GpioDrive::PushPull,
                v1::GpioDrive::OpenDrain => GpioDrive::OpenDrain,
                v1::GpioDrive::OpenSource => GpioDrive::OpenSource,
                v1::GpioDrive::Unspecified => {
                    return Err(invalid_message("gpio_config.drive is required"));
                }
            };
            GpioLineConfig::output(
                value.active_low,
                value.initial_value.ok_or_else(|| {
                    invalid_message("gpio_config.initial_value is required for output")
                })?,
                drive,
            )
        }
        v1::GpioDirection::Unspecified => Err(invalid_message("gpio_config.direction is required")),
    }
    .map_err(|_| invalid_message("gpio_config violates the public GPIO configuration bounds"))
}

pub fn usb_selector_from_proto(value: v1::ResourceSelector) -> HalResult<ResourceSelector> {
    selector_for_transport(value, TransportKind::Usb, "USB")
}

pub fn gpio_selector_from_proto(value: v1::ResourceSelector) -> HalResult<ResourceSelector> {
    selector_for_transport(value, TransportKind::Gpio, "GPIO")
}

pub fn open_usb_request_from_proto(value: v1::OpenUsbRequest) -> HalResult<(ResourceSelector, u8)> {
    let selector = value
        .selector
        .ok_or_else(|| invalid_message("open_usb selector is required"))?;
    let interface = usize::try_from(value.interface_number)
        .map_err(|_| invalid_message("open_usb interface_number is invalid"))?;
    if interface > MAX_USB_INTERFACE_NUMBER {
        return Err(invalid_message("open_usb interface_number exceeds u8"));
    }
    Ok((usb_selector_from_proto(selector)?, interface as u8))
}

pub fn open_gpio_request_from_proto(
    value: v1::OpenGpioRequest,
) -> HalResult<(ResourceSelector, Vec<u32>, GpioLineConfig)> {
    let selector = value
        .selector
        .ok_or_else(|| invalid_message("open_gpio selector is required"))?;
    if value.lines.is_empty() || value.lines.len() > MAX_GPIO_EVENTS {
        return Err(invalid_message(
            "open_gpio lines must be non-empty and bounded",
        ));
    }
    let config = value
        .config
        .ok_or_else(|| invalid_message("open_gpio config is required"))?;
    Ok((
        gpio_selector_from_proto(selector)?,
        value.lines,
        gpio_config_from_proto(config)?,
    ))
}

pub fn gpio_edge_request_from_proto(value: v1::GpioNextEdgeRequest) -> HalResult<GpioEdgeRequest> {
    let edges = match (value.rising, value.falling) {
        (true, true) => EdgeMask::BOTH,
        (true, false) => EdgeMask::RISING,
        (false, true) => EdgeMask::FALLING,
        (false, false) => return Err(invalid_message("gpio_next_edge must select an edge")),
    };
    GpioEdgeRequest::new(
        edges,
        usize::try_from(value.capacity)
            .map_err(|_| invalid_message("gpio_next_edge capacity is invalid"))?,
    )
    .map_err(|_| invalid_message("gpio_next_edge capacity violates public bounds"))
}

pub fn gpio_read_request_from_proto(
    value: v1::GpioReadRequest,
) -> HalResult<(SessionId, LeaseToken)> {
    parse_control_session_lease(value.session_id, value.lease, "GPIO")
}

pub fn usb_close_request_from_proto(
    value: v1::CloseUsbRequest,
) -> HalResult<(SessionId, LeaseToken)> {
    parse_control_session_lease(value.session_id, value.lease, "USB")
}

pub fn gpio_close_request_from_proto(
    value: v1::CloseGpioRequest,
) -> HalResult<(SessionId, LeaseToken)> {
    parse_control_session_lease(value.session_id, value.lease, "GPIO")
}

pub fn gpio_write_request_from_proto(
    value: v1::GpioWriteRequest,
) -> HalResult<(SessionId, LeaseToken, Vec<bool>)> {
    let (session, lease) = parse_control_session_lease(value.session_id, value.lease, "GPIO")?;
    if value.values.is_empty() || value.values.len() > MAX_GPIO_EVENTS {
        return Err(invalid_message(
            "gpio_write values must be non-empty and bounded",
        ));
    }
    Ok((session, lease, value.values))
}

pub fn gpio_next_edge_request_from_proto(
    value: v1::GpioNextEdgeRequest,
) -> HalResult<(SessionId, LeaseToken, GpioEdgeRequest, Duration)> {
    let timeout = nonzero_timeout(value.timeout_ms, "gpio_next_edge.timeout_ms")?;
    let session_id = value.session_id.clone();
    let lease_token = value.lease.clone();
    let (session, lease) = parse_control_session_lease(session_id, lease_token, "GPIO")?;
    let request = gpio_edge_request_from_proto(value)?;
    Ok((session, lease, request, timeout))
}

pub fn usb_transfer_response_from_proto(value: v1::UsbTransferResponse) -> HalResult<bytes::Bytes> {
    if value.data.len() > MAX_USB_TRANSFER_BYTES {
        return Err(invalid_message(
            "usb_transfer_response data exceeds the public USB transfer bound",
        ));
    }
    Ok(value.data.into())
}

pub fn usb_transfer_response_to_proto(value: bytes::Bytes) -> v1::UsbTransferResponse {
    v1::UsbTransferResponse {
        data: value.to_vec(),
    }
}

pub fn gpio_read_response_from_proto(value: v1::GpioReadResponse) -> HalResult<Vec<bool>> {
    if value.values.is_empty() || value.values.len() > MAX_GPIO_EVENTS {
        return Err(invalid_message(
            "gpio_read_response values must be non-empty and bounded",
        ));
    }
    Ok(value.values)
}

pub fn gpio_read_response_to_proto(values: &[bool]) -> v1::GpioReadResponse {
    v1::GpioReadResponse {
        values: values.to_vec(),
    }
}

pub fn gpio_next_edge_response_from_proto(
    value: v1::GpioNextEdgeResponse,
) -> HalResult<Option<GpioEdgeEvent>> {
    value.event.map(gpio_edge_event_from_proto).transpose()
}

pub fn gpio_next_edge_response_to_proto(value: Option<GpioEdgeEvent>) -> v1::GpioNextEdgeResponse {
    v1::GpioNextEdgeResponse {
        event: value.as_ref().map(Into::into),
    }
}

pub fn open_usb_response_from_proto(
    value: v1::OpenUsbResponse,
) -> HalResult<(SessionId, LeaseToken)> {
    parse_control_session_lease(value.session_id, value.lease, "USB")
}

pub fn open_gpio_response_from_proto(
    value: v1::OpenGpioResponse,
) -> HalResult<(SessionId, LeaseToken)> {
    parse_control_session_lease(value.session_id, value.lease, "GPIO")
}

pub fn open_usb_response_to_proto(session: &SessionId, lease: &LeaseToken) -> v1::OpenUsbResponse {
    v1::OpenUsbResponse {
        session_id: session.as_str().to_owned(),
        lease: Some(lease.into()),
    }
}

pub fn open_gpio_response_to_proto(
    session: &SessionId,
    lease: &LeaseToken,
) -> v1::OpenGpioResponse {
    v1::OpenGpioResponse {
        session_id: session.as_str().to_owned(),
        lease: Some(lease.into()),
    }
}

pub fn gpio_edge_event_from_proto(value: v1::GpioEdgeEvent) -> HalResult<GpioEdgeEvent> {
    let edge = match required_enum::<v1::GpioEdge>(value.edge, "gpio_edge_event.edge")? {
        v1::GpioEdge::Rising => GpioEdge::Rising,
        v1::GpioEdge::Falling => GpioEdge::Falling,
        v1::GpioEdge::Unspecified => {
            return Err(invalid_message("gpio_edge_event.edge is required"));
        }
    };
    if value.sequence == 0 {
        return Err(invalid_message(
            "gpio_edge_event.sequence must be greater than zero",
        ));
    }
    Ok(GpioEdgeEvent::new(edge, value.monotonic_ns, value.sequence))
}

impl From<&GpioLineConfig> for v1::GpioLineConfig {
    fn from(value: &GpioLineConfig) -> Self {
        let (direction, drive, initial_value) = match value.direction() {
            GpioDirection::Input => (v1::GpioDirection::Input, v1::GpioDrive::Unspecified, None),
            GpioDirection::Output => (
                v1::GpioDirection::Output,
                match value.drive().expect("output config carries drive") {
                    GpioDrive::PushPull => v1::GpioDrive::PushPull,
                    GpioDrive::OpenDrain => v1::GpioDrive::OpenDrain,
                    GpioDrive::OpenSource => v1::GpioDrive::OpenSource,
                },
                value.initial_value(),
            ),
        };
        Self {
            direction: direction as i32,
            active_low: value.active_low(),
            bias: match value.bias() {
                GpioBias::Disabled => v1::GpioBias::Disabled,
                GpioBias::PullUp => v1::GpioBias::PullUp,
                GpioBias::PullDown => v1::GpioBias::PullDown,
            } as i32,
            drive: drive as i32,
            initial_value,
        }
    }
}

impl From<&GpioEdgeEvent> for v1::GpioEdgeEvent {
    fn from(value: &GpioEdgeEvent) -> Self {
        Self {
            edge: match value.edge() {
                GpioEdge::Rising => v1::GpioEdge::Rising,
                GpioEdge::Falling => v1::GpioEdge::Falling,
            } as i32,
            monotonic_ns: value.monotonic_ns(),
            sequence: value.sequence(),
        }
    }
}

pub(crate) fn required_enum<T: TryFrom<i32>>(value: i32, field: &'static str) -> HalResult<T> {
    T::try_from(value).map_err(|_| invalid_message(format!("{field} has an unknown value")))
}

impl TryFrom<v1::ResourceSelector> for ResourceSelector {
    type Error = HalError;

    fn try_from(value: v1::ResourceSelector) -> HalResult<Self> {
        let id = ResourceId::parse(value.resource_id)
            .map_err(|_| invalid_message("resource selector has an invalid resource_id"))?;
        let quality = match required_enum::<v1::IdentityQuality>(
            value.minimum_identity_quality,
            "minimum_identity_quality",
        )? {
            v1::IdentityQuality::Weak => IdentityQuality::Weak,
            v1::IdentityQuality::Medium => IdentityQuality::Medium,
            v1::IdentityQuality::Strong => IdentityQuality::Strong,
            v1::IdentityQuality::Unspecified => {
                return Err(invalid_message("minimum_identity_quality is required"));
            }
        };
        let transport = match required_enum::<v1::TransportKind>(value.transport, "transport")? {
            v1::TransportKind::Serial => TransportKind::Serial,
            v1::TransportKind::Can => TransportKind::Can,
            v1::TransportKind::Usb => TransportKind::Usb,
            v1::TransportKind::Gpio => TransportKind::Gpio,
            v1::TransportKind::Camera => TransportKind::Camera,
            v1::TransportKind::Unspecified => {
                return Err(invalid_message("transport is required"));
            }
        };
        Ok(Self::exact(id, quality, transport))
    }
}

impl TryFrom<&ResourceSelector> for v1::ResourceSelector {
    type Error = HalError;

    fn try_from(value: &ResourceSelector) -> HalResult<Self> {
        Ok(Self {
            resource_id: value.id().as_str().to_owned(),
            minimum_identity_quality: quality_to_proto(value.minimum_identity_quality()) as i32,
            transport: transport_to_proto(value.transport())? as i32,
        })
    }
}

impl TryFrom<v1::ResourceDescriptor> for ResourceDescriptor {
    type Error = HalError;

    fn try_from(value: v1::ResourceDescriptor) -> HalResult<Self> {
        let selector = ResourceSelector::try_from(v1::ResourceSelector {
            resource_id: value.resource_id,
            minimum_identity_quality: value.identity_quality,
            transport: value.transport,
        })?;
        let endpoint = Endpoint::new(value.endpoint)
            .map_err(|_| invalid_message("resource descriptor has an invalid endpoint"))?;
        let capabilities = if value.capabilities.is_empty() {
            if selector.transport() != TransportKind::Serial {
                return Err(invalid_message(
                    "CAN resource descriptor requires explicit capabilities",
                ));
            }
            vec![
                CapabilityId::parse(crate::SERIAL_CAPABILITY)
                    .expect("the static Serial capability identifier is valid"),
            ]
        } else {
            value
                .capabilities
                .into_iter()
                .map(|capability| {
                    CapabilityId::parse(capability).map_err(|_| {
                        invalid_message("resource descriptor has an invalid capability")
                    })
                })
                .collect::<HalResult<Vec<_>>>()?
        };
        Ok(Self::new(
            selector.id().clone(),
            endpoint,
            selector.minimum_identity_quality(),
            selector.transport(),
            ResourceProperties::new(value.properties.into_iter().collect::<BTreeMap<_, _>>()),
            CapabilitySet::new(capabilities),
        ))
    }
}

impl TryFrom<&ResourceDescriptor> for v1::ResourceDescriptor {
    type Error = HalError;

    fn try_from(value: &ResourceDescriptor) -> HalResult<Self> {
        Ok(Self {
            resource_id: value.id().as_str().to_owned(),
            endpoint: value.endpoint().as_str().to_owned(),
            identity_quality: quality_to_proto(value.minimum_identity_quality()) as i32,
            transport: transport_to_proto(value.transport())? as i32,
            properties: value
                .properties()
                .iter()
                .map(|(key, value)| (key.to_owned(), value.to_owned()))
                .collect(),
            capabilities: value
                .capabilities()
                .as_slice()
                .iter()
                .map(|capability| capability.as_str().to_owned())
                .collect(),
        })
    }
}

impl TryFrom<v1::SerialConfig> for SerialConfig {
    type Error = HalError;

    fn try_from(value: v1::SerialConfig) -> HalResult<Self> {
        if value.baud_rate == 0 || value.read_timeout_ms == 0 {
            return Err(invalid_message(
                "baud_rate and read_timeout_ms must be greater than zero",
            ));
        }
        Ok(Self {
            baud_rate: value.baud_rate,
            data_bits: match required_enum::<v1::DataBits>(value.data_bits, "data_bits")? {
                v1::DataBits::Five => DataBits::Five,
                v1::DataBits::Six => DataBits::Six,
                v1::DataBits::Seven => DataBits::Seven,
                v1::DataBits::Eight => DataBits::Eight,
                v1::DataBits::Unspecified => return Err(invalid_message("data_bits is required")),
            },
            parity: match required_enum::<v1::Parity>(value.parity, "parity")? {
                v1::Parity::None => Parity::None,
                v1::Parity::Odd => Parity::Odd,
                v1::Parity::Even => Parity::Even,
                v1::Parity::Unspecified => return Err(invalid_message("parity is required")),
            },
            stop_bits: match required_enum::<v1::StopBits>(value.stop_bits, "stop_bits")? {
                v1::StopBits::One => StopBits::One,
                v1::StopBits::Two => StopBits::Two,
                v1::StopBits::Unspecified => {
                    return Err(invalid_message("stop_bits is required"));
                }
            },
            flow_control: match required_enum::<v1::FlowControl>(
                value.flow_control,
                "flow_control",
            )? {
                v1::FlowControl::None => FlowControl::None,
                v1::FlowControl::Software => FlowControl::Software,
                v1::FlowControl::Hardware => FlowControl::Hardware,
                v1::FlowControl::Unspecified => {
                    return Err(invalid_message("flow_control is required"));
                }
            },
            read_timeout: Duration::from_millis(value.read_timeout_ms),
        })
    }
}

impl From<&SerialConfig> for v1::SerialConfig {
    fn from(value: &SerialConfig) -> Self {
        Self {
            baud_rate: value.baud_rate,
            data_bits: match value.data_bits {
                DataBits::Five => v1::DataBits::Five,
                DataBits::Six => v1::DataBits::Six,
                DataBits::Seven => v1::DataBits::Seven,
                DataBits::Eight => v1::DataBits::Eight,
            } as i32,
            parity: match value.parity {
                Parity::None => v1::Parity::None,
                Parity::Odd => v1::Parity::Odd,
                Parity::Even => v1::Parity::Even,
            } as i32,
            stop_bits: match value.stop_bits {
                StopBits::One => v1::StopBits::One,
                StopBits::Two => v1::StopBits::Two,
            } as i32,
            flow_control: match value.flow_control {
                FlowControl::None => v1::FlowControl::None,
                FlowControl::Software => v1::FlowControl::Software,
                FlowControl::Hardware => v1::FlowControl::Hardware,
            } as i32,
            read_timeout_ms: value
                .read_timeout
                .as_millis()
                .try_into()
                .unwrap_or(u64::MAX),
        }
    }
}

impl TryFrom<v1::LeaseToken> for LeaseToken {
    type Error = HalError;

    fn try_from(value: v1::LeaseToken) -> HalResult<Self> {
        let lease_id = LeaseId::parse(value.lease_id)
            .map_err(|_| invalid_message("lease token has an invalid lease_id"))?;
        if value.generation == 0 {
            return Err(invalid_message(
                "lease generation must be greater than zero",
            ));
        }
        let mode = match required_enum::<v1::LeaseMode>(value.mode, "lease mode")? {
            v1::LeaseMode::Observe => LeaseMode::Observe,
            v1::LeaseMode::Control => LeaseMode::Control,
            v1::LeaseMode::Maintenance => LeaseMode::Maintenance,
            v1::LeaseMode::Unspecified => return Err(invalid_message("lease mode is required")),
        };
        Ok(Self::new(lease_id, value.generation, mode))
    }
}

impl From<&LeaseToken> for v1::LeaseToken {
    fn from(value: &LeaseToken) -> Self {
        Self {
            lease_id: value.lease_id().as_str().to_owned(),
            generation: value.generation(),
            mode: match value.mode() {
                LeaseMode::Observe => v1::LeaseMode::Observe,
                LeaseMode::Control => v1::LeaseMode::Control,
                LeaseMode::Maintenance => v1::LeaseMode::Maintenance,
            } as i32,
        }
    }
}

pub fn parse_session_lease(
    session_id: String,
    lease: Option<v1::LeaseToken>,
) -> HalResult<(SessionId, LeaseToken)> {
    let session = SessionId::parse(session_id)
        .map_err(|_| invalid_message("request has an invalid session_id"))?;
    let lease = lease.ok_or_else(|| invalid_message("request is missing lease"))?;
    Ok((session, lease.try_into()?))
}

pub fn serial_selector_from_proto(value: v1::ResourceSelector) -> HalResult<ResourceSelector> {
    let selector = ResourceSelector::try_from(value)?;
    if selector.transport() != TransportKind::Serial {
        return Err(invalid_message(
            "serial resource selector transport must be Serial",
        ));
    }
    Ok(selector)
}

pub fn enumerate_serial_response_from_proto(
    value: v1::EnumerateSerialResponse,
) -> HalResult<Vec<ResourceDescriptor>> {
    value
        .resources
        .into_iter()
        .map(|value| {
            let descriptor = ResourceDescriptor::try_from(value)?;
            if descriptor.transport() != TransportKind::Serial {
                return Err(invalid_message(
                    "enumerate_serial resource transport must be Serial",
                ));
            }
            Ok(descriptor)
        })
        .collect()
}

pub fn open_serial_request_from_proto(
    value: v1::OpenSerialRequest,
) -> HalResult<(ResourceSelector, SerialConfig)> {
    let selector = value
        .selector
        .ok_or_else(|| invalid_message("open_serial selector is required"))?;
    let config = value
        .config
        .ok_or_else(|| invalid_message("open_serial config is required"))?;
    Ok((serial_selector_from_proto(selector)?, config.try_into()?))
}

pub fn parse_serial_session_lease(
    session_id: String,
    lease: Option<v1::LeaseToken>,
) -> HalResult<(SessionId, LeaseToken)> {
    let (session, lease) = parse_session_lease(session_id, lease)?;
    if lease.mode() != LeaseMode::Control {
        return Err(invalid_message("Serial session lease mode must be Control"));
    }
    Ok((session, lease))
}

fn parse_control_session_lease(
    session_id: String,
    lease: Option<v1::LeaseToken>,
    hardware_class: &'static str,
) -> HalResult<(SessionId, LeaseToken)> {
    let (session, lease) = parse_session_lease(session_id, lease)?;
    if lease.mode() != LeaseMode::Control {
        return Err(invalid_message(format!(
            "{hardware_class} session lease mode must be Control"
        )));
    }
    Ok((session, lease))
}

fn nonzero_timeout(value: u64, field: &'static str) -> HalResult<Duration> {
    if value == 0 {
        return Err(invalid_message(format!(
            "{field} must be greater than zero"
        )));
    }
    Ok(Duration::from_millis(value))
}

fn selector_for_transport(
    value: v1::ResourceSelector,
    expected: TransportKind,
    label: &'static str,
) -> HalResult<ResourceSelector> {
    let selector = ResourceSelector::try_from(value)?;
    if selector.transport() != expected {
        return Err(invalid_message(format!(
            "{label} resource selector transport does not match"
        )));
    }
    Ok(selector)
}

pub fn open_serial_response_from_proto(
    value: v1::OpenSerialResponse,
) -> HalResult<(SessionId, LeaseToken)> {
    parse_serial_session_lease(value.session_id, value.lease)
}

impl From<&HalError> for v1::Error {
    fn from(value: &HalError) -> Self {
        Self {
            name: value.name().as_str().to_owned(),
            category: match value.category() {
                ErrorCategory::InvalidArgument => v1::ErrorCategory::InvalidArgument,
                ErrorCategory::NotFound => v1::ErrorCategory::NotFound,
                ErrorCategory::Conflict => v1::ErrorCategory::Conflict,
                ErrorCategory::Unavailable => v1::ErrorCategory::Unavailable,
                ErrorCategory::Internal => v1::ErrorCategory::Internal,
            } as i32,
            operation: value.operation().as_str().to_owned(),
            retryable: value.retryable(),
            debug_message: value.debug_message().to_owned(),
            resource_id: value
                .resource_id()
                .map_or_else(String::new, |resource_id| resource_id.as_str().to_owned()),
            platform_code: value.platform_code().unwrap_or_default().to_owned(),
            vendor_code: value.vendor_code().unwrap_or_default().to_owned(),
            context: value
                .context()
                .iter()
                .map(|(key, value)| (key.to_owned(), value.to_owned()))
                .collect(),
        }
    }
}

/// Decode a wire error, rejecting malformed peer-supplied details uniformly.
pub fn error_from_proto(value: v1::Error) -> HalResult<HalError> {
    let v1::Error {
        name,
        category,
        operation,
        retryable,
        debug_message,
        resource_id,
        platform_code,
        vendor_code,
        context,
    } = value;

    let category = match required_enum::<v1::ErrorCategory>(category, "category")? {
        v1::ErrorCategory::InvalidArgument => ErrorCategory::InvalidArgument,
        v1::ErrorCategory::NotFound => ErrorCategory::NotFound,
        v1::ErrorCategory::Conflict => ErrorCategory::Conflict,
        v1::ErrorCategory::Unavailable => ErrorCategory::Unavailable,
        v1::ErrorCategory::Internal => ErrorCategory::Internal,
        v1::ErrorCategory::Unspecified => {
            return Err(invalid_message("error category is required"));
        }
    };

    let mut error = HalError::new(name, category, operation, retryable, debug_message)
        .map_err(|_| invalid_message("error has invalid name or operation"))?;

    if !resource_id.is_empty() {
        let resource_id = ResourceId::parse(resource_id)
            .map_err(|_| invalid_message("error has an invalid resource_id"))?;
        error = error.with_resource_id(resource_id);
    }
    if !platform_code.is_empty() {
        error = error
            .with_platform_code(platform_code)
            .map_err(|_| invalid_message("error has an invalid platform_code"))?;
    }
    if !vendor_code.is_empty() {
        error = error
            .with_vendor_code(vendor_code)
            .map_err(|_| invalid_message("error has an invalid vendor_code"))?;
    }
    let context =
        ErrorContext::new(context).map_err(|_| invalid_message("error has an invalid context"))?;
    Ok(error.with_context(context))
}

impl From<&RuntimeEvent> for v1::RuntimeEvent {
    fn from(value: &RuntimeEvent) -> Self {
        Self {
            sequence: value.sequence(),
            kind: match value.kind() {
                RuntimeEventKind::SessionOpened => v1::RuntimeEventKind::SessionOpened,
                RuntimeEventKind::SessionClosed => v1::RuntimeEventKind::SessionClosed,
                RuntimeEventKind::CanBusActive => v1::RuntimeEventKind::CanBusActive,
                RuntimeEventKind::CanBusWarning => v1::RuntimeEventKind::CanBusWarning,
                RuntimeEventKind::CanBusPassive => v1::RuntimeEventKind::CanBusPassive,
                RuntimeEventKind::CanBusOff => v1::RuntimeEventKind::CanBusOff,
                RuntimeEventKind::CanBusStopped => v1::RuntimeEventKind::CanBusStopped,
                RuntimeEventKind::CanBusUnknown => v1::RuntimeEventKind::CanBusUnknown,
            } as i32,
            name: value.name().to_owned(),
            resource_id: value.resource_id().as_str().to_owned(),
            session_id: value.session_id().as_str().to_owned(),
            owner_id: value.owner_id().as_str().to_owned(),
            lease_generation: value.lease_generation(),
        }
    }
}

fn quality_to_proto(value: IdentityQuality) -> v1::IdentityQuality {
    match value {
        IdentityQuality::Weak => v1::IdentityQuality::Weak,
        IdentityQuality::Medium => v1::IdentityQuality::Medium,
        IdentityQuality::Strong => v1::IdentityQuality::Strong,
    }
}

fn transport_to_proto(value: TransportKind) -> HalResult<v1::TransportKind> {
    Ok(match value {
        TransportKind::Serial => v1::TransportKind::Serial,
        TransportKind::Can => v1::TransportKind::Can,
        TransportKind::Usb => v1::TransportKind::Usb,
        TransportKind::Gpio => v1::TransportKind::Gpio,
        TransportKind::Camera => v1::TransportKind::Camera,
    })
}
