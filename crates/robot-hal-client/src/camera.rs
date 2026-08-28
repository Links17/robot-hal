use std::{sync::Mutex, time::Duration};

use robot_hal_adapter_shared_memory::{FrameLease, MappingDescriptor, ReadOnlyMapping};
use robot_hal_camera::{CameraControlKind, CameraControlValue};
use robot_hal_core::{ErrorCategory, HalError, HalResult, LeaseToken, ResourceId, SessionId};
use robot_hal_protocol::v1::{self, envelope};
use robot_hal_protocol::{
    camera_control_value_from_proto, camera_control_value_to_proto,
    camera_controls_response_from_proto, camera_mapping_descriptor_from_proto,
    camera_next_frame_lease_response_from_proto, camera_open_response_from_proto,
};
use subtle::ConstantTimeEq;

use crate::HalClient;
use crate::connection::ExpectedResponse;

/// An opaque broker-owned Camera session with explicit, copy-only shared-memory access.
#[must_use = "a remote Camera handle owns a broker session until explicitly closed"]
pub struct RemoteCameraHandle {
    client: HalClient,
    resource_id: ResourceId,
    session_id: SessionId,
    lease: LeaseToken,
    closed: bool,
    mapping_descriptor: Mutex<Option<MappingDescriptor>>,
}

impl RemoteCameraHandle {
    pub(crate) fn from_response(
        client: HalClient,
        resource_id: ResourceId,
        response: v1::OpenCameraResponse,
    ) -> HalResult<Self> {
        let (session_id, lease) = camera_open_response_from_proto(response)
            .map_err(|error| attach_resource(error, &resource_id))?;
        Ok(Self {
            client,
            resource_id,
            session_id,
            lease,
            closed: false,
            mapping_descriptor: Mutex::new(None),
        })
    }

    pub async fn capture(&self, timeout: Duration) -> HalResult<()> {
        self.ensure_open("camera.capture")?;
        let timeout_ms = u64::try_from(timeout.as_millis()).map_err(|_| {
            self.local_error(
                "runtime.argument.invalid",
                ErrorCategory::InvalidArgument,
                "camera.capture",
                "camera capture timeout exceeds the wire range",
            )
        })?;
        if timeout_ms == 0 {
            return Err(self.local_error(
                "runtime.argument.invalid",
                ErrorCategory::InvalidArgument,
                "camera.capture",
                "camera capture timeout must be non-zero",
            ));
        }
        self.client
            .require_camera_capture("camera.capture", &self.resource_id)?;
        self.send(
            envelope::Payload::CaptureCameraRequest(v1::CaptureCameraRequest {
                session_id: self.session_id.as_str().to_owned(),
                lease: Some((&self.lease).into()),
                timeout_ms,
            }),
            ExpectedResponse::CaptureCamera {
                resource_id: self.resource_id.clone(),
            },
            "camera.capture",
        )
        .await
        .map(|_| ())
    }

    pub async fn mapping_descriptor(&self) -> HalResult<MappingDescriptor> {
        self.ensure_open("camera.mapping_descriptor")?;
        self.client
            .require_camera_frames_shm("camera.mapping_descriptor", &self.resource_id)?;
        let payload = self
            .send(
                envelope::Payload::CameraMappingDescriptorRequest(
                    v1::CameraMappingDescriptorRequest {
                        session_id: self.session_id.as_str().to_owned(),
                        lease: Some((&self.lease).into()),
                    },
                ),
                ExpectedResponse::CameraMappingDescriptor {
                    resource_id: self.resource_id.clone(),
                },
                "camera.mapping_descriptor",
            )
            .await?;
        let envelope::Payload::CameraMappingDescriptorResponse(response) = payload else {
            unreachable!()
        };
        let result = response
            .descriptor
            .ok_or_else(|| {
                robot_hal_protocol::invalid_message("camera mapping descriptor is missing")
            })
            .and_then(camera_mapping_descriptor_from_proto)
            .map_err(|error| attach_resource(error, &self.resource_id));
        if let Err(error) = &result {
            self.client.fail(error.clone());
        }
        if let Ok(descriptor) = &result {
            *self
                .mapping_descriptor
                .lock()
                .expect("camera mapping descriptor mutex is not poisoned") =
                Some(descriptor.clone());
        }
        result
    }

    /// Opens an independently read-only mapping. The mapping exposes only `copy`, never a
    /// mapping-backed frame view.
    pub fn open_mapping(&self, descriptor: &MappingDescriptor) -> HalResult<ReadOnlyMapping> {
        self.ensure_open("camera.mapping.open")?;
        self.client
            .require_camera_frames_shm("camera.mapping.open", &self.resource_id)?;
        self.ensure_current_mapping(descriptor, "camera.mapping.open")?;
        ReadOnlyMapping::open(descriptor).map_err(|error| attach_resource(error, &self.resource_id))
    }

