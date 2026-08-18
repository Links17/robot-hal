use crate::{lease_table::LeaseTable, runtime_error};
use seeed_hal_adapter_shared_memory::{
    BrokerMapping, FrameLease, FrameMetadata, MappingDescriptor, PixelFormat, PlaneLayout,
    RingConfig,
};
use seeed_hal_camera::{
    CameraAdapter, CameraControlDescriptor, CameraControlKind, CameraControlValue, CameraFrameSink,
    CameraRequest, camera_capture_capability,
};
use seeed_hal_core::{
    ErrorCategory, HalError, HalResult, LeaseToken, OwnerId, ResourceId, ResourceSelector,
    SessionId, resolve_resource,
};
use std::{
    collections::HashMap,
    panic::{AssertUnwindSafe, catch_unwind},
    sync::{Arc, Mutex as StdMutex},
    time::Duration,
};
use tokio::sync::{Mutex, mpsc, oneshot, watch};
use uuid::Uuid;

const COMMAND_QUEUE_CAPACITY: usize = 64;

struct Entry {
    resource: ResourceId,
    owner: OwnerId,
    lease: LeaseToken,
    worker: CameraWorker,
}

#[derive(Default)]
struct State {
    leases: LeaseTable,
    sessions: HashMap<SessionId, Entry>,
    closed: HashMap<SessionId, ClosedEntry>,
}

struct ClosedEntry {
    resource: ResourceId,
    lease: LeaseToken,
    terminal_error: Option<HalError>,
}

pub(crate) struct CameraManager {
    adapter: Option<Arc<dyn CameraAdapter>>,
    state: Arc<Mutex<State>>,
    close_timeout: Duration,
}

enum CameraCommand {
    Capture {
        timeout: Duration,
        reply: oneshot::Sender<HalResult<()>>,
    },
    MappingDescriptor {
        reply: oneshot::Sender<HalResult<MappingDescriptor>>,
    },
    NextFrameLease {
        reply: oneshot::Sender<HalResult<Option<FrameLease>>>,
    },
    DroppedCount {
        reply: oneshot::Sender<HalResult<u64>>,
    },
    Controls {
        reply: oneshot::Sender<HalResult<Vec<CameraControlDescriptor>>>,
    },
    GetControl {
        kind: CameraControlKind,
        reply: oneshot::Sender<HalResult<CameraControlValue>>,
    },
    SetControl {
        kind: CameraControlKind,
        value: CameraControlValue,
        reply: oneshot::Sender<HalResult<()>>,
    },
    SetAuto {
        kind: CameraControlKind,
        enabled: bool,
        reply: oneshot::Sender<HalResult<()>>,
    },
}

enum EnqueueError {
    Full(HalError),
    Closed,
}

impl CameraCommand {
    fn reject_closed(self) {
        match self {
            Self::Capture { reply, .. } => {
                let _ = reply.send(Err(session_closed("camera.capture")));
            }
            Self::MappingDescriptor { reply } => {
                let _ = reply.send(Err(session_closed("camera.mapping_descriptor")));
            }
            Self::NextFrameLease { reply } => {
                let _ = reply.send(Err(session_closed("camera.next_frame_lease")));
            }
            Self::DroppedCount { reply } => {
                let _ = reply.send(Err(session_closed("camera.dropped_count")));
            }
            Self::Controls { reply } => {
                let _ = reply.send(Err(session_closed("camera.controls")));
            }
            Self::GetControl { reply, .. } => {
                let _ = reply.send(Err(session_closed("camera.control.get")));
            }
            Self::SetControl { reply, .. } => {
                let _ = reply.send(Err(session_closed("camera.control.set")));
            }
            Self::SetAuto { reply, .. } => {
                let _ = reply.send(Err(session_closed("camera.control.auto")));
            }
        }
    }
}

#[derive(Clone)]
struct CameraWorker {
    commands: mpsc::Sender<CameraCommand>,
    shutdown: watch::Sender<bool>,
    admission: Arc<Mutex<()>>,
    completion: watch::Receiver<Option<HalResult<()>>>,
    terminal_error: watch::Receiver<Option<HalError>>,
}

