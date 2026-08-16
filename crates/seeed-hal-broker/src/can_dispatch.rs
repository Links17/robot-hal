use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};

use seeed_hal_can::{
    CAN_CLASSIC_CAPABILITY, CAN_CONFIGURE_CAPABILITY, CAN_ERROR_FRAMES_CAPABILITY,
    CAN_FD_CAPABILITY, CAN_RX_TIMESTAMP_CAPABILITY, CanFilterSet, CanFrame, CanMode,
    CanOpenConfig, MAX_CAN_BATCH_FRAMES, MAX_CAN_ERROR_CLASSES, MAX_CLASSIC_DATA_BYTES,
    MAX_FD_DATA_BYTES, can_classic_capability,
    can_configure_capability, can_error_frames_capability, can_fd_capability,
    can_rx_timestamp_capability,
};
use seeed_hal_core::{
    CapabilityId, CapabilitySet, ErrorCategory, HalError, HalResult, LeaseMode, LeaseToken, OwnerId,
    ResourceDescriptor, ResourceId, ResourceSelector, SessionId,
};
use seeed_hal_protocol::v1::{self, envelope};
use seeed_hal_protocol::{
    can_receive_request_from_proto, can_send_request_from_proto, can_send_response_to_proto,
    get_can_bus_status_request_from_proto, invalid_message, open_can_request_from_proto,
    parse_session_lease, replace_can_filters_request_from_proto,
};
use seeed_hal_runtime::HalRuntime;

pub(crate) const CAN_WIRE_MINOR: u32 = 1;

#[derive(Clone, Copy)]
pub(crate) struct CanDispatchLimits {
    pub(crate) protocol_minor: u32,
    pub(crate) max_frame_bytes: usize,
    pub(crate) max_read_bytes: usize,
    pub(crate) max_write_bytes: usize,
}

const CLOSED_CAN_SESSION_RETENTION: usize = 256;
// Exact prost maxima under the canonical CAN model. These are selected per
// authenticated session profile before runtime receive admission.
const MAX_CLASSIC_CAN_FRAME_PROTO_BYTES: usize = 22;
const MAX_FD_CAN_FRAME_PROTO_BYTES: usize = 82;
const MAX_ERROR_CAN_FRAME_PROTO_BYTES: usize = 2 + 10 + 2 + MAX_CAN_ERROR_CLASSES;
const MAX_CAN_TIMESTAMP_PROTO_BYTES: usize = 271;
const CAN_RECEIVE_RESPONSE_FIELD_NUMBER: u32 = 57;

#[derive(Clone, Copy)]
struct CanReceiveWireProfile {
    mode: CanMode,
    max_data_bytes: usize,
    max_frame_proto_bytes: usize,
    timestamp: bool,
}

struct CanSessionRecord {
    resource_id: ResourceId,
    capabilities: CapabilitySet,
    lease: LeaseToken,
    receive_profile: CanReceiveWireProfile,
    closed: bool,
}

struct CanSessionContext {
    resource_id: ResourceId,
    capabilities: CapabilitySet,
    receive_profile: CanReceiveWireProfile,
    closed: bool,
}

#[derive(Default)]
pub(crate) struct CanSessionRegistry {
    sessions: HashMap<SessionId, CanSessionRecord>,
    closed_order: VecDeque<SessionId>,
}

pub(crate) type CanSessions = Arc<Mutex<CanSessionRegistry>>;

pub(crate) fn new_session_registry() -> CanSessions {
    Arc::new(Mutex::new(CanSessionRegistry::default()))
}

pub(crate) fn broker_capabilities(protocol_minor: u32) -> Vec<String> {
    let mut capabilities = vec![seeed_hal_protocol::SERIAL_CAPABILITY.to_owned()];
    if protocol_minor >= CAN_WIRE_MINOR {
        capabilities.extend(
            [
                CAN_CLASSIC_CAPABILITY,
                CAN_FD_CAPABILITY,
                CAN_CONFIGURE_CAPABILITY,
                CAN_ERROR_FRAMES_CAPABILITY,
                CAN_RX_TIMESTAMP_CAPABILITY,
            ]
            .map(str::to_owned),
        );
    }
    capabilities
}

