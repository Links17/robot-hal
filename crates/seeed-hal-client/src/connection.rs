use std::collections::{HashMap, HashSet, VecDeque};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use bytes::{Bytes, BytesMut};
use futures_util::{SinkExt, StreamExt};
use prost::Message;
use seeed_hal_core::{ErrorCategory, HalError, HalResult, ResourceDescriptor, ResourceSelector};
use seeed_hal_protocol::v1::{self, envelope};
use seeed_hal_protocol::{MAX_FRAME_BYTES, PROTOCOL_MAJOR, PROTOCOL_MINOR, SERIAL_CAPABILITY};
use seeed_hal_serial::SerialConfig;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::{broadcast, mpsc, oneshot, watch};
use tokio::task::JoinHandle;
use tokio_util::codec::{Framed, LengthDelimitedCodec};

use crate::RemoteSerialHandle;

const DEFAULT_IO_CAPACITY: usize = 32;
const DEFAULT_EVENT_CAPACITY: usize = 64;
const DEFAULT_TRANSFER_BYTES: usize = 64 * 1024;
const TASK_SHUTDOWN_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(100);

/// Local broker connection settings. The startup token is intentionally not
/// printable and this type does not implement `Debug`.
pub struct ConnectionOptions {
    endpoint: PathBuf,
    startup_token: [u8; 32],
    max_frame_bytes: usize,
    max_read_bytes: usize,
    max_write_bytes: usize,
    pending_capacity: usize,
    writer_capacity: usize,
    event_capacity: usize,
}

impl ConnectionOptions {
    pub fn new(endpoint: impl Into<PathBuf>, startup_token: [u8; 32]) -> Self {
        Self {
            endpoint: endpoint.into(),
            startup_token,
            max_frame_bytes: MAX_FRAME_BYTES,
            max_read_bytes: DEFAULT_TRANSFER_BYTES,
            max_write_bytes: DEFAULT_TRANSFER_BYTES,
            pending_capacity: DEFAULT_IO_CAPACITY,
            writer_capacity: DEFAULT_IO_CAPACITY,
            event_capacity: DEFAULT_EVENT_CAPACITY,
        }
    }

    pub fn with_byte_limits(
        mut self,
        max_frame_bytes: usize,
        max_read_bytes: usize,
        max_write_bytes: usize,
    ) -> Self {
        self.max_frame_bytes = max_frame_bytes;
        self.max_read_bytes = max_read_bytes;
        self.max_write_bytes = max_write_bytes;
        self
    }

