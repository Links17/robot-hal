use async_trait::async_trait;
use seeed_hal_camera::{CameraAdapter, CameraCaptureSession, CameraRequest};
use seeed_hal_core::{
    ErrorCategory, HalError, HalResult, ResourceDescriptor, ResourceId, ResourceSelector,
};
#[cfg(target_os = "linux")]
use std::{
    collections::BTreeSet,
    sync::{Arc, Mutex},
};

#[cfg(target_os = "linux")]
mod native;

#[cfg_attr(not(target_os = "linux"), derive(Clone, Copy, Debug, Default))]
#[cfg_attr(target_os = "linux", derive(Clone, Debug))]
pub struct V4l2Adapter {
    #[cfg(target_os = "linux")]
    claims: Arc<Mutex<BTreeSet<ResourceId>>>,
}

impl V4l2Adapter {
    #[must_use]
    pub fn new() -> Self {
        #[cfg(target_os = "linux")]
        {
            return Self {
                claims: Arc::new(Mutex::new(BTreeSet::new())),
            };
        }
        #[cfg(not(target_os = "linux"))]
        {
            Self {}
        }
    }
}

#[cfg(target_os = "linux")]
impl Default for V4l2Adapter {
    fn default() -> Self {
        Self {
            claims: Arc::new(Mutex::new(BTreeSet::new())),
        }
    }
}

#[cfg_attr(not(any(target_os = "linux", test)), allow(dead_code))]
fn encode_resource_id(evidence: &str) -> HalResult<ResourceId> {
    if evidence.is_empty() {
        return Err(HalError::new(
            "runtime.resource.invalid",
            ErrorCategory::InvalidArgument,
            "camera.identity",
            false,
            "V4L2 identity evidence must not be empty",
        )?);
    }
    let mut encoded = String::with_capacity(evidence.len());
    for byte in evidence.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~') {
            encoded.push(byte as char);
        } else {
            encoded.push('%');
            encoded.push(hex(byte >> 4));
            encoded.push(hex(byte & 0x0f));
        }
    }
    ResourceId::parse(format!("camera:v4l2:{encoded}"))
}

#[cfg_attr(not(any(target_os = "linux", test)), allow(dead_code))]
const fn hex(nibble: u8) -> char {
    match nibble {
        0..=9 => (b'0' + nibble) as char,
        _ => (b'A' + (nibble - 10)) as char,
    }
}

#[async_trait]
impl CameraAdapter for V4l2Adapter {
    fn adapter_name(&self) -> &'static str {
        "v4l2"
    }

    async fn enumerate(&self) -> HalResult<Vec<ResourceDescriptor>> {
        #[cfg(target_os = "linux")]
        {
            tokio::task::spawn_blocking(native::enumerate_sync)
                .await
                .map_err(|error| worker_failed("camera.enumerate", error))?
        }
        #[cfg(not(target_os = "linux"))]
        {
            Err(unavailable("camera.enumerate"))
        }
    }

    async fn open(
        &self,
        selector: &ResourceSelector,
        request: &CameraRequest,
    ) -> HalResult<Box<dyn CameraCaptureSession>> {
        #[cfg(target_os = "linux")]
        {
            let selector = selector.clone();
            let request = request.clone();
            let claims = Arc::clone(&self.claims);
            tokio::task::spawn_blocking(move || native::open_sync(&selector, &request, claims))
                .await
                .map_err(|error| worker_failed("camera.open", error))?
        }
        #[cfg(not(target_os = "linux"))]
        {
            let _ = request;
            Err(unavailable("camera.open").with_resource_id(selector.id().clone()))
        }
    }
}

#[cfg(target_os = "linux")]
fn worker_failed(operation: &'static str, error: tokio::task::JoinError) -> HalError {
    HalError::new(
        "runtime.internal.worker_failed",
        ErrorCategory::Internal,
        operation,
        false,
        format!("V4L2 worker failed: {error}"),
    )
    .expect("static V4L2 worker error metadata is valid")
}

