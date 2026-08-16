use std::collections::{HashMap, VecDeque};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::time::{Duration, Instant};

use seeed_hal_can::{
    CanActiveConfig, CanAdapter, CanBatchSendError, CanBusState, CanBusStatus, CanChannel,
    CanFilterSet, CanFrame, CanLinkExpectation, CanOpenConfig, ReceivedCanFrame,
};
use seeed_hal_core::{
    ErrorCategory, ErrorContext, HalError, HalResult, LeaseToken, OwnerId, ResourceId,
    ResourceSelector, SessionId,
};
use tokio::sync::{oneshot, watch};

use crate::events::{EventPublisher, RuntimeEventKind};
use crate::runtime_error;

const MANAGEMENT_QUEUE_CAPACITY: usize = 64;
const CLEANUP_QUEUE_CAPACITY: usize = 64;
const MANAGEMENT_COMMAND_BUDGET: usize = 16;
const CLEANUP_COMMAND_BUDGET: usize = 16;
const RECEIVE_POLL_SLICE: Duration = Duration::from_millis(2);
const MAX_PENDING_RECEIVES_PER_SESSION: usize = 1;

#[derive(Clone)]
pub(crate) struct ActorSessionSpec {
    pub(crate) session_id: SessionId,
    pub(crate) owner_id: OwnerId,
    pub(crate) config: CanOpenConfig,
    pub(crate) filters: CanFilterSet,
    pub(crate) activation: Arc<Mutex<Option<LeaseToken>>>,
    pub(crate) cancelled: Arc<AtomicBool>,
    pub(crate) cleanup_done: watch::Sender<bool>,
    pub(crate) termination_expected: Arc<AtomicBool>,
    pub(crate) termination_failed: Arc<AtomicBool>,
}

struct ActorSession {
    owner_id: OwnerId,
    activation: Arc<Mutex<Option<LeaseToken>>>,
    cancelled: Arc<AtomicBool>,
    cleanup_done: watch::Sender<bool>,
    termination_expected: Arc<AtomicBool>,
    termination_failed: Arc<AtomicBool>,
    filters: CanFilterSet,
    received: VecDeque<ReceivedCanFrame>,
    dropped: u64,
    receive_error: Option<HalError>,
    pending_receive: Option<PendingReceive>,
}

struct PendingReceive {
    max_frames: usize,
    deadline: Instant,
    reply: oneshot::Sender<HalResult<Vec<ReceivedCanFrame>>>,
}

pub(crate) enum CanCommand {
    AddSession {
        session: ActorSessionSpec,
        reply: oneshot::Sender<HalResult<()>>,
    },
    SendBatch {
        session_id: SessionId,
        frames: Vec<CanFrame>,
        reply: oneshot::Sender<Result<(), CanBatchSendError>>,
    },
    Receive {
        session_id: SessionId,
        max_frames: usize,
        deadline: Instant,
        reply: oneshot::Sender<HalResult<Vec<ReceivedCanFrame>>>,
    },
    ReplaceFilters {
        session_id: SessionId,
        filters: CanFilterSet,
        reply: oneshot::Sender<HalResult<()>>,
    },
    BusStatus {
        session_id: SessionId,
        reply: oneshot::Sender<HalResult<CanBusStatus>>,
    },
    RemoveSession {
        session_id: SessionId,
        cleanup_done: Option<watch::Sender<bool>>,
        reply: oneshot::Sender<RemoveOutcome>,
    },
}

pub(crate) struct RemoveOutcome {
    pub(crate) last_session: bool,
    pub(crate) result: HalResult<()>,
}