    pub async fn next_frame_lease(
        &self,
        descriptor: &MappingDescriptor,
    ) -> HalResult<Option<FrameLease>> {
        self.ensure_open("camera.next_frame_lease")?;
        self.client
            .require_camera_frames_shm("camera.next_frame_lease", &self.resource_id)?;
        self.ensure_current_mapping(descriptor, "camera.next_frame_lease")?;
        let payload = self
            .send(
                envelope::Payload::CameraNextFrameLeaseRequest(v1::CameraNextFrameLeaseRequest {
                    session_id: self.session_id.as_str().to_owned(),
                    lease: Some((&self.lease).into()),
                }),
                ExpectedResponse::CameraNextFrameLease {
                    resource_id: self.resource_id.clone(),
                },
                "camera.next_frame_lease",
            )
            .await?;
        let envelope::Payload::CameraNextFrameLeaseResponse(response) = payload else {
            unreachable!()
        };
        let wire_lease = camera_next_frame_lease_response_from_proto(response)
            .map_err(|error| attach_resource(error, &self.resource_id));
        let wire_lease = match wire_lease {
            Ok(None) => return Ok(None),
            Ok(Some(lease)) => lease,
            Err(error) => {
                self.client.fail(error.clone());
                return Err(error);
            }
        };
        let mapping = self.open_mapping(descriptor)?;
        if mapping.mapping_identity() != descriptor.mapping_identity()
            || wire_lease.slot_index() >= mapping.slot_count()
        {
            let error = self.local_error(
                "runtime.protocol.invalid_message",
                ErrorCategory::InvalidArgument,
                "camera.next_frame_lease",
                "camera frame lease does not match the opened mapping layout",
            );
            self.client.fail(error.clone());
            return Err(error);
        }
        Ok(Some(wire_lease.bind(descriptor)))
    }

    pub async fn dropped_count(&self) -> HalResult<u64> {
        self.ensure_open("camera.dropped_count")?;
        self.client
            .require_camera_capture("camera.dropped_count", &self.resource_id)?;
        let payload = self
            .send(
                envelope::Payload::CameraDroppedCountRequest(v1::CameraDroppedCountRequest {
                    session_id: self.session_id.as_str().to_owned(),
                    lease: Some((&self.lease).into()),
                }),
                ExpectedResponse::CameraDroppedCount {
                    resource_id: self.resource_id.clone(),
                },
                "camera.dropped_count",
            )
            .await?;
        let envelope::Payload::CameraDroppedCountResponse(response) = payload else {
            unreachable!()
        };
        Ok(response.dropped_count)
    }

    pub async fn controls(&self) -> HalResult<Vec<robot_hal_camera::CameraControlDescriptor>> {
        self.ensure_open("camera.controls")?;
        self.client
            .require_camera_controls("camera.controls", &self.resource_id)?;
        let payload = self
            .send(
                envelope::Payload::CameraControlsRequest(v1::CameraControlsRequest {
                    session_id: self.session_id.as_str().to_owned(),
                    lease: Some((&self.lease).into()),
                }),
                ExpectedResponse::CameraControls {
                    resource_id: self.resource_id.clone(),
                },
                "camera.controls",
            )
            .await?;
        let envelope::Payload::CameraControlsResponse(response) = payload else {
            unreachable!()
        };
        camera_controls_response_from_proto(response)
            .map_err(|error| attach_resource(error, &self.resource_id))
    }

    pub async fn get_control(&self, kind: CameraControlKind) -> HalResult<CameraControlValue> {
        self.ensure_open("camera.control.get")?;
        self.client
            .require_camera_controls("camera.control.get", &self.resource_id)?;
        let payload = self
            .send(
                envelope::Payload::CameraGetControlRequest(v1::CameraGetControlRequest {
                    session_id: self.session_id.as_str().to_owned(),
                    lease: Some((&self.lease).into()),
                    kind: control_kind_to_proto(kind),
                }),
                ExpectedResponse::CameraGetControl {
                    resource_id: self.resource_id.clone(),
                },
                "camera.control.get",
            )
            .await?;
        let envelope::Payload::CameraGetControlResponse(response) = payload else {
            unreachable!()
        };
        let result = response
            .value
            .ok_or_else(|| robot_hal_protocol::invalid_message("camera control value is missing"))
            .and_then(camera_control_value_from_proto)
            .map_err(|error| attach_resource(error, &self.resource_id));
        if let Err(error) = &result {
            self.client.fail(error.clone());
        }
        result
    }