#[cfg(not(target_os = "linux"))]
fn unavailable(operation: &'static str) -> HalError {
    HalError::new(
        "runtime.adapter.unavailable",
        ErrorCategory::Unavailable,
        operation,
        false,
        "native V4L2 camera support is unavailable on this platform",
    )
    .expect("static V4L2 unavailable error metadata is valid")
}

#[cfg(test)]
mod tests {
    use super::{CameraAdapter, V4l2Adapter, encode_resource_id};
    use seeed_hal_camera::{CameraFormat, CameraPixelFormat, CameraRequest};
    use seeed_hal_core::{IdentityQuality, ResourceId, ResourceSelector, TransportKind};

    #[cfg(not(target_os = "linux"))]
    #[tokio::test]
    async fn non_linux_calls_are_stably_unavailable_without_discovery() {
        let adapter = V4l2Adapter::new();
        assert_eq!(
            adapter
                .enumerate()
                .await
                .expect_err("V4L2 must not probe a non-Linux host")
                .name()
                .as_str(),
            "runtime.adapter.unavailable"
        );

        let request = CameraRequest::new(
            CameraFormat::new(CameraPixelFormat::Nv12, 640, 480).unwrap(),
            4,
        )
        .unwrap();
        let selector = ResourceSelector::exact(
            ResourceId::parse("camera:v4l2:test").expect("valid ID"),
            IdentityQuality::Strong,
            TransportKind::Camera,
        );
        let error = match adapter.open(&selector, &request).await {
            Ok(_) => panic!("V4L2 must not probe a non-Linux host"),
            Err(error) => error,
        };
        assert_eq!(error.name().as_str(), "runtime.adapter.unavailable");
    }

    #[test]
    fn resource_id_percent_encodes_v4l2_identity_evidence() {
        assert_eq!(
            encode_resource_id("serial:ACME Camera/123")
                .unwrap()
                .as_str(),
            "camera:v4l2:serial%3AACME%20Camera%2F123"
        );
    }

    #[test]
    fn resource_id_rejects_empty_v4l2_identity_evidence() {
        assert_eq!(
            encode_resource_id("").unwrap_err().name().as_str(),
            "runtime.resource.invalid"
        );
    }

    #[cfg(all(target_os = "linux", feature = "hardware-tests"))]
    #[tokio::test]
    #[ignore = "requires an accessible physical V4L2 camera and SEEED_HAL_CAMERA_RESOURCE_ID"]
    async fn physical_camera_captures_requested_verified_frame() {
        use seeed_hal_camera::{
            CameraCaptureSession, CameraFormat, CameraPixelFormat, CameraRequest,
        };
        use seeed_hal_core::{IdentityQuality, ResourceId, ResourceSelector, TransportKind};
        use std::time::Duration;

        let resource_id = std::env::var("SEEED_HAL_CAMERA_RESOURCE_ID")
            .expect("set SEEED_HAL_CAMERA_RESOURCE_ID to an enumerated V4L2 camera");
        let adapter = V4l2Adapter::new();
        let descriptor = adapter
            .enumerate()
            .await
            .expect("physical V4L2 discovery must succeed")
            .into_iter()
            .find(|descriptor| descriptor.id().as_str() == resource_id)
            .expect("SEEED_HAL_CAMERA_RESOURCE_ID must select an enumerated camera");
        let request = CameraRequest::new(
            CameraFormat::new(CameraPixelFormat::Yuyv, 640, 480).unwrap(),
            4,
        )
        .unwrap();
        let mut session = adapter
            .open(
                &ResourceSelector::exact(
                    ResourceId::parse(descriptor.id().as_str()).unwrap(),
                    IdentityQuality::Medium,
                    TransportKind::Camera,
                ),
                &request,
            )
            .await
            .expect("selected V4L2 camera must negotiate the requested exact format");
        let frame = session
            .capture(Duration::from_secs(3))
            .await
            .expect("selected V4L2 camera must produce a frame");
        assert_eq!(frame.metadata().format(), request.format());
        assert_eq!(frame.metadata().sequence(), 1);
        session
            .close()
            .await
            .expect("physical V4L2 camera closes cleanly");
    }
}