impl CameraWorker {
    fn try_enqueue(
        &self,
        command: CameraCommand,
        operation: &'static str,
    ) -> Result<(), EnqueueError> {
        self.commands
            .try_send(command)
            .map_err(|error| match error {
                mpsc::error::TrySendError::Full(_) => EnqueueError::Full(runtime_error(
                    "runtime.queue.full",
                    ErrorCategory::Unavailable,
                    operation,
                    true,
                    "the bounded camera command queue has reached its 64-command capacity",
                )),
                mpsc::error::TrySendError::Closed(_) => EnqueueError::Closed,
            })
    }

    async fn request_close(&self) {
        let _admission = self.admission.lock().await;
        let _ = self.shutdown.send(true);
    }
    fn is_closing(&self) -> bool {
        *self.shutdown.borrow()
    }
    fn is_finished(&self) -> bool {
        self.completion.borrow().is_some()
    }
    fn terminal_error(&self) -> Option<HalError> {
        self.terminal_error.borrow().clone()
    }

    async fn wait_closed(&self) -> HalResult<()> {
        let mut completion = self.completion.clone();
        loop {
            if let Some(result) = completion.borrow().clone() {
                return result;
            }
            if completion.changed().await.is_err() {
                return Err(actor_unavailable("camera.close"));
            }
        }
    }
}

