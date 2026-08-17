use async_trait::async_trait;
use bytes::Bytes;
use seeed_hal_camera::{
    CameraCaptureSession, CameraControlDescriptor, CameraControlKind, CameraControlValue,
    CameraFormat, CameraFrame, CameraFrameMetadata, CameraPixelFormat, CameraPlaneLayout,
    CameraRequest, MAX_CAMERA_FRAME_BYTES, camera_capture_capability, camera_frames_shm_capability,
};
use seeed_hal_core::{
    CapabilitySet, Endpoint, ErrorCategory, HalError, HalResult, IdentityQuality,
    ResourceDescriptor, ResourceProperties, ResourceSelector, TransportKind, resolve_resource,
};
use std::{
    collections::{BTreeMap, BTreeSet},
    ffi::OsStr,
    path::Path,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    thread,
    time::{Duration, Instant},
};
use tokio::sync::oneshot;
use v4l::{
    Device,
    buffer::{Flags as BufferFlags, Type},
    capability::Flags as CapabilityFlags,
    format::{Format as V4lFormat, FourCC},
    io::{
        mmap::Stream,
        traits::{CaptureStream, Stream as V4lStream},
    },
    video::traits::Capture,
};

use super::{
    encode_resource_id,
    wait::{WaitResult, bounded_wait},
};

const CLOCK_DOMAIN: &str = "v4l2.receipt.monotonic";
const V4L2_BUFFER_COUNT: u32 = 4;
const V4L2_WAIT_QUANTUM: Duration = Duration::from_millis(20);
const V4L2_CLOSE_TIMEOUT: Duration = Duration::from_secs(1);

pub(super) fn enumerate_sync() -> HalResult<Vec<ResourceDescriptor>> {
    let paths =
        std::fs::read_dir("/dev").map_err(|error| platform_error("camera.enumerate", error))?;
    paths
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| is_video_endpoint(path))
        .map(|path| descriptor_from_path(&path))
        .filter_map(|result| match result {
            Ok(Some(descriptor)) => Some(Ok(descriptor)),
            Ok(None) => None,
            Err(error) => Some(Err(error)),
        })
        .collect()
}

pub(super) fn open_sync(
    selector: &ResourceSelector,
    request: &CameraRequest,
    claims: Arc<Mutex<BTreeSet<seeed_hal_core::ResourceId>>>,
) -> HalResult<Box<dyn CameraCaptureSession>> {
    request.format().validate()?;
    let descriptor = resolve_resource(
        &enumerate_sync()?,
        selector,
        &camera_capture_capability(),
        "camera.open",
    )?
    .clone();
    {
        let mut claimed = claims.lock().expect("V4L2 claim mutex poisoned");
        if !claimed.insert(descriptor.id().clone()) {
            return Err(adapter_conflict("camera.open", &descriptor));
        }
    }
    match start_worker(descriptor.clone(), request.format().clone()) {
        Ok((sender, worker, shutdown)) => Ok(Box::new(V4l2Session {
            descriptor,
            format: request.format().clone(),
            sender: Some(sender),
            worker: Some(worker),
            shutdown,
            claims,
            closed: false,
        })),
        Err(error) => {
            claims
                .lock()
                .expect("V4L2 claim mutex poisoned")
                .remove(descriptor.id());
            Err(error)
        }
    }
}

fn is_video_endpoint(path: &Path) -> bool {
    path.file_name()
        .and_then(OsStr::to_str)
        .is_some_and(|name| {
            name.starts_with("video") && name[5..].bytes().all(|byte| byte.is_ascii_digit())
        })
}

