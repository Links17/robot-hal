use async_trait::async_trait;
use seeed_hal_camera::{CameraAdapter, CameraCaptureSession, CameraRequest};
use seeed_hal_core::{
    ErrorCategory, HalError, HalResult, ResourceDescriptor, ResourceId, ResourceSelector,
};
use std::{
    collections::BTreeSet,
    sync::{Arc, Mutex},
};

#[cfg(target_os = "macos")]
mod native;

type Claims = Arc<Mutex<BTreeSet<ResourceId>>>;

#[derive(Clone, Debug)]
pub struct AvFoundationAdapter {
    claims: Claims,
}

#[cfg_attr(not(any(target_os = "macos", test)), allow(dead_code))]
fn quarantine_claim_until_worker_exits(
    worker: std::thread::JoinHandle<()>,
    claims: Claims,
    resource_id: ResourceId,
) -> tokio::task::JoinHandle<std::thread::Result<()>> {
    tokio::task::spawn_blocking(move || {
        let result = worker.join();
        claims
            .lock()
            .expect("AVFoundation claim mutex poisoned")
            .remove(&resource_id);
        result
    })
}

#[cfg(test)]
fn claim_conflict(claims: &Claims, resource_id: &ResourceId) -> bool {
    claims
        .lock()
        .expect("AVFoundation claim mutex poisoned")
        .contains(resource_id)
}

fn release_claim_after_drop(claims: &Claims, resource_id: &ResourceId, claim_quarantined: bool) {
    if !claim_quarantined {
        claims
            .lock()
            .expect("AVFoundation claim mutex poisoned")
            .remove(resource_id);
    }
}

impl AvFoundationAdapter {
    #[must_use]
    pub fn new() -> Self {
        Self {
            claims: Arc::new(Mutex::new(BTreeSet::new())),
        }
    }
}

impl Default for AvFoundationAdapter {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl CameraAdapter for AvFoundationAdapter {
    fn adapter_name(&self) -> &'static str {
        "avfoundation"
    }

    async fn enumerate(&self) -> HalResult<Vec<ResourceDescriptor>> {
        #[cfg(target_os = "macos")]
        {
            tokio::task::spawn_blocking(native::enumerate_sync)
                .await
                .map_err(|error| worker_failed("camera.enumerate", error))?
        }
        #[cfg(not(target_os = "macos"))]
        {
            Err(unavailable("camera.enumerate"))
        }
    }

    async fn open(
        &self,
        selector: &ResourceSelector,
        request: &CameraRequest,
    ) -> HalResult<Box<dyn CameraCaptureSession>> {
        #[cfg(target_os = "macos")]
        {
            let selector = selector.clone();
            let request = request.clone();
            let claims = Arc::clone(&self.claims);
            tokio::task::spawn_blocking(move || native::open_sync(&selector, &request, claims))
                .await
                .map_err(|error| worker_failed("camera.open", error))?
        }
        #[cfg(not(target_os = "macos"))]
        {
            let _ = request;
            Err(unavailable("camera.open").with_resource_id(selector.id().clone()))
        }
    }
}

fn encode_resource_id(unique_id: &str) -> HalResult<ResourceId> {
    if unique_id.is_empty() {
        return Err(HalError::new(
            "runtime.resource.invalid",
            ErrorCategory::InvalidArgument,
            "camera.identity",
            false,
            "AVFoundation unique ID must not be empty",
        )?);
    }
    let mut encoded = String::with_capacity(unique_id.len());
    for byte in unique_id.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~') {
            encoded.push(byte as char);
        } else {
            encoded.push('%');
            encoded.push(hex(byte >> 4));
            encoded.push(hex(byte & 0x0f));
        }
    }
    ResourceId::parse(format!("camera:avfoundation:{encoded}"))
}

const fn hex(nibble: u8) -> char {
    match nibble {
        0..=9 => (b'0' + nibble) as char,
        _ => (b'A' + (nibble - 10)) as char,
    }
}

#[cfg(not(target_os = "macos"))]
fn unavailable(operation: &'static str) -> HalError {
    HalError::new(
        "runtime.adapter.unavailable",
        ErrorCategory::Unavailable,
        operation,
        false,
        "AVFoundation camera support is unavailable on this platform",
    )
    .expect("static AVFoundation unavailable error metadata is valid")
}

