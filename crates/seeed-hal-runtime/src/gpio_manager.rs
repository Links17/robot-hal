use crate::{lease_table::LeaseTable, runtime_error};
use seeed_hal_core::{
    ErrorCategory, HalResult, LeaseToken, OwnerId, ResourceId, ResourceSelector, SessionId,
    resolve_resource,
};
use seeed_hal_gpio::{
    GpioAdapter, GpioEdgeEvent, GpioEdgeRequest, GpioLineConfig, gpio_lines_capability,
};
use std::{
    collections::HashMap,
    panic::{AssertUnwindSafe, catch_unwind},
    sync::Arc,
    time::Duration,
};
use tokio::sync::{Mutex, mpsc, oneshot, watch};
use uuid::Uuid;

const COMMAND_QUEUE_CAPACITY: usize = 64;

/// Observes GPIO command admissions in tests.
#[cfg(feature = "test-support")]
#[derive(Clone)]
pub struct GpioQueueObserver {
    admitted: watch::Sender<usize>,
}

#[cfg(feature = "test-support")]
impl GpioQueueObserver {
    /// Creates an observer and its monotonically increasing admission counter.
    pub fn new() -> (Self, watch::Receiver<usize>) {
        let (admitted, observed) = watch::channel(0);
        (Self { admitted }, observed)
    }

    fn record_admission(&self) {
        self.admitted.send_modify(|count| *count += 1);
    }
}

struct Entry {
    resource: ResourceId,
    owner: OwnerId,
    worker: GpioWorker,
}
#[derive(Default)]
struct State {
    leases: LeaseTable,
    sessions: HashMap<SessionId, Entry>,
    closed: HashMap<SessionId, (ResourceId, OwnerId)>,
}
pub(crate) struct GpioManager {
    adapter: Option<Arc<dyn GpioAdapter>>,
    state: Arc<Mutex<State>>,
    close_timeout: Duration,
    #[cfg(feature = "test-support")]
    queue_observer: Option<GpioQueueObserver>,
}

enum GpioCommand {
    Read {
        reply: oneshot::Sender<HalResult<Vec<bool>>>,
    },
    Write {
        values: Vec<bool>,
        reply: oneshot::Sender<HalResult<()>>,
    },
    NextEdge {
        request: GpioEdgeRequest,
        timeout: Duration,
        reply: oneshot::Sender<HalResult<Option<GpioEdgeEvent>>>,
    },
}

impl GpioCommand {
    fn reject_closed(self) {
        match self {
            Self::Read { reply } => {
                let _ = reply.send(Err(session_closed("gpio.read")));
            }
            Self::Write { reply, .. } => {
                let _ = reply.send(Err(session_closed("gpio.write")));
            }
            Self::NextEdge { reply, .. } => {
                let _ = reply.send(Err(session_closed("gpio.next_edge")));
            }
        }
    }
}

#[derive(Clone)]
struct GpioWorker {
    commands: mpsc::Sender<GpioCommand>,
    shutdown: watch::Sender<bool>,
    completion: watch::Receiver<Option<HalResult<()>>>,
    #[cfg(feature = "test-support")]
    queue_observer: Option<GpioQueueObserver>,
}

impl GpioWorker {
    fn try_enqueue(&self, command: GpioCommand, operation: &'static str) -> HalResult<()> {
        self.commands
            .try_send(command)
            .map_err(|error| match error {
                mpsc::error::TrySendError::Full(_) => runtime_error(
                    "runtime.queue.full",
                    ErrorCategory::Unavailable,
                    operation,
                    true,
                    "the bounded GPIO line group command queue has reached its 64-command capacity",
                ),
                mpsc::error::TrySendError::Closed(_) => actor_unavailable(operation),
            })?;
        #[cfg(feature = "test-support")]
        if let Some(observer) = &self.queue_observer {
            observer.record_admission();
        }
        Ok(())
    }

    fn request_close(&self) {
        let _ = self.shutdown.send(true);
    }

    fn is_closing(&self) -> bool {
        *self.shutdown.borrow()
    }

    fn is_finished(&self) -> bool {
        self.completion.borrow().is_some()
    }

