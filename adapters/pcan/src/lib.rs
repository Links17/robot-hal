#![deny(unsafe_op_in_unsafe_fn)]

pub mod identity;

#[cfg(any(target_os = "windows", target_os = "linux"))]
mod channel;

pub use identity::{PcanChannelMetadata, PcanIdentity, identity_from_metadata};

use async_trait::async_trait;
use seeed_hal_can::{CanAdapter, CanChannel, CanOpenConfig};
use seeed_hal_core::{
    ErrorCategory, HalError, HalResult, ResourceDescriptor, ResourceSelector,
};

#[cfg(any(target_os = "windows", target_os = "linux"))]
use std::sync::Arc;

#[cfg(any(target_os = "windows", target_os = "linux"))]
use seeed_hal_can::{
    can_classic_capability, can_configure_capability, can_fd_capability,
    can_rx_timestamp_capability,
};
#[cfg(any(target_os = "windows", target_os = "linux"))]
use seeed_hal_core::{
    CapabilitySet, Endpoint, ResourceProperties, TransportKind, resolve_resource,
};

#[cfg(any(target_os = "windows", target_os = "linux"))]
#[derive(Clone)]
pub struct PcanAdapter {
    driver: Arc<dyn channel::Driver>,
}

#[cfg(not(any(target_os = "windows", target_os = "linux")))]
#[derive(Clone, Debug)]
pub struct PcanAdapter {
    _private: (),
}

impl PcanAdapter {
    pub fn load() -> HalResult<Self> {
        #[cfg(any(target_os = "windows", target_os = "linux"))]
        {
            let driver = channel::RealDriver::load()
                .map_err(|error| map_load_error(error, "can.adapter.load"))?;
            Ok(Self {
                driver: Arc::new(driver),
            })
        }
        #[cfg(not(any(target_os = "windows", target_os = "linux")))]
        {
            Err(unavailable(
                "can.adapter.load",
                "PCAN-Basic is supported only on Windows and Linux",
            ))
        }
    }

    #[cfg(all(test, any(target_os = "windows", target_os = "linux")))]
    fn with_driver(driver: Arc<dyn channel::Driver>) -> Self {
        Self { driver }
    }
}

#[cfg(any(target_os = "windows", target_os = "linux"))]
#[async_trait]
impl CanAdapter for PcanAdapter {
    fn adapter_name(&self) -> &'static str {
        "pcan"
    }

    async fn enumerate(&self) -> HalResult<Vec<ResourceDescriptor>> {
        let driver = Arc::clone(&self.driver);
        tokio::task::spawn_blocking(move || enumerate_sync(driver.as_ref()))
            .await
            .map_err(|error| worker_error("can.enumerate", error))?
    }

    async fn open(
        &self,
        selector: &ResourceSelector,
        config: &CanOpenConfig,
    ) -> HalResult<Box<dyn CanChannel>> {
        let resource_id = selector.id().clone();
        let selector = selector.clone();
        let config = config.clone();
        let driver = Arc::clone(&self.driver);
        tokio::task::spawn_blocking(move || open_sync(driver.as_ref(), &selector, &config))
            .await
            .map_err(|error| worker_error("can.open", error).with_resource_id(resource_id))?
    }
}

#[cfg(not(any(target_os = "windows", target_os = "linux")))]
#[async_trait]
impl CanAdapter for PcanAdapter {
    fn adapter_name(&self) -> &'static str {
        "pcan"
    }

    async fn enumerate(&self) -> HalResult<Vec<ResourceDescriptor>> {
        Err(unavailable(
            "can.enumerate",
            "PCAN-Basic is supported only on Windows and Linux",
        ))
    }

    async fn open(
        &self,
        selector: &ResourceSelector,
        _config: &CanOpenConfig,
    ) -> HalResult<Box<dyn CanChannel>> {
        Err(unavailable(
            "can.open",
            "PCAN-Basic is supported only on Windows and Linux",
        )
        .with_resource_id(selector.id().clone()))
    }
}

