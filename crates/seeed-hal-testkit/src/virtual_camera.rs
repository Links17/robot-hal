use async_trait::async_trait;
use bytes::Bytes;
use seeed_hal_camera::{
    CameraAdapter, CameraCaptureSession, CameraControlDescriptor, CameraControlKind,
    CameraControlValue, CameraControlValues, CameraFormat, CameraFrame, CameraFrameMetadata,
    CameraPixelFormat, CameraPlaneLayout, CameraRequest, camera_capture_capability,
    camera_controls_capability, camera_frames_shm_capability,
};
use seeed_hal_core::{
    CapabilitySet, ErrorCategory, HalError, HalResult, IdentityQuality, ResourceDescriptor,
    ResourceId, ResourceProperties, ResourceSelector, TransportKind, resolve_resource,
};
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

const CLOCK_DOMAIN: &str = "virtual-camera";

#[derive(Clone, Debug)]
pub struct VirtualCameraAdapter {
    descriptor: ResourceDescriptor,
    state: Arc<Mutex<State>>,
}

#[derive(Debug)]
struct State {
    present: bool,
    claimed: bool,
    next_capture: Option<HalError>,
    unplug_before_next_publication: bool,
    controls: BTreeMap<CameraControlKind, ControlState>,
}

#[derive(Debug)]
struct ControlState {
    descriptor: CameraControlDescriptor,
    value: CameraControlValue,
    auto: bool,
}

impl VirtualCameraAdapter {
    pub fn pattern(resource_id: impl Into<String>) -> Self {
        let id = ResourceId::parse(resource_id.into()).expect("valid virtual camera resource id");
        let descriptor = ResourceDescriptor::new(
            id.clone(),
            seeed_hal_core::Endpoint::new(format!("virtual://camera/{}", id.as_str()))
                .expect("valid virtual camera endpoint"),
            IdentityQuality::Strong,
            TransportKind::Camera,
            ResourceProperties::new(
                [
                    ("adapter".to_owned(), "virtual".to_owned()),
                    ("mode".to_owned(), "pattern".to_owned()),
                ]
                .into_iter()
                .collect(),
            ),
            CapabilitySet::new(vec![
                camera_capture_capability(),
                camera_frames_shm_capability(),
                camera_controls_capability(),
            ]),
        );
        let controls = [
            integer_control(CameraControlKind::Exposure, 1, 1_000, 1, 100),
            integer_control(CameraControlKind::Gain, 0, 100, 1, 0),
            enum_control(
                CameraControlKind::WhiteBalance,
                &["auto", "daylight", "tungsten"],
                "auto",
            ),
            integer_control(CameraControlKind::Focus, 0, 255, 1, 0),
        ]
        .into_iter()
        .collect();
        Self {
            descriptor,
            state: Arc::new(Mutex::new(State {
                present: true,
                claimed: false,
                next_capture: None,
                unplug_before_next_publication: false,
                controls,
            })),
        }
    }

    pub fn fail_next_capture(&self, error: HalError) {
        self.state
            .lock()
            .expect("virtual camera mutex poisoned")
            .next_capture = Some(error);
    }

    pub fn unplug(&self) {
        self.state
            .lock()
            .expect("virtual camera mutex poisoned")
            .present = false;
    }

    /// Makes a previously unplugged virtual camera discoverable again.
    pub fn plug(&self) {
        self.state
            .lock()
            .expect("virtual camera mutex poisoned")
            .present = true;
    }

    pub fn unplug_before_next_publication(&self) {
        self.state
            .lock()
            .expect("virtual camera mutex poisoned")
            .unplug_before_next_publication = true;
    }
}

