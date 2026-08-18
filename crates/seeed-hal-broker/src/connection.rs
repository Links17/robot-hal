use std::collections::{HashSet, VecDeque};
use std::future::Future;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use bytes::{Bytes, BytesMut};
use futures_util::{SinkExt, StreamExt};
use prost::Message;
use seeed_hal_core::{ErrorCategory, HalError, HalResult, OwnerId, SessionId};
use seeed_hal_protocol::v1::{self, envelope};
use seeed_hal_protocol::{
    MAX_FRAME_BYTES, PROTOCOL_MAJOR, PROTOCOL_MINOR_MAXIMUM, PROTOCOL_MINOR_MINIMUM,
    gpio_config_from_proto, handshake_minor_range, invalid_message, negotiate_protocol_minor,
    open_serial_request_from_proto, parse_serial_session_lease, parse_session_lease,
};
use seeed_hal_runtime::{HalRuntime, RuntimeEvent, RuntimeEventKind};
use seeed_hal_serial::ControlLines;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::{mpsc, watch};
use tokio::task::JoinSet;
use tokio_util::codec::{Framed, LengthDelimitedCodec};
use uuid::Uuid;
use zeroize::{Zeroize, Zeroizing};

use crate::camera_dispatch::{
    self, CameraSessions, new_session_registry as new_camera_session_registry,
};
use crate::can_dispatch::{
    self, CanDispatchLimits, CanSessions, broker_capabilities, is_can_payload, is_can_session,
    new_session_registry,
};
use crate::usb_gpio_dispatch::{
    self, UsbGpioDispatchLimits, UsbGpioSessions,
    new_session_registry as new_usb_gpio_session_registry,
};
use crate::{Broker, StartupToken};

const DEFAULT_REQUEST_QUEUE_CAPACITY: usize = 32;
const DEFAULT_RESPONSE_QUEUE_CAPACITY: usize = 64;
const DEFAULT_MAX_IN_FLIGHT_REQUESTS: usize = 32;
const CONNECTION_TASK_SHUTDOWN_TIMEOUT: Duration = Duration::from_millis(100);
const CLOSED_SERIAL_SESSION_RETENTION: usize = 256;

#[derive(Default)]
struct SerialSessionRegistry {
    active: HashSet<SessionId>,
    closed: HashSet<SessionId>,
    closed_order: VecDeque<SessionId>,
}

type SerialSessions = Arc<Mutex<SerialSessionRegistry>>;

/// Bounded broker admission settings.
///
/// A full request or in-flight task queue returns `runtime.queue.full`. A full
/// response queue closes the connection and is recorded in [`ConnectionOutcome`]
/// because another response cannot be admitted safely. Runtime event delivery
/// uses the runtime's bounded event queue and reports `runtime.event.lagged`.
#[derive(Clone, Debug)]
pub struct BrokerConfig {
    request_queue_capacity: usize,
    response_queue_capacity: usize,
    max_in_flight_requests: usize,
}

impl Default for BrokerConfig {
    fn default() -> Self {
        Self {
            request_queue_capacity: DEFAULT_REQUEST_QUEUE_CAPACITY,
            response_queue_capacity: DEFAULT_RESPONSE_QUEUE_CAPACITY,
            max_in_flight_requests: DEFAULT_MAX_IN_FLIGHT_REQUESTS,
        }
    }
}

impl BrokerConfig {
    pub fn with_request_queue_capacity(mut self, capacity: usize) -> Self {
        self.request_queue_capacity = capacity.max(1);
        self
    }

    pub fn with_response_queue_capacity(mut self, capacity: usize) -> Self {
        self.response_queue_capacity = capacity.max(1);
        self
    }

    pub fn with_max_in_flight_requests(mut self, capacity: usize) -> Self {
        self.max_in_flight_requests = capacity.max(1);
        self
    }
}

#[derive(Debug)]
pub struct ConnectionOutcome {
    cleanup_error: Option<HalError>,
    connection_error: Option<HalError>,
}

impl ConnectionOutcome {
    pub fn cleanup_error(&self) -> Option<&HalError> {
        self.cleanup_error.as_ref()
    }

    pub fn connection_error(&self) -> Option<&HalError> {
        self.connection_error.as_ref()
    }
}

#[derive(Clone, Copy)]
struct NegotiatedLimits {
    protocol_minor: u32,
    max_frame_bytes: usize,
    max_read_bytes: usize,
    max_write_bytes: usize,
}

struct InboundRequest {
    envelope: SensitiveEnvelope,
    encoded_len: usize,
}

struct SensitiveEnvelope {
    envelope: v1::Envelope,
    #[cfg(test)]
    drop_observer: Option<Arc<Mutex<Option<Vec<u8>>>>>,
}

impl SensitiveEnvelope {
    fn new(envelope: v1::Envelope) -> Self {
        Self {
            envelope,
            #[cfg(test)]
            drop_observer: None,
        }
    }

    #[cfg(test)]
    fn with_drop_observer(
        envelope: v1::Envelope,
        drop_observer: Arc<Mutex<Option<Vec<u8>>>>,
    ) -> Self {
        Self {
            envelope,
            drop_observer: Some(drop_observer),
        }
    }
}