pub(crate) fn is_can_payload(payload: &envelope::Payload) -> bool {
    matches!(
        payload,
        envelope::Payload::EnumerateCanRequest(_)
            | envelope::Payload::EnumerateCanResponse(_)
            | envelope::Payload::OpenCanRequest(_)
            | envelope::Payload::OpenCanResponse(_)
            | envelope::Payload::CanSendRequest(_)
            | envelope::Payload::CanSendResponse(_)
            | envelope::Payload::CanReceiveRequest(_)
            | envelope::Payload::CanReceiveResponse(_)
            | envelope::Payload::ReplaceCanFiltersRequest(_)
            | envelope::Payload::ReplaceCanFiltersResponse(_)
            | envelope::Payload::GetCanBusStatusRequest(_)
            | envelope::Payload::GetCanBusStatusResponse(_)
    )
}

pub(crate) fn is_can_session(sessions: &CanSessions, session_id: &str) -> bool {
    SessionId::parse(session_id.to_owned()).is_ok_and(|session| {
        sessions
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .sessions
            .contains_key(&session)
    })
}

pub(crate) async fn dispatch(
    runtime: HalRuntime,
    owner: OwnerId,
    payload: envelope::Payload,
    limits: CanDispatchLimits,
    sessions: CanSessions,
) -> HalResult<envelope::Payload> {
    if limits.protocol_minor < CAN_WIRE_MINOR {
        return Err(capability_unsupported(
            "CAN operations require negotiated protocol minor 1",
            None,
        ));
    }

    match payload {
        envelope::Payload::EnumerateCanRequest(_) => enumerate(runtime).await,
        envelope::Payload::OpenCanRequest(request) => {
            open(runtime, owner, request, sessions).await
        }
        envelope::Payload::CanSendRequest(request) => {
            send(runtime, request, limits, sessions).await
        }
        envelope::Payload::CanReceiveRequest(request) => {
            receive(runtime, request, limits, sessions).await
        }
        envelope::Payload::ReplaceCanFiltersRequest(request) => {
            replace_filters(runtime, request, sessions).await
        }
        envelope::Payload::GetCanBusStatusRequest(request) => {
            status(runtime, request, sessions).await
        }
        _ => Err(invalid_message(
            "CAN response payloads are not valid client requests",
        )),
    }
}

pub(crate) async fn close(
    runtime: HalRuntime,
    request: v1::CloseSessionRequest,
    sessions: CanSessions,
) -> HalResult<envelope::Payload> {
    let (session, lease) = parse_session_lease(request.session_id, request.lease)?;
    let context = validate_session(
        &sessions,
        &session,
        &lease,
        LeaseMode::Observe,
        "can.close",
        true,
    )?;
    if context.closed {
        return Ok(envelope::Payload::CloseSessionResponse(v1::Empty {}));
    }
    runtime.close_can(session.clone(), &lease).await?;
    record_closed(&sessions, &session);
    Ok(envelope::Payload::CloseSessionResponse(v1::Empty {}))
}

async fn enumerate(runtime: HalRuntime) -> HalResult<envelope::Payload> {
    let resources = runtime
        .enumerate_can()
        .await?
        .iter()
        .map(v1::ResourceDescriptor::from)
        .collect();
    Ok(envelope::Payload::EnumerateCanResponse(
        v1::EnumerateCanResponse { resources },
    ))
}

async fn open(
    runtime: HalRuntime,
    owner: OwnerId,
    request: v1::OpenCanRequest,
    sessions: CanSessions,
) -> HalResult<envelope::Payload> {
    let (selector, mode, config, filters) = open_can_request_from_proto(request)?;
    let descriptors = runtime.enumerate_can().await?;
    let selected = select_descriptor(&descriptors, &selector)?;
    validate_open_capabilities(selected, &config, &filters)?;
    let resource_id = selected.id().clone();
    let capabilities = selected.capabilities().clone();
    let receive_profile = receive_wire_profile(&config, &capabilities);
    let handle = runtime
        .open_can(owner, selector, mode, config, filters)
        .await?;
    let (session_id, lease) = handle.into_parts();
    sessions
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .sessions
        .insert(
            session_id.clone(),
            CanSessionRecord {
                resource_id,
                capabilities,
                lease: lease.clone(),
                receive_profile,
                closed: false,
            },
        );
    Ok(envelope::Payload::OpenCanResponse(v1::OpenCanResponse {
        session_id: session_id.as_str().to_owned(),
        lease: Some((&lease).into()),
    }))
}

