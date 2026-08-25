#[cfg(not(target_os = "macos"))]
use seeed_hal_camera::{CameraCaptureSession, CameraRequest};
#[cfg(not(target_os = "macos"))]
use seeed_hal_core::{HalResult, ResourceDescriptor, ResourceSelector};

#[cfg(not(target_os = "macos"))]
pub(super) fn enumerate_sync() -> HalResult<Vec<ResourceDescriptor>> {
    unreachable!("only compiled for macOS")
}

#[cfg(not(target_os = "macos"))]
pub(super) fn open_sync(
    _selector: &ResourceSelector,
    _request: &CameraRequest,
) -> HalResult<Box<dyn CameraCaptureSession>> {
    unreachable!("only compiled for macOS")
}

#[cfg(target_os = "macos")]
mod macos {
    use async_trait::async_trait;
    use block2::RcBlock;
    use core::ptr::NonNull;
    use dispatch2::{DispatchQueue, DispatchQueueAttr, DispatchRetained};
    use objc2::{
        AnyThread, DefinedClass, define_class,
        rc::{Retained, autoreleasepool},
        runtime::{AnyObject, Bool, NSObject, NSObjectProtocol, ProtocolObject},
    };
    use objc2_av_foundation::{
        AVAuthorizationStatus, AVCaptureConnection, AVCaptureDevice, AVCaptureDeviceFormat,
        AVCaptureDeviceInput, AVCaptureDeviceWasDisconnectedNotification, AVCaptureOutput,
        AVCaptureSession, AVCaptureSessionErrorKey, AVCaptureSessionRuntimeErrorNotification,
        AVCaptureVideoDataOutput, AVCaptureVideoDataOutputSampleBufferDelegate, AVError,
        AVMediaType, AVMediaTypeVideo,
    };
    use objc2_core_media::{
        CMSampleBuffer, CMVideoDimensions, CMVideoFormatDescriptionGetDimensions,
    };
    use objc2_core_video::{
        CVPixelBuffer, CVPixelBufferGetBaseAddress, CVPixelBufferGetBaseAddressOfPlane,
        CVPixelBufferGetBytesPerRow, CVPixelBufferGetBytesPerRowOfPlane, CVPixelBufferGetDataSize,
        CVPixelBufferGetHeight, CVPixelBufferGetHeightOfPlane, CVPixelBufferGetPixelFormatType,
        CVPixelBufferGetPlaneCount, CVPixelBufferGetWidth, CVPixelBufferLockBaseAddress,
        CVPixelBufferLockFlags, CVPixelBufferUnlockBaseAddress,
        kCVPixelFormatType_420YpCbCr8BiPlanarFullRange,
        kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange, kCVPixelFormatType_422YpCbCr8_yuvs,
    };
    use objc2_foundation::{
        NSDictionary, NSError, NSNotification, NSNotificationCenter, NSOperationQueue, NSString,
    };
    use seeed_hal_camera::{
        CameraCaptureSession, CameraControlDescriptor, CameraControlKind, CameraControlValue,
        CameraFormat, CameraFrame, CameraFrameMetadata, CameraFrameSink, CameraPixelFormat,
        CameraPlaneLayout, CameraRequest, camera_capture_capability, camera_frames_shm_capability,
    };
    use seeed_hal_core::{
        CapabilitySet, Endpoint, ErrorCategory, HalError, HalResult, IdentityQuality,
        ResourceDescriptor, ResourceProperties, ResourceSelector, TransportKind, resolve_resource,
    };
    use std::{
        collections::BTreeMap,
        sync::{Arc, Mutex, mpsc},
        time::Duration,
    };
    use tokio::sync::oneshot;

    use super::super::{
        encode_resource_id, quarantine_claim_until_worker_exits, release_claim_after_drop,
    };

    const CLOCK_DOMAIN: &str = "avfoundation";
    const AVFOUNDATION_CLOSE_TIMEOUT: Duration = Duration::from_secs(2);

