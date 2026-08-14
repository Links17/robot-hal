#![forbid(unsafe_code)]

mod events;
mod lease_table;
mod registry;
mod serial_actor;

use std::sync::Arc;

use bytes::Bytes;
pub use events::{EventSubscription, RuntimeEvent, RuntimeEventKind};
use registry::{CloseAction, Registry};
use seeed_hal_core::{
    ErrorCategory, HalError, HalResult, LeaseToken, OwnerId, ResourceDescriptor, ResourceSelector,
    SessionId,
};
use seeed_hal_serial::{ControlLines, SerialAdapter, SerialConfig};
use serial_actor::{ActorMetadata, SerialCommand, spawn_serial_actor};
use tokio::sync::{Mutex, oneshot, watch};
use uuid::Uuid;

#[derive(Default)]
pub struct HalRuntimeBuilder {
    serial_adapter: Option<Arc<dyn SerialAdapter>>,
}

impl HalRuntimeBuilder {
    pub fn serial_adapter<A>(mut self, adapter: A) -> Self
    where
        A: SerialAdapter + 'static,
    {
        self.serial_adapter = Some(Arc::new(adapter));
        self
    }

    pub fn build(self) -> HalRuntime {
        HalRuntime {
            inner: Arc::new(RuntimeInner {
                serial_adapter: self.serial_adapter,
                registry: Arc::new(Mutex::new(Registry::default())),
                events: events::EventPublisher::new(),
            }),
        }
    }
}

struct RuntimeInner {
    serial_adapter: Option<Arc<dyn SerialAdapter>>,
    registry: Arc<Mutex<Registry>>,
    events: events::EventPublisher,
}

#[derive(Clone)]
pub struct HalRuntime {
    inner: Arc<RuntimeInner>,
}

impl HalRuntime {
    pub fn builder() -> HalRuntimeBuilder {
        HalRuntimeBuilder::default()
    }

    pub async fn enumerate_serial(&self) -> HalResult<Vec<ResourceDescriptor>> {
        self.serial_adapter("serial.enumerate")?.enumerate().await
    }

    pub async fn open_serial(
        &self,
        owner: OwnerId,
        selector: ResourceSelector,
        config: SerialConfig,
    ) -> HalResult<SerialHandle> {
        let adapter = self.serial_adapter("serial.open")?;
        let session_id = SessionId::parse(Uuid::new_v4().to_string())?;
        let resource_id = selector.id().clone();
        let lease = self.inner.registry.lock().await.reserve_open(
            resource_id.clone(),
            session_id.clone(),
            owner.clone(),
        )?;

        let session = match adapter.open(&selector, config).await {
            Ok(session) => session,
            Err(error) => {
                self.inner.registry.lock().await.cancel_open(&session_id);
                return Err(error);
            }
        };

        let metadata = ActorMetadata {
            session_id: session_id.clone(),
        };
        let actor = spawn_serial_actor(
            session,
            Arc::downgrade(&self.inner.registry),
            self.inner.events.clone(),
            metadata.clone(),
        );
        let accepted = self.inner.registry.lock().await.finish_open(
            &metadata,
            actor.clone(),
            &self.inner.events,
        );
        if !accepted {
            actor.request_close();
            let _ = actor.wait_closed().await;
            return Err(runtime_error(
                "runtime.session.closed",
                ErrorCategory::Conflict,
                "serial.open",
                false,
                "the owner was revoked while the serial resource was opening",
            ));
        }

        Ok(SerialHandle {
            runtime: self.clone(),
            session_id,
            lease,
            closed: false,
        })
    }

    pub async fn read_serial(
        &self,
        session_id: SessionId,
        lease: &LeaseToken,
        max_bytes: usize,
    ) -> HalResult<Bytes> {
        self.request_serial(session_id, lease, "serial.read", |reply| {
            SerialCommand::Read { max_bytes, reply }
        })
        .await
    }

    pub async fn write_serial(
        &self,
        session_id: SessionId,
        lease: &LeaseToken,
        bytes: Bytes,
    ) -> HalResult<()> {
        self.request_serial(session_id, lease, "serial.write", |reply| {
            SerialCommand::Write { bytes, reply }
        })
        .await
    }

    pub async fn flush_serial(&self, session_id: SessionId, lease: &LeaseToken) -> HalResult<()> {
        self.request_serial(session_id, lease, "serial.flush", |reply| {
            SerialCommand::Flush { reply }
        })
        .await
    }

    pub async fn set_serial_control_lines(
        &self,
        session_id: SessionId,
        lease: &LeaseToken,
        lines: ControlLines,
    ) -> HalResult<()> {
        self.request_serial(session_id, lease, "serial.set_control_lines", |reply| {
            SerialCommand::SetControlLines { lines, reply }
        })
        .await
    }