#[async_trait]
impl CameraAdapter for VirtualCameraAdapter {
    fn adapter_name(&self) -> &'static str {
        "virtual.camera.pattern"
    }

    async fn enumerate(&self) -> HalResult<Vec<ResourceDescriptor>> {
        let state = self.state.lock().expect("virtual camera mutex poisoned");
        Ok(state
            .present
            .then(|| vec![self.descriptor.clone()])
            .unwrap_or_default())
    }

    async fn open(
        &self,
        selector: &ResourceSelector,
        request: &CameraRequest,
    ) -> HalResult<Box<dyn CameraCaptureSession>> {
        request.format().validate()?;
        if !is_supported(request.format()) {
            return Err(HalError::new(
                "camera.format.unsupported",
                ErrorCategory::InvalidArgument,
                "camera.open",
                false,
                "virtual camera does not support the requested format",
            )?);
        }
        let mut state = self.state.lock().expect("virtual camera mutex poisoned");
        if !state.present {
            return Err(unplugged("camera.open", &self.descriptor));
        }
        let descriptor = resolve_resource(
            std::slice::from_ref(&self.descriptor),
            selector,
            &camera_capture_capability(),
            "camera.open",
        )?
        .clone();
        if state.claimed {
            return Err(conflict("camera.open", &descriptor));
        }
        state.claimed = true;
        Ok(Box::new(VirtualCameraSession {
            descriptor,
            format: request.format().clone(),
            state: Arc::clone(&self.state),
            next_sequence: 1,
            closed: false,
        }))
    }
}

struct VirtualCameraSession {
    descriptor: ResourceDescriptor,
    format: CameraFormat,
    state: Arc<Mutex<State>>,
    next_sequence: u64,
    closed: bool,
}

#[async_trait]
impl CameraCaptureSession for VirtualCameraSession {
    fn descriptor(&self) -> &ResourceDescriptor {
        &self.descriptor
    }

    fn format(&self) -> &CameraFormat {
        &self.format
    }

    async fn capture(&mut self, _timeout: Duration) -> HalResult<CameraFrame> {
        let mut state = self.state.lock().expect("virtual camera mutex poisoned");
        ensure_active_locked(self.closed, &state, "camera.capture", &self.descriptor)?;
        unplug_before_publication(&mut state);
        ensure_active_locked(self.closed, &state, "camera.capture", &self.descriptor)?;
        if let Some(error) = state.next_capture.take() {
            return Err(error.with_resource_id(self.descriptor.id().clone()));
        }
        let payload_length = payload_length(&self.format)?;
        let payload = Bytes::from(
            (0..payload_length)
                .map(|index| (index % 251) as u8)
                .collect::<Vec<_>>(),
        );
        let metadata = CameraFrameMetadata::new(
            self.format.clone(),
            plane_layout(&self.format, payload_length)?,
            self.next_sequence,
            self.next_sequence,
            CLOCK_DOMAIN,
            0,
        )?;
        self.next_sequence = self.next_sequence.saturating_add(1);
        CameraFrame::new(metadata, payload)
    }

    async fn controls(&mut self) -> HalResult<Vec<CameraControlDescriptor>> {
        let mut state = self.state.lock().expect("virtual camera mutex poisoned");
        ensure_active_locked(self.closed, &state, "camera.controls", &self.descriptor)?;
        unplug_before_publication(&mut state);
        ensure_active_locked(self.closed, &state, "camera.controls", &self.descriptor)?;
        Ok(state
            .controls
            .values()
            .map(|control| control.descriptor.clone())
            .collect())
    }

    async fn get_control(&mut self, kind: CameraControlKind) -> HalResult<CameraControlValue> {
        let state = self.state.lock().expect("virtual camera mutex poisoned");
        ensure_active_locked(self.closed, &state, "camera.control.get", &self.descriptor)?;
        let control = state
            .controls
            .get(&kind)
            .ok_or_else(|| unsupported("camera.control.get", &self.descriptor))?;
        if !control.descriptor.readable() {
            return Err(unsupported("camera.control.get", &self.descriptor));
        }
        Ok(control.value.clone())
    }

    async fn set_control(
        &mut self,
        kind: CameraControlKind,
        value: CameraControlValue,
    ) -> HalResult<()> {
        let mut state = self.state.lock().expect("virtual camera mutex poisoned");
        ensure_active_locked(self.closed, &state, "camera.control.set", &self.descriptor)?;
        let control = state
            .controls
            .get_mut(&kind)
            .ok_or_else(|| unsupported("camera.control.set", &self.descriptor))?;
        if !control.descriptor.writable() || !control.descriptor.values().contains(&value) {
            return Err(HalError::new(
                "camera.control.invalid",
                ErrorCategory::InvalidArgument,
                "camera.control.set",
                false,
                "camera control value is unsupported",
            )?
            .with_resource_id(self.descriptor.id().clone()));
        }
        control.value = value;
        control.auto = false;
        Ok(())
    }

