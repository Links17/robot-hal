use async_trait::async_trait;
use bytes::Bytes;
use robot_hal_camera::{
    CameraCaptureSession, CameraControlDescriptor, CameraControlKind, CameraControlValue,
    CameraFormat, CameraFrame, CameraFrameMetadata, CameraPixelFormat, CameraPlaneLayout,
    CameraRequest, MAX_CAMERA_FRAME_BYTES, camera_capture_capability, camera_frames_shm_capability,
};
use robot_hal_core::{
    CapabilitySet, Endpoint, ErrorCategory, HalError, HalResult, IdentityQuality,
    ResourceDescriptor, ResourceProperties, ResourceSelector, TransportKind, resolve_resource,
};
use std::{
    collections::BTreeMap,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Receiver, SyncSender},
    },
    thread,
    time::{Duration, Instant},
};
use tokio::sync::oneshot;
use windows::{
    Win32::{
        Foundation::S_OK,
        Media::MediaFoundation::{
            IMFActivate, IMFMediaSource, IMFSample, IMFSourceReader, IMFSourceReaderCallback,
            MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE, MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE_VIDCAP_GUID,
            MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE_VIDCAP_SYMBOLIC_LINK, MF_MT_DEFAULT_STRIDE,
            MF_MT_FRAME_SIZE, MF_MT_MAJOR_TYPE, MF_MT_SUBTYPE, MF_SOURCE_READER_ASYNC_CALLBACK,
            MF_SOURCE_READER_FIRST_VIDEO_STREAM, MF_VERSION, MFCreateAttributes, MFCreateMediaType,
            MFCreateSourceReaderFromMediaSource, MFEnumDeviceSources, MFMediaType_Video,
            MFShutdown, MFStartup, MFVideoFormat_MJPG, MFVideoFormat_NV12, MFVideoFormat_YUY2,
        },
        System::Com::{COINIT_MULTITHREADED, CoInitializeEx, CoTaskMemFree, CoUninitialize},
    },
    core::{HRESULT, implement},
};

use super::{Claims, encode_resource_id, quarantine_claim_until_worker_exits};
use windows::Win32::Media::MediaFoundation::IMFSourceReaderCallback_Impl;

const CLOCK_DOMAIN: &str = "mediafoundation.receipt.monotonic";
const WAIT_QUANTUM: Duration = Duration::from_millis(20);
const CLOSE_TIMEOUT: Duration = Duration::from_secs(1);

pub(super) fn enumerate_sync() -> HalResult<Vec<ResourceDescriptor>> {
    let _runtime = MfRuntime::initialize("camera.enumerate")?;
    devices()?
        .into_iter()
        .map(|device| descriptor_from_device(&device))
        .collect()
}

