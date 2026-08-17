use async_trait::async_trait;
use seeed_hal_camera::{CameraAdapter, CameraCaptureSession, CameraRequest};
use seeed_hal_core::{
    ErrorCategory, HalError, HalResult, ResourceDescriptor, ResourceId, ResourceSelector,
};

#[cfg(windows)]
mod native;

#[cfg(windows)]
use std::{
    collections::BTreeSet,
    sync::{Arc, Mutex},
};

#[cfg_attr(not(windows), derive(Clone, Copy, Debug, Default))]
#[cfg_attr(windows, derive(Clone, Debug))]
pub struct MediaFoundationAdapter {
    #[cfg(windows)]
    claims: Arc<Mutex<BTreeSet<ResourceId>>>,
}

impl MediaFoundationAdapter {
    #[must_use]
    pub fn new() -> Self {
        #[cfg(windows)]
        {
            return Self {
                claims: Arc::new(Mutex::new(BTreeSet::new())),
            };
        }
        #[cfg(not(windows))]
        {
            Self {}
        }
    }
}

#[async_trait]
impl CameraAdapter for MediaFoundationAdapter {
    fn adapter_name(&self) -> &'static str {
        "mediafoundation"
    }

    async fn enumerate(&self) -> HalResult<Vec<ResourceDescriptor>> {
        #[cfg(windows)]
        {
            tokio::task::spawn_blocking(native::enumerate_sync)
                .await
                .map_err(|error| worker_failed("camera.enumerate", error))?
        }
        #[cfg(not(windows))]
        {
            Err(unavailable("camera.enumerate"))
        }
    }

    async fn open(
        &self,
        selector: &ResourceSelector,
        request: &CameraRequest,
    ) -> HalResult<Box<dyn CameraCaptureSession>> {
        #[cfg(windows)]
        {
            let selector = selector.clone();
            let request = request.clone();
            let claims = Arc::clone(&self.claims);
            tokio::task::spawn_blocking(move || native::open_sync(&selector, &request, claims))
                .await
                .map_err(|error| worker_failed("camera.open", error))?
        }
        #[cfg(not(windows))]
        {
            let _ = request;
            Err(unavailable("camera.open").with_resource_id(selector.id().clone()))
        }
    }
}

#[cfg_attr(not(any(windows, test)), allow(dead_code))]
fn encode_resource_id(symbolic_link: &str) -> HalResult<ResourceId> {
    if symbolic_link.is_empty() {
        return Err(HalError::new(
            "runtime.resource.invalid",
            ErrorCategory::InvalidArgument,
            "camera.identity",
            false,
            "Media Foundation symbolic link must not be empty",
        )?);
    }
    let mut encoded = String::with_capacity(symbolic_link.len());
    for byte in symbolic_link.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~') {
            encoded.push(byte as char);
        } else {
            encoded.push('%');
            encoded.push(hex(byte >> 4));
            encoded.push(hex(byte & 0x0f));
        }
    }
    ResourceId::parse(format!("camera:mediafoundation:{encoded}"))
}

#[cfg_attr(not(any(windows, test)), allow(dead_code))]
const fn hex(nibble: u8) -> char {
    match nibble {
        0..=9 => (b'0' + nibble) as char,
        _ => (b'A' + (nibble - 10)) as char,
    }
}

#[cfg(not(windows))]
fn unavailable(operation: &'static str) -> HalError {
    HalError::new(
        "runtime.adapter.unavailable",
        ErrorCategory::Unavailable,
        operation,
        false,
        "Windows Media Foundation camera support is unavailable on this platform",
    )
    .expect("static Media Foundation unavailable error metadata is valid")
}

#[cfg(windows)]
fn worker_failed(operation: &'static str, error: tokio::task::JoinError) -> HalError {
    HalError::new(
        "runtime.internal.worker_failed",
        ErrorCategory::Internal,
        operation,
        false,
        format!("Media Foundation worker failed: {error}"),
    )
    .expect("static Media Foundation worker error metadata is valid")
}

#[cfg(test)]
mod tests {
    use super::{CameraAdapter, MediaFoundationAdapter, encode_resource_id};
    use seeed_hal_camera::{CameraFormat, CameraPixelFormat, CameraRequest};
    use seeed_hal_core::{IdentityQuality, ResourceId, ResourceSelector, TransportKind};

    #[test]
    fn resource_id_percent_encodes_media_foundation_symbolic_link() {
        assert_eq!(
            encode_resource_id(r"\\?\usb#vid_1234&pid_5678#camera/0")
                .expect("valid symbolic link")
                .as_str(),
            "camera:mediafoundation:%5C%5C%3F%5Cusb%23vid_1234%26pid_5678%23camera%2F0"
        );
    }

    #[test]
    fn resource_id_rejects_empty_media_foundation_symbolic_link() {
        assert_eq!(
            encode_resource_id("")
                .expect_err("empty link is not a stable identity")
                .name()
                .as_str(),
            "runtime.resource.invalid"
        );
    }

    #[tokio::test]
    async fn non_windows_calls_are_stably_unavailable_without_discovery() {
        let adapter = MediaFoundationAdapter::new();
        assert_eq!(
            adapter
                .enumerate()
                .await
                .expect_err("Media Foundation must not probe a non-Windows host")
                .name()
                .as_str(),
            "runtime.adapter.unavailable"
        );

        let request = CameraRequest::new(
            CameraFormat::new(CameraPixelFormat::Nv12, 640, 480).expect("valid format"),
            4,
        )
        .expect("valid request");
        let selector = ResourceSelector::exact(
            ResourceId::parse("camera:mediafoundation:test").expect("valid resource ID"),
            IdentityQuality::Strong,
            TransportKind::Camera,
        );
        let error = match adapter.open(&selector, &request).await {
            Ok(_) => panic!("Media Foundation must not probe a non-Windows host"),
            Err(error) => error,
        };
        assert_eq!(error.name().as_str(), "runtime.adapter.unavailable");
    }

    #[cfg(all(windows, feature = "hardware-tests"))]
    #[tokio::test]
    #[ignore = "requires an accessible physical camera and SEEED_HAL_CAMERA_RESOURCE_ID"]
    async fn physical_camera_captures_requested_verified_frame() {
        use seeed_hal_camera::CameraCaptureSession;
        use std::time::Duration;

        let resource_id = std::env::var("SEEED_HAL_CAMERA_RESOURCE_ID")
            .expect("set SEEED_HAL_CAMERA_RESOURCE_ID to an enumerated Media Foundation camera");
        let adapter = MediaFoundationAdapter::new();
        let descriptor = adapter
            .enumerate()
            .await
            .expect("physical Media Foundation discovery must succeed")
            .into_iter()
            .find(|descriptor| descriptor.id().as_str() == resource_id)
            .expect("SEEED_HAL_CAMERA_RESOURCE_ID must select an enumerated camera");
        let request = CameraRequest::new(
            CameraFormat::new(CameraPixelFormat::Nv12, 640, 480).expect("valid format"),
            4,
        )
        .expect("valid request");
        let mut session = adapter
            .open(
                &ResourceSelector::exact(
                    ResourceId::parse(descriptor.id().as_str()).expect("valid resource ID"),
                    IdentityQuality::Medium,
                    TransportKind::Camera,
                ),
                &request,
            )
            .await
            .expect("selected camera must negotiate the requested exact format");
        let frame = session
            .capture(Duration::from_secs(3))
            .await
            .expect("selected camera must capture a verified frame");
        assert_eq!(frame.metadata().format(), request.format());
        assert_eq!(frame.metadata().sequence(), 1);
        session.close().await.expect("selected camera must close");
    }
}