fn descriptor_from_path(path: &Path) -> HalResult<Option<ResourceDescriptor>> {
    let device =
        Device::with_path(path).map_err(|error| platform_error("camera.enumerate", error))?;
    let capabilities = device
        .query_caps()
        .map_err(|error| platform_error("camera.enumerate", error))?;
    if !capabilities
        .capabilities
        .contains(CapabilityFlags::VIDEO_CAPTURE)
        || !capabilities
            .capabilities
            .contains(CapabilityFlags::STREAMING)
    {
        return Ok(None);
    }
    let endpoint = path.to_string_lossy().into_owned();
    let evidence = identity_evidence(path)?;
    let mut properties = BTreeMap::new();
    properties.insert("adapter".to_owned(), "v4l2".to_owned());
    properties.insert("endpoint".to_owned(), endpoint.clone());
    properties.insert("camera.driver".to_owned(), capabilities.driver);
    properties.insert("camera.card".to_owned(), capabilities.card);
    properties.insert("camera.bus".to_owned(), capabilities.bus);
    properties.insert(
        "camera.identity_evidence".to_owned(),
        evidence.value.clone(),
    );
    properties.insert(
        "camera.identity_source".to_owned(),
        evidence.source.to_owned(),
    );
    Ok(Some(ResourceDescriptor::new(
        encode_resource_id(&evidence.value)?,
        Endpoint::new(endpoint)?,
        evidence.quality,
        TransportKind::Camera,
        ResourceProperties::new(properties),
        CapabilitySet::new(vec![
            camera_capture_capability(),
            camera_frames_shm_capability(),
        ]),
    )))
}

struct IdentityEvidence {
    value: String,
    source: &'static str,
    quality: IdentityQuality,
}

fn identity_evidence(endpoint: &Path) -> HalResult<IdentityEvidence> {
    let name = endpoint
        .file_name()
        .and_then(OsStr::to_str)
        .ok_or_else(|| invalid("camera.identity", "V4L2 endpoint has no UTF-8 filename"))?;
    let mut current = std::fs::canonicalize(
        Path::new("/sys/class/video4linux")
            .join(name)
            .join("device"),
    )
    .map_err(|error| platform_error("camera.identity", error))?;
    for _ in 0..16 {
        let serial = current.join("serial");
        if let Ok(value) = std::fs::read_to_string(&serial) {
            if let Some(evidence) =
                identity_from_serial_or_sysfs_path(value.trim(), current.to_string_lossy().as_ref())
            {
                return Ok(evidence);
            }
        }
        if !current.pop() {
            break;
        }
    }
    let canonical = std::fs::canonicalize(
        Path::new("/sys/class/video4linux")
            .join(name)
            .join("device"),
    )
    .map_err(|error| platform_error("camera.identity", error))?;
    let unique_path = canonical.to_string_lossy().into_owned();
    if unique_path.is_empty() {
        return Err(invalid(
            "camera.identity",
            "V4L2 sysfs device path must not be empty",
        ));
    }
    identity_from_serial_or_sysfs_path("", &unique_path).ok_or_else(|| {
        invalid(
            "camera.identity",
            "V4L2 sysfs device path must not be empty",
        )
    })
}

fn identity_from_serial_or_sysfs_path(
    serial: &str,
    canonical_device_path: &str,
) -> Option<IdentityEvidence> {
    if !serial.is_empty() {
        return Some(IdentityEvidence {
            value: format!("serial:{serial}"),
            source: "sysfs.serial",
            quality: IdentityQuality::Strong,
        });
    }
    (!canonical_device_path.is_empty()).then(|| IdentityEvidence {
        value: format!("sysfs:{canonical_device_path}"),
        source: "sysfs.canonical_device_path",
        quality: IdentityQuality::Medium,
    })
}

fn start_worker(
    descriptor: ResourceDescriptor,
    requested: CameraFormat,
) -> HalResult<(
    mpsc::SyncSender<Command>,
    thread::JoinHandle<()>,
    Arc<AtomicBool>,
)> {
    let (sender, receiver) = mpsc::sync_channel(1);
    let (ready_sender, ready_receiver) = mpsc::sync_channel(1);
    let shutdown = Arc::new(AtomicBool::new(false));
    let worker_shutdown = Arc::clone(&shutdown);
    let worker = thread::Builder::new()
        .name("seeed-hal-v4l2-capture".to_owned())
        .spawn(move || {
            capture_worker(
                requested,
                descriptor,
                receiver,
                ready_sender,
                worker_shutdown,
            )
        })
        .map_err(|error| platform_error("camera.open", error))?;
    ready_receiver
        .recv()
        .map_err(|_| platform_error("camera.open", "V4L2 capture worker exited before setup"))??;
    Ok((sender, worker, shutdown))
}

enum Command {
    Capture {
        deadline: Instant,
        response: oneshot::Sender<HalResult<CameraFrame>>,
    },
    Close,
}