impl CanCommand {
    fn reject_unavailable(self) {
        match self {
            Self::AddSession { session, reply } => {
                session.cleanup_done.send_replace(true);
                let _ = reply.send(Err(actor_unavailable("can.session")));
            }
            Self::ReplaceFilters { reply, .. } => {
                let _ = reply.send(Err(actor_unavailable("can.session")));
            }
            Self::RemoveSession {
                cleanup_done,
                reply,
                ..
            } => {
                if let Some(done) = cleanup_done {
                    done.send_replace(true);
                }
                let _ = reply.send(RemoveOutcome {
                    last_session: false,
                    result: Err(actor_unavailable("can.close")),
                });
            }
            Self::SendBatch { reply, .. } => {
                let _ = reply.send(Err(CanBatchSendError::new(actor_unavailable(
                    "can.send_batch",
                ))));
            }
            Self::Receive { reply, .. } => {
                let _ = reply.send(Err(actor_unavailable("can.receive")));
            }
            Self::BusStatus { reply, .. } => {
                let _ = reply.send(Err(actor_unavailable("can.status")));
            }
        }
    }
}

#[derive(Clone)]
pub(crate) struct CanActorHandle {
    commands: mpsc::SyncSender<CanCommand>,
    cleanup: mpsc::SyncSender<CanCommand>,
    tx_reserved: Arc<AtomicUsize>,
    tx_capacity: usize,
    completion: watch::Receiver<Option<HalResult<()>>>,
}

impl CanActorHandle {
    pub(crate) fn try_command(
        &self,
        command: CanCommand,
        operation: &'static str,
    ) -> HalResult<()> {
        self.commands.try_send(command).map_err(|error| match error {
            mpsc::TrySendError::Full(_) => queue_full(operation, MANAGEMENT_QUEUE_CAPACITY),
            mpsc::TrySendError::Disconnected(_) => actor_unavailable(operation),
        })
    }

    pub(crate) fn try_send_batch(&self, command: CanCommand, frames: usize) -> HalResult<()> {
        let mut current = self.tx_reserved.load(Ordering::Acquire);
        loop {
            let Some(next) = current.checked_add(frames) else {
                return Err(queue_full("can.send_batch", self.tx_capacity));
            };
            if next > self.tx_capacity {
                return Err(queue_full("can.send_batch", self.tx_capacity));
            }
            match self.tx_reserved.compare_exchange_weak(
                current,
                next,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => break,
                Err(actual) => current = actual,
            }
        }
        if let Err(error) = self.commands.try_send(command) {
            self.tx_reserved.fetch_sub(frames, Ordering::AcqRel);
            return Err(match error {
                mpsc::TrySendError::Full(_) => {
                    queue_full("can.send_batch", MANAGEMENT_QUEUE_CAPACITY)
                }
                mpsc::TrySendError::Disconnected(_) => actor_unavailable("can.send_batch"),
            });
        }
        Ok(())
    }

    pub(crate) fn try_cleanup(&self, command: CanCommand) -> HalResult<()> {
        self.cleanup.try_send(command).map_err(|error| match error {
            mpsc::TrySendError::Full(_) => queue_full("can.cleanup", CLEANUP_QUEUE_CAPACITY),
            mpsc::TrySendError::Disconnected(_) => actor_unavailable("can.cleanup"),
        })
    }

    pub(crate) fn is_finished(&self) -> bool {
        self.completion.borrow().is_some()
    }

    pub(crate) async fn wait_finished(&self) -> HalResult<()> {
        let mut completion = self.completion.clone();
        loop {
            if let Some(result) = completion.borrow().clone() {
                return result;
            }
            if completion.changed().await.is_err() {
                return Err(actor_unavailable("can.close"));
            }
        }
    }

    #[cfg(test)]
    pub(crate) fn test_handle() -> (
        Self,
        mpsc::Receiver<CanCommand>,
        watch::Sender<Option<HalResult<()>>>,
    ) {
        let (commands, _command_rx) = mpsc::sync_channel(1);
        let (cleanup, cleanup_rx) = mpsc::sync_channel(1);
        let (completion_tx, completion) = watch::channel(None);
        (
            Self {
                commands,
                cleanup,
                tx_reserved: Arc::new(AtomicUsize::new(0)),
                tx_capacity: 1,
                completion,
            },
            cleanup_rx,
            completion_tx,
        )
    }
}

