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
    use bytes::Bytes;
    use dispatch2::{DispatchQueue, DispatchQueueAttr, DispatchRetained};
    use objc2::{
        AnyThread, DefinedClass, define_class,
        rc::{Retained, autoreleasepool},
        runtime::{NSObject, NSObjectProtocol, ProtocolObject},
    };
    use objc2_av_foundation::{
        AVAuthorizationStatus, AVCaptureConnection, AVCaptureDevice, AVCaptureDeviceInput,
        AVCaptureOutput, AVCaptureSession, AVCaptureVideoDataOutput,
        AVCaptureVideoDataOutputSampleBufferDelegate, AVMediaType, AVMediaTypeVideo,
    };
    use objc2_core_media::{CMSampleBuffer, CMVideoFormatDescriptionGetDimensions};
    use objc2_core_video::{
        CVPixelBuffer, CVPixelBufferGetBaseAddress, CVPixelBufferGetBaseAddressOfPlane,
        CVPixelBufferGetBytesPerRow, CVPixelBufferGetBytesPerRowOfPlane, CVPixelBufferGetDataSize,
        CVPixelBufferGetHeight, CVPixelBufferGetHeightOfPlane, CVPixelBufferGetPixelFormatType,
        CVPixelBufferGetPlaneCount, CVPixelBufferGetWidth, CVPixelBufferLockBaseAddress,
        CVPixelBufferLockFlags, CVPixelBufferUnlockBaseAddress,
        kCVPixelFormatType_420YpCbCr8BiPlanarFullRange,
        kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange, kCVPixelFormatType_422YpCbCr8_yuvs,
    };
    use objc2_foundation::{NSDictionary, NSString};
    use seeed_hal_camera::{
        CameraCaptureSession, CameraControlDescriptor, CameraControlKind, CameraControlValue,
        CameraFormat, CameraFrame, CameraFrameMetadata, CameraPixelFormat, CameraPlaneLayout,
        CameraRequest, camera_capture_capability, camera_frames_shm_capability,
    };
    use seeed_hal_core::{
        CapabilitySet, Endpoint, ErrorCategory, HalError, HalResult, IdentityQuality,
        ResourceDescriptor, ResourceProperties, ResourceSelector, TransportKind, resolve_resource,
    };
    use std::{
        collections::BTreeMap,
        sync::{Arc, Mutex},
        time::Duration,
    };

    use super::super::encode_resource_id;

    const CLOCK_DOMAIN: &str = "avfoundation";

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
            let native_id = NSString::from_str(unique_id);
            // SAFETY: The unique ID originates from the freshly enumerated,
            // selected descriptor and is passed to objc2's generated binding.
            let device = unsafe { AVCaptureDevice::deviceWithUniqueID(&native_id) }
                .ok_or_else(|| platform_error("camera.open", "selected camera disappeared"))?;
            // SAFETY: Device state and identity access use generated getters on
            // the retained device resolved immediately above.
            if !unsafe { device.isConnected() }
                || unsafe { device.uniqueID() }.to_string() != *unique_id
            {
                return Err(
                    platform_error("camera.open", "selected camera identity changed")
                        .with_resource_id(descriptor.id().clone()),
                );
            }
            let media_type = video_media_type("camera.open")?;
            // SAFETY: The generated AVFoundation query is passed only the
            // verified AVMediaTypeVideo singleton.
            if unsafe { AVCaptureDevice::authorizationStatusForMediaType(media_type) }
                != AVAuthorizationStatus::Authorized
            {
                return Err(
                    permission_denied("camera.open").with_resource_id(descriptor.id().clone())
                );
            }
            ensure_active_format(&device, request.format(), &descriptor)?;
            {
                let mut claimed = claims.lock().expect("AVFoundation claim mutex poisoned");
                if !claimed.insert(descriptor.id().clone()) {
                    return Err(adapter_conflict("camera.open", &descriptor));
                }
            }
            match configure_session(
                device,
                descriptor.clone(),
                request.format().clone(),
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
        // SAFETY: The selected retained device is connected and authorized;
        // its active format description is retained for this comparison.
        let active_format = unsafe { device.activeFormat() };
        // SAFETY: See the active-format access rationale above.
        let description = unsafe { active_format.formatDescription() };
        // SAFETY: The retained AVFoundation format description is a valid
        // CMVideoFormatDescription for the selected video capture device.
        let dimensions = unsafe { CMVideoFormatDescriptionGetDimensions(&description) };
        // SAFETY: CoreMedia's generated getter reads the immutable format
        // description returned by the selected device.
        let subtype = unsafe { description.media_sub_type() };
        let matches_pixel_format = match requested.pixel_format() {
            CameraPixelFormat::Nv12 => {
                subtype == kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange
                    || subtype == kCVPixelFormatType_420YpCbCr8BiPlanarFullRange
            }
            CameraPixelFormat::Yuyv => subtype == kCVPixelFormatType_422YpCbCr8_yuvs,
            CameraPixelFormat::Mjpeg => false,
        };
        if !matches_pixel_format
            || dimensions.width != i32::try_from(requested.width()).unwrap_or_default()
            || dimensions.height != i32::try_from(requested.height()).unwrap_or_default()
        {
            return Err(format_unsupported("camera.open", descriptor));
        }
        Ok(())
    }

    fn configure_session(
        device: Retained<AVCaptureDevice>,
        descriptor: ResourceDescriptor,
        format: CameraFormat,
        claims: Arc<Mutex<std::collections::BTreeSet<seeed_hal_core::ResourceId>>>,
    ) -> HalResult<Box<dyn CameraCaptureSession>> {
        // SAFETY: Generated factory invoked with a retained, freshly resolved
        // camera; NSError is mapped to a structured fail-closed error.
        let input = unsafe { AVCaptureDeviceInput::deviceInputWithDevice_error(&device) }
            .map_err(|error| platform_error("camera.open", error.to_string()))?;
        // SAFETY: Generated constructors take no external ABI arguments.
        let session = unsafe { AVCaptureSession::new() };
        // SAFETY: See session constructor rationale.
        let output = unsafe { AVCaptureVideoDataOutput::new() };
        configure_output(&output, &format, &descriptor)?;
        let frames = Arc::new(Mutex::new(FrameState::default()));
        let delegate = FrameDelegate::new(Arc::clone(&frames));
        let callback_queue = DispatchQueue::new(
            "io.seeed.hal.avfoundation.capture",
            DispatchQueueAttr::SERIAL,
        );
        let protocol: &ProtocolObject<dyn AVCaptureVideoDataOutputSampleBufferDelegate> =
            ProtocolObject::from_ref(&*delegate);
        // SAFETY: The output retains the delegate while callbacks are enabled;
        // this session additionally retains it and serializes callbacks on its
        // dedicated serial queue.
        unsafe { output.setSampleBufferDelegate_queue(Some(protocol), Some(&callback_queue)) };
        // SAFETY: The documented preconditions for `addInput:` and `addOutput:`
        // are checked by `canAdd*` before calls are made.
        unsafe {
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
        Ok(Box::new(AvFoundationSession {
            descriptor,
            format,
            native: Some(NativeSession {
                session,
                input,
                output,
                _delegate: delegate,
                _callback_queue: callback_queue,
            }),
            frames,
            next_sequence: 1,
            claims,
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
        // AVFoundation accepts an empty dictionary to request device-native
        // samples. This avoids spelling CoreVideo's CFString keys as
        // fabricated NSString values; exact requested format and dimensions
        // are still enforced against every delivered frame below.
        let settings = NSDictionary::<NSString, objc2::runtime::AnyObject>::new();
        // SAFETY: `settings` contains only AVFoundation's documented
        // CVPixelBuffer pixel-format, width, and height keys. Later frame
        // validation fail-closes if the device does not honor it exactly.
        unsafe { output.setVideoSettings(Some(&settings)) };
        Ok(())
    }

    #[derive(Default)]
    struct FrameState {
        frame: Option<NativeFrame>,
        dropped_count: u64,
    }

    struct NativeFrame {
        pixel_format: u32,
        width: usize,
        height: usize,
        planes: Vec<NativePlane>,
        timestamp_ns: u64,
    }

    struct NativePlane {
        bytes: Vec<u8>,
        stride: usize,
    }

    define_class!(
        #[unsafe(super(NSObject))]
        #[ivars = Arc<Mutex<FrameState>>]
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
                if let Ok(frame) = copy_native_frame(sample_buffer) {
                    let mut state = self.ivars().lock().expect("AVFoundation frame mutex poisoned");
                    if state.frame.replace(frame).is_some() {
                        state.dropped_count = state.dropped_count.saturating_add(1);
                    }
                }
            }

            #[unsafe(method(captureOutput:didDropSampleBuffer:fromConnection:))]
            fn capture_output_did_drop_sample_buffer_from_connection(
                &self,
                _output: &AVCaptureOutput,
                _sample_buffer: &CMSampleBuffer,
                _connection: &AVCaptureConnection,
            ) {
                let mut state = self.ivars().lock().expect("AVFoundation frame mutex poisoned");
                state.dropped_count = state.dropped_count.saturating_add(1);
            }
        }
    );

    impl FrameDelegate {
        fn new(state: Arc<Mutex<FrameState>>) -> Retained<Self> {
            // SAFETY: objc2's documented define_class pattern initializes the
            // declared ivar before NSObject `init`.
            unsafe { objc2::msg_send![super(Self::alloc().set_ivars(state)), init] }
        }
    }

    fn copy_native_frame(sample_buffer: &CMSampleBuffer) -> Result<NativeFrame, ()> {
        // SAFETY: The delegate owns the callback's valid sample buffer. objc2
        // retains the returned image buffer until this function returns.
        let image = unsafe { sample_buffer.image_buffer() }.ok_or(())?;
        let pixel_buffer: &CVPixelBuffer = &image;
        let flags = CVPixelBufferLockFlags::ReadOnly;
        // SAFETY: The pixel buffer is valid for this callback scope and only
        // read access is requested.
        if unsafe { CVPixelBufferLockBaseAddress(pixel_buffer, flags) } != 0 {
            return Err(());
        }
        let result = copy_locked_pixel_buffer(pixel_buffer, sample_buffer);
        // SAFETY: This exactly matches the successful read-only lock above.
        let _ = unsafe { CVPixelBufferUnlockBaseAddress(pixel_buffer, flags) };
        result
    }

    fn copy_locked_pixel_buffer(
        pixel_buffer: &CVPixelBuffer,
        sample_buffer: &CMSampleBuffer,
    ) -> Result<NativeFrame, ()> {
        // SAFETY: All CoreVideo queries operate on this read-locked buffer.
        let pixel_format = unsafe { CVPixelBufferGetPixelFormatType(pixel_buffer) };
        // SAFETY: See preceding CoreVideo query.
        let width = unsafe { CVPixelBufferGetWidth(pixel_buffer) };
        // SAFETY: See preceding CoreVideo query.
        let height = unsafe { CVPixelBufferGetHeight(pixel_buffer) };
        // SAFETY: See preceding CoreVideo query.
        let plane_count = unsafe { CVPixelBufferGetPlaneCount(pixel_buffer) };
        let mut planes = Vec::with_capacity(plane_count.max(1));
        if plane_count == 0 {
            // SAFETY: The buffer remains read-locked.
            let length = unsafe { CVPixelBufferGetDataSize(pixel_buffer) };
            // SAFETY: The buffer remains read-locked.
            let base = unsafe { CVPixelBufferGetBaseAddress(pixel_buffer) }.cast::<u8>();
            if base.is_null() || length == 0 {
                return Err(());
            }
            // SAFETY: CoreVideo supplied `base` and `length` for this locked
            // buffer; bytes are copied before the matching unlock.
            let bytes = unsafe { std::slice::from_raw_parts(base, length) }.to_vec();
            // SAFETY: The buffer remains read-locked.
            let stride = unsafe { CVPixelBufferGetBytesPerRow(pixel_buffer) };
            planes.push(NativePlane { bytes, stride });
        } else {
            for index in 0..plane_count {
                // SAFETY: `index` is bounded by this buffer's plane count.
                let stride = unsafe { CVPixelBufferGetBytesPerRowOfPlane(pixel_buffer, index) };
                // SAFETY: `index` is bounded by this buffer's plane count.
                let rows = unsafe { CVPixelBufferGetHeightOfPlane(pixel_buffer, index) };
                let length = stride.checked_mul(rows).ok_or(())?;
                // SAFETY: `index` is bounded by this buffer's plane count.
                let base =
                    unsafe { CVPixelBufferGetBaseAddressOfPlane(pixel_buffer, index) }.cast::<u8>();
                if base.is_null() || length == 0 {
                    return Err(());
                }
                // SAFETY: CoreVideo supplied the plane's address and
                // overflow-checked extent while the matching lock is held.
                let bytes = unsafe { std::slice::from_raw_parts(base, length) }.to_vec();
                planes.push(NativePlane { bytes, stride });
            }
        }
        // SAFETY: The valid callback buffer remains owned for this call.
        let seconds = unsafe { sample_buffer.presentation_time_stamp().seconds() };
        let timestamp_ns = if seconds.is_finite() && seconds >= 0.0 {
            (seconds * 1_000_000_000.0).min(u64::MAX as f64) as u64
        } else {
            0
        };
        Ok(NativeFrame {
            pixel_format,
            width,
            height,
            planes,
            timestamp_ns,
        })
    }

    struct NativeSession {
        session: Retained<AVCaptureSession>,
        input: Retained<AVCaptureDeviceInput>,
        output: Retained<AVCaptureVideoDataOutput>,
        _delegate: Retained<FrameDelegate>,
        _callback_queue: DispatchRetained<DispatchQueue>,
    }

    struct AvFoundationSession {
        descriptor: ResourceDescriptor,
        format: CameraFormat,
        native: Option<NativeSession>,
        frames: Arc<Mutex<FrameState>>,
        next_sequence: u64,
        claims: Arc<Mutex<std::collections::BTreeSet<seeed_hal_core::ResourceId>>>,
        closed: bool,
    }

    // SAFETY: CameraCaptureSession methods are invoked by the runtime's
    // single native camera worker. AVCaptureSession itself is confined to that
    // worker after creation; callbacks use a dedicated serial GCD queue and
    // communicate only through the Send+Sync bounded FrameState mutex.
    unsafe impl Send for AvFoundationSession {}

    #[async_trait]
    impl CameraCaptureSession for AvFoundationSession {
        fn descriptor(&self) -> &ResourceDescriptor {
            &self.descriptor
        }

        fn format(&self) -> &CameraFormat {
            &self.format
        }

        async fn capture(&mut self, timeout: Duration) -> HalResult<CameraFrame> {
            ensure_open(self.closed, "camera.capture", &self.descriptor)?;
            let started = std::time::Instant::now();
            loop {
                let candidate = {
                    let mut frames = self
                        .frames
                        .lock()
                        .expect("AVFoundation frame mutex poisoned");
                    frames
                        .frame
                        .take()
                        .map(|frame| (frame, frames.dropped_count))
                };
                if let Some((frame, dropped_count)) = candidate {
                    let published = publish_frame(
                        frame,
                        &self.format,
                        self.next_sequence,
                        dropped_count,
                        &self.descriptor,
                    )?;
                    self.next_sequence = self.next_sequence.saturating_add(1);
                    return Ok(published);
                }
                if started.elapsed() >= timeout {
                    return Err(timeout_error("camera.capture", &self.descriptor));
                }
                std::thread::sleep(Duration::from_millis(1));
            }
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
            if let Some(native) = self.native.take() {
                // SAFETY: The session owns the installed output/delegate. It
                // disables callbacks and stops the capture graph before the
                // Objective-C handles are dropped.
                unsafe {
                    native.output.setSampleBufferDelegate_queue(None, None);
                    native.session.stopRunning();
                    native.session.removeOutput(&native.output);
                    native.session.removeInput(&native.input);
                }
                self.closed = true;
                self.frames
                    .lock()
                    .expect("AVFoundation frame mutex poisoned")
                    .frame = None;
            }
            self.claims
                .lock()
                .expect("AVFoundation claim mutex poisoned")
                .remove(self.descriptor.id());
            Ok(())
        }
    }

    impl Drop for AvFoundationSession {
        fn drop(&mut self) {
            if let Some(native) = self.native.take() {
                // SAFETY: Drop owns the capture graph and disables callback
                // delivery before its Rust-backed delegate may be released.
                unsafe {
                    native.output.setSampleBufferDelegate_queue(None, None);
                    native.session.stopRunning();
                }
            }
            self.claims
                .lock()
                .expect("AVFoundation claim mutex poisoned")
                .remove(self.descriptor.id());
        }
    }

    fn publish_frame(
        frame: NativeFrame,
        requested: &CameraFormat,
        sequence: u64,
        dropped_count: u64,
        descriptor: &ResourceDescriptor,
    ) -> HalResult<CameraFrame> {
        let actual_format = if (frame.pixel_format
            == kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange
            || frame.pixel_format == kCVPixelFormatType_420YpCbCr8BiPlanarFullRange)
            && frame.planes.len() == 2
        {
            CameraPixelFormat::Nv12
        } else if frame.pixel_format == kCVPixelFormatType_422YpCbCr8_yuvs
            && frame.planes.len() == 1
        {
            CameraPixelFormat::Yuyv
        } else {
            return Err(format_unsupported("camera.capture", descriptor));
        };
        let width = u32::try_from(frame.width)
            .map_err(|_| format_unsupported("camera.capture", descriptor))?;
        let height = u32::try_from(frame.height)
            .map_err(|_| format_unsupported("camera.capture", descriptor))?;
        if actual_format != requested.pixel_format()
            || width != requested.width()
            || height != requested.height()
        {
            return Err(format_unsupported("camera.capture", descriptor));
        }
        let mut payload = Vec::new();
        let mut planes = Vec::with_capacity(frame.planes.len());
        for plane in frame.planes {
            let offset = payload.len();
            payload.extend_from_slice(&plane.bytes);
            planes.push(CameraPlaneLayout::new(
                offset,
                plane.bytes.len(),
                plane.stride,
            )?);
        }
        CameraFrame::new(
            CameraFrameMetadata::new(
                requested.clone(),
                planes,
                sequence,
                frame.timestamp_ns,
                CLOCK_DOMAIN,
                dropped_count,
            )?,
            Bytes::from(payload),
        )
    }

    fn video_media_type(operation: &'static str) -> HalResult<&'static AVMediaType> {
        // SAFETY: AVMediaTypeVideo is the immutable exported AVFoundation
        // singleton; optional absence becomes a fail-closed adapter error.
        unsafe { AVMediaTypeVideo }
            .ok_or_else(|| platform_error(operation, "AVMediaTypeVideo is absent"))
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
}

#[cfg(target_os = "macos")]
pub(super) use macos::{enumerate_sync, open_sync};
