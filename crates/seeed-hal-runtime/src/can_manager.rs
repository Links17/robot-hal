use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use seeed_hal_can::{
    CanAdapter, CanBatchSendError, CanBusStatus, CanFilterSet, CanFrame, CanMode, CanOpenConfig,
    MAX_CAN_BATCH_FRAMES, ReceivedCanFrame, can_classic_capability, can_configure_capability,
    can_fd_capability,
};
use seeed_hal_core::{
    ErrorCategory, HalResult, LeaseMode, LeaseToken, OwnerId, ResourceDescriptor, ResourceId,
    ResourceSelector, SessionId, resolve_resource,
};
use tokio::sync::{oneshot, watch};
use uuid::Uuid;

use crate::can_actor::{ActorSessionSpec, CanActorHandle, CanCommand, spawn_can_actor};
use crate::can_lease_table::{CanLeaseTable, CanReservation};
use crate::events::{EventPublisher, RuntimeEventKind};
use crate::runtime_error;

const CLOSED_SESSION_CAPACITY: usize = 256;

#[derive(Clone)]
struct DiscoveryRecord {
    adapter_index: usize,
    descriptor: ResourceDescriptor,
}

struct PendingSession {
    reservation: CanReservation,
    cancellation: watch::Sender<bool>,
    done: watch::Sender<bool>,
    cancelled: Arc<AtomicBool>,
    actor_epoch: Option<u64>,
}

struct ActiveSession {
    resource_id: ResourceId,
    owner_id: OwnerId,
    token: LeaseToken,
    actor: CanActorHandle,
    actor_epoch: u64,
    termination_expected: Arc<AtomicBool>,
    termination_failed: Arc<AtomicBool>,
    terminal_health_emitted: Arc<AtomicBool>,
    closing: bool,
}

struct ActorEntry {
    handle: CanActorHandle,
    epoch: u64,
}

struct ClosedSession {
    token: LeaseToken,
    resource_id: ResourceId,
}

#[derive(Default)]
struct ManagerState {
    leases: CanLeaseTable,
    pending: HashMap<SessionId, PendingSession>,
    active: HashMap<SessionId, ActiveSession>,
    actors: HashMap<ResourceId, ActorEntry>,
    next_actor_epoch: u64,
    closed: HashMap<SessionId, ClosedSession>,
    closed_order: VecDeque<SessionId>,
}

pub(crate) struct CanManager {
    adapters: Vec<Arc<dyn CanAdapter>>,
    state: Arc<Mutex<ManagerState>>,
    events: EventPublisher,
    rx_capacity: usize,
    tx_capacity: usize,
    close_timeout: Duration,
    #[cfg(test)]
    open_commit_gate: Option<Arc<ManagerTestGate>>,
    #[cfg(test)]
    close_finalize_gate: Option<Arc<ManagerTestGate>>,
    #[cfg(test)]
    close_event_gate: Option<Arc<ManagerThreadGate>>,
    #[cfg(test)]
    open_reserve_signal: Option<Arc<ManagerStateSignal>>,
}

#[cfg(test)]
#[derive(Default)]
struct ManagerTestGate {
    reached: AtomicBool,
    released: AtomicBool,
    changed: tokio::sync::Notify,
}

#[cfg(test)]
impl ManagerTestGate {
    async fn wait(&self) {
        self.reached.store(true, Ordering::Release);
        self.changed.notify_one();
        while !self.released.load(Ordering::Acquire) {
            self.changed.notified().await;
        }
    }

    async fn wait_reached(&self) {
        while !self.reached.load(Ordering::Acquire) {
            self.changed.notified().await;
        }
    }

    fn release(&self) {
        self.released.store(true, Ordering::Release);
        self.changed.notify_one();
    }
}

#[cfg(test)]
#[derive(Default)]
struct ManagerThreadGate {
    reached: AtomicBool,
    released: Mutex<bool>,
    changed: std::sync::Condvar,
}

#[cfg(test)]
impl ManagerThreadGate {
    fn wait(&self) {
        self.reached.store(true, Ordering::Release);
        let mut released = self
            .released
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        while !*released {
            released = self
                .changed
                .wait(released)
                .unwrap_or_else(|poisoned| poisoned.into_inner());
        }
    }

    async fn wait_reached(&self) {
        while !self.reached.load(Ordering::Acquire) {
            tokio::task::yield_now().await;
        }
    }

    fn release(&self) {
        *self
            .released
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = true;
        self.changed.notify_one();
    }
}

#[cfg(test)]
#[derive(Default)]
struct ManagerStateSignal {
    armed: AtomicBool,
    reached: AtomicBool,
    changed: tokio::sync::Notify,
}

#[cfg(test)]
impl ManagerStateSignal {
    fn arm(&self) {
        self.armed.store(true, Ordering::Release);
    }

    fn reach_if_armed(&self) {
        if self.armed.load(Ordering::Acquire) {
            self.reached.store(true, Ordering::Release);
            self.changed.notify_one();
        }
    }

    async fn wait_reached(&self) {
        while !self.reached.load(Ordering::Acquire) {
            self.changed.notified().await;
        }
    }
}

impl CanManager {
    pub(crate) fn new(
        adapters: Vec<Arc<dyn CanAdapter>>,
        events: EventPublisher,
        rx_capacity: usize,
        tx_capacity: usize,
        close_timeout: Duration,
    ) -> Self {
        Self {
            adapters,
            state: Arc::new(Mutex::new(ManagerState::default())),
            events,
            rx_capacity,
            tx_capacity,
            close_timeout,
            #[cfg(test)]
            open_commit_gate: None,
            #[cfg(test)]
            close_finalize_gate: None,
            #[cfg(test)]
            close_event_gate: None,
            #[cfg(test)]
            open_reserve_signal: None,
        }
    }

    pub(crate) async fn enumerate(&self) -> HalResult<Vec<ResourceDescriptor>> {
        Ok(self
            .discovery_records("can.enumerate")
            .await?
            .into_iter()
            .map(|record| record.descriptor)
            .collect())
    }