    pub fn with_queue_capacities(
        mut self,
        pending_capacity: usize,
        writer_capacity: usize,
        event_capacity: usize,
    ) -> Self {
        self.pending_capacity = pending_capacity.max(1);
        self.writer_capacity = writer_capacity.max(1);
        self.event_capacity = event_capacity.max(1);
        self
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ClientEvent {
    sequence: u64,
    name: String,
    resource_id: String,
    session_id: String,
    lease_generation: u64,
}

impl ClientEvent {
    pub fn sequence(&self) -> u64 {
        self.sequence
    }
    pub fn name(&self) -> &str {
        &self.name
    }
    pub fn resource_id(&self) -> &str {
        &self.resource_id
    }
    pub fn session_id(&self) -> &str {
        &self.session_id
    }
    pub fn lease_generation(&self) -> u64 {
        self.lease_generation
    }
}

pub struct EventSubscription {
    receiver: broadcast::Receiver<HalResult<ClientEvent>>,
}

impl EventSubscription {
    pub async fn recv(&mut self) -> HalResult<ClientEvent> {
        match self.receiver.recv().await {
            Ok(result) => result,
            Err(broadcast::error::RecvError::Closed) => Err(client_error(
                "runtime.event.closed",
                ErrorCategory::Unavailable,
                "runtime.event.receive",
                false,
                "the client event stream is closed",
            )),
            Err(broadcast::error::RecvError::Lagged(skipped)) => Err(client_error(
                "runtime.event.lagged",
                ErrorCategory::Unavailable,
                "runtime.event.receive",
                true,
                format!("event subscriber fell behind by {skipped} events"),
            )),
        }
    }
}

#[derive(Clone, Copy, Eq, PartialEq)]
pub(crate) enum ExpectedResponse {
    Handshake,
    EnumerateSerial,
    OpenSerial,
    SerialRead,
    SerialWrite,
    SerialFlush,
    SetControlLines,
    CloseSession,
}

struct PendingRequest {
    expected: ExpectedResponse,
    reply: oneshot::Sender<HalResult<envelope::Payload>>,
}

struct RequestState {
    next_request_id: u64,
    pending: HashMap<u64, PendingRequest>,
    cancelled: HashSet<u64>,
    completed: HashSet<u64>,
    completed_order: VecDeque<u64>,
    terminal: Option<HalError>,
}

struct Outbound {
    envelope: v1::Envelope,
    frame_limit: usize,
}

#[derive(Clone, Copy)]
struct Limits {
    frame: usize,
    read: usize,
    write: usize,
}

struct Shared {
    requests: Mutex<RequestState>,
    limits: Mutex<Limits>,
    pending_capacity: usize,
    tombstone_capacity: usize,
    writer: mpsc::Sender<Outbound>,
    events: broadcast::Sender<HalResult<ClientEvent>>,
    shutdown: watch::Sender<bool>,
}

struct ClientTasks {
    writer: JoinHandle<()>,
    reader: JoinHandle<()>,
}

struct ClientInner {
    shared: Arc<Shared>,
    tasks: Mutex<Option<ClientTasks>>,
}

impl Drop for ClientInner {
    fn drop(&mut self) {
        terminate(&self.shared, closed_error());
        if let Some(tasks) = self.tasks.lock().unwrap_or_else(|p| p.into_inner()).take() {
            tasks.writer.abort();
            tasks.reader.abort();
        }
    }
}

#[derive(Clone)]
pub struct HalClient {
    inner: Arc<ClientInner>,
}

impl HalClient {
    pub async fn connect(options: ConnectionOptions) -> HalResult<Self> {
        validate_options(&options)?;
        #[cfg(unix)]
        let io = tokio::net::UnixStream::connect(&options.endpoint)
            .await
            .map_err(|error| disconnected_error("runtime.broker.connect", error.to_string()))?;
        #[cfg(windows)]
        let io = {
            use tokio::net::windows::named_pipe::ClientOptions;
            let endpoint = options.endpoint.to_str().ok_or_else(|| {
                client_error(
                    "runtime.argument.invalid",
                    ErrorCategory::InvalidArgument,
                    "runtime.broker.connect",
                    false,
                    "named pipe endpoint is not valid UTF-8",
                )
            })?;
            ClientOptions::new()
                .open(endpoint)
                .map_err(|error| disconnected_error("runtime.broker.connect", error.to_string()))?
        };
        Self::from_io(io, options).await
    }

    async fn from_io<T>(io: T, options: ConnectionOptions) -> HalResult<Self>
    where
        T: AsyncRead + AsyncWrite + Unpin + Send + 'static,
    {
        let framed = Framed::new(io, frame_codec());
        let (sink, stream) = framed.split();
        let (writer_tx, writer_rx) = mpsc::channel(options.writer_capacity);
        let (event_tx, _) = broadcast::channel(options.event_capacity);
        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        let shared = Arc::new(Shared {
            requests: Mutex::new(RequestState {
                next_request_id: 1,
                pending: HashMap::with_capacity(options.pending_capacity),
                cancelled: HashSet::with_capacity(options.pending_capacity),
                completed: HashSet::with_capacity(options.pending_capacity),
                completed_order: VecDeque::with_capacity(options.pending_capacity),
                terminal: None,
            }),
            limits: Mutex::new(Limits {
                frame: options.max_frame_bytes,
                read: options.max_read_bytes,
                write: options.max_write_bytes,
            }),
            pending_capacity: options.pending_capacity,
            tombstone_capacity: options.pending_capacity,
            writer: writer_tx,
            events: event_tx,
            shutdown: shutdown_tx,
        });
        let writer_shared = shared.clone();
        let writer = tokio::spawn(writer_task(
            sink,
            writer_rx,
            shutdown_rx.clone(),
            writer_shared,
        ));
        let reader_shared = shared.clone();
        let reader = tokio::spawn(reader_task(stream, shutdown_rx, reader_shared));
        let client = Self {
            inner: Arc::new(ClientInner {
                shared,
                tasks: Mutex::new(Some(ClientTasks { writer, reader })),
            }),
        };

        let requested = Limits {
            frame: options.max_frame_bytes,
            read: options.max_read_bytes,
            write: options.max_write_bytes,
        };
        let response = client
            .request(
                envelope::Payload::HandshakeRequest(v1::HandshakeRequest {
                    startup_token: options.startup_token.to_vec(),
                    protocol_major: PROTOCOL_MAJOR,
                    protocol_minor: PROTOCOL_MINOR,
                    required_capabilities: vec![SERIAL_CAPABILITY.to_owned()],
                    max_frame_bytes: requested.frame as u32,
                    max_read_bytes: requested.read as u32,
                    max_write_bytes: requested.write as u32,
                }),
                ExpectedResponse::Handshake,
            )
            .await?;
        let envelope::Payload::HandshakeResponse(response) = response else {
            unreachable!()
        };
        validate_handshake_response(&response, requested)?;
        *client
            .inner
            .shared
            .limits
            .lock()
            .unwrap_or_else(|p| p.into_inner()) = Limits {
            frame: response.max_frame_bytes as usize,
            read: response.max_read_bytes as usize,
            write: response.max_write_bytes as usize,
        };
        Ok(client)
    }

    pub async fn enumerate_serial(&self) -> HalResult<Vec<ResourceDescriptor>> {
        let payload = self
            .request(
                envelope::Payload::EnumerateSerialRequest(v1::EnumerateSerialRequest {}),
                ExpectedResponse::EnumerateSerial,
            )
            .await?;
        let envelope::Payload::EnumerateSerialResponse(response) = payload else {
            unreachable!()
        };
        let result: HalResult<Vec<ResourceDescriptor>> = response
            .resources
            .into_iter()
            .map(TryInto::try_into)
            .collect();
        if let Err(error) = &result {
            terminate(&self.inner.shared, error.clone());
        }
        result
    }

    pub async fn open_serial(
        &self,
        selector: ResourceSelector,
        config: SerialConfig,
    ) -> HalResult<RemoteSerialHandle> {
        let payload = self
            .request(
                envelope::Payload::OpenSerialRequest(v1::OpenSerialRequest {
                    selector: Some((&selector).into()),
                    config: Some((&config).into()),
                }),
                ExpectedResponse::OpenSerial,
            )
            .await?;
        let envelope::Payload::OpenSerialResponse(response) = payload else {
            unreachable!()
        };
        let result = RemoteSerialHandle::from_response(self.clone(), response);
        if let Err(error) = &result {
            terminate(&self.inner.shared, error.clone());
        }
        result
    }

    pub fn subscribe(&self) -> EventSubscription {
        EventSubscription {
            receiver: self.inner.shared.events.subscribe(),
        }
    }

    pub async fn close(self) -> HalResult<()> {
        terminate(&self.inner.shared, closed_error());
        let tasks = self
            .inner
            .tasks
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .take();
        if let Some(tasks) = tasks {
            finish_task(tasks.writer).await;
            finish_task(tasks.reader).await;
        }
        Ok(())
    }

    pub(crate) fn limits(&self) -> (usize, usize, usize) {
        let limits = *self
            .inner
            .shared
            .limits
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        (limits.frame, limits.read, limits.write)
    }

    pub(crate) async fn send(
        &self,
        payload: envelope::Payload,
        expected: ExpectedResponse,
    ) -> HalResult<envelope::Payload> {
        self.request(payload, expected).await
    }

    pub(crate) fn fail(&self, error: HalError) {
        terminate(&self.inner.shared, error);
    }

    async fn request(
        &self,
        payload: envelope::Payload,
        expected: ExpectedResponse,
    ) -> HalResult<envelope::Payload> {
        let (reply_tx, reply_rx) = oneshot::channel();
        let (request_id, frame_limit) = {
            let mut state = self
                .inner
                .shared
                .requests
                .lock()
                .unwrap_or_else(|p| p.into_inner());
            if let Some(error) = &state.terminal {
                return Err(error.clone());
            }
            if state.pending.len() >= self.inner.shared.pending_capacity {
                return Err(client_error(
                    "runtime.queue.full",
                    ErrorCategory::Unavailable,
                    "runtime.client.request",
                    true,
                    "client pending request storage is full",
                ));
            }
            let request_id = state.next_request_id;
            if request_id == 0 {
                return Err(client_error(
                    "runtime.protocol.request_id_exhausted",
                    ErrorCategory::Internal,
                    "runtime.client.request",
                    false,
                    "request ID space is exhausted",
                ));
            }
            state.next_request_id = request_id.checked_add(1).unwrap_or(0);
            state.pending.insert(
                request_id,
                PendingRequest {
                    expected,
                    reply: reply_tx,
                },
            );
            let frame = self
                .inner
                .shared
                .limits
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .frame;
            (request_id, frame)
        };
        let envelope = v1::Envelope {
            request_id,
            payload: Some(payload),
        };
        let encoded_len = envelope.encoded_len();
        if encoded_len > frame_limit || encoded_len > MAX_FRAME_BYTES {
            remove_pending(&self.inner.shared, request_id);
            return Err(frame_too_large(
                "outbound envelope exceeds the active frame limit",
            ));
        }
        if let Err(error) = self.inner.shared.writer.try_send(Outbound {
            envelope,
            frame_limit,
        }) {
            remove_pending(&self.inner.shared, request_id);
            return Err(match error {
                mpsc::error::TrySendError::Full(_) => client_error(
                    "runtime.queue.full",
                    ErrorCategory::Unavailable,
                    "runtime.protocol.write",
                    true,
                    "client writer queue is full",
                ),
                mpsc::error::TrySendError::Closed(_) => disconnected_error(
                    "runtime.protocol.write",
                    "client writer task is not available",
                ),
            });
        }
        let mut guard = CancellationGuard {
            shared: self.inner.shared.clone(),
            request_id,
            armed: true,
        };
        let result = reply_rx.await.map_err(|_| {
            disconnected_error("runtime.client.request", "request reply channel closed")
        })?;
        guard.armed = false;
        result
    }
}

struct CancellationGuard {
    shared: Arc<Shared>,
    request_id: u64,
    armed: bool,
}

impl Drop for CancellationGuard {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        let mut state = self
            .shared
            .requests
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        if state.pending.remove(&self.request_id).is_some() {
            if state.cancelled.len() >= self.shared.tombstone_capacity {
                drop(state);
                terminate(
                    &self.shared,
                    client_error(
                        "runtime.queue.cancelled_full",
                        ErrorCategory::Unavailable,
                        "runtime.client.cancel",
                        false,
                        "cancelled request tracking is full",
                    ),
                );
            } else {
                state.cancelled.insert(self.request_id);
            }
        }
    }
}

async fn writer_task<S>(
    mut sink: S,
    mut requests: mpsc::Receiver<Outbound>,
    mut shutdown: watch::Receiver<bool>,
    shared: Arc<Shared>,
) where
    S: futures_util::Sink<Bytes, Error = std::io::Error> + Unpin,
{
    loop {
        let outbound = tokio::select! {
            changed = shutdown.changed() => {
                if changed.is_err() || *shutdown.borrow() { break; }
                continue;
            }
            outbound = requests.recv() => match outbound { Some(value) => value, None => break },
        };
        let encoded_len = outbound.envelope.encoded_len();
        if encoded_len > outbound.frame_limit || encoded_len > MAX_FRAME_BYTES {
            terminate(
                &shared,
                frame_too_large("writer rejected an oversized envelope"),
            );
            break;
        }
        let mut encoded = BytesMut::with_capacity(encoded_len);
        if let Err(error) = outbound.envelope.encode(&mut encoded) {
            terminate(
                &shared,
                client_error(
                    "runtime.protocol.encode_failed",
                    ErrorCategory::Internal,
                    "runtime.protocol.write",
                    false,
                    error.to_string(),
                ),
            );
            break;
        }
        let send = sink.send(encoded.freeze());
        tokio::pin!(send);
        tokio::select! {
            changed = shutdown.changed() => {
                if changed.is_err() || *shutdown.borrow() { break; }
            }
            result = &mut send => {
                if let Err(error) = result {
                    terminate(
                        &shared,
                        disconnected_error("runtime.protocol.write", error.to_string()),
                    );
                    break;
                }
            }
        }
    }
}

async fn reader_task<R>(mut stream: R, mut shutdown: watch::Receiver<bool>, shared: Arc<Shared>)
where
    R: futures_util::Stream<Item = Result<BytesMut, std::io::Error>> + Unpin,
{
    loop {
        let frame = tokio::select! {
            changed = shutdown.changed() => {
                if changed.is_err() || *shutdown.borrow() { return; }
                continue;
            }
            frame = stream.next() => frame,
        };
        let Some(frame) = frame else {
            terminate(
                &shared,
                disconnected_error("runtime.protocol.read", "broker closed the connection"),
            );
            return;
        };
        let frame = match frame {
            Ok(frame) => frame,
            Err(error) => {
                terminate(
                    &shared,
                    disconnected_error("runtime.protocol.read", error.to_string()),
                );
                return;
            }
        };
        let limit = shared
            .limits
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .frame;
        if frame.len() > limit || frame.len() > MAX_FRAME_BYTES {
            terminate(
                &shared,
                frame_too_large("inbound frame exceeds the active frame limit"),
            );
            return;
        }
        let envelope = match v1::Envelope::decode(frame) {
            Ok(envelope) => envelope,
            Err(error) => {
                terminate(
                    &shared,
                    client_error(
                        "runtime.protocol.invalid_message",
                        ErrorCategory::InvalidArgument,
                        "runtime.protocol.decode",
                        false,
                        error.to_string(),
                    ),
                );
                return;
            }
        };
        if envelope.request_id == 0 {
            match envelope.payload {
                Some(envelope::Payload::RuntimeEvent(event)) => {
                    if event.sequence == 0
                        || matches!(
                            v1::RuntimeEventKind::try_from(event.kind),
                            Err(_) | Ok(v1::RuntimeEventKind::Unspecified)
                        )
                    {
                        terminate(
                            &shared,
                            client_error(
                                "runtime.protocol.invalid_message",
                                ErrorCategory::InvalidArgument,
                                "runtime.protocol.decode",
                                false,
                                "runtime event metadata is invalid",
                            ),
                        );
                        return;
                    }
                    let event = ClientEvent {
                        sequence: event.sequence,
                        name: event.name,
                        resource_id: event.resource_id,
                        session_id: event.session_id,
                        lease_generation: event.lease_generation,
                    };
                    let _ = shared.events.send(Ok(event));
                }
                Some(envelope::Payload::Error(error)) => match decode_error(error) {
                    Ok(error) => {
                        let _ = shared.events.send(Err(error));
                    }
                    Err(error) => {
                        terminate(&shared, error);
                        return;
                    }
                },
                _ => {
                    terminate(
                        &shared,
                        client_error(
                            "runtime.protocol.invalid_message",
                            ErrorCategory::InvalidArgument,
                            "runtime.protocol.read",
                            false,
                            "request ID zero is reserved for events",
                        ),
                    );
                    return;
                }
            }
            continue;
        }

        let pending = {
            let mut state = shared.requests.lock().unwrap_or_else(|p| p.into_inner());
            if let Some(pending) = state.pending.remove(&envelope.request_id) {
                remember_completed(&mut state, envelope.request_id, shared.tombstone_capacity);
                Some(pending)
            } else if state.cancelled.remove(&envelope.request_id) {
                None
            } else {
                let duplicate = state.completed.contains(&envelope.request_id);
                drop(state);
                terminate(
                    &shared,
                    client_error(
                        if duplicate {
                            "runtime.protocol.duplicate_response"
                        } else {
                            "runtime.protocol.unknown_response"
                        },
                        ErrorCategory::Conflict,
                        "runtime.protocol.read",
                        false,
                        if duplicate {
                            "broker sent a duplicate response"
                        } else {
                            "broker sent an unknown response ID"
                        },
                    ),
                );
                return;
            }
        };
        let Some(pending) = pending else {
            continue;
        };
        let result = match envelope.payload {
            Some(envelope::Payload::Error(error)) => decode_error(error).and_then(Err),
            Some(payload) if response_matches(pending.expected, &payload) => Ok(payload),
            _ => Err(client_error(
                "runtime.protocol.unexpected_response",
                ErrorCategory::InvalidArgument,
                "runtime.protocol.read",
                false,
                "response payload does not match its request",
            )),
        };
        let terminal = result
            .as_ref()
            .err()
            .filter(|error| {
                matches!(
                    error.name().as_str(),
                    "runtime.protocol.unexpected_response" | "runtime.protocol.invalid_message"
                )
            })
            .cloned();
        let _ = pending.reply.send(result);
        if let Some(error) = terminal {
            terminate(&shared, error);
            return;
        }
    }
}

fn response_matches(expected: ExpectedResponse, payload: &envelope::Payload) -> bool {
    matches!(
        (expected, payload),
        (
            ExpectedResponse::Handshake,
            envelope::Payload::HandshakeResponse(_)
        ) | (
            ExpectedResponse::EnumerateSerial,
            envelope::Payload::EnumerateSerialResponse(_)
        ) | (
            ExpectedResponse::OpenSerial,
            envelope::Payload::OpenSerialResponse(_)
        ) | (
            ExpectedResponse::SerialRead,
            envelope::Payload::SerialReadResponse(_)
        ) | (
            ExpectedResponse::SerialWrite,
            envelope::Payload::SerialWriteResponse(_)
        ) | (
            ExpectedResponse::SerialFlush,
            envelope::Payload::SerialFlushResponse(_)
        ) | (
            ExpectedResponse::SetControlLines,
            envelope::Payload::SetSerialControlLinesResponse(_)
        ) | (
            ExpectedResponse::CloseSession,
            envelope::Payload::CloseSessionResponse(_)
        )
    )
}

fn remember_completed(state: &mut RequestState, request_id: u64, capacity: usize) {
    if state.completed.insert(request_id) {
        state.completed_order.push_back(request_id);
    }
    while state.completed_order.len() > capacity {
        if let Some(oldest) = state.completed_order.pop_front() {
            state.completed.remove(&oldest);
        }
    }
}

fn remove_pending(shared: &Shared, request_id: u64) {
    shared
        .requests
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .pending
        .remove(&request_id);
}

fn terminate(shared: &Shared, error: HalError) {
    let replies = {
        let mut state = shared.requests.lock().unwrap_or_else(|p| p.into_inner());
        if state.terminal.is_some() {
            return;
        }
        state.terminal = Some(error.clone());
        state
            .pending
            .drain()
            .map(|(_, pending)| pending.reply)
            .collect::<Vec<_>>()
    };
    for reply in replies {
        let _ = reply.send(Err(error.clone()));
    }
    let _ = shared.shutdown.send(true);
}

async fn finish_task(mut task: JoinHandle<()>) {
    if tokio::time::timeout(TASK_SHUTDOWN_TIMEOUT, &mut task)
        .await
        .is_err()
    {
        task.abort();
        let _ = task.await;
    }
}

fn validate_options(options: &ConnectionOptions) -> HalResult<()> {
    if options.max_frame_bytes == 0
        || options.max_frame_bytes > MAX_FRAME_BYTES
        || options.max_read_bytes == 0
        || options.max_write_bytes == 0
        || options.max_frame_bytes > u32::MAX as usize
        || options.max_read_bytes > u32::MAX as usize
        || options.max_write_bytes > u32::MAX as usize
    {
        return Err(client_error(
            "runtime.argument.invalid",
            ErrorCategory::InvalidArgument,
            "runtime.broker.connect",
            false,
            "connection byte limits are invalid",
        ));
    }
    Ok(())
}

fn validate_handshake_response(
    response: &v1::HandshakeResponse,
    requested: Limits,
) -> HalResult<()> {
    if response.protocol_major != PROTOCOL_MAJOR
        || response.protocol_minor != PROTOCOL_MINOR
        || response.max_frame_bytes == 0
        || response.max_frame_bytes as usize > requested.frame
        || response.max_read_bytes == 0
        || response.max_read_bytes as usize > requested.read
        || response.max_write_bytes == 0
        || response.max_write_bytes as usize > requested.write
        || !response
            .capabilities
            .iter()
            .any(|value| value == SERIAL_CAPABILITY)
    {
        return Err(client_error(
            "runtime.protocol.invalid_handshake",
            ErrorCategory::Conflict,
            "runtime.protocol.handshake",
            false,
            "broker returned invalid negotiated settings",
        ));
    }
    Ok(())
}

fn decode_error(error: v1::Error) -> HalResult<HalError> {
    let category = match v1::ErrorCategory::try_from(error.category) {
        Ok(v1::ErrorCategory::InvalidArgument) => ErrorCategory::InvalidArgument,
        Ok(v1::ErrorCategory::NotFound) => ErrorCategory::NotFound,
        Ok(v1::ErrorCategory::Conflict) => ErrorCategory::Conflict,
        Ok(v1::ErrorCategory::Unavailable) => ErrorCategory::Unavailable,
        Ok(v1::ErrorCategory::Internal) => ErrorCategory::Internal,
        _ => {
            return Err(client_error(
                "runtime.protocol.invalid_message",
                ErrorCategory::InvalidArgument,
                "runtime.protocol.decode",
                false,
                "broker error has an unknown category",
            ));
        }
    };
    HalError::new(
        error.name,
        category,
        error.operation,
        error.retryable,
        error.debug_message,
    )
    .map_err(|_| {
        client_error(
            "runtime.protocol.invalid_message",
            ErrorCategory::InvalidArgument,
            "runtime.protocol.decode",
            false,
            "broker error metadata is invalid",
        )
    })
}

fn frame_codec() -> LengthDelimitedCodec {
    LengthDelimitedCodec::builder()
        .max_frame_length(MAX_FRAME_BYTES)
        .new_codec()
}

fn client_error(
    name: &'static str,
    category: ErrorCategory,
    operation: &'static str,
    retryable: bool,
    message: impl Into<String>,
) -> HalError {
    HalError::new(name, category, operation, retryable, message)
        .expect("static client error metadata is valid")
}

fn disconnected_error(operation: &'static str, message: impl Into<String>) -> HalError {
    client_error(
        "runtime.broker.disconnected",
        ErrorCategory::Unavailable,
        operation,
        true,
        message,
    )
}

fn closed_error() -> HalError {
    client_error(
        "runtime.client.closed",
        ErrorCategory::Conflict,
        "runtime.client.close",
        false,
        "client is closed",
    )
}

fn frame_too_large(message: &'static str) -> HalError {
    client_error(
        "runtime.protocol.frame_too_large",
        ErrorCategory::InvalidArgument,
        "runtime.protocol.frame",
        false,
        message,
    )
}
