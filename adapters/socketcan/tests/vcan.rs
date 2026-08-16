#![cfg(target_os = "linux")]

use std::sync::atomic::{AtomicU32, Ordering};

use async_trait::async_trait;
use seeed_hal_adapter_socketcan::SocketCanAdapter;
use seeed_hal_can::{
    CanAdapter, CanBitTiming, CanChannel, CanConfigureConfig, CanLinkExpectation,
    CanMode, CanOpenConfig, IdentityQuality, ResourceDescriptor, ResourceSelector,
    can_configure_capability,
};
use seeed_hal_core::{ErrorCategory, HalResult};
use socketcan::nl::{CanCtrlMode, CanInterface, InterfaceDetails, Mtu};

static NEXT_INTERFACE: AtomicU32 = AtomicU32::new(0);

struct VcanFixture {
    name: String,
    interface: Option<CanInterface>,
}

impl VcanFixture {
    fn create() -> Self {
        let suffix = NEXT_INTERFACE.fetch_add(1, Ordering::Relaxed) & 0xffff;
        let name = format!("sh{:06x}{suffix:04x}", std::process::id() & 0x00ff_ffff);
        let interface = CanInterface::create_vcan(&name, None)
            .unwrap_or_else(|error| panic!("create {name}: {error}"));
        let mut fixture = Self {
            name,
            interface: Some(interface),
        };
        if let Err(error) = fixture
            .interface
            .as_ref()
            .expect("new fixture interface")
            .bring_up()
        {
            let cleanup = fixture.delete().err();
            panic!("bring up {}: {error}; cleanup={cleanup:?}", fixture.name);
        }
        fixture
    }

    fn delete(&mut self) -> Result<(), String> {
        let Some(interface) = self.interface.take() else {
            return Ok(());
        };
        match interface.delete() {
            Ok(()) => Ok(()),
            Err((interface, error)) => {
                self.interface = Some(interface);
                Err(error.to_string())
            }
        }
    }
}

impl Drop for VcanFixture {
    fn drop(&mut self) {
        let _ = self.delete();
    }
}

async fn descriptor_for(
    adapter: &SocketCanAdapter,
    interface: &str,
) -> ResourceDescriptor {
    adapter
        .enumerate()
        .await
        .expect("enumerate SocketCAN interfaces")
        .into_iter()
        .find(|descriptor| descriptor.endpoint().as_str() == interface)
        .unwrap_or_else(|| panic!("enumeration includes {interface}"))
}

fn empty_attach() -> CanOpenConfig {
    CanOpenConfig::Attach(
        CanLinkExpectation::new(None, None, None, None, None)
            .expect("empty Attach expectation is valid"),
    )
}

#[tokio::test]
#[ignore = "requires Linux vcan and CAP_NET_ADMIN"]
async fn vcan_discovery_is_weak_and_does_not_advertise_configuration() {
    let fixture = VcanFixture::create();
    assert!(!fixture.name.starts_with("vcan"));
    let adapter = SocketCanAdapter::new();

    let descriptor = descriptor_for(&adapter, &fixture.name).await;

    assert_eq!(descriptor.minimum_identity_quality(), IdentityQuality::Weak);
    assert_eq!(descriptor.properties().get("virtual"), Some("true"));
    assert!(!descriptor
        .capabilities()
        .contains(&can_configure_capability()));
}

#[tokio::test]
#[ignore = "requires Linux vcan and CAP_NET_ADMIN"]
async fn vcan_attach_without_kernel_timing_is_structured_and_resource_scoped() {
    let fixture = VcanFixture::create();
    let adapter = SocketCanAdapter::new();
    let descriptor = descriptor_for(&adapter, &fixture.name).await;

    let error = match adapter.open(&descriptor.selector(), &empty_attach()).await {
        Ok(_) => panic!("vcan without kernel timing must not fabricate active timing"),
        Err(error) => error,
    };

    assert_eq!(
        error.name().as_str(),
        "runtime.transport.unsupported_configuration"
    );
    assert_eq!(error.category(), ErrorCategory::InvalidArgument);
    assert_eq!(error.operation().as_str(), "can.open");
    assert!(!error.retryable());
    assert_eq!(error.resource_id(), Some(descriptor.id()));
}

#[tokio::test]
#[ignore = "requires Linux vcan and CAP_NET_ADMIN"]
async fn deleted_vcan_fails_with_the_canonical_resource_id() {
    let mut fixture = VcanFixture::create();
    let adapter = SocketCanAdapter::new();
    let descriptor = descriptor_for(&adapter, &fixture.name).await;
    let selector = descriptor.selector();
    fixture.delete().expect("delete vcan fixture");

    let error = match adapter.open(&selector, &empty_attach()).await {
        Ok(_) => panic!("deleted vcan must not open"),
        Err(error) => error,
    };

    assert_eq!(error.name().as_str(), "runtime.resource.not_found");
    assert_eq!(error.category(), ErrorCategory::NotFound);
    assert_eq!(error.operation().as_str(), "can.open");
    assert!(!error.retryable());
    assert_eq!(error.resource_id(), Some(selector.id()));
}

struct SelectedSocketCanAdapter {
    interface: String,
    inner: SocketCanAdapter,
}

