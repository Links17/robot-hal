use std::collections::HashSet;
use std::sync::{Arc, Mutex};

use bytes::{Bytes, BytesMut};
use futures_util::{SinkExt, StreamExt};
use prost::Message;
use seeed_hal_core::{ErrorCategory, HalError, HalResult, OwnerId};
use seeed_hal_protocol::v1::{self, envelope};
use seeed_hal_protocol::{
    MAX_FRAME_BYTES, PROTOCOL_MAJOR, PROTOCOL_MINOR, SERIAL_CAPABILITY, invalid_message,
    parse_session_lease,
};
use seeed_hal_runtime::HalRuntime;
use seeed_hal_serial::ControlLines;
use subtle::ConstantTimeEq;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::{mpsc, watch};
use tokio::task::JoinSet;
use tokio_util::codec::{Framed, LengthDelimitedCodec};
use uuid::Uuid;

use crate::{Broker, StartupToken};

const DEFAULT_REQUEST_QUEUE_CAPACITY: usize = 32;
const DEFAULT_RESPONSE_QUEUE_CAPACITY: usize = 64;
const DEFAULT_MAX_IN_FLIGHT_REQUESTS: usize = 32;

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
    max_read_bytes: usize,
    max_write_bytes: usize,
}

impl Broker {
    pub async fn serve_connection<T>(&self, io: T) -> ConnectionOutcome
    where
        T: AsyncRead + AsyncWrite + Unpin + Send + 'static,
    {
        let owner = OwnerId::parse(format!("broker:connection:{}", Uuid::new_v4()))
            .expect("generated broker owner identifier is valid");
        let framed = Framed::new(io, frame_codec());
        let (sink, stream) = framed.split();
        let active = Arc::new(Mutex::new(HashSet::new()));
        let (request_tx, request_rx) = mpsc::channel(self.config.request_queue_capacity);
        let (response_tx, response_rx) = mpsc::channel(self.config.response_queue_capacity);
        let (cancel_tx, cancel_rx) = watch::channel(false);

        let reader = tokio::spawn(read_requests(
            stream,
            request_tx,
            response_tx.clone(),
            active.clone(),
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

        let dispatch_error = dispatch_requests(
            self.runtime.clone(),
            self.startup_token.clone(),
            self.config.clone(),
            owner.clone(),
            request_rx,
            response_tx.clone(),
            active,
            cancel_rx,
        )
        .await;

        let _ = cancel_tx.send(true);
        drop(response_tx);
        let reader_error = join_error(reader.await, "runtime.protocol.read");
        let writer_error = join_error(writer.await, "runtime.protocol.write");
        let cleanup_error = self.runtime.revoke_owner(&owner).await.err();

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
    request_tx: mpsc::Sender<v1::Envelope>,
    response_tx: mpsc::Sender<v1::Envelope>,
    active: Arc<Mutex<HashSet<u64>>>,
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
        let frame = frame.map_err(|error| {
            protocol_error(
                "runtime.protocol.frame_invalid",
                "runtime.protocol.read",
                ErrorCategory::InvalidArgument,
                false,
                error.to_string(),
            )
        })?;
        let request = v1::Envelope::decode(frame).map_err(|error| {
            protocol_error(
                "runtime.protocol.invalid_message",
                "runtime.protocol.decode",
                ErrorCategory::InvalidArgument,
                false,
                error.to_string(),
            )
        })?;
        let request_id = request.request_id;
        if request_id == 0 {
            send_response(
                &response_tx,
                error_envelope(request_id, invalid_message("request_id must be non-zero")),
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
            )?;
            return Ok(());
        }

        match request_tx.try_send(request) {
            Ok(()) => {}
            Err(mpsc::error::TrySendError::Full(request)) => {
                remove_active(&active, request_id);
                send_response(
                    &response_tx,
                    error_envelope(
                        request.request_id,
                        queue_full("broker request queue is full"),
                    ),
                )?;
            }
            Err(mpsc::error::TrySendError::Closed(_)) => return Ok(()),
        }
    }
}

async fn write_responses<S>(
    mut sink: futures_util::stream::SplitSink<Framed<S, LengthDelimitedCodec>, Bytes>,
    mut responses: mpsc::Receiver<v1::Envelope>,
) -> HalResult<()>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    while let Some(response) = responses.recv().await {
        let mut encoded = BytesMut::with_capacity(response.encoded_len());
        response.encode(&mut encoded).map_err(|error| {
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
    mut requests: mpsc::Receiver<v1::Envelope>,
    responses: mpsc::Sender<v1::Envelope>,
    active: Arc<Mutex<HashSet<u64>>>,
    mut cancel: watch::Receiver<bool>,
) -> Option<HalError> {
    let mut handshaken = false;
    let mut limits = None;
    let mut events = runtime.subscribe();
    let mut tasks = JoinSet::new();
    let mut failure = None;

    loop {
        tokio::select! {
            biased;
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
                            if let Err(error) = send_response(&responses, response) {
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
                    Ok(event) => v1::Envelope {
                        request_id: 0,
                        payload: Some(envelope::Payload::RuntimeEvent((&event).into())),
                    },
                    Err(error) => error_envelope(0, error),
                };
                if let Err(error) = send_response(&responses, response) {
                    failure = Some(error);
                    break;
                }
            }
            request = requests.recv() => {
                let Some(request) = request else { break; };
                let request_id = request.request_id;
                let payload = request.payload;

                if !handshaken {
                    match payload {
                        Some(envelope::Payload::HandshakeRequest(handshake)) => {
                            match validate_handshake(&startup_token, handshake) {
                                Ok((response, negotiated)) => {
                                    handshaken = true;
                                    limits = Some(negotiated);
                                    remove_active(&active, request_id);
                                    if let Err(error) = send_response(
                                        &responses,
                                        v1::Envelope {
                                            request_id,
                                            payload: Some(envelope::Payload::HandshakeResponse(response)),
                                        },
                                    ) {
                                        failure = Some(error);
                                        break;
                                    }
                                }
                                Err(error) => {
                                    remove_active(&active, request_id);
                                    if let Err(error) = send_response(&responses, error_envelope(request_id, error)) {
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
                            if let Err(error) = send_response(&responses, error_envelope(request_id, error)) {
                                failure = Some(error);
                                break;
                            }
                        }
                    }
                    continue;
                }

                if matches!(payload, Some(envelope::Payload::HandshakeRequest(_))) {
                    remove_active(&active, request_id);
                    let error = invalid_message("handshake may only be sent once per connection");
                    if let Err(error) = send_response(&responses, error_envelope(request_id, error)) {
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
                    ) {
                        failure = Some(error);
                        break;
                    }
                    continue;
                }

                let runtime = runtime.clone();
                let owner = owner.clone();
                let limits = limits.expect("successful handshake sets negotiated limits");
                tasks.spawn(async move {
                    let response = dispatch_operation(runtime, owner, request_id, payload, limits).await;
                    (request_id, response)
                });
            }
        }
    }

    tasks.abort_all();
    while tasks.join_next().await.is_some() {}
    failure
}

fn validate_handshake(
    expected_token: &StartupToken,
    request: v1::HandshakeRequest,
) -> HalResult<(v1::HandshakeResponse, NegotiatedLimits)> {
    let token_matches = request.startup_token.len() == expected_token.expose_bytes().len()
        && bool::from(
            request
                .startup_token
                .as_slice()
                .ct_eq(expected_token.expose_bytes().as_slice()),
        );
    if !token_matches {
        return Err(protocol_error(
            "runtime.protocol.authentication_failed",
            "runtime.protocol.handshake",
            ErrorCategory::Conflict,
            false,
            "startup token did not authenticate the connection",
        ));
    }
    if request.protocol_major != PROTOCOL_MAJOR || request.protocol_minor != PROTOCOL_MINOR {
        return Err(protocol_error(
            "runtime.protocol.incompatible_version",
            "runtime.protocol.handshake",
            ErrorCategory::Conflict,
            false,
            "the requested protocol version is not supported",
        ));
    }
    for capability in &request.required_capabilities {
        if capability != SERIAL_CAPABILITY {
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
        || read_limit > frame_limit
        || write_limit == 0
        || write_limit > frame_limit
    {
        return Err(invalid_message(
            "negotiated frame/read/write byte limits are invalid",
        ));
    }

    Ok((
        v1::HandshakeResponse {
            protocol_major: PROTOCOL_MAJOR,
            protocol_minor: PROTOCOL_MINOR,
            capabilities: vec![SERIAL_CAPABILITY.to_owned()],
            max_frame_bytes: request.max_frame_bytes,
            max_read_bytes: request.max_read_bytes,
            max_write_bytes: request.max_write_bytes,
        },
        NegotiatedLimits {
            max_read_bytes: read_limit,
            max_write_bytes: write_limit,
        },
    ))
}

async fn dispatch_operation(
    runtime: HalRuntime,
    owner: OwnerId,
    request_id: u64,
    payload: Option<envelope::Payload>,
    limits: NegotiatedLimits,
) -> v1::Envelope {
    let result = dispatch_operation_inner(runtime, owner, payload, limits).await;
    match result {
        Ok(payload) => v1::Envelope {
            request_id,
            payload: Some(payload),
        },
        Err(error) => error_envelope(request_id, error),
    }
}

async fn dispatch_operation_inner(
    runtime: HalRuntime,
    owner: OwnerId,
    payload: Option<envelope::Payload>,
    limits: NegotiatedLimits,
) -> HalResult<envelope::Payload> {
    match payload {
        Some(envelope::Payload::EnumerateSerialRequest(_)) => {
            let resources = runtime
                .enumerate_serial()
                .await?
                .iter()
                .map(v1::ResourceDescriptor::from)
                .collect();
            Ok(envelope::Payload::EnumerateSerialResponse(
                v1::EnumerateSerialResponse { resources },
            ))
        }
        Some(envelope::Payload::OpenSerialRequest(request)) => {
            let selector = request
                .selector
                .ok_or_else(|| invalid_message("open request is missing selector"))?
                .try_into()?;
            let config = request
                .config
                .ok_or_else(|| invalid_message("open request is missing config"))?
                .try_into()?;
            let handle = runtime.open_serial(owner, selector, config).await?;
            let (session_id, lease) = handle.into_parts();
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
            let (session, lease) = parse_session_lease(request.session_id, request.lease)?;
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
            let (session, lease) = parse_session_lease(request.session_id, request.lease)?;
            runtime
                .write_serial(session, &lease, Bytes::from(request.data))
                .await?;
            Ok(envelope::Payload::SerialWriteResponse(v1::Empty {}))
        }
        Some(envelope::Payload::SerialFlushRequest(request)) => {
            let (session, lease) = parse_session_lease(request.session_id, request.lease)?;
            runtime.flush_serial(session, &lease).await?;
            Ok(envelope::Payload::SerialFlushResponse(v1::Empty {}))
        }
        Some(envelope::Payload::SetSerialControlLinesRequest(request)) => {
            let (session, lease) = parse_session_lease(request.session_id, request.lease)?;
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
            let (session, lease) = parse_session_lease(request.session_id, request.lease)?;
            runtime.close_serial(session, &lease).await?;
            Ok(envelope::Payload::CloseSessionResponse(v1::Empty {}))
        }
        None => Err(invalid_message("envelope payload is required")),
        _ => Err(invalid_message(
            "response and event payloads are not valid client requests",
        )),
    }
}

fn send_response(responses: &mpsc::Sender<v1::Envelope>, response: v1::Envelope) -> HalResult<()> {
    responses.try_send(response).map_err(|error| match error {
        mpsc::error::TrySendError::Full(_) => queue_full("broker response queue is full"),
        mpsc::error::TrySendError::Closed(_) => protocol_error(
            "runtime.protocol.connection_lost",
            "runtime.protocol.write",
            ErrorCategory::Unavailable,
            true,
            "broker response channel is closed",
        ),
    })
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

fn join_error(
    result: Result<HalResult<()>, tokio::task::JoinError>,
    operation: &'static str,
) -> Option<HalError> {
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
