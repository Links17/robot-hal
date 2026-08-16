use bytes::Bytes;
use seeed_hal_can::{
    CanAdapter, CanBitTiming, CanBusState, CanConfigureConfig, CanFrame, CanId, CanMode,
    CanOpenConfig, CanTimestamp, CanTimestampSource, ReceivedCanFrame,
};
use seeed_hal_testkit::{run_can_adapter_conformance, VirtualCanAdapter};
use std::time::Duration;

#[tokio::test]
async fn shared_conformance_helper_passes_virtual_loopback() {
    run_can_adapter_conformance(&VirtualCanAdapter::loopback("can:virtual:conformance"))
        .await
        .unwrap();
}

#[tokio::test]
async fn frames_preserve_order_and_metadata() {
    let adapter = VirtualCanAdapter::loopback("can:virtual:frames");
    let descriptor = adapter.enumerate().await.unwrap().remove(0);
    let mut channel = adapter
        .open(&descriptor.selector(), &CanOpenConfig::Attach(
            seeed_hal_can::CanLinkExpectation::new(None, None, None, None, None).unwrap(),
        ))
        .await
        .unwrap();
    let standard = CanFrame::classic_data(CanId::standard(1).unwrap(), Bytes::from_static(&[1])).unwrap();
    let extended = CanFrame::fd_data(CanId::extended(2).unwrap(), Bytes::from_static(&[2; 12]), true, true).unwrap();
    let remote = CanFrame::classic_remote(CanId::standard(3).unwrap(), 4).unwrap();
    let error = CanFrame::error(vec![seeed_hal_can::CanErrorClass::BusError], Bytes::from_static(&[9])).unwrap();
    for frame in [&standard, &extended, &remote, &error] { channel.send(frame).unwrap(); }
    for expected in [&standard, &extended, &remote, &error] {
        assert_eq!(channel.receive(Duration::from_millis(10)).unwrap().unwrap().frame(), expected);
    }
    assert_eq!(adapter.transmitted_frames(), vec![standard, extended, remote, error]);
}

#[tokio::test]
async fn injected_timestamp_and_status_are_observable() {
    let adapter = VirtualCanAdapter::loopback("can:virtual:hooks");
    let descriptor = adapter.enumerate().await.unwrap().remove(0);
    let mut channel = adapter.open(&descriptor.selector(), &CanOpenConfig::Attach(
        seeed_hal_can::CanLinkExpectation::new(None, None, None, None, None).unwrap(),
    )).await.unwrap();
    let frame = CanFrame::classic_remote(CanId::standard(7).unwrap(), 2).unwrap();
    let timestamp = CanTimestamp::new(42, CanTimestampSource::Hardware, "clock-a").unwrap();
    adapter.inject_received(frame.clone(), Some(timestamp.clone())).unwrap();
    let received = channel.receive(Duration::from_millis(10)).unwrap().unwrap();
    assert_eq!(received, ReceivedCanFrame::new(frame, Some(timestamp)));
    adapter.set_bus_status(seeed_hal_can::CanBusStatus::new(CanBusState::Warning, Some(3), Some(4)));
    assert_eq!(channel.bus_status().unwrap().state(), CanBusState::Warning);
}

#[tokio::test]
async fn configure_is_exclusive_and_restored_on_close() {
    let adapter = VirtualCanAdapter::loopback("can:virtual:configure");
    let descriptor = adapter.enumerate().await.unwrap().remove(0);
    let nominal = CanBitTiming::new(250_000, None, None).unwrap();
    let data = CanBitTiming::new(1_000_000, None, None).unwrap();
    let request = CanConfigureConfig::new(CanMode::Fd, nominal, Some(data), false, false).unwrap();
    let mut channel = adapter.open(&descriptor.selector(), &CanOpenConfig::Configure(request)).await.unwrap();
    assert_eq!(channel.active_config().mode(), CanMode::Fd);
    let conflict = match adapter.open(&descriptor.selector(), &CanOpenConfig::Configure(
        CanConfigureConfig::new(CanMode::Classic, nominal, None, false, true).unwrap(),
    )).await { Ok(_) => panic!("concurrent Configure must fail"), Err(error) => error };
    assert_eq!(conflict.name().as_str(), "runtime.adapter.conflict");
    channel.close().unwrap();
    let attached = adapter.open(&descriptor.selector(), &CanOpenConfig::Attach(
        seeed_hal_can::CanLinkExpectation::new(Some(CanMode::Classic), Some(500_000), None, Some(false), Some(true)).unwrap(),
    )).await.unwrap();
    drop(attached);
}

#[tokio::test]
async fn receive_timeout_and_close_are_finite_and_structured() {
    let adapter = VirtualCanAdapter::loopback("can:virtual:timeout");
    let descriptor = adapter.enumerate().await.unwrap().remove(0);
    let mut channel = adapter.open(&descriptor.selector(), &CanOpenConfig::Attach(
        seeed_hal_can::CanLinkExpectation::new(None, None, None, None, None).unwrap(),
    )).await.unwrap();
    assert!(channel.receive(Duration::from_millis(1)).unwrap().is_none());
    channel.close().unwrap();
    assert_eq!(channel.send(&CanFrame::classic_remote(CanId::standard(1).unwrap(), 0).unwrap()).unwrap_err().name().as_str(), "runtime.session.closed");
}

#[tokio::test]
async fn descriptor_is_strong_can_and_advertises_exact_capabilities() {
    let adapter = VirtualCanAdapter::loopback("can:virtual:capabilities");
    let descriptor = adapter.enumerate().await.unwrap().remove(0);
    assert_eq!(descriptor.transport(), seeed_hal_can::TransportKind::Can);
    assert_eq!(descriptor.minimum_identity_quality(), seeed_hal_can::IdentityQuality::Strong);
    for capability in [
        seeed_hal_can::can_classic_capability(),
        seeed_hal_can::can_fd_capability(),
        seeed_hal_can::can_configure_capability(),
        seeed_hal_can::can_error_frames_capability(),
        seeed_hal_can::can_rx_timestamp_capability(),
    ] {
        assert!(descriptor.capabilities().contains(&capability));
    }
}

#[tokio::test]
async fn fault_hooks_are_one_shot_and_transition_waits_are_bounded() {
    let adapter = VirtualCanAdapter::loopback("can:virtual:faults");
    assert!(!adapter.wait_for_open(Duration::from_millis(1)));
    let descriptor = adapter.enumerate().await.unwrap().remove(0);
    let mut channel = adapter.open(&descriptor.selector(), &CanOpenConfig::Attach(
        seeed_hal_can::CanLinkExpectation::new(None, None, None, None, None).unwrap(),
    )).await.unwrap();
    assert!(adapter.wait_for_open(Duration::from_millis(10)));
    adapter.fail_next_status(seeed_hal_core::HalError::new(
        "can.status.injected", seeed_hal_core::ErrorCategory::Unavailable,
        "test.status", true, "injected",
    ).unwrap());
    assert_eq!(channel.bus_status().unwrap_err().name().as_str(), "can.status.injected");
    assert_eq!(channel.bus_status().unwrap().state(), CanBusState::Active);
    channel.close().unwrap();
    assert!(adapter.wait_for_close(Duration::from_millis(10)));
}
