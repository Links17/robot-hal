#![cfg(target_os = "linux")]

use std::sync::atomic::{AtomicU32, Ordering};

use async_trait::async_trait;
use seeed_hal_adapter_socketcan::SocketCanAdapter;
use seeed_hal_can::{
    CanAdapter, CanChannel, CanLinkExpectation, CanOpenConfig, IdentityQuality,
    ResourceDescriptor, ResourceSelector, can_configure_capability,
};
use seeed_hal_core::{ErrorCategory, HalResult};
use socketcan::nl::CanInterface;

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
        if let Err(error) = interface.bring_up() {
            let _ = interface.delete();
            panic!("bring up {name}: {error}");
        }
        Self {
            name,
            interface: Some(interface),
        }
    }

    fn delete(mut self) {
        let interface = self.interface.take().expect("vcan fixture is present");
        interface
            .delete()
            .unwrap_or_else(|(_, error)| panic!("delete {}: {error}", self.name));
    }
}

impl Drop for VcanFixture {
    fn drop(&mut self) {
        if let Some(interface) = self.interface.take() {
            let _ = interface.delete();
        }
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
    let fixture = VcanFixture::create();
    let adapter = SocketCanAdapter::new();
    let descriptor = descriptor_for(&adapter, &fixture.name).await;
    let selector = descriptor.selector();
    fixture.delete();

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
