use seeed_hal_camera::{
    CameraControlKind, CameraControlValue, CameraFormat, CameraPixelFormat, CameraRequest,
};
use seeed_hal_core::OwnerId;
use seeed_hal_runtime::HalRuntime;
use seeed_hal_testkit::VirtualCameraAdapter;
use std::time::Duration;

fn request() -> CameraRequest {
    CameraRequest::new(
        CameraFormat::new(CameraPixelFormat::Nv12, 640, 480).unwrap(),
        4,
    )
    .unwrap()
}

#[tokio::test]
async fn camera_runtime_is_exclusive_fences_stale_leases_and_publishes_a_ring_frame() {
    let adapter = VirtualCameraAdapter::pattern("camera:runtime:fencing");
    let runtime = HalRuntime::builder().camera_adapter(adapter).build();
    let descriptor = runtime.enumerate_camera().await.unwrap().remove(0);
    let owner = OwnerId::parse("owner:camera-runtime").unwrap();
    let mut first = runtime
        .open_camera(owner.clone(), descriptor.selector(), request())
        .await
        .unwrap();

    let conflict = match runtime
        .open_camera(
            OwnerId::parse("owner:camera-conflict").unwrap(),
            descriptor.selector(),
            request(),
        )
        .await
    {
        Ok(_) => panic!("a second capture session must be rejected"),
        Err(error) => error,
    };
    assert_eq!(conflict.name().as_str(), "runtime.lease.conflict");

    let mapping = first.mapping_descriptor().await.unwrap();
    first.capture(Duration::ZERO).await.unwrap();
    let lease = first.next_frame_lease().await.unwrap().unwrap();
    let mut reader = seeed_hal_adapter_shared_memory::ReadOnlyMapping::open(&mapping).unwrap();
    assert_eq!(
        reader.copy(lease).unwrap().unwrap().payload().len(),
        460_800
    );

    first
        .set_control(
            CameraControlKind::Exposure,
            CameraControlValue::Integer(101),
        )
        .await
        .unwrap();
    assert_eq!(
        first
            .get_control(CameraControlKind::Exposure)
            .await
            .unwrap(),
        CameraControlValue::Integer(101)
    );

    let stale = first.lease_token().clone();
    let old_session = first.session_id();
    first.close().await.unwrap();
    let mut second = runtime
        .open_camera(owner, descriptor.selector(), request())
        .await
        .unwrap();
    assert_eq!(
        runtime
            .capture_camera(old_session, &stale, Duration::ZERO)
            .await
            .unwrap_err()
            .name()
            .as_str(),
        "runtime.lease.stale_generation"
    );
    second.close().await.unwrap();
}

#[tokio::test]
async fn camera_owner_revoke_releases_the_mapping_and_control_session() {
    let adapter = VirtualCameraAdapter::pattern("camera:runtime:revoke");
    let runtime = HalRuntime::builder().camera_adapter(adapter).build();
    let descriptor = runtime.enumerate_camera().await.unwrap().remove(0);
    let owner = OwnerId::parse("owner:camera-revoked").unwrap();
    let handle = runtime
        .open_camera(owner.clone(), descriptor.selector(), request())
        .await
        .unwrap();

    runtime.revoke_owner(&owner).await.unwrap();
    assert_eq!(
        handle
            .capture(Duration::ZERO)
            .await
            .unwrap_err()
            .name()
            .as_str(),
        "runtime.session.closed"
    );
    runtime
        .open_camera(
            OwnerId::parse("owner:camera-replacement").unwrap(),
            descriptor.selector(),
            request(),
        )
        .await
        .unwrap()
        .close()
        .await
        .unwrap();
}

#[tokio::test]
async fn camera_hot_unplug_closes_then_releases_for_reopen() {
    let adapter = VirtualCameraAdapter::pattern("camera:runtime:unplug");
    let runtime = HalRuntime::builder()
        .camera_adapter(adapter.clone())
        .build();
    let descriptor = runtime.enumerate_camera().await.unwrap().remove(0);
    let owner = OwnerId::parse("owner:camera-unplug").unwrap();
    let handle = runtime
        .open_camera(owner, descriptor.selector(), request())
        .await
        .unwrap();

    adapter.unplug_before_next_publication();
    assert_eq!(
        handle
            .capture(Duration::ZERO)
            .await
            .unwrap_err()
            .name()
            .as_str(),
        "camera.session.unplugged"
    );
    let terminal = tokio::time::timeout(Duration::from_secs(1), async {
        loop {
            let error = handle.capture(Duration::ZERO).await.unwrap_err();
            if error.name().as_str() != "runtime.session.closed" {
                return error;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("terminal worker cleanup must complete");
    assert_eq!(terminal.name().as_str(), "camera.session.unplugged");

    adapter.plug();
    let mut replacement = tokio::time::timeout(Duration::from_secs(1), async {
        loop {
            match runtime
                .open_camera(
                    OwnerId::parse("owner:camera-after-unplug").unwrap(),
                    descriptor.selector(),
                    request(),
                )
                .await
            {
                Ok(handle) => return handle,
                Err(error) if error.name().as_str() == "runtime.lease.conflict" => {
                    tokio::task::yield_now().await;
                }
                Err(error) => panic!("unexpected reopen failure: {error:?}"),
            }
        }
    })
    .await
    .unwrap();
    replacement.close().await.unwrap();
}

#[tokio::test]
async fn camera_close_invalidates_an_already_open_frame_reader() {
    let adapter = VirtualCameraAdapter::pattern("camera:runtime:reader-close");
    let runtime = HalRuntime::builder().camera_adapter(adapter).build();
    let descriptor = runtime.enumerate_camera().await.unwrap().remove(0);
    let owner = OwnerId::parse("owner:camera-reader-close").unwrap();
    let mut handle = runtime
        .open_camera(owner, descriptor.selector(), request())
        .await
        .unwrap();
    let mapping = handle.mapping_descriptor().await.unwrap();
    handle.capture(Duration::ZERO).await.unwrap();
    let lease = handle.next_frame_lease().await.unwrap().unwrap();
    let mut reader = seeed_hal_adapter_shared_memory::ReadOnlyMapping::open(&mapping).unwrap();

    handle.close().await.unwrap();

    assert!(reader.copy(lease).unwrap().is_none());
}
