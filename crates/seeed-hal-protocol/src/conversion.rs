use std::collections::BTreeMap;
use std::time::Duration;

use seeed_hal_core::{
    CapabilityId, CapabilitySet, Endpoint, ErrorCategory, ErrorContext, HalError, HalResult,
    IdentityQuality, LeaseId, LeaseMode, LeaseToken, ResourceDescriptor, ResourceId,
    ResourceProperties, ResourceSelector, SessionId, TransportKind,
};
use seeed_hal_runtime::{RuntimeEvent, RuntimeEventKind};
use seeed_hal_serial::{DataBits, FlowControl, Parity, SerialConfig, StopBits};

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

fn required_enum<T: TryFrom<i32>>(value: i32, field: &'static str) -> HalResult<T> {
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
            v1::TransportKind::Unspecified => {
                return Err(invalid_message("transport is required"));
            }
        };
        Ok(Self::exact(id, quality, transport))
    }
}

impl From<&ResourceSelector> for v1::ResourceSelector {
    fn from(value: &ResourceSelector) -> Self {
        Self {
            resource_id: value.id().as_str().to_owned(),
            minimum_identity_quality: quality_to_proto(value.minimum_identity_quality()) as i32,
            transport: transport_to_proto(value.transport()) as i32,
        }
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

impl From<&ResourceDescriptor> for v1::ResourceDescriptor {
    fn from(value: &ResourceDescriptor) -> Self {
        Self {
            resource_id: value.id().as_str().to_owned(),
            endpoint: value.endpoint().as_str().to_owned(),
            identity_quality: quality_to_proto(value.minimum_identity_quality()) as i32,
            transport: transport_to_proto(value.transport()) as i32,
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
        }
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

fn transport_to_proto(value: TransportKind) -> v1::TransportKind {
    match value {
        TransportKind::Serial => v1::TransportKind::Serial,
    }
}
