use crate::{lease_table::LeaseTable, runtime_error};
use bytes::Bytes;
use seeed_hal_core::{
    ErrorCategory, HalResult, LeaseToken, OwnerId, ResourceId, ResourceSelector, SessionId,
    resolve_resource,
};
use seeed_hal_usb::{
    UsbAdapter, UsbInterfaceClaim, UsbInterfaceSession, UsbTransfer, usb_control_capability,
};
use std::{collections::HashMap, sync::Arc, time::Duration};
use tokio::sync::Mutex;
use uuid::Uuid;

struct Entry {
    resource: ResourceId,
    owner: OwnerId,
    session: Box<dyn UsbInterfaceSession>,
}
#[derive(Default)]
struct State {
    leases: LeaseTable,
    sessions: HashMap<SessionId, Entry>,
    closed: HashMap<SessionId, (ResourceId, OwnerId)>,
}
pub(crate) struct UsbManager {
    adapter: Option<Arc<dyn UsbAdapter>>,
    state: Mutex<State>,
}
impl UsbManager {
    pub(crate) fn new(adapter: Option<Arc<dyn UsbAdapter>>) -> Self {
        Self {
            adapter,
            state: Mutex::new(State::default()),
        }
    }
    fn adapter(&self, op: &'static str) -> HalResult<Arc<dyn UsbAdapter>> {
        self.adapter.clone().ok_or_else(|| {
            runtime_error(
                "runtime.adapter.not_configured",
                ErrorCategory::Unavailable,
                op,
                false,
                "no USB adapter was registered",
            )
        })
    }
    pub(crate) async fn enumerate(&self) -> HalResult<Vec<seeed_hal_core::ResourceDescriptor>> {
        self.adapter("usb.enumerate")?.enumerate().await
    }
    pub(crate) async fn open(
        &self,
        owner: OwnerId,
        selector: ResourceSelector,
        claim: UsbInterfaceClaim,
    ) -> HalResult<(SessionId, LeaseToken)> {
        let adapter = self.adapter("usb.open")?;
        let descriptors = adapter.enumerate().await?;
        let descriptor = resolve_resource(
            &descriptors,
            &selector,
            &usb_control_capability(),
            "usb.open",
        )?
        .clone();
        let id = SessionId::parse(Uuid::new_v4().to_string())?;
        let lease = {
            let mut state = self.state.lock().await;
            state
                .leases
                .reserve_control(descriptor.id().clone(), id.clone(), owner.clone())?
        };
        let session = match adapter.open(&selector, claim).await {
            Ok(s) => s,
            Err(e) => {
                self.state.lock().await.leases.release(descriptor.id(), &id);
                return Err(e);
            }
        };
        let mut state = self.state.lock().await;
        if !state.leases.commit(descriptor.id(), &id, &lease) {
            return Err(runtime_error(
                "runtime.session.closed",
                ErrorCategory::Conflict,
                "usb.open",
                false,
                "USB open was cancelled",
            ));
        }
        state.sessions.insert(
            id.clone(),
            Entry {
                resource: descriptor.id().clone(),
                owner,
                session,
            },
        );
        Ok((id, lease))
    }
    pub(crate) async fn transfer(
        &self,
        id: SessionId,
        lease: &LeaseToken,
        transfer: UsbTransfer,
        timeout: Duration,
    ) -> HalResult<Bytes> {
        let mut state = self.state.lock().await;
        let entry = match state.sessions.remove(&id) {
            Some(entry) => entry,
            None => {
                if let Some((resource, owner)) = state.closed.get(&id) {
                    state
                        .leases
                        .validate(resource, &id, owner, lease, "usb.transfer")?;
                }
                return Err(runtime_error(
                    "runtime.session.closed",
                    ErrorCategory::Conflict,
                    "usb.transfer",
                    false,
                    "USB session is closed",
                ));
            }
        };
        state
            .leases
            .validate(&entry.resource, &id, &entry.owner, lease, "usb.transfer")?;
        let resource = entry.resource.clone();
        let mut entry = entry;
        drop(state);
        let result = entry.session.transfer(transfer, timeout).await;
        let mut state = self.state.lock().await;
        state.sessions.insert(id, entry);
        result.map_err(|e| e.with_resource_id(resource))
    }
    pub(crate) async fn close(&self, id: SessionId, lease: &LeaseToken) -> HalResult<()> {
        let mut state = self.state.lock().await;
        let entry = state.sessions.remove(&id).ok_or_else(|| {
            runtime_error(
                "runtime.session.closed",
                ErrorCategory::Conflict,
                "usb.close",
                false,
                "USB session is closed",
            )
        })?;
        state
            .leases
            .validate(&entry.resource, &id, &entry.owner, lease, "usb.close")?;
        state.leases.release(&entry.resource, &id);
        state
            .closed
            .insert(id, (entry.resource.clone(), entry.owner.clone()));
        drop(state);
        let mut entry = entry;
        entry
            .session
            .close()
            .await
            .map_err(|e| e.with_resource_id(entry.resource))
    }
}