#[cfg(any(target_os = "windows", target_os = "linux"))]
fn enumerate_sync(driver: &dyn channel::Driver) -> HalResult<Vec<ResourceDescriptor>> {
    driver
        .discover()
        .map_err(|error| channel::map_driver_error("can.enumerate", error, None))?
        .into_iter()
        .map(descriptor_from_device)
        .collect()
}

#[cfg(any(target_os = "windows", target_os = "linux"))]
fn descriptor_from_device(device: channel::DriverDevice) -> HalResult<ResourceDescriptor> {
    let metadata = PcanChannelMetadata {
        handle: device.handle,
        device_type: device.device_type,
        controller_number: device.controller_number,
        device_name: device.device_name.clone(),
        device_id: device.device_id,
    };
    let identity = identity_from_metadata(&metadata)?;
    let mut capabilities = vec![
        can_classic_capability(),
        can_configure_capability(),
        can_rx_timestamp_capability(),
    ];
    if device.fd_capable {
        capabilities.push(can_fd_capability());
    }
    let mut properties = std::collections::BTreeMap::new();
    properties.insert("adapter".to_owned(), "pcan".to_owned());
    properties.insert("handle".to_owned(), format!("0x{:04X}", device.handle));
    properties.insert("device_type".to_owned(), format!("0x{:02X}", device.device_type));
    properties.insert(
        "controller_number".to_owned(),
        device.controller_number.to_string(),
    );
    properties.insert("fd_capable".to_owned(), device.fd_capable.to_string());
    properties.insert(
        "channel_condition".to_owned(),
        format!("0x{:08X}", device.channel_condition),
    );
    if let Some(name) = device.device_name {
        properties.insert("hardware_name".to_owned(), name);
    }
    if let Some(id) = device.device_id {
        properties.insert("vendor_device_id".to_owned(), format!("0x{id:08X}"));
    }

    Ok(ResourceDescriptor::new(
        identity.id,
        Endpoint::new(format!("pcan://0x{:04X}", device.handle))?,
        identity.quality,
        TransportKind::Can,
        ResourceProperties::new(properties),
        CapabilitySet::new(capabilities),
    ))
}

#[cfg(any(target_os = "windows", target_os = "linux"))]
fn open_sync(
    driver: &dyn channel::Driver,
    selector: &ResourceSelector,
    config: &CanOpenConfig,
) -> HalResult<Box<dyn CanChannel>> {
    let devices = driver
        .discover()
        .map_err(|error| channel::map_driver_error("can.open", error, Some(selector.id())))?;
    let mut paired = Vec::with_capacity(devices.len());
    for device in devices {
        paired.push((descriptor_from_device(device.clone())?, device));
    }
    let descriptors: Vec<_> = paired.iter().map(|(descriptor, _)| descriptor.clone()).collect();
    let descriptor = resolve_resource(
        &descriptors,
        selector,
        &can_classic_capability(),
        "can.open",
    )?
    .clone();
    let device = paired
        .into_iter()
        .find(|(candidate, _)| candidate.endpoint() == descriptor.endpoint())
        .map(|(_, device)| device)
        .expect("resolved descriptor came from paired PCAN discovery");
    let (backend, active) = driver
        .open(&device, config)
        .map_err(|error| channel::map_driver_error("can.open", error, Some(descriptor.id())))?;
    Ok(Box::new(channel::NativePcanChannel::new(
        descriptor, active, backend,
    )))
}

#[cfg(any(target_os = "windows", target_os = "linux"))]
fn map_load_error(error: channel::DriverError, operation: &'static str) -> HalError {
    channel::map_driver_error(operation, error, None)
}

#[cfg(not(any(target_os = "windows", target_os = "linux")))]
fn unavailable(operation: &'static str, message: impl Into<String>) -> HalError {
    HalError::new(
        "can.adapter.unavailable",
        ErrorCategory::Unavailable,
        operation,
        false,
        message,
    )
    .expect("static PCAN unavailable error metadata is valid")
}

