use robot_hal_camera::{
    CameraAdapter, CameraControlKind, CameraControlValue, CameraFormat, CameraFrame,
    CameraFrameMetadata, CameraPixelFormat, CameraPlaneLayout, CameraRequest,
    DEFAULT_CAMERA_SLOT_COUNT, MAX_CAMERA_FRAME_BYTES, MAX_CAMERA_HEIGHT, MAX_CAMERA_WIDTH,
};
use robot_hal_core::{ErrorCategory, HalError};
use robot_hal_testkit::{VirtualCameraAdapter, run_camera_adapter_conformance};
use std::time::Duration;

#[tokio::test]
async fn virtual_camera_passes_public_adapter_conformance() {
    let adapter = VirtualCameraAdapter::pattern("virtual-camera");

    run_camera_adapter_conformance(&adapter)
        .await
        .expect("virtual camera must satisfy the public conformance contract");
}

#[tokio::test]
async fn capture_only_camera_passes_public_adapter_conformance() {
    let adapter = VirtualCameraAdapter::capture_only("capture-only-camera");
    let descriptor = adapter
        .enumerate()
        .await
        .expect("enumeration succeeds")
        .pop()
        .expect("capture-only camera is present");

    assert!(
        descriptor
            .capabilities()
            .contains(&robot_hal_camera::camera_capture_capability())
    );
    assert!(
        descriptor
            .capabilities()
            .contains(&robot_hal_camera::camera_frames_shm_capability())
    );
    assert!(
        !descriptor
            .capabilities()
            .contains(&robot_hal_camera::camera_controls_capability())
    );

    run_camera_adapter_conformance(&adapter)
        .await
        .expect("a capture-only camera must satisfy the public conformance contract");
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

#[test]
fn camera_frames_reject_invalid_plane_count_overlap_and_bounds() {
    let nv12 = CameraFormat::new(CameraPixelFormat::Nv12, 640, 480).unwrap();
    let yuyv = CameraFormat::new(CameraPixelFormat::Yuyv, 320, 240).unwrap();
    let y_plane = CameraPlaneLayout::new(0, 640 * 480, 640).unwrap();
    let uv_plane = CameraPlaneLayout::new(640 * 480, 640 * 240, 640).unwrap();

    let wrong_nv12_count =
        CameraFrameMetadata::new(nv12.clone(), vec![y_plane], 1, 1, "monotonic", 0)
            .expect_err("NV12 requires exactly two planes");
    assert_eq!(wrong_nv12_count.name().as_str(), "camera.frame.invalid");

    let wrong_yuyv_count = CameraFrameMetadata::new(
        yuyv.clone(),
        vec![
            CameraPlaneLayout::new(0, 320 * 240 * 2, 640).unwrap(),
            CameraPlaneLayout::new(320 * 240 * 2, 1, 1).unwrap(),
        ],
        1,
        1,
        "monotonic",
        0,
    )
    .expect_err("YUYV requires exactly one plane");
    assert_eq!(wrong_yuyv_count.name().as_str(), "camera.frame.invalid");

    let overlapping = CameraFrameMetadata::new(
        nv12,
        vec![
            y_plane,
            CameraPlaneLayout::new(640 * 480 - 1, 640 * 240, 640).unwrap(),
        ],
        1,
        1,
        "monotonic",
        0,
    )
    .expect_err("camera planes must not overlap");
    assert_eq!(overlapping.name().as_str(), "camera.frame.invalid");

    let valid_metadata = CameraFrameMetadata::new(
        yuyv,
        vec![CameraPlaneLayout::new(0, 320 * 240 * 2, 640).unwrap()],
        1,
        1,
        "monotonic",
        0,
    )
    .unwrap();
    let out_of_bounds = CameraFrame::new(valid_metadata, bytes::Bytes::from(vec![0; 1]))
        .expect_err("camera planes must remain within the payload");
    assert_eq!(out_of_bounds.name().as_str(), "camera.frame.invalid");

    let overflow = CameraPlaneLayout::new(usize::MAX, 1, 1)
        .expect_err("plane range arithmetic must not overflow");
    assert_eq!(overflow.name().as_str(), "camera.frame.invalid");

    let valid_nv12 = CameraFrameMetadata::new(
        CameraFormat::new(CameraPixelFormat::Nv12, 640, 480).unwrap(),
        vec![y_plane, uv_plane],
        1,
        1,
        "virtual-camera",
        0,
    )
    .expect("the virtual adapter NV12 layout remains valid");
    CameraFrame::new(valid_nv12, bytes::Bytes::from(vec![0; 640 * 480 * 3 / 2]))
        .expect("the virtual adapter NV12 payload remains valid");
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

#[tokio::test]
async fn virtual_camera_does_not_publish_after_unplug_between_validation_and_publication() {
    let adapter = VirtualCameraAdapter::pattern("virtual-camera-race");
    let descriptor = adapter.enumerate().await.unwrap().pop().unwrap();
    let request = CameraRequest::new(
        CameraFormat::new(CameraPixelFormat::Nv12, 640, 480).unwrap(),
        DEFAULT_CAMERA_SLOT_COUNT,
    )
    .unwrap();
    let mut session = adapter
        .open(&descriptor.selector(), &request)
        .await
        .unwrap();

    adapter.unplug_before_next_publication();
    let error = session
        .capture(Duration::ZERO)
        .await
        .expect_err("capture must fail closed when unplugged before frame publication");
    assert_eq!(error.name().as_str(), "camera.session.unplugged");

    let control_adapter = VirtualCameraAdapter::pattern("virtual-camera-control-race");
    let control_descriptor = control_adapter.enumerate().await.unwrap().pop().unwrap();
    let mut control_session = control_adapter
        .open(&control_descriptor.selector(), &request)
        .await
        .unwrap();
    control_adapter.unplug_before_next_publication();
    let error = control_session
        .controls()
        .await
        .expect_err("controls must fail closed when unplugged before publication");
    assert_eq!(error.name().as_str(), "camera.session.unplugged");
}
