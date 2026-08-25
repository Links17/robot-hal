use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};

use crate::capability_gate::require;
use seeed_hal_camera::{
    CAMERA_CAPTURE_CAPABILITY, CAMERA_CONTROLS_CAPABILITY, CAMERA_FRAMES_SHM_CAPABILITY,
    camera_capture_capability, camera_controls_capability, camera_frames_shm_capability,
};
use seeed_hal_core::{
    CapabilitySet, ErrorCategory, HalError, HalResult, LeaseToken, OwnerId, ResourceDescriptor,
    ResourceId, SessionId,
};
use seeed_hal_protocol::v1::{self, envelope};
use seeed_hal_protocol::{
    camera_capture_request_from_proto, camera_control_descriptor_to_proto,
    camera_control_kind_from_proto, camera_control_value_from_proto, camera_control_value_to_proto,
    camera_mapping_descriptor_to_proto, camera_open_request_from_proto,
    camera_open_response_to_proto, camera_session_lease_from_proto, invalid_message,
};
use seeed_hal_runtime::HalRuntime;

pub(crate) const CAMERA_WIRE_MINOR: u32 = 3;
const CLOSED_SESSION_RETENTION: usize = 256;

#[derive(Clone)]
struct SessionRecord {
    resource_id: ResourceId,
    capabilities: CapabilitySet,
    lease: LeaseToken,
    closed: bool,
}

#[derive(Default)]
pub(crate) struct CameraSessionRegistry {
    sessions: HashMap<SessionId, SessionRecord>,
    closed_order: VecDeque<SessionId>,
}

pub(crate) type CameraSessions = Arc<Mutex<CameraSessionRegistry>>;

pub(crate) fn new_session_registry() -> CameraSessions {
    Arc::new(Mutex::new(CameraSessionRegistry::default()))
}

pub(crate) fn broker_capabilities(protocol_minor: u32) -> Vec<String> {
    if protocol_minor < CAMERA_WIRE_MINOR {
        return Vec::new();
    }
    [
        CAMERA_CAPTURE_CAPABILITY,
        CAMERA_FRAMES_SHM_CAPABILITY,
        CAMERA_CONTROLS_CAPABILITY,
    ]
    .map(str::to_owned)
    .into()
}

