use seeed_hal_camera::{
    CameraAdapter, CameraControlKind, CameraControlValue, CameraFormat, CameraPixelFormat,
    CameraRequest, DEFAULT_CAMERA_SLOT_COUNT, MAX_CAMERA_FRAME_BYTES, MAX_CAMERA_HEIGHT,
    MAX_CAMERA_WIDTH,
};
use seeed_hal_core::{ErrorCategory, HalError};
use seeed_hal_testkit::{VirtualCameraAdapter, run_camera_adapter_conformance};
use std::time::Duration;

#[tokio::test]
async fn virtual_camera_passes_public_adapter_conformance() {
    let adapter = VirtualCameraAdapter::pattern("virtual-camera");

    run_camera_adapter_conformance(&adapter)
        .await
        .expect("virtual camera must satisfy the public conformance contract");
}

#[test]
fn camera_format_and_request_enforce_public_bounds() {
    let format = CameraFormat::new(CameraPixelFormat::Nv12, 640, 480)
        .expect("a bounded NV12 format must be valid");
    assert_eq!(format.pixel_format(), CameraPixelFormat::Nv12);
    assert_eq!(format.width(), 640);
    assert_eq!(format.height(), 480);

    let request = CameraRequest::new(format.clone(), DEFAULT_CAMERA_SLOT_COUNT)
        .expect("the default slot capacity must be valid");
    assert_eq!(request.slot_count(), DEFAULT_CAMERA_SLOT_COUNT);

    let dimensions = CameraFormat::new(
        CameraPixelFormat::Yuyv,
        MAX_CAMERA_WIDTH + 1,
        MAX_CAMERA_HEIGHT,
    )
    .expect_err("dimensions above the public maximum must fail");
    assert_eq!(dimensions.name().as_str(), "camera.format.invalid");

    assert_eq!(MAX_CAMERA_FRAME_BYTES, 24 * 1024 * 1024);

    let slots = CameraRequest::new(format, DEFAULT_CAMERA_SLOT_COUNT - 1)
        .expect_err("fewer than four slots must fail");
    assert_eq!(slots.name().as_str(), "camera.request.invalid");
}

#[tokio::test]
async fn virtual_camera_negotiates_frames_and_enforces_exclusive_open() {
    let adapter = VirtualCameraAdapter::pattern("virtual-camera");
    let descriptor = adapter
        .enumerate()
        .await
        .expect("enumeration succeeds")
        .pop()
        .expect("virtual camera is present");
    let request = CameraRequest::new(
        CameraFormat::new(CameraPixelFormat::Yuyv, 320, 240).unwrap(),
        4,
    )
    .unwrap();
    let mut session = adapter
        .open(&descriptor.selector(), &request)
        .await
        .expect("a supported format opens");

    let frame = session
        .capture(Duration::ZERO)
        .await
        .expect("pattern frame is available");
    assert_eq!(frame.metadata().format(), request.format());
    assert_eq!(frame.metadata().sequence(), 1);
    assert_eq!(frame.metadata().monotonic_timestamp_ns(), 1);
    assert_eq!(frame.metadata().clock_domain(), "virtual-camera");
    assert_eq!(frame.payload()[0], 0);

    let exclusive = match adapter.open(&descriptor.selector(), &request).await {
        Ok(_) => panic!("a camera may have only one active capture session"),
        Err(error) => error,
    };
    assert_eq!(exclusive.name().as_str(), "runtime.adapter.conflict");
    assert_eq!(exclusive.category(), ErrorCategory::Conflict);

    session.close().await.expect("close succeeds");
    let closed = session
        .capture(Duration::ZERO)
        .await
        .expect_err("a closed session rejects capture");
    assert_eq!(closed.name().as_str(), "runtime.session.closed");
}

#[tokio::test]
async fn virtual_camera_controls_faults_and_hot_unplug_are_deterministic() {
    let adapter = VirtualCameraAdapter::pattern("virtual-camera");
    let descriptor = adapter.enumerate().await.unwrap().pop().unwrap();
    let request = CameraRequest::new(
        CameraFormat::new(CameraPixelFormat::Nv12, 640, 480).unwrap(),
        4,
    )
    .unwrap();
    let mut session = adapter
        .open(&descriptor.selector(), &request)
        .await
        .unwrap();

    let descriptors = session.controls().await.expect("controls enumerate");
    assert_eq!(
        descriptors
            .iter()
            .map(|descriptor| descriptor.kind())
            .collect::<Vec<_>>(),
        vec![
            CameraControlKind::Exposure,
            CameraControlKind::Gain,
            CameraControlKind::WhiteBalance,
            CameraControlKind::Focus,
        ]
    );
    session
        .set_control(
            CameraControlKind::Exposure,
            CameraControlValue::Integer(120),
        )
        .await
        .expect("exposure accepts a value in its range");
    assert_eq!(
        session
            .get_control(CameraControlKind::Exposure)
            .await
            .expect("exposure is readable"),
        CameraControlValue::Integer(120)
    );
    session
        .set_auto(CameraControlKind::Focus, true)
        .await
        .expect("focus supports auto mode");

    adapter.fail_next_capture(
        HalError::new(
            "runtime.transport.timeout",
            ErrorCategory::Unavailable,
            "camera.capture",
            true,
            "injected capture failure",
        )
        .unwrap(),
    );
    assert_eq!(
        session
            .capture(Duration::ZERO)
            .await
            .expect_err("one-shot capture failure is returned")
            .name()
            .as_str(),
        "runtime.transport.timeout"
    );
    assert_eq!(
        session
            .capture(Duration::ZERO)
            .await
            .unwrap()
            .metadata()
            .sequence(),
        1,
        "a failed capture does not advance virtual frame sequence"
    );

    adapter.unplug();
    assert!(adapter.enumerate().await.unwrap().is_empty());
    let unplugged = session
        .capture(Duration::ZERO)
        .await
        .expect_err("hot-unplug retires the active session");
    assert_eq!(unplugged.name().as_str(), "camera.session.unplugged");
}
