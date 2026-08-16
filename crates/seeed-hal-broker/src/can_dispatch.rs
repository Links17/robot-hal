use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};

use seeed_hal_can::{
    CAN_CLASSIC_CAPABILITY, CAN_CONFIGURE_CAPABILITY, CAN_ERROR_FRAMES_CAPABILITY,
    CAN_FD_CAPABILITY, CAN_RX_TIMESTAMP_CAPABILITY, CanFilterSet, CanFrame, CanMode,
    CanOpenConfig, MAX_CAN_BATCH_FRAMES, MAX_FD_DATA_BYTES, can_classic_capability,
    can_configure_capability, can_error_frames_capability, can_fd_capability,
    can_rx_timestamp_capability,
};
use seeed_hal_core::{
    CapabilityId, CapabilitySet, ErrorCategory, HalError, HalResult, OwnerId, ResourceDescriptor,
    ResourceId, ResourceSelector, SessionId,
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
// Maximum encoded bytes for one fully populated CAN frame plus timestamp and
// protobuf field overhead. Used only for pre-dispatch admission.
const MAX_RECEIVED_CAN_WIRE_BYTES: usize = 400;

struct CanSessionRecord {
    capabilities: CapabilitySet,
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
        envelope::Payload::GetCanBusStatusRequest(request) => status(runtime, request).await,
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
    let capabilities = selected.capabilities().clone();
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
                capabilities,
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
    if let Some(capabilities) = session_capabilities(&sessions, &session) {
        validate_frame_capabilities(&frames, &capabilities, None)?;
    }
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
    let requested_frames = usize::try_from(request.max_frames).unwrap_or(usize::MAX);
    let maximum_payload = requested_frames
        .checked_mul(MAX_FD_DATA_BYTES)
        .ok_or_else(|| invalid_message("CAN receive payload bound overflows usize"))?;
    if maximum_payload > limits.max_read_bytes {
        return Err(invalid_message(
            "CAN receive payload bound exceeds the negotiated read maximum",
        ));
    }
    let maximum_response = requested_frames
        .checked_mul(MAX_RECEIVED_CAN_WIRE_BYTES)
        .and_then(|frames| frames.checked_add(32))
        .ok_or_else(|| invalid_message("CAN receive response bound overflows usize"))?;
    if maximum_response > limits.max_frame_bytes {
        return Err(invalid_message(
            "CAN receive response bound exceeds the negotiated frame maximum",
        ));
    }
    let (session, lease, max_frames, timeout) = can_receive_request_from_proto(request)?;
    let frames = runtime
        .receive_can(session.clone(), &lease, max_frames, timeout)
        .await?;
    if let Some(capabilities) = session_capabilities(&sessions, &session) {
        validate_received_capabilities(&frames, &capabilities)?;
    }
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
    if let Some(capabilities) = session_capabilities(&sessions, &session) {
        validate_filter_capabilities(&filters, &capabilities, None)?;
    }
    runtime
        .replace_can_filters(session, &lease, filters)
        .await?;
    Ok(envelope::Payload::ReplaceCanFiltersResponse(v1::Empty {}))
}

async fn status(
    runtime: HalRuntime,
    request: v1::GetCanBusStatusRequest,
) -> HalResult<envelope::Payload> {
    let (session, lease) = get_can_bus_status_request_from_proto(request)?;
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
) -> HalResult<()> {
    for received in frames {
        validate_frame_capabilities(std::slice::from_ref(received.frame()), capabilities, None)?;
        if received.timestamp().is_some() {
            require_capability(
                capabilities,
                &can_rx_timestamp_capability(),
                CAN_RX_TIMESTAMP_CAPABILITY,
                None,
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

fn session_capabilities(sessions: &CanSessions, session: &SessionId) -> Option<CapabilitySet> {
    sessions
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .sessions
        .get(session)
        .map(|record| record.capabilities.clone())
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
