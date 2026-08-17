use async_trait::async_trait;
use bytes::Bytes;
use seeed_hal_core::{
    CapabilitySet, ErrorCategory, HalError, HalResult, IdentityQuality, ResourceDescriptor,
    ResourceId, ResourceProperties, ResourceSelector, TransportKind, resolve_resource,
};
use seeed_hal_usb::{
    UsbAdapter, UsbInterfaceClaim, UsbInterfaceSession, UsbTransfer, usb_bulk_capability,
    usb_control_capability, usb_interrupt_capability,
};
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::Duration;

#[derive(Clone, Debug)]
pub struct VirtualUsbAdapter {
    descriptor: ResourceDescriptor,
    state: Arc<Mutex<State>>,
}
#[derive(Debug, Default)]
struct State {
    claims: HashSet<u8>,
    data: HashMap<u8, Bytes>,
    control: Bytes,
}

impl VirtualUsbAdapter {
    pub fn loopback(resource_id: impl Into<String>) -> Self {
        let id = ResourceId::parse(resource_id.into()).expect("valid virtual USB resource id");
        let descriptor = ResourceDescriptor::new(
            id.clone(),
            seeed_hal_core::Endpoint::new(format!("virtual://usb/{}", id.as_str()))
                .expect("valid endpoint"),
            IdentityQuality::Strong,
            TransportKind::Usb,
            ResourceProperties::new(
                [
                    ("adapter".into(), "virtual".into()),
                    ("mode".into(), "loopback".into()),
                ]
                .into_iter()
                .collect(),
            ),
            CapabilitySet::new(vec![
                usb_control_capability(),
                usb_bulk_capability(),
                usb_interrupt_capability(),
            ]),
        );
        Self {
            descriptor,
            state: Arc::new(Mutex::new(State::default())),
        }
    }
}

#[async_trait]
impl UsbAdapter for VirtualUsbAdapter {
    fn adapter_name(&self) -> &'static str {
        "virtual.usb.loopback"
    }
    async fn enumerate(&self) -> HalResult<Vec<ResourceDescriptor>> {
        Ok(vec![self.descriptor.clone()])
    }
    async fn open(
        &self,
        selector: &ResourceSelector,
        claim: UsbInterfaceClaim,
    ) -> HalResult<Box<dyn UsbInterfaceSession>> {
        let descriptor = resolve_resource(
            std::slice::from_ref(&self.descriptor),
            selector,
            &usb_control_capability(),
            "usb.open",
        )?
        .clone();
        let mut state = self.state.lock().expect("virtual USB mutex poisoned");
        if !state.claims.insert(claim.number()) {
            return Err(conflict("usb.open"));
        }
        Ok(Box::new(VirtualUsbSession {
            descriptor,
            claim,
            state: Arc::clone(&self.state),
            closed: false,
        }))
    }
}

struct VirtualUsbSession {
    descriptor: ResourceDescriptor,
    claim: UsbInterfaceClaim,
    state: Arc<Mutex<State>>,
    closed: bool,
}
#[async_trait]
impl UsbInterfaceSession for VirtualUsbSession {
    fn descriptor(&self) -> &ResourceDescriptor {
        &self.descriptor
    }
    fn interface_claim(&self) -> UsbInterfaceClaim {
        self.claim
    }
    async fn transfer(&mut self, transfer: UsbTransfer, _timeout: Duration) -> HalResult<Bytes> {
        if self.closed {
            return Err(closed("usb.transfer"));
        }
        transfer.validate()?;
        let mut state = self.state.lock().expect("virtual USB mutex poisoned");
        match transfer {
            UsbTransfer::ControlOut { data, .. } => {
                state.control = data;
                Ok(Bytes::new())
            }
            UsbTransfer::ControlIn { max_bytes, .. } => {
                Ok(state.control.slice(..state.control.len().min(max_bytes)))
            }
            UsbTransfer::BulkOut { endpoint, data }
            | UsbTransfer::InterruptOut { endpoint, data } => {
                state.data.insert(endpoint & 0x7f, data);
                Ok(Bytes::new())
            }
            UsbTransfer::BulkIn {
                endpoint,
                max_bytes,
            }
            | UsbTransfer::InterruptIn {
                endpoint,
                max_bytes,
            } => {
                let data = state
                    .data
                    .get(&(endpoint & 0x7f))
                    .cloned()
                    .unwrap_or_default();
                Ok(data.slice(..data.len().min(max_bytes)))
            }
        }
    }
    async fn close(&mut self) -> HalResult<()> {
        if !self.closed {
            self.state
                .lock()
                .expect("virtual USB mutex poisoned")
                .claims
                .remove(&self.claim.number());
            self.closed = true;
        }
        Ok(())
    }
}
impl Drop for VirtualUsbSession {
    fn drop(&mut self) {
        if !self.closed {
            self.state
                .lock()
                .expect("virtual USB mutex poisoned")
                .claims
                .remove(&self.claim.number());
        }
    }
}
fn conflict(operation: &'static str) -> HalError {
    HalError::new(
        "runtime.adapter.conflict",
        ErrorCategory::Conflict,
        operation,
        false,
        "USB interface is already claimed",
    )
    .expect("valid error")
}
fn closed(operation: &'static str) -> HalError {
    HalError::new(
        "runtime.session.closed",
        ErrorCategory::Unavailable,
        operation,
        false,
        "USB session is closed",
    )
    .expect("valid error")
}