pub(super) fn open_sync(
    selector: &ResourceSelector,
    request: &CameraRequest,
    claims: Claims,
) -> HalResult<Box<dyn CameraCaptureSession>> {
    request.format().validate()?;
    let devices = devices()?;
    let descriptors = devices
        .iter()
        .map(descriptor_from_device)
        .collect::<HalResult<Vec<_>>>()?;
    let descriptor = resolve_resource(
        &descriptors,
        selector,
        &camera_capture_capability(),
        "camera.open",
    )?
    .clone();
    let device = devices
        .into_iter()
        .find(|device| device.symbolic_link == descriptor.endpoint().as_str())
        .expect("resolved descriptor must originate from Media Foundation discovery");
    {
        let mut claimed = claims
            .lock()
            .expect("Media Foundation claim mutex poisoned");
        if claimed.contains(descriptor.id()) {
            return Err(conflict("camera.open", &descriptor));
        }
        claimed.insert(descriptor.id().clone());
    }
    match start_worker(
        device.symbolic_link,
        descriptor.clone(),
        request.format().clone(),
    ) {
        Ok((sender, worker, shutdown)) => Ok(Box::new(MediaFoundationSession {
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
                .expect("Media Foundation claim mutex poisoned")
                .remove(descriptor.id());
            Err(error)
        }
    }
}

struct Device {
    activate: IMFActivate,
    symbolic_link: String,
}

fn devices() -> HalResult<Vec<Device>> {
    let _runtime = MfRuntime::initialize("camera.enumerate")?;
    // SAFETY: MFCreateAttributes initializes the out pointer for the documented
    // Media Foundation attribute-store factory contract.
    let attributes = unsafe {
        let mut attributes = None;
        MFCreateAttributes(&mut attributes, 1)
            .map_err(|error| platform_error("camera.enumerate", error))?;
        attributes
            .ok_or_else(|| platform_error("camera.enumerate", "MFCreateAttributes returned null"))?
    };
    // SAFETY: SetGUID receives addresses of immutable Media Foundation GUID constants.
    unsafe {
        attributes
            .SetGUID(
                &MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE,
                &MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE_VIDCAP_GUID,
            )
            .map_err(|error| platform_error("camera.enumerate", error))?;
    }
    let mut raw = std::ptr::null_mut();
    let mut count = 0;
    // SAFETY: MFEnumDeviceSources allocates an array with CoTaskMemAlloc; this function
    // frees that exact array below after cloning its COM interface references.
    unsafe {
        MFEnumDeviceSources(&attributes, &mut raw, &mut count)
            .map_err(|error| platform_error("camera.enumerate", error))?;
    }
    let activations = if raw.is_null() {
        Vec::new()
    } else {
        // SAFETY: MFEnumDeviceSources returned `count` contiguous initialized entries.
        let values = unsafe { std::slice::from_raw_parts(raw, count as usize) }
            .iter()
            .filter_map(Clone::clone)
            .collect::<Vec<_>>();
        // SAFETY: `raw` is the allocation returned by MFEnumDeviceSources above.
        unsafe { CoTaskMemFree(Some(raw.cast())) };
        values
    };
    activations
        .into_iter()
        .map(|activate| {
            let symbolic_link = read_string(
                &activate,
                &MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE_VIDCAP_SYMBOLIC_LINK,
                "camera.enumerate",
            )?;
            if symbolic_link.is_empty() {
                return Err(invalid(
                    "camera.identity",
                    "Media Foundation camera symbolic link must not be empty",
                ));
            }
            Ok(Device {
                activate,
                symbolic_link,
            })
        })
        .collect()
}

fn descriptor_from_device(device: &Device) -> HalResult<ResourceDescriptor> {
    let mut properties = BTreeMap::new();
    properties.insert("adapter".to_owned(), "mediafoundation".to_owned());
    properties.insert("endpoint".to_owned(), device.symbolic_link.clone());
    properties.insert(
        "camera.identity_source".to_owned(),
        "mf.video_capture.symbolic_link".to_owned(),
    );
    Ok(ResourceDescriptor::new(
        encode_resource_id(&device.symbolic_link)?,
        Endpoint::new(device.symbolic_link.clone())?,
        // A video-capture symbolic link is the stable PnP-facing identity supplied by MF;
        // the ordinal from MFEnumDeviceSources is intentionally never used as identity.
        IdentityQuality::Medium,
        TransportKind::Camera,
        ResourceProperties::new(properties),
        CapabilitySet::new(vec![
            camera_capture_capability(),
            camera_frames_shm_capability(),
        ]),
    ))
}

fn read_string(
    attributes: &IMFActivate,
    key: &windows::core::GUID,
    operation: &'static str,
) -> HalResult<String> {
    // SAFETY: IMFAttributes returns the UTF-16 length for the requested documented key.
    let length = unsafe { attributes.GetStringLength(key) }
        .map_err(|error| platform_error(operation, error))?;
    let mut value = vec![0_u16; length as usize + 1];
    // SAFETY: `value` has length+1 writable UTF-16 units, as required by IMFAttributes::GetString.
    unsafe {
        attributes
            .GetString(key, &mut value, None)
            .map_err(|error| platform_error(operation, error))?;
    }
    Ok(String::from_utf16_lossy(&value[..length as usize]))
}

fn start_worker(
    symbolic_link: String,
    descriptor: ResourceDescriptor,
    format: CameraFormat,
) -> HalResult<(SyncSender<Command>, thread::JoinHandle<()>, Arc<AtomicBool>)> {
    let (sender, receiver) = mpsc::sync_channel(1);
    let (ready_sender, ready_receiver) = mpsc::sync_channel(1);
    let shutdown = Arc::new(AtomicBool::new(false));
    let worker_shutdown = Arc::clone(&shutdown);
    let worker = thread::Builder::new()
        .name("robot-hal-mediafoundation-capture".to_owned())
        .spawn(move || {
            worker(
                symbolic_link,
                descriptor,
                format,
                receiver,
                ready_sender,
                worker_shutdown,
            )
        })
        .map_err(|error| platform_error("camera.open", error))?;
    ready_receiver.recv().map_err(|_| {
        platform_error("camera.open", "Media Foundation worker exited during setup")
    })??;
    Ok((sender, worker, shutdown))
}

enum Command {
    Capture {
        deadline: Instant,
        response: oneshot::Sender<HalResult<CameraFrame>>,
    },
    Close,
}

enum SampleNotification {
    Sample(Option<IMFSample>),
    Error(HRESULT),
}

#[implement(IMFSourceReaderCallback)]
struct SourceReaderCallback {
    samples: SyncSender<SampleNotification>,
}

impl IMFSourceReaderCallback_Impl for SourceReaderCallback_Impl {
    fn OnReadSample(
        &self,
        status: HRESULT,
        _stream: u32,
        _flags: u32,
        _timestamp: i64,
        sample: Option<&IMFSample>,
    ) -> windows::core::Result<()> {
        let notification = if status == S_OK {
            SampleNotification::Sample(sample.cloned())
        } else {
            SampleNotification::Error(status)
        };
        let _ = self.samples.try_send(notification);
        Ok(())
    }

    fn OnFlush(&self, _stream: u32) -> windows::core::Result<()> {
        Ok(())
    }

    fn OnEvent(
        &self,
        _stream: u32,
        _event: Option<&windows::Win32::Media::MediaFoundation::IMFMediaEvent>,
    ) -> windows::core::Result<()> {
        Ok(())
    }
}

fn worker(
    symbolic_link: String,
    descriptor: ResourceDescriptor,
    format: CameraFormat,
    commands: Receiver<Command>,
    ready: SyncSender<HalResult<()>>,
    shutdown: Arc<AtomicBool>,
) {
    let runtime = match MfRuntime::initialize("camera.open") {
        Ok(runtime) => runtime,
        Err(error) => {
            let _ = ready.send(Err(error));
            return;
        }
    };
    let result = (|| -> HalResult<()> {
        let activate = devices()?
            .into_iter()
            .find(|device| device.symbolic_link == symbolic_link)
            .ok_or_else(|| platform_error("camera.open", "camera disappeared before activation"))?
            .activate;
        // SAFETY: IMFActivate::ActivateObject creates the documented IMFMediaSource
        // interface from the activation object returned by MFEnumDeviceSources.
        let source: IMFMediaSource = unsafe { activate.ActivateObject() }
            .map_err(|error| platform_error("camera.open", error))?;
        let (sample_sender, sample_receiver) = mpsc::sync_channel(1);
        let callback: IMFSourceReaderCallback = SourceReaderCallback {
            samples: sample_sender,
        }
        .into();
        // SAFETY: callback is retained by `callback` for the reader lifetime and implements
        // the documented asynchronous Source Reader callback interface.
        let reader_attributes = unsafe {
            let mut attributes = None;
            MFCreateAttributes(&mut attributes, 1)
                .map_err(|error| platform_error("camera.open", error))?;
            let attributes = attributes
                .ok_or_else(|| platform_error("camera.open", "MFCreateAttributes returned null"))?;
            attributes
                .SetUnknown(&MF_SOURCE_READER_ASYNC_CALLBACK, &callback)
                .map_err(|error| platform_error("camera.open", error))?;
            attributes
        };
        // SAFETY: reader attributes contain a valid async callback; recreating the reader is
        // required by Media Foundation to opt into non-blocking ReadSample completion.
        let reader = unsafe { MFCreateSourceReaderFromMediaSource(&source, &reader_attributes) }
            .map_err(|error| platform_error("camera.open", error))?;
        let negotiated = configure_exact(&reader, &format, &descriptor)?;
        ready
            .send(Ok(()))
            .map_err(|_| platform_error("camera.open", "caller abandoned worker startup"))?;
        let started = Instant::now();
        let mut sequence = 1_u64;
        while !shutdown.load(Ordering::Acquire) {
            let command = match commands.recv_timeout(WAIT_QUANTUM) {
                Ok(command) => command,
                Err(mpsc::RecvTimeoutError::Timeout) => continue,
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            };
            match command {
                Command::Close => break,
                Command::Capture { deadline, response } => {
                    let result = capture_async(
                        &reader,
                        &sample_receiver,
                        deadline,
                        &format,
                        negotiated.stride,
                        &descriptor,
                        sequence,
                        started,
                        &shutdown,
                    );
                    if result.is_ok() {
                        sequence = sequence.saturating_add(1);
                    }
                    let _ = response.send(result);
                }
            }
        }
        // SAFETY: Flush cancels outstanding asynchronous requests before reader/source COM
        // references are released, per IMFSourceReader::Flush contract.
        unsafe { reader.Flush(MF_SOURCE_READER_FIRST_VIDEO_STREAM.0 as u32) }
            .map_err(|error| platform_error("camera.close", error))?;
        Ok(())
    })();
    if let Err(error) = result {
        let _ = ready.send(Err(error));
    }
    drop(runtime);
}

struct Negotiated {
    stride: usize,
}

fn configure_exact(
    reader: &IMFSourceReader,
    format: &CameraFormat,
    descriptor: &ResourceDescriptor,
) -> HalResult<Negotiated> {
    // SAFETY: MFCreateMediaType initializes a new Media Foundation media type.
    let media_type =
        unsafe { MFCreateMediaType() }.map_err(|error| platform_error("camera.open", error))?;
    let subtype = mf_subtype(format.pixel_format());
    let frame_size = (u64::from(format.width()) << 32) | u64::from(format.height());
    // SAFETY: Media Foundation copies immutable GUID/value attributes into `media_type`.
    unsafe {
        media_type
            .SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)
            .and_then(|_| media_type.SetGUID(&MF_MT_SUBTYPE, &subtype))
            .and_then(|_| media_type.SetUINT64(&MF_MT_FRAME_SIZE, frame_size))
            .map_err(|_| format_unsupported("camera.open", descriptor))?;
        reader
            .SetCurrentMediaType(
                MF_SOURCE_READER_FIRST_VIDEO_STREAM.0 as u32,
                None,
                &media_type,
            )
            .map_err(|_| format_unsupported("camera.open", descriptor))?;
        let actual = reader
            .GetCurrentMediaType(MF_SOURCE_READER_FIRST_VIDEO_STREAM.0 as u32)
            .map_err(|_| format_unsupported("camera.open", descriptor))?;
        ensure_exact_media_type(&actual, format, descriptor)
    }
}

fn ensure_exact_media_type(
    actual: &windows::Win32::Media::MediaFoundation::IMFMediaType,
    requested: &CameraFormat,
    descriptor: &ResourceDescriptor,
) -> HalResult<Negotiated> {
    // SAFETY: getters read immutable Media Foundation media-type attributes.
    let (major, subtype, dimensions, stride) = unsafe {
        (
            actual.GetGUID(&MF_MT_MAJOR_TYPE),
            actual.GetGUID(&MF_MT_SUBTYPE),
            actual.GetUINT64(&MF_MT_FRAME_SIZE),
            actual.GetUINT32(&MF_MT_DEFAULT_STRIDE),
        )
    };
    let (Ok(major), Ok(subtype), Ok(dimensions), Ok(stride)) = (major, subtype, dimensions, stride)
    else {
        return Err(format_unsupported("camera.open", descriptor));
    };
    let width = (dimensions >> 32) as u32;
    let height = dimensions as u32;
    let stride =
        usize::try_from(stride).map_err(|_| format_unsupported("camera.open", descriptor))?;
    if major != MFMediaType_Video
        || subtype != mf_subtype(requested.pixel_format())
        || width != requested.width()
        || height != requested.height()
        || !valid_stride(requested, stride)
    {
        return Err(format_unsupported("camera.open", descriptor));
    }
    Ok(Negotiated { stride })
}

fn capture_async(
    reader: &IMFSourceReader,
    samples: &Receiver<SampleNotification>,
    deadline: Instant,
    format: &CameraFormat,
    stride: usize,
    descriptor: &ResourceDescriptor,
    sequence: u64,
    started: Instant,
    shutdown: &AtomicBool,
) -> HalResult<CameraFrame> {
    // SAFETY: the reader was created with MF_SOURCE_READER_ASYNC_CALLBACK, so ReadSample
    // schedules completion on the callback rather than synchronously blocking this worker.
    unsafe {
        reader
            .ReadSample(
                MF_SOURCE_READER_FIRST_VIDEO_STREAM.0 as u32,
                0,
                None,
                None,
                None,
                None,
            )
            .map_err(|error| {
                platform_error("camera.capture", error).with_resource_id(descriptor.id().clone())
            })?;
    }
    loop {
        if shutdown.load(Ordering::Acquire) {
            return Err(closed("camera.capture", descriptor));
        }
        let now = Instant::now();
        if now >= deadline {
            // SAFETY: Flush is the documented cancellation operation for outstanding
            // asynchronous Source Reader requests.
            unsafe { reader.Flush(MF_SOURCE_READER_FIRST_VIDEO_STREAM.0 as u32) }.map_err(
                |error| {
                    platform_error("camera.capture", error)
                        .with_resource_id(descriptor.id().clone())
                },
            )?;
            return Err(timeout_error("camera.capture", descriptor));
        }
        let wait = (deadline - now).min(WAIT_QUANTUM);
        match samples.recv_timeout(wait) {
            Ok(SampleNotification::Sample(sample)) => {
                let sample = sample.ok_or_else(|| {
                    platform_error(
                        "camera.capture",
                        "Media Foundation returned an empty sample",
                    )
                    .with_resource_id(descriptor.id().clone())
                })?;
                return frame_from_sample(sample, format, stride, descriptor, sequence, started);
            }
            Ok(SampleNotification::Error(error)) => {
                return Err(platform_error("camera.capture", error)
                    .with_resource_id(descriptor.id().clone()));
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                return Err(closed("camera.capture", descriptor));
            }
        }
    }
}

fn frame_from_sample(
    sample: IMFSample,
    format: &CameraFormat,
    stride: usize,
    descriptor: &ResourceDescriptor,
    sequence: u64,
    started: Instant,
) -> HalResult<CameraFrame> {
    // SAFETY: ConvertToContiguousBuffer returns a buffer owned by `sample`, live through copy.
    let buffer = unsafe { sample.ConvertToContiguousBuffer() }.map_err(|error| {
        platform_error("camera.capture", error).with_resource_id(descriptor.id().clone())
    })?;
    let mut pointer = std::ptr::null_mut();
    let mut maximum = 0;
    let mut used = 0;
    // SAFETY: Lock initializes the returned pointer and lengths, and the buffer remains live.
    unsafe {
        buffer
            .Lock(&mut pointer, Some(&mut maximum), Some(&mut used))
            .map_err(|error| {
                platform_error("camera.capture", error).with_resource_id(descriptor.id().clone())
            })?;
    }
    let result = (|| {
        let used = usize::try_from(used).map_err(|_| {
            invalid(
                "camera.capture",
                "Media Foundation sample size does not fit usize",
            )
        })?;
        let maximum = usize::try_from(maximum).map_err(|_| {
            invalid(
                "camera.capture",
                "Media Foundation buffer size does not fit usize",
            )
        })?;
        if pointer.is_null() || used == 0 || used > maximum || used > MAX_CAMERA_FRAME_BYTES {
            return Err(invalid(
                "camera.capture",
                "Media Foundation sample payload is outside the public bound",
            )
            .with_resource_id(descriptor.id().clone()));
        }
        validate_layout(format, stride, used, descriptor)?;
        // SAFETY: Lock returned `pointer` valid for `used` bytes until Unlock below.
        let payload = unsafe { std::slice::from_raw_parts(pointer, used) };
        let planes = planes(format, stride, used, descriptor)?;
        CameraFrame::new(
            CameraFrameMetadata::new(
                format.clone(),
                planes,
                sequence,
                u64::try_from(started.elapsed().as_nanos()).unwrap_or(u64::MAX),
                CLOCK_DOMAIN,
                0,
            )?,
            Bytes::copy_from_slice(payload),
        )
        .map_err(|error| error.with_resource_id(descriptor.id().clone()))
    })();
    // SAFETY: exactly one successful Lock above is paired with Unlock before buffer release.
    let unlock = unsafe { buffer.Unlock() }.map_err(|error| {
        platform_error("camera.capture", error).with_resource_id(descriptor.id().clone())
    });
    match (result, unlock) {
        (Ok(frame), Ok(())) => Ok(frame),
        (Err(error), _) | (_, Err(error)) => Err(error),
    }
}

fn mf_subtype(format: CameraPixelFormat) -> windows::core::GUID {
    match format {
        CameraPixelFormat::Nv12 => MFVideoFormat_NV12,
        CameraPixelFormat::Yuyv => MFVideoFormat_YUY2,
        CameraPixelFormat::Mjpeg => MFVideoFormat_MJPG,
    }
}

fn valid_stride(format: &CameraFormat, stride: usize) -> bool {
    match format.pixel_format() {
        CameraPixelFormat::Nv12 => stride >= format.width() as usize,
        CameraPixelFormat::Yuyv => stride >= format.width() as usize * 2,
        CameraPixelFormat::Mjpeg => stride > 0,
    }
}

fn validate_layout(
    format: &CameraFormat,
    stride: usize,
    used: usize,
    descriptor: &ResourceDescriptor,
) -> HalResult<()> {
    let height = format.height() as usize;
    let required = match format.pixel_format() {
        CameraPixelFormat::Nv12 => stride
            .checked_mul(height)
            .and_then(|y| y.checked_add(stride.checked_mul(height.div_ceil(2))?)),
        CameraPixelFormat::Yuyv => stride.checked_mul(height),
        CameraPixelFormat::Mjpeg => Some(1),
    }
    .ok_or_else(|| {
        invalid(
            "camera.capture",
            "Media Foundation plane arithmetic overflows",
        )
    })?;
    if !valid_stride(format, stride) || used < required {
        return Err(invalid(
            "camera.capture",
            "Media Foundation sample is shorter than negotiated layout",
        )
        .with_resource_id(descriptor.id().clone()));
    }
    Ok(())
}

fn planes(
    format: &CameraFormat,
    stride: usize,
    payload: usize,
    descriptor: &ResourceDescriptor,
) -> HalResult<Vec<CameraPlaneLayout>> {
    match format.pixel_format() {
        CameraPixelFormat::Nv12 => {
            let y = stride
                .checked_mul(format.height() as usize)
                .ok_or_else(|| {
                    invalid("camera.capture", "Media Foundation NV12 Y plane overflows")
                })?;
            let uv = stride
                .checked_mul((format.height() as usize).div_ceil(2))
                .ok_or_else(|| {
                    invalid("camera.capture", "Media Foundation NV12 UV plane overflows")
                })?;
            if y.checked_add(uv).is_none_or(|end| end > payload) {
                return Err(invalid(
                    "camera.capture",
                    "Media Foundation NV12 planes exceed payload",
                )
                .with_resource_id(descriptor.id().clone()));
            }
            Ok(vec![
                CameraPlaneLayout::new(0, y, stride)?,
                CameraPlaneLayout::new(y, uv, stride)?,
            ])
        }
        CameraPixelFormat::Yuyv | CameraPixelFormat::Mjpeg => {
            Ok(vec![CameraPlaneLayout::new(0, payload, stride)?])
        }
    }
}

struct MediaFoundationSession {
    descriptor: ResourceDescriptor,
    format: CameraFormat,
    sender: Option<SyncSender<Command>>,
    worker: Option<thread::JoinHandle<()>>,
    shutdown: Arc<AtomicBool>,
    claims: Claims,
    closed: bool,
}

#[async_trait]
impl CameraCaptureSession for MediaFoundationSession {
    fn descriptor(&self) -> &ResourceDescriptor {
        &self.descriptor
    }

    fn format(&self) -> &CameraFormat {
        &self.format
    }

    async fn capture(&mut self, timeout: Duration) -> HalResult<CameraFrame> {
        ensure_open(self.closed, "camera.capture", &self.descriptor)?;
        let (response, receiver) = oneshot::channel();
        let deadline = Instant::now()
            .checked_add(timeout)
            .unwrap_or_else(Instant::now);
        self.sender
            .as_ref()
            .ok_or_else(|| closed("camera.capture", &self.descriptor))?
            .try_send(Command::Capture { deadline, response })
            .map_err(|_| closed("camera.capture", &self.descriptor))?;
        tokio::time::timeout(timeout, receiver)
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
            let mut reaped = quarantine_claim_until_worker_exits(
                worker,
                Arc::clone(&self.claims),
                self.descriptor.id().clone(),
            );
            match tokio::time::timeout(CLOSE_TIMEOUT, &mut reaped).await {
                Ok(Ok(Ok(()))) => Ok(()),
                Ok(Ok(Err(_))) => Err(platform_error(
                    "camera.close",
                    "Media Foundation worker panicked",
                )),
                Ok(Err(_)) => Err(platform_error("camera.close", "quarantine reaper failed")),
                Err(_) => Err(close_timeout_error(&self.descriptor)),
            }?;
        }
        Ok(())
    }
}