async fn send(
    runtime: HalRuntime,
    request: v1::CanSendRequest,
    limits: CanDispatchLimits,
    sessions: CanSessions,
) -> HalResult<envelope::Payload> {
    validate_send_bounds(&request, limits.max_write_bytes)?;
    let (session, lease, frames) = can_send_request_from_proto(request)?;
    let context = validate_session(
        &sessions,
        &session,
        &lease,
        LeaseMode::Control,
        "can.send_batch",
        false,
    )?;
    validate_frame_capabilities(
        &frames,
        &context.capabilities,
        Some(&context.resource_id),
    )?;
    let input_count = frames.len();
    let response = match runtime.send_can_batch(session, &lease, frames).await {
        Ok(()) => can_send_response_to_proto(Ok(()), input_count)?,
        Err(error) => can_send_response_to_proto(Err(&error), input_count)?,
    };
    Ok(envelope::Payload::CanSendResponse(response))
}

async fn receive(
    runtime: HalRuntime,
    request: v1::CanReceiveRequest,
    limits: CanDispatchLimits,
    sessions: CanSessions,
) -> HalResult<envelope::Payload> {
    let (session, lease, max_frames, timeout) = can_receive_request_from_proto(request)?;
    let context = validate_session(
        &sessions,
        &session,
        &lease,
        LeaseMode::Observe,
        "can.receive",
        false,
    )?;
    let requested_frames = max_frames;
    let maximum_payload = requested_frames
        .checked_mul(context.receive_profile.max_data_bytes)
        .ok_or_else(|| invalid_message("CAN receive payload bound overflows usize"))?;
    if maximum_payload > limits.max_read_bytes {
        return Err(invalid_message(
            "CAN receive payload bound exceeds the negotiated read maximum",
        ));
    }
    let maximum_response =
        can_receive_response_envelope_bound(requested_frames, context.receive_profile)
        .ok_or_else(|| invalid_message("CAN receive response bound overflows usize"))?;
    if maximum_response > limits.max_frame_bytes {
        return Err(invalid_message(
            "CAN receive response bound exceeds the negotiated frame maximum",
        ));
    }
    let frames = runtime
        .receive_can(session.clone(), &lease, max_frames, timeout)
        .await?;
    validate_received_capabilities(
        &frames,
        &context.capabilities,
        &context.resource_id,
        context.receive_profile,
    )?;
    Ok(envelope::Payload::CanReceiveResponse(
        v1::CanReceiveResponse {
            frames: frames.iter().map(Into::into).collect(),
        },
    ))
}

async fn replace_filters(
    runtime: HalRuntime,
    request: v1::ReplaceCanFiltersRequest,
    sessions: CanSessions,
) -> HalResult<envelope::Payload> {
    let (session, lease, filters) = replace_can_filters_request_from_proto(request)?;
    let context = validate_session(
        &sessions,
        &session,
        &lease,
        LeaseMode::Observe,
        "can.replace_filters",
        false,
    )?;
    validate_filter_capabilities(
        &filters,
        &context.capabilities,
        Some(&context.resource_id),
    )?;
    runtime
        .replace_can_filters(session, &lease, filters)
        .await?;
    Ok(envelope::Payload::ReplaceCanFiltersResponse(v1::Empty {}))
}

async fn status(
    runtime: HalRuntime,
    request: v1::GetCanBusStatusRequest,
    sessions: CanSessions,
) -> HalResult<envelope::Payload> {
    let (session, lease) = get_can_bus_status_request_from_proto(request)?;
    validate_session(
        &sessions,
        &session,
        &lease,
        LeaseMode::Observe,
        "can.status",
        false,
    )?;
    let status = runtime.can_bus_status(session, &lease).await?;
    Ok(envelope::Payload::GetCanBusStatusResponse(
        v1::GetCanBusStatusResponse {
            status: Some((&status).into()),
        },
    ))
}