impl Drop for SensitiveEnvelope {
    fn drop(&mut self) {
        if let Some(envelope::Payload::HandshakeRequest(request)) = self.envelope.payload.as_mut() {
            request.startup_token.as_mut_slice().zeroize();
            #[cfg(test)]
            if let Some(observer) = &self.drop_observer {
                *observer
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner()) =
                    Some(request.startup_token.clone());
            }
        }
    }
}

struct OutboundEnvelope {
    envelope: v1::Envelope,
    max_frame_bytes: usize,
}

impl Broker {
    pub async fn serve_connection<T>(&self, io: T) -> ConnectionOutcome
    where
        T: AsyncRead + AsyncWrite + Unpin + Send + 'static,
    {
        self.serve_connection_until(io, std::future::pending())
            .await
    }

    /// Serves one connection until the peer disconnects or `shutdown`
    /// completes. Cooperative shutdown always reaches owner revocation before
    /// this future returns.
    pub async fn serve_connection_until<T, F>(&self, io: T, shutdown: F) -> ConnectionOutcome
    where
        T: AsyncRead + AsyncWrite + Unpin + Send + 'static,
        F: Future<Output = ()>,
    {
        let owner = OwnerId::parse(format!("broker:connection:{}", Uuid::new_v4()))
            .expect("generated broker owner identifier is valid");
        let framed = Framed::new(io, frame_codec());
        let (sink, stream) = framed.split();
        let active = Arc::new(Mutex::new(HashSet::new()));
        let can_sessions = new_session_registry();
        let serial_sessions = Arc::new(Mutex::new(SerialSessionRegistry::default()));
        let usb_gpio_sessions = new_usb_gpio_session_registry();
        let camera_sessions = new_camera_session_registry();
        let (frame_limit_tx, frame_limit_rx) = watch::channel(None::<usize>);
        let (request_tx, request_rx) = mpsc::channel(self.config.request_queue_capacity);
        let (response_tx, response_rx) = mpsc::channel(self.config.response_queue_capacity);
        let (cancel_tx, cancel_rx) = watch::channel(false);

        let reader = tokio::spawn(read_requests(
            stream,
            request_tx,
            response_tx.clone(),
            active.clone(),
            frame_limit_rx,
            cancel_rx.clone(),
        ));
        let writer_cancel = cancel_tx.clone();
        let writer = tokio::spawn(async move {
            let result = write_responses(sink, response_rx).await;
            if result.is_err() {
                let _ = writer_cancel.send(true);
            }
            result
        });

        let mut dispatch = Box::pin(dispatch_requests(
            self.runtime.clone(),
            self.startup_token.clone(),
            self.config.clone(),
            owner.clone(),
            request_rx,
            response_tx.clone(),
            active,
            can_sessions,
            serial_sessions,
            usb_gpio_sessions,
            camera_sessions,
            frame_limit_tx,
            cancel_rx,
        ));
        let mut shutdown = Box::pin(shutdown);
        let dispatch_error = tokio::select! {
            result = dispatch.as_mut() => result,
            () = shutdown.as_mut() => {
                let _ = cancel_tx.send(true);
                dispatch.as_mut().await
            },
        };

        let cleanup_error = self.runtime.revoke_owner(&owner).await.err();
        let _ = cancel_tx.send(true);
        drop(response_tx);
        let reader_error = finish_connection_task(reader, "runtime.protocol.read").await;
        let writer_error = finish_connection_task(writer, "runtime.protocol.write").await;

        ConnectionOutcome {
            cleanup_error,
            connection_error: dispatch_error.or(reader_error).or(writer_error),
        }
    }
}

fn frame_codec() -> LengthDelimitedCodec {
    LengthDelimitedCodec::builder()
        .max_frame_length(MAX_FRAME_BYTES)
        .new_codec()
}