fn spawn_worker(
    adapter: Arc<dyn CameraAdapter>,
    selector: ResourceSelector,
    request: CameraRequest,
    generation: u64,
) -> HalResult<(CameraWorker, oneshot::Receiver<HalResult<()>>)> {
    let runtime = tokio::runtime::Handle::try_current().map_err(|_| {
        runtime_error(
            "runtime.executor.unavailable",
            ErrorCategory::Unavailable,
            "camera.open",
            true,
            "opening a camera worker requires an active Tokio runtime handle",
        )
    })?;
    let (commands, mut command_rx) = mpsc::channel::<CameraCommand>(COMMAND_QUEUE_CAPACITY);
    let (opened_tx, opened_rx) = oneshot::channel();
    let (completion_tx, completion) = watch::channel(None);
    let (terminal_error_tx, terminal_error) = watch::channel(None);
    let (shutdown, worker_shutdown) = watch::channel(false);
    let admission = Arc::new(Mutex::new(()));
    let worker_admission = Arc::clone(&admission);
    let name = format!("seeed-hal-camera-{}", selector.id().as_str());
    std::thread::Builder::new()
        .name(name)
        .spawn(move || {
            let result = catch_unwind(AssertUnwindSafe(|| {
                let mut session = match runtime.block_on(adapter.open(&selector, &request)) {
                    Ok(session) => session,
                    Err(error) => {
                        let _ = opened_tx.send(Err(error.clone()));
                        return Err(error);
                    }
                };
                let config = RingConfig::new(
                    session.format().clone(),
                    request.slot_count(),
                    session.format().worst_case_frame_bytes()?,
                )?;
                let mapping = match BrokerMapping::create(config) {
                    Ok(mapping) => mapping,
                    Err(error) => {
                        let _ = opened_tx.send(Err(error.clone()));
                        let _ = runtime.block_on(session.close());
                        return Err(error);
                    }
                };
                let mapping = Arc::new(StdMutex::new(mapping));
                let _ = opened_tx.send(Ok(()));
                let mut terminal_error = None;
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
                    let _admission = runtime.block_on(worker_admission.lock());
                    if *worker_shutdown.borrow() {
                        command.reject_closed();
                        break;
                    }
                    let terminal = match command {
                        CameraCommand::Capture { timeout, reply } => {
                            if reply.is_closed() {
                                None
                            } else {
                                let sink: Arc<dyn CameraFrameSink> = Arc::new(RuntimeFrameSink {
                                    mapping: Arc::clone(&mapping),
                                    generation,
                                });
                                let result = runtime.block_on(session.capture_into(timeout, sink));
                                let terminal = result
                                    .as_ref()
                                    .err()
                                    .filter(|error| is_terminal(error))
                                    .cloned();
                                let _ = reply.send(result.clone());
                                terminal
                            }
                        }
                        CameraCommand::MappingDescriptor { reply } => {
                            let mapping = mapping.lock().expect("camera mapping mutex poisoned");
                            let _ = reply.send(Ok(mapping.descriptor().clone()));
                            None
                        }
                        CameraCommand::NextFrameLease { reply } => {
                            let mut mapping =
                                mapping.lock().expect("camera mapping mutex poisoned");
                            let _ = reply.send(mapping.next_frame_lease());
                            None
                        }
                        CameraCommand::DroppedCount { reply } => {
                            let mapping = mapping.lock().expect("camera mapping mutex poisoned");
                            let _ = reply.send(Ok(mapping.dropped_count()));
                            None
                        }
                        CameraCommand::Controls { reply } => {
                            let result = runtime.block_on(session.controls());
                            let terminal = result
                                .as_ref()
                                .err()
                                .filter(|error| is_terminal(error))
                                .cloned();
                            let _ = reply.send(result);
                            terminal
                        }
                        CameraCommand::GetControl { kind, reply } => {
                            let result = runtime.block_on(session.get_control(kind));
                            let terminal = result
                                .as_ref()
                                .err()
                                .filter(|error| is_terminal(error))
                                .cloned();
                            let _ = reply.send(result);
                            terminal
                        }
                        CameraCommand::SetControl { kind, value, reply } => {
                            let result = runtime.block_on(session.set_control(kind, value));
                            let terminal = result
                                .as_ref()
                                .err()
                                .filter(|error| is_terminal(error))
                                .cloned();
                            let _ = reply.send(result);
                            terminal
                        }
                        CameraCommand::SetAuto {
                            kind,
                            enabled,
                            reply,
                        } => {
                            let result = runtime.block_on(session.set_auto(kind, enabled));
                            let terminal = result
                                .as_ref()
                                .err()
                                .filter(|error| is_terminal(error))
                                .cloned();
                            let _ = reply.send(result);
                            terminal
                        }
                    };
                    if let Some(error) = terminal {
                        let _ = terminal_error_tx.send(Some(error.clone()));
                        terminal_error = Some(error);
                        break;
                    }
                }
                let mapping_result = mapping
                    .lock()
                    .expect("camera mapping mutex poisoned")
                    .close();
                let close_result = runtime.block_on(session.close());
                while let Ok(command) = command_rx.try_recv() {
                    command.reject_closed();
                }
                if let Some(error) = terminal_error {
                    // The native terminal outcome has priority, but completion still retains
                    // cleanup errors through the worker's result below when no native terminal
                    // occurred.
                    let _ = (mapping_result, close_result);
                    Err(error)
                } else {
                    let cleanup = close_result.and(mapping_result);
                    if let Err(error) = &cleanup {
                        let _ = terminal_error_tx.send(Some(error.clone()));
                    }
                    cleanup
                }
            }))
            .unwrap_or_else(|_| Err(actor_unavailable("camera.worker")));
            if let Err(error) = &result {
                if terminal_error_tx.borrow().is_none() {
                    let _ = terminal_error_tx.send(Some(error.clone()));
                }
            }
            let _ = completion_tx.send(Some(result));
        })
        .map_err(|error| {
            runtime_error(
                "runtime.actor.spawn_failed",
                ErrorCategory::Unavailable,
                "camera.open",
                true,
                format!("failed to spawn camera worker: {error}"),
            )
        })?;
    Ok((
        CameraWorker {
            commands,
            shutdown,
            admission,
            completion,
            terminal_error,
        },
        opened_rx,
    ))
}

struct RuntimeFrameSink {
    mapping: Arc<StdMutex<BrokerMapping>>,
    generation: u64,
}

impl CameraFrameSink for RuntimeFrameSink {
    fn publish(
        &self,
        metadata: seeed_hal_camera::CameraFrameMetadata,
        copy_payload: &mut dyn FnMut(&mut [u8]) -> HalResult<usize>,
    ) -> HalResult<()> {
        let metadata = FrameMetadata::new(
            PixelFormat::from(metadata.format().pixel_format()),
            metadata.format().width(),
            metadata.format().height(),
            metadata.sequence(),
            self.generation,
            metadata.monotonic_timestamp_ns(),
            metadata.dropped_count(),
            metadata
                .planes()
                .iter()
                .map(|plane| PlaneLayout::new(plane.offset(), plane.length(), plane.stride()))
                .collect::<HalResult<Vec<_>>>()?,
        )?;
        self.mapping
            .lock()
            .expect("camera mapping mutex poisoned")
            .writer()
            .publish_with(metadata, copy_payload)
    }
}