fn capture_worker(
    format: CameraFormat,
    descriptor: ResourceDescriptor,
    receiver: mpsc::Receiver<Command>,
    ready: mpsc::SyncSender<HalResult<usize>>,
    shutdown: Arc<AtomicBool>,
) {
    let device = match Device::with_path(descriptor.endpoint().as_str()) {
        Ok(device) => device,
        Err(error) => {
            let _ = ready.send(Err(platform_error("camera.open", error)));
            return;
        }
    };
    let actual = match device.set_format(&V4lFormat::new(
        format.width(),
        format.height(),
        fourcc(format.pixel_format()),
    )) {
        Ok(actual) => actual,
        Err(error) => {
            let _ = ready.send(Err(platform_error("camera.open", error)));
            return;
        }
    };
    if let Err(error) = ensure_exact_format(&format, &actual, &descriptor) {
        let _ = ready.send(Err(error));
        return;
    }
    let stride = match usize::try_from(actual.stride) {
        Ok(stride) => stride,
        Err(_) => {
            let _ = ready.send(Err(invalid(
                "camera.open",
                "V4L2 stride does not fit usize",
            )));
            return;
        }
    };
    let mut stream = match Stream::with_buffers(&device, Type::VideoCapture, V4L2_BUFFER_COUNT) {
        Ok(stream) => stream,
        Err(error) => {
            let _ = ready.send(Err(platform_error("camera.open", error)));
            return;
        }
    };
    if ready.send(Ok(stride)).is_err() {
        return;
    }
    let started = Instant::now();
    let mut sequence = 1_u64;
    while let Ok(command) = receiver.recv() {
        match command {
            Command::Capture { deadline, response } => {
                let result = bounded_wait(
                    deadline,
                    V4L2_WAIT_QUANTUM,
                    || shutdown.load(Ordering::Acquire),
                    |wait_for| {
                        stream.set_timeout(wait_for);
                        let result = capture_one(
                            &mut stream,
                            &format,
                            stride,
                            &descriptor,
                            sequence,
                            started,
                        );
                        if result.as_ref().is_err_and(|error| {
                            error.name().as_str() == "runtime.transport.timeout"
                        }) {
                            stream.stop().map_err(|error| {
                                platform_error("camera.capture", error)
                                    .with_resource_id(descriptor.id().clone())
                            })?;
                        }
                        result
                    },
                    |error| error.name().as_str() == "runtime.transport.timeout",
                );
                match result {
                    Ok(WaitResult::Ready(frame)) => {
                        sequence = sequence.saturating_add(1);
                        let _ = response.send(Ok(frame));
                    }
                    Ok(WaitResult::TimedOut) => {
                        let _ = response.send(Err(timeout_error("camera.capture", &descriptor)));
                    }
                    Ok(WaitResult::Shutdown) => break,
                    Err(error) => {
                        let _ = response.send(Err(error));
                    }
                }
            }
            Command::Close => break,
        }
    }
}

fn capture_one(
    stream: &mut Stream<'_>,
    format: &CameraFormat,
    stride: usize,
    descriptor: &ResourceDescriptor,
    sequence: u64,
    started: Instant,
) -> HalResult<CameraFrame> {
    let (native, metadata) = stream.next().map_err(|error| {
        if error.kind() == std::io::ErrorKind::TimedOut {
            timeout_error("camera.capture", descriptor)
        } else {
            platform_error("camera.capture", error).with_resource_id(descriptor.id().clone())
        }
    })?;
    if metadata.flags.contains(BufferFlags::ERROR) {
        return Err(
            platform_error("camera.capture", "V4L2 returned a corrupt frame")
                .with_resource_id(descriptor.id().clone()),
        );
    }
    let used = usize::try_from(metadata.bytesused)
        .map_err(|_| invalid("camera.capture", "V4L2 bytesused does not fit usize"))?;
    let expected_maximum = usize::try_from(format.worst_case_frame_bytes()?)
        .map_err(|_| invalid("camera.capture", "camera frame maximum does not fit usize"))?;
    if used == 0 || used > native.len() || used > expected_maximum || used > MAX_CAMERA_FRAME_BYTES
    {
        return Err(invalid(
            "camera.capture",
            "V4L2 frame payload exceeds negotiated or mapped bounds",
        )
        .with_resource_id(descriptor.id().clone()));
    }
    validate_payload_layout(format, stride, native, used, descriptor)?;
    let planes = plane_layout(format, stride, used, descriptor)?;
    let receipt_timestamp_ns = u64::try_from(started.elapsed().as_nanos()).unwrap_or(u64::MAX);
    CameraFrame::new(
        CameraFrameMetadata::new(
            format.clone(),
            planes,
            sequence,
            receipt_timestamp_ns,
            CLOCK_DOMAIN,
            0,
        )?,
        Bytes::copy_from_slice(&native[..used]),
    )
    .map_err(|error| error.with_resource_id(descriptor.id().clone()))
}