    async fn wait_closed(&self) -> HalResult<()> {
        let mut completion = self.completion.clone();
        loop {
            if let Some(result) = completion.borrow().clone() {
                return result;
            }
            if completion.changed().await.is_err() {
                return Err(actor_unavailable("gpio.close"));
            }
        }
    }
}

fn spawn_worker(
    adapter: Arc<dyn GpioAdapter>,
    selector: ResourceSelector,
    lines: Vec<u32>,
    config: GpioLineConfig,
    #[cfg(feature = "test-support")] queue_observer: Option<GpioQueueObserver>,
) -> HalResult<(GpioWorker, oneshot::Receiver<HalResult<()>>)> {
    let runtime = tokio::runtime::Handle::try_current().map_err(|_| {
        runtime_error(
            "runtime.executor.unavailable",
            ErrorCategory::Unavailable,
            "gpio.open",
            true,
            "opening a GPIO worker requires an active Tokio runtime handle",
        )
    })?;
    let (commands, mut command_rx) = mpsc::channel::<GpioCommand>(COMMAND_QUEUE_CAPACITY);
    let (opened_tx, opened_rx) = oneshot::channel();
    let (completion_tx, completion) = watch::channel(None);
    let (shutdown, worker_shutdown) = watch::channel(false);
    let name = format!("seeed-hal-gpio-{}", selector.id().as_str());
    std::thread::Builder::new()
        .name(name)
        .spawn(move || {
            let result = catch_unwind(AssertUnwindSafe(|| {
                let mut session = match runtime.block_on(adapter.open(&selector, &lines, config)) {
                    Ok(session) => {
                        let _ = opened_tx.send(Ok(()));
                        session
                    }
                    Err(error) => {
                        let _ = opened_tx.send(Err(error.clone()));
                        return Err(error);
                    }
                };
                while !*worker_shutdown.borrow() {
                    let command = match command_rx.try_recv() {
                        Ok(command) => command,
                        Err(mpsc::error::TryRecvError::Empty) => {
                            std::thread::sleep(Duration::from_millis(1));
                            continue;
                        }
                        Err(mpsc::error::TryRecvError::Disconnected) => break,
                    };
                    if *worker_shutdown.borrow() {
                        command.reject_closed();
                        break;
                    }
                    match command {
                        GpioCommand::Read { reply } => {
                            if !reply.is_closed() {
                                let _ = reply.send(runtime.block_on(session.read()));
                            }
                        }
                        GpioCommand::Write { values, reply } => {
                            if !reply.is_closed() {
                                let _ = reply.send(runtime.block_on(session.write(&values)));
                            }
                        }
                        GpioCommand::NextEdge {
                            request,
                            timeout,
                            reply,
                        } => {
                            if !reply.is_closed() {
                                let _ = reply
                                    .send(runtime.block_on(session.next_edge(request, timeout)));
                            }
                        }
                    }
                }
                let close_result = runtime.block_on(session.close());
                while let Ok(command) = command_rx.try_recv() {
                    command.reject_closed();
                }
                close_result
            }))
            .unwrap_or_else(|_| Err(actor_unavailable("gpio.worker")));
            let _ = completion_tx.send(Some(result));
        })
        .map_err(|error| {
            runtime_error(
                "runtime.actor.spawn_failed",
                ErrorCategory::Unavailable,
                "gpio.open",
                true,
                format!("failed to spawn GPIO line group worker: {error}"),
            )
        })?;
    Ok((
        GpioWorker {
            commands,
            shutdown,
            completion,
            #[cfg(feature = "test-support")]
            queue_observer,
        },
        opened_rx,
    ))
}

fn actor_unavailable(operation: &'static str) -> seeed_hal_core::HalError {
    runtime_error(
        "runtime.actor.unavailable",
        ErrorCategory::Internal,
        operation,
        false,
        "the GPIO line group worker terminated before completing the operation",
    )
}

fn session_closed(operation: &'static str) -> seeed_hal_core::HalError {
    runtime_error(
        "runtime.session.closed",
        ErrorCategory::Conflict,
        operation,
        false,
        "the GPIO session is closed",
    )
}