    pub(crate) async fn open(
        &self,
        owner_id: OwnerId,
        selector: ResourceSelector,
        mode: LeaseMode,
        config: CanOpenConfig,
        filters: CanFilterSet,
    ) -> HalResult<(SessionId, LeaseToken)> {
        validate_open_mode(mode, &config, selector.id())?;
        let records = self.discovery_records("can.open").await?;
        if records
            .iter()
            .filter(|record| record.descriptor.id() == selector.id())
            .take(2)
            .count()
            > 1
        {
            return Err(runtime_error(
                "runtime.resource.ambiguous",
                ErrorCategory::Conflict,
                "can.open",
                false,
                "the selected CAN resource ID is duplicated across combined discovery",
            )
            .with_resource_id(selector.id().clone()));
        }
        let descriptors: Vec<_> = records
            .iter()
            .map(|record| record.descriptor.clone())
            .collect();
        let required_capability = required_capability(&config);
        let selected = resolve_resource(&descriptors, &selector, &required_capability, "can.open")?;
        let selected_index = descriptors
            .iter()
            .position(|descriptor| std::ptr::eq(descriptor, selected))
            .expect("the canonical CAN resolver returned a member of its input");
        let record = &records[selected_index];
        let resource_id = record.descriptor.id().clone();
        let session_id = SessionId::parse(Uuid::new_v4().to_string())?;
        let activation = Arc::new(Mutex::new(None));
        let cancelled = Arc::new(AtomicBool::new(false));
        let termination_expected = Arc::new(AtomicBool::new(false));
        let termination_failed = Arc::new(AtomicBool::new(false));
        let terminal_health_emitted = Arc::new(AtomicBool::new(false));
        let (cancellation, done) = {
            #[cfg(test)]
            if let Some(signal) = &self.open_reserve_signal {
                signal.reach_if_armed();
            }
            let mut state = lock_state(&self.state);
            self.reconcile_finished_actor(&mut state, &resource_id);
            let reservation = state.leases.reserve(
                resource_id.clone(),
                session_id.clone(),
                owner_id.clone(),
                mode,
            )?;
            let (cancel_tx, cancel_rx) = watch::channel(false);
            let (done_tx, _) = watch::channel(false);
            state.pending.insert(
                session_id.clone(),
                PendingSession {
                    reservation,
                    cancellation: cancel_tx,
                    done: done_tx.clone(),
                    cancelled: Arc::clone(&cancelled),
                    actor_epoch: None,
                },
            );
            (cancel_rx, done_tx)
        };
        let mut guard = PendingCanOpen::new(
            Arc::clone(&self.state),
            session_id.clone(),
            Arc::clone(&activation),
            Arc::clone(&cancelled),
            done,
            self.close_timeout,
        );
        let session_spec = ActorSessionSpec {
            session_id: session_id.clone(),
            owner_id: owner_id.clone(),
            config: config.clone(),
            filters,
            activation,
            cancelled,
            cleanup_done: guard.done.clone(),
            termination_expected: Arc::clone(&termination_expected),
            termination_failed: Arc::clone(&termination_failed),
        };

        let (actor, added) = {
            let mut state = lock_state(&self.state);
            self.reconcile_finished_actor(&mut state, &resource_id);
            let (actor, epoch, reply_rx) = if let Some(entry) = state.actors.get(&resource_id) {
                let actor = entry.handle.clone();
                let epoch = entry.epoch;
                let (reply_tx, reply_rx) = oneshot::channel();
                actor.try_command(
                    CanCommand::AddSession {
                        session: session_spec,
                        reply: reply_tx,
                    },
                    "can.open",
                )?;
                (actor, epoch, reply_rx)
            } else {
                let adapter = Arc::clone(&self.adapters[record.adapter_index]);
                let epoch = state.next_actor_epoch.checked_add(1).ok_or_else(|| {
                    runtime_error(
                        "runtime.actor.epoch_exhausted",
                        ErrorCategory::Internal,
                        "can.open",
                        false,
                        "the CAN actor lifecycle epoch reached u64::MAX",
                    )
                })?;
                state.next_actor_epoch = epoch;
                let (actor, reply_rx) = spawn_can_actor(
                    adapter,
                    selector,
                    session_spec,
                    self.rx_capacity,
                    self.tx_capacity,
                    self.close_timeout,
                    self.events.clone(),
                )?;
                state.actors.insert(
                    resource_id.clone(),
                    ActorEntry {
                        handle: actor.clone(),
                        epoch,
                    },
                );
                (actor, epoch, reply_rx)
            };
            if let Some(pending) = state.pending.get_mut(&session_id) {
                pending.actor_epoch = Some(epoch);
            }
            (actor, reply_rx)
        };
        guard.set_actor(actor.clone());

        let add_result = tokio::select! {
            result = tokio::time::timeout(self.close_timeout.saturating_add(Duration::from_millis(20)), added) => match result {
                Ok(reply) => reply.map_err(|_| actor_unavailable("can.open", &resource_id))?,
                Err(_) => Err(runtime_error(
                    "runtime.transport.timeout",
                    ErrorCategory::Unavailable,
                    "can.open",
                    true,
                    "the CAN actor did not complete session admission before the deadline",
                ).with_resource_id(resource_id.clone())),
            },
            _ = wait_cancelled(cancellation) => Err(session_closed("can.open", &resource_id)),
        };
        if let Err(error) = add_result {
            let _ = guard.cleanup().await;
            return Err(error.with_resource_id(resource_id));
        }

        #[cfg(test)]
        if let Some(gate) = &self.open_commit_gate {
            gate.wait().await;
        }

        let commit_result = {
            let mut state = lock_state(&self.state);
            self.reconcile_finished_actor(&mut state, &resource_id);
            match state.pending.remove(&session_id) {
                Some(pending) => state.leases.commit(pending.reservation).inspect(|token| {
                    let actor_epoch = pending
                        .actor_epoch
                        .expect("admitted CAN pending session has an actor epoch");
                    state.active.insert(
                        session_id.clone(),
                        ActiveSession {
                            resource_id: resource_id.clone(),
                            owner_id: owner_id.clone(),
                            token: token.clone(),
                            actor,
                            actor_epoch,
                            termination_expected,
                            termination_failed,
                            terminal_health_emitted,
                            closing: false,
                        },
                    );
                    self.events.publish(
                        RuntimeEventKind::SessionOpened,
                        resource_id.clone(),
                        session_id.clone(),
                        owner_id.clone(),
                        token.generation(),
                    );
                    guard.activate(token.clone());
                    pending.done.send_replace(true);
                }),
                None => Err(session_closed("can.open", &resource_id)),
            }
        };
        let token = match commit_result {
            Ok(token) => token,
            Err(error) => {
                let _ = guard.cleanup().await;
                return Err(error);
            }
        };
        guard.disarm();
        Ok((session_id, token))
    }