fn validate_send_bounds(request: &v1::CanSendRequest, max_write_bytes: usize) -> HalResult<()> {
    if !(1..=MAX_CAN_BATCH_FRAMES).contains(&request.frames.len()) {
        return Err(invalid_message(
            "CAN send batch must contain 1..=64 frames",
        ));
    }
    let payload_bytes = request.frames.iter().try_fold(0_usize, |total, frame| {
        total
            .checked_add(frame.data.len())
            .ok_or_else(|| invalid_message("CAN send payload bound overflows usize"))
    })?;
    if payload_bytes > max_write_bytes {
        return Err(invalid_message(
            "CAN send payload exceeds the negotiated write maximum",
        ));
    }
    Ok(())
}

fn can_receive_response_envelope_bound(
    frame_count: usize,
    profile: CanReceiveWireProfile,
) -> Option<usize> {
    let frame_field = length_delimited_field_len(1, profile.max_frame_proto_bytes);
    let timestamp_field = profile
        .timestamp
        .then_some(length_delimited_field_len(
            2,
            MAX_CAN_TIMESTAMP_PROTO_BYTES,
        ))
        .unwrap_or(0);
    let received_frame = frame_field.checked_add(timestamp_field)?;
    let repeated_frame = length_delimited_field_len(1, received_frame);
    let response_payload = repeated_frame.checked_mul(frame_count)?;
    Some(envelope_encoded_len(
        CAN_RECEIVE_RESPONSE_FIELD_NUMBER,
        response_payload,
    ))
}

fn receive_wire_profile(
    config: &CanOpenConfig,
    capabilities: &CapabilitySet,
) -> CanReceiveWireProfile {
    let mode = match config {
        CanOpenConfig::Configure(config) => config.mode(),
        CanOpenConfig::Attach(expectation) => expectation.mode().unwrap_or_else(|| {
            if capabilities.contains(&can_fd_capability()) {
                CanMode::Fd
            } else {
                CanMode::Classic
            }
        }),
    };
    let mode_frame_bytes = match mode {
        CanMode::Classic => MAX_CLASSIC_CAN_FRAME_PROTO_BYTES,
        CanMode::Fd => MAX_FD_CAN_FRAME_PROTO_BYTES,
    };
    let max_frame_proto_bytes = if capabilities.contains(&can_error_frames_capability()) {
        mode_frame_bytes.max(MAX_ERROR_CAN_FRAME_PROTO_BYTES)
    } else {
        mode_frame_bytes
    };
    CanReceiveWireProfile {
        mode,
        max_data_bytes: match mode {
            CanMode::Classic => MAX_CLASSIC_DATA_BYTES,
            CanMode::Fd => MAX_FD_DATA_BYTES,
        },
        max_frame_proto_bytes,
        timestamp: capabilities.contains(&can_rx_timestamp_capability()),
    }
}

fn envelope_encoded_len(payload_field_number: u32, payload_len: usize) -> usize {
    1 + 10
        + prost_varint_len((u64::from(payload_field_number) << 3) | 2)
        + prost_varint_len(payload_len as u64)
        + payload_len
}

fn length_delimited_field_len(field_number: u32, value_len: usize) -> usize {
    prost_varint_len((u64::from(field_number) << 3) | 2)
        + prost_varint_len(value_len as u64)
        + value_len
}

fn prost_varint_len(mut value: u64) -> usize {
    let mut len = 1;
    while value >= 0x80 {
        value >>= 7;
        len += 1;
    }
    len
}

