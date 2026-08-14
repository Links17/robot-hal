#![forbid(unsafe_code)]

mod events;
mod lease_table;
mod registry;
mod serial_actor;

use std::sync::Arc;
use std::time::Duration;

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

pub struct HalRuntimeBuilder {
    serial_adapter: Option<Arc<dyn SerialAdapter>>,
    serial_close_timeout: Duration,
}

impl Default for HalRuntimeBuilder {
    fn default() -> Self {
        Self {
            serial_adapter: None,
            serial_close_timeout: Duration::from_secs(2),
        }
    }
}

impl HalRuntimeBuilder {
    pub fn serial_adapter<A>(mut self, adapter: A) -> Self
    where
        A: SerialAdapter + 'static,
    {
        self.serial_adapter = Some(Arc::new(adapter));
        self
    }

    /// Sets the deadline for adapter-level Serial cleanup.
    ///
    /// The default is two seconds. If an adapter does not finish `close()` by
    /// the deadline, the runtime drops the actor-owned session, releases its
    /// lease, and reports `runtime.session.close_timeout`.
    pub fn serial_close_timeout(mut self, timeout: Duration) -> Self {
        self.serial_close_timeout = timeout;
        self
    }

    pub fn build(self) -> HalRuntime {
        HalRuntime {
            inner: Arc::new(RuntimeInner {
                serial_adapter: self.serial_adapter,
                registry: Arc::new(Mutex::new(Registry::default())),
                events: events::EventPublisher::new(),
                serial_close_timeout: self.serial_close_timeout,
            }),
        }
    }
}

struct RuntimeInner {
    serial_adapter: Option<Arc<dyn SerialAdapter>>,
    registry: Arc<Mutex<Registry>>,
    events: events::EventPublisher,
    serial_close_timeout: Duration,
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
        let reservation = self.inner.registry.lock().await.reserve_open(
            resource_id.clone(),
            session_id.clone(),
            owner.clone(),
        )?;
        let lease = reservation.lease;
        let mut pending_open = PendingOpen::new(self.inner.registry.clone(), session_id.clone());

        let session_result = tokio::select! {
            result = adapter.open(&selector, config) => result,
            _ = wait_until_cancelled(reservation.cancellation) => Err(runtime_error(
                "runtime.session.closed",
                ErrorCategory::Conflict,
                "serial.open",
                false,
                "the owner was revoked while the serial resource was opening",
            )),
        };
        let session = match session_result {
            Ok(session) => session,
            Err(error) => {
                self.inner.registry.lock().await.cancel_open(&session_id);
                pending_open.disarm();
                return Err(error);
            }
        };

        let metadata = ActorMetadata {
            session_id: session_id.clone(),
            close_timeout: self.inner.serial_close_timeout,
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
        pending_open.disarm();
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

    /// Closes a Serial session after authenticating its session ID and lease.
    ///
    /// Close replay is idempotent for the 256 most recently closed sessions
    /// in this runtime, provided the caller supplies the exact original
    /// session ID and lease token. Closing a 257th newer session evicts the
    /// oldest replay entry; a later replay for that entry returns
    /// `runtime.session.not_found`.
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

struct PendingOpen {
    registry: Arc<Mutex<Registry>>,
    session_id: SessionId,
    armed: bool,
}

impl PendingOpen {
    fn new(registry: Arc<Mutex<Registry>>, session_id: SessionId) -> Self {
        Self {
            registry,
            session_id,
            armed: true,
        }
    }

    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for PendingOpen {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        if let Ok(mut registry) = self.registry.try_lock() {
            registry.cancel_open(&self.session_id);
            return;
        }
        let registry = self.registry.clone();
        let session_id = self.session_id.clone();
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            handle.spawn(async move {
                registry.lock().await.cancel_open(&session_id);
            });
        }
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

async fn wait_until_cancelled(mut cancellation: watch::Receiver<bool>) {
    loop {
        if *cancellation.borrow() {
            return;
        }
        if cancellation.changed().await.is_err() {
            return;
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