impl Drop for MediaFoundationSession {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::Release);
        if let Some(sender) = self.sender.take() {
            let _ = sender.try_send(Command::Close);
        }
        if let Some(worker) = self.worker.take() {
            let _ = quarantine_claim_until_worker_exits(
                worker,
                Arc::clone(&self.claims),
                self.descriptor.id().clone(),
            );
        }
    }
}

struct MfRuntime;

impl MfRuntime {
    fn initialize(operation: &'static str) -> HalResult<Self> {
        // SAFETY: this worker/discovery thread owns its MTA COM initialization until Drop.
        unsafe { CoInitializeEx(None, COINIT_MULTITHREADED).ok() }
            .map_err(|error| platform_error(operation, error))?;
        // SAFETY: MFStartup/MFShutdown are paired by this RAII guard on the same thread.
        if let Err(error) = unsafe { MFStartup(MF_VERSION, 0) } {
            // SAFETY: this reverses the successful CoInitializeEx immediately above.
            unsafe { CoUninitialize() };
            return Err(platform_error(operation, error));
        }
        Ok(Self)
    }
}

impl Drop for MfRuntime {
    fn drop(&mut self) {
        // SAFETY: this balances the successful MFStartup and CoInitializeEx in initialize.
        unsafe {
            let _ = MFShutdown();
            CoUninitialize();
        }
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
    .expect("static Media Foundation invalid metadata is valid")
}

fn conflict(operation: &'static str, descriptor: &ResourceDescriptor) -> HalError {
    HalError::new(
        "runtime.adapter.conflict",
        ErrorCategory::Conflict,
        operation,
        false,
        "Media Foundation camera is already claimed by an active session",
    )
    .expect("static Media Foundation conflict metadata is valid")
    .with_resource_id(descriptor.id().clone())
}

fn format_unsupported(operation: &'static str, descriptor: &ResourceDescriptor) -> HalError {
    HalError::new(
        "camera.format.unsupported",
        ErrorCategory::InvalidArgument,
        operation,
        false,
        "Media Foundation did not negotiate the requested exact camera format",
    )
    .expect("static Media Foundation format metadata is valid")
    .with_resource_id(descriptor.id().clone())
}

fn control_unsupported(operation: &'static str, descriptor: &ResourceDescriptor) -> HalError {
    HalError::new(
        "camera.control.unsupported",
        ErrorCategory::InvalidArgument,
        operation,
        false,
        "Media Foundation camera controls are not advertised by this adapter slice",
    )
    .expect("static Media Foundation control metadata is valid")
    .with_resource_id(descriptor.id().clone())
}

fn timeout_error(operation: &'static str, descriptor: &ResourceDescriptor) -> HalError {
    HalError::new(
        "runtime.transport.timeout",
        ErrorCategory::Unavailable,
        operation,
        true,
        "timed out waiting for a Media Foundation video frame",
    )
    .expect("static Media Foundation timeout metadata is valid")
    .with_resource_id(descriptor.id().clone())
}

fn close_timeout_error(descriptor: &ResourceDescriptor) -> HalError {
    HalError::new(
        "runtime.adapter.close_timeout",
        ErrorCategory::Unavailable,
        "camera.close",
        true,
        "Media Foundation worker did not release the camera before close timed out",
    )
    .expect("static Media Foundation close timeout metadata is valid")
    .with_resource_id(descriptor.id().clone())
}

fn closed(operation: &'static str, descriptor: &ResourceDescriptor) -> HalError {
    HalError::new(
        "runtime.session.closed",
        ErrorCategory::Conflict,
        operation,
        false,
        "Media Foundation camera session is closed",
    )
    .expect("static Media Foundation closed metadata is valid")
    .with_resource_id(descriptor.id().clone())
}

fn platform_error(operation: &'static str, error: impl std::fmt::Display) -> HalError {
    HalError::new(
        "runtime.transport.unavailable",
        ErrorCategory::Unavailable,
        operation,
        true,
        format!("Media Foundation error: {error}"),
    )
    .expect("static Media Foundation platform metadata is valid")
}