fn fourcc(format: CameraPixelFormat) -> FourCC {
    FourCC::new(match format {
        CameraPixelFormat::Nv12 => b"NV12",
        CameraPixelFormat::Yuyv => b"YUYV",
        CameraPixelFormat::Mjpeg => b"MJPG",
    })
}

fn ensure_exact_format(
    requested: &CameraFormat,
    actual: &V4lFormat,
    descriptor: &ResourceDescriptor,
) -> HalResult<()> {
    if actual.width != requested.width()
        || actual.height != requested.height()
        || actual.fourcc != fourcc(requested.pixel_format())
        || usize::try_from(actual.size).unwrap_or(usize::MAX) > MAX_CAMERA_FRAME_BYTES
    {
        return Err(format_unsupported("camera.open", descriptor));
    }
    match requested.pixel_format() {
        CameraPixelFormat::Nv12 if actual.stride < requested.width() => {
            Err(format_unsupported("camera.open", descriptor))
        }
        CameraPixelFormat::Yuyv
            if actual.stride
                < requested
                    .width()
                    .checked_mul(2)
                    .ok_or_else(|| format_unsupported("camera.open", descriptor))? =>
        {
            Err(format_unsupported("camera.open", descriptor))
        }
        CameraPixelFormat::Mjpeg if actual.size == 0 => {
            Err(format_unsupported("camera.open", descriptor))
        }
        _ => Ok(()),
    }
}

fn validate_payload_layout(
    format: &CameraFormat,
    stride: usize,
    native: &[u8],
    used: usize,
    descriptor: &ResourceDescriptor,
) -> HalResult<()> {
    let width = format.width() as usize;
    let height = format.height() as usize;
    match format.pixel_format() {
        CameraPixelFormat::Nv12 => {
            let minimum = stride
                .checked_mul(height)
                .and_then(|y| y.checked_add(stride.checked_mul(height.div_ceil(2))?))
                .ok_or_else(|| invalid("camera.capture", "NV12 layout arithmetic overflows"))?;
            if stride < width || used < minimum || native.len() < minimum {
                return Err(invalid(
                    "camera.capture",
                    "V4L2 NV12 payload is shorter than its format",
                )
                .with_resource_id(descriptor.id().clone()));
            }
            Ok(())
        }
        CameraPixelFormat::Yuyv => {
            let minimum_stride = width
                .checked_mul(2)
                .ok_or_else(|| invalid("camera.capture", "YUYV stride overflows"))?;
            let minimum = stride
                .checked_mul(height)
                .ok_or_else(|| invalid("camera.capture", "YUYV layout arithmetic overflows"))?;
            if stride < minimum_stride || used < minimum || native.len() < minimum {
                return Err(invalid(
                    "camera.capture",
                    "V4L2 YUYV payload is shorter than its format",
                )
                .with_resource_id(descriptor.id().clone()));
            }
            Ok(())
        }
        CameraPixelFormat::Mjpeg => Ok(()),
    }
}

fn plane_layout(
    format: &CameraFormat,
    stride: usize,
    payload_length: usize,
    descriptor: &ResourceDescriptor,
) -> HalResult<Vec<CameraPlaneLayout>> {
    let height = format.height() as usize;
    match format.pixel_format() {
        CameraPixelFormat::Nv12 => {
            let y_length = stride
                .checked_mul(height)
                .ok_or_else(|| invalid("camera.capture", "V4L2 NV12 Y layout overflows"))?;
            let uv_length = stride
                .checked_mul(height.div_ceil(2))
                .ok_or_else(|| invalid("camera.capture", "V4L2 NV12 UV layout overflows"))?;
            let end = y_length
                .checked_add(uv_length)
                .ok_or_else(|| invalid("camera.capture", "V4L2 NV12 layout overflows"))?;
            if end > payload_length {
                return Err(invalid("camera.capture", "V4L2 NV12 planes exceed payload")
                    .with_resource_id(descriptor.id().clone()));
            }
            Ok(vec![
                CameraPlaneLayout::new(0, y_length, stride)?,
                CameraPlaneLayout::new(y_length, uv_length, stride)?,
            ])
        }
        CameraPixelFormat::Yuyv | CameraPixelFormat::Mjpeg => {
            Ok(vec![CameraPlaneLayout::new(0, payload_length, stride)?])
        }
    }
}

