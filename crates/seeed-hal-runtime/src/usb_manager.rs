use crate::{lease_table::LeaseTable, runtime_error};
use bytes::Bytes;
use seeed_hal_core::{
    ErrorCategory, HalResult, LeaseToken, OwnerId, ResourceId, ResourceSelector, SessionId,
    resolve_resource,
};
use seeed_hal_usb::{UsbAdapter, UsbInterfaceClaim, UsbTransfer, usb_control_capability};
use std::{
    collections::HashMap,
    panic::{AssertUnwindSafe, catch_unwind},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};
use tokio::sync::{Mutex, mpsc, oneshot, watch};
use uuid::Uuid;

const COMMAND_QUEUE_CAPACITY: usize = 64;

struct Entry {
    resource: ResourceId,
    owner: OwnerId,
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
    state: Mutex<State>,
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
    shutdown: Arc<AtomicBool>,
    completion: watch::Receiver<Option<HalResult<()>>>,
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
            })
    }

    fn request_close(&self) {
        self.shutdown.store(true, Ordering::Release);
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
    let shutdown = Arc::new(AtomicBool::new(false));
    let worker_shutdown = Arc::clone(&shutdown);
    let name = format!("seeed-hal-usb-{}", selector.id().as_str());
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
                while !worker_shutdown.load(Ordering::Acquire) {
                    let command = match command_rx.try_recv() {
                        Ok(command) => command,
                        Err(mpsc::error::TryRecvError::Empty) => {
                            std::thread::sleep(Duration::from_millis(1));
                            continue;
                        }
                        Err(mpsc::error::TryRecvError::Disconnected) => break,
                    };
                    if worker_shutdown.load(Ordering::Acquire) {
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
        "the USB interface worker terminated before completing the operation",
    )
}

fn session_closed(operation: &'static str) -> seeed_hal_core::HalError {
    runtime_error(
        "runtime.session.closed",
        ErrorCategory::Conflict,
        operation,
        false,
        "the USB session is closed",
    )
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
        let (worker, opened) = match spawn_worker(adapter, selector, claim) {
            Ok(worker) => worker,
            Err(e) => {
                self.state.lock().await.leases.release(descriptor.id(), &id);
                return Err(e);
            }
        };
        if let Err(e) = opened
            .await
            .unwrap_or_else(|_| Err(actor_unavailable("usb.open")))
        {
            self.state.lock().await.leases.release(descriptor.id(), &id);
            return Err(e);
        }
        let mut state = self.state.lock().await;
        if !state.leases.commit(descriptor.id(), &id, &lease) {
            worker.request_close();
            drop(state);
            let _ = worker.wait_closed().await;
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
        response
            .await
            .unwrap_or_else(|_| Err(actor_unavailable("usb.transfer")))
            .map_err(|error| error.with_resource_id(resource))
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
        entry.worker.request_close();
        entry
            .worker
            .wait_closed()
            .await
            .map_err(|e| e.with_resource_id(entry.resource))
    }
    pub(crate) async fn revoke_owner(&self, owner: &OwnerId) -> HalResult<()> {
        let workers = {
            let mut state = self.state.lock().await;
            let ids = state
                .sessions
                .iter()
                .filter(|(_, entry)| &entry.owner == owner)
                .map(|(id, _)| id.clone())
                .collect::<Vec<_>>();
            let mut workers = Vec::new();
            for id in ids {
                if let Some(entry) = state.sessions.remove(&id) {
                    state.leases.release(&entry.resource, &id);
                    state
                        .closed
                        .insert(id, (entry.resource.clone(), entry.owner.clone()));
                    entry.worker.request_close();
                    workers.push((entry.worker, entry.resource));
                }
            }
            workers
        };
        for (worker, resource) in workers {
            worker
                .wait_closed()
                .await
                .map_err(|error| error.with_resource_id(resource))?;
        }
        Ok(())
    }
}