    pub(crate) async fn send_batch(
        &self,
        session_id: SessionId,
        token: &LeaseToken,
        frames: Vec<CanFrame>,
    ) -> Result<(), CanBatchSendError> {
        let (actor, resource_id) = self
            .session_actor(&session_id, token, LeaseMode::Control, "can.send_batch")
            .map_err(CanBatchSendError::new)?;
        if frames.is_empty() || frames.len() > MAX_CAN_BATCH_FRAMES {
            return Err(CanBatchSendError::new(
                runtime_error(
                    "can.frame.invalid",
                    ErrorCategory::InvalidArgument,
                    "can.send_batch",
                    false,
                    "a CAN send batch must contain 1..=64 frames",
                )
                .with_resource_id(resource_id),
            ));
        }
        for frame in &frames {
            frame.validate().map_err(|error| {
                CanBatchSendError::new(error.with_resource_id(resource_id.clone()))
            })?;
        }
        let (reply_tx, reply_rx) = oneshot::channel();
        let count = frames.len();
        actor
            .try_send_batch(
                CanCommand::SendBatch {
                    session_id,
                    frames,
                    reply: reply_tx,
                },
                count,
            )
            .map_err(|error| CanBatchSendError::new(error.with_resource_id(resource_id.clone())))?;
        reply_rx.await.unwrap_or_else(|_| {
            Err(CanBatchSendError::new(actor_unavailable(
                "can.send_batch",
                &resource_id,
            )))
        })
    }

    pub(crate) async fn receive(
        &self,
        session_id: SessionId,
        token: &LeaseToken,
        max_frames: usize,
        timeout: Duration,
    ) -> HalResult<Vec<ReceivedCanFrame>> {
        let (actor, resource_id) =
            self.session_actor(&session_id, token, LeaseMode::Observe, "can.receive")?;
        if !(1..=MAX_CAN_BATCH_FRAMES).contains(&max_frames) {
            return Err(runtime_error(
                "can.receive.invalid_limit",
                ErrorCategory::InvalidArgument,
                "can.receive",
                false,
                "CAN receive max_frames must be 1..=64",
            )
            .with_resource_id(resource_id));
        }
        let (reply_tx, reply_rx) = oneshot::channel();
        actor
            .try_command(
                CanCommand::Receive {
                    session_id,
                    max_frames,
                    deadline: Instant::now()
                        .checked_add(timeout)
                        .unwrap_or_else(Instant::now),
                    reply: reply_tx,
                },
                "can.receive",
            )
            .map_err(|error| error.with_resource_id(resource_id.clone()))?;
        match tokio::time::timeout(timeout.saturating_add(Duration::from_millis(20)), reply_rx)
            .await
        {
            Ok(Ok(result)) => result,
            Ok(Err(_)) => Err(actor_unavailable("can.receive", &resource_id)),
            Err(_) => Err(runtime_error(
                "runtime.transport.timeout",
                ErrorCategory::Unavailable,
                "can.receive",
                true,
                "the CAN receive request exceeded its finite deadline",
            )
            .with_resource_id(resource_id)),
        }
    }

    pub(crate) async fn replace_filters(
        &self,
        session_id: SessionId,
        token: &LeaseToken,
        filters: CanFilterSet,
    ) -> HalResult<()> {
        let (actor, resource_id) = self.session_actor(
            &session_id,
            token,
            LeaseMode::Observe,
            "can.replace_filters",
        )?;
        let (reply_tx, reply_rx) = oneshot::channel();
        actor
            .try_command(
                CanCommand::ReplaceFilters {
                    session_id,
                    filters,
                    reply: reply_tx,
                },
                "can.replace_filters",
            )
            .map_err(|error| error.with_resource_id(resource_id.clone()))?;
        match tokio::time::timeout(self.close_timeout, reply_rx).await {
            Ok(Ok(result)) => result,
            Ok(Err(_)) => Err(actor_unavailable("can.replace_filters", &resource_id)),
            Err(_) => Err(operation_timeout("can.replace_filters", &resource_id)),
        }
    }

    pub(crate) async fn bus_status(
        &self,
        session_id: SessionId,
        token: &LeaseToken,
    ) -> HalResult<CanBusStatus> {
        let (actor, resource_id) =
            self.session_actor(&session_id, token, LeaseMode::Observe, "can.status")?;
        let (reply_tx, reply_rx) = oneshot::channel();
        actor
            .try_command(
                CanCommand::BusStatus {
                    session_id,
                    reply: reply_tx,
                },
                "can.status",
            )
            .map_err(|error| error.with_resource_id(resource_id.clone()))?;
        match tokio::time::timeout(self.close_timeout, reply_rx).await {
            Ok(Ok(result)) => result,
            Ok(Err(_)) => Err(actor_unavailable("can.status", &resource_id)),
            Err(_) => Err(operation_timeout("can.status", &resource_id)),
        }
    }