struct V4l2Session {
    descriptor: ResourceDescriptor,
    format: CameraFormat,
    sender: Option<mpsc::SyncSender<Command>>,
    worker: Option<thread::JoinHandle<()>>,
    shutdown: Arc<AtomicBool>,
    claims: Arc<Mutex<BTreeSet<seeed_hal_core::ResourceId>>>,
    closed: bool,
}

#[async_trait]
impl CameraCaptureSession for V4l2Session {
    fn descriptor(&self) -> &ResourceDescriptor {
        &self.descriptor
    }

    fn format(&self) -> &CameraFormat {
        &self.format
    }

    async fn capture(&mut self, timeout: Duration) -> HalResult<CameraFrame> {
        ensure_open(self.closed, "camera.capture", &self.descriptor)?;
        let (response_sender, response_receiver) = oneshot::channel();
        let deadline = Instant::now()
            .checked_add(timeout)
            .unwrap_or_else(Instant::now);
        self.sender
            .as_ref()
            .ok_or_else(|| closed("camera.capture", &self.descriptor))?
            .try_send(Command::Capture {
                deadline,
                response: response_sender,
            })
            .map_err(|_| closed("camera.capture", &self.descriptor))?;
        tokio::time::timeout(timeout, response_receiver)
            .await
            .map_err(|_| timeout_error("camera.capture", &self.descriptor))?
            .map_err(|_| closed("camera.capture", &self.descriptor))?
    }

    async fn controls(&mut self) -> HalResult<Vec<CameraControlDescriptor>> {
        ensure_open(self.closed, "camera.controls", &self.descriptor)?;
        Err(control_unsupported("camera.controls", &self.descriptor))
    }

    async fn get_control(&mut self, _kind: CameraControlKind) -> HalResult<CameraControlValue> {
        ensure_open(self.closed, "camera.control.get", &self.descriptor)?;
        Err(control_unsupported("camera.control.get", &self.descriptor))
    }

    async fn set_control(
        &mut self,
        _kind: CameraControlKind,
        _value: CameraControlValue,
    ) -> HalResult<()> {
        ensure_open(self.closed, "camera.control.set", &self.descriptor)?;
        Err(control_unsupported("camera.control.set", &self.descriptor))
    }

    async fn set_auto(&mut self, _kind: CameraControlKind, _enabled: bool) -> HalResult<()> {
        ensure_open(self.closed, "camera.control.auto", &self.descriptor)?;
        Err(control_unsupported("camera.control.auto", &self.descriptor))
    }

    async fn close(&mut self) -> HalResult<()> {
        if self.closed {
            return Ok(());
        }
        self.closed = true;
        self.shutdown.store(true, Ordering::Release);
        if let Some(sender) = self.sender.take() {
            let _ = sender.try_send(Command::Close);
        }
        if let Some(worker) = self.worker.take() {
            tokio::time::timeout(
                V4L2_CLOSE_TIMEOUT,
                tokio::task::spawn_blocking(move || worker.join()),
            )
            .await
            .map_err(|_| timeout_error("camera.close", &self.descriptor))?
            .map_err(|error| worker_failed("camera.close", error))?
            .map_err(|_| platform_error("camera.close", "V4L2 capture worker panicked"))?;
        }
        self.claims
            .lock()
            .expect("V4L2 claim mutex poisoned")
            .remove(self.descriptor.id());
        Ok(())
    }
}

impl Drop for V4l2Session {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::Release);
        if let Some(sender) = self.sender.take() {
            let _ = sender.try_send(Command::Close);
        }
        self.claims
            .lock()
            .expect("V4L2 claim mutex poisoned")
            .remove(self.descriptor.id());
    }
}

