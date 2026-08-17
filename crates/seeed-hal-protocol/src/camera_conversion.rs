use std::time::Duration;

use seeed_hal_adapter_shared_memory::{FrameLease, MappingDescriptor};
use seeed_hal_camera::{
    CameraControlDescriptor, CameraControlKind, CameraControlValue, CameraControlValues,
    CameraFormat, CameraPixelFormat, CameraRequest, MAX_CAMERA_SLOT_COUNT, MIN_CAMERA_SLOT_COUNT,
};
use seeed_hal_core::{HalResult, LeaseMode, LeaseToken, ResourceSelector, SessionId};

use crate::{invalid_message, v1};

pub fn camera_selector_from_proto(value: v1::ResourceSelector) -> HalResult<ResourceSelector> {
    let selector: ResourceSelector = value.try_into()?;
    if selector.transport() != seeed_hal_core::TransportKind::Camera {
        return Err(invalid_message(
            "camera resource selector transport must be Camera",
        ));
    }
    Ok(selector)
}

pub fn camera_format_from_proto(value: v1::CameraFormat) -> HalResult<CameraFormat> {
    let pixel_format = match v1::CameraPixelFormat::try_from(value.pixel_format)
        .map_err(|_| invalid_message("camera format has an unknown pixel format"))?
    {
        v1::CameraPixelFormat::Nv12 => CameraPixelFormat::Nv12,
        v1::CameraPixelFormat::Yuyv => CameraPixelFormat::Yuyv,
        v1::CameraPixelFormat::Mjpeg => CameraPixelFormat::Mjpeg,
        v1::CameraPixelFormat::Unspecified => {
            return Err(invalid_message("camera format pixel format is required"));
        }
    };
    CameraFormat::new(pixel_format, value.width, value.height)
        .map_err(|_| invalid_message("camera format violates public bounds"))
}

pub fn camera_open_request_from_proto(
    value: v1::OpenCameraRequest,
) -> HalResult<(ResourceSelector, CameraRequest)> {
    let selector = value
        .selector
        .ok_or_else(|| invalid_message("camera open selector is required"))?;
    let request = value
        .request
        .ok_or_else(|| invalid_message("camera open request is required"))?;
    let format = request
        .format
        .ok_or_else(|| invalid_message("camera request format is required"))?;
    let slots = usize::try_from(request.slot_count)
        .map_err(|_| invalid_message("camera slot count is invalid"))?;
    if !(MIN_CAMERA_SLOT_COUNT..=MAX_CAMERA_SLOT_COUNT).contains(&slots) {
        return Err(invalid_message("camera slot count violates public bounds"));
    }
    Ok((
        camera_selector_from_proto(selector)?,
        CameraRequest::new(camera_format_from_proto(format)?, slots)
            .map_err(|_| invalid_message("camera request violates public bounds"))?,
    ))
}

pub fn camera_session_lease_from_proto(
    session_id: String,
    lease: Option<v1::LeaseToken>,
) -> HalResult<(SessionId, LeaseToken)> {
    let (session, lease) = crate::parse_session_lease(session_id, lease)?;
    if lease.mode() != LeaseMode::Control {
        return Err(invalid_message("Camera session lease mode must be Control"));
    }
    Ok((session, lease))
}

pub fn camera_capture_request_from_proto(
    value: v1::CaptureCameraRequest,
) -> HalResult<(SessionId, LeaseToken, Duration)> {
    if value.timeout_ms == 0 {
        return Err(invalid_message(
            "camera capture timeout must be greater than zero",
        ));
    }
    let (session, lease) = camera_session_lease_from_proto(value.session_id, value.lease)?;
    Ok((session, lease, Duration::from_millis(value.timeout_ms)))
}

pub fn camera_open_response_to_proto(
    session: &SessionId,
    lease: &LeaseToken,
) -> v1::OpenCameraResponse {
    v1::OpenCameraResponse {
        session_id: session.as_str().to_owned(),
        lease: Some(lease.into()),
    }
}