    async fn set_auto(&mut self, kind: CameraControlKind, enabled: bool) -> HalResult<()> {
        let mut state = self.state.lock().expect("virtual camera mutex poisoned");
        ensure_active_locked(self.closed, &state, "camera.control.auto", &self.descriptor)?;
        let control = state
            .controls
            .get_mut(&kind)
            .ok_or_else(|| unsupported("camera.control.auto", &self.descriptor))?;
        if !control.descriptor.auto_supported() {
            return Err(unsupported("camera.control.auto", &self.descriptor));
        }
        control.auto = enabled;
        Ok(())
    }

    async fn close(&mut self) -> HalResult<()> {
        if !self.closed {
            self.state
                .lock()
                .expect("virtual camera mutex poisoned")
                .claimed = false;
            self.closed = true;
        }
        Ok(())
    }
}

impl Drop for VirtualCameraSession {
    fn drop(&mut self) {
        if !self.closed {
            self.state
                .lock()
                .expect("virtual camera mutex poisoned")
                .claimed = false;
        }
    }
}

fn integer_control(
    kind: CameraControlKind,
    minimum: i64,
    maximum: i64,
    step: i64,
    value: i64,
) -> (CameraControlKind, ControlState) {
    let descriptor = CameraControlDescriptor::new(
        kind,
        true,
        true,
        true,
        CameraControlValues::range(minimum, maximum, step).expect("virtual camera range is valid"),
        true,
        None,
    )
    .expect("virtual camera control is valid");
    (
        kind,
        ControlState {
            descriptor,
            value: CameraControlValue::Integer(value),
            auto: false,
        },
    )
}

fn enum_control(
    kind: CameraControlKind,
    values: &[&str],
    value: &str,
) -> (CameraControlKind, ControlState) {
    let values = values
        .iter()
        .map(|value| CameraControlValue::Enum((*value).to_owned()))
        .collect();
    let descriptor = CameraControlDescriptor::new(
        kind,
        true,
        true,
        true,
        CameraControlValues::enumerated(values).expect("virtual camera enum is valid"),
        true,
        None,
    )
    .expect("virtual camera control is valid");
    (
        kind,
        ControlState {
            descriptor,
            value: CameraControlValue::Enum(value.to_owned()),
            auto: false,
        },
    )
}

fn is_supported(format: &CameraFormat) -> bool {
    matches!(
        format.pixel_format(),
        CameraPixelFormat::Nv12 | CameraPixelFormat::Yuyv
    ) && matches!((format.width(), format.height()), (640, 480) | (320, 240))
}

fn payload_length(format: &CameraFormat) -> HalResult<usize> {
    format.worst_case_frame_bytes()
}

fn plane_layout(format: &CameraFormat, payload_length: usize) -> HalResult<Vec<CameraPlaneLayout>> {
    let width = format.width() as usize;
    let height = format.height() as usize;
    match format.pixel_format() {
        CameraPixelFormat::Nv12 => {
            let y_length = width * height;
            Ok(vec![
                CameraPlaneLayout::new(0, y_length, width)?,
                CameraPlaneLayout::new(y_length, payload_length - y_length, width)?,
            ])
        }
        CameraPixelFormat::Yuyv | CameraPixelFormat::Mjpeg => {
            Ok(vec![CameraPlaneLayout::new(0, payload_length, width * 2)?])
        }
    }
}

fn ensure_active_locked(
    closed_session: bool,
    state: &State,
    operation: &'static str,
    descriptor: &ResourceDescriptor,
) -> HalResult<()> {
    if closed_session {
        return Err(closed(operation, descriptor));
    }
    if !state.present {
        return Err(unplugged(operation, descriptor));
    }
    Ok(())
}

fn unplug_before_publication(state: &mut State) {
    if state.unplug_before_next_publication {
        state.unplug_before_next_publication = false;
        state.present = false;
    }
}

fn conflict(operation: &'static str, descriptor: &ResourceDescriptor) -> HalError {
    HalError::new(
        "runtime.adapter.conflict",
        ErrorCategory::Conflict,
        operation,
        false,
        "camera capture session is already open",
    )
    .expect("static camera conflict metadata is valid")
    .with_resource_id(descriptor.id().clone())
}