#[cfg(target_os = "macos")]
fn worker_failed(operation: &'static str, error: tokio::task::JoinError) -> HalError {
    HalError::new(
        "runtime.internal.worker_failed",
        ErrorCategory::Internal,
        operation,
        false,
        format!("AVFoundation enumeration worker failed: {error}"),
    )
    .expect("static AVFoundation worker error metadata is valid")
}

#[cfg(test)]
mod tests {
    use super::{
        claim_conflict, encode_resource_id, quarantine_claim_until_worker_exits,
        release_claim_after_drop,
    };
    use seeed_hal_core::ResourceId;
    use std::{
        collections::BTreeSet,
        sync::{Arc, Mutex, mpsc},
    };

    #[test]
    fn resource_id_percent_encodes_avfoundation_unique_id() {
        assert_eq!(
            encode_resource_id("FaceTime HD Camera:ABC/123")
                .expect("valid unique ID")
                .as_str(),
            "camera:avfoundation:FaceTime%20HD%20Camera%3AABC%2F123"
        );
    }

    #[test]
    fn resource_id_rejects_empty_avfoundation_unique_id() {
        let error = encode_resource_id("").expect_err("empty native ID is not a stable identity");
        assert_eq!(error.name().as_str(), "runtime.resource.invalid");
    }