pub(crate) fn spawn_can_actor(
    adapter: Arc<dyn CanAdapter>,
    selector: ResourceSelector,
    first_session: ActorSessionSpec,
    rx_capacity: usize,
    tx_capacity: usize,
    close_timeout: Duration,
    events: EventPublisher,
) -> HalResult<(CanActorHandle, oneshot::Receiver<HalResult<()>>)> {
    let (command_tx, command_rx) = mpsc::sync_channel(MANAGEMENT_QUEUE_CAPACITY);
    let (cleanup_tx, cleanup_rx) = mpsc::sync_channel(CLEANUP_QUEUE_CAPACITY);
    let tx_reserved = Arc::new(AtomicUsize::new(0));
    let (completion_tx, completion_rx) = watch::channel(None);
    let (open_tx, open_rx) = oneshot::channel();
    let runtime = tokio::runtime::Handle::try_current().map_err(|_| {
        runtime_error(
            "runtime.executor.unavailable",
            ErrorCategory::Unavailable,
            "can.open",
            true,
            "opening a CAN actor requires an active Tokio runtime handle",
        )
    })?;
    let thread_name = format!("seeed-hal-can-{}", selector.id().as_str());
    let actor_tx_reserved = Arc::clone(&tx_reserved);
    let first_cleanup_done = first_session.cleanup_done.clone();
    std::thread::Builder::new()
        .name(thread_name)
        .spawn(move || {
            let outcome = catch_unwind(AssertUnwindSafe(|| {
                let open_config = first_session.config.clone();
                let channel = runtime.block_on(async {
                    tokio::time::timeout(
                        close_timeout,
                        adapter.open(&selector, &open_config),
                    )
                    .await
                });
                match channel {
                    Ok(Ok(channel)) => {
                        let _ = open_tx.send(Ok(()));
                        run_actor(
                            channel,
                            first_session,
                            command_rx,
                            cleanup_rx,
                            actor_tx_reserved,
                            rx_capacity,
                            close_timeout,
                            events,
                        )
                    }
                    Ok(Err(error)) => {
                        let _ = open_tx.send(Err(error.clone()));
                        Err(error)
                    }
                    Err(_) => {
                        let error = runtime_error(
                            "runtime.transport.timeout",
                            ErrorCategory::Unavailable,
                            "can.open",
                            true,
                            format!("CAN adapter open exceeded its {close_timeout:?} deadline"),
                        )
                        .with_resource_id(selector.id().clone());
                        let _ = open_tx.send(Err(error.clone()));
                        Err(error)
                    }
                }
            }));
            let result = match outcome {
                Ok(result) => result,
                Err(_) => Err(actor_unavailable("can.actor")),
            };
            if result.is_err() {
                first_cleanup_done.send_replace(true);
            }
            let _ = completion_tx.send(Some(result));
        })
        .map_err(|error| {
            runtime_error(
                "runtime.actor.spawn_failed",
                ErrorCategory::Unavailable,
                "can.open",
                true,
                format!("failed to spawn CAN actor worker: {error}"),
            )
        })?;

    Ok((
        CanActorHandle {
            commands: command_tx,
            cleanup: cleanup_tx,
            tx_reserved,
            tx_capacity,
            completion: completion_rx,
        },
        open_rx,
    ))
}