pub fn camera_open_response_from_proto(
    value: v1::OpenCameraResponse,
) -> HalResult<(SessionId, LeaseToken)> {
    camera_session_lease_from_proto(value.session_id, value.lease)
}

pub fn camera_control_kind_from_proto(value: i32) -> HalResult<CameraControlKind> {
    match v1::CameraControlKind::try_from(value)
        .map_err(|_| invalid_message("camera control kind has an unknown value"))?
    {
        v1::CameraControlKind::Exposure => Ok(CameraControlKind::Exposure),
        v1::CameraControlKind::Gain => Ok(CameraControlKind::Gain),
        v1::CameraControlKind::WhiteBalance => Ok(CameraControlKind::WhiteBalance),
        v1::CameraControlKind::Focus => Ok(CameraControlKind::Focus),
        v1::CameraControlKind::Unspecified => {
            Err(invalid_message("camera control kind is required"))
        }
    }
}

pub fn camera_control_value_from_proto(
    value: v1::CameraControlValue,
) -> HalResult<CameraControlValue> {
    match value.value {
        Some(v1::camera_control_value::Value::IntegerValue(value)) => {
            Ok(CameraControlValue::Integer(value))
        }
        Some(v1::camera_control_value::Value::EnumValue(value))
            if !value.is_empty() && value.is_ascii() && value.len() <= 255 =>
        {
            Ok(CameraControlValue::Enum(value))
        }
        Some(v1::camera_control_value::Value::EnumValue(_)) => {
            Err(invalid_message("camera enum control value is invalid"))
        }
        None => Err(invalid_message("camera control value is required")),
    }
}

pub fn camera_control_value_to_proto(value: &CameraControlValue) -> v1::CameraControlValue {
    v1::CameraControlValue {
        value: Some(match value {
            CameraControlValue::Integer(value) => {
                v1::camera_control_value::Value::IntegerValue(*value)
            }
            CameraControlValue::Enum(value) => {
                v1::camera_control_value::Value::EnumValue(value.clone())
            }
        }),
    }
}

pub fn camera_control_descriptor_from_proto(
    value: v1::CameraControlDescriptor,
) -> HalResult<CameraControlDescriptor> {
    let values = match value
        .values
        .ok_or_else(|| invalid_message("camera control values are required"))?
        .values
    {
        Some(v1::camera_control_values::Values::Range(range)) => {
            CameraControlValues::range(range.minimum, range.maximum, range.step)
        }
        Some(v1::camera_control_values::Values::Enumerated(values)) => {
            if values.values.len() > 64 {
                return Err(invalid_message(
                    "camera enumerated control values exceed bound",
                ));
            }
            CameraControlValues::enumerated(
                values
                    .values
                    .into_iter()
                    .map(camera_control_value_from_proto)
                    .collect::<HalResult<Vec<_>>>()?,
            )
        }
        None => return Err(invalid_message("camera control values kind is required")),
    }
    .map_err(|_| invalid_message("camera control values are invalid"))?;
    if !value.diagnostic.is_empty()
        && (!value.diagnostic.is_ascii() || value.diagnostic.len() > 255)
    {
        return Err(invalid_message("camera control diagnostic is invalid"));
    }
    CameraControlDescriptor::new(
        camera_control_kind_from_proto(value.kind)?,
        value.readable,
        value.writable,
        value.auto_supported,
        values,
        value.current_value_available,
        (!value.diagnostic.is_empty()).then_some(value.diagnostic),
    )
    .map_err(|_| invalid_message("camera control descriptor is invalid"))
}