impl GpioManager {
    pub(crate) fn new(
        adapter: Option<Arc<dyn GpioAdapter>>,
        close_timeout: Duration,
        #[cfg(feature = "test-support")] queue_observer: Option<GpioQueueObserver>,
    ) -> Self {
        Self {
            adapter,
            state: Arc::new(Mutex::new(State::default())),
            close_timeout,
            #[cfg(feature = "test-support")]
            queue_observer,
        }
    }

    fn reap_when_finished(&self, id: SessionId, worker: GpioWorker) {
        let state = Arc::clone(&self.state);
        tokio::spawn(async move {
            let _ = worker.wait_closed().await;
            finish_session(&state, &id).await;
        });
    }

    fn release_reservation_when_finished(
        &self,
        resource: ResourceId,
        id: SessionId,
        worker: GpioWorker,
    ) {
        let state = Arc::clone(&self.state);
        tokio::spawn(async move {
            let _ = worker.wait_closed().await;
            state.lock().await.leases.release(&resource, &id);
        });
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
        let (worker, opened) = match spawn_worker(
            adapter,
            selector,
            lines,
            config,
            #[cfg(feature = "test-support")]
            self.queue_observer.clone(),
        ) {
            Ok(worker) => worker,
            Err(e) => {
                self.state.lock().await.leases.release(descriptor.id(), &id);
                return Err(e);
            }
        };
        match tokio::time::timeout(self.close_timeout, opened).await {
            Ok(Ok(Ok(()))) => {}
            Ok(Ok(Err(error))) => {
                self.state.lock().await.leases.release(descriptor.id(), &id);
                return Err(error);
            }
            Ok(Err(_)) => {
                self.state.lock().await.leases.release(descriptor.id(), &id);
                return Err(actor_unavailable("gpio.open"));
            }
            Err(_) => {
                worker.request_close();
                self.release_reservation_when_finished(descriptor.id().clone(), id, worker);
                return Err(open_timeout("gpio.open", self.close_timeout)
                    .with_resource_id(descriptor.id().clone()));
            }
        }
        let mut s = self.state.lock().await;
        if !s.leases.commit(descriptor.id(), &id, &lease) {
            worker.request_close();
            drop(s);
            self.release_reservation_when_finished(descriptor.id().clone(), id, worker);
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
                worker,
            },
        );
        Ok((id, lease))
    }
    async fn session_worker(
        &self,
        id: &SessionId,
        lease: &LeaseToken,
        op: &'static str,
    ) -> HalResult<(GpioWorker, ResourceId)> {
        let mut s = self.state.lock().await;
        match s.sessions.get(id) {
            Some(entry) => {
                s.leases
                    .validate(&entry.resource, id, &entry.owner, lease, op)?;
                if entry.worker.is_finished() {
                    let entry = s.sessions.remove(id).expect("session was looked up");
                    s.leases.release(&entry.resource, id);
                    s.closed
                        .insert(id.clone(), (entry.resource.clone(), entry.owner));
                    return Err(actor_unavailable(op).with_resource_id(entry.resource));
                }
                if entry.worker.is_closing() {
                    return Err(session_closed(op).with_resource_id(entry.resource.clone()));
                }
                Ok((entry.worker.clone(), entry.resource.clone()))
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
    pub(crate) async fn read(&self, id: SessionId, lease: &LeaseToken) -> HalResult<Vec<bool>> {
        let (worker, resource) = self.session_worker(&id, lease, "gpio.read").await?;
        let (reply, response) = oneshot::channel();
        worker
            .try_enqueue(GpioCommand::Read { reply }, "gpio.read")
            .map_err(|error| error.with_resource_id(resource.clone()))?;
        wait_for_reply(response, &worker, "gpio.read")
            .await
            .map_err(|error| error.with_resource_id(resource))
    }
    pub(crate) async fn write(
        &self,
        id: SessionId,
        lease: &LeaseToken,
        values: Vec<bool>,
    ) -> HalResult<()> {
        let (worker, resource) = self.session_worker(&id, lease, "gpio.write").await?;
        let (reply, response) = oneshot::channel();
        worker
            .try_enqueue(GpioCommand::Write { values, reply }, "gpio.write")
            .map_err(|error| error.with_resource_id(resource.clone()))?;
        wait_for_reply(response, &worker, "gpio.write")
            .await
            .map_err(|error| error.with_resource_id(resource))
    }
    pub(crate) async fn next_edge(
        &self,
        id: SessionId,
        lease: &LeaseToken,
        request: GpioEdgeRequest,
        timeout: Duration,
    ) -> HalResult<Option<GpioEdgeEvent>> {
        let (worker, resource) = self.session_worker(&id, lease, "gpio.next_edge").await?;
        let (reply, response) = oneshot::channel();
        worker
            .try_enqueue(
                GpioCommand::NextEdge {
                    request,
                    timeout,
                    reply,
                },
                "gpio.next_edge",
            )
            .map_err(|error| error.with_resource_id(resource.clone()))?;
        wait_for_reply(response, &worker, "gpio.next_edge")
            .await
            .map_err(|error| error.with_resource_id(resource))
    }
    pub(crate) async fn close(&self, id: SessionId, lease: &LeaseToken) -> HalResult<()> {
        let (worker, resource) = self.session_worker(&id, lease, "gpio.close").await?;
        worker.request_close();
        self.reap_when_finished(id.clone(), worker.clone());
        match tokio::time::timeout(self.close_timeout, worker.wait_closed()).await {
            Ok(result) => {
                let result = result.map_err(|error| error.with_resource_id(resource.clone()));
                finish_session(&self.state, &id).await;
                result
            }
            Err(_) => {
                Err(close_timeout("gpio.close", self.close_timeout).with_resource_id(resource))
            }
        }
    }
    pub(crate) async fn revoke_owner(&self, owner: &OwnerId) -> HalResult<()> {
        let workers = {
            let state = self.state.lock().await;
            let ids = state
                .sessions
                .iter()
                .filter(|(_, entry)| &entry.owner == owner)
                .map(|(id, _)| id.clone())
                .collect::<Vec<_>>();
            let mut workers = Vec::new();
            for id in ids {
                if let Some(entry) = state.sessions.get(&id) {
                    entry.worker.request_close();
                    workers.push((id, entry.worker.clone(), entry.resource.clone()));
                }
            }
            workers
        };
        for (id, worker, resource) in workers {
            self.reap_when_finished(id.clone(), worker.clone());
            match tokio::time::timeout(self.close_timeout, worker.wait_closed()).await {
                Ok(result) => {
                    result.map_err(|error| error.with_resource_id(resource.clone()))?;
                    finish_session(&self.state, &id).await;
                }
                Err(_) => {
                    return Err(close_timeout("gpio.revoke_owner", self.close_timeout)
                        .with_resource_id(resource));
                }
            }
        }
        Ok(())
    }
}

async fn finish_session(state: &Mutex<State>, id: &SessionId) {
    let mut state = state.lock().await;
    if let Some(entry) = state.sessions.remove(id) {
        state.leases.release(&entry.resource, id);
        state
            .closed
            .insert(id.clone(), (entry.resource.clone(), entry.owner));
    }
}

async fn wait_for_reply<T>(
    response: oneshot::Receiver<HalResult<T>>,
    worker: &GpioWorker,
    operation: &'static str,
) -> HalResult<T> {
    let mut closing = worker.shutdown.subscribe();
    tokio::select! {
        result = response => result.unwrap_or_else(|_| Err(actor_unavailable(operation))),
        changed = closing.changed() => {
            let _ = changed;
            Err(session_closed(operation))
        }
    }
}

fn close_timeout(operation: &'static str, timeout: Duration) -> seeed_hal_core::HalError {
    runtime_error(
        "runtime.session.close_timeout",
        ErrorCategory::Unavailable,
        operation,
        false,
        format!(
            "GPIO worker did not release its native session within {timeout:?}; the resource remains quarantined"
        ),
    )
}

fn open_timeout(operation: &'static str, timeout: Duration) -> seeed_hal_core::HalError {
    runtime_error(
        "runtime.transport.timeout",
        ErrorCategory::Unavailable,
        operation,
        true,
        format!(
            "GPIO worker did not finish opening within {timeout:?}; the resource remains quarantined"
        ),
    )
}