    pub(crate) async fn close(&self, session_id: SessionId, token: &LeaseToken) -> HalResult<()> {
        let (actor, resource_id, actor_epoch) = {
            let mut state = lock_state(&self.state);
            if let Some(resource_id) = state
                .active
                .get(&session_id)
                .map(|entry| entry.resource_id.clone())
            {
                self.reconcile_finished_actor(&mut state, &resource_id);
            }
            let Some(entry) = state.active.get(&session_id) else {
                return closed_result(&state, &session_id, token, "can.close");
            };
            let entry_resource = entry.resource_id.clone();
            let entry_owner = entry.owner_id.clone();
            let entry_actor = entry.actor.clone();
            let entry_actor_epoch = entry.actor_epoch;
            let entry_closing = entry.closing;
            state.leases.validate(
                &entry_resource,
                &session_id,
                &entry_owner,
                token,
                LeaseMode::Observe,
                "can.close",
            )?;
            if entry_closing {
                return Err(session_closed("can.close", &entry_resource));
            }
            let (reply_tx, reply_rx) = oneshot::channel();
            entry_actor
                .try_cleanup(CanCommand::RemoveSession {
                    session_id: session_id.clone(),
                    cleanup_done: None,
                    reply: reply_tx,
                })
                .map_err(|error| error.with_resource_id(entry_resource.clone()))?;
            state
                .active
                .get_mut(&session_id)
                .expect("validated CAN session remains present while manager state is locked")
                .closing = true;
            ((entry_actor, reply_rx), entry_resource, entry_actor_epoch)
        };
        let (actor_handle, reply_rx) = actor;
        let (result, last_session) = match tokio::time::timeout(self.close_timeout, reply_rx).await
        {
            Ok(Ok(outcome)) => (outcome.result, outcome.last_session),
            Ok(Err(_)) => (Err(actor_unavailable("can.close", &resource_id)), false),
            Err(_) => (
                Err(runtime_error(
                    "runtime.session.close_timeout",
                    ErrorCategory::Unavailable,
                    "can.close",
                    false,
                    format!(
                        "CAN session close exceeded its {:?} deadline",
                        self.close_timeout
                    ),
                )
                .with_resource_id(resource_id.clone())),
                false,
            ),
        };
        if last_session {
            let _ = tokio::time::timeout(self.close_timeout, actor_handle.wait_finished()).await;
        }

        #[cfg(test)]
        if let Some(gate) = &self.close_finalize_gate {
            gate.wait().await;
        }
        self.finish_close(
            &session_id,
            token,
            &resource_id,
            result.is_err(),
            actor_handle.is_finished(),
            actor_epoch,
        );
        result
    }

    pub(crate) async fn close_reliably(
        &self,
        session_id: SessionId,
        token: &LeaseToken,
    ) -> HalResult<()> {
        let deadline = Instant::now()
            .checked_add(self.close_timeout)
            .unwrap_or_else(Instant::now);
        loop {
            match self.close(session_id.clone(), token).await {
                Err(error)
                    if error.name().as_str() == "runtime.queue.full"
                        && Instant::now() < deadline =>
                {
                    tokio::time::sleep(Duration::from_millis(1)).await;
                }
                result => return result,
            }
        }
    }

    pub(crate) async fn revoke_owner(&self, owner_id: &OwnerId) -> HalResult<()> {
        let (pending, active) = {
            let mut state = lock_state(&self.state);
            let pending_ids: Vec<_> = state
                .pending
                .iter()
                .filter(|(_, entry)| entry.reservation.owner_id() == owner_id)
                .map(|(session_id, _)| session_id.clone())
                .collect();
            let mut pending_done = Vec::new();
            for session_id in pending_ids {
                if let Some(entry) = state.pending.remove(&session_id) {
                    let _ = state.leases.cancel(&entry.reservation);
                    entry.cancelled.store(true, Ordering::Release);
                    entry.cancellation.send_replace(true);
                    pending_done.push(entry.done.subscribe());
                }
            }
            let active: Vec<_> = state
                .active
                .iter()
                .filter(|(_, entry)| &entry.owner_id == owner_id)
                .map(|(session_id, entry)| (session_id.clone(), entry.token.clone()))
                .collect();
            (pending_done, active)
        };

        let mut first_error = None;
        for done in pending {
            let result = wait_pending_done(done, self.close_timeout).await;
            if first_error.is_none() {
                first_error = result.err();
            }
        }
        for (session_id, token) in active {
            let result = self.close_reliably(session_id, &token).await;
            if first_error.is_none() {
                first_error = result.err();
            }
        }
        first_error.map_or(Ok(()), Err)
    }

    pub(crate) fn retained_generation_count(&self) -> usize {
        lock_state(&self.state).leases.retained_generation_count()
    }