fn validate_open_capabilities(
    descriptor: &ResourceDescriptor,
    config: &CanOpenConfig,
    filters: &CanFilterSet,
) -> HalResult<()> {
    let resource_id = Some(descriptor.id());
    match config {
        CanOpenConfig::Attach(expectation) => match expectation.mode() {
            Some(CanMode::Classic) => require_capability(
                descriptor.capabilities(),
                &can_classic_capability(),
                CAN_CLASSIC_CAPABILITY,
                resource_id,
            )?,
            Some(CanMode::Fd) => require_capability(
                descriptor.capabilities(),
                &can_fd_capability(),
                CAN_FD_CAPABILITY,
                resource_id,
            )?,
            None => {}
        },
        CanOpenConfig::Configure(config) => {
            require_capability(
                descriptor.capabilities(),
                &can_configure_capability(),
                CAN_CONFIGURE_CAPABILITY,
                resource_id,
            )?;
            let (capability, name) = match config.mode() {
                CanMode::Classic => (can_classic_capability(), CAN_CLASSIC_CAPABILITY),
                CanMode::Fd => (can_fd_capability(), CAN_FD_CAPABILITY),
            };
            require_capability(
                descriptor.capabilities(),
                &capability,
                name,
                resource_id,
            )?;
        }
    }
    validate_filter_capabilities(filters, descriptor.capabilities(), resource_id)
}

fn validate_filter_capabilities(
    filters: &CanFilterSet,
    capabilities: &CapabilitySet,
    resource_id: Option<&ResourceId>,
) -> HalResult<()> {
    if filters
        .as_slice()
        .iter()
        .any(|filter| filter.classes().error())
    {
        require_capability(
            capabilities,
            &can_error_frames_capability(),
            CAN_ERROR_FRAMES_CAPABILITY,
            resource_id,
        )?;
    }
    Ok(())
}

fn validate_frame_capabilities(
    frames: &[CanFrame],
    capabilities: &CapabilitySet,
    resource_id: Option<&ResourceId>,
) -> HalResult<()> {
    for frame in frames {
        if let Err(error) = frame.validate() {
            return Err(match resource_id {
                Some(resource_id) => error.with_resource_id(resource_id.clone()),
                None => error,
            });
        }
        let (capability, name) = match frame {
            CanFrame::ClassicData { .. } | CanFrame::ClassicRemote { .. } => {
                (can_classic_capability(), CAN_CLASSIC_CAPABILITY)
            }
            CanFrame::FdData { .. } => (can_fd_capability(), CAN_FD_CAPABILITY),
            CanFrame::Error { .. } => (
                can_error_frames_capability(),
                CAN_ERROR_FRAMES_CAPABILITY,
            ),
        };
        require_capability(capabilities, &capability, name, resource_id)?;
    }
    Ok(())
}

fn validate_received_capabilities(
    frames: &[seeed_hal_can::ReceivedCanFrame],
    capabilities: &CapabilitySet,
    resource_id: &ResourceId,
    profile: CanReceiveWireProfile,
) -> HalResult<()> {
    for received in frames {
        if profile.mode == CanMode::Classic
            && matches!(received.frame(), CanFrame::FdData { .. })
        {
            return Err(capability_unsupported(
                "Classical CAN session received a CAN FD frame",
                Some(resource_id),
            ));
        }
        validate_frame_capabilities(
            std::slice::from_ref(received.frame()),
            capabilities,
            Some(resource_id),
        )?;
        if received.timestamp().is_some() {
            require_capability(
                capabilities,
                &can_rx_timestamp_capability(),
                CAN_RX_TIMESTAMP_CAPABILITY,
                Some(resource_id),
            )?;
        }
    }
    Ok(())
}

fn require_capability(
    capabilities: &CapabilitySet,
    capability: &CapabilityId,
    name: &'static str,
    resource_id: Option<&ResourceId>,
) -> HalResult<()> {
    if capabilities.contains(capability) {
        return Ok(());
    }
    Err(capability_unsupported(
        format!("selected CAN resource does not advertise {name}"),
        resource_id,
    ))
}

fn select_descriptor<'a>(
    descriptors: &'a [ResourceDescriptor],
    selector: &ResourceSelector,
) -> HalResult<&'a ResourceDescriptor> {
    let mut matches = descriptors.iter().filter(|descriptor| {
        descriptor.id() == selector.id()
            && descriptor.transport() == selector.transport()
            && descriptor
                .minimum_identity_quality()
                .satisfies(selector.minimum_identity_quality())
    });
    let Some(selected) = matches.next() else {
        return Err(resource_selection_error(
            "runtime.resource.not_found",
            ErrorCategory::NotFound,
            "CAN resource selector did not match an enumerated descriptor",
            selector.id(),
        ));
    };
    if matches.next().is_some() {
        return Err(resource_selection_error(
            "runtime.resource.ambiguous",
            ErrorCategory::Conflict,
            "CAN resource selector matched more than one enumerated descriptor",
            selector.id(),
        ));
    }
    Ok(selected)
}

