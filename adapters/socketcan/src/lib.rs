#![forbid(unsafe_code)]

pub mod identity;

#[cfg(target_os = "linux")]
mod channel;
#[cfg(target_os = "linux")]
mod link;

pub use identity::{CanIdentity, CanInterfaceMetadata, identity_from_metadata};

use async_trait::async_trait;
use seeed_hal_can::{CanAdapter, CanChannel, CanOpenConfig};
use seeed_hal_core::{ErrorCategory, HalError, HalResult, ResourceDescriptor, ResourceSelector};

#[cfg(target_os = "linux")]
use seeed_hal_can::{can_classic_capability, can_configure_capability, can_fd_capability};
#[cfg(target_os = "linux")]
use seeed_hal_core::{CapabilitySet, ResourceProperties, TransportKind, resolve_resource};

#[derive(Clone, Debug, Default)]
pub struct SocketCanAdapter;

impl SocketCanAdapter {
    #[must_use]
    pub const fn new() -> Self {
        Self
    }
}

#[cfg(target_os = "linux")]
#[async_trait]
impl CanAdapter for SocketCanAdapter {
    fn adapter_name(&self) -> &'static str {
        "socketcan"
    }

    async fn enumerate(&self) -> HalResult<Vec<ResourceDescriptor>> {
        tokio::task::spawn_blocking(enumerate_sync)
            .await
            .map_err(|error| join_error("can.enumerate", error))?
    }

    async fn open(
        &self,
        selector: &ResourceSelector,
        config: &CanOpenConfig,
    ) -> HalResult<Box<dyn CanChannel>> {
        let resource_id = selector.id().clone();
        let selector = selector.clone();
        let config = config.clone();
        tokio::task::spawn_blocking(move || open_sync(&selector, &config))
            .await
            .map_err(|error| join_error("can.open", error).with_resource_id(resource_id))?
    }
}

#[cfg(not(target_os = "linux"))]
#[async_trait]
impl CanAdapter for SocketCanAdapter {
    fn adapter_name(&self) -> &'static str {
        "socketcan"
    }

    async fn enumerate(&self) -> HalResult<Vec<ResourceDescriptor>> {
        Err(unavailable("can.enumerate"))
    }

    async fn open(
        &self,
        selector: &ResourceSelector,
        _config: &CanOpenConfig,
    ) -> HalResult<Box<dyn CanChannel>> {
        Err(unavailable("can.open").with_resource_id(selector.id().clone()))
    }
}

#[cfg(target_os = "linux")]
fn enumerate_sync() -> HalResult<Vec<ResourceDescriptor>> {
    let interfaces = socketcan::available_interfaces()
        .map_err(|error| discovery_error("can.enumerate", error.as_ref()))?;
    interfaces
        .into_iter()
        .map(|interface| descriptor_from_interface(&interface))
        .collect()
}

#[cfg(target_os = "linux")]
fn descriptor_from_interface(interface: &str) -> HalResult<ResourceDescriptor> {
    let metadata = identity::metadata_from_sysfs(interface);
    let identity = identity_from_metadata(&metadata)?;
    let details = link::details_for_descriptor(interface)
        .map_err(|error| discovery_error("can.enumerate", error))?;
    let nonvirtual_sysfs_evidence = !metadata.virtual_interface && metadata.stable_path.is_some();
    let (supports_fd, supports_configure) =
        link::capabilities_for_details(&details, nonvirtual_sysfs_evidence);
    let mut capabilities = vec![can_classic_capability()];
    if supports_fd {
        capabilities.push(can_fd_capability());
    }
    if supports_configure {
        capabilities.push(can_configure_capability());
    }

    let mut properties = std::collections::BTreeMap::new();
    properties.insert("adapter".to_owned(), "socketcan".to_owned());
    properties.insert("interface".to_owned(), interface.to_owned());
    properties.insert("virtual".to_owned(), metadata.virtual_interface.to_string());
    properties.insert(
        "link_state".to_owned(),
        if details.is_up {
            "up".to_owned()
        } else {
            "down".to_owned()
        },
    );
    properties.insert(
        "mode".to_owned(),
        if link::is_fd_active(&details) {
            "fd".to_owned()
        } else {
            "classic".to_owned()
        },
    );
    Ok(ResourceDescriptor::new(
        identity.id,
        seeed_hal_core::Endpoint::new(interface.to_owned())?,
        identity.quality,
        TransportKind::Can,
        ResourceProperties::new(properties),
        CapabilitySet::new(capabilities),
    ))
}

#[cfg(target_os = "linux")]
fn open_sync(
    selector: &ResourceSelector,
    config: &CanOpenConfig,
) -> HalResult<Box<dyn CanChannel>> {
    let descriptors =
        enumerate_sync().map_err(|error| error.with_resource_id(selector.id().clone()))?;
    let descriptor = resolve_resource(
        &descriptors,
        selector,
        &can_classic_capability(),
        "can.open",
    )?
    .clone();
    let interface = descriptor.endpoint().as_str().to_owned();
    let link = match config {
        CanOpenConfig::Attach(expectation) => {
            link::LinkLease::attach(&interface, expectation, &descriptor)?
        }
        CanOpenConfig::Configure(request) => {
            if !descriptor
                .capabilities()
                .contains(&can_configure_capability())
            {
                return Err(HalError::new(
                    "runtime.protocol.capability_unsupported",
                    ErrorCategory::Conflict,
                    "can.configure",
                    false,
                    "SocketCAN configuration is unavailable for this interface",
                )?
                .with_resource_id(descriptor.id().clone()));
            }
            link::LinkLease::configure(&interface, request, &descriptor)?
        }
    };
    channel::NativeSocketCanChannel::open(descriptor, link)
        .map(|channel| Box::new(channel) as Box<dyn CanChannel>)
}

#[cfg(target_os = "linux")]
fn discovery_error(
    operation: &'static str,
    error: &(dyn std::error::Error + Send + Sync),
) -> HalError {
    HalError::new(
        "runtime.transport.unavailable",
        ErrorCategory::Unavailable,
        operation,
        true,
        format!("SocketCAN discovery failed: {error}"),
    )
    .expect("static SocketCAN error metadata is valid")
}

#[cfg(not(target_os = "linux"))]
fn unavailable(operation: &'static str) -> HalError {
    HalError::new(
        "runtime.adapter.unavailable",
        ErrorCategory::Unavailable,
        operation,
        false,
        "SocketCAN is only available on Linux",
    )
    .expect("static SocketCAN error metadata is valid")
}

#[cfg(target_os = "linux")]
fn join_error(operation: &'static str, error: tokio::task::JoinError) -> HalError {
    HalError::new(
        "runtime.internal.worker_failed",
        ErrorCategory::Internal,
        operation,
        false,
        format!("SocketCAN blocking worker failed: {error}"),
    )
    .expect("static SocketCAN error metadata is valid")
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;

    #[test]
    fn discovery_error_accepts_boxed_send_sync_error() {
        let source: Box<dyn std::error::Error + Send + Sync> =
            Box::new(std::io::Error::other("interface list unavailable"));

        let error = discovery_error("can.enumerate", source.as_ref());

        assert_eq!(error.name().as_str(), "runtime.transport.unavailable");
        assert_eq!(error.category(), ErrorCategory::Unavailable);
        assert_eq!(error.operation().as_str(), "can.enumerate");
        assert!(error.retryable());
        assert_eq!(
            error.debug_message(),
            "SocketCAN discovery failed: interface list unavailable"
        );
    }
}