#[cfg(any(target_os = "windows", target_os = "linux"))]
fn worker_error(operation: &'static str, error: tokio::task::JoinError) -> HalError {
    HalError::new(
        "runtime.internal.worker_failed",
        ErrorCategory::Internal,
        operation,
        false,
        format!("PCAN blocking worker failed: {error}"),
    )
    .expect("static PCAN worker error metadata is valid")
}

#[cfg(all(test, any(target_os = "windows", target_os = "linux")))]
mod tests {
    use std::collections::VecDeque;
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use seeed_hal_can::{
        CanActiveConfig, CanBitTiming, CanBusState, CanBusStatus, CanConfigureConfig,
        CanFrame, CanId, CanMode, CanTimestamp, CanTimestampSource,
        ReceivedCanFrame, can_error_frames_capability,
    };

    use super::*;
    use crate::channel::{Driver, DriverChannel, DriverDevice, DriverError};

    struct FakeDriver {
        devices: Result<Vec<DriverDevice>, String>,
        state: Arc<Mutex<FakeState>>,
    }

    struct FakeState {
        received: VecDeque<ReceivedCanFrame>,
        sent: Vec<CanFrame>,
        receive_timeouts: Vec<Duration>,
        close_count: usize,
        fail_after_sends: Option<usize>,
        status: CanBusStatus,
    }

    impl Driver for FakeDriver {
        fn discover(&self) -> Result<Vec<DriverDevice>, DriverError> {
            self.devices
                .clone()
                .map_err(DriverError::Unavailable)
        }

        fn open(
            &self,
            _device: &DriverDevice,
            config: &CanOpenConfig,
        ) -> Result<(Box<dyn DriverChannel>, CanActiveConfig), DriverError> {
            let CanOpenConfig::Configure(request) = config else {
                return Err(DriverError::Unsupported(
                    "fake requires Configure".to_owned(),
                ));
            };
            let active = CanActiveConfig::new(
                request.mode(),
                *request.nominal(),
                request.data().copied(),
                request.listen_only(),
                request.loopback(),
                "pcan-fake",
            )
            .map_err(|error| DriverError::Platform(error.to_string()))?;
            Ok((
                Box::new(FakeChannel {
                    state: Arc::clone(&self.state),
                    closed: false,
                }),
                active,
            ))
        }
    }

    struct FakeChannel {
        state: Arc<Mutex<FakeState>>,
        closed: bool,
    }

    impl DriverChannel for FakeChannel {
        fn receive(
            &mut self,
            timeout: Duration,
        ) -> Result<Option<ReceivedCanFrame>, DriverError> {
            let mut state = self.state.lock().expect("fake PCAN mutex poisoned");
            state.receive_timeouts.push(timeout);
            Ok(state.received.pop_front())
        }

        fn send(&mut self, frame: &CanFrame) -> Result<(), DriverError> {
            let mut state = self.state.lock().expect("fake PCAN mutex poisoned");
            if state
                .fail_after_sends
                .is_some_and(|limit| state.sent.len() >= limit)
            {
                return Err(DriverError::Status(0x00080));
            }
            state.sent.push(frame.clone());
            Ok(())
        }

        fn bus_status(&mut self) -> Result<CanBusStatus, DriverError> {
            Ok(self
                .state
                .lock()
                .expect("fake PCAN mutex poisoned")
                .status
                .clone())
        }

        fn close(&mut self) -> Result<(), DriverError> {
            if !self.closed {
                self.closed = true;
                self.state
                    .lock()
                    .expect("fake PCAN mutex poisoned")
                    .close_count += 1;
            }
            Ok(())
        }
    }

    fn fake_adapter(
        devices: Result<Vec<DriverDevice>, String>,
        state: Arc<Mutex<FakeState>>,
    ) -> PcanAdapter {
        PcanAdapter::with_driver(Arc::new(FakeDriver { devices, state }))
    }