pub(crate) async fn dispatch(
    runtime: HalRuntime,
    owner: OwnerId,
    payload: envelope::Payload,
    sessions: CameraSessions,
) -> HalResult<envelope::Payload> {
    match payload {
        envelope::Payload::EnumerateCameraRequest(_) => Ok(
            envelope::Payload::EnumerateCameraResponse(v1::EnumerateCameraResponse {
                resources: runtime
                    .enumerate_camera()
                    .await?
                    .iter()
                    .map(TryInto::try_into)
                    .collect::<HalResult<Vec<_>>>()?,
            }),
        ),
        envelope::Payload::OpenCameraRequest(request) => {
            open(runtime, owner, request, sessions).await
        }
        envelope::Payload::CaptureCameraRequest(request) => {
            let (session, lease, timeout) = camera_capture_request_from_proto(request)?;
            let record = validate(&sessions, &session, &lease, "camera.capture", false)?;
            require(
                &record.capabilities,
                &camera_capture_capability(),
                CAMERA_CAPTURE_CAPABILITY,
                &record.resource_id,
            )?;
            runtime.capture_camera(session, &lease, timeout).await?;
            Ok(envelope::Payload::CaptureCameraResponse(
                v1::CaptureCameraResponse {},
            ))
        }
        envelope::Payload::CameraMappingDescriptorRequest(request) => {
            let (session, lease) =
                camera_session_lease_from_proto(request.session_id, request.lease)?;
            let record = validate(
                &sessions,
                &session,
                &lease,
                "camera.mapping_descriptor",
                false,
            )?;
            require(
                &record.capabilities,
                &camera_frames_shm_capability(),
                CAMERA_FRAMES_SHM_CAPABILITY,
                &record.resource_id,
            )?;
            let descriptor = runtime.camera_mapping_descriptor(session, &lease).await?;
            Ok(envelope::Payload::CameraMappingDescriptorResponse(
                v1::CameraMappingDescriptorResponse {
                    descriptor: Some(camera_mapping_descriptor_to_proto(&descriptor)),
                },
            ))
        }
        envelope::Payload::CameraNextFrameLeaseRequest(request) => {
            let (session, lease) =
                camera_session_lease_from_proto(request.session_id, request.lease)?;
            let record = validate(
                &sessions,
                &session,
                &lease,
                "camera.next_frame_lease",
                false,
            )?;
            require(
                &record.capabilities,
                &camera_frames_shm_capability(),
                CAMERA_FRAMES_SHM_CAPABILITY,
                &record.resource_id,
            )?;
            let lease = runtime.camera_next_frame_lease(session, &lease).await?;
            Ok(envelope::Payload::CameraNextFrameLeaseResponse(
                v1::CameraNextFrameLeaseResponse {
                    lease: lease.as_ref().map(seeed_hal_protocol::frame_lease_to_proto),
                },
            ))
        }
        envelope::Payload::CameraDroppedCountRequest(request) => {
            let (session, lease) =
                camera_session_lease_from_proto(request.session_id, request.lease)?;
            validate(&sessions, &session, &lease, "camera.dropped_count", false)?;
            Ok(envelope::Payload::CameraDroppedCountResponse(
                v1::CameraDroppedCountResponse {
                    dropped_count: runtime.camera_dropped_count(session, &lease).await?,
                },
            ))
        }
        envelope::Payload::CameraControlsRequest(request) => {
            let (session, lease) =
                camera_session_lease_from_proto(request.session_id, request.lease)?;
            let record = validate(&sessions, &session, &lease, "camera.controls", false)?;
            require(
                &record.capabilities,
                &camera_controls_capability(),
                CAMERA_CONTROLS_CAPABILITY,
                &record.resource_id,
            )?;
            Ok(envelope::Payload::CameraControlsResponse(
                v1::CameraControlsResponse {
                    controls: runtime
                        .camera_controls(session, &lease)
                        .await?
                        .iter()
                        .map(camera_control_descriptor_to_proto)
                        .collect(),
                },
            ))
        }
        envelope::Payload::CameraGetControlRequest(request) => {
            let (session, lease) =
                camera_session_lease_from_proto(request.session_id, request.lease)?;
            let record = validate(&sessions, &session, &lease, "camera.control.get", false)?;
            require(
                &record.capabilities,
                &camera_controls_capability(),
                CAMERA_CONTROLS_CAPABILITY,
                &record.resource_id,
            )?;
            let value = runtime
                .camera_get_control(
                    session,
                    &lease,
                    camera_control_kind_from_proto(request.kind)?,
                )
                .await?;
            Ok(envelope::Payload::CameraGetControlResponse(
                v1::CameraGetControlResponse {
                    value: Some(camera_control_value_to_proto(&value)),
                },
            ))
        }
        envelope::Payload::CameraSetControlRequest(request) => {
            let (session, lease) =
                camera_session_lease_from_proto(request.session_id, request.lease)?;
            let record = validate(&sessions, &session, &lease, "camera.control.set", false)?;
            require(
                &record.capabilities,
                &camera_controls_capability(),
                CAMERA_CONTROLS_CAPABILITY,
                &record.resource_id,
            )?;
            let value = request
                .value
                .ok_or_else(|| invalid_message("camera control value is required"))?;
            runtime
                .camera_set_control(
                    session,
                    &lease,
                    camera_control_kind_from_proto(request.kind)?,
                    camera_control_value_from_proto(value)?,
                )
                .await?;
            Ok(envelope::Payload::CameraSetControlResponse(v1::Empty {}))
        }
        envelope::Payload::CameraSetAutoRequest(request) => {
            let (session, lease) =
                camera_session_lease_from_proto(request.session_id, request.lease)?;
            let record = validate(&sessions, &session, &lease, "camera.control.auto", false)?;
            require(
                &record.capabilities,
                &camera_controls_capability(),
                CAMERA_CONTROLS_CAPABILITY,
                &record.resource_id,
            )?;
            runtime
                .camera_set_auto(
                    session,
                    &lease,
                    camera_control_kind_from_proto(request.kind)?,
                    request.enabled,
                )
                .await?;
            Ok(envelope::Payload::CameraSetAutoResponse(v1::Empty {}))
        }
        envelope::Payload::CloseCameraRequest(request) => {
            let (session, lease) =
                camera_session_lease_from_proto(request.session_id, request.lease)?;
            if !validate(&sessions, &session, &lease, "camera.close", true)?.closed {
                runtime.close_camera(session.clone(), &lease).await?;
                record_closed(&sessions, &session);
            }
            Ok(envelope::Payload::CloseCameraResponse(v1::Empty {}))
        }
        _ => Err(invalid_message(
            "Camera response payloads are not valid client requests",
        )),
    }
}