    async fn discovery_records(&self, operation: &'static str) -> HalResult<Vec<DiscoveryRecord>> {
        if self.adapters.is_empty() {
            return Err(runtime_error(
                "runtime.adapter.not_configured",
                ErrorCategory::Unavailable,
                operation,
                false,
                "no CAN adapter was registered with the runtime",
            ));
        }
        let runtime = tokio::runtime::Handle::try_current().map_err(|_| {
            runtime_error(
                "runtime.executor.unavailable",
                ErrorCategory::Unavailable,
                operation,
                true,
                "CAN discovery requires an active Tokio runtime handle",
            )
        })?;
        let mut receivers = Vec::with_capacity(self.adapters.len());
        for (adapter_index, adapter) in self.adapters.iter().enumerate() {
            let adapter = Arc::clone(adapter);
            let runtime = runtime.clone();
            let worker_timeout = self.close_timeout;
            let (reply_tx, reply_rx) = oneshot::channel();
            let name = format!("seeed-hal-can-enumerate-{adapter_index}");
            std::thread::Builder::new()
                .name(name)
                .spawn(move || {
                    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        match runtime.block_on(async {
                            tokio::time::timeout(worker_timeout, adapter.enumerate()).await
                        }) {
                            Ok(result) => result,
                            Err(_) => Err(runtime_error(
                                "runtime.transport.timeout",
                                ErrorCategory::Unavailable,
                                "can.enumerate",
                                true,
                                format!(
                                    "CAN adapter discovery exceeded its {worker_timeout:?} deadline"
                                ),
                            )),
                        }
                    }))
                    .unwrap_or_else(|_| {
                        Err(runtime_error(
                            "runtime.actor.unavailable",
                            ErrorCategory::Internal,
                            "can.enumerate",
                            false,
                            "a CAN adapter panicked during discovery",
                        ))
                    });
                    let _ = reply_tx.send(result);
                })
                .map_err(|error| {
                    runtime_error(
                        "runtime.actor.spawn_failed",
                        ErrorCategory::Unavailable,
                        operation,
                        true,
                        format!("failed to spawn CAN discovery worker: {error}"),
                    )
                })?;
            receivers.push((adapter_index, reply_rx));
        }
        let mut records = Vec::new();
        for (adapter_index, receiver) in receivers {
            let descriptors = tokio::time::timeout(
                self.close_timeout.saturating_add(Duration::from_millis(20)),
                receiver,
            )
            .await
            .map_err(|_| {
                runtime_error(
                    "runtime.transport.timeout",
                    ErrorCategory::Unavailable,
                    operation,
                    true,
                    "CAN adapter discovery exceeded its finite deadline",
                )
            })?
            .map_err(|_| {
                runtime_error(
                    "runtime.actor.unavailable",
                    ErrorCategory::Internal,
                    operation,
                    false,
                    "a CAN discovery worker exited without a reply",
                )
            })??;
            records.extend(descriptors.into_iter().map(|descriptor| DiscoveryRecord {
                adapter_index,
                descriptor,
            }));
        }
        records.sort_by(|left, right| {
            left.descriptor
                .id()
                .cmp(right.descriptor.id())
                .then(left.adapter_index.cmp(&right.adapter_index))
                .then(left.descriptor.endpoint().cmp(right.descriptor.endpoint()))
        });
        Ok(records)
    }

    fn session_actor(
        &self,
        session_id: &SessionId,
        token: &LeaseToken,
        required_mode: LeaseMode,
        operation: &'static str,
    ) -> HalResult<(CanActorHandle, ResourceId)> {
        let mut state = lock_state(&self.state);
        if let Some(resource_id) = state
            .active
            .get(session_id)
            .map(|entry| entry.resource_id.clone())
        {
            self.reconcile_finished_actor(&mut state, &resource_id);
        }
        let Some(entry) = state.active.get(session_id) else {
            return Err(missing_session_error(&state, session_id, token, operation));
        };
        state.leases.validate(
            &entry.resource_id,
            session_id,
            &entry.owner_id,
            token,
            required_mode,
            operation,
        )?;
        if entry.closing {
            return Err(session_closed(operation, &entry.resource_id));
        }
        Ok((entry.actor.clone(), entry.resource_id.clone()))
    }

    fn reconcile_finished_actor(&self, state: &mut ManagerState, resource_id: &ResourceId) {
        let Some(epoch) = state
            .actors
            .get(resource_id)
            .and_then(|entry| entry.handle.is_finished().then_some(entry.epoch))
        else {
            return;
        };
        state.actors.remove(resource_id);

        let active_ids: Vec<_> = state
            .active
            .iter()
            .filter(|(_, session)| {
                &session.resource_id == resource_id && session.actor_epoch == epoch
            })
            .map(|(session_id, _)| session_id.clone())
            .collect();
        for session_id in active_ids {
            let Some(session) = state.active.remove(&session_id) else {
                continue;
            };
            let _ = state
                .leases
                .release(resource_id, &session_id, &session.token);
            let expected = session.termination_expected.load(Ordering::Acquire);
            let failed = session.termination_failed.load(Ordering::Acquire);
            if (!expected || failed)
                && !session.terminal_health_emitted.swap(true, Ordering::AcqRel)
            {
                self.events.publish(
                    RuntimeEventKind::CanBusUnknown,
                    resource_id.clone(),
                    session_id.clone(),
                    session.owner_id.clone(),
                    session.token.generation(),
                );
            }
            self.events.publish(
                RuntimeEventKind::SessionClosed,
                resource_id.clone(),
                session_id.clone(),
                session.owner_id,
                session.token.generation(),
            );
            remember_closed(state, session_id, session.token, resource_id.clone());
        }

        let pending_ids: Vec<_> = state
            .pending
            .iter()
            .filter(|(_, session)| session.actor_epoch == Some(epoch))
            .map(|(session_id, _)| session_id.clone())
            .collect();
        for session_id in pending_ids {
            if let Some(session) = state.pending.remove(&session_id) {
                let _ = state.leases.cancel(&session.reservation);
                session.cancelled.store(true, Ordering::Release);
                session.cancellation.send_replace(true);
                session.done.send_replace(true);
            }
        }
    }

    fn finish_close(
        &self,
        session_id: &SessionId,
        token: &LeaseToken,
        resource_id: &ResourceId,
        close_failed: bool,
        actor_finished: bool,
        actor_epoch: u64,
    ) {
        let mut state = lock_state(&self.state);
        let Some(entry) = state.active.remove(session_id) else {
            return;
        };
        let _ = state.leases.release(resource_id, session_id, token);
        remember_closed(
            &mut state,
            session_id.clone(),
            entry.token.clone(),
            entry.resource_id.clone(),
        );
        #[cfg(test)]
        if let Some(gate) = &self.close_event_gate {
            gate.wait();
        }
        if close_failed && !entry.terminal_health_emitted.swap(true, Ordering::AcqRel) {
            self.events.publish(
                RuntimeEventKind::CanBusUnknown,
                resource_id.clone(),
                session_id.clone(),
                entry.owner_id.clone(),
                entry.token.generation(),
            );
        }
        self.events.publish(
            RuntimeEventKind::SessionClosed,
            resource_id.clone(),
            session_id.clone(),
            entry.owner_id,
            entry.token.generation(),
        );
        if actor_finished
            && state
                .actors
                .get(resource_id)
                .is_some_and(|current| current.epoch == actor_epoch && current.handle.is_finished())
        {
            state.actors.remove(resource_id);
        }
    }
}

fn remember_closed(
    state: &mut ManagerState,
    session_id: SessionId,
    token: LeaseToken,
    resource_id: ResourceId,
) {
    while state.closed_order.len() >= CLOSED_SESSION_CAPACITY {
        if let Some(expired) = state.closed_order.pop_front() {
            state.closed.remove(&expired);
        }
    }
    state.closed_order.push_back(session_id.clone());
    state
        .closed
        .insert(session_id, ClosedSession { token, resource_id });
}