    pub async fn set_control(
        &self,
        kind: CameraControlKind,
        value: CameraControlValue,
    ) -> HalResult<()> {
        self.ensure_open("camera.control.set")?;
        self.client
            .require_camera_controls("camera.control.set", &self.resource_id)?;
        self.send(
            envelope::Payload::CameraSetControlRequest(v1::CameraSetControlRequest {
                session_id: self.session_id.as_str().to_owned(),
                lease: Some((&self.lease).into()),
                kind: control_kind_to_proto(kind),
                value: Some(camera_control_value_to_proto(&value)),
            }),
            ExpectedResponse::CameraSetControl {
                resource_id: self.resource_id.clone(),
            },
            "camera.control.set",
        )
        .await
        .map(|_| ())
    }

    pub async fn set_auto(&self, kind: CameraControlKind, enabled: bool) -> HalResult<()> {
        self.ensure_open("camera.control.auto")?;
        self.client
            .require_camera_controls("camera.control.auto", &self.resource_id)?;
        self.send(
            envelope::Payload::CameraSetAutoRequest(v1::CameraSetAutoRequest {
                session_id: self.session_id.as_str().to_owned(),
                lease: Some((&self.lease).into()),
                kind: control_kind_to_proto(kind),
                enabled,
            }),
            ExpectedResponse::CameraSetAuto {
                resource_id: self.resource_id.clone(),
            },
            "camera.control.auto",
        )
        .await
        .map(|_| ())
    }

    pub async fn close(&mut self) -> HalResult<()> {
        self.ensure_open("camera.close")?;
        self.send(
            envelope::Payload::CloseCameraRequest(v1::CloseCameraRequest {
                session_id: self.session_id.as_str().to_owned(),
                lease: Some((&self.lease).into()),
            }),
            ExpectedResponse::CloseCamera {
                resource_id: self.resource_id.clone(),
            },
            "camera.close",
        )
        .await?;
        self.closed = true;
        Ok(())
    }

    async fn send(
        &self,
        payload: envelope::Payload,
        expected: ExpectedResponse,
        operation: &'static str,
    ) -> HalResult<envelope::Payload> {
        self.client
            .ensure_payload_for_resource(&payload, operation, &self.resource_id)?;
        self.client
            .send(payload, expected)
            .await
            .map_err(|error| attach_resource(error, &self.resource_id))
    }

    fn ensure_open(&self, operation: &'static str) -> HalResult<()> {
        if self.closed {
            return Err(self.local_error(
                "runtime.session.closed",
                ErrorCategory::Conflict,
                operation,
                "the remote Camera handle is closed",
            ));
        }
        Ok(())
    }

    fn local_error(
        &self,
        name: &'static str,
        category: ErrorCategory,
        operation: &'static str,
        message: &'static str,
    ) -> HalError {
        HalError::new(name, category, operation, false, message)
            .expect("static Camera client error metadata is valid")
            .with_resource_id(self.resource_id.clone())
    }

    fn ensure_current_mapping(
        &self,
        descriptor: &MappingDescriptor,
        operation: &'static str,
    ) -> HalResult<()> {
        let mapping = self
            .mapping_descriptor
            .lock()
            .expect("camera mapping descriptor mutex is not poisoned");
        if mapping.as_ref().is_some_and(|current| {
            current.mapping_name() == descriptor.mapping_name()
                && current.mapping_identity() == descriptor.mapping_identity()
                && current
                    .capability_token_bytes()
                    .ct_eq(descriptor.capability_token_bytes())
                    .into()
                && current.total_length() == descriptor.total_length()
        }) {
            return Ok(());
        }
        Err(self.local_error(
            "runtime.argument.invalid",
            ErrorCategory::InvalidArgument,
            operation,
            "camera mapping descriptor does not belong to this session",
        ))
    }
}

fn control_kind_to_proto(kind: CameraControlKind) -> i32 {
    (match kind {
        CameraControlKind::Exposure => v1::CameraControlKind::Exposure,
        CameraControlKind::Gain => v1::CameraControlKind::Gain,
        CameraControlKind::WhiteBalance => v1::CameraControlKind::WhiteBalance,
        CameraControlKind::Focus => v1::CameraControlKind::Focus,
    }) as i32
}

fn attach_resource(error: HalError, resource_id: &ResourceId) -> HalError {
    if error.resource_id().is_some() {
        error
    } else {
        error.with_resource_id(resource_id.clone())
    }
}
