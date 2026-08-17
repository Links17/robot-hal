#![forbid(unsafe_code)]

mod can_actor;
mod can_lease_table;
mod can_manager;
mod events;
mod gpio_manager;
mod lease_table;
mod registry;
mod serial_actor;
mod usb_manager;

use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use can_manager::CanManager;
pub use events::{EventSubscription, RuntimeEvent, RuntimeEventKind};
use gpio_manager::GpioManager;
use registry::{CloseAction, Registry};
use seeed_hal_can::{
    CanAdapter, CanBatchSendError, CanBusStatus, CanFilterSet, CanFrame, CanOpenConfig,
    DEFAULT_CAN_RX_CAPACITY, DEFAULT_CAN_TX_CAPACITY, ReceivedCanFrame,
};
use seeed_hal_core::{
    ErrorCategory, HalError, HalResult, LeaseMode, LeaseToken, OwnerId, ResourceDescriptor,
    ResourceSelector, SessionId,
};
use seeed_hal_gpio::{GpioAdapter, GpioEdgeEvent, GpioEdgeRequest, GpioLineConfig};
use seeed_hal_serial::{ControlLines, SerialAdapter, SerialConfig};
use seeed_hal_usb::{UsbAdapter, UsbInterfaceClaim, UsbTransfer};
use serial_actor::{ActorMetadata, SerialCommand, spawn_serial_actor};
use tokio::sync::{Mutex, oneshot, watch};
use usb_manager::UsbManager;
use uuid::Uuid;

pub struct HalRuntimeBuilder {
    serial_adapter: Option<Arc<dyn SerialAdapter>>,
    serial_close_timeout: Duration,
    can_adapters: Vec<Arc<dyn CanAdapter>>,
    can_rx_capacity: usize,
    can_tx_capacity: usize,
    can_close_timeout: Duration,
    usb_adapter: Option<Arc<dyn UsbAdapter>>,
    gpio_adapter: Option<Arc<dyn GpioAdapter>>,
}

/// Maximum configurable frames in one session's software receive ring.
pub const MAX_RUNTIME_CAN_RX_CAPACITY: usize = 4096;
/// Maximum configurable frames admitted across one physical channel's TX queue.
pub const MAX_RUNTIME_CAN_TX_CAPACITY: usize = 4096;