struct PendingCanOpen {
    state: Arc<Mutex<ManagerState>>,
    session_id: SessionId,
    activation: Arc<Mutex<Option<LeaseToken>>>,
    cancelled: Arc<AtomicBool>,
    done: watch::Sender<bool>,
    actor: Option<CanActorHandle>,
    actor_assigned: bool,
    close_timeout: Duration,
    armed: bool,
}

impl PendingCanOpen {
    fn new(
        state: Arc<Mutex<ManagerState>>,
        session_id: SessionId,
        activation: Arc<Mutex<Option<LeaseToken>>>,
        cancelled: Arc<AtomicBool>,
        done: watch::Sender<bool>,
        close_timeout: Duration,
    ) -> Self {
        Self {
            state,
            session_id,
            activation,
            cancelled,
            done,
            actor: None,
            actor_assigned: false,
            close_timeout,
            armed: true,
        }
    }

    fn set_actor(&mut self, actor: CanActorHandle) {
        self.actor = Some(actor);
        self.actor_assigned = true;
    }

    fn activate(&self, token: LeaseToken) {
        *self
            .activation
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(token);
    }

    async fn cleanup(&mut self) -> HalResult<()> {
        self.cancelled.store(true, Ordering::Release);
        let Some(actor) = self.actor.take() else {
            if !self.actor_assigned {
                self.done.send_replace(true);
            }
            return Ok(());
        };
        reliable_actor_remove(
            actor,
            self.session_id.clone(),
            self.done.clone(),
            self.close_timeout,
        )
        .await
    }

    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for PendingCanOpen {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        self.cancelled.store(true, Ordering::Release);
        let mut state = lock_state(&self.state);
        if let Some(pending) = state.pending.remove(&self.session_id) {
            let _ = state.leases.cancel(&pending.reservation);
        }
        drop(state);
        let Some(actor) = self.actor.take() else {
            if !self.actor_assigned {
                self.done.send_replace(true);
            }
            return;
        };
        let session_id = self.session_id.clone();
        let done = self.done.clone();
        let timeout = self.close_timeout;
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            runtime.spawn(async move {
                let _ = reliable_actor_remove(actor, session_id, done, timeout).await;
            });
        }
    }
}

fn validate_open_mode(
    mode: LeaseMode,
    config: &CanOpenConfig,
    resource_id: &ResourceId,
) -> HalResult<()> {
    if matches!(config, CanOpenConfig::Configure(_)) && mode != LeaseMode::Maintenance {
        return Err(runtime_error(
            "runtime.lease.mode_denied",
            ErrorCategory::Conflict,
            "can.open",
            false,
            "CAN Configure requires a Maintenance lease",
        )
        .with_resource_id(resource_id.clone()));
    }
    Ok(())
}

fn required_capability(config: &CanOpenConfig) -> seeed_hal_core::CapabilityId {
    match config {
        CanOpenConfig::Configure(_) => can_configure_capability(),
        CanOpenConfig::Attach(expectation) if expectation.mode() == Some(CanMode::Fd) => {
            can_fd_capability()
        }
        CanOpenConfig::Attach(_) => can_classic_capability(),
    }
}

fn closed_result(
    state: &ManagerState,
    session_id: &SessionId,
    supplied: &LeaseToken,
    operation: &'static str,
) -> HalResult<()> {
    let Some(closed) = state.closed.get(session_id) else {
        return Err(session_not_found(operation));
    };
    validate_closed_token(&closed.token, supplied, operation)
        .map_err(|error| error.with_resource_id(closed.resource_id.clone()))?;
    Ok(())
}

fn missing_session_error(
    state: &ManagerState,
    session_id: &SessionId,
    supplied: &LeaseToken,
    operation: &'static str,
) -> seeed_hal_core::HalError {
    let Some(closed) = state.closed.get(session_id) else {
        return session_not_found(operation);
    };
    validate_closed_token(&closed.token, supplied, operation)
        .err()
        .map(|error| error.with_resource_id(closed.resource_id.clone()))
        .unwrap_or_else(|| {
            runtime_error(
                "runtime.session.closed",
                ErrorCategory::Conflict,
                operation,
                false,
                "the CAN session is closed",
            )
            .with_resource_id(closed.resource_id.clone())
        })
}

fn validate_closed_token(
    closed: &LeaseToken,
    supplied: &LeaseToken,
    operation: &'static str,
) -> HalResult<()> {
    if supplied.generation() < closed.generation() {
        return Err(runtime_error(
            "runtime.lease.stale_generation",
            ErrorCategory::Conflict,
            operation,
            false,
            "the supplied lease generation predates the closed CAN session",
        ));
    }
    if supplied != closed {
        return Err(runtime_error(
            "runtime.lease.invalid_token",
            ErrorCategory::Conflict,
            operation,
            false,
            "the supplied lease token does not match the closed CAN session",
        ));
    }
    Ok(())
}

async fn wait_cancelled(mut cancellation: watch::Receiver<bool>) {
    loop {
        if *cancellation.borrow() || cancellation.changed().await.is_err() {
            return;
        }
    }
}

async fn wait_pending_done(mut done: watch::Receiver<bool>, timeout: Duration) -> HalResult<()> {
    let wait = async {
        loop {
            if *done.borrow() || done.changed().await.is_err() {
                return;
            }
        }
    };
    tokio::time::timeout(timeout, wait).await.map_err(|_| {
        runtime_error(
            "runtime.session.close_timeout",
            ErrorCategory::Unavailable,
            "runtime.owner.revoke",
            false,
            "a provisional CAN open did not finish cleanup before the deadline",
        )
    })
}

