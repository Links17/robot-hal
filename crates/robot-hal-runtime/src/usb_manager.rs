use crate::session_lifecycle::SessionLifecycle;
use crate::{lease_table::LeaseTable, runtime_error};
use bytes::Bytes;
use robot_hal_core::{
    ErrorCategory, HalResult, LeaseToken, OwnerId, ResourceId, ResourceSelector, SessionId,
    resolve_resource,
};
use robot_hal_usb::{UsbAdapter, UsbInterfaceClaim, UsbTransfer, usb_control_capability};
use std::{
    collections::HashMap,
    panic::{AssertUnwindSafe, catch_unwind},
    sync::Arc,
    time::Duration,
};
use tokio::sync::{Mutex, mpsc, oneshot, watch};
use uuid::Uuid;

const COMMAND_QUEUE_CAPACITY: usize = 64;

/// Observes USB command admissions in tests.
#[cfg(feature = "test-support")]
#[derive(Clone)]
pub struct UsbQueueObserver {
    admitted: watch::Sender<usize>,
}

#[cfg(feature = "test-support")]
impl UsbQueueObserver {
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
    lifecycle: SessionLifecycle,
    worker: UsbWorker,
}
#[derive(Default)]
struct State {
    leases: LeaseTable,
    sessions: HashMap<SessionId, Entry>,
    closed: HashMap<SessionId, (ResourceId, OwnerId)>,
}
pub(crate) struct UsbManager {
    adapter: Option<Arc<dyn UsbAdapter>>,
    state: Arc<Mutex<State>>,
    close_timeout: Duration,
    #[cfg(feature = "test-support")]
    queue_observer: Option<UsbQueueObserver>,
}

enum UsbCommand {
    Transfer {
        transfer: UsbTransfer,
        timeout: Duration,
        reply: oneshot::Sender<HalResult<Bytes>>,
    },
}

impl UsbCommand {
    fn reject_closed(self) {
        let Self::Transfer { reply, .. } = self;
        let _ = reply.send(Err(session_closed("usb.transfer")));
    }
}

#[derive(Clone)]
struct UsbWorker {
    commands: mpsc::Sender<UsbCommand>,
    shutdown: watch::Sender<bool>,
    completion: watch::Receiver<Option<HalResult<()>>>,
    #[cfg(feature = "test-support")]
    queue_observer: Option<UsbQueueObserver>,
}