    pub fn enumerate_sync() -> HalResult<Vec<ResourceDescriptor>> {
        autoreleasepool(|_| {
            let media_type = video_media_type("camera.enumerate")?;
            // SAFETY: `devicesWithMediaType:` is the generated objc2 binding;
            // `media_type` is AVFoundation's checked non-null video constant.
            #[allow(
                deprecated,
                reason = "discovery-session bindings are unavailable in objc2 0.3.1"
            )]
            let devices = unsafe { AVCaptureDevice::devicesWithMediaType(media_type) };
            devices
                .to_vec()
                .into_iter()
                .map(|device| descriptor_from_device(&device))
                .collect()
        })
    }

    fn descriptor_from_device(device: &AVCaptureDevice) -> HalResult<ResourceDescriptor> {
        // SAFETY: These generated getters operate on an enumerated retained
        // AVCaptureDevice. Values are copied into owned descriptor fields.
        let unique_id = unsafe { device.uniqueID() }.to_string();
        let endpoint = format!("avfoundation://device/{unique_id}");
        let mut properties = BTreeMap::new();
        properties.insert("adapter".to_owned(), "avfoundation".to_owned());
        properties.insert("endpoint".to_owned(), endpoint.clone());
        properties.insert("camera.unique_id".to_owned(), unique_id);
        // SAFETY: See the unique-ID getter safety rationale above.
        properties.insert(
            "camera.name".to_owned(),
            unsafe { device.localizedName() }.to_string(),
        );
        // SAFETY: See the unique-ID getter safety rationale above.
        properties.insert(
            "camera.model_id".to_owned(),
            unsafe { device.modelID() }.to_string(),
        );
        Ok(ResourceDescriptor::new(
            encode_resource_id(properties["camera.unique_id"].as_str())?,
            Endpoint::new(endpoint)?,
            IdentityQuality::Strong,
            TransportKind::Camera,
            ResourceProperties::new(properties),
            CapabilitySet::new(vec![
                camera_capture_capability(),
                camera_frames_shm_capability(),
            ]),
        ))
    }

    pub fn open_sync(
        selector: &ResourceSelector,
        request: &CameraRequest,
        claims: Arc<Mutex<std::collections::BTreeSet<seeed_hal_core::ResourceId>>>,
    ) -> HalResult<Box<dyn CameraCaptureSession>> {
        request.format().validate()?;
        autoreleasepool(|_| {
            let descriptors = enumerate_sync()?;
            let descriptor = resolve_resource(
                &descriptors,
                selector,
                &camera_capture_capability(),
                "camera.open",
            )?
            .clone();
            let unique_id = descriptor
                .properties()
                .get("camera.unique_id")
                .ok_or_else(|| platform_error("camera.open", "camera identity is missing"))?;
            {
                let mut claimed = claims.lock().expect("AVFoundation claim mutex poisoned");
                if !claimed.insert(descriptor.id().clone()) {
                    return Err(adapter_conflict("camera.open", &descriptor));
                }
            }
            match configure_session(
                descriptor.clone(),
                request.format().clone(),
                unique_id.to_owned(),
                Arc::clone(&claims),
            ) {
                Ok(session) => Ok(session),
                Err(error) => {
                    let mut claimed = claims.lock().expect("AVFoundation claim mutex poisoned");
                    claimed.remove(descriptor.id());
                    Err(error)
                }
            }
        })
    }

    fn ensure_active_format(
        device: &AVCaptureDevice,
        requested: &CameraFormat,
        descriptor: &ResourceDescriptor,
    ) -> HalResult<()> {
        let matching = find_matching_device_format(device, requested, descriptor)?;
        // SAFETY: The selected retained device is connected and authorized; lock
        // configuration before changing its active format.
        unsafe {
            device
                .lockForConfiguration()
                .map_err(|error| platform_error("camera.open", error.to_string()))?;
            device.setActiveFormat(&matching);
            device.unlockForConfiguration();
        }
        Ok(())
    }

    fn find_matching_device_format(
        device: &AVCaptureDevice,
        requested: &CameraFormat,
        descriptor: &ResourceDescriptor,
    ) -> HalResult<Retained<AVCaptureDeviceFormat>> {
        // SAFETY: Generated getter returns retained formats for this device.
        let formats = unsafe { device.formats() };
        for format in formats {
            // SAFETY: Generated getter reads the retained enumerated format.
            let description = unsafe { format.formatDescription() };
            // SAFETY: CoreMedia reads the immutable format description above.
            let dimensions = unsafe { CMVideoFormatDescriptionGetDimensions(&description) };
            // SAFETY: CoreMedia reads the immutable format description above.
            let subtype = unsafe { description.media_sub_type() };
            if format_description_matches(requested, subtype, dimensions) {
                return Ok(format);
            }
        }
        Err(format_unsupported("camera.open", descriptor))
    }

    fn format_description_matches(
        requested: &CameraFormat,
        subtype: u32,
        dimensions: CMVideoDimensions,
    ) -> bool {
        let matches_pixel_format = match requested.pixel_format() {
            CameraPixelFormat::Nv12 => {
                subtype == kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange
                    || subtype == kCVPixelFormatType_420YpCbCr8BiPlanarFullRange
            }
            CameraPixelFormat::Yuyv => subtype == kCVPixelFormatType_422YpCbCr8_yuvs,
            CameraPixelFormat::Mjpeg => false,
        };
        matches_pixel_format
            && dimensions.width == i32::try_from(requested.width()).unwrap_or_default()
            && dimensions.height == i32::try_from(requested.height()).unwrap_or_default()
    }

    fn requested_pixel_format_type(format: &CameraFormat) -> HalResult<u32> {
        match format.pixel_format() {
            CameraPixelFormat::Nv12 => Ok(kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange),
            CameraPixelFormat::Yuyv => Ok(kCVPixelFormatType_422YpCbCr8_yuvs),
            CameraPixelFormat::Mjpeg => Err(HalError::new(
                "camera.format.unsupported",
                ErrorCategory::InvalidArgument,
                "camera.open",
                false,
                "AVFoundation did not deliver the requested verified camera format",
            )
            .expect("static AVFoundation format error metadata is valid")),
        }
    }

    fn configure_session(
        descriptor: ResourceDescriptor,
        format: CameraFormat,
        unique_id: String,
        claims: Arc<Mutex<std::collections::BTreeSet<seeed_hal_core::ResourceId>>>,
    ) -> HalResult<Box<dyn CameraCaptureSession>> {
        let (commands, command_rx) = mpsc::sync_channel(8);
        let (opened_tx, opened_rx) = mpsc::sync_channel(1);
        let thread_descriptor = descriptor.clone();
        let thread_format = format.clone();
        let worker = std::thread::Builder::new()
            .name("seeed-hal-avfoundation-capture".to_owned())
            .spawn(move || {
                native_capture_worker(
                    thread_descriptor,
                    thread_format,
                    unique_id,
                    command_rx,
                    opened_tx,
                );
            })
            .map_err(|error| platform_error("camera.open", error.to_string()))?;
        opened_rx
            .recv()
            .map_err(|_| platform_error("camera.open", "AVFoundation capture thread exited"))??;
        Ok(Box::new(AvFoundationSession {
            descriptor,
            format,
            commands: Some(commands),
            worker: Some(worker),
            claims,
            claim_quarantined: false,
            next_capture_id: 1,
            closed: false,
        }))
    }

    fn configure_output(
        output: &AVCaptureVideoDataOutput,
        format: &CameraFormat,
        descriptor: &ResourceDescriptor,
    ) -> HalResult<()> {
        if matches!(format.pixel_format(), CameraPixelFormat::Mjpeg) {
            return Err(format_unsupported("camera.open", descriptor));
        }
        let _ = requested_pixel_format_type(format)?;
        // After locking the active format, AVFoundation delivers the negotiated
        // device-native samples; every frame is still verified below.
        let settings = NSDictionary::<NSString, objc2::runtime::AnyObject>::new();
        // SAFETY: Empty settings request the locked active format from the device.
        unsafe { output.setVideoSettings(Some(&settings)) };
        Ok(())
    }

    struct CaptureState {
        pending: Option<PendingCapture>,
        dropped_count: u64,
        next_sequence: u64,
        first_frame_seen: bool,
        descriptor: ResourceDescriptor,
        format: CameraFormat,
        unplugged: bool,
        /// Consecutive capture deadlines after at least one published frame.
        /// macOS UVC often keeps phantom discovery entries after unplug while
        /// the stream is already dead; stall detection fail-closes the session.
        consecutive_stalls: u32,
    }

    struct PendingCapture {
        _id: u64,
        sink: Arc<dyn CameraFrameSink>,
        response: oneshot::Sender<HalResult<()>>,
        deadline: std::time::Instant,
    }

    define_class!(
        #[unsafe(super(NSObject))]
        #[ivars = Arc<Mutex<CaptureState>>]
        struct FrameDelegate;

        unsafe impl NSObjectProtocol for FrameDelegate {}

        unsafe impl AVCaptureVideoDataOutputSampleBufferDelegate for FrameDelegate {
            #[unsafe(method(captureOutput:didOutputSampleBuffer:fromConnection:))]
            fn capture_output_did_output_sample_buffer_from_connection(
                &self,
                _output: &AVCaptureOutput,
                sample_buffer: &CMSampleBuffer,
                _connection: &AVCaptureConnection,
            ) {
                let pending = {
                    let mut state = self
                        .ivars()
                        .lock()
                        .expect("AVFoundation capture mutex poisoned");
                    state.first_frame_seen = true;
                    state.pending.take()
                };
                if let Some(pending) = pending {
                    let result = publish_sample_buffer_into(
                        sample_buffer,
                        pending.sink.as_ref(),
                        self.ivars(),
                    );
                    if result.is_ok() {
                        let mut state = self
                            .ivars()
                            .lock()
                            .expect("AVFoundation capture mutex poisoned");
                        state.next_sequence = state.next_sequence.saturating_add(1);
                        state.consecutive_stalls = 0;
                    }
                    let _ = pending.response.send(result);
                }
            }

            #[unsafe(method(captureOutput:didDropSampleBuffer:fromConnection:))]
            fn capture_output_did_drop_sample_buffer_from_connection(
                &self,
                _output: &AVCaptureOutput,
                _sample_buffer: &CMSampleBuffer,
                _connection: &AVCaptureConnection,
            ) {
                let mut state = self.ivars().lock().expect("AVFoundation capture mutex poisoned");
                state.dropped_count = state.dropped_count.saturating_add(1);
            }
        }
    );

    impl FrameDelegate {
        fn new(state: Arc<Mutex<CaptureState>>) -> Retained<Self> {
            // SAFETY: objc2's documented define_class pattern initializes the
            // declared ivar before NSObject `init`.
            unsafe { objc2::msg_send![super(Self::alloc().set_ivars(state)), init] }
        }
    }

    fn publish_sample_buffer_into(
        sample_buffer: &CMSampleBuffer,
        sink: &dyn CameraFrameSink,
        state: &Arc<Mutex<CaptureState>>,
    ) -> HalResult<()> {
        // SAFETY: The delegate owns the callback's valid sample buffer. objc2
        // retains the returned image buffer until this function returns.
        let image = unsafe { sample_buffer.image_buffer() }
            .ok_or_else(|| platform_error("camera.capture", "sample buffer has no image buffer"))?;
        let pixel_buffer: &CVPixelBuffer = &image;
        let flags = CVPixelBufferLockFlags::ReadOnly;
        // SAFETY: The pixel buffer is valid for this callback scope and only
        // read access is requested.
        if unsafe { CVPixelBufferLockBaseAddress(pixel_buffer, flags) } != 0 {
            return Err(platform_error(
                "camera.capture",
                "could not lock AVFoundation pixel buffer",
            ));
        }
        let result = publish_locked_pixel_buffer(pixel_buffer, sample_buffer, sink, state);
        // SAFETY: This exactly matches the successful read-only lock above.
        let _ = unsafe { CVPixelBufferUnlockBaseAddress(pixel_buffer, flags) };
        result
    }

    fn publish_locked_pixel_buffer(
        pixel_buffer: &CVPixelBuffer,
        sample_buffer: &CMSampleBuffer,
        sink: &dyn CameraFrameSink,
        state: &Arc<Mutex<CaptureState>>,
    ) -> HalResult<()> {
        // SAFETY: All CoreVideo queries operate on this read-locked buffer.
        let pixel_format = unsafe { CVPixelBufferGetPixelFormatType(pixel_buffer) };
        // SAFETY: See preceding CoreVideo query.
        let width = unsafe { CVPixelBufferGetWidth(pixel_buffer) };
        // SAFETY: See preceding CoreVideo query.
        let height = unsafe { CVPixelBufferGetHeight(pixel_buffer) };
        // SAFETY: See preceding CoreVideo query.
        let plane_count = unsafe { CVPixelBufferGetPlaneCount(pixel_buffer) };
        let mut planes = Vec::with_capacity(plane_count.max(1));
        let mut source_planes = Vec::with_capacity(plane_count.max(1));
        if plane_count == 0 {
            // SAFETY: The buffer remains read-locked.
            let length = unsafe { CVPixelBufferGetDataSize(pixel_buffer) };
            // SAFETY: The buffer remains read-locked.
            let base = unsafe { CVPixelBufferGetBaseAddress(pixel_buffer) }.cast::<u8>();
            if base.is_null() || length == 0 {
                return Err(platform_error(
                    "camera.capture",
                    "pixel buffer has no readable bytes",
                ));
            }
            // SAFETY: The buffer remains read-locked.
            let stride = unsafe { CVPixelBufferGetBytesPerRow(pixel_buffer) };
            planes.push(CameraPlaneLayout::new(0, length, stride)?);
            source_planes.push((base, length));
        } else {
            for index in 0..plane_count {
                // SAFETY: `index` is bounded by this buffer's plane count.
                let stride = unsafe { CVPixelBufferGetBytesPerRowOfPlane(pixel_buffer, index) };
                // SAFETY: `index` is bounded by this buffer's plane count.
                let rows = unsafe { CVPixelBufferGetHeightOfPlane(pixel_buffer, index) };
                let length = stride.checked_mul(rows).ok_or_else(|| {
                    platform_error("camera.capture", "pixel-buffer plane length overflow")
                })?;
                // SAFETY: `index` is bounded by this buffer's plane count.
                let base =
                    unsafe { CVPixelBufferGetBaseAddressOfPlane(pixel_buffer, index) }.cast::<u8>();
                if base.is_null() || length == 0 {
                    return Err(platform_error(
                        "camera.capture",
                        "pixel buffer plane has no readable bytes",
                    ));
                }
                let offset = source_planes.iter().map(|(_, length)| *length).sum();
                planes.push(CameraPlaneLayout::new(offset, length, stride)?);
                source_planes.push((base, length));
            }
        }
        // SAFETY: The valid callback buffer remains owned for this call.
        let seconds = unsafe { sample_buffer.presentation_time_stamp().seconds() };
        let timestamp_ns = if seconds.is_finite() && seconds >= 0.0 {
            (seconds * 1_000_000_000.0).min(u64::MAX as f64) as u64
        } else {
            0
        };
        let state = state.lock().expect("AVFoundation capture mutex poisoned");
        let actual_format = if (pixel_format == kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange
            || pixel_format == kCVPixelFormatType_420YpCbCr8BiPlanarFullRange)
            && planes.len() == 2
        {
            CameraPixelFormat::Nv12
        } else if pixel_format == kCVPixelFormatType_422YpCbCr8_yuvs && planes.len() == 1 {
            CameraPixelFormat::Yuyv
        } else {
            return Err(format_unsupported("camera.capture", &state.descriptor));
        };
        let width = u32::try_from(width)
            .map_err(|_| format_unsupported("camera.capture", &state.descriptor))?;
        let height = u32::try_from(height)
            .map_err(|_| format_unsupported("camera.capture", &state.descriptor))?;
        if actual_format != state.format.pixel_format()
            || width != state.format.width()
            || height != state.format.height()
        {
            return Err(format_unsupported("camera.capture", &state.descriptor));
        }
        let metadata = CameraFrameMetadata::new(
            state.format.clone(),
            planes,
            state.next_sequence,
            timestamp_ns,
            CLOCK_DOMAIN,
            state.dropped_count,
        )?;
        drop(state);
        sink.publish(metadata, &mut |destination| {
            let length = source_planes
                .iter()
                .map(|(_, length)| *length)
                .sum::<usize>();
            if length > destination.len() {
                return Err(platform_error(
                    "camera.capture",
                    "shared-memory slot is smaller than the locked pixel buffer",
                ));
            }
            let mut offset = 0;
            for (source, length) in &source_planes {
                // SAFETY: CoreVideo supplied each source extent while this function retains
                // the matching read-only pixel-buffer lock; destination is capacity-checked.
                unsafe {
                    std::ptr::copy_nonoverlapping(
                        *source,
                        destination.as_mut_ptr().add(offset),
                        *length,
                    );
                }
                offset += length;
            }
            Ok(length)
        })
    }

    struct NativeSession {
        session: Retained<AVCaptureSession>,
        input: Retained<AVCaptureDeviceInput>,
        output: Retained<AVCaptureVideoDataOutput>,
        _delegate: Retained<FrameDelegate>,
        _callback_queue: DispatchRetained<DispatchQueue>,
        /// Private queue so disconnect notifications deliver without a main CFRunLoop
        /// (Rust tests / broker hosts do not spin NSApplication).
        _notification_queue: Retained<NSOperationQueue>,
        _disconnect_observer: Retained<AnyObject>,
        _runtime_error_observer: Retained<AnyObject>,
    }

    fn native_capture_worker(
        descriptor: ResourceDescriptor,
        format: CameraFormat,
        unique_id: String,
        commands: mpsc::Receiver<Command>,
        opened: mpsc::SyncSender<HalResult<()>>,
    ) {
        let setup = autoreleasepool(|_| {
            let native_id = NSString::from_str(&unique_id);
            // SAFETY: The capture thread resolves only the selected immutable device identity.
            let device = unsafe { AVCaptureDevice::deviceWithUniqueID(&native_id) }
                .ok_or_else(|| platform_error("camera.open", "selected camera disappeared"))?;
            // SAFETY: Generated getters read the retained device selected above.
            if !unsafe { device.isConnected() }
                || unsafe { device.uniqueID() }.to_string() != unique_id
            {
                return Err(
                    platform_error("camera.open", "selected camera identity changed")
                        .with_resource_id(descriptor.id().clone()),
                );
            }
            let media_type = video_media_type("camera.open")?;
            ensure_camera_authorized(media_type)
                .map_err(|error| error.with_resource_id(descriptor.id().clone()))?;
            ensure_active_format(&device, &format, &descriptor)?;
            // SAFETY: Generated factory accepts the retained selected camera.
            let input = unsafe { AVCaptureDeviceInput::deviceInputWithDevice_error(&device) }
                .map_err(|error| platform_error("camera.open", error.to_string()))?;
            // SAFETY: Generated constructors have no caller-provided ABI values.
            let session = unsafe { AVCaptureSession::new() };
            // SAFETY: See the session constructor rationale.
            let output = unsafe { AVCaptureVideoDataOutput::new() };
            configure_output(&output, &format, &descriptor)?;
            let captures = Arc::new(Mutex::new(CaptureState {
                pending: None,
                dropped_count: 0,
                next_sequence: 1,
                first_frame_seen: false,
                descriptor: descriptor.clone(),
                format: format.clone(),
                unplugged: false,
                consecutive_stalls: 0,
            }));
            let delegate = FrameDelegate::new(Arc::clone(&captures));
            let callback_queue = DispatchQueue::new(
                "io.seeed.hal.avfoundation.capture",
                DispatchQueueAttr::SERIAL,
            );
            let protocol: &ProtocolObject<dyn AVCaptureVideoDataOutputSampleBufferDelegate> =
                ProtocolObject::from_ref(&*delegate);
            // SAFETY: The native thread owns graph setup and the serial callback queue.
            unsafe {
                output.setSampleBufferDelegate_queue(Some(protocol), Some(&callback_queue));
                session.beginConfiguration();
                if !session.canAddInput(&input) || !session.canAddOutput(&output) {
                    session.commitConfiguration();
                    return Err(platform_error(
                        "camera.open",
                        "AVFoundation cannot add selected camera input or video output",
                    )
                    .with_resource_id(descriptor.id().clone()));
                }
                session.addInput(&input);
                session.addOutput(&output);
                session.commitConfiguration();
                session.startRunning();
            }
            let notification_queue = notification_queue()?;
            let disconnect_observer = register_disconnect_observer(
                &captures,
                &descriptor,
                &unique_id,
                &notification_queue,
            )?;
            let runtime_error_observer = register_session_runtime_error_observer(
                &session,
                &captures,
                &descriptor,
                &notification_queue,
            )?;
            Ok((
                NativeSession {
                    session,
                    input,
                    output,
                    _delegate: delegate,
                    _callback_queue: callback_queue,
                    _notification_queue: notification_queue,
                    _disconnect_observer: disconnect_observer,
                    _runtime_error_observer: runtime_error_observer,
                },
                captures,
            ))
        });
        let Ok((native, captures)) = setup else {
            let _ = opened.send(setup.map(|_| ()));
            return;
        };
        // Frame-readiness probe: wait up to 2 s for the first sample to arrive.
        // macOS UVC sometimes enumerates phantom post-unplug entries that appear
        // connected but never produce frames. Detecting this here lets the caller
        // retry a different resource_id instead of spinning through capture timeouts.
        {
            let probe_deadline = std::time::Instant::now() + Duration::from_secs(2);
            let mut frame_seen = false;
            while std::time::Instant::now() < probe_deadline {
                std::thread::sleep(Duration::from_millis(40));
                let state = captures
                    .lock()
                    .expect("AVFoundation capture mutex poisoned");
                if state.first_frame_seen {
                    frame_seen = true;
                    break;
                }
                if state.unplugged {
                    break;
                }
            }
            if !frame_seen {
                let unplugged = captures
                    .lock()
                    .expect("AVFoundation capture mutex poisoned")
                    .unplugged;
                let err = if unplugged {
                    unplugged_error("camera.open", &descriptor)
                } else {
                    platform_error(
                        "camera.open",
                        "device produced no frames within readiness window; \
                         likely a phantom post-unplug enumeration entry",
                    )
                };
                let _ = teardown_native_session(native);
                let _ = opened.send(Err(err));
                return;
            }
        }
        let _ = opened.send(Ok(()));
        loop {
            match commands.recv_timeout(Duration::from_millis(50)) {
                Ok(command) => match command {
                    Command::Capture {
                        id,
                        timeout,
                        sink,
                        response,
                    } => {
                        if mark_unplugged_if_disconnected(&captures, &native, &descriptor) {
                            let _ =
                                response.send(Err(unplugged_error("camera.capture", &descriptor)));
                            continue;
                        }
                        {
                            let mut state = captures
                                .lock()
                                .expect("AVFoundation capture mutex poisoned");
                            if state.pending.is_some() {
                                let _ = response.send(Err(platform_error(
                                    "camera.capture",
                                    "a capture request is already pending",
                                )));
                                continue;
                            }
                            state.pending = Some(PendingCapture {
                                _id: id,
                                sink,
                                response,
                                deadline: std::time::Instant::now() + timeout,
                            });
                        }
                        if wait_for_capture_command(&commands, &captures, &native, &descriptor) {
                            return;
                        }
                    }
                    Command::Close { response } => {
                        cancel_pending_capture(
                            &captures,
                            closed_error("camera.capture", &descriptor),
                        );
                        let result = teardown_native_session(native);
                        let _ = response.send(result);
                        return;
                    }
                },
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    if mark_unplugged_if_disconnected(&captures, &native, &descriptor) {
                        cancel_pending_capture(
                            &captures,
                            unplugged_error("camera.capture", &descriptor),
                        );
                    }
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            }
        }
        cancel_pending_capture(&captures, closed_error("camera.capture", &descriptor));
        let _ = teardown_native_session(native);
    }

    /// Returns true after it tears down the graph in response to a close command.
    fn wait_for_capture_command(
        commands: &mpsc::Receiver<Command>,
        captures: &Arc<Mutex<CaptureState>>,
        native: &NativeSession,
        descriptor: &ResourceDescriptor,
    ) -> bool {
        loop {
            if mark_unplugged_if_disconnected(captures, native, descriptor) {
                cancel_pending_capture(captures, unplugged_error("camera.capture", descriptor));
                return false;
            }
            let deadline = captures
                .lock()
                .expect("AVFoundation capture mutex poisoned")
                .pending
                .as_ref()
                .map(|pending| pending.deadline);
            let Some(deadline) = deadline else {
                return false;
            };
            let now = std::time::Instant::now();
            if now >= deadline {
                cancel_pending_capture(captures, stall_or_timeout_error(captures, descriptor));
                return false;
            }
            match commands.recv_timeout(
                deadline
                    .saturating_duration_since(now)
                    .min(Duration::from_millis(10)),
            ) {
                Ok(Command::Close { response }) => {
                    cancel_pending_capture(captures, closed_error("camera.capture", descriptor));
                    let result = teardown_native_session_ref(native);
                    let _ = response.send(result);
                    return true;
                }
                Ok(Command::Capture {
                    id,
                    timeout,
                    sink,
                    response,
                }) => {
                    let mut state = captures
                        .lock()
                        .expect("AVFoundation capture mutex poisoned");
                    if state.pending.is_some() {
                        drop(state);
                        let _ = response.send(Err(platform_error(
                            "camera.capture",
                            "a capture request is already pending",
                        )));
                    } else {
                        state.pending = Some(PendingCapture {
                            _id: id,
                            sink,
                            response,
                            deadline: std::time::Instant::now() + timeout,
                        });
                    }
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    if std::time::Instant::now() >= deadline {
                        cancel_pending_capture(
                            captures,
                            stall_or_timeout_error(captures, descriptor),
                        );
                        return false;
                    }
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    cancel_pending_capture(captures, closed_error("camera.capture", descriptor));
                    let _ = teardown_native_session_ref(native);
                    return true;
                }
            }
        }
    }

    fn mark_session_unplugged(
        captures: &Arc<Mutex<CaptureState>>,
        descriptor: &ResourceDescriptor,
    ) {
        let mut state = captures
            .lock()
            .expect("AVFoundation capture mutex poisoned");
        if state.unplugged {
            return;
        }
        state.unplugged = true;
        drop(state);
        cancel_pending_capture(captures, unplugged_error("camera.capture", descriptor));
    }

    fn notification_queue() -> HalResult<Retained<NSOperationQueue>> {
        // SAFETY: Generated NSOperationQueue constructor has no caller ABI values.
        let queue = unsafe { NSOperationQueue::new() };
        // SAFETY: Name/concurrency setters take owned Foundation values.
        unsafe {
            queue.setName(Some(&NSString::from_str(
                "io.seeed.hal.avfoundation.notifications",
            )));
            queue.setMaxConcurrentOperationCount(1);
        }
        Ok(queue)
    }

    fn register_disconnect_observer(
        captures: &Arc<Mutex<CaptureState>>,
        descriptor: &ResourceDescriptor,
        unique_id: &str,
        queue: &NSOperationQueue,
    ) -> HalResult<Retained<AnyObject>> {
        let captures = Arc::clone(captures);
        let descriptor = descriptor.clone();
        let unique_id = unique_id.to_owned();
        let block = RcBlock::new(move |notification: NonNull<NSNotification>| {
            autoreleasepool(|_| {
                let notification = unsafe { notification.as_ref() };
                let Some(object) = (unsafe { notification.object() }) else {
                    return;
                };
                let Some(device) = object.downcast_ref::<AVCaptureDevice>() else {
                    return;
                };
                if unsafe { device.uniqueID() }.to_string() != unique_id {
                    return;
                }
                mark_session_unplugged(&captures, &descriptor);
            });
        });
        // SAFETY: A private NSOperationQueue delivers callbacks without requiring the
        // process main CFRunLoop / NSApplication that Rust tests do not run.
        Ok(unsafe {
            Retained::from(
                NSNotificationCenter::defaultCenter().addObserverForName_object_queue_usingBlock(
                    Some(AVCaptureDeviceWasDisconnectedNotification),
                    None,
                    Some(queue),
                    &block,
                ),
            )
        })
    }

    fn register_session_runtime_error_observer(
        session: &AVCaptureSession,
        captures: &Arc<Mutex<CaptureState>>,
        descriptor: &ResourceDescriptor,
        queue: &NSOperationQueue,
    ) -> HalResult<Retained<AnyObject>> {
        let captures = Arc::clone(captures);
        let descriptor = descriptor.clone();
        let block = RcBlock::new(move |notification: NonNull<NSNotification>| {
            autoreleasepool(|_| {
                let notification = unsafe { notification.as_ref() };
                let Some(user_info) = (unsafe { notification.userInfo() }) else {
                    return;
                };
                let Some(error_object) =
                    user_info.objectForKey(unsafe { &**AVCaptureSessionErrorKey })
                else {
                    return;
                };
                let Some(error) = error_object.downcast_ref::<NSError>() else {
                    return;
                };
                let code = error.code();
                if code == AVError::DeviceWasDisconnected.0 || code == AVError::DeviceNotConnected.0
                {
                    mark_session_unplugged(&captures, &descriptor);
                }
            });
        });
        // SAFETY: Runtime errors are scoped to the retained capture session object.
        Ok(unsafe {
            Retained::from(
                NSNotificationCenter::defaultCenter().addObserverForName_object_queue_usingBlock(
                    Some(AVCaptureSessionRuntimeErrorNotification),
                    Some(session),
                    Some(queue),
                    &block,
                ),
            )
        })
    }

    fn native_device_connected(native: &NativeSession, unique_id: &str) -> bool {
        // SAFETY: Capture thread owns the session for the lifetime of this poll.
        if !unsafe { native.session.isRunning() } {
            return false;
        }
        if !device_still_discoverable(unique_id) {
            return false;
        }
        let native_id = NSString::from_str(unique_id);
        // SAFETY: Resolve the selected immutable device identity outside the retained input.
        match unsafe { AVCaptureDevice::deviceWithUniqueID(&native_id) } {
            Some(device) => unsafe { device.isConnected() },
            None => false,
        }
    }

    fn device_still_discoverable(unique_id: &str) -> bool {
        autoreleasepool(|_| {
            let Ok(media_type) = video_media_type("camera.capture") else {
                return true;
            };
            // SAFETY: `devicesWithMediaType:` is AVFoundation's generated discovery API.
            #[allow(
                deprecated,
                reason = "discovery-session bindings are unavailable in objc2 0.3.1"
            )]
            let devices = unsafe { AVCaptureDevice::devicesWithMediaType(media_type) };
            devices.iter().any(|device| {
                // SAFETY: Getter reads an enumerated retained device identity.
                unsafe { device.uniqueID() }.to_string() == unique_id
            })
        })
    }

    #[allow(dead_code)]
    pub fn device_connectable_sync(unique_id: &str) -> bool {
        autoreleasepool(|_| {
            let native_id = NSString::from_str(unique_id);
            // SAFETY: Resolve the immutable device identity for a reconnect probe.
            match unsafe { AVCaptureDevice::deviceWithUniqueID(&native_id) } {
                Some(device) => unsafe { device.isConnected() },
                None => false,
            }
        })
    }

    fn mark_unplugged_if_disconnected(
        captures: &Arc<Mutex<CaptureState>>,
        native: &NativeSession,
        descriptor: &ResourceDescriptor,
    ) -> bool {
        let unique_id = descriptor
            .properties()
            .get("camera.unique_id")
            .unwrap_or("");
        let mut state = captures
            .lock()
            .expect("AVFoundation capture mutex poisoned");
        if camera_capture_wait_status(native_device_connected(native, unique_id), state.unplugged)
            == CaptureWaitStatus::Unplugged
        {
            state.unplugged = true;
            true
        } else {
            false
        }
    }

    fn cancel_pending_capture(captures: &Arc<Mutex<CaptureState>>, error: HalError) {
        let pending = captures
            .lock()
            .expect("AVFoundation capture mutex poisoned")
            .pending
            .take();
        if let Some(pending) = pending {
            let _ = pending.response.send(Err(error));
        }
    }

    fn teardown_native_session(native: NativeSession) -> HalResult<()> {
        teardown_native_session_ref(&native)
    }

    fn teardown_native_session_ref(native: &NativeSession) -> HalResult<()> {
        // SAFETY: Observers must be removed before graph teardown completes.
        unsafe {
            let center = NSNotificationCenter::defaultCenter();
            center.removeObserver(&native._disconnect_observer);
            center.removeObserver(&native._runtime_error_observer);
            native.output.setSampleBufferDelegate_queue(None, None);
            native.session.stopRunning();
            native.session.removeOutput(&native.output);
            native.session.removeInput(&native.input);
        }
        native._callback_queue.barrier_sync(|| {});
        Ok(())
    }

    enum Command {
        Capture {
            id: u64,
            timeout: Duration,
            sink: Arc<dyn CameraFrameSink>,
            response: oneshot::Sender<HalResult<()>>,
        },
        Close {
            response: oneshot::Sender<HalResult<()>>,
        },
    }

    struct AvFoundationSession {
        descriptor: ResourceDescriptor,
        format: CameraFormat,
        commands: Option<mpsc::SyncSender<Command>>,
        worker: Option<std::thread::JoinHandle<()>>,
        claims: Arc<Mutex<std::collections::BTreeSet<seeed_hal_core::ResourceId>>>,
        claim_quarantined: bool,
        next_capture_id: u64,
        closed: bool,
    }

    #[async_trait]
    impl CameraCaptureSession for AvFoundationSession {
        fn descriptor(&self) -> &ResourceDescriptor {
            &self.descriptor
        }

        fn format(&self) -> &CameraFormat {
            &self.format
        }

        async fn capture(&mut self, timeout: Duration) -> HalResult<CameraFrame> {
            let _ = timeout;
            Err(platform_error(
                "camera.capture",
                "AVFoundation frames are published only through the shared-memory capture sink",
            )
            .with_resource_id(self.descriptor.id().clone()))
        }

        async fn capture_into(
            &mut self,
            timeout: Duration,
            sink: Arc<dyn CameraFrameSink>,
        ) -> HalResult<()> {
            ensure_open(self.closed, "camera.capture", &self.descriptor)?;
            let (response_sender, response_receiver) = oneshot::channel();
            let id = self.next_capture_id;
            self.next_capture_id = self.next_capture_id.saturating_add(1);
            self.commands
                .as_ref()
                .ok_or_else(|| closed_error("camera.capture", &self.descriptor))?
                .try_send(Command::Capture {
                    id,
                    timeout,
                    sink,
                    response: response_sender,
                })
                .map_err(|_| closed_error("camera.capture", &self.descriptor))?;
            // The capture worker owns the deadline and may upgrade a stream stall
            // into `camera.session.unplugged`. Do not wrap with a racing Tokio
            // timeout that would drop that terminal error.
            response_receiver
                .await
                .map_err(|_| closed_error("camera.capture", &self.descriptor))?
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
            close_session(
                &mut self.commands,
                &mut self.worker,
                Arc::clone(&self.claims),
                self.descriptor.id().clone(),
                self.closed,
                &mut self.claim_quarantined,
            )
            .await?;
            self.closed = true;
            Ok(())
        }
    }

    impl Drop for AvFoundationSession {
        fn drop(&mut self) {
            if let Some(worker) = self.worker.take() {
                let claims = Arc::clone(&self.claims);
                let resource_id = self.descriptor.id().clone();
                if let Some(commands) = self.commands.take() {
                    let (response, _) = oneshot::channel();
                    let _ = commands.try_send(Command::Close { response });
                }
                std::thread::spawn(move || {
                    let _ = worker.join();
                    claims
                        .lock()
                        .expect("AVFoundation claim mutex poisoned")
                        .remove(&resource_id);
                });
            } else {
                release_claim_after_drop(
                    &self.claims,
                    self.descriptor.id(),
                    self.claim_quarantined,
                );
            }
        }
    }

    async fn close_session(
        commands: &mut Option<mpsc::SyncSender<Command>>,
        worker: &mut Option<std::thread::JoinHandle<()>>,
        claims: Arc<Mutex<std::collections::BTreeSet<seeed_hal_core::ResourceId>>>,
        resource_id: seeed_hal_core::ResourceId,
        already_closed: bool,
        claim_quarantined: &mut bool,
    ) -> HalResult<()> {
        if already_closed {
            return Ok(());
        }
        if let Some(commands) = commands.take() {
            let (response_sender, response_receiver) = oneshot::channel();
            let _ = commands.try_send(Command::Close {
                response: response_sender,
            });
            let _ = tokio::time::timeout(AVFOUNDATION_CLOSE_TIMEOUT, response_receiver).await;
        }
        if let Some(worker) = worker.take() {
            *claim_quarantined = true;
            let mut reaper = quarantine_claim_until_worker_exits(
                worker,
                Arc::clone(&claims),
                resource_id.clone(),
            );
            match tokio::time::timeout(AVFOUNDATION_CLOSE_TIMEOUT, &mut reaper).await {
                Ok(Ok(Ok(()))) => Ok(()),
                Ok(Ok(Err(_))) => Err(platform_error(
                    "camera.close",
                    "AVFoundation capture thread panicked during teardown",
                )),
                Ok(Err(error)) => Err(platform_error("camera.close", error.to_string())),
                Err(_) => Err(close_timeout_error(&resource_id)),
            }
        } else {
            release_claim_after_drop(&claims, &resource_id, *claim_quarantined);
            Ok(())
        }
    }

    fn video_media_type(operation: &'static str) -> HalResult<&'static AVMediaType> {
        // SAFETY: AVMediaTypeVideo is the immutable exported AVFoundation
        // singleton; optional absence becomes a fail-closed adapter error.
        unsafe { AVMediaTypeVideo }
            .ok_or_else(|| platform_error(operation, "AVMediaTypeVideo is absent"))
    }

    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    enum CameraAuthorizationAction {
        Continue,
        RequestAccess,
        Deny,
    }

    fn camera_authorization_action(status: AVAuthorizationStatus) -> CameraAuthorizationAction {
        match status {
            AVAuthorizationStatus::Authorized => CameraAuthorizationAction::Continue,
            AVAuthorizationStatus::NotDetermined => CameraAuthorizationAction::RequestAccess,
            _ => CameraAuthorizationAction::Deny,
        }
    }

    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    enum CaptureWaitStatus {
        Continue,
        Unplugged,
    }

    fn camera_capture_wait_status(connected: bool, already_unplugged: bool) -> CaptureWaitStatus {
        if already_unplugged || !connected {
            CaptureWaitStatus::Unplugged
        } else {
            CaptureWaitStatus::Continue
        }
    }

    /// After at least one published frame, this many consecutive capture
    /// deadlines without a frame treat the session as physically unplugged.
    const STREAM_STALL_UNPLUG_THRESHOLD: u32 = 3;

    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    enum CaptureStallStatus {
        Timeout,
        Unplugged,
    }

    fn camera_capture_stall_status(
        frames_published: bool,
        consecutive_stalls_after_increment: u32,
        already_unplugged: bool,
        disconnected: bool,
    ) -> CaptureStallStatus {
        if already_unplugged || disconnected {
            return CaptureStallStatus::Unplugged;
        }
        if frames_published && consecutive_stalls_after_increment >= STREAM_STALL_UNPLUG_THRESHOLD {
            CaptureStallStatus::Unplugged
        } else {
            CaptureStallStatus::Timeout
        }
    }

    fn stall_or_timeout_error(
        captures: &Arc<Mutex<CaptureState>>,
        descriptor: &ResourceDescriptor,
    ) -> HalError {
        let unique_id = descriptor
            .properties()
            .get("camera.unique_id")
            .unwrap_or("");
        let disconnected = !device_still_discoverable(unique_id) || {
            let native_id = NSString::from_str(unique_id);
            match unsafe { AVCaptureDevice::deviceWithUniqueID(&native_id) } {
                Some(device) => !unsafe { device.isConnected() },
                None => true,
            }
        };
        let mut state = captures
            .lock()
            .expect("AVFoundation capture mutex poisoned");
        let frames_published = state.first_frame_seen;
        let consecutive = state.consecutive_stalls.saturating_add(1);
        state.consecutive_stalls = consecutive;
        match camera_capture_stall_status(
            frames_published,
            consecutive,
            state.unplugged,
            disconnected,
        ) {
            CaptureStallStatus::Unplugged => {
                state.unplugged = true;
                drop(state);
                unplugged_error("camera.capture", descriptor)
            }
            CaptureStallStatus::Timeout => {
                drop(state);
                timeout_error("camera.capture", descriptor)
            }
        }
    }

    fn ensure_camera_authorized(media_type: &AVMediaType) -> HalResult<()> {
        // SAFETY: `media_type` is AVFoundation's checked video constant.
        let status = unsafe { AVCaptureDevice::authorizationStatusForMediaType(media_type) };
        match camera_authorization_action(status) {
            CameraAuthorizationAction::Continue => Ok(()),
            CameraAuthorizationAction::Deny => Err(permission_denied("camera.open")),
            CameraAuthorizationAction::RequestAccess => request_camera_access(media_type),
        }
    }

    fn request_camera_access(media_type: &AVMediaType) -> HalResult<()> {
        let (tx, rx) = mpsc::sync_channel(1);
        let handler = RcBlock::new(move |granted: Bool| {
            let _ = tx.send(granted.as_bool());
        });
        // SAFETY: `media_type` is AVMediaTypeVideo. The heap block matches the
        // generated `Fn(Bool)` completion signature and stays alive for this
        // call; AVFoundation copies the block before returning.
        unsafe {
            AVCaptureDevice::requestAccessForMediaType_completionHandler(media_type, &handler);
        }
        match rx.recv() {
            Ok(true) => Ok(()),
            _ => Err(permission_denied("camera.open")),
        }
    }

    fn ensure_open(
        closed: bool,
        operation: &'static str,
        descriptor: &ResourceDescriptor,
    ) -> HalResult<()> {
        if closed {
            Err(HalError::new(
                "runtime.session.closed",
                ErrorCategory::Conflict,
                operation,
                false,
                "AVFoundation camera session is closed",
            )?
            .with_resource_id(descriptor.id().clone()))
        } else {
            Ok(())
        }
    }

    fn format_unsupported(operation: &'static str, descriptor: &ResourceDescriptor) -> HalError {
        HalError::new(
            "camera.format.unsupported",
            ErrorCategory::InvalidArgument,
            operation,
            false,
            "AVFoundation did not deliver the requested verified camera format",
        )
        .expect("static AVFoundation format error metadata is valid")
        .with_resource_id(descriptor.id().clone())
    }

    fn control_unsupported(operation: &'static str, descriptor: &ResourceDescriptor) -> HalError {
        HalError::new(
            "camera.control.unsupported",
            ErrorCategory::Conflict,
            operation,
            false,
            "AVFoundation camera controls are not advertised by this adapter slice",
        )
        .expect("static AVFoundation control error metadata is valid")
        .with_resource_id(descriptor.id().clone())
    }

    fn timeout_error(operation: &'static str, descriptor: &ResourceDescriptor) -> HalError {
        HalError::new(
            "runtime.transport.timeout",
            ErrorCategory::Unavailable,
            operation,
            true,
            "timed out waiting for an AVFoundation video frame",
        )
        .expect("static AVFoundation timeout error metadata is valid")
        .with_resource_id(descriptor.id().clone())
    }

    fn unplugged_error(operation: &'static str, descriptor: &ResourceDescriptor) -> HalError {
        HalError::new(
            "camera.session.unplugged",
            ErrorCategory::Unavailable,
            operation,
            false,
            "camera was unplugged",
        )
        .expect("static AVFoundation unplugged error metadata is valid")
        .with_resource_id(descriptor.id().clone())
    }

    fn closed_error(operation: &'static str, descriptor: &ResourceDescriptor) -> HalError {
        HalError::new(
            "runtime.session.closed",
            ErrorCategory::Conflict,
            operation,
            false,
            "AVFoundation camera session is closed",
        )
        .expect("static AVFoundation closed error metadata is valid")
        .with_resource_id(descriptor.id().clone())
    }

    fn close_timeout_error(descriptor: &seeed_hal_core::ResourceId) -> HalError {
        HalError::new(
            "runtime.adapter.close_timeout",
            ErrorCategory::Unavailable,
            "camera.close",
            true,
            "AVFoundation capture thread did not finish native teardown before close timed out",
        )
        .expect("static AVFoundation close timeout metadata is valid")
        .with_resource_id(descriptor.clone())
    }

    fn permission_denied(operation: &'static str) -> HalError {
        HalError::new(
            "runtime.adapter.unavailable",
            ErrorCategory::Unavailable,
            operation,
            false,
            "camera permission is not authorized",
        )
        .expect("static AVFoundation permission error metadata is valid")
    }

    fn adapter_conflict(operation: &'static str, descriptor: &ResourceDescriptor) -> HalError {
        HalError::new(
            "runtime.adapter.conflict",
            ErrorCategory::Conflict,
            operation,
            true,
            "AVFoundation camera is already claimed by an active session",
        )
        .expect("static AVFoundation conflict error metadata is valid")
        .with_resource_id(descriptor.id().clone())
    }

    fn platform_error(operation: &'static str, message: impl Into<String>) -> HalError {
        HalError::new(
            "runtime.transport.unavailable",
            ErrorCategory::Unavailable,
            operation,
            true,
            format!("AVFoundation error: {}", message.into()),
        )
        .expect("static AVFoundation platform error metadata is valid")
    }

    #[cfg(test)]
    mod authorization_tests {
        use super::{
            AVAuthorizationStatus, CameraAuthorizationAction, camera_authorization_action,
        };

        #[test]
        fn authorized_status_continues_without_prompt() {
            assert_eq!(
                camera_authorization_action(AVAuthorizationStatus::Authorized),
                CameraAuthorizationAction::Continue
            );
        }

        #[test]
        fn not_determined_status_requests_access_once() {
            assert_eq!(
                camera_authorization_action(AVAuthorizationStatus::NotDetermined),
                CameraAuthorizationAction::RequestAccess
            );
        }

        #[test]
        fn denied_status_fails_closed_without_prompt() {
            assert_eq!(
                camera_authorization_action(AVAuthorizationStatus::Denied),
                CameraAuthorizationAction::Deny
            );
        }

        #[test]
        fn restricted_status_fails_closed_without_prompt() {
            assert_eq!(
                camera_authorization_action(AVAuthorizationStatus::Restricted),
                CameraAuthorizationAction::Deny
            );
        }
    }

    #[cfg(test)]
    mod disconnect_tests {
        use super::{
            CaptureStallStatus, CaptureWaitStatus, STREAM_STALL_UNPLUG_THRESHOLD,
            camera_capture_stall_status, camera_capture_wait_status,
        };

        #[test]
        fn connected_session_keeps_waiting_for_frames() {
            assert_eq!(
                camera_capture_wait_status(true, false),
                CaptureWaitStatus::Continue
            );
        }

        #[test]
        fn disconnected_device_is_unplugged() {
            assert_eq!(
                camera_capture_wait_status(false, false),
                CaptureWaitStatus::Unplugged
            );
        }

        #[test]
        fn already_unplugged_session_stays_unplugged() {
            assert_eq!(
                camera_capture_wait_status(true, true),
                CaptureWaitStatus::Unplugged
            );
        }

        #[test]
        fn stream_stall_before_first_frame_stays_timeout() {
            assert_eq!(
                camera_capture_stall_status(false, STREAM_STALL_UNPLUG_THRESHOLD, false, false),
                CaptureStallStatus::Timeout
            );
        }

        #[test]
        fn stream_stall_after_frames_becomes_unplugged() {
            assert_eq!(
                camera_capture_stall_status(true, STREAM_STALL_UNPLUG_THRESHOLD, false, false),
                CaptureStallStatus::Unplugged
            );
        }

        #[test]
        fn few_stalls_after_frames_stay_timeout() {
            assert_eq!(
                camera_capture_stall_status(
                    true,
                    STREAM_STALL_UNPLUG_THRESHOLD.saturating_sub(1),
                    false,
                    false
                ),
                CaptureStallStatus::Timeout
            );
        }
    }
}

#[cfg(not(target_os = "macos"))]
pub(super) fn device_connectable_sync(_unique_id: &str) -> bool {
    unreachable!("only compiled for macOS")
}

#[allow(unused_imports)]
pub(super) use macos::device_connectable_sync;
#[cfg(target_os = "macos")]
pub(super) use macos::{enumerate_sync, open_sync};