    fn device() -> DriverDevice {
        DriverDevice {
            handle: 0x51,
            device_type: 0x05,
            controller_number: 0,
            device_name: Some("PCAN-USB FD".to_owned()),
            device_id: Some(0x1234),
            channel_condition: 1,
            fd_capable: true,
        }
    }

    fn state() -> Arc<Mutex<FakeState>> {
        let frame = CanFrame::fd_data(
            CanId::extended(0x12345).unwrap(),
            vec![0x5a; 12],
            true,
            true,
        )
        .unwrap();
        let timestamp = CanTimestamp::new(
            42_000,
            CanTimestampSource::Hardware,
            "pcan-fake",
        )
        .unwrap();
        Arc::new(Mutex::new(FakeState {
            received: VecDeque::from([ReceivedCanFrame::new(frame, Some(timestamp))]),
            sent: Vec::new(),
            receive_timeouts: Vec::new(),
            close_count: 0,
            fail_after_sends: Some(1),
            status: CanBusStatus::new(CanBusState::Warning, Some(2), Some(3)),
        }))
    }

    fn fd_config() -> CanOpenConfig {
        CanOpenConfig::Configure(
            CanConfigureConfig::new(
                CanMode::Fd,
                CanBitTiming::new(500_000, None, None).unwrap(),
                Some(CanBitTiming::new(2_000_000, None, None).unwrap()),
                false,
                false,
            )
            .unwrap(),
        )
    }

    #[tokio::test]
    async fn fake_driver_covers_discovery_polling_partial_send_status_and_cleanup() {
        let state = state();
        let adapter = fake_adapter(Ok(vec![device()]), Arc::clone(&state));
        let descriptors = adapter.enumerate().await.unwrap();
        let descriptor = &descriptors[0];

        assert!(descriptor.capabilities().contains(&can_classic_capability()));
        assert!(descriptor.capabilities().contains(&can_fd_capability()));
        assert!(descriptor.capabilities().contains(&can_configure_capability()));
        assert!(descriptor
            .capabilities()
            .contains(&can_rx_timestamp_capability()));
        assert!(!descriptor
            .capabilities()
            .contains(&can_error_frames_capability()));

        let mut channel = adapter
            .open(&descriptor.selector(), &fd_config())
            .await
            .unwrap();
        let first = CanFrame::classic_data(CanId::standard(0x123).unwrap(), vec![1]).unwrap();
        let second = CanFrame::classic_data(CanId::standard(0x124).unwrap(), vec![2]).unwrap();
        channel.send(&first).unwrap();
        let rejected = channel.send(&second).unwrap_err();
        assert_eq!(rejected.name().as_str(), "runtime.queue.full");
        assert_eq!(rejected.vendor_code(), Some("0x00000080"));
        assert_eq!(state.lock().unwrap().sent, vec![first]);

        let received = channel
            .receive(Duration::from_millis(7))
            .unwrap()
            .expect("fake frame");
        assert_eq!(received.frame().bitrate_switch(), Some(true));
        assert_eq!(
            state.lock().unwrap().receive_timeouts,
            vec![Duration::from_millis(7)]
        );
        assert_eq!(channel.bus_status().unwrap().state(), CanBusState::Warning);
        channel.close().unwrap();
        channel.close().unwrap();
        assert_eq!(state.lock().unwrap().close_count, 1);
    }

    #[tokio::test]
    async fn unavailable_fake_driver_maps_to_adapter_unavailable() {
        let adapter = fake_adapter(
            Err("PCANBasic.dll was not found".to_owned()),
            state(),
        );

        let error = adapter.enumerate().await.unwrap_err();

        assert_eq!(error.name().as_str(), "can.adapter.unavailable");
        assert_eq!(error.operation().as_str(), "can.enumerate");
        assert!(error.resource_id().is_none());
    }
}