async fn read_requests<R>(
    mut stream: futures_util::stream::SplitStream<Framed<R, LengthDelimitedCodec>>,
    request_tx: mpsc::Sender<InboundRequest>,
    response_tx: mpsc::Sender<OutboundEnvelope>,
    active: Arc<Mutex<HashSet<u64>>>,
    mut frame_limit: watch::Receiver<Option<usize>>,
    mut cancel: watch::Receiver<bool>,
) -> HalResult<()>
where
    R: AsyncRead + AsyncWrite + Unpin,
{
    loop {
        let frame = tokio::select! {
            changed = cancel.changed() => {
                if changed.is_err() || *cancel.borrow() {
                    return Ok(());
                }
                continue;
            }
            frame = stream.next() => frame,
        };
        let Some(frame) = frame else {
            return Ok(());
        };
        let mut frame = frame.map_err(|error| {
            protocol_error(
                "runtime.protocol.frame_invalid",
                "runtime.protocol.read",
                ErrorCategory::InvalidArgument,
                false,
                error.to_string(),
            )
        })?;
        let encoded_len = frame.len();
        if encoded_len > reader_frame_limit(&frame_limit) {
            return Err(frame_too_large(
                "inbound frame exceeds the active connection maximum",
            ));
        }
        let request =
            SensitiveEnvelope::new(v1::Envelope::decode(frame.as_ref()).map_err(|error| {
                protocol_error(
                    "runtime.protocol.invalid_message",
                    "runtime.protocol.decode",
                    ErrorCategory::InvalidArgument,
                    false,
                    error.to_string(),
                )
            })?);
        let is_handshake = matches!(
            request.envelope.payload,
            Some(envelope::Payload::HandshakeRequest(_))
        );
        if is_handshake {
            frame.as_mut().zeroize();
        }
        let request_id = request.envelope.request_id;
        if request_id == 0 {
            send_response(
                &response_tx,
                error_envelope(request_id, invalid_message("request_id must be non-zero")),
                reader_frame_limit(&frame_limit),
            )?;
            continue;
        }

        let inserted = {
            active
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .insert(request_id)
        };
        if !inserted {
            send_response(
                &response_tx,
                error_envelope(
                    request_id,
                    protocol_error(
                        "runtime.protocol.duplicate_request_id",
                        "runtime.protocol.admit",
                        ErrorCategory::Conflict,
                        false,
                        "request_id is already in flight",
                    ),
                ),
                reader_frame_limit(&frame_limit),
            )?;
            return Ok(());
        }

        match request_tx.try_send(InboundRequest {
            envelope: request,
            encoded_len,
        }) {
            Ok(()) => {
                if is_handshake && !wait_for_negotiated_limit(&mut frame_limit, &mut cancel).await {
                    return Ok(());
                }
            }
            Err(mpsc::error::TrySendError::Full(request)) => {
                remove_active(&active, request_id);
                send_response(
                    &response_tx,
                    error_envelope(
                        request.envelope.envelope.request_id,
                        queue_full("broker request queue is full"),
                    ),
                    reader_frame_limit(&frame_limit),
                )?;
            }
            Err(mpsc::error::TrySendError::Closed(_)) => return Ok(()),
        }
    }
}

fn reader_frame_limit(frame_limit: &watch::Receiver<Option<usize>>) -> usize {
    (*frame_limit.borrow()).unwrap_or(MAX_FRAME_BYTES)
}

async fn wait_for_negotiated_limit(
    frame_limit: &mut watch::Receiver<Option<usize>>,
    cancel: &mut watch::Receiver<bool>,
) -> bool {
    loop {
        if frame_limit.borrow().is_some() {
            return true;
        }
        if *cancel.borrow() {
            return false;
        }
        tokio::select! {
            changed = frame_limit.changed() => {
                if changed.is_err() {
                    return false;
                }
            }
            changed = cancel.changed() => {
                if changed.is_err() {
                    return false;
                }
            }
        }
    }
}