fn ensure_open(
    closed_session: bool,
    operation: &'static str,
    descriptor: &ResourceDescriptor,
) -> HalResult<()> {
    if closed_session {
        Err(closed(operation, descriptor))
    } else {
        Ok(())
    }
}

fn invalid(operation: &'static str, message: &'static str) -> HalError {
    HalError::new(
        "camera.frame.invalid",
        ErrorCategory::InvalidArgument,
        operation,
        false,
        message,
    )
    .expect("static V4L2 invalid metadata is valid")
}

fn closed(operation: &'static str, descriptor: &ResourceDescriptor) -> HalError {
    HalError::new(
        "runtime.session.closed",
        ErrorCategory::Conflict,
        operation,
        false,
        "V4L2 camera session is closed",
    )
    .expect("static V4L2 closed metadata is valid")
    .with_resource_id(descriptor.id().clone())
}

fn adapter_conflict(operation: &'static str, descriptor: &ResourceDescriptor) -> HalError {
    HalError::new(
        "runtime.adapter.conflict",
        ErrorCategory::Conflict,
        operation,
        false,
        "V4L2 camera is already claimed by an active session",
    )
    .expect("static V4L2 conflict metadata is valid")
    .with_resource_id(descriptor.id().clone())
}

fn format_unsupported(operation: &'static str, descriptor: &ResourceDescriptor) -> HalError {
    HalError::new(
        "camera.format.unsupported",
        ErrorCategory::InvalidArgument,
        operation,
        false,
        "V4L2 did not negotiate the requested exact camera format",
    )
    .expect("static V4L2 format metadata is valid")
    .with_resource_id(descriptor.id().clone())
}

fn control_unsupported(operation: &'static str, descriptor: &ResourceDescriptor) -> HalError {
    HalError::new(
        "camera.control.unsupported",
        ErrorCategory::InvalidArgument,
        operation,
        false,
        "V4L2 camera controls are not advertised by this adapter slice",
    )
    .expect("static V4L2 control metadata is valid")
    .with_resource_id(descriptor.id().clone())
}

fn timeout_error(operation: &'static str, descriptor: &ResourceDescriptor) -> HalError {
    HalError::new(
        "runtime.transport.timeout",
        ErrorCategory::Unavailable,
        operation,
        true,
        "timed out waiting for a V4L2 video frame",
    )
    .expect("static V4L2 timeout metadata is valid")
    .with_resource_id(descriptor.id().clone())
}

fn platform_error(operation: &'static str, error: impl std::fmt::Display) -> HalError {
    HalError::new(
        "runtime.transport.unavailable",
        ErrorCategory::Unavailable,
        operation,
        true,
        format!("V4L2 error: {error}"),
    )
    .expect("static V4L2 platform metadata is valid")
}

fn worker_failed(operation: &'static str, error: tokio::task::JoinError) -> HalError {
    HalError::new(
        "runtime.internal.worker_failed",
        ErrorCategory::Internal,
        operation,
        false,
        format!("V4L2 capture worker failed: {error}"),
    )
    .expect("static V4L2 worker metadata is valid")
}

#[cfg(test)]
mod tests {
    use super::identity_from_serial_or_sysfs_path;
    use seeed_hal_core::IdentityQuality;

    #[test]
    fn serial_identity_is_strong_and_distinct_from_transient_endpoint() {
        let evidence = identity_from_serial_or_sysfs_path(
            "ACME-CAM-42",
            "/devices/usb1/1-1/video4linux/video0",
        )
        .expect("serial evidence produces identity");
        assert_eq!(evidence.value, "serial:ACME-CAM-42");
        assert_eq!(evidence.source, "sysfs.serial");
        assert_eq!(evidence.quality, IdentityQuality::Strong);
    }

    #[test]
    fn sysfs_unique_path_evidence_is_medium_without_serial() {
        let evidence =
            identity_from_serial_or_sysfs_path("", "/devices/virtual/video4linux/video0")
                .expect("canonical sysfs path produces identity");
        assert_eq!(evidence.value, "sysfs:/devices/virtual/video4linux/video0");
        assert_eq!(evidence.source, "sysfs.canonical_device_path");
        assert_eq!(evidence.quality, IdentityQuality::Medium);
    }

    #[test]
    fn empty_identity_evidence_fails_closed() {
        assert!(identity_from_serial_or_sysfs_path("", "").is_none());
    }
}