async fn reliable_actor_remove(
    actor: CanActorHandle,
    session_id: SessionId,
    done: watch::Sender<bool>,
    timeout: Duration,
) -> HalResult<()> {
    let deadline = Instant::now()
        .checked_add(timeout)
        .unwrap_or_else(Instant::now);
    loop {
        if actor.is_finished() {
            done.send_replace(true);
            return Ok(());
        }
        let (reply, _) = oneshot::channel();
        match actor.try_cleanup(CanCommand::RemoveSession {
            session_id: session_id.clone(),
            cleanup_done: Some(done.clone()),
            reply,
        }) {
            Ok(()) => break,
            Err(error) if error.name().as_str() == "runtime.queue.full" => {
                if Instant::now() >= deadline {
                    return Err(runtime_error(
                        "runtime.session.close_timeout",
                        ErrorCategory::Unavailable,
                        "can.cleanup",
                        false,
                        "CAN cleanup admission exceeded its finite deadline",
                    ));
                }
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
            Err(_) => {
                done.send_replace(true);
                return Ok(());
            }
        }
    }
    let remaining = deadline.saturating_duration_since(Instant::now());
    tokio::select! {
        biased;
        _ = actor.wait_finished() => {
            done.send_replace(true);
            Ok(())
        },
        result = wait_pending_done(done.subscribe(), remaining) => result,
    }
}

fn lock_state(state: &Mutex<ManagerState>) -> std::sync::MutexGuard<'_, ManagerState> {
    state
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn actor_unavailable(
    operation: &'static str,
    resource_id: &ResourceId,
) -> seeed_hal_core::HalError {
    runtime_error(
        "runtime.actor.unavailable",
        ErrorCategory::Internal,
        operation,
        false,
        "the CAN actor dropped an operation reply",
    )
    .with_resource_id(resource_id.clone())
}

fn session_closed(operation: &'static str, resource_id: &ResourceId) -> seeed_hal_core::HalError {
    runtime_error(
        "runtime.session.closed",
        ErrorCategory::Conflict,
        operation,
        false,
        "the CAN session is closed",
    )
    .with_resource_id(resource_id.clone())
}

fn session_not_found(operation: &'static str) -> seeed_hal_core::HalError {
    runtime_error(
        "runtime.session.not_found",
        ErrorCategory::NotFound,
        operation,
        false,
        "the CAN session ID is unknown",
    )
}

fn operation_timeout(
    operation: &'static str,
    resource_id: &ResourceId,
) -> seeed_hal_core::HalError {
    runtime_error(
        "runtime.transport.timeout",
        ErrorCategory::Unavailable,
        operation,
        true,
        "the CAN actor operation exceeded its finite deadline",
    )
    .with_resource_id(resource_id.clone())
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::time::Duration;

    use seeed_hal_can::{CanFilterSet, CanLinkExpectation, CanOpenConfig};
    use seeed_hal_core::{ErrorCategory, HalError, LeaseMode, OwnerId};
    use seeed_hal_testkit::VirtualCanAdapter;

    use super::{
        CanManager, ManagerStateSignal, ManagerTestGate, ManagerThreadGate, reliable_actor_remove,
    };
    use crate::can_actor::CanActorHandle;
    use crate::events::{EventPublisher, RuntimeEventKind};

    fn owner(value: &str) -> OwnerId {
        OwnerId::parse(value).unwrap()
    }

    fn attach() -> CanOpenConfig {
        CanOpenConfig::Attach(CanLinkExpectation::new(None, None, None, None, None).unwrap())
    }

    fn filters() -> CanFilterSet {
        CanFilterSet::new(Vec::new()).unwrap()
    }

    fn manager(adapter: VirtualCanAdapter) -> (CanManager, EventPublisher) {
        let events = EventPublisher::new();
        let manager = CanManager::new(
            vec![Arc::new(adapter)],
            events.clone(),
            16,
            16,
            Duration::from_millis(100),
        );
        (manager, events)
    }

    #[tokio::test]
    async fn non_first_provisional_cleanup_finishes_when_actor_terminates_after_admission() {
        let (actor, cleanup_commands, completion) = CanActorHandle::test_handle();
        let session_id = seeed_hal_core::SessionId::parse("cleanup-termination").unwrap();
        let (done, _) = tokio::sync::watch::channel(false);
        let cleanup = tokio::spawn(reliable_actor_remove(
            actor,
            session_id,
            done,
            Duration::from_secs(1),
        ));
        let queued = loop {
            match cleanup_commands.try_recv() {
                Ok(command) => break command,
                Err(std::sync::mpsc::TryRecvError::Empty) => tokio::task::yield_now().await,
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    panic!("cleanup queue disconnected before admission")
                }
            }
        };
        completion.send_replace(Some(Err(HalError::new(
            "test.actor.terminated",
            ErrorCategory::Internal,
            "can.actor",
            false,
            "injected actor termination",
        )
        .unwrap())));
        tokio::time::timeout(Duration::from_millis(100), cleanup)
            .await
            .expect("confirmed actor termination must finish provisional cleanup")
            .unwrap()
            .unwrap();
        drop(queued);
    }

    #[tokio::test]
    async fn revoke_at_commit_boundary_cannot_publish_an_inverted_open_event() {
        let adapter = VirtualCanAdapter::loopback("can:runtime:commit-revoke");
        let selector = adapter.descriptor().selector();
        let (mut manager, events) = manager(adapter);
        let gate = Arc::new(ManagerTestGate::default());
        manager.open_commit_gate = Some(Arc::clone(&gate));
        let manager = Arc::new(manager);
        let mut subscription = events.subscribe();
        let owner_id = owner("commit-revoke");
        let opening = {
            let manager = Arc::clone(&manager);
            let owner_id = owner_id.clone();
            tokio::spawn(async move {
                manager
                    .open(owner_id, selector, LeaseMode::Observe, attach(), filters())
                    .await
            })
        };
        gate.wait_reached().await;
        let (revoke, opening) = tokio::join!(
            biased;
            manager.revoke_owner(&owner_id),
            async {
                gate.release();
                opening.await
            },
        );
        let error = opening.unwrap().unwrap_err();
        assert_eq!(error.name().as_str(), "runtime.session.closed");
        revoke.unwrap();
        assert!(
            tokio::time::timeout(Duration::from_millis(10), subscription.recv())
                .await
                .is_err(),
            "a revoked pending session must not publish SessionOpened"
        );
    }

    #[tokio::test]
    async fn normal_actor_close_reconciliation_emits_no_unknown_health() {
        let adapter = VirtualCanAdapter::loopback("can:runtime:normal-close-events");
        let selector = adapter.descriptor().selector();
        let (mut manager, events) = manager(adapter);
        let gate = Arc::new(ManagerTestGate::default());
        manager.close_finalize_gate = Some(Arc::clone(&gate));
        let manager = Arc::new(manager);
        let mut subscription = events.subscribe();
        let (session, token) = manager
            .open(
                owner("normal-close"),
                selector.clone(),
                LeaseMode::Observe,
                attach(),
                filters(),
            )
            .await
            .unwrap();
        let closing = {
            let manager = Arc::clone(&manager);
            let token = token.clone();
            tokio::spawn(async move { manager.close(session, &token).await })
        };
        gate.wait_reached().await;
        manager
            .open(
                owner("normal-reuse"),
                selector,
                LeaseMode::Observe,
                attach(),
                filters(),
            )
            .await
            .unwrap();
        gate.release();
        closing.await.unwrap().unwrap();
        assert_eq!(
            subscription.recv().await.unwrap().kind(),
            RuntimeEventKind::SessionOpened
        );
        assert_eq!(
            subscription.recv().await.unwrap().kind(),
            RuntimeEventKind::SessionClosed
        );
        assert_eq!(
            subscription.recv().await.unwrap().kind(),
            RuntimeEventKind::SessionOpened
        );
        assert!(
            tokio::time::timeout(Duration::from_millis(10), subscription.recv())
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn failed_actor_close_reconciliation_emits_one_unknown_before_close() {
        let adapter = VirtualCanAdapter::loopback("can:runtime:failed-close-events");
        let selector = adapter.descriptor().selector();
        adapter.fail_next_close(
            HalError::new(
                "test.can.close",
                ErrorCategory::Unavailable,
                "can.close",
                false,
                "injected close failure",
            )
            .unwrap(),
        );
        let (mut manager, events) = manager(adapter);
        let gate = Arc::new(ManagerTestGate::default());
        manager.close_finalize_gate = Some(Arc::clone(&gate));
        let manager = Arc::new(manager);
        let mut subscription = events.subscribe();
        let (session, token) = manager
            .open(
                owner("failed-close"),
                selector.clone(),
                LeaseMode::Observe,
                attach(),
                filters(),
            )
            .await
            .unwrap();
        let closing = {
            let manager = Arc::clone(&manager);
            let token = token.clone();
            tokio::spawn(async move { manager.close(session, &token).await })
        };
        gate.wait_reached().await;
        manager
            .open(
                owner("failed-reuse"),
                selector,
                LeaseMode::Observe,
                attach(),
                filters(),
            )
            .await
            .unwrap();
        gate.release();
        assert_eq!(
            closing.await.unwrap().unwrap_err().name().as_str(),
            "test.can.close"
        );
        assert_eq!(
            subscription.recv().await.unwrap().kind(),
            RuntimeEventKind::SessionOpened
        );
        assert_eq!(
            subscription.recv().await.unwrap().kind(),
            RuntimeEventKind::CanBusUnknown
        );
        assert_eq!(
            subscription.recv().await.unwrap().kind(),
            RuntimeEventKind::SessionClosed
        );
        assert_eq!(
            subscription.recv().await.unwrap().kind(),
            RuntimeEventKind::SessionOpened
        );
        assert!(
            tokio::time::timeout(Duration::from_millis(10), subscription.recv())
                .await
                .is_err()
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn finish_first_failed_close_publishes_terminal_events_before_reopen() {
        let adapter = VirtualCanAdapter::loopback("can:runtime:finish-first-events");
        let selector = adapter.descriptor().selector();
        adapter.fail_next_close(
            HalError::new(
                "test.can.close",
                ErrorCategory::Unavailable,
                "can.close",
                false,
                "injected close failure",
            )
            .unwrap(),
        );
        let (mut manager, events) = manager(adapter);
        let terminal_gate = Arc::new(ManagerThreadGate::default());
        let reserve_signal = Arc::new(ManagerStateSignal::default());
        manager.close_event_gate = Some(Arc::clone(&terminal_gate));
        manager.open_reserve_signal = Some(Arc::clone(&reserve_signal));
        let manager = Arc::new(manager);
        let mut subscription = events.subscribe();
        let (session, token) = manager
            .open(
                owner("finish-first-close"),
                selector.clone(),
                LeaseMode::Observe,
                attach(),
                filters(),
            )
            .await
            .unwrap();
        reserve_signal.arm();
        let closing = {
            let manager = Arc::clone(&manager);
            let token = token.clone();
            tokio::spawn(async move { manager.close(session, &token).await })
        };
        terminal_gate.wait_reached().await;
        let reopening = {
            let manager = Arc::clone(&manager);
            tokio::spawn(async move {
                manager
                    .open(
                        owner("finish-first-reuse"),
                        selector,
                        LeaseMode::Observe,
                        attach(),
                        filters(),
                    )
                    .await
            })
        };
        reserve_signal.wait_reached().await;
        assert!(!reopening.is_finished());
        terminal_gate.release();
        assert_eq!(
            closing.await.unwrap().unwrap_err().name().as_str(),
            "test.can.close"
        );
        reopening.await.unwrap().unwrap();
        assert_eq!(
            subscription.recv().await.unwrap().kind(),
            RuntimeEventKind::SessionOpened
        );
        assert_eq!(
            subscription.recv().await.unwrap().kind(),
            RuntimeEventKind::CanBusUnknown
        );
        assert_eq!(
            subscription.recv().await.unwrap().kind(),
            RuntimeEventKind::SessionClosed
        );
        assert_eq!(
            subscription.recv().await.unwrap().kind(),
            RuntimeEventKind::SessionOpened
        );
        assert!(
            tokio::time::timeout(Duration::from_millis(10), subscription.recv())
                .await
                .is_err()
        );
    }
}