    pub async fn close_serial(&self, session_id: SessionId, lease: &LeaseToken) -> HalResult<()> {
        let action = {
            let mut registry = self.inner.registry.lock().await;
            let action = registry.begin_close(&session_id, lease)?;
            if let CloseAction::Wait(actor) = &action {
                actor.request_close();
            }
            action
        };
        match action {
            CloseAction::AlreadyClosed => Ok(()),
            CloseAction::Wait(actor) => actor.wait_closed().await,
        }
    }

    pub async fn revoke_owner(&self, owner: &OwnerId) -> HalResult<()> {
        let targets = {
            let mut registry = self.inner.registry.lock().await;
            let targets = registry.begin_revoke(owner);
            for target in &targets {
                if let Some(actor) = &target.actor {
                    actor.request_close();
                }
            }
            targets
        };

        let mut first_error = None;
        for target in targets {
            let result = match target.actor {
                Some(actor) => actor.wait_closed().await,
                None => wait_until_done(target.done).await,
            };
            if first_error.is_none() {
                first_error = result.err();
            }
        }
        match first_error {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }

    pub fn subscribe(&self) -> EventSubscription {
        self.inner.events.subscribe()
    }

    async fn request_serial<T>(
        &self,
        session_id: SessionId,
        lease: &LeaseToken,
        operation: &'static str,
        command: impl FnOnce(oneshot::Sender<HalResult<T>>) -> SerialCommand,
    ) -> HalResult<T> {
        let (reply_tx, reply_rx) = oneshot::channel();
        self.inner.registry.lock().await.enqueue(
            &session_id,
            lease,
            command(reply_tx),
            operation,
        )?;
        reply_rx.await.map_err(|_| {
            runtime_error(
                "runtime.actor.unavailable",
                ErrorCategory::Internal,
                operation,
                false,
                "the serial actor dropped an operation reply",
            )
        })?
    }

    fn serial_adapter(&self, operation: &'static str) -> HalResult<Arc<dyn SerialAdapter>> {
        self.inner.serial_adapter.clone().ok_or_else(|| {
            runtime_error(
                "runtime.adapter.not_configured",
                ErrorCategory::Unavailable,
                operation,
                false,
                "no serial adapter was registered with the runtime",
            )
        })
    }
}

pub struct SerialHandle {
    runtime: HalRuntime,
    session_id: SessionId,
    lease: LeaseToken,
    closed: bool,
}

impl SerialHandle {
    pub fn session_id(&self) -> SessionId {
        self.session_id.clone()
    }

    pub fn lease_token(&self) -> &LeaseToken {
        &self.lease
    }

    pub async fn read(&self, max_bytes: usize) -> HalResult<Bytes> {
        self.runtime
            .read_serial(self.session_id(), &self.lease, max_bytes)
            .await
    }

    pub async fn write(&self, bytes: Bytes) -> HalResult<()> {
        self.runtime
            .write_serial(self.session_id(), &self.lease, bytes)
            .await
    }

    pub async fn flush(&self) -> HalResult<()> {
        self.runtime
            .flush_serial(self.session_id(), &self.lease)
            .await
    }

    pub async fn set_control_lines(&self, lines: ControlLines) -> HalResult<()> {
        self.runtime
            .set_serial_control_lines(self.session_id(), &self.lease, lines)
            .await
    }

    pub async fn close(mut self) -> HalResult<()> {
        let result = self
            .runtime
            .close_serial(self.session_id(), &self.lease)
            .await;
        if result.is_ok() {
            self.closed = true;
        }
        result
    }
}

impl Drop for SerialHandle {
    fn drop(&mut self) {
        if self.closed {
            return;
        }
        let runtime = self.runtime.clone();
        let session_id = self.session_id.clone();
        let lease = self.lease.clone();
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            handle.spawn(async move {
                let _ = runtime.close_serial(session_id, &lease).await;
            });
        }
    }
}

async fn wait_until_done(mut done: watch::Receiver<bool>) -> HalResult<()> {
    loop {
        if *done.borrow() {
            return Ok(());
        }
        if done.changed().await.is_err() {
            return Err(runtime_error(
                "runtime.actor.unavailable",
                ErrorCategory::Internal,
                "runtime.owner.revoke",
                false,
                "an opening serial session disappeared before cleanup completed",
            ));
        }
    }
}

pub(crate) fn runtime_error(
    name: &'static str,
    category: ErrorCategory,
    operation: &'static str,
    retryable: bool,
    debug_message: impl Into<String>,
) -> HalError {
    HalError::new(name, category, operation, retryable, debug_message)
        .expect("static runtime error metadata must be valid")
}