fn is_terminal(error: &HalError) -> bool {
    error.category() == ErrorCategory::Unavailable
}
fn actor_unavailable(op: &'static str) -> HalError {
    runtime_error(
        "runtime.actor.unavailable",
        ErrorCategory::Internal,
        op,
        false,
        "the camera worker terminated before completing the operation",
    )
}
fn session_closed(op: &'static str) -> HalError {
    runtime_error(
        "runtime.session.closed",
        ErrorCategory::Conflict,
        op,
        false,
        "the camera session is closed",
    )
}
fn close_timeout(op: &'static str, timeout: Duration) -> HalError {
    runtime_error(
        "runtime.session.close_timeout",
        ErrorCategory::Unavailable,
        op,
        false,
        format!(
            "camera worker did not release its native session within {timeout:?}; the resource remains quarantined"
        ),
    )
}
fn open_timeout(op: &'static str, timeout: Duration) -> HalError {
    runtime_error(
        "runtime.transport.timeout",
        ErrorCategory::Unavailable,
        op,
        true,
        format!(
            "camera worker did not finish opening within {timeout:?}; the resource remains quarantined"
        ),
    )
}

impl CameraManager {
    pub(crate) fn new(adapter: Option<Arc<dyn CameraAdapter>>, close_timeout: Duration) -> Self {
        Self {
            adapter,
            state: Arc::new(Mutex::new(State::default())),
            close_timeout,
        }
    }
    fn adapter(&self, op: &'static str) -> HalResult<Arc<dyn CameraAdapter>> {
        self.adapter.clone().ok_or_else(|| {
            runtime_error(
                "runtime.adapter.not_configured",
                ErrorCategory::Unavailable,
                op,
                false,
                "no Camera adapter was registered",
            )
        })
    }
    pub(crate) async fn enumerate(&self) -> HalResult<Vec<seeed_hal_core::ResourceDescriptor>> {
        self.adapter("camera.enumerate")?.enumerate().await
    }
    fn reap_when_finished(&self, id: SessionId, worker: CameraWorker) {
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
        lease: LeaseToken,
        worker: CameraWorker,
    ) {
        let state = Arc::clone(&self.state);
        tokio::spawn(async move {
            let _ = worker.wait_closed().await;
            let mut state = state.lock().await;
            state.leases.quarantine(&resource, &id, &lease);
            state.leases.release(&resource, &id);
        });
    }
    pub(crate) async fn open(
        &self,
        owner: OwnerId,
        selector: ResourceSelector,
        request: CameraRequest,
    ) -> HalResult<(SessionId, LeaseToken)> {
        let adapter = self.adapter("camera.open")?;
        let descriptor = resolve_resource(
            &adapter.enumerate().await?,
            &selector,
            &camera_capture_capability(),
            "camera.open",
        )?
        .clone();
        let id = SessionId::parse(Uuid::new_v4().to_string())?;
        let lease = {
            self.state.lock().await.leases.reserve_control(
                descriptor.id().clone(),
                id.clone(),
                owner.clone(),
            )?
        };
        let (worker, opened) = match spawn_worker(adapter, selector, request, lease.generation()) {
            Ok(worker) => worker,
            Err(error) => {
                self.state.lock().await.leases.release(descriptor.id(), &id);
                return Err(error);
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
                return Err(actor_unavailable("camera.open"));
            }
            Err(_) => {
                worker.request_close().await;
                self.release_reservation_when_finished(descriptor.id().clone(), id, lease, worker);
                return Err(open_timeout("camera.open", self.close_timeout)
                    .with_resource_id(descriptor.id().clone()));
            }
        }
        let mut state = self.state.lock().await;
        if !state.leases.commit(descriptor.id(), &id, &lease) {
            worker.request_close().await;
            drop(state);
            self.release_reservation_when_finished(descriptor.id().clone(), id, lease, worker);
            return Err(session_closed("camera.open").with_resource_id(descriptor.id().clone()));
        }
        state.sessions.insert(
            id.clone(),
            Entry {
                resource: descriptor.id().clone(),
                owner,
                lease: lease.clone(),
                worker: worker.clone(),
            },
        );
        drop(state);
        self.reap_when_finished(id.clone(), worker);
        Ok((id, lease))
    }
    async fn worker(
        &self,
        id: &SessionId,
        lease: &LeaseToken,
        op: &'static str,
    ) -> HalResult<(CameraWorker, ResourceId)> {
        let mut state = self.state.lock().await;
        let Some(entry) = state.sessions.get(id) else {
            if let Some(closed) = state.closed.get(id) {
                let current_generation = state.leases.current_generation(&closed.resource);
                if lease.generation() < current_generation {
                    return Err(runtime_error(
                        "runtime.lease.stale_generation",
                        ErrorCategory::Conflict,
                        op,
                        false,
                        "lease generation is older than the closed camera session generation",
                    )
                    .with_resource_id(closed.resource.clone()));
                }
                if lease != &closed.lease {
                    return Err(runtime_error(
                        "runtime.lease.invalid_token",
                        ErrorCategory::Conflict,
                        op,
                        false,
                        "the lease token does not match the closed camera session",
                    )
                    .with_resource_id(closed.resource.clone()));
                }
                if let Some(error) = &closed.terminal_error {
                    return Err(error.clone().with_resource_id(closed.resource.clone()));
                }
            }
            return Err(session_closed(op));
        };
        state
            .leases
            .validate(&entry.resource, id, &entry.owner, lease, op)?;
        if entry.worker.is_finished() {
            let entry = state.sessions.remove(id).expect("session was present");
            state.leases.release(&entry.resource, id);
            state.closed.insert(
                id.clone(),
                ClosedEntry {
                    resource: entry.resource.clone(),
                    lease: entry.lease,
                    terminal_error: entry.worker.terminal_error(),
                },
            );
            return Err(entry
                .worker
                .terminal_error()
                .unwrap_or_else(|| actor_unavailable(op))
                .with_resource_id(entry.resource));
        }
        if entry.worker.is_closing() {
            return Err(session_closed(op).with_resource_id(entry.resource.clone()));
        }
        Ok((entry.worker.clone(), entry.resource.clone()))
    }
    async fn request<T>(
        &self,
        id: SessionId,
        lease: &LeaseToken,
        op: &'static str,
        command: impl FnOnce(oneshot::Sender<HalResult<T>>) -> CameraCommand,
    ) -> HalResult<T> {
        let (worker, resource) = self.worker(&id, lease, op).await?;
        // This is the linearization point shared with native command execution:
        // a worker that can publish a terminal result holds this lock until its
        // result has been sent to `terminal_error`.
        let admission = worker.admission.lock().await;
        if worker.is_closing() {
            return Err(session_closed(op).with_resource_id(resource));
        }
        if let Some(error) = worker.terminal_error() {
            return Err(error.with_resource_id(resource));
        }
        let (reply, response) = oneshot::channel();
        match worker.try_enqueue(command(reply), op) {
            Ok(()) => {}
            Err(EnqueueError::Full(error)) => {
                return Err(error.with_resource_id(resource));
            }
            Err(EnqueueError::Closed) => {
                return Err(worker
                    .terminal_error()
                    .unwrap_or_else(|| actor_unavailable(op))
                    .with_resource_id(resource));
            }
        }
        drop(admission);
        let mut closing = worker.shutdown.subscribe();
        tokio::select! {
            result = response => result.unwrap_or_else(|_| Err(actor_unavailable(op))),
            changed = closing.changed() => { let _ = changed; Err(session_closed(op)) }
        }
        .map_err(|error| error.with_resource_id(resource))
    }
    pub(crate) async fn capture(
        &self,
        id: SessionId,
        lease: &LeaseToken,
        timeout: Duration,
    ) -> HalResult<()> {
        self.request(id, lease, "camera.capture", |reply| {
            CameraCommand::Capture { timeout, reply }
        })
        .await
    }
    pub(crate) async fn mapping_descriptor(
        &self,
        id: SessionId,
        lease: &LeaseToken,
    ) -> HalResult<MappingDescriptor> {
        self.request(id, lease, "camera.mapping_descriptor", |reply| {
            CameraCommand::MappingDescriptor { reply }
        })
        .await
    }
    pub(crate) async fn next_frame_lease(
        &self,
        id: SessionId,
        lease: &LeaseToken,
    ) -> HalResult<Option<FrameLease>> {
        self.request(id, lease, "camera.next_frame_lease", |reply| {
            CameraCommand::NextFrameLease { reply }
        })
        .await
    }
    pub(crate) async fn dropped_count(&self, id: SessionId, lease: &LeaseToken) -> HalResult<u64> {
        self.request(id, lease, "camera.dropped_count", |reply| {
            CameraCommand::DroppedCount { reply }
        })
        .await
    }
    pub(crate) async fn controls(
        &self,
        id: SessionId,
        lease: &LeaseToken,
    ) -> HalResult<Vec<CameraControlDescriptor>> {
        self.request(id, lease, "camera.controls", |reply| {
            CameraCommand::Controls { reply }
        })
        .await
    }
    pub(crate) async fn get_control(
        &self,
        id: SessionId,
        lease: &LeaseToken,
        kind: CameraControlKind,
    ) -> HalResult<CameraControlValue> {
        self.request(id, lease, "camera.control.get", |reply| {
            CameraCommand::GetControl { kind, reply }
        })
        .await
    }
    pub(crate) async fn set_control(
        &self,
        id: SessionId,
        lease: &LeaseToken,
        kind: CameraControlKind,
        value: CameraControlValue,
    ) -> HalResult<()> {
        self.request(id, lease, "camera.control.set", |reply| {
            CameraCommand::SetControl { kind, value, reply }
        })
        .await
    }
    pub(crate) async fn set_auto(
        &self,
        id: SessionId,
        lease: &LeaseToken,
        kind: CameraControlKind,
        enabled: bool,
    ) -> HalResult<()> {
        self.request(id, lease, "camera.control.auto", |reply| {
            CameraCommand::SetAuto {
                kind,
                enabled,
                reply,
            }
        })
        .await
    }
    pub(crate) async fn close(&self, id: SessionId, lease: &LeaseToken) -> HalResult<()> {
        let (worker, resource) = self.worker(&id, lease, "camera.close").await?;
        worker.request_close().await;
        self.reap_when_finished(id.clone(), worker.clone());
        match tokio::time::timeout(self.close_timeout, worker.wait_closed()).await {
            Ok(result) => {
                let result = result.map_err(|error| error.with_resource_id(resource.clone()));
                finish_session(&self.state, &id).await;
                result
            }
            Err(_) => {
                Err(close_timeout("camera.close", self.close_timeout).with_resource_id(resource))
            }
        }
    }
    pub(crate) async fn revoke_owner(&self, owner: &OwnerId) -> HalResult<()> {
        let workers = {
            let state = self.state.lock().await;
            state
                .sessions
                .iter()
                .filter(|(_, entry)| &entry.owner == owner)
                .map(|(id, entry)| (id.clone(), entry.worker.clone(), entry.resource.clone()))
                .collect::<Vec<_>>()
        };
        for (_, worker, _) in &workers {
            worker.request_close().await;
        }
        for (id, worker, _) in &workers {
            self.reap_when_finished(id.clone(), worker.clone());
        }
        let mut first_error = None;
        for (id, worker, resource) in workers {
            let result = match tokio::time::timeout(self.close_timeout, worker.wait_closed()).await
            {
                Ok(result) => result.map_err(|error| error.with_resource_id(resource.clone())),
                Err(_) => Err(close_timeout("camera.revoke_owner", self.close_timeout)
                    .with_resource_id(resource)),
            };
            if result.is_ok() {
                finish_session(&self.state, &id).await;
            }
            if first_error.is_none() {
                first_error = result.err();
            }
        }
        first_error.map_or(Ok(()), Err)
    }
}

