#![cfg(any(target_os = "windows", target_os = "linux"))]

use std::time::{Duration, Instant};

use async_trait::async_trait;
use seeed_hal_adapter_pcan::PcanAdapter;
use seeed_hal_can::{
    CanAdapter, CanBitTiming, CanChannel, CanConfigureConfig, CanFrame, CanId,
    CanMode, CanOpenConfig, ResourceDescriptor, ResourceSelector,
};
use seeed_hal_core::HalResult;

async fn selected_descriptor(adapter: &PcanAdapter) -> ResourceDescriptor {
    let resource_id = std::env::var("SEEED_HAL_PCAN_RESOURCE_ID")
        .expect("set SEEED_HAL_PCAN_RESOURCE_ID to the provisioned PCAN resource");
    adapter
        .enumerate()
        .await
        .expect("enumerate PCAN channels")
        .into_iter()
        .find(|descriptor| descriptor.id().as_str() == resource_id)
        .unwrap_or_else(|| panic!("PCAN resource {resource_id} is present"))
}

fn configured(mode: CanMode) -> CanOpenConfig {
    let nominal = std::env::var("SEEED_HAL_PCAN_NOMINAL_BITRATE")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(500_000);
    let data = std::env::var("SEEED_HAL_PCAN_DATA_BITRATE")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(2_000_000);
    CanOpenConfig::Configure(
        CanConfigureConfig::new(
            mode,
            CanBitTiming::new(nominal, None, None).expect("valid nominal timing"),
            (mode == CanMode::Fd)
                .then(|| CanBitTiming::new(data, None, None).expect("valid data timing")),
            false,
            false,
        )
        .expect("valid PCAN configuration"),
    )
}

#[tokio::test]
#[ignore = "requires PCAN-Basic, a selected adapter, and an external Classical CAN loopback peer"]
async fn selected_classic_channel_preserves_standard_and_remote_frames() {
    let adapter = PcanAdapter::load().expect("load PCAN-Basic");
    let descriptor = selected_descriptor(&adapter).await;
    let mut channel = adapter
        .open(&descriptor.selector(), &configured(CanMode::Classic))
        .await
        .expect("open selected Classical PCAN channel");
    let frames = [
        CanFrame::classic_data(CanId::standard(0x321).unwrap(), vec![1, 2, 3]).unwrap(),
        CanFrame::classic_remote(CanId::extended(0x18da_00f1).unwrap(), 8).unwrap(),
    ];

    for expected in frames {
        channel.send(&expected).expect("send loopback frame");
        let received = channel
            .receive(Duration::from_secs(2))
            .expect("receive loopback frame")
            .expect("loopback frame before deadline");
        assert_eq!(received.frame(), &expected);
    }
    channel.close().expect("uninitialize PCAN channel");
}

#[tokio::test]
#[ignore = "requires PCAN-Basic, an FD-capable selected adapter, and an external CAN FD loopback peer"]
async fn selected_fd_channel_preserves_brs_esi_and_payload() {
    let adapter = PcanAdapter::load().expect("load PCAN-Basic");
    let descriptor = selected_descriptor(&adapter).await;
    let mut channel = adapter
        .open(&descriptor.selector(), &configured(CanMode::Fd))
        .await
        .expect("open selected FD PCAN channel");
    let expected = CanFrame::fd_data(
        CanId::extended(0x12345).unwrap(),
        vec![0x5a; 12],
        true,
        true,
    )
    .unwrap();

    channel.send(&expected).expect("send FD loopback frame");
    let received = channel
        .receive(Duration::from_secs(2))
        .expect("receive FD loopback frame")
        .expect("FD loopback frame before deadline");
    assert_eq!(received.frame(), &expected);
    channel.close().expect("uninitialize PCAN channel");
}

#[tokio::test]
#[ignore = "requires an operator-controlled PCAN disconnect selected by SEEED_HAL_PCAN_RESOURCE_ID"]
async fn selected_channel_reports_disconnect_with_canonical_identity() {
    let adapter = PcanAdapter::load().expect("load PCAN-Basic");
    let descriptor = selected_descriptor(&adapter).await;
    let mut channel = adapter
        .open(&descriptor.selector(), &configured(CanMode::Classic))
        .await
        .expect("open selected PCAN channel before disconnect");
    let deadline = Instant::now() + Duration::from_secs(30);

    loop {
        match channel.bus_status() {
            Err(error) => {
                assert_eq!(error.resource_id(), Some(descriptor.id()));
                assert!(error.vendor_code().is_some());
                break;
            }
            Ok(_) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(50)),
            Ok(_) => panic!("disconnect was not observed before the operator deadline"),
        }
    }
}

#[tokio::test]
#[ignore = "requires a terminated selected PCAN bus that is intentionally driven bus-off"]
async fn selected_channel_reports_bus_off() {
    let adapter = PcanAdapter::load().expect("load PCAN-Basic");
    let descriptor = selected_descriptor(&adapter).await;
    let mut channel = adapter
        .open(&descriptor.selector(), &configured(CanMode::Classic))
        .await
        .expect("open selected PCAN channel");
    let frame = CanFrame::classic_data(CanId::standard(0x123).unwrap(), vec![0x55]).unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);

    loop {
        match channel.send(&frame) {
            Err(error) if error.name().as_str() == "can.bus.off" => {
                assert_eq!(error.resource_id(), Some(descriptor.id()));
                assert!(error.vendor_code().is_some());
                break;
            }
            Err(error) if error.name().as_str() == "runtime.queue.full" => {}
            Err(error) => panic!("unexpected PCAN send failure before bus-off: {error}"),
            Ok(()) => {}
        }
        if Instant::now() >= deadline {
            panic!("PCAN channel did not enter bus-off before the deadline");
        }
    }
}

struct SelectedPcanAdapter {
    resource_id: String,
    inner: PcanAdapter,
}

#[async_trait]
impl CanAdapter for SelectedPcanAdapter {
    fn adapter_name(&self) -> &'static str {
        "pcan.selected"
    }

    async fn enumerate(&self) -> HalResult<Vec<ResourceDescriptor>> {
        Ok(self
            .inner
            .enumerate()
            .await?
            .into_iter()
            .filter(|descriptor| descriptor.id().as_str() == self.resource_id)
            .collect())
    }

    async fn open(
        &self,
        selector: &ResourceSelector,
        config: &CanOpenConfig,
    ) -> HalResult<Box<dyn CanChannel>> {
        self.inner.open(selector, config).await
    }
}

#[tokio::test]
#[ignore = "requires a preconfigured selected PCAN channel and an external loopback peer"]
async fn selected_preconfigured_channel_passes_shared_conformance() {
    let adapter = SelectedPcanAdapter {
        resource_id: std::env::var("SEEED_HAL_PCAN_RESOURCE_ID")
            .expect("set SEEED_HAL_PCAN_RESOURCE_ID"),
        inner: PcanAdapter::load().expect("load PCAN-Basic"),
    };

    seeed_hal_testkit::run_can_adapter_conformance(&adapter)
        .await
        .expect("selected PCAN channel passes capability-gated conformance");
}