fn run_actor(
    mut channel: Box<dyn CanChannel>,
    first_session: ActorSessionSpec,
    commands: mpsc::Receiver<CanCommand>,
    cleanup: mpsc::Receiver<CanCommand>,
    tx_reserved: Arc<AtomicUsize>,
    rx_capacity: usize,
    close_timeout: Duration,
    events: EventPublisher,
) -> HalResult<()> {
    let resource_id = channel.descriptor().id().clone();
    let active_config = channel.active_config().clone();
    let mut sessions = HashMap::new();
    sessions.insert(
        first_session.session_id,
        new_session(
            first_session.owner_id,
            first_session.filters,
            first_session.activation,
            first_session.cancelled,
            first_session.cleanup_done,
            first_session.termination_expected,
            first_session.termination_failed,
            rx_capacity,
        ),
    );
    let mut last_bus_state = None;

    loop {
        if let Some(result) = prune_cancelled_sessions(
            channel.as_mut(),
            &mut sessions,
            &resource_id,
            close_timeout,
        ) {
            reject_remaining(&cleanup, &tx_reserved);
            reject_remaining(&commands, &tx_reserved);
            return result;
        }
        let mut disconnected = false;
        for _ in 0..CLEANUP_COMMAND_BUDGET {
            let command = match cleanup.try_recv() {
                Ok(command) => command,
                Err(mpsc::TryRecvError::Empty) | Err(mpsc::TryRecvError::Disconnected) => break,
            };
            let should_close = execute_command(
                channel.as_mut(),
                command,
                &mut sessions,
                &active_config,
                &resource_id,
                &tx_reserved,
                rx_capacity,
                close_timeout,
                &events,
                &mut last_bus_state,
            );
            if should_close {
                reject_remaining(&cleanup, &tx_reserved);
                reject_remaining(&commands, &tx_reserved);
                return Ok(());
            }
        }
        for _ in 0..MANAGEMENT_COMMAND_BUDGET {
            let command = match commands.try_recv() {
                Ok(command) => command,
                Err(mpsc::TryRecvError::Empty) => break,
                Err(mpsc::TryRecvError::Disconnected) => {
                    disconnected = true;
                    break;
                }
            };
            let should_close = execute_command(
                channel.as_mut(),
                command,
                &mut sessions,
                &active_config,
                &resource_id,
                &tx_reserved,
                rx_capacity,
                close_timeout,
                &events,
                &mut last_bus_state,
            );
            if should_close {
                reject_remaining(&cleanup, &tx_reserved);
                reject_remaining(&commands, &tx_reserved);
                return Ok(());
            }
        }
        if sessions.is_empty() {
            return Ok(());
        }

        match channel.receive(RECEIVE_POLL_SLICE) {
            Ok(Some(frame)) => fan_out(frame, &mut sessions, rx_capacity, &resource_id),
            Ok(None) => {}
            Err(error) if error.name().as_str() == "can.receive.lagged" => {
                let dropped = error
                    .context()
                    .iter()
                    .find_map(|(key, value)| (key == "dropped_count").then_some(value))
                    .and_then(|value| value.parse::<u64>().ok())
                    .unwrap_or(1);
                for session in sessions.values_mut() {
                    session.dropped = session.dropped.saturating_add(dropped);
                }
            }
            Err(error) => {
                for session in sessions.values_mut() {
                    session.receive_error = Some(error.clone());
                }
            }
        }
        service_pending_receives(&mut sessions, &resource_id);
        if disconnected {
            let close_result = channel.close();
            reject_all_sessions(&mut sessions, &resource_id);
            reject_remaining(&cleanup, &tx_reserved);
            return close_result;
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn execute_command(
    channel: &mut dyn CanChannel,
    command: CanCommand,
    sessions: &mut HashMap<SessionId, ActorSession>,
    active_config: &CanActiveConfig,
    resource_id: &ResourceId,
    tx_reserved: &AtomicUsize,
    rx_capacity: usize,
    close_timeout: Duration,
    events: &EventPublisher,
    last_bus_state: &mut Option<CanBusState>,
) -> bool {
    match command {
        CanCommand::AddSession { session, reply } => {
            if session.cancelled.load(Ordering::Acquire) {
                session.cleanup_done.send_replace(true);
                let _ = reply.send(Err(session_closed("can.open", resource_id)));
                return false;
            }
            let cleanup_done = session.cleanup_done.clone();
            let result = compatible_configuration(active_config, &session.config, resource_id)
                .and_then(|()| {
                    if sessions.contains_key(&session.session_id) {
                        return Err(session_error(
                            "runtime.session.conflict",
                            "can.open",
                            resource_id,
                            "the CAN actor already contains this session",
                        ));
                    }
                    sessions.insert(
                        session.session_id,
                        new_session(
                            session.owner_id,
                            session.filters,
                            session.activation,
                            session.cancelled,
                            session.cleanup_done,
                            session.termination_expected,
                            session.termination_failed,
                            rx_capacity,
                        ),
                    );
                    Ok(())
                });
            if result.is_err() {
                cleanup_done.send_replace(true);
            }
            let _ = reply.send(result);
        }
        CanCommand::SendBatch {
            session_id,
            frames,
            reply,
        } => {
            let frame_count = frames.len();
            let result = if sessions.contains_key(&session_id) {
                let mut committed = 0;
                let mut result = Ok(());
                for frame in &frames {
                    if let Err(error) = channel.send(frame) {
                        result = Err(CanBatchSendError::backend_prefix(error, committed));
                        break;
                    }
                    committed += 1;
                }
                result
            } else {
                Err(CanBatchSendError::new(session_closed(
                    "can.send_batch",
                    resource_id,
                )))
            };
            tx_reserved.fetch_sub(frame_count, Ordering::AcqRel);
            let _ = reply.send(result);
        }
        CanCommand::Receive {
            session_id,
            max_frames,
            deadline,
            reply,
        } => match session_mut(sessions, &session_id, "can.receive", resource_id) {
            Ok(session) => {
                if session.pending_receive.is_some() {
                    let _ = reply.send(Err(queue_full(
                        "can.receive",
                        MAX_PENDING_RECEIVES_PER_SESSION,
                    )
                    .with_resource_id(resource_id.clone())));
                } else if let Some(result) = take_receive_result(session, max_frames, resource_id) {
                    let _ = reply.send(result);
                } else if Instant::now() >= deadline {
                    let _ = reply.send(Ok(Vec::new()));
                } else {
                    session.pending_receive = Some(PendingReceive {
                        max_frames,
                        deadline,
                        reply,
                    });
                }
            }
            Err(error) => {
                let _ = reply.send(Err(error));
            }
        },
        CanCommand::ReplaceFilters {
            session_id,
            filters,
            reply,
        } => {
            let result = session_mut(sessions, &session_id, "can.replace_filters", resource_id)
                .map(|session| session.filters = filters);
            let _ = reply.send(result);
        }
        CanCommand::BusStatus { session_id, reply } => {
            let result = if sessions.contains_key(&session_id) {
                channel.bus_status().map(|status| {
                    publish_bus_transition(
                        status.state(),
                        last_bus_state,
                        sessions,
                        resource_id,
                        events,
                    );
                    status
                })
            } else {
                Err(session_closed("can.status", resource_id))
            };
            let _ = reply.send(result);
        }
        CanCommand::RemoveSession {
            session_id,
            cleanup_done,
            reply,
        } => {
            let Some(mut removed) = sessions.remove(&session_id) else {
                if let Some(done) = cleanup_done {
                    done.send_replace(true);
                }
                let _ = reply.send(RemoveOutcome {
                    last_session: false,
                    result: Err(session_closed("can.close", resource_id)),
                });
                return false;
            };
            if let Some(pending) = removed.pending_receive.take() {
                let _ = pending
                    .reply
                    .send(Err(session_closed("can.receive", resource_id)));
            }
            if sessions.is_empty() {
                let started = Instant::now();
                let backend_result = channel.close();
                let result = if started.elapsed() > close_timeout {
                    Err(runtime_error(
                        "runtime.session.close_timeout",
                        ErrorCategory::Unavailable,
                        "can.close",
                        false,
                        format!("CAN channel close exceeded its {close_timeout:?} deadline"),
                    )
                    .with_resource_id(resource_id.clone()))
                } else {
                    backend_result
                };
                removed
                    .termination_failed
                    .store(result.is_err(), Ordering::Release);
                removed
                    .termination_expected
                    .store(true, Ordering::Release);
                let _ = reply.send(RemoveOutcome {
                    last_session: true,
                    result,
                });
                removed.cleanup_done.send_replace(true);
                if let Some(done) = cleanup_done {
                    done.send_replace(true);
                }
                return true;
            }
            removed.cleanup_done.send_replace(true);
            if let Some(done) = cleanup_done {
                done.send_replace(true);
            }
            let _ = reply.send(RemoveOutcome {
                last_session: false,
                result: Ok(()),
            });
        }
    }
    false
}

fn new_session(
    owner_id: OwnerId,
    filters: CanFilterSet,
    activation: Arc<Mutex<Option<LeaseToken>>>,
    cancelled: Arc<AtomicBool>,
    cleanup_done: watch::Sender<bool>,
    termination_expected: Arc<AtomicBool>,
    termination_failed: Arc<AtomicBool>,
    rx_capacity: usize,
) -> ActorSession {
    ActorSession {
        owner_id,
        activation,
        cancelled,
        cleanup_done,
        termination_expected,
        termination_failed,
        filters,
        received: VecDeque::with_capacity(rx_capacity),
        dropped: 0,
        receive_error: None,
        pending_receive: None,
    }
}

fn prune_cancelled_sessions(
    channel: &mut dyn CanChannel,
    sessions: &mut HashMap<SessionId, ActorSession>,
    resource_id: &ResourceId,
    close_timeout: Duration,
) -> Option<HalResult<()>> {
    let cancelled: Vec<_> = sessions
        .iter()
        .filter(|(_, session)| {
            session.cancelled.load(Ordering::Acquire)
                && session
                    .activation
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .is_none()
        })
        .map(|(session_id, _)| session_id.clone())
        .collect();
    if cancelled.is_empty() {
        return None;
    }
    let mut removed = Vec::with_capacity(cancelled.len());
    for session_id in cancelled {
        if let Some(mut session) = sessions.remove(&session_id) {
            if let Some(pending) = session.pending_receive.take() {
                let _ = pending
                    .reply
                    .send(Err(session_closed("can.receive", resource_id)));
            }
            removed.push(session.cleanup_done);
        }
    }
    if sessions.is_empty() {
        let started = Instant::now();
        let backend_result = channel.close();
        let result = if started.elapsed() > close_timeout {
            Err(runtime_error(
                "runtime.session.close_timeout",
                ErrorCategory::Unavailable,
                "can.close",
                false,
                format!("CAN provisional cleanup exceeded its {close_timeout:?} deadline"),
            )
            .with_resource_id(resource_id.clone()))
        } else {
            backend_result
        };
        for done in removed {
            done.send_replace(true);
        }
        Some(result)
    } else {
        for done in removed {
            done.send_replace(true);
        }
        None
    }
}

fn fan_out(
    frame: ReceivedCanFrame,
    sessions: &mut HashMap<SessionId, ActorSession>,
    rx_capacity: usize,
    resource_id: &ResourceId,
) {
    for session in sessions.values_mut() {
        if !session.filters.matches(frame.frame()) {
            continue;
        }
        if session.received.len() == rx_capacity {
            session.received.pop_front();
            session.dropped = session.dropped.saturating_add(1);
        }
        session.received.push_back(frame.clone());
    }
    service_pending_receives(sessions, resource_id);
}

fn service_pending_receives(
    sessions: &mut HashMap<SessionId, ActorSession>,
    resource_id: &ResourceId,
) {
    let now = Instant::now();
    for session in sessions.values_mut() {
        let Some(pending) = session.pending_receive.take() else {
            continue;
        };
        if pending.reply.is_closed() {
            continue;
        }
        if let Some(result) = take_receive_result(session, pending.max_frames, resource_id) {
            let _ = pending.reply.send(result);
        } else if now >= pending.deadline {
            let _ = pending.reply.send(Ok(Vec::new()));
        } else {
            session.pending_receive = Some(pending);
        }
    }
}

fn take_receive_result(
    session: &mut ActorSession,
    max_frames: usize,
    resource_id: &ResourceId,
) -> Option<HalResult<Vec<ReceivedCanFrame>>> {
    if session.dropped > 0 {
        let dropped = std::mem::take(&mut session.dropped);
        let context = ErrorContext::new([("dropped_count", dropped.to_string())])
            .expect("static CAN lag context is valid");
        return Some(Err(runtime_error(
            "can.receive.lagged",
            ErrorCategory::Unavailable,
            "can.receive",
            true,
            "the bounded CAN receive ring dropped oldest frames",
        )
        .with_context(context)
        .with_resource_id(resource_id.clone())));
    }
    if let Some(error) = session.receive_error.take() {
        return Some(Err(error.with_resource_id(resource_id.clone())));
    }
    if session.received.is_empty() {
        return None;
    }
    let count = max_frames.min(session.received.len());
    Some(Ok(session.received.drain(..count).collect()))
}

fn compatible_configuration(
    active: &CanActiveConfig,
    requested: &CanOpenConfig,
    resource_id: &ResourceId,
) -> HalResult<()> {
    let mismatch = match requested {
        CanOpenConfig::Attach(expectation) => expectation_mismatch(expectation, active),
        CanOpenConfig::Configure(_) => true,
    };
    if mismatch {
        return Err(runtime_error(
            "can.configuration.mismatch",
            ErrorCategory::Conflict,
            "can.open",
            false,
            "the requested CAN configuration is incompatible with the live shared actor",
        )
        .with_resource_id(resource_id.clone()));
    }
    Ok(())
}

fn expectation_mismatch(expectation: &CanLinkExpectation, active: &CanActiveConfig) -> bool {
    expectation.mode().is_some_and(|value| value != active.mode())
        || expectation
            .nominal_bitrate()
            .is_some_and(|value| value != active.nominal().bitrate())
        || expectation.data_bitrate().is_some_and(|value| {
            active
                .data()
                .is_none_or(|timing| timing.bitrate() != value)
        })
        || expectation
            .listen_only()
            .is_some_and(|value| value != active.listen_only())
        || expectation
            .loopback()
            .is_some_and(|value| value != active.loopback())
}

fn publish_bus_transition(
    state: CanBusState,
    previous: &mut Option<CanBusState>,
    sessions: &HashMap<SessionId, ActorSession>,
    resource_id: &ResourceId,
    events: &EventPublisher,
) {
    let Some(old) = previous.replace(state) else {
        return;
    };
    if old == state {
        return;
    }
    let kind = match state {
        CanBusState::Active => RuntimeEventKind::CanBusActive,
        CanBusState::Warning => RuntimeEventKind::CanBusWarning,
        CanBusState::Passive => RuntimeEventKind::CanBusPassive,
        CanBusState::BusOff => RuntimeEventKind::CanBusOff,
        CanBusState::Stopped => RuntimeEventKind::CanBusStopped,
        CanBusState::Unknown => RuntimeEventKind::CanBusUnknown,
    };
    for (session_id, session) in sessions {
        publish_health_for_session(kind, resource_id, session_id, session, events);
    }
}

fn publish_health_for_session(
    kind: RuntimeEventKind,
    resource_id: &ResourceId,
    session_id: &SessionId,
    session: &ActorSession,
    events: &EventPublisher,
) {
    let token = session
        .activation
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .clone();
    if let Some(token) = token {
        events.publish(
            kind,
            resource_id.clone(),
            session_id.clone(),
            session.owner_id.clone(),
            token.generation(),
        );
    }
}

fn reject_all_sessions(
    sessions: &mut HashMap<SessionId, ActorSession>,
    resource_id: &ResourceId,
) {
    for session in sessions.values_mut() {
        if let Some(pending) = session.pending_receive.take() {
            let _ = pending
                .reply
                .send(Err(actor_unavailable("can.receive").with_resource_id(resource_id.clone())));
        }
        session.cleanup_done.send_replace(true);
    }
}

fn reject_remaining(commands: &mpsc::Receiver<CanCommand>, tx_reserved: &AtomicUsize) {
    while let Ok(command) = commands.try_recv() {
        match command {
            CanCommand::SendBatch { frames, reply, .. } => {
                tx_reserved.fetch_sub(frames.len(), Ordering::AcqRel);
                let _ = reply.send(Err(CanBatchSendError::new(actor_unavailable(
                    "can.send_batch",
                ))));
            }
            command => command.reject_unavailable(),
        }
    }
}

fn session_mut<'a>(
    sessions: &'a mut HashMap<SessionId, ActorSession>,
    session_id: &SessionId,
    operation: &'static str,
    resource_id: &ResourceId,
) -> HalResult<&'a mut ActorSession> {
    sessions
        .get_mut(session_id)
        .ok_or_else(|| session_closed(operation, resource_id))
}

fn queue_full(operation: &'static str, capacity: usize) -> HalError {
    runtime_error(
        "runtime.queue.full",
        ErrorCategory::Unavailable,
        operation,
        true,
        format!("the bounded CAN queue has reached its {capacity}-item capacity"),
    )
}

fn actor_unavailable(operation: &'static str) -> HalError {
    runtime_error(
        "runtime.actor.unavailable",
        ErrorCategory::Internal,
        operation,
        false,
        "the CAN actor worker is unavailable",
    )
}

fn session_closed(operation: &'static str, resource_id: &ResourceId) -> HalError {
    session_error(
        "runtime.session.closed",
        operation,
        resource_id,
        "the CAN session is closed",
    )
}

fn session_error(
    name: &'static str,
    operation: &'static str,
    resource_id: &ResourceId,
    message: &'static str,
) -> HalError {
    runtime_error(name, ErrorCategory::Conflict, operation, false, message)
        .with_resource_id(resource_id.clone())
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::AtomicBool;
    use std::sync::{Arc, Mutex};

    use seeed_hal_can::{CanFilterSet, CanLinkExpectation, CanOpenConfig};
    use seeed_hal_core::{OwnerId, SessionId};
    use tokio::sync::{oneshot, watch};

    use super::{ActorSessionSpec, CanCommand};

    fn session(done: watch::Sender<bool>) -> ActorSessionSpec {
        ActorSessionSpec {
            session_id: SessionId::parse("rejected-add").unwrap(),
            owner_id: OwnerId::parse("rejected-owner").unwrap(),
            config: CanOpenConfig::Attach(
                CanLinkExpectation::new(None, None, None, None, None).unwrap(),
            ),
            filters: CanFilterSet::new(Vec::new()).unwrap(),
            activation: Arc::new(Mutex::new(None)),
            cancelled: Arc::new(AtomicBool::new(false)),
            cleanup_done: done,
            termination_expected: Arc::new(AtomicBool::new(false)),
            termination_failed: Arc::new(AtomicBool::new(false)),
        }
    }

    #[test]
    fn rejecting_queued_add_session_signals_cleanup_completion() {
        let (done, done_rx) = watch::channel(false);
        let (reply, _) = oneshot::channel();
        CanCommand::AddSession {
            session: session(done),
            reply,
        }
        .reject_unavailable();
        assert!(*done_rx.borrow());
    }

    #[test]
    fn rejecting_queued_remove_signals_cleanup_completion() {
        let (done, done_rx) = watch::channel(false);
        let (reply, _) = oneshot::channel();
        CanCommand::RemoveSession {
            session_id: SessionId::parse("rejected-remove").unwrap(),
            cleanup_done: Some(done),
            reply,
        }
        .reject_unavailable();
        assert!(*done_rx.borrow());
    }
}