fn validate_session(
    sessions: &CanSessions,
    session: &SessionId,
    supplied: &LeaseToken,
    required_mode: LeaseMode,
    operation: &'static str,
    allow_closed: bool,
) -> HalResult<CanSessionContext> {
    let sessions = sessions
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let Some(record) = sessions.sessions.get(session) else {
        return Err(session_error(
            "runtime.session.not_found",
            operation,
            "the CAN session is not owned by this broker connection",
            None,
        ));
    };
    if record.closed {
        if supplied == &record.lease {
            if !allow_closed {
                return Err(session_error(
                    "runtime.session.closed",
                    operation,
                    "the CAN session is closed",
                    Some(&record.resource_id),
                ));
            }
            return Ok(CanSessionContext {
                resource_id: record.resource_id.clone(),
                capabilities: record.capabilities.clone(),
                receive_profile: record.receive_profile,
                closed: true,
            });
        }
        return Err(session_token_error(record, supplied, operation));
    }
    if supplied != &record.lease {
        return Err(session_token_error(record, supplied, operation));
    }
    if !lease_mode_allows(record.lease.mode(), required_mode) {
        return Err(session_error(
            "runtime.lease.mode_denied",
            operation,
            "the CAN lease mode does not permit this operation",
            Some(&record.resource_id),
        ));
    }
    Ok(CanSessionContext {
        resource_id: record.resource_id.clone(),
        capabilities: record.capabilities.clone(),
        receive_profile: record.receive_profile,
        closed: false,
    })
}

fn lease_mode_allows(actual: LeaseMode, required: LeaseMode) -> bool {
    matches!(
        (actual, required),
        (LeaseMode::Observe, LeaseMode::Observe)
            | (LeaseMode::Control, LeaseMode::Observe | LeaseMode::Control)
            | (LeaseMode::Maintenance, LeaseMode::Observe | LeaseMode::Control | LeaseMode::Maintenance)
    )
}

fn session_token_error(
    record: &CanSessionRecord,
    supplied: &LeaseToken,
    operation: &'static str,
) -> HalError {
    let name = if supplied.generation() < record.lease.generation() {
        "runtime.lease.stale_generation"
    } else {
        "runtime.lease.invalid_token"
    };
    session_error(
        name,
        operation,
        "the CAN lease token does not match the connection-owned session",
        Some(&record.resource_id),
    )
}

fn session_error(
    name: &'static str,
    operation: &'static str,
    message: &'static str,
    resource_id: Option<&ResourceId>,
) -> HalError {
    let category = if name == "runtime.session.not_found" {
        ErrorCategory::NotFound
    } else {
        ErrorCategory::Conflict
    };
    let error = HalError::new(name, category, operation, false, message)
        .expect("static CAN broker session error metadata is valid");
    match resource_id {
        Some(resource_id) => error.with_resource_id(resource_id.clone()),
        None => error,
    }
}

fn record_closed(sessions: &CanSessions, session: &SessionId) {
    let mut sessions = sessions
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let Some(record) = sessions.sessions.get_mut(session) else {
        return;
    };
    if record.closed {
        return;
    }
    record.closed = true;
    sessions.closed_order.push_back(session.clone());
    while sessions.closed_order.len() > CLOSED_CAN_SESSION_RETENTION {
        let evicted = sessions
            .closed_order
            .pop_front()
            .expect("closed CAN session retention is non-empty");
        if sessions
            .sessions
            .get(&evicted)
            .is_some_and(|record| record.closed)
        {
            sessions.sessions.remove(&evicted);
        }
    }
}

fn capability_unsupported(
    message: impl Into<String>,
    resource_id: Option<&ResourceId>,
) -> HalError {
    let error = HalError::new(
        "runtime.protocol.capability_unsupported",
        ErrorCategory::Conflict,
        "runtime.protocol.dispatch",
        false,
        message,
    )
    .expect("static CAN broker error metadata is valid");
    resource_id.map_or(error.clone(), |id| error.with_resource_id(id.clone()))
}