async fn open(
    runtime: HalRuntime,
    owner: OwnerId,
    request: v1::OpenCameraRequest,
    sessions: CameraSessions,
) -> HalResult<envelope::Payload> {
    let (selector, request) = camera_open_request_from_proto(request)?;
    let descriptors = runtime.enumerate_camera().await?;
    let descriptor = select(&descriptors, &selector)?;
    require(
        descriptor.capabilities(),
        &camera_capture_capability(),
        CAMERA_CAPTURE_CAPABILITY,
        descriptor.id(),
    )?;
    let handle = runtime.open_camera(owner, selector, request).await?;
    let (session, lease) = handle.into_parts();
    sessions
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .sessions
        .insert(
            session.clone(),
            SessionRecord {
                resource_id: descriptor.id().clone(),
                capabilities: descriptor.capabilities().clone(),
                lease: lease.clone(),
                closed: false,
            },
        );
    Ok(envelope::Payload::OpenCameraResponse(
        camera_open_response_to_proto(&session, &lease),
    ))
}

fn select<'a>(
    descriptors: &'a [ResourceDescriptor],
    selector: &seeed_hal_core::ResourceSelector,
) -> HalResult<&'a ResourceDescriptor> {
    let mut matches = descriptors.iter().filter(|value| {
        value.id() == selector.id()
            && value.transport() == selector.transport()
            && value
                .minimum_identity_quality()
                .satisfies(selector.minimum_identity_quality())
    });
    let Some(descriptor) = matches.next() else {
        return Err(session_error(
            "runtime.resource.not_found",
            "camera.open",
            "resource selector did not match",
            None,
        ));
    };
    if matches.next().is_some() {
        return Err(session_error(
            "runtime.resource.ambiguous",
            "camera.open",
            "resource selector was ambiguous",
            Some(descriptor.id()),
        ));
    }
    Ok(descriptor)
}

fn validate(
    sessions: &CameraSessions,
    session: &SessionId,
    lease: &LeaseToken,
    operation: &'static str,
    allow_closed: bool,
) -> HalResult<SessionRecord> {
    let sessions = sessions.lock().unwrap_or_else(|p| p.into_inner());
    let Some(record) = sessions.sessions.get(session) else {
        return Err(session_error(
            "runtime.session.not_found",
            operation,
            "session is not owned by this broker connection",
            None,
        ));
    };
    if &record.lease != lease {
        return Err(session_error(
            if lease.generation() < record.lease.generation() {
                "runtime.lease.stale_generation"
            } else {
                "runtime.lease.invalid_token"
            },
            operation,
            "lease token does not match connection-owned session",
            Some(&record.resource_id),
        ));
    }
    if record.closed && !allow_closed {
        return Err(session_error(
            "runtime.session.closed",
            operation,
            "session is closed",
            Some(&record.resource_id),
        ));
    }
    Ok(record.clone())
}

fn record_closed(sessions: &CameraSessions, session: &SessionId) {
    let mut sessions = sessions.lock().unwrap_or_else(|p| p.into_inner());
    let Some(record) = sessions.sessions.get_mut(session) else {
        return;
    };
    if record.closed {
        return;
    }
    record.closed = true;
    sessions.closed_order.push_back(session.clone());
    while sessions.closed_order.len() > CLOSED_SESSION_RETENTION {
        if let Some(evicted) = sessions.closed_order.pop_front() {
            sessions.sessions.remove(&evicted);
        }
    }
}

fn session_error(
    name: &'static str,
    operation: &'static str,
    message: &'static str,
    resource: Option<&ResourceId>,
) -> HalError {
    let category = match name {
        "runtime.session.not_found" | "runtime.resource.not_found" => ErrorCategory::NotFound,
        "runtime.resource.ambiguous"
        | "runtime.lease.stale_generation"
        | "runtime.lease.invalid_token"
        | "runtime.session.closed"
        | "runtime.protocol.capability_unsupported" => ErrorCategory::Conflict,
        _ => ErrorCategory::InvalidArgument,
    };
    resource.map_or_else(
        || {
            HalError::new(name, category, operation, false, message)
                .expect("static camera broker error")
        },
        |resource| {
            HalError::new(name, category, operation, false, message)
                .expect("static camera broker error")
                .with_resource_id(resource.clone())
        },
    )
}