async fn write_responses<S>(
    mut sink: futures_util::stream::SplitSink<Framed<S, LengthDelimitedCodec>, Bytes>,
    mut responses: mpsc::Receiver<OutboundEnvelope>,
) -> HalResult<()>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    while let Some(response) = responses.recv().await {
        let encoded_len = response.envelope.encoded_len();
        if encoded_len > MAX_FRAME_BYTES || encoded_len > response.max_frame_bytes {
            return Err(frame_too_large(
                "outbound envelope exceeds the negotiated maximum",
            ));
        }
        let mut encoded = BytesMut::with_capacity(encoded_len);
        response.envelope.encode(&mut encoded).map_err(|error| {
            protocol_error(
                "runtime.protocol.encode_failed",
                "runtime.protocol.write",
                ErrorCategory::Internal,
                false,
                error.to_string(),
            )
        })?;
        sink.send(encoded.freeze()).await.map_err(|error| {
            protocol_error(
                "runtime.protocol.connection_lost",
                "runtime.protocol.write",
                ErrorCategory::Unavailable,
                true,
                error.to_string(),
            )
        })?;
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn dispatch_requests(
    runtime: HalRuntime,
    startup_token: StartupToken,
    config: BrokerConfig,
    owner: OwnerId,
    mut requests: mpsc::Receiver<InboundRequest>,
    responses: mpsc::Sender<OutboundEnvelope>,
    active: Arc<Mutex<HashSet<u64>>>,
    can_sessions: CanSessions,
    serial_sessions: SerialSessions,
    usb_gpio_sessions: UsbGpioSessions,
    camera_sessions: CameraSessions,
    frame_limit: watch::Sender<Option<usize>>,
    mut cancel: watch::Receiver<bool>,
) -> Option<HalError> {
    let mut handshaken = false;
    let mut limits: Option<NegotiatedLimits> = None;
    let mut events = runtime.subscribe();
    let mut tasks = JoinSet::new();
    let mut failure = None;

    loop {
        tokio::select! {
            changed = cancel.changed() => {
                if changed.is_err() || *cancel.borrow() {
                    break;
                }
            }
            joined = tasks.join_next(), if !tasks.is_empty() => {
                if let Some(result) = joined {
                    match result {
                        Ok((request_id, response)) => {
                            remove_active(&active, request_id);
                            if let Err(error) = send_response(
                                &responses,
                                response,
                                negotiated_frame_limit(limits),
                            ) {
                                failure = Some(error);
                                break;
                            }
                        }
                        Err(error) => {
                            failure = Some(protocol_error(
                                "runtime.protocol.task_failed",
                                "runtime.protocol.dispatch",
                                ErrorCategory::Internal,
                                false,
                                error.to_string(),
                            ));
                            break;
                        }
                    }
                }
            }
            event = events.recv(), if handshaken => {
                let response = match event {
                    Ok(event) if event_is_visible_to_owner(&event, &owner) => v1::Envelope {
                        request_id: 0,
                        payload: Some(envelope::Payload::RuntimeEvent((&event).into())),
                    },
                    Ok(_) => continue,
                    Err(error) => error_envelope(0, error),
                };
                if let Err(error) = send_response(
                    &responses,
                    response,
                    negotiated_frame_limit(limits),
                ) {
                    failure = Some(error);
                    break;
                }
            }
            request = requests.recv() => {
                let Some(request) = request else { break; };
                if handshaken
                    && request.encoded_len
                        > limits
                            .expect("successful handshake sets negotiated limits")
                            .max_frame_bytes
                {
                    remove_active(&active, request.envelope.envelope.request_id);
                    failure = Some(frame_too_large(
                        "inbound frame exceeds the negotiated maximum",
                    ));
                    break;
                }
                let mut request = request.envelope;
                let request_id = request.envelope.request_id;

                if !handshaken {
                    match request.envelope.payload.as_mut() {
                        Some(envelope::Payload::HandshakeRequest(handshake)) => {
                            match validate_handshake(&startup_token, handshake) {
                                Ok((response, negotiated)) => {
                                    handshaken = true;
                                    limits = Some(negotiated);
                                    frame_limit.send_replace(Some(negotiated.max_frame_bytes));
                                    remove_active(&active, request_id);
                                    if let Err(error) = send_response(
                                        &responses,
                                        v1::Envelope {
                                            request_id,
                                            payload: Some(envelope::Payload::HandshakeResponse(response)),
                                        },
                                        negotiated.max_frame_bytes,
                                    ) {
                                        failure = Some(error);
                                        break;
                                    }
                                }
                                Err(error) => {
                                    remove_active(&active, request_id);
                                    if let Err(error) = send_response(
                                        &responses,
                                        error_envelope(request_id, error),
                                        MAX_FRAME_BYTES,
                                    ) {
                                        failure = Some(error);
                                    }
                                    break;
                                }
                            }
                        }
                        _ => {
                            remove_active(&active, request_id);
                            let error = protocol_error(
                                "runtime.protocol.handshake_required",
                                "runtime.protocol.dispatch",
                                ErrorCategory::Conflict,
                                false,
                                "the connection must complete a handshake before operations",
                            );
                            if let Err(error) = send_response(
                                &responses,
                                error_envelope(request_id, error),
                                MAX_FRAME_BYTES,
                            ) {
                                failure = Some(error);
                                break;
                            }
                        }
                    }
                    continue;
                }

                if matches!(
                    request.envelope.payload.as_ref(),
                    Some(envelope::Payload::HandshakeRequest(_))
                ) {
                    remove_active(&active, request_id);
                    let error = invalid_message("handshake may only be sent once per connection");
                    if let Err(error) = send_response(
                        &responses,
                        error_envelope(request_id, error),
                        negotiated_frame_limit(limits),
                    ) {
                        failure = Some(error);
                        break;
                    }
                    continue;
                }

                if tasks.len() >= config.max_in_flight_requests {
                    remove_active(&active, request_id);
                    if let Err(error) = send_response(
                        &responses,
                        error_envelope(request_id, queue_full("broker task queue is full")),
                        negotiated_frame_limit(limits),
                    ) {
                        failure = Some(error);
                        break;
                    }
                    continue;
                }

                let runtime = runtime.clone();
                let owner = owner.clone();
                let can_sessions = can_sessions.clone();
                let serial_sessions = serial_sessions.clone();
                let usb_gpio_sessions = usb_gpio_sessions.clone();
                let camera_sessions = camera_sessions.clone();
                let limits = limits.expect("successful handshake sets negotiated limits");
                let payload = request.envelope.payload.take();
                tasks.spawn(async move {
                    let response = dispatch_operation(
                        runtime,
                        owner,
                        request_id,
                        payload,
                        limits,
                        can_sessions,
                        serial_sessions,
                        usb_gpio_sessions,
                        camera_sessions,
                    ).await;
                    (request_id, response)
                });
            }
        }
    }

    tasks.abort_all();
    while tasks.join_next().await.is_some() {}
    failure
}

fn event_is_visible_to_owner(event: &RuntimeEvent, owner: &OwnerId) -> bool {
    match event.kind() {
        RuntimeEventKind::SessionOpened
        | RuntimeEventKind::SessionClosed
        | RuntimeEventKind::CanBusActive
        | RuntimeEventKind::CanBusWarning
        | RuntimeEventKind::CanBusPassive
        | RuntimeEventKind::CanBusOff
        | RuntimeEventKind::CanBusStopped
        | RuntimeEventKind::CanBusUnknown => event.owner_id() == owner,
    }
}

fn validate_handshake(
    expected_token: &StartupToken,
    request: &mut v1::HandshakeRequest,
) -> HalResult<(v1::HandshakeResponse, NegotiatedLimits)> {
    let presented_token = Zeroizing::new(std::mem::take(&mut request.startup_token));
    let token_matches = expected_token.authenticates(presented_token.as_slice());
    if !token_matches {
        return Err(protocol_error(
            "runtime.protocol.authentication_failed",
            "runtime.protocol.handshake",
            ErrorCategory::Conflict,
            false,
            "startup token did not authenticate the connection",
        ));
    }
    let (client_minimum, client_maximum) = handshake_minor_range(request)?;
    let selected_minor = negotiate_protocol_minor(
        request.protocol_major,
        client_minimum,
        client_maximum,
        PROTOCOL_MAJOR,
        PROTOCOL_MINOR_MINIMUM,
        PROTOCOL_MINOR_MAXIMUM,
    )?;
    let mut capabilities = broker_capabilities(selected_minor);
    capabilities.extend(usb_gpio_dispatch::broker_capabilities(selected_minor));
    capabilities.extend(camera_dispatch::broker_capabilities(selected_minor));
    for capability in &request.required_capabilities {
        if !capabilities.contains(capability) {
            return Err(protocol_error(
                "runtime.protocol.unsupported_capability",
                "runtime.protocol.handshake",
                ErrorCategory::Conflict,
                false,
                "a required capability is not supported",
            ));
        }
    }
    let frame_limit = usize::try_from(request.max_frame_bytes).unwrap_or(usize::MAX);
    let read_limit = usize::try_from(request.max_read_bytes).unwrap_or(usize::MAX);
    let write_limit = usize::try_from(request.max_write_bytes).unwrap_or(usize::MAX);
    if frame_limit == 0
        || frame_limit > MAX_FRAME_BYTES
        || read_limit == 0
        || write_limit == 0
        || read_response_encoded_len(read_limit) > frame_limit
        || write_request_encoded_len(write_limit) > frame_limit
    {
        return Err(invalid_message(
            "negotiated frame/read/write byte limits are invalid",
        ));
    }

    let response = v1::HandshakeResponse {
        protocol_major: PROTOCOL_MAJOR,
        protocol_minor: selected_minor,
        capabilities,
        max_frame_bytes: request.max_frame_bytes,
        max_read_bytes: request.max_read_bytes,
        max_write_bytes: request.max_write_bytes,
        protocol_minor_minimum: PROTOCOL_MINOR_MINIMUM,
        protocol_minor_maximum: PROTOCOL_MINOR_MAXIMUM,
    };
    let response_envelope = v1::Envelope {
        request_id: u64::MAX,
        payload: Some(envelope::Payload::HandshakeResponse(response.clone())),
    };
    if response_envelope.encoded_len() > frame_limit {
        return Err(invalid_message(
            "negotiated frame limit cannot contain the handshake response",
        ));
    }

    Ok((
        response,
        NegotiatedLimits {
            protocol_minor: selected_minor,
            max_frame_bytes: frame_limit,
            max_read_bytes: read_limit,
            max_write_bytes: write_limit,
        },
    ))
}

fn read_response_encoded_len(data_len: usize) -> usize {
    let inner_len = length_delimited_field_len(1, data_len);
    envelope_encoded_len(25, inner_len)
}

fn write_request_encoded_len(data_len: usize) -> usize {
    const RUNTIME_UUID_LEN: usize = 36;
    let session_id = length_delimited_field_len(1, RUNTIME_UUID_LEN);
    let lease_inner = length_delimited_field_len(1, RUNTIME_UUID_LEN) + 1 + 10 + 1 + 1;
    let lease = length_delimited_field_len(2, lease_inner);
    let data = length_delimited_field_len(3, data_len);
    envelope_encoded_len(26, session_id + lease + data)
}

fn envelope_encoded_len(payload_field_number: u32, payload_len: usize) -> usize {
    // request_id field/tag plus the largest possible non-zero uint64 value.
    1 + 10
        + prost_varint_len((u64::from(payload_field_number) << 3) | 2)
        + prost_varint_len(payload_len as u64)
        + payload_len
}

fn length_delimited_field_len(field_number: u32, value_len: usize) -> usize {
    prost_varint_len((u64::from(field_number) << 3) | 2)
        + prost_varint_len(value_len as u64)
        + value_len
}

fn prost_varint_len(mut value: u64) -> usize {
    let mut len = 1;
    while value >= 0x80 {
        value >>= 7;
        len += 1;
    }
    len
}

#[allow(clippy::too_many_arguments)]
async fn dispatch_operation(
    runtime: HalRuntime,
    owner: OwnerId,
    request_id: u64,
    payload: Option<envelope::Payload>,
    limits: NegotiatedLimits,
    can_sessions: CanSessions,
    serial_sessions: SerialSessions,
    usb_gpio_sessions: UsbGpioSessions,
    camera_sessions: CameraSessions,
) -> v1::Envelope {
    let result = match payload {
        Some(payload)
            if is_camera_payload(&payload)
                && (limits.protocol_minor < camera_dispatch::CAMERA_WIRE_MINOR) =>
        {
            Err(protocol_error(
                "runtime.protocol.unsupported_capability",
                "runtime.protocol.dispatch",
                ErrorCategory::Conflict,
                false,
                "Camera operations require negotiated protocol minor 3",
            ))
        }
        Some(payload) if is_usb_gpio_payload(&payload) && limits.protocol_minor < 2 => {
            Err(protocol_error(
                "runtime.protocol.unsupported_capability",
                "runtime.protocol.dispatch",
                ErrorCategory::Conflict,
                false,
                "USB and GPIO operations require negotiated protocol minor 2",
            ))
        }
        Some(payload) if is_can_payload(&payload) => {
            can_dispatch::dispatch(
                runtime,
                owner,
                payload,
                CanDispatchLimits {
                    protocol_minor: limits.protocol_minor,
                    max_frame_bytes: limits.max_frame_bytes,
                    max_read_bytes: limits.max_read_bytes,
                    max_write_bytes: limits.max_write_bytes,
                },
                can_sessions,
            )
            .await
        }
        Some(payload) if is_usb_gpio_payload(&payload) => {
            usb_gpio_dispatch::dispatch(
                runtime,
                owner,
                payload,
                UsbGpioDispatchLimits {
                    max_frame_bytes: limits.max_frame_bytes,
                    max_read_bytes: limits.max_read_bytes,
                    max_write_bytes: limits.max_write_bytes,
                },
                usb_gpio_sessions,
            )
            .await
        }
        Some(payload) if is_camera_payload(&payload) => {
            camera_dispatch::dispatch(runtime, owner, payload, camera_sessions).await
        }
        payload => {
            dispatch_operation_inner(
                runtime,
                owner,
                payload,
                limits,
                can_sessions,
                serial_sessions,
            )
            .await
        }
    };
    match result {
        Ok(payload) => v1::Envelope {
            request_id,
            payload: Some(payload),
        },
        Err(error) => error_envelope(request_id, error),
    }
}

fn is_camera_payload(payload: &envelope::Payload) -> bool {
    matches!(
        payload,
        envelope::Payload::EnumerateCameraRequest(_)
            | envelope::Payload::EnumerateCameraResponse(_)
            | envelope::Payload::OpenCameraRequest(_)
            | envelope::Payload::OpenCameraResponse(_)
            | envelope::Payload::CaptureCameraRequest(_)
            | envelope::Payload::CaptureCameraResponse(_)
            | envelope::Payload::CameraMappingDescriptorRequest(_)
            | envelope::Payload::CameraMappingDescriptorResponse(_)
            | envelope::Payload::CameraNextFrameLeaseRequest(_)
            | envelope::Payload::CameraNextFrameLeaseResponse(_)
            | envelope::Payload::CameraDroppedCountRequest(_)
            | envelope::Payload::CameraDroppedCountResponse(_)
            | envelope::Payload::CameraControlsRequest(_)
            | envelope::Payload::CameraControlsResponse(_)
            | envelope::Payload::CameraGetControlRequest(_)
            | envelope::Payload::CameraGetControlResponse(_)
            | envelope::Payload::CameraSetControlRequest(_)
            | envelope::Payload::CameraSetControlResponse(_)
            | envelope::Payload::CameraSetAutoRequest(_)
            | envelope::Payload::CameraSetAutoResponse(_)
            | envelope::Payload::CloseCameraRequest(_)
            | envelope::Payload::CloseCameraResponse(_)
    )
}

fn is_usb_gpio_payload(payload: &envelope::Payload) -> bool {
    matches!(
        payload,
        envelope::Payload::EnumerateUsbRequest(_)
            | envelope::Payload::EnumerateUsbResponse(_)
            | envelope::Payload::OpenUsbRequest(_)
            | envelope::Payload::OpenUsbResponse(_)
            | envelope::Payload::UsbTransferRequest(_)
            | envelope::Payload::UsbTransferResponse(_)
            | envelope::Payload::CloseUsbRequest(_)
            | envelope::Payload::CloseUsbResponse(_)
            | envelope::Payload::EnumerateGpioRequest(_)
            | envelope::Payload::EnumerateGpioResponse(_)
            | envelope::Payload::OpenGpioRequest(_)
            | envelope::Payload::OpenGpioResponse(_)
            | envelope::Payload::GpioReadRequest(_)
            | envelope::Payload::GpioReadResponse(_)
            | envelope::Payload::GpioWriteRequest(_)
            | envelope::Payload::GpioWriteResponse(_)
            | envelope::Payload::GpioNextEdgeRequest(_)
            | envelope::Payload::GpioNextEdgeResponse(_)
            | envelope::Payload::CloseGpioRequest(_)
            | envelope::Payload::CloseGpioResponse(_)
    )
}

async fn dispatch_operation_inner(
    runtime: HalRuntime,
    owner: OwnerId,
    payload: Option<envelope::Payload>,
    limits: NegotiatedLimits,
    can_sessions: CanSessions,
    serial_sessions: SerialSessions,
) -> HalResult<envelope::Payload> {
    match payload {
        Some(envelope::Payload::EnumerateUsbRequest(_)) => {
            let resources = runtime
                .enumerate_usb()
                .await?
                .iter()
                .map(TryInto::try_into)
                .collect::<HalResult<Vec<_>>>()?;
            Ok(envelope::Payload::EnumerateUsbResponse(
                v1::EnumerateUsbResponse { resources },
            ))
        }
        Some(envelope::Payload::EnumerateGpioRequest(_)) => {
            let resources = runtime
                .enumerate_gpio()
                .await?
                .iter()
                .map(TryInto::try_into)
                .collect::<HalResult<Vec<_>>>()?;
            Ok(envelope::Payload::EnumerateGpioResponse(
                v1::EnumerateGpioResponse { resources },
            ))
        }
        Some(envelope::Payload::OpenUsbRequest(request)) => {
            let selector = request
                .selector
                .ok_or_else(|| invalid_message("usb open selector is required"))?
                .try_into()?;
            let interface = u8::try_from(request.interface_number)
                .map_err(|_| invalid_message("USB interface number exceeds u8"))?;
            let handle = runtime.open_usb(owner, selector, interface).await?;
            let (session_id, lease) = handle.into_parts();
            Ok(envelope::Payload::OpenUsbResponse(v1::OpenUsbResponse {
                session_id: session_id.as_str().to_owned(),
                lease: Some((&lease).into()),
            }))
        }
        Some(envelope::Payload::OpenGpioRequest(request)) => {
            if request.lines.is_empty() {
                return Err(invalid_message("GPIO open requires at least one line"));
            }
            let selector = request
                .selector
                .ok_or_else(|| invalid_message("GPIO open selector is required"))?
                .try_into()?;
            let config = gpio_config_from_proto(
                request
                    .config
                    .ok_or_else(|| invalid_message("GPIO line configuration is required"))?,
            )?;
            let handle = runtime
                .open_gpio(owner, selector, request.lines, config)
                .await?;
            let (session_id, lease) = handle.into_parts();
            Ok(envelope::Payload::OpenGpioResponse(v1::OpenGpioResponse {
                session_id: session_id.as_str().to_owned(),
                lease: Some((&lease).into()),
            }))
        }
        Some(envelope::Payload::EnumerateSerialRequest(_)) => {
            let resources = runtime
                .enumerate_serial()
                .await?
                .iter()
                .map(TryInto::try_into)
                .collect::<HalResult<Vec<_>>>()?;
            Ok(envelope::Payload::EnumerateSerialResponse(
                v1::EnumerateSerialResponse { resources },
            ))
        }
        Some(envelope::Payload::OpenSerialRequest(request)) => {
            let (selector, config) = open_serial_request_from_proto(request)?;
            let handle = runtime.open_serial(owner, selector, config).await?;
            let (session_id, lease) = handle.into_parts();
            register_serial_session(&serial_sessions, session_id.clone());
            Ok(envelope::Payload::OpenSerialResponse(
                v1::OpenSerialResponse {
                    session_id: session_id.as_str().to_owned(),
                    lease: Some((&lease).into()),
                },
            ))
        }
        Some(envelope::Payload::SerialReadRequest(request)) => {
            let max_bytes = usize::try_from(request.max_bytes).unwrap_or(usize::MAX);
            if max_bytes == 0 || max_bytes > limits.max_read_bytes {
                return Err(invalid_message(
                    "serial read byte limit exceeds the negotiated maximum",
                ));
            }
            let (session, lease) = parse_serial_session_lease(request.session_id, request.lease)?;
            let data = runtime.read_serial(session, &lease, max_bytes).await?;
            Ok(envelope::Payload::SerialReadResponse(
                v1::SerialReadResponse {
                    data: data.to_vec(),
                },
            ))
        }
        Some(envelope::Payload::SerialWriteRequest(request)) => {
            if request.data.len() > limits.max_write_bytes {
                return Err(invalid_message(
                    "serial write byte limit exceeds the negotiated maximum",
                ));
            }
            let (session, lease) = parse_serial_session_lease(request.session_id, request.lease)?;
            runtime
                .write_serial(session, &lease, Bytes::from(request.data))
                .await?;
            Ok(envelope::Payload::SerialWriteResponse(v1::Empty {}))
        }
        Some(envelope::Payload::SerialFlushRequest(request)) => {
            let (session, lease) = parse_serial_session_lease(request.session_id, request.lease)?;
            runtime.flush_serial(session, &lease).await?;
            Ok(envelope::Payload::SerialFlushResponse(v1::Empty {}))
        }
        Some(envelope::Payload::SetSerialControlLinesRequest(request)) => {
            let (session, lease) = parse_serial_session_lease(request.session_id, request.lease)?;
            runtime
                .set_serial_control_lines(
                    session,
                    &lease,
                    ControlLines {
                        data_terminal_ready: request.data_terminal_ready,
                        request_to_send: request.request_to_send,
                    },
                )
                .await?;
            Ok(envelope::Payload::SetSerialControlLinesResponse(
                v1::Empty {},
            ))
        }
        Some(envelope::Payload::CloseSessionRequest(request)) => {
            if is_can_session(&can_sessions, &request.session_id) {
                return can_dispatch::close(runtime, request, can_sessions).await;
            }
            let (session, lease) = parse_session_lease(request.session_id, request.lease)?;
            if !is_serial_session(&serial_sessions, &session) {
                return Err(protocol_error(
                    "runtime.session.not_found",
                    "serial.close",
                    ErrorCategory::NotFound,
                    false,
                    "the Serial session is not owned by this broker connection",
                ));
            }
            runtime.close_serial(session.clone(), &lease).await?;
            record_serial_closed(&serial_sessions, &session);
            Ok(envelope::Payload::CloseSessionResponse(v1::Empty {}))
        }
        None => Err(invalid_message("envelope payload is required")),
        _ => Err(invalid_message(
            "response and event payloads are not valid client requests",
        )),
    }
}

fn register_serial_session(sessions: &SerialSessions, session: SessionId) {
    sessions
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .active
        .insert(session);
}

fn is_serial_session(sessions: &SerialSessions, session: &SessionId) -> bool {
    let sessions = sessions
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    sessions.active.contains(session) || sessions.closed.contains(session)
}

fn record_serial_closed(sessions: &SerialSessions, session: &SessionId) {
    let mut sessions = sessions
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if !sessions.active.remove(session) || !sessions.closed.insert(session.clone()) {
        return;
    }
    sessions.closed_order.push_back(session.clone());
    while sessions.closed_order.len() > CLOSED_SERIAL_SESSION_RETENTION {
        let evicted = sessions
            .closed_order
            .pop_front()
            .expect("closed Serial session retention is non-empty");
        sessions.closed.remove(&evicted);
    }
}

fn send_response(
    responses: &mpsc::Sender<OutboundEnvelope>,
    response: v1::Envelope,
    max_frame_bytes: usize,
) -> HalResult<()> {
    responses
        .try_send(OutboundEnvelope {
            envelope: response,
            max_frame_bytes,
        })
        .map_err(|error| match error {
            mpsc::error::TrySendError::Full(_) => response_queue_full(),
            mpsc::error::TrySendError::Closed(_) => protocol_error(
                "runtime.protocol.connection_lost",
                "runtime.protocol.write",
                ErrorCategory::Unavailable,
                true,
                "broker response channel is closed",
            ),
        })
}

fn response_queue_full() -> HalError {
    protocol_error(
        "runtime.queue.response_full",
        "runtime.protocol.write",
        ErrorCategory::Unavailable,
        true,
        "broker response queue is full",
    )
}

fn negotiated_frame_limit(limits: Option<NegotiatedLimits>) -> usize {
    limits
        .map(|limits| limits.max_frame_bytes)
        .unwrap_or(MAX_FRAME_BYTES)
}

fn error_envelope(request_id: u64, error: HalError) -> v1::Envelope {
    v1::Envelope {
        request_id,
        payload: Some(envelope::Payload::Error((&error).into())),
    }
}

fn remove_active(active: &Arc<Mutex<HashSet<u64>>>, request_id: u64) {
    active
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .remove(&request_id);
}

fn queue_full(message: &'static str) -> HalError {
    protocol_error(
        "runtime.queue.full",
        "runtime.protocol.admit",
        ErrorCategory::Unavailable,
        true,
        message,
    )
}

fn frame_too_large(message: &'static str) -> HalError {
    protocol_error(
        "runtime.protocol.frame_too_large",
        "runtime.protocol.frame",
        ErrorCategory::InvalidArgument,
        false,
        message,
    )
}

fn protocol_error(
    name: &'static str,
    operation: &'static str,
    category: ErrorCategory,
    retryable: bool,
    debug_message: impl Into<String>,
) -> HalError {
    HalError::new(name, category, operation, retryable, debug_message)
        .expect("static broker error metadata is valid")
}

async fn finish_connection_task(
    mut task: tokio::task::JoinHandle<HalResult<()>>,
    operation: &'static str,
) -> Option<HalError> {
    let result = match tokio::time::timeout(CONNECTION_TASK_SHUTDOWN_TIMEOUT, &mut task).await {
        Ok(result) => result,
        Err(_) => {
            task.abort();
            let _ = task.await;
            return Some(protocol_error(
                "runtime.protocol.task_shutdown_timeout",
                operation,
                ErrorCategory::Internal,
                false,
                "connection task did not stop before its shutdown deadline",
            ));
        }
    };
    match result {
        Ok(Ok(())) => None,
        Ok(Err(error)) => Some(error),
        Err(error) => Some(protocol_error(
            "runtime.protocol.task_failed",
            operation,
            ErrorCategory::Internal,
            false,
            error.to_string(),
        )),
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use seeed_hal_protocol::v1::{self, envelope};

    use super::SensitiveEnvelope;

    #[test]
    fn sensitive_envelope_zeroizes_decoded_handshake_on_early_drop() {
        for request_id in [0, 7] {
            let observed = Arc::new(Mutex::new(None));
            let request = SensitiveEnvelope::with_drop_observer(
                v1::Envelope {
                    request_id,
                    payload: Some(envelope::Payload::HandshakeRequest(v1::HandshakeRequest {
                        startup_token: vec![0x5a; 32],
                        ..Default::default()
                    })),
                },
                observed.clone(),
            );

            drop(request);

            assert_eq!(*observed.lock().unwrap(), Some(vec![0; 32]));
        }
    }
}
