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
        use seeed_hal_camera::{CameraFormat, CameraPixelFormat, CameraRequest};
        use seeed_hal_core::{IdentityQuality, ResourceId, ResourceSelector, TransportKind};
        use std::time::Duration;

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
        let request = CameraRequest::new(
            CameraFormat::new(CameraPixelFormat::Nv12, 640, 480).unwrap(),
            4,
        )
        .unwrap();
        let mut session = adapter
            .open(
                &ResourceSelector::exact(
                    ResourceId::parse(descriptor.id().as_str()).unwrap(),
                    IdentityQuality::Strong,
                    TransportKind::Camera,
                ),
                &request,
            )
            .await
            .expect("selected physical camera must open for the requested format");
        let frame = session
            .capture(Duration::from_secs(3))
            .await
            .expect("selected physical camera must provide a frame");
        assert_eq!(frame.metadata().format(), request.format());
        session
            .close()
            .await
            .expect("physical camera closes cleanly");
    }
}