impl UsbWorker {
    fn try_enqueue(&self, command: UsbCommand, operation: &'static str) -> HalResult<()> {
        self.commands
            .try_send(command)
            .map_err(|error| match error {
                mpsc::error::TrySendError::Full(_) => runtime_error(
                    "runtime.queue.full",
                    ErrorCategory::Unavailable,
                    operation,
                    true,
                    "the bounded USB interface command queue has reached its 64-command capacity",
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
                return Err(actor_unavailable("usb.close"));
            }
        }
    }
}

fn spawn_worker(
    adapter: Arc<dyn UsbAdapter>,
    selector: ResourceSelector,
    claim: UsbInterfaceClaim,
    #[cfg(feature = "test-support")] queue_observer: Option<UsbQueueObserver>,
) -> HalResult<(UsbWorker, oneshot::Receiver<HalResult<()>>)> {
    let runtime = tokio::runtime::Handle::try_current().map_err(|_| {
        runtime_error(
            "runtime.executor.unavailable",
            ErrorCategory::Unavailable,
            "usb.open",
            true,
            "opening a USB worker requires an active Tokio runtime handle",
        )
    })?;
    let (commands, mut command_rx) = mpsc::channel::<UsbCommand>(COMMAND_QUEUE_CAPACITY);
    let (opened_tx, opened_rx) = oneshot::channel();
    let (completion_tx, completion) = watch::channel(None);
    let (shutdown, worker_shutdown) = watch::channel(false);
    let name = format!("robot-hal-usb-{}", selector.id().as_str());
    std::thread::Builder::new()
        .name(name)
        .spawn(move || {
            let result = catch_unwind(AssertUnwindSafe(|| {
                let mut session = match runtime.block_on(adapter.open(&selector, claim)) {
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
                        UsbCommand::Transfer {
                            transfer,
                            timeout,
                            reply,
                        } => {
                            // A cancelled caller does not cause native session I/O to start.
                            if !reply.is_closed() {
                                let result = runtime.block_on(session.transfer(transfer, timeout));
                                let _ = reply.send(result);
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
            .unwrap_or_else(|_| Err(actor_unavailable("usb.worker")));
            let _ = completion_tx.send(Some(result));
        })
        .map_err(|error| {
            runtime_error(
                "runtime.actor.spawn_failed",
                ErrorCategory::Unavailable,
                "usb.open",
                true,
                format!("failed to spawn USB interface worker: {error}"),
            )
        })?;
    Ok((
        UsbWorker {
            commands,
            shutdown,
            completion,
            #[cfg(feature = "test-support")]
            queue_observer,
        },
        opened_rx,
    ))
}

fn actor_unavailable(operation: &'static str) -> robot_hal_core::HalError {
    runtime_error(
        "runtime.actor.unavailable",
        ErrorCategory::Internal,
        operation,
        false,
        "the USB interface worker terminated before completing the operation",
    )
}

fn session_closed(operation: &'static str) -> robot_hal_core::HalError {
    runtime_error(
        "runtime.session.closed",
        ErrorCategory::Conflict,
        operation,
        false,
        "the USB session is closed",
    )
}

impl UsbManager {
    pub(crate) fn new(
        adapter: Option<Arc<dyn UsbAdapter>>,
        close_timeout: Duration,
        #[cfg(feature = "test-support")] queue_observer: Option<UsbQueueObserver>,
    ) -> Self {
        Self {
            adapter,
            state: Arc::new(Mutex::new(State::default())),
            close_timeout,
            #[cfg(feature = "test-support")]
            queue_observer,
        }
    }

    fn reap_when_finished(&self, id: SessionId, worker: UsbWorker) {
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
        worker: UsbWorker,
    ) {
        let state = Arc::clone(&self.state);
        tokio::spawn(async move {
            let _ = worker.wait_closed().await;
            state.lock().await.leases.release(&resource, &id);
        });
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
    pub(crate) async fn enumerate(&self) -> HalResult<Vec<robot_hal_core::ResourceDescriptor>> {
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
        let (worker, opened) = match spawn_worker(
            adapter,
            selector,
            claim,
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
                return Err(actor_unavailable("usb.open"));
            }
            Err(_) => {
                worker.request_close();
                self.release_reservation_when_finished(descriptor.id().clone(), id, worker);
                return Err(open_timeout("usb.open", self.close_timeout)
                    .with_resource_id(descriptor.id().clone()));
            }
        }
        let mut state = self.state.lock().await;
        if !state.leases.commit(descriptor.id(), &id, &lease) {
            worker.request_close();
            drop(state);
            self.release_reservation_when_finished(descriptor.id().clone(), id, worker);
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
                lifecycle: SessionLifecycle::Opening
                    .commit_open("usb.open")
                    .expect("a committed USB session starts active"),
                worker,
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
        let (worker, resource) = {
            let mut state = self.state.lock().await;
            let entry = match state.sessions.get(&id) {
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
            if entry.worker.is_finished() {
                let entry = state.sessions.remove(&id).expect("session was looked up");
                state.leases.release(&entry.resource, &id);
                state
                    .closed
                    .insert(id, (entry.resource.clone(), entry.owner));
                return Err(actor_unavailable("usb.transfer").with_resource_id(entry.resource));
            }
            if !entry.lifecycle.is_active() || entry.worker.is_closing() {
                return Err(session_closed("usb.transfer").with_resource_id(entry.resource.clone()));
            }
            (entry.worker.clone(), entry.resource.clone())
        };
        let (reply, response) = oneshot::channel();
        worker
            .try_enqueue(
                UsbCommand::Transfer {
                    transfer,
                    timeout,
                    reply,
                },
                "usb.transfer",
            )
            .map_err(|error| error.with_resource_id(resource.clone()))?;
        let mut closing = worker.shutdown.subscribe();
        tokio::select! {
            result = response => result.unwrap_or_else(|_| Err(actor_unavailable("usb.transfer"))),
            changed = closing.changed() => {
                let _ = changed;
                Err(session_closed("usb.transfer"))
            }
        }
        .map_err(|error| error.with_resource_id(resource))
    }
    pub(crate) async fn close(&self, id: SessionId, lease: &LeaseToken) -> HalResult<()> {
        let mut state = self.state.lock().await;
        let entry = state.sessions.get(&id).ok_or_else(|| {
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
        let resource = entry.resource.clone();
        let worker = entry.worker.clone();
        let lifecycle = entry.lifecycle.begin_close("usb.close")?;
        state
            .sessions
            .get_mut(&id)
            .expect("session was looked up")
            .lifecycle = lifecycle;
        drop(state);
        worker.request_close();
        self.reap_when_finished(id.clone(), worker.clone());
        match tokio::time::timeout(self.close_timeout, worker.wait_closed()).await {
            Ok(result) => {
                let result = result.map_err(|e| e.with_resource_id(resource.clone()));
                finish_session(&self.state, &id).await;
                result
            }
            Err(_) => {
                Err(close_timeout("usb.close", self.close_timeout).with_resource_id(resource))
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
                    return Err(close_timeout("usb.revoke_owner", self.close_timeout)
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

fn close_timeout(operation: &'static str, timeout: Duration) -> robot_hal_core::HalError {
    runtime_error(
        "runtime.session.close_timeout",
        ErrorCategory::Unavailable,
        operation,
        false,
        format!(
            "USB worker did not release its native session within {timeout:?}; the resource remains quarantined"
        ),
    )
}

fn open_timeout(operation: &'static str, timeout: Duration) -> robot_hal_core::HalError {
    runtime_error(
        "runtime.transport.timeout",
        ErrorCategory::Unavailable,
        operation,
        true,
        format!(
            "USB worker did not finish opening within {timeout:?}; the resource remains quarantined"
        ),
    )
}
