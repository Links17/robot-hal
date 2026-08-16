#![cfg(target_os = "linux")]

use std::time::Duration;

use seeed_hal_adapter_socketcan::SocketCanAdapter;
use seeed_hal_can::{
    can_configure_capability, can_fd_capability, CanAdapter, CanBitTiming, CanConfigureConfig,
    CanFrame, CanId, CanLinkExpectation, CanMode, CanOpenConfig,
};
use seeed_hal_core::ResourceSelector;

/// Native vcan coverage is opt-in because it requires CAP_NET_ADMIN and a
/// kernel vcan device. The test body documents the adapter boundary without
/// making the default workspace suite depend on host networking privileges.
#[tokio::test]
#[ignore = "requires a provisioned Linux vcan interface and CAP_NET_ADMIN"]
async fn vcan_classic_attach_loopback_and_close() {
    let adapter = SocketCanAdapter::new();
    let descriptors = adapter.enumerate().await.unwrap();
    let descriptor = descriptors
        .iter()
        .find(|descriptor| descriptor.endpoint().as_str().starts_with("vcan"))
        .expect("provisioned vcan interface");
    let selector: ResourceSelector = descriptor.selector();
    let mut channel = adapter
        .open(
            &selector,
            &CanOpenConfig::Attach(
                CanLinkExpectation::new(None, None, None, None, None).unwrap(),
            ),
        )
        .await
        .unwrap();

    let frame =
        CanFrame::classic_data(CanId::standard(0x123).unwrap(), [1, 2, 3]).unwrap();
    channel.send(&frame).unwrap();
    let _ = channel.receive(Duration::from_millis(25));
    let _ = channel.bus_status().unwrap();
    channel.close().unwrap();
}

#[tokio::test]
#[ignore = "requires a provisioned Linux vcan FD interface and CAP_NET_ADMIN"]
async fn vcan_fd_and_configure_restore_are_opt_in() {
    let adapter = SocketCanAdapter::new();
    let descriptors = adapter.enumerate().await.unwrap();
    let Some(descriptor) = descriptors.iter().find(|descriptor| {
        descriptor.endpoint().as_str().starts_with("vcan")
            && descriptor.capabilities().contains(&can_fd_capability())
    }) else {
        return;
    };
    let mut channel = adapter
        .open(
            &descriptor.selector(),
            &CanOpenConfig::Attach(
                CanLinkExpectation::new(Some(CanMode::Fd), None, None, None, None).unwrap(),
            ),
        )
        .await
        .unwrap();
    let frame = CanFrame::fd_data(
        CanId::extended(0x12345).unwrap(),
        [0x5a; 12],
        false,
        false,
    )
    .unwrap();
    channel.send(&frame).unwrap();
    let _ = channel.receive(Duration::from_millis(25));
    channel.close().unwrap();
}

#[tokio::test]
#[ignore = "requires a provisioned Linux CAN interface and CAP_NET_ADMIN"]
async fn native_configure_permission_or_unsupported_errors_are_resource_scoped() {
    let adapter = SocketCanAdapter::new();
    let descriptors = adapter.enumerate().await.unwrap();
    let Some(descriptor) = descriptors.iter().find(|descriptor| {
        descriptor.capabilities().contains(&can_configure_capability())
    }) else {
        return;
    };
    let nominal = CanBitTiming::new(500_000, None, None).unwrap();
    let request = CanConfigureConfig::new(CanMode::Classic, nominal, None, false, false).unwrap();
    match adapter
        .open(&descriptor.selector(), &CanOpenConfig::Configure(request))
        .await
    {
        Ok(mut channel) => {
            let _ = channel.close();
        }
        Err(error) => assert_eq!(error.resource_id(), Some(descriptor.id())),
    }
}

#[tokio::test]
#[ignore = "requires a provisioned Linux vcan interface and CAP_NET_ADMIN"]
async fn vcan_passes_shared_can_conformance() {
    let adapter = SocketCanAdapter::new();
    seeed_hal_testkit::run_can_adapter_conformance(&adapter)
        .await
        .unwrap();
}
