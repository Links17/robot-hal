use crate::{lease_table::LeaseTable, runtime_error};
use seeed_hal_core::{
    ErrorCategory, HalResult, LeaseToken, OwnerId, ResourceId, ResourceSelector, SessionId,
    resolve_resource,
};
use seeed_hal_gpio::{
    GpioAdapter, GpioEdgeEvent, GpioEdgeRequest, GpioLineConfig, GpioLineSession,
    gpio_lines_capability,
};
use std::{collections::HashMap, sync::Arc, time::Duration};
use tokio::sync::Mutex;
use uuid::Uuid;

struct Entry {
    resource: ResourceId,
    owner: OwnerId,
    session: Box<dyn GpioLineSession>,
}
#[derive(Default)]
struct State {
    leases: LeaseTable,
    sessions: HashMap<SessionId, Entry>,
    closed: HashMap<SessionId, (ResourceId, OwnerId)>,
}
pub(crate) struct GpioManager {
    adapter: Option<Arc<dyn GpioAdapter>>,
    state: Mutex<State>,
}
impl GpioManager {
    pub(crate) fn new(adapter: Option<Arc<dyn GpioAdapter>>) -> Self {
        Self {
            adapter,
            state: Mutex::new(State::default()),
        }
    }
    fn adapter(&self, op: &'static str) -> HalResult<Arc<dyn GpioAdapter>> {
        self.adapter.clone().ok_or_else(|| {
            runtime_error(
                "runtime.adapter.not_configured",
                ErrorCategory::Unavailable,
                op,
                false,
                "no GPIO adapter was registered",
            )
        })
    }
    pub(crate) async fn enumerate(&self) -> HalResult<Vec<seeed_hal_core::ResourceDescriptor>> {
        self.adapter("gpio.enumerate")?.enumerate().await
    }
    pub(crate) async fn open(
        &self,
        owner: OwnerId,
        selector: ResourceSelector,
        lines: Vec<u32>,
        config: GpioLineConfig,
    ) -> HalResult<(SessionId, LeaseToken)> {
        let adapter = self.adapter("gpio.open")?;
        let descriptors = adapter.enumerate().await?;
        let descriptor = resolve_resource(
            &descriptors,
            &selector,
            &gpio_lines_capability(),
            "gpio.open",
        )?
        .clone();
        let id = SessionId::parse(Uuid::new_v4().to_string())?;
        let lease = {
            let mut s = self.state.lock().await;
            s.leases
                .reserve_control(descriptor.id().clone(), id.clone(), owner.clone())?
        };
        let session = match adapter.open(&selector, &lines, config).await {
            Ok(v) => v,
            Err(e) => {
                self.state.lock().await.leases.release(descriptor.id(), &id);
                return Err(e);
            }
        };
        let mut s = self.state.lock().await;
        if !s.leases.commit(descriptor.id(), &id, &lease) {
            return Err(runtime_error(
                "runtime.session.closed",
                ErrorCategory::Conflict,
                "gpio.open",
                false,
                "GPIO open was cancelled",
            ));
        }
        s.sessions.insert(
            id.clone(),
            Entry {
                resource: descriptor.id().clone(),
                owner,
                session,
            },
        );
        Ok((id, lease))
    }
    async fn take(&self, id: &SessionId, lease: &LeaseToken, op: &'static str) -> HalResult<Entry> {
        let mut s = self.state.lock().await;
        match s.sessions.remove(id) {
            Some(entry) => {
                s.leases
                    .validate(&entry.resource, id, &entry.owner, lease, op)?;
                Ok(entry)
            }
            None => {
                if let Some((r, o)) = s.closed.get(id) {
                    s.leases.validate(r, id, o, lease, op)?;
                }
                Err(runtime_error(
                    "runtime.session.closed",
                    ErrorCategory::Conflict,
                    op,
                    false,
                    "GPIO session is closed",
                ))
            }
        }
    }
    async fn put(&self, id: SessionId, entry: Entry) {
        self.state.lock().await.sessions.insert(id, entry);
    }
    pub(crate) async fn read(&self, id: SessionId, lease: &LeaseToken) -> HalResult<Vec<bool>> {
        let mut e = self.take(&id, lease, "gpio.read").await?;
        let r = e.resource.clone();
        let out = e.session.read().await.map_err(|x| x.with_resource_id(r));
        self.put(id, e).await;
        out
    }
    pub(crate) async fn write(
        &self,
        id: SessionId,
        lease: &LeaseToken,
        values: Vec<bool>,
    ) -> HalResult<()> {
        let mut e = self.take(&id, lease, "gpio.write").await?;
        let r = e.resource.clone();
        let out = e
            .session
            .write(&values)
            .await
            .map_err(|x| x.with_resource_id(r));
        self.put(id, e).await;
        out
    }
    pub(crate) async fn next_edge(
        &self,
        id: SessionId,
        lease: &LeaseToken,
        request: GpioEdgeRequest,
        timeout: Duration,
    ) -> HalResult<Option<GpioEdgeEvent>> {
        let mut e = self.take(&id, lease, "gpio.next_edge").await?;
        let r = e.resource.clone();
        let out = e
            .session
            .next_edge(request, timeout)
            .await
            .map_err(|x| x.with_resource_id(r));
        self.put(id, e).await;
        out
    }
    pub(crate) async fn close(&self, id: SessionId, lease: &LeaseToken) -> HalResult<()> {
        let mut e = self.take(&id, lease, "gpio.close").await?;
        let mut s = self.state.lock().await;
        s.leases.release(&e.resource, &id);
        s.closed.insert(id, (e.resource.clone(), e.owner.clone()));
        drop(s);
        e.session
            .close()
            .await
            .map_err(|x| x.with_resource_id(e.resource))
    }
    pub(crate) async fn revoke_owner(&self, owner: &OwnerId) -> HalResult<()> {
        let entries = {
            let mut state = self.state.lock().await;
            let ids = state
                .sessions
                .iter()
                .filter(|(_, entry)| &entry.owner == owner)
                .map(|(id, _)| id.clone())
                .collect::<Vec<_>>();
            let mut entries = Vec::new();
            for id in ids {
                if let Some(entry) = state.sessions.remove(&id) {
                    state.leases.release(&entry.resource, &id);
                    state
                        .closed
                        .insert(id, (entry.resource.clone(), entry.owner.clone()));
                    entries.push(entry);
                }
            }
            entries
        };
        for mut entry in entries {
            entry.session.close().await?;
        }
        Ok(())
    }
}
