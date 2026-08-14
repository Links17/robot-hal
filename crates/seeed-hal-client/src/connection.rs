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
use zeroize::{Zeroize, ZeroizeOnDrop};

use crate::RemoteSerialHandle;

const DEFAULT_IO_CAPACITY: usize = 32;
const DEFAULT_EVENT_CAPACITY: usize = 64;
const DEFAULT_TRANSFER_BYTES: usize = 64 * 1024;
const TASK_SHUTDOWN_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(100);

/// Local broker connection settings. The startup token is intentionally not
/// printable and this type does not implement `Debug`.
pub struct ConnectionOptions {
    endpoint: PathBuf,
    startup_token: SecretToken,
    max_frame_bytes: usize,
    max_read_bytes: usize,
    max_write_bytes: usize,
    pending_capacity: usize,
    writer_capacity: usize,
    event_capacity: usize,
}

#[derive(Zeroize, ZeroizeOnDrop)]
struct SecretToken([u8; 32]);

impl SecretToken {
    fn new(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    fn expose(&self) -> &[u8; 32] {
        &self.0
    }
}

impl ConnectionOptions {
    pub fn new(endpoint: impl Into<PathBuf>, startup_token: [u8; 32]) -> Self {
        Self {
            endpoint: endpoint.into(),
            startup_token: SecretToken::new(startup_token),
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
    EnumerateSerial,
    OpenSerial,
    SerialRead { max_bytes: usize },
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
    cancelled: HashMap<u64, ExpectedResponse>,
    completed: HashSet<u64>,
    completed_order: VecDeque<u64>,
    terminal: Option<HalError>,
}

impl RequestState {
    fn take_request_id(&mut self) -> HalResult<u64> {
        let request_id = self.next_request_id;
        if request_id == 0 {
            return Err(client_error(
                "runtime.protocol.request_id_exhausted",
                ErrorCategory::Internal,
                "runtime.client.request",
                false,
                "request ID space is exhausted",
            ));
        }
        self.next_request_id = request_id.checked_add(1).unwrap_or(0);
        Ok(request_id)
    }
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
        let requested = Limits {
            frame: options.max_frame_bytes,
            read: options.max_read_bytes,
            write: options.max_write_bytes,
        };
        let mut framed = Framed::new(io, frame_codec(requested.frame));
        let negotiated = perform_handshake(&mut framed, &options, requested).await?;
        framed.codec_mut().set_max_frame_length(negotiated.frame);
        let (sink, stream) = framed.split();
        let (writer_tx, writer_rx) = mpsc::channel(options.writer_capacity);
        let (event_tx, _) = broadcast::channel(options.event_capacity);
        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        let shared = Arc::new(Shared {
            requests: Mutex::new(RequestState {
                next_request_id: 2,
                pending: HashMap::with_capacity(options.pending_capacity),
                cancelled: HashMap::with_capacity(options.pending_capacity),
                completed: HashSet::from([1]),
                completed_order: VecDeque::from([1]),
                terminal: None,
            }),
            limits: Mutex::new(negotiated),
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
        Ok(Self {
            inner: Arc::new(ClientInner {
                shared,
                tasks: Mutex::new(Some(ClientTasks { writer, reader })),
            }),
        })
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
            let request_id = state.take_request_id()?;
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
        if let Some(pending) = state.pending.remove(&self.request_id) {
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
                state.cancelled.insert(self.request_id, pending.expected);
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
        let mut outbound = tokio::select! {
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
            zeroize_handshake(&mut outbound.envelope);
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
        let contains_secret = zeroize_handshake(&mut outbound.envelope);
        let wire = Bytes::copy_from_slice(&encoded);
        if contains_secret {
            encoded.as_mut().zeroize();
        }
        let send = sink.send(wire);
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

fn zeroize_handshake(envelope: &mut v1::Envelope) -> bool {
    if let Some(envelope::Payload::HandshakeRequest(request)) = envelope.payload.as_mut() {
        request.startup_token.zeroize();
        true
    } else {
        false
    }
}

async fn perform_handshake<T>(
    framed: &mut Framed<T, LengthDelimitedCodec>,
    options: &ConnectionOptions,
    requested: Limits,
) -> HalResult<Limits>
where
    T: AsyncRead + AsyncWrite + Unpin,
{
    let mut envelope = v1::Envelope {
        request_id: 1,
        payload: Some(envelope::Payload::HandshakeRequest(v1::HandshakeRequest {
            startup_token: options.startup_token.expose().to_vec(),
            protocol_major: PROTOCOL_MAJOR,
            protocol_minor: PROTOCOL_MINOR,
            required_capabilities: vec![SERIAL_CAPABILITY.to_owned()],
            max_frame_bytes: requested.frame as u32,
            max_read_bytes: requested.read as u32,
            max_write_bytes: requested.write as u32,
        })),
    };
    let encoded_len = envelope.encoded_len();
    if encoded_len > requested.frame || encoded_len > MAX_FRAME_BYTES {
        zeroize_handshake(&mut envelope);
        return Err(frame_too_large(
            "handshake envelope exceeds the offered frame limit",
        ));
    }
    let mut encoded = BytesMut::with_capacity(encoded_len);
    if let Err(error) = envelope.encode(&mut encoded) {
        zeroize_handshake(&mut envelope);
        return Err(client_error(
            "runtime.protocol.encode_failed",
            ErrorCategory::Internal,
            "runtime.protocol.handshake",
            false,
            error.to_string(),
        ));
    }
    zeroize_handshake(&mut envelope);
    let wire = Bytes::copy_from_slice(&encoded);
    encoded.as_mut().zeroize();
    framed
        .send(wire)
        .await
        .map_err(|error| disconnected_error("runtime.protocol.handshake", error.to_string()))?;

    let frame = framed
        .next()
        .await
        .ok_or_else(|| {
            disconnected_error(
                "runtime.protocol.handshake",
                "broker closed before handshake response",
            )
        })?
        .map_err(|error| frame_read_error("runtime.protocol.handshake", error))?;
    if frame.len() > requested.frame || frame.len() > MAX_FRAME_BYTES {
        return Err(frame_too_large(
            "handshake response exceeds the offered frame limit",
        ));
    }
    let response = v1::Envelope::decode(frame).map_err(|error| {
        client_error(
            "runtime.protocol.invalid_message",
            ErrorCategory::InvalidArgument,
            "runtime.protocol.handshake",
            false,
            error.to_string(),
        )
    })?;
    if response.request_id != 1 {
        return Err(client_error(
            "runtime.protocol.unknown_response",
            ErrorCategory::Conflict,
            "runtime.protocol.handshake",
            false,
            "handshake response has an unknown request ID",
        ));
    }
    match response.payload {
        Some(envelope::Payload::HandshakeResponse(response)) => {
            validate_handshake_response(&response, requested)?;
            Ok(Limits {
                frame: response.max_frame_bytes as usize,
                read: response.max_read_bytes as usize,
                write: response.max_write_bytes as usize,
            })
        }
        Some(envelope::Payload::Error(error)) => Err(decode_error(error)?),
        _ => Err(client_error(
            "runtime.protocol.unexpected_response",
            ErrorCategory::InvalidArgument,
            "runtime.protocol.handshake",
            false,
            "broker returned a non-handshake response during negotiation",
        )),
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
                terminate(&shared, frame_read_error("runtime.protocol.read", error));
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
        if let Err(error) = preflight_inbound(&frame, &shared) {
            terminate(&shared, error);
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
            } else if state.cancelled.remove(&envelope.request_id).is_some() {
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

fn preflight_inbound(frame: &[u8], shared: &Shared) -> HalResult<()> {
    let mut request_id = 0_u64;
    visit_fields(frame, |field, wire| {
        if let (1, WireValue::Varint(value)) = (field, wire) {
            request_id = value;
        }
        Ok(())
    })?;

    let negotiated_read = shared
        .limits
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .read;
    let requested_read = {
        let state = shared
            .requests
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        state
            .pending
            .get(&request_id)
            .map(|pending| pending.expected)
            .or_else(|| state.cancelled.get(&request_id).copied())
            .and_then(|expected| match expected {
                ExpectedResponse::SerialRead { max_bytes } => Some(max_bytes),
                _ => None,
            })
    };
    visit_fields(frame, |field, wire| {
        if let (25, WireValue::Bytes(read_response)) = (field, wire) {
            visit_fields(read_response, |field, wire| {
                if let (1, WireValue::Bytes(data)) = (field, wire) {
                    if data.len() > negotiated_read
                        || requested_read.is_some_and(|max| data.len() > max)
                    {
                        return Err(frame_too_large(
                            "serial read response exceeds the negotiated or requested byte limit",
                        ));
                    }
                }
                Ok(())
            })?;
        }
        Ok(())
    })
}

#[derive(Clone, Copy)]
enum WireValue<'a> {
    Varint(u64),
    Bytes(&'a [u8]),
    Fixed,
}

fn visit_fields<'a>(
    mut input: &'a [u8],
    mut visitor: impl FnMut(u32, WireValue<'a>) -> HalResult<()>,
) -> HalResult<()> {
    while !input.is_empty() {
        let (key, key_len) = read_varint(input)?;
        input = &input[key_len..];
        let field = u32::try_from(key >> 3).unwrap_or(u32::MAX);
        if field == 0 {
            return Err(invalid_wire("protobuf field number zero is invalid"));
        }
        match key & 0x07 {
            0 => {
                let (value, len) = read_varint(input)?;
                input = &input[len..];
                visitor(field, WireValue::Varint(value))?;
            }
            1 => {
                input = input
                    .get(8..)
                    .ok_or_else(|| invalid_wire("truncated fixed64 protobuf field"))?;
                visitor(field, WireValue::Fixed)?;
            }
            2 => {
                let (len, prefix_len) = read_varint(input)?;
                input = &input[prefix_len..];
                let len = usize::try_from(len)
                    .map_err(|_| invalid_wire("protobuf byte field length overflows usize"))?;
                let bytes = input
                    .get(..len)
                    .ok_or_else(|| invalid_wire("truncated length-delimited protobuf field"))?;
                input = &input[len..];
                visitor(field, WireValue::Bytes(bytes))?;
            }
            3 => input = skip_group(input, field, 1)?,
            4 => return Err(invalid_wire("unexpected protobuf end-group field")),
            5 => {
                input = input
                    .get(4..)
                    .ok_or_else(|| invalid_wire("truncated fixed32 protobuf field"))?;
                visitor(field, WireValue::Fixed)?;
            }
            _ => return Err(invalid_wire("unsupported protobuf wire type")),
        }
    }
    Ok(())
}

const MAX_PROTOBUF_GROUP_DEPTH: usize = 64;

fn skip_group(mut input: &[u8], expected_field: u32, depth: usize) -> HalResult<&[u8]> {
    if depth > MAX_PROTOBUF_GROUP_DEPTH {
        return Err(invalid_wire("protobuf group nesting is too deep"));
    }
    while !input.is_empty() {
        let (key, key_len) = read_varint(input)?;
        input = &input[key_len..];
        let field = u32::try_from(key >> 3).unwrap_or(u32::MAX);
        if field == 0 {
            return Err(invalid_wire("protobuf field number zero is invalid"));
        }
        match key & 0x07 {
            0 => {
                let (_, len) = read_varint(input)?;
                input = &input[len..];
            }
            1 => {
                input = input
                    .get(8..)
                    .ok_or_else(|| invalid_wire("truncated fixed64 protobuf field"))?;
            }
            2 => {
                let (len, prefix_len) = read_varint(input)?;
                input = &input[prefix_len..];
                let len = usize::try_from(len)
                    .map_err(|_| invalid_wire("protobuf byte field length overflows usize"))?;
                input = input
                    .get(len..)
                    .ok_or_else(|| invalid_wire("truncated length-delimited protobuf field"))?;
            }
            3 => input = skip_group(input, field, depth + 1)?,
            4 if field == expected_field => return Ok(input),
            4 => {
                return Err(invalid_wire(
                    "protobuf end-group field does not match start-group",
                ));
            }
            5 => {
                input = input
                    .get(4..)
                    .ok_or_else(|| invalid_wire("truncated fixed32 protobuf field"))?;
            }
            _ => return Err(invalid_wire("unsupported protobuf wire type")),
        }
    }
    Err(invalid_wire("unterminated protobuf group"))
}

fn read_varint(input: &[u8]) -> HalResult<(u64, usize)> {
    let mut value = 0_u64;
    for (index, byte) in input.iter().copied().take(10).enumerate() {
        if index == 9 && byte > 1 {
            return Err(invalid_wire("protobuf varint overflows u64"));
        }
        value |= u64::from(byte & 0x7f) << (index * 7);
        if byte & 0x80 == 0 {
            return Ok((value, index + 1));
        }
    }
    Err(invalid_wire("truncated protobuf varint"))
}

fn invalid_wire(message: &'static str) -> HalError {
    client_error(
        "runtime.protocol.invalid_message",
        ErrorCategory::InvalidArgument,
        "runtime.protocol.decode",
        false,
        message,
    )
}

fn response_matches(expected: ExpectedResponse, payload: &envelope::Payload) -> bool {
    matches!(
        (expected, payload),
        (
            ExpectedResponse::EnumerateSerial,
            envelope::Payload::EnumerateSerialResponse(_)
        ) | (
            ExpectedResponse::OpenSerial,
            envelope::Payload::OpenSerialResponse(_)
        ) | (
            ExpectedResponse::SerialRead { .. },
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

fn frame_codec(max_frame_bytes: usize) -> LengthDelimitedCodec {
    LengthDelimitedCodec::builder()
        .max_frame_length(max_frame_bytes.min(MAX_FRAME_BYTES))
        .new_codec()
}

fn frame_read_error(operation: &'static str, error: std::io::Error) -> HalError {
    if error.kind() == std::io::ErrorKind::InvalidData {
        frame_too_large("inbound frame length prefix exceeds the active limit")
    } else {
        disconnected_error(operation, error.to_string())
    }
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

#[cfg(test)]
mod tests {
    use std::collections::{HashMap, HashSet, VecDeque};

    use zeroize::Zeroize;

    use super::{RequestState, SecretToken, WireValue, visit_fields};

    #[test]
    fn client_secret_zeroize_clears_owned_bytes() {
        let mut token = SecretToken::new([0x5a; 32]);
        token.zeroize();
        assert_eq!(token.expose(), &[0; 32]);
    }

    #[test]
    fn request_id_exhaustion_uses_last_nonzero_id_then_fails_closed() {
        let mut state = RequestState {
            next_request_id: u64::MAX,
            pending: HashMap::new(),
            cancelled: HashMap::new(),
            completed: HashSet::new(),
            completed_order: VecDeque::new(),
            terminal: None,
        };

        assert_eq!(state.take_request_id().unwrap(), u64::MAX);
        assert_eq!(
            state.take_request_id().unwrap_err().name().as_str(),
            "runtime.protocol.request_id_exhausted"
        );
    }

    #[test]
    fn protobuf_scanner_skips_nested_unknown_groups_without_visiting_contents() {
        let frame = [
            0x1b, // field 3, start group
            0x08, 0x07, // grouped field 1
            0x23, // field 4, nested start group
            0xca, 0x01, 0x02, 0x0a, 0x00, // grouped field 25 bytes
            0x24, // field 4, end group
            0x1c, // field 3, end group
            0x08, 0x2a, // top-level field 1
        ];
        let mut visited = Vec::new();

        visit_fields(&frame, |field, wire| {
            if let WireValue::Varint(value) = wire {
                visited.push((field, value));
            }
            Ok(())
        })
        .unwrap();

        assert_eq!(visited, vec![(1, 42)]);
    }

    #[test]
    fn protobuf_scanner_rejects_invalid_group_structure_and_excessive_depth() {
        for malformed in [&[0x1c][..], &[0x1b][..], &[0x1b, 0x24][..]] {
            assert_eq!(
                visit_fields(malformed, |_, _| Ok(()))
                    .unwrap_err()
                    .name()
                    .as_str(),
                "runtime.protocol.invalid_message"
            );
        }

        let mut too_deep = vec![0x1b; 65];
        too_deep.extend(std::iter::repeat_n(0x1c, 65));
        assert_eq!(
            visit_fields(&too_deep, |_, _| Ok(()))
                .unwrap_err()
                .name()
                .as_str(),
            "runtime.protocol.invalid_message"
        );
    }
}
