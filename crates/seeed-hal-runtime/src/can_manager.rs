use std::collections::{HashMap, VecDeque};
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

use crate::can_actor::{
    ActorSessionSpec, CanActorHandle, CanCommand, spawn_can_actor,
};
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
}

struct ActiveSession {
    resource_id: ResourceId,
    owner_id: OwnerId,
    token: LeaseToken,
    actor: CanActorHandle,
    closing: bool,
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
    actors: HashMap<ResourceId, CanActorHandle>,
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
        let descriptors: Vec<_> = records
            .iter()
            .map(|record| record.descriptor.clone())
            .collect();
        let required_capability = required_capability(&config);
        let selected = resolve_resource(
            &descriptors,
            &selector,
            &required_capability,
            "can.open",
        )?;
        let selected_index = descriptors
            .iter()
            .position(|descriptor| std::ptr::eq(descriptor, selected))
            .expect("the canonical CAN resolver returned a member of its input");
        let record = &records[selected_index];
        let resource_id = record.descriptor.id().clone();
        let session_id = SessionId::parse(Uuid::new_v4().to_string())?;
        let (cancellation, done) = {
            let mut state = lock_state(&self.state);
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
                },
            );
            (cancel_rx, done_tx)
        };
        let activation = Arc::new(Mutex::new(None));
        let mut guard = PendingCanOpen::new(
            Arc::clone(&self.state),
            session_id.clone(),
            Arc::clone(&activation),
            done,
        );
        let session_spec = ActorSessionSpec {
            session_id: session_id.clone(),
            owner_id: owner_id.clone(),
            config: config.clone(),
            filters,
            activation,
        };

        let (actor, added) = {
            let mut state = lock_state(&self.state);
            if state
                .actors
                .get(&resource_id)
                .is_some_and(CanActorHandle::is_finished)
            {
                state.actors.remove(&resource_id);
            }
            if let Some(actor) = state.actors.get(&resource_id).cloned() {
                let (reply_tx, reply_rx) = oneshot::channel();
                actor.try_command(
                    CanCommand::AddSession {
                        session: session_spec,
                        reply: reply_tx,
                    },
                    "can.open",
                )?;
                (actor, reply_rx)
            } else {
                let adapter = Arc::clone(&self.adapters[record.adapter_index]);
                let (actor, reply_rx) = spawn_can_actor(
                    adapter,
                    selector,
                    session_spec,
                    self.rx_capacity,
                    self.tx_capacity,
                    self.close_timeout,
                    self.events.clone(),
                )?;
                state.actors.insert(resource_id.clone(), actor.clone());
                (actor, reply_rx)
            }
        };
        guard.set_actor(actor.clone());

        let add_result = tokio::select! {
            result = tokio::time::timeout(self.close_timeout, added) => match result {
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
            if let Some(cleanup) = guard.begin_actor_session_removal() {
                let _ = tokio::time::timeout(self.close_timeout, cleanup).await;
            }
            return Err(error.with_resource_id(resource_id));
        }

        let commit_result = {
            let mut state = lock_state(&self.state);
            match state.pending.remove(&session_id) {
                Some(pending) => state.leases.commit(pending.reservation).map(|token| {
                    pending.done.send_replace(true);
                    state.active.insert(
                        session_id.clone(),
                        ActiveSession {
                            resource_id: resource_id.clone(),
                            owner_id: owner_id.clone(),
                            token: token.clone(),
                            actor,
                            closing: false,
                        },
                    );
                    token
                }),
                None => Err(session_closed("can.open", &resource_id)),
            }
        };
        let token = match commit_result {
            Ok(token) => token,
            Err(error) => {
                if let Some(cleanup) = guard.begin_actor_session_removal() {
                    let _ = tokio::time::timeout(self.close_timeout, cleanup).await;
                }
                return Err(error);
            }
        };
        guard.activate(token.clone());
        guard.disarm();
        self.events.publish(
            RuntimeEventKind::SessionOpened,
            resource_id,
            session_id.clone(),
            owner_id,
            token.generation(),
        );
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
            return Err(CanBatchSendError::new(runtime_error(
                "can.frame.invalid",
                ErrorCategory::InvalidArgument,
                "can.send_batch",
                false,
                "a CAN send batch must contain 1..=64 frames",
            ).with_resource_id(resource_id)));
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
            ).with_resource_id(resource_id));
        }
        let (reply_tx, reply_rx) = oneshot::channel();
        actor
            .try_command(
                CanCommand::Receive {
                    session_id,
                    max_frames,
                    deadline: Instant::now().checked_add(timeout).unwrap_or_else(Instant::now),
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

    pub(crate) async fn close(
        &self,
        session_id: SessionId,
        token: &LeaseToken,
    ) -> HalResult<()> {
        let (actor, resource_id, owner_id, generation) = {
            let mut state = lock_state(&self.state);
            let Some(entry) = state.active.get(&session_id) else {
                return closed_result(&state, &session_id, token, "can.close");
            };
            let entry_resource = entry.resource_id.clone();
            let entry_owner = entry.owner_id.clone();
            let entry_actor = entry.actor.clone();
            let entry_generation = entry.token.generation();
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
                .try_command(
                    CanCommand::RemoveSession {
                        session_id: session_id.clone(),
                        reply: reply_tx,
                    },
                    "can.close",
                )
                .map_err(|error| error.with_resource_id(entry_resource.clone()))?;
            state
                .active
                .get_mut(&session_id)
                .expect("validated CAN session remains present while manager state is locked")
                .closing = true;
            (
                (entry_actor, reply_rx),
                entry_resource,
                entry_owner,
                entry_generation,
            )
        };
        let (actor_handle, reply_rx) = actor;
        let (result, last_session) = match tokio::time::timeout(self.close_timeout, reply_rx).await {
            Ok(Ok(outcome)) => (outcome.result, outcome.last_session),
            Ok(Err(_)) => (Err(actor_unavailable("can.close", &resource_id)), false),
            Err(_) => (Err(runtime_error(
                "runtime.session.close_timeout",
                ErrorCategory::Unavailable,
                "can.close",
                false,
                format!("CAN session close exceeded its {:?} deadline", self.close_timeout),
            )
            .with_resource_id(resource_id.clone())), false),
        };
        if last_session {
            let _ = tokio::time::timeout(self.close_timeout, actor_handle.wait_finished()).await;
        }
        self.finish_close(&session_id, token, &resource_id);
        if actor_handle.is_finished() {
            let mut state = lock_state(&self.state);
            if state
                .actors
                .get(&resource_id)
                .is_some_and(|current| current.is_finished())
            {
                state.actors.remove(&resource_id);
            }
        }
        self.events.publish(
            RuntimeEventKind::SessionClosed,
            resource_id,
            session_id,
            owner_id,
            generation,
        );
        result
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
                    entry.cancellation.send_replace(true);
                    pending_done.push(entry.done.subscribe());
                }
            }
            let active = state
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
            let result = self.close(session_id, &token).await;
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
            let (reply_tx, reply_rx) = oneshot::channel();
            let name = format!("seeed-hal-can-enumerate-{adapter_index}");
            std::thread::Builder::new()
                .name(name)
                .spawn(move || {
                    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        runtime.block_on(adapter.enumerate())
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
            let descriptors = tokio::time::timeout(self.close_timeout, receiver)
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
        let state = lock_state(&self.state);
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

    fn finish_close(
        &self,
        session_id: &SessionId,
        token: &LeaseToken,
        resource_id: &ResourceId,
    ) {
        let mut state = lock_state(&self.state);
        let Some(entry) = state.active.remove(session_id) else {
            return;
        };
        let _ = state.leases.release(resource_id, session_id, token);
        while state.closed_order.len() >= CLOSED_SESSION_CAPACITY {
            if let Some(expired) = state.closed_order.pop_front() {
                state.closed.remove(&expired);
            }
        }
        state.closed_order.push_back(session_id.clone());
        state.closed.insert(
            session_id.clone(),
            ClosedSession {
                token: entry.token,
                resource_id: entry.resource_id,
            },
        );
    }
}

struct PendingCanOpen {
    state: Arc<Mutex<ManagerState>>,
    session_id: SessionId,
    activation: Arc<Mutex<Option<LeaseToken>>>,
    done: watch::Sender<bool>,
    actor: Option<CanActorHandle>,
    armed: bool,
}

impl PendingCanOpen {
    fn new(
        state: Arc<Mutex<ManagerState>>,
        session_id: SessionId,
        activation: Arc<Mutex<Option<LeaseToken>>>,
        done: watch::Sender<bool>,
    ) -> Self {
        Self {
            state,
            session_id,
            activation,
            done,
            actor: None,
            armed: true,
        }
    }

    fn set_actor(&mut self, actor: CanActorHandle) {
        self.actor = Some(actor);
    }

    fn activate(&self, token: LeaseToken) {
        *self
            .activation
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(token);
    }

    fn remove_actor_session(&mut self) {
        let _ = self.begin_actor_session_removal();
    }

    fn begin_actor_session_removal(
        &mut self,
    ) -> Option<oneshot::Receiver<crate::can_actor::RemoveOutcome>> {
        let Some(actor) = &self.actor else {
            return None;
        };
        let (reply, receiver) = oneshot::channel();
        if actor.try_command(
            CanCommand::RemoveSession {
                session_id: self.session_id.clone(),
                reply,
            },
            "can.open.cleanup",
        ).is_err() {
            self.actor = None;
            return None;
        }
        self.actor = None;
        Some(receiver)
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
        let mut state = lock_state(&self.state);
        if let Some(pending) = state.pending.remove(&self.session_id) {
            let _ = state.leases.cancel(&pending.reservation);
            pending.done.send_replace(true);
        }
        drop(state);
        self.remove_actor_session();
        self.done.send_replace(true);
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
        .unwrap_or_else(|| runtime_error(
            "runtime.session.closed",
            ErrorCategory::Conflict,
            operation,
            false,
            "the CAN session is closed",
        ).with_resource_id(closed.resource_id.clone()))
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

fn lock_state(state: &Mutex<ManagerState>) -> std::sync::MutexGuard<'_, ManagerState> {
    state
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn actor_unavailable(operation: &'static str, resource_id: &ResourceId) -> seeed_hal_core::HalError {
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

fn operation_timeout(operation: &'static str, resource_id: &ResourceId) -> seeed_hal_core::HalError {
    runtime_error(
        "runtime.transport.timeout",
        ErrorCategory::Unavailable,
        operation,
        true,
        "the CAN actor operation exceeded its finite deadline",
    )
    .with_resource_id(resource_id.clone())
}