impl Default for HalRuntimeBuilder {
    fn default() -> Self {
        Self {
            serial_adapter: None,
            serial_close_timeout: Duration::from_secs(2),
            can_adapters: Vec::new(),
            can_rx_capacity: DEFAULT_CAN_RX_CAPACITY,
            can_tx_capacity: DEFAULT_CAN_TX_CAPACITY,
            can_close_timeout: Duration::from_secs(2),
            usb_adapter: None,
            gpio_adapter: None,
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

    /// Registers another CAN adapter. Adapters are queried as one discovery set.
    pub fn can_adapter<A>(mut self, adapter: A) -> Self
    where
        A: CanAdapter + 'static,
    {
        self.can_adapters.push(Arc::new(adapter));
        self
    }

    pub fn usb_adapter<A>(mut self, adapter: A) -> Self
    where
        A: UsbAdapter + 'static,
    {
        self.usb_adapter = Some(Arc::new(adapter));
        self
    }
    pub fn gpio_adapter<A>(mut self, adapter: A) -> Self
    where
        A: GpioAdapter + 'static,
    {
        self.gpio_adapter = Some(Arc::new(adapter));
        self
    }

    /// Sets each CAN session's drop-oldest RX ring capacity.
    ///
    /// Values are clamped to `1..=4096`; the default is 256 frames.
    pub fn can_rx_capacity(mut self, capacity: usize) -> Self {
        self.can_rx_capacity = capacity.clamp(1, MAX_RUNTIME_CAN_RX_CAPACITY);
        self
    }

    /// Sets each physical CAN actor's atomically admitted TX frame capacity.
    ///
    /// Values are clamped to `1..=4096`; the default is 64 frames. Individual
    /// batches remain bounded to 64 frames by the CAN interface contract.
    pub fn can_tx_capacity(mut self, capacity: usize) -> Self {
        self.can_tx_capacity = capacity.clamp(1, MAX_RUNTIME_CAN_TX_CAPACITY);
        self
    }

    /// Sets the finite deadline for CAN discovery and actor cleanup.
    pub fn can_close_timeout(mut self, timeout: Duration) -> Self {
        self.can_close_timeout = timeout;
        self
    }

    pub fn build(self) -> HalRuntime {
        let events = events::EventPublisher::new();
        let can_manager = CanManager::new(
            self.can_adapters,
            events.clone(),
            self.can_rx_capacity,
            self.can_tx_capacity,
            self.can_close_timeout,
        );
        HalRuntime {
            inner: Arc::new(RuntimeInner {
                serial_adapter: self.serial_adapter,
                registry: Arc::new(Mutex::new(Registry::default())),
                events,
                serial_close_timeout: self.serial_close_timeout,
                can_manager,
                usb_manager: UsbManager::new(self.usb_adapter),
                gpio_manager: GpioManager::new(self.gpio_adapter),
            }),
        }
    }
}

struct RuntimeInner {
    serial_adapter: Option<Arc<dyn SerialAdapter>>,
    registry: Arc<Mutex<Registry>>,
    events: events::EventPublisher,
    serial_close_timeout: Duration,
    can_manager: CanManager,
    usb_manager: UsbManager,
    gpio_manager: GpioManager,
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

    pub async fn enumerate_can(&self) -> HalResult<Vec<ResourceDescriptor>> {
        self.inner.can_manager.enumerate().await
    }
    pub async fn enumerate_usb(&self) -> HalResult<Vec<ResourceDescriptor>> {
        self.inner.usb_manager.enumerate().await
    }
    pub async fn open_usb(
        &self,
        owner: OwnerId,
        selector: ResourceSelector,
        interface: u8,
    ) -> HalResult<UsbHandle> {
        let (session_id, lease) = self
            .inner
            .usb_manager
            .open(owner, selector, UsbInterfaceClaim::new(interface as usize)?)
            .await?;
        Ok(UsbHandle {
            runtime: self.clone(),
            session_id,
            lease,
            closed: false,
        })
    }
    pub async fn usb_transfer(
        &self,
        session: SessionId,
        lease: &LeaseToken,
        transfer: UsbTransfer,
        timeout: Duration,
    ) -> HalResult<Bytes> {
        self.inner
            .usb_manager
            .transfer(session, lease, transfer, timeout)
            .await
    }
    pub async fn close_usb(&self, session: SessionId, lease: &LeaseToken) -> HalResult<()> {
        self.inner.usb_manager.close(session, lease).await
    }

    pub async fn enumerate_gpio(&self) -> HalResult<Vec<ResourceDescriptor>> {
        self.inner.gpio_manager.enumerate().await
    }
    pub async fn open_gpio(
        &self,
        owner: OwnerId,
        selector: ResourceSelector,
        lines: Vec<u32>,
        config: GpioLineConfig,
    ) -> HalResult<GpioHandle> {
        let (session_id, lease) = self
            .inner
            .gpio_manager
            .open(owner, selector, lines, config)
            .await?;
        Ok(GpioHandle {
            runtime: self.clone(),
            session_id,
            lease,
            closed: false,
        })
    }
    pub async fn gpio_read(&self, session: SessionId, lease: &LeaseToken) -> HalResult<Vec<bool>> {
        self.inner.gpio_manager.read(session, lease).await
    }
    pub async fn gpio_write(
        &self,
        session: SessionId,
        lease: &LeaseToken,
        values: Vec<bool>,
    ) -> HalResult<()> {
        self.inner.gpio_manager.write(session, lease, values).await
    }
    pub async fn gpio_next_edge(
        &self,
        session: SessionId,
        lease: &LeaseToken,
        request: GpioEdgeRequest,
        timeout: Duration,
    ) -> HalResult<Option<GpioEdgeEvent>> {
        self.inner
            .gpio_manager
            .next_edge(session, lease, request, timeout)
            .await
    }
    pub async fn close_gpio(&self, session: SessionId, lease: &LeaseToken) -> HalResult<()> {
        self.inner.gpio_manager.close(session, lease).await
    }

    pub async fn open_can(
        &self,
        owner: OwnerId,
        selector: ResourceSelector,
        mode: LeaseMode,
        config: CanOpenConfig,
        filters: CanFilterSet,
    ) -> HalResult<CanHandle> {
        let (session_id, lease) = self
            .inner
            .can_manager
            .open(owner, selector, mode, config, filters)
            .await?;
        Ok(CanHandle {
            runtime: self.clone(),
            session_id,
            lease,
            closed: false,
        })
    }

    pub async fn send_can(
        &self,
        session: SessionId,
        lease: &LeaseToken,
        frame: CanFrame,
    ) -> Result<(), CanBatchSendError> {
        self.send_can_batch(session, lease, vec![frame]).await
    }

    pub async fn send_can_batch(
        &self,
        session: SessionId,
        lease: &LeaseToken,
        frames: Vec<CanFrame>,
    ) -> Result<(), CanBatchSendError> {
        self.inner
            .can_manager
            .send_batch(session, lease, frames)
            .await
    }

    pub async fn receive_can(
        &self,
        session: SessionId,
        lease: &LeaseToken,
        max_frames: usize,
        timeout: Duration,
    ) -> HalResult<Vec<ReceivedCanFrame>> {
        self.inner
            .can_manager
            .receive(session, lease, max_frames, timeout)
            .await
    }

    pub async fn replace_can_filters(
        &self,
        session: SessionId,
        lease: &LeaseToken,
        filters: CanFilterSet,
    ) -> HalResult<()> {
        self.inner
            .can_manager
            .replace_filters(session, lease, filters)
            .await
    }

    pub async fn can_bus_status(
        &self,
        session: SessionId,
        lease: &LeaseToken,
    ) -> HalResult<CanBusStatus> {
        self.inner.can_manager.bus_status(session, lease).await
    }

    pub async fn close_can(&self, session: SessionId, lease: &LeaseToken) -> HalResult<()> {
        self.inner.can_manager.close(session, lease).await
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
                return Err(error.with_resource_id(resource_id));
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
            actor
                .wait_closed()
                .await
                .map_err(|error| error.with_resource_id(resource_id.clone()))?;
            return Err(runtime_error(
                "runtime.session.closed",
                ErrorCategory::Conflict,
                "serial.open",
                false,
                "the owner was revoked while the serial resource was opening",
            )
            .with_resource_id(resource_id));
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
            if let CloseAction::Wait(actor, _) = &action {
                actor.request_close();
            }
            action
        };
        match action {
            CloseAction::AlreadyClosed => Ok(()),
            CloseAction::Wait(actor, resource_id) => actor
                .wait_closed()
                .await
                .map_err(|error| error.with_resource_id(resource_id)),
        }
    }

    pub async fn revoke_owner(&self, owner: &OwnerId) -> HalResult<()> {
        let serial_result = self.revoke_serial_owner(owner).await;
        let can_result = self.inner.can_manager.revoke_owner(owner).await;
        let usb_result = self.inner.usb_manager.revoke_owner(owner).await;
        let gpio_result = self.inner.gpio_manager.revoke_owner(owner).await;
        match (serial_result, can_result, usb_result, gpio_result) {
            (Err(error), _, _, _) => Err(error),
            (Ok(()), Err(error), _, _) => Err(error),
            (Ok(()), Ok(()), Err(error), _) => Err(error),
            (Ok(()), Ok(()), Ok(()), Err(error)) => Err(error),
            (Ok(()), Ok(()), Ok(()), Ok(())) => Ok(()),
        }
    }

    async fn revoke_serial_owner(&self, owner: &OwnerId) -> HalResult<()> {
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
            }
            .map_err(|error| error.with_resource_id(target.resource_id));
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

    /// Returns the number of physical resource identities whose last exposed
    /// lease generation is retained for fencing.
    pub async fn retained_generation_count(&self) -> usize {
        self.inner.registry.lock().await.retained_generation_count()
            + self.inner.can_manager.retained_generation_count()
    }

    async fn request_serial<T>(
        &self,
        session_id: SessionId,
        lease: &LeaseToken,
        operation: &'static str,
        command: impl FnOnce(oneshot::Sender<HalResult<T>>) -> SerialCommand,
    ) -> HalResult<T> {
        let (reply_tx, reply_rx) = oneshot::channel();
        let resource_id = self.inner.registry.lock().await.enqueue(
            &session_id,
            lease,
            command(reply_tx),
            operation,
        )?;
        match reply_rx.await {
            Ok(result) => result,
            Err(_) => Err(runtime_error(
                "runtime.actor.unavailable",
                ErrorCategory::Internal,
                operation,
                false,
                "the serial actor dropped an operation reply",
            )),
        }
        .map_err(|error| error.with_resource_id(resource_id))
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

pub struct CanHandle {
    runtime: HalRuntime,
    session_id: SessionId,
    lease: LeaseToken,
    closed: bool,
}

pub struct UsbHandle {
    runtime: HalRuntime,
    session_id: SessionId,
    lease: LeaseToken,
    closed: bool,
}

pub struct GpioHandle {
    runtime: HalRuntime,
    session_id: SessionId,
    lease: LeaseToken,
    closed: bool,
}
impl GpioHandle {
    pub fn session_id(&self) -> SessionId {
        self.session_id.clone()
    }
    pub fn lease_token(&self) -> &LeaseToken {
        &self.lease
    }
    pub async fn read(&self) -> HalResult<Vec<bool>> {
        self.runtime.gpio_read(self.session_id(), &self.lease).await
    }
    pub async fn write(&self, values: Vec<bool>) -> HalResult<()> {
        self.runtime
            .gpio_write(self.session_id(), &self.lease, values)
            .await
    }
    pub async fn next_edge(
        &self,
        request: GpioEdgeRequest,
        timeout: Duration,
    ) -> HalResult<Option<GpioEdgeEvent>> {
        self.runtime
            .gpio_next_edge(self.session_id(), &self.lease, request, timeout)
            .await
    }
    pub async fn close(&mut self) -> HalResult<()> {
        let result = self
            .runtime
            .close_gpio(self.session_id(), &self.lease)
            .await;
        if result.is_ok() {
            self.closed = true;
        }
        result
    }
}
impl UsbHandle {
    pub fn session_id(&self) -> SessionId {
        self.session_id.clone()
    }
    pub fn lease_token(&self) -> &LeaseToken {
        &self.lease
    }
    pub async fn transfer(&self, transfer: UsbTransfer, timeout: Duration) -> HalResult<Bytes> {
        self.runtime
            .usb_transfer(self.session_id(), &self.lease, transfer, timeout)
            .await
    }
    pub async fn close(&mut self) -> HalResult<()> {
        let result = self.runtime.close_usb(self.session_id(), &self.lease).await;
        if result.is_ok() {
            self.closed = true;
        }
        result
    }
}

impl CanHandle {
    pub fn session_id(&self) -> SessionId {
        self.session_id.clone()
    }

    pub fn lease_token(&self) -> &LeaseToken {
        &self.lease
    }

    /// Transfers the session to a broker-facing caller using ID/token methods.
    pub fn into_parts(mut self) -> (SessionId, LeaseToken) {
        self.closed = true;
        (self.session_id.clone(), self.lease.clone())
    }

    pub async fn send(&self, frame: CanFrame) -> Result<(), CanBatchSendError> {
        self.runtime
            .send_can(self.session_id(), &self.lease, frame)
            .await
    }

    pub async fn send_batch(&self, frames: Vec<CanFrame>) -> Result<(), CanBatchSendError> {
        self.runtime
            .send_can_batch(self.session_id(), &self.lease, frames)
            .await
    }

    pub async fn receive(
        &self,
        max_frames: usize,
        timeout: Duration,
    ) -> HalResult<Vec<ReceivedCanFrame>> {
        self.runtime
            .receive_can(self.session_id(), &self.lease, max_frames, timeout)
            .await
    }

    pub async fn replace_filters(&self, filters: CanFilterSet) -> HalResult<()> {
        self.runtime
            .replace_can_filters(self.session_id(), &self.lease, filters)
            .await
    }

    pub async fn bus_status(&self) -> HalResult<CanBusStatus> {
        self.runtime
            .can_bus_status(self.session_id(), &self.lease)
            .await
    }

    /// Closes the session. A failed local admission leaves this handle open so
    /// the caller can retry; only a successful close marks it terminal.
    pub async fn close(&mut self) -> HalResult<()> {
        let result = self.runtime.close_can(self.session_id(), &self.lease).await;
        if result.is_ok() {
            self.closed = true;
        }
        result
    }
}

impl Drop for CanHandle {
    fn drop(&mut self) {
        if self.closed {
            return;
        }
        let runtime = self.runtime.clone();
        let session_id = self.session_id.clone();
        let lease = self.lease.clone();
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            handle.spawn(async move {
                let _ = runtime
                    .inner
                    .can_manager
                    .close_reliably(session_id, &lease)
                    .await;
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

    /// Transfers an opened session to a caller that will use the runtime's
    /// session-ID and fenced lease-token operations directly.
    ///
    /// This is the broker handoff seam: it suppresses the handle's RAII close
    /// while retaining all ownership and fencing state in [`HalRuntime`].
    pub fn into_parts(mut self) -> (SessionId, LeaseToken) {
        self.closed = true;
        (self.session_id.clone(), self.lease.clone())
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

async fn wait_until_done(mut done: watch::Receiver<Option<HalResult<()>>>) -> HalResult<()> {
    loop {
        if let Some(result) = done.borrow().clone() {
            return result;
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

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::time::Duration;

    use bytes::Bytes;
    use seeed_hal_core::{HalResult, OwnerId, ResourceDescriptor, SessionId};
    use seeed_hal_serial::{ControlLines, SerialAdapter, SerialConfig, SerialSession};
    use seeed_hal_testkit::VirtualSerialAdapter;
    use tokio::sync::Notify;

    use super::{ActorMetadata, HalRuntime, spawn_serial_actor, wait_until_cancelled};

    struct PendingCloseSession {
        inner: Box<dyn SerialSession>,
        close_started: Arc<Notify>,
    }

    #[async_trait::async_trait]
    impl SerialSession for PendingCloseSession {
        fn descriptor(&self) -> &ResourceDescriptor {
            self.inner.descriptor()
        }

        async fn read(&mut self, max_bytes: usize) -> HalResult<Bytes> {
            self.inner.read(max_bytes).await
        }

        async fn write_all(&mut self, bytes: &[u8]) -> HalResult<()> {
            self.inner.write_all(bytes).await
        }

        async fn flush(&mut self) -> HalResult<()> {
            self.inner.flush().await
        }

        async fn set_control_lines(&mut self, lines: ControlLines) -> HalResult<()> {
            self.inner.set_control_lines(lines).await
        }

        async fn close(&mut self) -> HalResult<()> {
            self.close_started.notify_one();
            std::future::pending().await
        }
    }

    #[tokio::test(start_paused = true)]
    async fn pending_open_revoke_propagates_actor_close_timeout() {
        let adapter = VirtualSerialAdapter::loopback("serial:virtual:pending-open-revoke");
        let runtime = HalRuntime::builder()
            .serial_adapter(adapter.clone())
            .serial_close_timeout(Duration::from_millis(25))
            .build();
        let descriptor = adapter.enumerate().await.unwrap().remove(0);
        let owner = OwnerId::parse("client-a").unwrap();
        let session_id = SessionId::parse("pending-open-revoke").unwrap();
        let reservation = runtime.inner.registry.lock().await.reserve_open(
            descriptor.id().clone(),
            session_id.clone(),
            owner.clone(),
        );
        let reservation = reservation.unwrap();

        let revoking_runtime = runtime.clone();
        let revoking_owner = owner.clone();
        let revoke =
            tokio::spawn(async move { revoking_runtime.revoke_owner(&revoking_owner).await });
        wait_until_cancelled(reservation.cancellation).await;

        let inner = adapter
            .open(&descriptor.selector(), SerialConfig::default())
            .await
            .unwrap();
        let close_started = Arc::new(Notify::new());
        let metadata = ActorMetadata {
            session_id,
            close_timeout: Duration::from_millis(25),
        };
        let actor = spawn_serial_actor(
            Box::new(PendingCloseSession {
                inner,
                close_started: close_started.clone(),
            }),
            Arc::downgrade(&runtime.inner.registry),
            runtime.inner.events.clone(),
            metadata.clone(),
        );
        let accepted = runtime.inner.registry.lock().await.finish_open(
            &metadata,
            actor.clone(),
            &runtime.inner.events,
        );
        assert!(!accepted);

        actor.request_close();
        close_started.notified().await;
        tokio::time::advance(Duration::from_millis(25)).await;
        let error = revoke.await.unwrap().unwrap_err();
        assert_eq!(error.name().as_str(), "runtime.session.close_timeout");
        assert_eq!(error.resource_id(), Some(descriptor.id()));

        runtime
            .open_serial(
                OwnerId::parse("client-b").unwrap(),
                descriptor.selector(),
                SerialConfig::default(),
            )
            .await
            .unwrap()
            .close()
            .await
            .unwrap();
    }
}