#[async_trait]
impl CanAdapter for SelectedSocketCanAdapter {
    fn adapter_name(&self) -> &'static str {
        "socketcan.selected"
    }

    async fn enumerate(&self) -> HalResult<Vec<ResourceDescriptor>> {
        Ok(self
            .inner
            .enumerate()
            .await?
            .into_iter()
            .filter(|descriptor| descriptor.endpoint().as_str() == self.interface.as_str())
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
#[ignore = "requires a provisioned real CAN interface selected by SEEED_HAL_SOCKETCAN_CONFORMANCE_INTERFACE"]
async fn selected_real_interface_passes_shared_can_conformance() {
    let interface = std::env::var("SEEED_HAL_SOCKETCAN_CONFORMANCE_INTERFACE")
        .expect("set SEEED_HAL_SOCKETCAN_CONFORMANCE_INTERFACE to a provisioned real CAN link");
    let adapter = SelectedSocketCanAdapter {
        interface,
        inner: SocketCanAdapter::new(),
    };

    let descriptors = adapter
        .enumerate()
        .await
        .expect("enumerate selected SocketCAN interface");
    let descriptor = descriptors
        .first()
        .expect("selected SocketCAN interface is present");
    assert_eq!(descriptor.properties().get("virtual"), Some("false"));

    seeed_hal_testkit::run_can_adapter_conformance(&adapter)
        .await
        .expect("selected SocketCAN interface passes shared conformance");
}

fn timing_from_kernel(timing: socketcan::nl::CanBitTiming) -> CanBitTiming {
    let sample_point = u16::try_from(timing.sample_point)
        .ok()
        .filter(|value| *value != 0);
    let sjw = u16::try_from(timing.sjw)
        .ok()
        .filter(|value| *value != 0);
    CanBitTiming::new(timing.bitrate, sample_point, sjw)
        .expect("selected real interface reports representable timing")
}

fn configure_from_kernel(details: &InterfaceDetails) -> CanConfigureConfig {
    let fd = details.mtu == Some(Mtu::Fd)
        && details
            .can
            .ctrl_mode
            .is_some_and(|modes| modes.has_mode(CanCtrlMode::Fd));
    let nominal = timing_from_kernel(
        details
            .can
            .bit_timing
            .expect("selected real interface reports nominal timing"),
    );
    let data = fd.then(|| {
        timing_from_kernel(
            details
                .can
                .data_bit_timing
                .expect("selected FD interface reports data timing"),
        )
    });
    let modes = details.can.ctrl_mode.unwrap_or_default();
    let restart_ms = details.can.restart_ms.filter(|value| *value != 0);
    CanConfigureConfig::new_with_restart(
        if fd { CanMode::Fd } else { CanMode::Classic },
        nominal,
        data,
        modes.has_mode(CanCtrlMode::ListenOnly),
        modes.has_mode(CanCtrlMode::Loopback),
        restart_ms,
    )
    .expect("selected real interface reports a valid active configuration")
}

struct UpStateRestore<'a> {
    interface: &'a CanInterface,
    restore_up: bool,
    pending: bool,
}

impl UpStateRestore<'_> {
    fn restore(&mut self) {
        let result = if self.restore_up {
            self.interface.bring_up()
        } else {
            self.interface.bring_down()
        };
        result.expect("restore applied CAN interface state");
        self.pending = false;
    }

    fn disarm(&mut self) {
        self.pending = false;
    }
}

impl Drop for UpStateRestore<'_> {
    fn drop(&mut self) {
        if self.pending {
            if self.restore_up {
                let _ = self.interface.bring_up();
            } else {
                let _ = self.interface.bring_down();
            }
        }
    }
}

#[tokio::test]
#[ignore = "requires a provisioned real CAN interface selected by SEEED_HAL_SOCKETCAN_CONFORMANCE_INTERFACE and optional CAP_NET_ADMIN"]
async fn selected_real_configure_reports_permission_or_close_retries_after_conflict() {
    let interface_name = std::env::var("SEEED_HAL_SOCKETCAN_CONFORMANCE_INTERFACE")
        .expect("set SEEED_HAL_SOCKETCAN_CONFORMANCE_INTERFACE to a provisioned real CAN link");
    let adapter = SocketCanAdapter::new();
    let descriptor = descriptor_for(&adapter, &interface_name).await;
    assert!(descriptor
        .capabilities()
        .contains(&can_configure_capability()));
    let interface = CanInterface::open(&interface_name).expect("open selected CAN interface");
    let baseline = interface
        .details()
        .expect("query selected CAN interface baseline");
    let request = configure_from_kernel(&baseline);

    let mut channel = match adapter
        .open(&descriptor.selector(), &CanOpenConfig::Configure(request))
        .await
    {
        Ok(channel) => channel,
        Err(error) => {
            assert_eq!(
                error.name().as_str(),
                "runtime.transport.permission_denied"
            );
            assert_eq!(error.category(), ErrorCategory::Conflict);
            assert_eq!(error.operation().as_str(), "can.configure");
            assert!(!error.retryable());
            assert_eq!(error.resource_id(), Some(descriptor.id()));
            return;
        }
    };

    let applied = interface
        .details()
        .expect("query configured CAN interface");
    if applied.is_up {
        interface.bring_down().expect("inject down-state conflict");
    } else {
        interface.bring_up().expect("inject up-state conflict");
    }
    let mut applied_state = UpStateRestore {
        interface: &interface,
        restore_up: applied.is_up,
        pending: true,
    };
    let first_close = channel.close();
    let conflict = match first_close {
        Err(conflict) => {
            applied_state.restore();
            conflict
        }
        Ok(()) => {
            applied_state.disarm();
            panic!("external state change must reject restore");
        }
    };
    drop(applied_state);
    assert_eq!(conflict.name().as_str(), "can.configuration.conflict");
    assert_eq!(conflict.resource_id(), Some(descriptor.id()));

    channel
        .close()
        .expect("retry restores the retained pre-Configure snapshot");
    let restored = interface
        .details()
        .expect("query restored CAN interface");
    assert_eq!(restored.is_up, baseline.is_up);
    assert_eq!(restored.mtu, baseline.mtu);
}