fn closed(operation: &'static str, descriptor: &ResourceDescriptor) -> HalError {
    HalError::new(
        "runtime.session.closed",
        ErrorCategory::Conflict,
        operation,
        false,
        "camera session is closed",
    )
    .expect("static camera closed metadata is valid")
    .with_resource_id(descriptor.id().clone())
}

fn unplugged(operation: &'static str, descriptor: &ResourceDescriptor) -> HalError {
    HalError::new(
        "camera.session.unplugged",
        ErrorCategory::Unavailable,
        operation,
        false,
        "camera was unplugged",
    )
    .expect("static camera unplugged metadata is valid")
    .with_resource_id(descriptor.id().clone())
}

fn unsupported(operation: &'static str, descriptor: &ResourceDescriptor) -> HalError {
    HalError::new(
        "camera.control.unsupported",
        ErrorCategory::InvalidArgument,
        operation,
        false,
        "camera control or operation is unsupported",
    )
    .expect("static camera unsupported metadata is valid")
    .with_resource_id(descriptor.id().clone())
}

/// Capability-gated reusable checks for the public camera adapter/session contract.
pub async fn run_camera_adapter_conformance<A: CameraAdapter>(adapter: &A) -> HalResult<()> {
    let descriptors = adapter.enumerate().await?;
    let descriptor = descriptors.first().ok_or_else(|| {
        HalError::new(
            "runtime.resource.not_found",
            ErrorCategory::NotFound,
            "camera.conformance",
            false,
            "adapter enumerated no camera resources",
        )
        .expect("static camera conformance error metadata is valid")
    })?;
    if !descriptor
        .capabilities()
        .contains(&camera_capture_capability())
    {
        return Err(HalError::new(
            "camera.capture.unsupported",
            ErrorCategory::InvalidArgument,
            "camera.conformance",
            false,
            "camera descriptor does not advertise capture",
        )?);
    }
    if descriptor.transport() != TransportKind::Camera {
        return Err(HalError::new(
            "camera.descriptor.invalid",
            ErrorCategory::InvalidArgument,
            "camera.conformance",
            false,
            "camera descriptor must use the camera transport kind",
        )?);
    }
    let request = CameraRequest::new(
        CameraFormat::new(CameraPixelFormat::Nv12, 640, 480)?,
        seeed_hal_camera::DEFAULT_CAMERA_SLOT_COUNT,
    )?;
    let selector = descriptor.selector();
    let mut session = adapter.open(&selector, &request).await?;
    assert_eq!(session.format(), request.format());

    let first = session.capture(Duration::ZERO).await?;
    let second = session.capture(Duration::ZERO).await?;
    assert_eq!(first.metadata().format(), request.format());
    assert!(first.payload().len() <= seeed_hal_camera::MAX_CAMERA_FRAME_BYTES);
    assert!(second.metadata().sequence() > first.metadata().sequence());
    assert!(
        second.metadata().monotonic_timestamp_ns() >= first.metadata().monotonic_timestamp_ns()
    );
    assert!(!second.metadata().clock_domain().is_empty());

    let controls = session.controls().await?;
    for kind in [
        CameraControlKind::Exposure,
        CameraControlKind::Gain,
        CameraControlKind::WhiteBalance,
        CameraControlKind::Focus,
    ] {
        assert!(controls.iter().any(|descriptor| descriptor.kind() == kind));
        let _ = session.get_control(kind).await?;
    }

    session.close().await?;
    let error = session
        .capture(Duration::ZERO)
        .await
        .expect_err("closed sessions must reject capture");
    assert_eq!(error.name().as_str(), "runtime.session.closed");

    let unsupported = CameraRequest::new(
        CameraFormat::new(CameraPixelFormat::Mjpeg, 640, 480)?,
        seeed_hal_camera::DEFAULT_CAMERA_SLOT_COUNT,
    )?;
    let error = match adapter.open(&selector, &unsupported).await {
        Ok(_) => panic!("an unsupported requested format must fail closed"),
        Err(error) => error,
    };
    assert_eq!(error.name().as_str(), "camera.format.unsupported");
    Ok(())
}