    #[tokio::test]
    async fn close_timeout_quarantines_the_claim_until_the_native_worker_exits() {
        let resource_id =
            ResourceId::parse("camera:avfoundation:teardown-test").expect("valid resource ID");
        let claims = Arc::new(Mutex::new(BTreeSet::from([resource_id.clone()])));
        let (release_sender, release_worker) = mpsc::channel();
        let (worker_exited, worker_exit_wait) = mpsc::channel();
        let worker = std::thread::spawn(move || {
            release_worker
                .recv()
                .expect("test must release the simulated native worker");
            worker_exited
                .send(())
                .expect("test must observe the simulated native worker exit");
        });

        let mut reaped =
            quarantine_claim_until_worker_exits(worker, Arc::clone(&claims), resource_id.clone());

        assert!(
            claim_conflict(&claims, &resource_id),
            "a second session must not open while the timed-out native worker can own the camera"
        );
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(20), &mut reaped)
                .await
                .is_err(),
            "the claim reaper must wait for native worker teardown"
        );

        release_sender
            .send(())
            .expect("test must release the simulated native worker");
        worker_exit_wait
            .recv_timeout(std::time::Duration::from_secs(1))
            .expect("simulated worker must exit");
        reaped
            .await
            .expect("claim reaper task must finish")
            .expect("simulated worker must not panic");
        assert!(
            !claim_conflict(&claims, &resource_id),
            "claim must be released only after the native worker exits"
        );
    }

    #[test]
    fn dropping_a_quarantined_session_keeps_its_claim() {
        let resource_id =
            ResourceId::parse("camera:avfoundation:quarantined-drop").expect("valid resource ID");
        let claims = Arc::new(Mutex::new(BTreeSet::from([resource_id.clone()])));

        release_claim_after_drop(&claims, &resource_id, true);

        assert!(
            claim_conflict(&claims, &resource_id),
            "Drop must not reopen a resource while its reaper owns the claim"
        );
    }

    #[cfg(not(target_os = "macos"))]
    #[tokio::test]
    async fn non_macos_calls_are_stably_unavailable_without_discovery() {
        use super::{AvFoundationAdapter, CameraAdapter};
        use seeed_hal_camera::{CameraFormat, CameraPixelFormat, CameraRequest};
        use seeed_hal_core::{IdentityQuality, ResourceId, ResourceSelector, TransportKind};

        let adapter = AvFoundationAdapter::new();
        assert_eq!(
            adapter
                .enumerate()
                .await
                .expect_err("AVFoundation must not probe a non-macOS host")
                .name()
                .as_str(),
            "runtime.adapter.unavailable"
        );
        let selector =
            ResourceSelector::by_id(ResourceId::parse("camera:avfoundation:test").unwrap());
        let request = CameraRequest::new(
            CameraFormat::new(CameraPixelFormat::Nv12, 640, 480).unwrap(),
            4,
        )
        .unwrap();
        assert_eq!(
            adapter
                .open(&selector, &request)
                .await
                .expect_err("AVFoundation must not probe a non-macOS host")
                .name()
                .as_str(),
            "runtime.adapter.unavailable"
        );
    }

    #[cfg(all(target_os = "macos", feature = "hardware-tests"))]
    #[tokio::test]
    #[ignore = "requires an authorized physical camera and SEEED_HAL_CAMERA_RESOURCE_ID"]
    async fn physical_camera_captures_requested_verified_frame() {
        use super::{AvFoundationAdapter, CameraAdapter};
        use seeed_hal_camera::{
            CameraFormat, CameraFrameMetadata, CameraFrameSink, CameraPixelFormat, CameraRequest,
        };
        use seeed_hal_core::{
            HalResult, IdentityQuality, ResourceId, ResourceSelector, TransportKind,
        };
        use std::{
            sync::{Arc, Mutex},
            time::Duration,
        };

        struct CollectingSink {
            metadata: Mutex<Option<CameraFrameMetadata>>,
            bytes_written: Mutex<usize>,
        }

        impl CameraFrameSink for CollectingSink {
            fn publish(
                &self,
                metadata: CameraFrameMetadata,
                copy_payload: &mut dyn FnMut(&mut [u8]) -> HalResult<usize>,
            ) -> HalResult<()> {
                let mut buffer = vec![0_u8; 4 * 1024 * 1024];
                let written = copy_payload(&mut buffer)?;
                *self.bytes_written.lock().expect("sink mutex poisoned") = written;
                *self.metadata.lock().expect("sink mutex poisoned") = Some(metadata);
                Ok(())
            }
        }

        let Some(resource_id) = std::env::var("SEEED_HAL_CAMERA_RESOURCE_ID").ok() else {
            return;
        };
        let adapter = AvFoundationAdapter::new();
        let descriptor = adapter
            .enumerate()
            .await
            .expect("physical AVFoundation discovery must succeed")
            .into_iter()
            .find(|descriptor| descriptor.id().as_str() == resource_id)
            .expect("SEEED_HAL_CAMERA_RESOURCE_ID must select an enumerated camera");
        eprintln!(
            "qualifying camera name={}",
            descriptor
                .properties()
                .get("camera.name")
                .unwrap_or("missing")
        );
        let request = CameraRequest::new(
            CameraFormat::new(CameraPixelFormat::Nv12, 1920, 1080).unwrap(),
            4,
        )
        .unwrap();
        let selector = ResourceSelector::exact(
            ResourceId::parse(descriptor.id().as_str()).unwrap(),
            IdentityQuality::Strong,
            TransportKind::Camera,
        );
        let mjpeg = CameraRequest::new(
            CameraFormat::new(CameraPixelFormat::Mjpeg, 1920, 1080).unwrap(),
            4,
        )
        .unwrap();
        let mjpeg_error = adapter
            .open(&selector, &mjpeg)
            .await
            .err()
            .expect("MJPEG must fail closed");
        assert_eq!(mjpeg_error.name().as_str(), "camera.format.unsupported");
        let mut session = adapter
            .open(&selector, &request)
            .await
            .expect("selected physical camera must open for the requested format");
        let conflict = adapter
            .open(&selector, &request)
            .await
            .err()
            .expect("a second open must conflict until the first session closes");
        assert_eq!(conflict.name().as_str(), "runtime.adapter.conflict");
        assert_eq!(
            session
                .controls()
                .await
                .expect_err("AVFoundation must not advertise camera controls")
                .name()
                .as_str(),
            "camera.control.unsupported"
        );
        let capture_error = session
            .capture(Duration::from_secs(1))
            .await
            .expect_err("AVFoundation must not return frame bytes from capture()");
        assert_eq!(
            capture_error.name().as_str(),
            "runtime.transport.unavailable"
        );
        let sink = Arc::new(CollectingSink {
            metadata: Mutex::new(None),
            bytes_written: Mutex::new(0),
        });
        session
            .capture_into(
                Duration::from_secs(3),
                Arc::clone(&sink) as Arc<dyn CameraFrameSink>,
            )
            .await
            .expect("selected physical camera must publish a frame into the capture sink");
        let metadata = sink
            .metadata
            .lock()
            .expect("sink mutex poisoned")
            .clone()
            .expect("capture sink must receive frame metadata");
        assert_eq!(metadata.format(), request.format());
        assert!(
            metadata.sequence() > 0,
            "captured frame sequence must be nonzero"
        );
        assert!(
            *sink.bytes_written.lock().expect("sink mutex poisoned") > 0,
            "captured frame payload must be nonzero"
        );
        session
            .close()
            .await
            .expect("physical camera closes cleanly");
        let mut session = adapter
            .open(&selector, &request)
            .await
            .expect("camera must reopen after close");
        *sink.metadata.lock().expect("sink mutex poisoned") = None;
        *sink.bytes_written.lock().expect("sink mutex poisoned") = 0;
        session
            .capture_into(
                Duration::from_secs(3),
                Arc::clone(&sink) as Arc<dyn CameraFrameSink>,
            )
            .await
            .expect("reopened camera must publish a frame");
        let reopened = sink
            .metadata
            .lock()
            .expect("sink mutex poisoned")
            .clone()
            .expect("reopened capture sink must receive frame metadata");
        assert_eq!(reopened.format(), request.format());
        assert!(reopened.sequence() > 0);
        session
            .close()
            .await
            .expect("reopened physical camera closes cleanly");
    }

    #[cfg(all(target_os = "macos", feature = "hardware-tests"))]
    #[tokio::test]
    #[ignore = "requires operator-controlled unplug of SEEED_HAL_CAMERA_RESOURCE_ID"]
    async fn physical_camera_hot_unplug_becomes_terminal_then_reopens() {
        use super::{AvFoundationAdapter, CameraAdapter};
        use crate::native;
        use seeed_hal_camera::{
            CameraFormat, CameraFrameMetadata, CameraFrameSink, CameraPixelFormat, CameraRequest,
        };
        use seeed_hal_core::{
            HalResult, IdentityQuality, ResourceId, ResourceSelector, TransportKind,
        };
        use std::{
            sync::{Arc, Mutex},
            time::{Duration, Instant},
        };

        struct CollectingSink {
            metadata: Mutex<Option<CameraFrameMetadata>>,
            bytes_written: Mutex<usize>,
        }

        impl CameraFrameSink for CollectingSink {
            fn publish(
                &self,
                metadata: CameraFrameMetadata,
                copy_payload: &mut dyn FnMut(&mut [u8]) -> HalResult<usize>,
            ) -> HalResult<()> {
                let mut buffer = vec![0_u8; 4 * 1024 * 1024];
                let written = copy_payload(&mut buffer)?;
                *self.bytes_written.lock().expect("sink mutex poisoned") = written;
                *self.metadata.lock().expect("sink mutex poisoned") = Some(metadata);
                Ok(())
            }
        }

        let Some(resource_id) = std::env::var("SEEED_HAL_CAMERA_RESOURCE_ID").ok() else {
            return;
        };
        let adapter = AvFoundationAdapter::new();
        let descriptor = adapter
            .enumerate()
            .await
            .expect("physical AVFoundation discovery must succeed")
            .into_iter()
            .find(|descriptor| descriptor.id().as_str() == resource_id)
            .expect("SEEED_HAL_CAMERA_RESOURCE_ID must select an enumerated camera");
        eprintln!(
            "hot-unplug fixture name={}",
            descriptor
                .properties()
                .get("camera.name")
                .unwrap_or("missing")
        );
        let selector = ResourceSelector::exact(
            ResourceId::parse(descriptor.id().as_str()).unwrap(),
            IdentityQuality::Strong,
            TransportKind::Camera,
        );
        let candidates = [
            (CameraPixelFormat::Nv12, 1920, 1080),
            (CameraPixelFormat::Nv12, 1280, 720),
            (CameraPixelFormat::Nv12, 640, 480),
            (CameraPixelFormat::Yuyv, 1920, 1080),
            (CameraPixelFormat::Yuyv, 1280, 720),
            (CameraPixelFormat::Yuyv, 640, 480),
        ];
        let mut session = None;
        let mut request = None;
        for (pixel_format, width, height) in candidates {
            let candidate =
                CameraRequest::new(CameraFormat::new(pixel_format, width, height).unwrap(), 4)
                    .unwrap();
            match adapter.open(&selector, &candidate).await {
                Ok(opened) => {
                    eprintln!("hot-unplug opened format={pixel_format:?} {width}x{height}");
                    session = Some(opened);
                    request = Some(candidate);
                    break;
                }
                Err(error) if error.name().as_str() == "camera.format.unsupported" => continue,
                Err(error) => panic!("unexpected open error before unplug: {error:?}"),
            }
        }
        let request = request.expect("fixture must support an exact NV12 or YUYV active format");
        let mut session = session.expect("selected physical camera must open before unplug");
        let sink = Arc::new(CollectingSink {
            metadata: Mutex::new(None),
            bytes_written: Mutex::new(0),
        });
        session
            .capture_into(
                Duration::from_secs(3),
                Arc::clone(&sink) as Arc<dyn CameraFrameSink>,
            )
            .await
            .expect("camera must publish a frame before unplug");

        eprintln!(
            "UNPLUG the selected USB camera now (the camera itself, not the whole hub). Waiting 90s..."
        );
        let unplug_deadline = Instant::now() + Duration::from_secs(90);
        let mut discovery_gone = false;
        let mut last_status = Instant::now();
        let unplugged = loop {
            let discovery_present = adapter
                .enumerate()
                .await
                .expect("enumeration while waiting for unplug must succeed")
                .iter()
                .any(|descriptor| descriptor.id().as_str() == resource_id);
            if !discovery_gone && !discovery_present {
                discovery_gone = true;
                eprintln!(
                    "discovery no longer lists the selected camera; waiting for camera.session.unplugged"
                );
            }
            match session
                .capture_into(
                    Duration::from_millis(500),
                    Arc::clone(&sink) as Arc<dyn CameraFrameSink>,
                )
                .await
            {
                Ok(()) if Instant::now() < unplug_deadline => {
                    if last_status.elapsed() >= Duration::from_secs(5) {
                        eprintln!(
                            "hot-unplug wait: still receiving frames; discovery_present={discovery_present}"
                        );
                        last_status = Instant::now();
                    }
                    continue;
                }
                Ok(()) => panic!("disconnect was not observed before the operator deadline"),
                Err(error) if error.name().as_str() == "camera.session.unplugged" => break error,
                Err(error) if error.name().as_str() == "runtime.transport.timeout" => {
                    if last_status.elapsed() >= Duration::from_secs(5) {
                        eprintln!(
                            "hot-unplug wait: capture timeout; discovery_present={discovery_present}"
                        );
                        last_status = Instant::now();
                    }
                    if Instant::now() < unplug_deadline {
                        continue;
                    }
                    panic!("disconnect was not observed before the operator deadline");
                }
                Err(error) => {
                    panic!(
                        "unexpected capture error while waiting for unplug: {} ({})",
                        error.name().as_str(),
                        error.debug_message()
                    )
                }
            }
        };
        assert_eq!(
            unplugged.resource_id().map(|id| id.as_str()),
            Some(resource_id.as_str())
        );
        let still_unplugged = session
            .capture_into(
                Duration::from_millis(500),
                Arc::clone(&sink) as Arc<dyn CameraFrameSink>,
            )
            .await
            .expect_err("an unplugged session must stay terminal");
        assert_eq!(still_unplugged.name().as_str(), "camera.session.unplugged");
        // macOS UVC often keeps a phantom discovery entry while the capture
        // stream is already dead; session terminal state is the primary signal.
        let discovery_after_unplug = adapter
            .enumerate()
            .await
            .expect("enumeration after unplug must succeed");
        if discovery_after_unplug
            .iter()
            .any(|descriptor| descriptor.id().as_str() == resource_id)
        {
            eprintln!(
                "note: selected camera still listed in discovery after unplug (macOS UVC phantom entry)"
            );
        }
        let unique_id = descriptor
            .properties()
            .get("camera.unique_id")
            .expect("fixture descriptor must carry camera.unique_id")
            .to_owned();
        session
            .close()
            .await
            .expect("unplugged camera session still closes");

        eprintln!("RECONNECT the selected USB camera now. Waiting 90s...");
        let reconnect_deadline = Instant::now() + Duration::from_secs(90);
        let mut last_status = Instant::now();
        let mut session = None;
        let reopen_candidates: Vec<_> = std::iter::once((
            request.format().pixel_format(),
            request.format().width(),
            request.format().height(),
        ))
        .chain(candidates)
        .collect();
        while session.is_none() && Instant::now() < reconnect_deadline {
            let listed = adapter
                .enumerate()
                .await
                .expect("enumeration during reconnect must succeed")
                .iter()
                .any(|descriptor| descriptor.id().as_str() == resource_id);
            let connectable = tokio::task::spawn_blocking({
                let unique_id = unique_id.clone();
                move || native::device_connectable_sync(&unique_id)
            })
            .await
            .expect("reconnect probe task must complete");
            if last_status.elapsed() >= Duration::from_secs(5) {
                eprintln!("reconnect wait: listed={listed} connectable={connectable}");
                last_status = Instant::now();
            }
            if listed && connectable {
                for (pixel_format, width, height) in &reopen_candidates {
                    let candidate = CameraRequest::new(
                        CameraFormat::new(*pixel_format, *width, *height).unwrap(),
                        4,
                    )
                    .unwrap();
                    match adapter.open(&selector, &candidate).await {
                        Ok(opened) => {
                            eprintln!(
                                "hot-unplug reopened format={pixel_format:?} {width}x{height}"
                            );
                            session = Some(opened);
                            break;
                        }
                        Err(error) if error.name().as_str() == "camera.format.unsupported" => {
                            continue;
                        }
                        Err(error) if error.retryable() => break,
                        Err(error) => panic!("unexpected open error after reconnect: {error:?}"),
                    }
                }
            }
            if session.is_none() {
                tokio::time::sleep(Duration::from_millis(200)).await;
            }
        }
        // Phase 2: first-frame recovery.
        // `open` now includes a 2-second frame-readiness probe, so a session
        // returned here has already delivered at least one frame internally.
        // We still allow a bounded retry window in case the probe window is tight
        // or a subsequent capture stalls. `camera.session.unplugged` during this
        // phase means the device disconnected again — fall back to the reconnect
        // wait loop rather than panicking.
        let first_frame_deadline = Instant::now() + Duration::from_secs(30);
        let mut session = session.expect("camera must reopen after reconnect");
        let mut frame_recovered = false;
        let mut reopen_attempt = 0_u32;
        while !frame_recovered {
            if Instant::now() >= first_frame_deadline {
                panic!("reconnected camera must publish a frame before deadline (30s)");
            }
            match session
                .capture_into(
                    Duration::from_millis(1000),
                    Arc::clone(&sink) as Arc<dyn CameraFrameSink>,
                )
                .await
            {
                Ok(()) => {
                    frame_recovered = true;
                }
                Err(error) if error.name().as_str() == "runtime.transport.timeout" => {
                    // Session opened but frame delayed — unlikely after probe, but retry.
                    eprintln!(
                        "reconnect capture warmup: transport.timeout (attempt={reopen_attempt})"
                    );
                    session
                        .close()
                        .await
                        .expect("timed-out reconnect session must close cleanly");
                    tokio::time::sleep(Duration::from_millis(300)).await;
                    let mut reopened = None;
                    for (pixel_format, width, height) in &reopen_candidates {
                        let candidate = CameraRequest::new(
                            CameraFormat::new(*pixel_format, *width, *height).unwrap(),
                            4,
                        )
                        .unwrap();
                        match adapter.open(&selector, &candidate).await {
                            Ok(opened) => {
                                reopen_attempt = reopen_attempt.saturating_add(1);
                                eprintln!(
                                    "reconnect warmup reopen attempt={reopen_attempt} \
                                     format={pixel_format:?} {width}x{height}"
                                );
                                reopened = Some(opened);
                                break;
                            }
                            Err(error) if error.name().as_str() == "camera.format.unsupported" => {
                                continue;
                            }
                            Err(error) if error.retryable() => break,
                            Err(error) => {
                                panic!("unexpected open error during reconnect warmup: {error:?}")
                            }
                        }
                    }
                    if let Some(opened) = reopened {
                        session = opened;
                    } else {
                        tokio::time::sleep(Duration::from_millis(300)).await;
                    }
                }
                Err(error) if error.name().as_str() == "camera.session.unplugged" => {
                    // Device disconnected again during first-frame recovery.
                    eprintln!(
                        "reconnect capture: device unplugged again during warmup — \
                         returning to reconnect wait loop"
                    );
                    panic!(
                        "device unplugged during first-frame recovery after reconnect: {error:?}"
                    );
                }
                Err(error) => panic!("unexpected capture error after reconnect: {error:?}"),
            }
        }
        session
            .close()
            .await
            .expect("reconnected camera closes cleanly");
    }
}