fn resource_selection_error(
    name: &'static str,
    category: ErrorCategory,
    message: &'static str,
    resource_id: &ResourceId,
) -> HalError {
    HalError::new(name, category, "can.open", false, message)
        .expect("static CAN broker error metadata is valid")
        .with_resource_id(resource_id.clone())
}

#[cfg(test)]
mod tests {
    use bytes::Bytes;
    use prost::Message;
    use seeed_hal_can::{
        CanErrorClass, CanFrame, CanId, CanMode, CanTimestamp, CanTimestampSource,
        ReceivedCanFrame,
    };
    use seeed_hal_protocol::v1::{self, envelope};

    use super::{
        CanReceiveWireProfile, MAX_CAN_TIMESTAMP_PROTO_BYTES,
        MAX_CLASSIC_CAN_FRAME_PROTO_BYTES, MAX_ERROR_CAN_FRAME_PROTO_BYTES,
        MAX_FD_CAN_FRAME_PROTO_BYTES, can_receive_response_envelope_bound,
        length_delimited_field_len,
    };

    #[test]
    fn canonical_can_bounds_derive_exact_session_receive_envelope_maxima() {
        assert_eq!(MAX_CLASSIC_CAN_FRAME_PROTO_BYTES, 22);
        assert_eq!(MAX_ERROR_CAN_FRAME_PROTO_BYTES, 24);
        assert_eq!(MAX_FD_CAN_FRAME_PROTO_BYTES, 82);
        assert_eq!(
            length_delimited_field_len(1, MAX_FD_CAN_FRAME_PROTO_BYTES)
                + length_delimited_field_len(2, MAX_CAN_TIMESTAMP_PROTO_BYTES),
            358
        );
        let classic = CanReceiveWireProfile {
            mode: CanMode::Classic,
            max_data_bytes: 8,
            max_frame_proto_bytes: MAX_CLASSIC_CAN_FRAME_PROTO_BYTES,
            timestamp: false,
        };
        let fd_timestamp = CanReceiveWireProfile {
            mode: CanMode::Fd,
            max_data_bytes: 64,
            max_frame_proto_bytes: MAX_FD_CAN_FRAME_PROTO_BYTES,
            timestamp: true,
        };
        assert_eq!(
            can_receive_response_envelope_bound(1, classic),
            Some(40)
        );
        assert_eq!(
            can_receive_response_envelope_bound(1, fd_timestamp),
            Some(376)
        );
        assert_eq!(
            can_receive_response_envelope_bound(64, fd_timestamp),
            Some(23_120)
        );
    }

    #[test]
    fn canonical_values_reach_the_proven_protobuf_maxima() {
        let fd = CanFrame::fd_data(
            CanId::extended(0x1fff_ffff).unwrap(),
            Bytes::from(vec![0xff; 64]),
            true,
            true,
        )
        .unwrap();
        assert_eq!(v1::CanFrame::from(&fd).encoded_len(), 82);

        let error = CanFrame::error(
            vec![CanErrorClass::Other; seeed_hal_can::MAX_CAN_ERROR_CLASSES],
            Bytes::from(vec![0xff; 8]),
        )
        .unwrap();
        assert_eq!(v1::CanFrame::from(&error).encoded_len(), 24);

        let timestamp = CanTimestamp::new(
            u64::MAX,
            CanTimestampSource::HostMonotonic,
            "x".repeat(255),
        )
        .unwrap();
        assert_eq!(v1::CanTimestamp::from(&timestamp).encoded_len(), 271);

        let received = ReceivedCanFrame::new(fd, Some(timestamp));
        let response = v1::CanReceiveResponse {
            frames: vec![v1::ReceivedCanFrame::from(&received)],
        };
        assert_eq!(response.frames[0].encoded_len(), 358);
        let response = v1::Envelope {
            request_id: u64::MAX,
            payload: Some(envelope::Payload::CanReceiveResponse(response)),
        };
        assert_eq!(response.encoded_len(), 376);
    }
}