async fn finish_session(state: &Mutex<State>, id: &SessionId) {
    let mut state = state.lock().await;
    if let Some(entry) = state.sessions.remove(id) {
        state.leases.release(&entry.resource, id);
        state.closed.insert(
            id.clone(),
            ClosedEntry {
                resource: entry.resource.clone(),
                lease: entry.lease,
                terminal_error: entry.worker.terminal_error(),
            },
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn request_replays_published_terminal_error_when_active_worker_channel_is_closed() {
        let manager = Arc::new(CameraManager::new(None, Duration::from_secs(1)));
        let resource = ResourceId::parse("camera:runtime:terminal-race").unwrap();
        let owner = OwnerId::parse("owner:camera-terminal-race").unwrap();
        let id = SessionId::parse("11111111-1111-1111-1111-111111111111").unwrap();
        let (commands, mut receiver) = mpsc::channel::<CameraCommand>(1);
        let (stop_worker, stop_worker_rx) = oneshot::channel();
        let worker_task = tokio::spawn(async move {
            tokio::select! {
                command = receiver.recv() => {
                    command.expect("the old behavior enqueues a command").reject_closed();
                    true
                }
                _ = stop_worker_rx => false,
            }
        });
        let (shutdown, _) = watch::channel(false);
        let (_, completion) = watch::channel(None);
        let (_, terminal_error) = watch::channel(Some(
            HalError::new(
                "camera.session.unplugged",
                ErrorCategory::Unavailable,
                "camera.capture",
                false,
                "camera was unplugged",
            )
            .unwrap(),
        ));
        let worker = CameraWorker {
            commands,
            shutdown,
            admission: Arc::new(Mutex::new(())),
            completion,
            terminal_error,
        };
        let lease = {
            let mut state = manager.state.lock().await;
            let lease = state
                .leases
                .reserve_control(resource.clone(), id.clone(), owner.clone())
                .unwrap();
            assert!(state.leases.commit(&resource, &id, &lease));
            state.sessions.insert(
                id.clone(),
                Entry {
                    resource: resource.clone(),
                    owner,
                    lease: lease.clone(),
                    worker,
                },
            );
            lease
        };

        let error = manager
            .capture(id, &lease, Duration::ZERO)
            .await
            .expect_err("a closed worker channel must replay its published terminal error");

        assert_eq!(error.name().as_str(), "camera.session.unplugged");
        assert_eq!(error.resource_id(), Some(&resource));
        stop_worker
            .send(())
            .expect("terminal replay must leave the live worker receiver idle");
        assert!(
            !worker_task.await.expect("test worker task must not panic"),
            "terminal replay must not enqueue a command for cleanup to reject as closed"
        );
        assert_eq!(
            manager.state.lock().await.sessions.len(),
            1,
            "the request must exercise the active-session pre-reap window"
        );
    }

    #[tokio::test]
    async fn request_must_not_enqueue_after_terminal_error_is_published() {
        let manager = Arc::new(CameraManager::new(None, Duration::from_secs(1)));
        let resource = ResourceId::parse("camera:runtime:terminal-admission-race").unwrap();
        let owner = OwnerId::parse("owner:camera-terminal-admission-race").unwrap();
        let id = SessionId::parse("22222222-2222-2222-2222-222222222222").unwrap();
        let (commands, mut receiver) = mpsc::channel::<CameraCommand>(1);
        let (shutdown, _) = watch::channel(false);
        let (_, completion) = watch::channel(None);
        let (terminal_error_tx, terminal_error) = watch::channel(None);
        let admission = Arc::new(Mutex::new(()));
        let worker = CameraWorker {
            commands,
            shutdown,
            admission: Arc::clone(&admission),
            completion,
            terminal_error,
        };
        let lease = {
            let mut state = manager.state.lock().await;
            let lease = state
                .leases
                .reserve_control(resource.clone(), id.clone(), owner.clone())
                .unwrap();
            assert!(state.leases.commit(&resource, &id, &lease));
            state.sessions.insert(
                id.clone(),
                Entry {
                    resource: resource.clone(),
                    owner,
                    lease: lease.clone(),
                    worker,
                },
            );
            lease
        };
        let terminal_turn = admission.lock().await;
        let capture = tokio::spawn({
            let manager = Arc::clone(&manager);
            let id = id.clone();
            let lease = lease.clone();
            async move { manager.capture(id, &lease, Duration::ZERO).await }
        });
        terminal_error_tx
            .send(Some(
                HalError::new(
                    "camera.session.unplugged",
                    ErrorCategory::Unavailable,
                    "camera.capture",
                    false,
                    "camera was unplugged",
                )
                .unwrap(),
            ))
            .unwrap();
        drop(terminal_turn);

        let error = capture
            .await
            .expect("capture task must not panic")
            .expect_err(
                "a post-terminal request must fail without reaching the cleanup command receiver",
            );
        assert_eq!(error.name().as_str(), "camera.session.unplugged");
        assert!(
            receiver.try_recv().is_err(),
            "terminal publication wins admission and must leave the command receiver idle"
        );
    }
}