pub fn camera_control_descriptor_to_proto(
    value: &CameraControlDescriptor,
) -> v1::CameraControlDescriptor {
    let values = match value.values() {
        CameraControlValues::Range {
            minimum,
            maximum,
            step,
        } => v1::camera_control_values::Values::Range(v1::CameraControlRange {
            minimum: *minimum,
            maximum: *maximum,
            step: *step,
        }),
        CameraControlValues::Enumerated(values) => {
            v1::camera_control_values::Values::Enumerated(v1::CameraControlEnumValues {
                values: values.iter().map(camera_control_value_to_proto).collect(),
            })
        }
    };
    v1::CameraControlDescriptor {
        kind: match value.kind() {
            CameraControlKind::Exposure => v1::CameraControlKind::Exposure,
            CameraControlKind::Gain => v1::CameraControlKind::Gain,
            CameraControlKind::WhiteBalance => v1::CameraControlKind::WhiteBalance,
            CameraControlKind::Focus => v1::CameraControlKind::Focus,
        } as i32,
        readable: value.readable(),
        writable: value.writable(),
        auto_supported: value.auto_supported(),
        values: Some(v1::CameraControlValues {
            values: Some(values),
        }),
        current_value_available: value.current_value_available(),
        diagnostic: value.diagnostic().unwrap_or_default().to_owned(),
    }
}

pub fn camera_controls_response_from_proto(
    value: v1::CameraControlsResponse,
) -> HalResult<Vec<CameraControlDescriptor>> {
    if value.controls.len() > 4 {
        return Err(invalid_message(
            "camera control descriptor count exceeds bound",
        ));
    }
    value
        .controls
        .into_iter()
        .map(camera_control_descriptor_from_proto)
        .collect()
}

pub fn camera_mapping_descriptor_from_proto(
    value: v1::MappingDescriptor,
) -> HalResult<MappingDescriptor> {
    if value.mapping_name.is_empty()
        || !value.mapping_name.is_ascii()
        || value.mapping_name.len() > 255
        || value.mapping_identity.len() != 32
        || value.capability_token.len() != 32
        || value.total_length == 0
        || value.total_length > 256 * 1024 * 1024
    {
        return Err(invalid_message("camera mapping descriptor is invalid"));
    }
    let identity: [u8; 32] = value
        .mapping_identity
        .try_into()
        .expect("length is checked");
    let token: [u8; 32] = value
        .capability_token
        .try_into()
        .expect("length is checked");
    MappingDescriptor::new(
        value.mapping_name,
        identity,
        token,
        usize::try_from(value.total_length)
            .map_err(|_| invalid_message("camera mapping length is invalid"))?,
    )
    .map_err(|_| invalid_message("camera mapping descriptor is invalid"))
}

pub fn camera_mapping_descriptor_to_proto(value: &MappingDescriptor) -> v1::MappingDescriptor {
    v1::MappingDescriptor {
        mapping_name: value.mapping_name().to_owned(),
        mapping_identity: value.mapping_identity().bytes().to_vec(),
        capability_token: value.capability_token_bytes().to_vec(),
        total_length: value.total_length() as u64,
    }
}

pub fn camera_next_frame_lease_response_from_proto(
    value: v1::CameraNextFrameLeaseResponse,
) -> HalResult<Option<FrameLease>> {
    let Some(lease) = value.lease else {
        return Ok(None);
    };
    if lease.generation == 0 {
        return Err(invalid_message("camera frame lease generation is required"));
    }
    let slot_index = usize::try_from(lease.slot_index)
        .map_err(|_| invalid_message("camera frame slot index is invalid"))?;
    if slot_index >= MAX_CAMERA_SLOT_COUNT {
        return Err(invalid_message("camera frame slot index exceeds bound"));
    }
    // Frame leases are meaningful only with their mapping identity; callers
    // combine this wire value with their previously authenticated descriptor.
    Ok(None)
}

pub fn frame_lease_to_proto(value: &FrameLease) -> v1::FrameLease {
    v1::FrameLease {
        slot_index: value.slot_index() as u32,
        sequence: value.sequence(),
        generation: value.generation(),
    }
}
