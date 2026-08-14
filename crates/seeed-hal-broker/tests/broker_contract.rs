use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Condvar, Mutex};
use std::task::{Context, Poll};

use bytes::Bytes;
use futures_util::{SinkExt, StreamExt};
use prost::Message;
use seeed_hal_broker::{Broker, BrokerConfig, StartupToken};
use seeed_hal_core::{HalResult, OwnerId, ResourceDescriptor, ResourceSelector};
use seeed_hal_protocol::v1::{self, envelope};
use seeed_hal_runtime::{HalRuntime, RuntimeEventKind};
use seeed_hal_serial::{ControlLines, SerialAdapter, SerialConfig, SerialSession};
use seeed_hal_testkit::VirtualSerialAdapter;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, ReadBuf};
use tokio::sync::{Notify, oneshot};
use tokio_util::codec::{Framed, LengthDelimitedCodec};

const TOKEN: [u8; 32] = [0xa5; 32];

async fn test_deadline<T>(
    future: impl std::future::Future<Output = T>,
    message: &'static str,
) -> T {
    tokio::time::timeout(std::time::Duration::from_secs(1), future)
        .await
        .expect(message)
}

#[derive(Clone, Default)]
struct BlockingWriteGate {
    state: Arc<BlockingWriteGateState>,
}

#[derive(Default)]
struct BlockingWriteGateState {
    armed: AtomicBool,
    blocked: AtomicBool,
    blocked_notify: Notify,
}

struct GatedIo<T> {
    inner: T,
    gate: BlockingWriteGate,
    target_request_id: u64,
}

#[derive(Clone)]
struct SecondEnumerateGateAdapter {
    inner: VirtualSerialAdapter,
    enumerate_calls: Arc<AtomicUsize>,
    enumerate_started: Arc<Notify>,
    enumerate_release: Arc<(Mutex<bool>, Condvar)>,
    operation_dropped: Arc<AtomicBool>,
    operation_dropped_notify: Arc<Notify>,
    close_after_operation_drop: Arc<AtomicBool>,
    close_started: Arc<Notify>,
}

impl SecondEnumerateGateAdapter {
    fn new(resource_id: &str) -> Self {
        Self {
            inner: VirtualSerialAdapter::loopback(resource_id),
            enumerate_calls: Arc::new(AtomicUsize::new(0)),
            enumerate_started: Arc::new(Notify::new()),
            enumerate_release: Arc::new((Mutex::new(false), Condvar::new())),
            operation_dropped: Arc::new(AtomicBool::new(false)),
            operation_dropped_notify: Arc::new(Notify::new()),
            close_after_operation_drop: Arc::new(AtomicBool::new(false)),
            close_started: Arc::new(Notify::new()),
        }
    }

    async fn wait_until_second_enumerate_started(&self) {
        test_deadline(
            self.enumerate_started.notified(),
            "second enumerate operation must start",
        )
        .await;
    }

    fn release_second_enumerate(&self) {
        let (released, condvar) = &*self.enumerate_release;
        *released.lock().unwrap() = true;
        condvar.notify_one();
    }

    async fn wait_until_close_started(&self) {
        test_deadline(
            self.close_started.notified(),
            "owner revoke must start session closure",
        )
        .await;
    }

    async fn wait_until_operation_dropped(&self) {
        loop {
            let notified = self.operation_dropped_notify.notified();
            if self.operation_dropped.load(Ordering::Acquire) {
                return;
            }
            test_deadline(notified, "in-flight operation future must be joined").await;
        }
    }
}

struct OperationDropGuard {
    dropped: Arc<AtomicBool>,
    dropped_notify: Arc<Notify>,
}

impl Drop for OperationDropGuard {
    fn drop(&mut self) {
        self.dropped.store(true, Ordering::Release);
        self.dropped_notify.notify_one();
    }
}

#[async_trait::async_trait]
impl SerialAdapter for SecondEnumerateGateAdapter {
    fn adapter_name(&self) -> &'static str {
        "virtual.serial.second-enumerate-gate"
    }

    async fn enumerate(&self) -> HalResult<Vec<ResourceDescriptor>> {
        if self.enumerate_calls.fetch_add(1, Ordering::AcqRel) == 1 {
            let _drop_guard = OperationDropGuard {
                dropped: self.operation_dropped.clone(),
                dropped_notify: self.operation_dropped_notify.clone(),
            };
            self.enumerate_started.notify_one();
            {
                let (released, condvar) = &*self.enumerate_release;
                let mut released = released.lock().unwrap();
                while !*released {
                    released = condvar.wait(released).unwrap();
                }
            }
            self.inner.enumerate().await
        } else {
            self.inner.enumerate().await
        }
    }

    async fn open(
        &self,
        selector: &ResourceSelector,
        config: SerialConfig,
    ) -> HalResult<Box<dyn SerialSession>> {
        let inner = self.inner.open(selector, config).await?;
        Ok(Box::new(CloseOrderSession {
            inner,
            operation_dropped: self.operation_dropped.clone(),
            close_after_operation_drop: self.close_after_operation_drop.clone(),
            close_started: self.close_started.clone(),
        }))
    }
}

struct CloseOrderSession {
    inner: Box<dyn SerialSession>,
    operation_dropped: Arc<AtomicBool>,
    close_after_operation_drop: Arc<AtomicBool>,
    close_started: Arc<Notify>,
}

#[async_trait::async_trait]
impl SerialSession for CloseOrderSession {
    fn descriptor(&self) -> &ResourceDescriptor {
        self.inner.descriptor()
    }

    async fn read(&mut self, max_bytes: usize) -> HalResult<Bytes> {
        self.inner.read(max_bytes).await
    }

    async fn write_all(&mut self, bytes: &[u8]) -> HalResult<()> {
        self.inner.write_all(bytes).await
    }

    async fn flush(&mut self) -> HalResult<()> {
        self.inner.flush().await
    }

    async fn set_control_lines(&mut self, lines: ControlLines) -> HalResult<()> {
        self.inner.set_control_lines(lines).await
    }

    async fn close(&mut self) -> HalResult<()> {
        self.close_started.notify_one();
        self.close_after_operation_drop.store(
            self.operation_dropped.load(Ordering::Acquire),
            Ordering::Release,
        );
        self.inner.close().await
    }
}

impl BlockingWriteGate {
    fn wrap<T>(inner: T, target_request_id: u64) -> (Self, GatedIo<T>) {
        let gate = Self::default();
        (
            gate.clone(),
            GatedIo {
                inner,
                gate,
                target_request_id,
            },
        )
    }

    fn arm(&self) {
        self.state.armed.store(true, Ordering::Release);
    }

    async fn wait_until_blocked(&self) {
        loop {
            let notified = self.state.blocked_notify.notified();
            if self.state.blocked.load(Ordering::Acquire) {
                return;
            }
            notified.await;
        }
    }
}

impl<T> AsyncRead for GatedIo<T>
where
    T: AsyncRead + Unpin,
{
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.inner).poll_read(cx, buf)
    }
}

impl<T> AsyncWrite for GatedIo<T>
where
    T: AsyncWrite + Unpin,
{
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<Result<usize, std::io::Error>> {
        if self.gate.state.armed.load(Ordering::Acquire)
            && framed_buffer_contains_request_id(buf, self.target_request_id)
        {
            if !self.gate.state.blocked.swap(true, Ordering::AcqRel) {
                self.gate.state.blocked_notify.notify_waiters();
            }
            return Poll::Pending;
        }
        Pin::new(&mut self.inner).poll_write(cx, buf)
    }

    fn poll_flush(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Result<(), std::io::Error>> {
        Pin::new(&mut self.inner).poll_flush(cx)
    }

    fn poll_shutdown(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Result<(), std::io::Error>> {
        Pin::new(&mut self.inner).poll_shutdown(cx)
    }
}

fn framed_buffer_contains_request_id(mut buf: &[u8], target_request_id: u64) -> bool {
    while buf.len() >= 4 {
        let frame_len = u32::from_be_bytes(buf[..4].try_into().unwrap()) as usize;
        let Some(frame_end) = 4_usize.checked_add(frame_len) else {
            return false;
        };
        let Some(frame) = buf.get(4..frame_end) else {
            return false;
        };
        if v1::Envelope::decode(frame)
            .is_ok_and(|envelope| envelope.request_id == target_request_id)
        {
            return true;
        }
        buf = &buf[frame_end..];
    }
    false
}

struct Client<T> {
    framed: Framed<T, LengthDelimitedCodec>,
    next_request_id: u64,
}

impl<T> Client<T>
where
    T: AsyncRead + AsyncWrite + Unpin,
{
    fn new(io: T) -> Self {
        Self {
            framed: Framed::new(io, codec()),
            next_request_id: 1,
        }
    }

    async fn request(&mut self, payload: envelope::Payload) -> v1::Envelope {
        let request_id = self.next_request_id;
        self.next_request_id += 1;
        self.send(request_id, payload).await;
        loop {
            let response = self.recv().await;
            if response.request_id == request_id {
                return response;
            }
            assert_eq!(
                response.request_id, 0,
                "only runtime events are unsolicited"
            );
        }
    }

    async fn send(&mut self, request_id: u64, payload: envelope::Payload) {
        self.try_send(request_id, payload).await.unwrap();
    }

    async fn try_send(
        &mut self,
        request_id: u64,
        payload: envelope::Payload,
    ) -> std::io::Result<()> {
        let encoded = v1::Envelope {
            request_id,
            payload: Some(payload),
        }
        .encode_to_vec();
        self.framed.send(Bytes::from(encoded)).await
    }

    async fn recv(&mut self) -> v1::Envelope {
        let frame = test_deadline(
            self.framed.next(),
            "test client must receive the next frame",
        )
        .await
        .unwrap()
        .unwrap();
        v1::Envelope::decode(frame).unwrap()
    }

    async fn handshake(&mut self) {
        let response = self
            .request(envelope::Payload::HandshakeRequest(valid_handshake(
                TOKEN.to_vec(),
            )))
            .await;
        assert!(matches!(
            response.payload,
            Some(envelope::Payload::HandshakeResponse(_))
        ));
    }
}

fn codec() -> LengthDelimitedCodec {
    LengthDelimitedCodec::builder()
        .max_frame_length(seeed_hal_protocol::MAX_FRAME_BYTES)
        .new_codec()
}

fn valid_handshake(token: Vec<u8>) -> v1::HandshakeRequest {
    v1::HandshakeRequest {
        startup_token: token,
        protocol_major: 1,
        protocol_minor: 0,
        required_capabilities: vec!["serial.bytes/v1".to_owned()],
        max_frame_bytes: 1024 * 1024,
        max_read_bytes: 64 * 1024,
        max_write_bytes: 64 * 1024,
        protocol_minor_minimum: 0,
        protocol_minor_maximum: 0,
    }
}

fn small_frame_handshake() -> v1::HandshakeRequest {
    let mut handshake = valid_handshake(TOKEN.to_vec());
    handshake.max_frame_bytes = 110;
    handshake.max_read_bytes = 1;
    handshake.max_write_bytes = 1;
    handshake
}

fn broker() -> Broker {
    let runtime = HalRuntime::builder()
        .serial_adapter(VirtualSerialAdapter::loopback("serial:virtual:broker"))
        .build();
    Broker::with_startup_token(runtime, StartupToken::from_bytes(TOKEN))
}

fn runtime() -> HalRuntime {
    HalRuntime::builder()
        .serial_adapter(VirtualSerialAdapter::loopback("serial:virtual:broker"))
        .build()
}

fn serial_config(read_timeout_ms: u64) -> v1::SerialConfig {
    v1::SerialConfig {
        baud_rate: 115_200,
        data_bits: v1::DataBits::Eight as i32,
        parity: v1::Parity::None as i32,
        stop_bits: v1::StopBits::One as i32,
        flow_control: v1::FlowControl::None as i32,
        read_timeout_ms,
    }
}

async fn open_virtual_serial<T>(
    client: &mut Client<T>,
    read_timeout_ms: u64,
) -> v1::OpenSerialResponse
where
    T: AsyncRead + AsyncWrite + Unpin,
{
    let enumerate = client
        .request(envelope::Payload::EnumerateSerialRequest(
            v1::EnumerateSerialRequest {},
        ))
        .await;
    let descriptor = match enumerate.payload.unwrap() {
        envelope::Payload::EnumerateSerialResponse(response) => {
            response.resources.into_iter().next().unwrap()
        }
        _ => panic!("expected enumerate response"),
    };
    let response = client
        .request(envelope::Payload::OpenSerialRequest(
            v1::OpenSerialRequest {
                selector: Some(v1::ResourceSelector {
                    resource_id: descriptor.resource_id,
                    minimum_identity_quality: descriptor.identity_quality,
                    transport: descriptor.transport,
                }),
                config: Some(serial_config(read_timeout_ms)),
            },
        ))
        .await;
    match response.payload.unwrap() {
        envelope::Payload::OpenSerialResponse(response) => response,
        payload => panic!("expected open response, got {payload:?}"),
    }
}

fn error_name(envelope: &v1::Envelope) -> &str {
    match envelope.payload.as_ref() {
        Some(envelope::Payload::Error(error)) => &error.name,
        _ => panic!("expected an error envelope"),
    }
}

async fn rejected_handshake(request: v1::HandshakeRequest) -> String {
    let (server_io, client_io) = tokio::io::duplex(16 * 1024);
    let broker = broker();
    let server = tokio::spawn(async move { broker.serve_connection(server_io).await });
    let mut client = Client::new(client_io);
    let response = client
        .request(envelope::Payload::HandshakeRequest(request))
        .await;
    let name = error_name(&response).to_owned();
    drop(client);
    assert!(server.await.unwrap().cleanup_error().is_none());
    name
}

#[tokio::test]
async fn broker_rejects_operations_before_handshake() {
    let (server_io, client_io) = tokio::io::duplex(16 * 1024);
    let broker = broker();
    let server = tokio::spawn(async move { broker.serve_connection(server_io).await });
    let mut client = Client::new(client_io);

    let response = client
        .request(envelope::Payload::EnumerateSerialRequest(
            v1::EnumerateSerialRequest {},
        ))
        .await;
    assert_eq!(error_name(&response), "runtime.protocol.handshake_required");

    drop(client);
    assert!(server.await.unwrap().cleanup_error().is_none());
}

#[tokio::test]
async fn invalid_startup_token_fails_closed() {
    let (server_io, client_io) = tokio::io::duplex(16 * 1024);
    let broker = broker();
    let server = tokio::spawn(async move { broker.serve_connection(server_io).await });
    let mut client = Client::new(client_io);

    let response = client
        .request(envelope::Payload::HandshakeRequest(valid_handshake(vec![
            0;
            32
        ])))
        .await;
    assert_eq!(
        error_name(&response),
        "runtime.protocol.authentication_failed"
    );

    drop(client);
    assert!(server.await.unwrap().cleanup_error().is_none());
}

#[tokio::test]
async fn handshake_version_capability_and_byte_limits_fail_closed() {
    let mut wrong_version = valid_handshake(TOKEN.to_vec());
    wrong_version.protocol_major = 2;
    assert_eq!(
        rejected_handshake(wrong_version).await,
        "runtime.protocol.version_incompatible"
    );

    let mut unsupported_capability = valid_handshake(TOKEN.to_vec());
    unsupported_capability.required_capabilities = vec!["can.fd/v1".to_owned()];
    assert_eq!(
        rejected_handshake(unsupported_capability).await,
        "runtime.protocol.unsupported_capability"
    );

    let mut oversized_frame = valid_handshake(TOKEN.to_vec());
    oversized_frame.max_frame_bytes = (seeed_hal_protocol::MAX_FRAME_BYTES + 1) as u32;
    assert_eq!(
        rejected_handshake(oversized_frame).await,
        "runtime.protocol.invalid_message"
    );

    let mut missing_envelope_overhead = valid_handshake(TOKEN.to_vec());
    missing_envelope_overhead.max_frame_bytes = 128;
    missing_envelope_overhead.max_read_bytes = 128;
    missing_envelope_overhead.max_write_bytes = 1;
    assert_eq!(
        rejected_handshake(missing_envelope_overhead).await,
        "runtime.protocol.invalid_message"
    );
}

#[tokio::test]
async fn broker_selects_highest_shared_minor_and_reports_its_supported_range() {
    let (server_io, client_io) = tokio::io::duplex(16 * 1024);
    let broker = broker();
    let server = tokio::spawn(async move { broker.serve_connection(server_io).await });
    let mut client = Client::new(client_io);
    let mut handshake = valid_handshake(TOKEN.to_vec());
    handshake.protocol_minor = 3;
    handshake.protocol_minor_minimum = 0;
    handshake.protocol_minor_maximum = 3;

    let response = client
        .request(envelope::Payload::HandshakeRequest(handshake))
        .await;
    let accepted = match response.payload {
        Some(envelope::Payload::HandshakeResponse(response)) => response,
        other => panic!("expected handshake response, got {other:?}"),
    };
    assert_eq!(accepted.protocol_minor, 0);
    assert_eq!(accepted.protocol_minor_minimum, 0);
    assert_eq!(accepted.protocol_minor_maximum, 0);

    drop(client);
    assert!(server.await.unwrap().cleanup_error().is_none());
}

#[tokio::test]
async fn negotiated_frame_limit_rejects_oversized_raw_inbound_frame() {
    let (server_io, client_io) = tokio::io::duplex(16 * 1024);
    let broker = broker();
    let server = tokio::spawn(async move { broker.serve_connection(server_io).await });
    let mut client = Client::new(client_io);
    let mut handshake = valid_handshake(TOKEN.to_vec());
    handshake.max_frame_bytes = 256;
    handshake.max_read_bytes = 64;
    handshake.max_write_bytes = 1;
    let response = client
        .request(envelope::Payload::HandshakeRequest(handshake))
        .await;
    assert!(matches!(
        response.payload,
        Some(envelope::Payload::HandshakeResponse(_))
    ));

    client
        .send(
            40,
            envelope::Payload::SerialFlushRequest(v1::SerialFlushRequest {
                session_id: "s".repeat(255),
                lease: Some(v1::LeaseToken {
                    lease_id: "l".repeat(255),
                    generation: 1,
                    mode: v1::LeaseMode::Control as i32,
                }),
            }),
        )
        .await;

    let next = tokio::time::timeout(std::time::Duration::from_secs(1), client.framed.next())
        .await
        .expect("negotiated frame violation must close the connection");
    assert!(
        next.is_none(),
        "broker must initiate EOF without dispatching"
    );
    let outcome = server.await.unwrap();
    assert_eq!(
        outcome.connection_error().unwrap().name().as_str(),
        "runtime.protocol.frame_too_large"
    );
}

#[tokio::test]
async fn pipelined_handshake_then_oversized_frame_uses_negotiated_limit() {
    let runtime = runtime();
    let broker = Broker::with_config(
        runtime,
        StartupToken::from_bytes(TOKEN),
        BrokerConfig::default().with_request_queue_capacity(1),
    );
    let (server_io, client_io) = tokio::io::duplex(16 * 1024);
    let mut client = Client::new(client_io);
    client
        .send(
            1,
            envelope::Payload::HandshakeRequest(small_frame_handshake()),
        )
        .await;
    client
        .send(
            2,
            envelope::Payload::SerialFlushRequest(v1::SerialFlushRequest {
                session_id: "s".repeat(255),
                lease: Some(v1::LeaseToken {
                    lease_id: "l".repeat(255),
                    generation: 1,
                    mode: v1::LeaseMode::Control as i32,
                }),
            }),
        )
        .await;
    let server = tokio::spawn(async move { broker.serve_connection(server_io).await });

    loop {
        let next = tokio::time::timeout(std::time::Duration::from_secs(1), client.framed.next())
            .await
            .expect("pipelined negotiated-frame violation must terminate");
        match next {
            Some(Ok(frame)) => assert!(frame.len() <= 110),
            Some(Err(error)) => panic!("client framing failed: {error}"),
            None => break,
        }
    }
    let outcome = server.await.unwrap();
    assert!(outcome.cleanup_error().is_none());
    assert_eq!(
        outcome.connection_error().unwrap().name().as_str(),
        "runtime.protocol.frame_too_large"
    );
}

#[tokio::test]
async fn pipelined_zero_id_error_never_uses_pre_handshake_frame_limit() {
    let broker = Broker::with_startup_token(runtime(), StartupToken::from_bytes(TOKEN));
    let (server_io, client_io) = tokio::io::duplex(16 * 1024);
    let mut client = Client::new(client_io);
    let mut handshake = small_frame_handshake();
    handshake.max_frame_bytes = 108;
    client
        .send(1, envelope::Payload::HandshakeRequest(handshake))
        .await;
    client
        .send(
            0,
            envelope::Payload::EnumerateSerialRequest(v1::EnumerateSerialRequest {}),
        )
        .await;
    let server = tokio::spawn(async move { broker.serve_connection(server_io).await });

    let mut observed_frames = 0;
    for _ in 0..2 {
        match tokio::time::timeout(std::time::Duration::from_millis(200), client.framed.next())
            .await
        {
            Ok(Some(Ok(frame))) => {
                assert!(frame.len() <= 108, "outbound frame violated negotiation");
                observed_frames += 1;
            }
            Ok(Some(Err(error))) => panic!("client framing failed: {error}"),
            Ok(None) | Err(_) => break,
        }
    }
    assert!(
        observed_frames >= 1,
        "handshake response must be observable"
    );

    drop(client);
    assert!(server.await.unwrap().cleanup_error().is_none());
}

#[tokio::test]
async fn pipelined_duplicate_id_error_uses_negotiated_frame_limit() {
    let broker = Broker::with_startup_token(runtime(), StartupToken::from_bytes(TOKEN));
    let (server_io, client_io) = tokio::io::duplex(16 * 1024);
    let mut client = Client::new(client_io);
    let mut handshake = small_frame_handshake();
    handshake.max_frame_bytes = 108;
    client
        .send(1, envelope::Payload::HandshakeRequest(handshake))
        .await;
    for _ in 0..2 {
        client
            .send(
                2,
                envelope::Payload::EnumerateSerialRequest(v1::EnumerateSerialRequest {}),
            )
            .await;
    }
    let server = tokio::spawn(async move { broker.serve_connection(server_io).await });

    loop {
        let next = tokio::time::timeout(std::time::Duration::from_secs(1), client.framed.next())
            .await
            .expect("pipelined duplicate IDs must terminate the connection");
        match next {
            Some(Ok(frame)) => assert!(frame.len() <= 108, "outbound frame violated negotiation"),
            Some(Err(error)) => panic!("client framing failed: {error}"),
            None => break,
        }
    }
    let outcome = server.await.unwrap();
    assert!(outcome.cleanup_error().is_none());
}

#[tokio::test]
async fn pipelined_request_queue_pressure_uses_negotiated_frame_limit() {
    let broker = Broker::with_config(
        runtime(),
        StartupToken::from_bytes(TOKEN),
        BrokerConfig::default().with_request_queue_capacity(1),
    );
    let (server_io, client_io) = tokio::io::duplex(16 * 1024);
    let mut client = Client::new(client_io);
    let mut handshake = small_frame_handshake();
    handshake.max_frame_bytes = 108;
    client
        .send(1, envelope::Payload::HandshakeRequest(handshake))
        .await;
    for request_id in 2..=4 {
        client
            .send(
                request_id,
                envelope::Payload::EnumerateSerialRequest(v1::EnumerateSerialRequest {}),
            )
            .await;
    }
    let server = tokio::spawn(async move { broker.serve_connection(server_io).await });

    loop {
        let next = tokio::time::timeout(std::time::Duration::from_secs(1), client.framed.next())
            .await
            .expect("pipelined request pressure must terminate the connection");
        match next {
            Some(Ok(frame)) => assert!(frame.len() <= 108, "outbound frame violated negotiation"),
            Some(Err(error)) => panic!("client framing failed: {error}"),
            None => break,
        }
    }
    let outcome = server.await.unwrap();
    assert!(outcome.cleanup_error().is_none());
    assert_eq!(
        outcome.connection_error().unwrap().name().as_str(),
        "runtime.protocol.frame_too_large"
    );
}

#[tokio::test]
async fn handshake_frame_length_prefix_over_hard_cap_fails_before_decode() {
    let broker = Broker::with_startup_token(runtime(), StartupToken::from_bytes(TOKEN));
    let (server_io, mut client_io) = tokio::io::duplex(16);
    let oversized_len = u32::try_from(seeed_hal_protocol::MAX_FRAME_BYTES + 1).unwrap();
    client_io
        .write_all(&oversized_len.to_be_bytes())
        .await
        .unwrap();
    let server = tokio::spawn(async move { broker.serve_connection(server_io).await });

    let mut byte = [0_u8; 1];
    let read = tokio::time::timeout(std::time::Duration::from_secs(1), client_io.read(&mut byte))
        .await
        .expect("oversized handshake prefix must terminate the connection")
        .unwrap();
    assert_eq!(read, 0, "broker must initiate EOF before reading the body");

    let outcome = server.await.unwrap();
    assert!(outcome.cleanup_error().is_none());
    assert_eq!(
        outcome.connection_error().unwrap().name().as_str(),
        "runtime.protocol.frame_invalid"
    );
}

#[tokio::test]
async fn negotiated_frame_limit_rejects_oversized_outbound_before_encoding() {
    let (server_io, client_io) = tokio::io::duplex(16 * 1024);
    let broker = broker();
    let server = tokio::spawn(async move { broker.serve_connection(server_io).await });
    let mut client = Client::new(client_io);
    let mut handshake = valid_handshake(TOKEN.to_vec());
    handshake.max_frame_bytes = 110;
    handshake.max_read_bytes = 1;
    handshake.max_write_bytes = 1;
    let response = client
        .request(envelope::Payload::HandshakeRequest(handshake))
        .await;
    assert!(matches!(
        response.payload,
        Some(envelope::Payload::HandshakeResponse(_))
    ));

    client
        .send(
            41,
            envelope::Payload::EnumerateSerialRequest(v1::EnumerateSerialRequest {}),
        )
        .await;
    let next = tokio::time::timeout(std::time::Duration::from_secs(1), client.framed.next())
        .await
        .expect("oversized outbound envelope must close the connection");
    assert!(
        next.is_none(),
        "broker must not write an oversized envelope: observed {next:?}"
    );
    let outcome = server.await.unwrap();
    assert_eq!(
        outcome.connection_error().unwrap().name().as_str(),
        "runtime.protocol.frame_too_large"
    );
}

#[tokio::test]
async fn virtual_serial_supports_the_complete_v1_operation_set() {
    let (server_io, client_io) = tokio::io::duplex(64 * 1024);
    let broker = broker();
    let server = tokio::spawn(async move { broker.serve_connection(server_io).await });
    let mut client = Client::new(client_io);
    client.handshake().await;
    let session = open_virtual_serial(&mut client, 1_000).await;
    let lease = session.lease.clone();

    let write = client
        .request(envelope::Payload::SerialWriteRequest(
            v1::SerialWriteRequest {
                session_id: session.session_id.clone(),
                lease: lease.clone(),
                data: b"loopback".to_vec(),
            },
        ))
        .await;
    assert!(matches!(
        write.payload,
        Some(envelope::Payload::SerialWriteResponse(_))
    ));

    let flush = client
        .request(envelope::Payload::SerialFlushRequest(
            v1::SerialFlushRequest {
                session_id: session.session_id.clone(),
                lease: lease.clone(),
            },
        ))
        .await;
    assert!(matches!(
        flush.payload,
        Some(envelope::Payload::SerialFlushResponse(_))
    ));

    let control = client
        .request(envelope::Payload::SetSerialControlLinesRequest(
            v1::SetSerialControlLinesRequest {
                session_id: session.session_id.clone(),
                lease: lease.clone(),
                data_terminal_ready: true,
                request_to_send: true,
            },
        ))
        .await;
    assert!(matches!(
        control.payload,
        Some(envelope::Payload::SetSerialControlLinesResponse(_))
    ));

    let read = client
        .request(envelope::Payload::SerialReadRequest(
            v1::SerialReadRequest {
                session_id: session.session_id.clone(),
                lease: lease.clone(),
                max_bytes: 64,
            },
        ))
        .await;
    match read.payload.unwrap() {
        envelope::Payload::SerialReadResponse(response) => {
            assert_eq!(response.data, b"loopback")
        }
        _ => panic!("expected read response"),
    }

    let close = client
        .request(envelope::Payload::CloseSessionRequest(
            v1::CloseSessionRequest {
                session_id: session.session_id,
                lease,
            },
        ))
        .await;
    assert!(matches!(
        close.payload,
        Some(envelope::Payload::CloseSessionResponse(_))
    ));

    drop(client);
    assert!(server.await.unwrap().cleanup_error().is_none());
}

#[tokio::test]
async fn runtime_events_are_forwarded_as_unsolicited_envelopes() {
    let (server_io, client_io) = tokio::io::duplex(64 * 1024);
    let broker = broker();
    let server = tokio::spawn(async move { broker.serve_connection(server_io).await });
    let mut client = Client::new(client_io);
    client.handshake().await;
    let enumerate = client
        .request(envelope::Payload::EnumerateSerialRequest(
            v1::EnumerateSerialRequest {},
        ))
        .await;
    let descriptor = match enumerate.payload.unwrap() {
        envelope::Payload::EnumerateSerialResponse(response) => {
            response.resources.into_iter().next().unwrap()
        }
        _ => panic!("expected enumerate response"),
    };
    client
        .send(
            50,
            envelope::Payload::OpenSerialRequest(v1::OpenSerialRequest {
                selector: Some(v1::ResourceSelector {
                    resource_id: descriptor.resource_id,
                    minimum_identity_quality: descriptor.identity_quality,
                    transport: descriptor.transport,
                }),
                config: Some(serial_config(1_000)),
            }),
        )
        .await;

    let mut saw_response = false;
    let mut saw_event = false;
    while !saw_response || !saw_event {
        let envelope = client.recv().await;
        match envelope.payload.unwrap() {
            envelope::Payload::OpenSerialResponse(_) => {
                assert_eq!(envelope.request_id, 50);
                saw_response = true;
            }
            envelope::Payload::RuntimeEvent(event) => {
                assert_eq!(envelope.request_id, 0);
                assert_eq!(event.name, "session.opened");
                saw_event = true;
            }
            _ => panic!("unexpected payload while opening serial"),
        }
    }

    drop(client);
    assert!(server.await.unwrap().cleanup_error().is_none());
}

#[tokio::test]
async fn runtime_events_are_filtered_to_the_connection_owner() {
    let runtime = runtime();
    let broker = Broker::with_startup_token(runtime, StartupToken::from_bytes(TOKEN));
    let (server_one_io, client_one_io) = tokio::io::duplex(64 * 1024);
    let (server_two_io, client_two_io) = tokio::io::duplex(64 * 1024);
    let server_one_broker = broker.clone();
    let server_one =
        tokio::spawn(async move { server_one_broker.serve_connection(server_one_io).await });
    let server_two = tokio::spawn(async move { broker.serve_connection(server_two_io).await });
    let mut client_one = Client::new(client_one_io);
    let mut client_two = Client::new(client_two_io);
    client_one.handshake().await;
    client_two.handshake().await;

    let _session = open_virtual_serial(&mut client_one, 1_000).await;
    let leaked = tokio::time::timeout(
        std::time::Duration::from_millis(50),
        client_two.framed.next(),
    )
    .await;
    assert!(
        leaked.is_err(),
        "a connection must not observe another owner's runtime event"
    );

    drop(client_one);
    drop(client_two);
    assert!(server_one.await.unwrap().cleanup_error().is_none());
    assert!(server_two.await.unwrap().cleanup_error().is_none());
}

#[tokio::test]
async fn runtime_event_queue_lag_is_reported_structurally() {
    let runtime = runtime();
    let broker = Broker::with_startup_token(runtime.clone(), StartupToken::from_bytes(TOKEN));
    let (server_io, client_io) = tokio::io::duplex(64 * 1024);
    let server = tokio::spawn(async move { broker.serve_connection(server_io).await });
    let mut client = Client::new(client_io);

    let before_handshake = client
        .request(envelope::Payload::EnumerateSerialRequest(
            v1::EnumerateSerialRequest {},
        ))
        .await;
    assert_eq!(
        error_name(&before_handshake),
        "runtime.protocol.handshake_required"
    );

    let descriptor = runtime.enumerate_serial().await.unwrap().remove(0);
    for index in 0..33 {
        runtime
            .open_serial(
                OwnerId::parse(format!("broker-contract:event-lag-{index}")).unwrap(),
                descriptor.selector(),
                SerialConfig::default(),
            )
            .await
            .unwrap()
            .close()
            .await
            .unwrap();
    }

    client
        .send(
            500,
            envelope::Payload::HandshakeRequest(valid_handshake(TOKEN.to_vec())),
        )
        .await;
    let mut saw_handshake = false;
    let mut saw_lag = false;
    while !saw_handshake || !saw_lag {
        let response = client.recv().await;
        match response.payload.as_ref() {
            Some(envelope::Payload::HandshakeResponse(_)) => saw_handshake = true,
            Some(envelope::Payload::Error(error))
                if error.name == "runtime.event.lagged" && response.request_id == 0 =>
            {
                saw_lag = true;
            }
            _ => panic!("unexpected response while observing event lag"),
        }
    }

    drop(client);
    assert!(server.await.unwrap().cleanup_error().is_none());
}

#[tokio::test]
async fn disconnect_revokes_owned_sessions() {
    let runtime = runtime();
    let mut events = runtime.subscribe();
    let broker = Broker::with_startup_token(runtime.clone(), StartupToken::from_bytes(TOKEN));
    let (server_io, client_io) = tokio::io::duplex(64 * 1024);
    let server = tokio::spawn(async move { broker.serve_connection(server_io).await });
    let mut client = Client::new(client_io);
    client.handshake().await;
    let session = open_virtual_serial(&mut client, 1_000).await;
    let expected_session_id = session.session_id;
    drop(client);

    let outcome = server.await.unwrap();
    assert!(outcome.cleanup_error().is_none());
    let mut closed = false;
    for _ in 0..2 {
        let event = test_deadline(events.recv(), "disconnect must publish runtime events")
            .await
            .unwrap();
        if event.kind() == RuntimeEventKind::SessionClosed
            && event.session_id().as_str() == expected_session_id
        {
            closed = true;
        }
    }
    assert!(
        closed,
        "disconnect must publish closure for the owned session"
    );

    let descriptor = runtime.enumerate_serial().await.unwrap().remove(0);
    runtime
        .open_serial(
            OwnerId::parse("broker-contract:next-owner").unwrap(),
            descriptor.selector(),
            SerialConfig::default(),
        )
        .await
        .unwrap()
        .close()
        .await
        .unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn shutdown_joins_in_flight_operations_before_revoking_owner() {
    let adapter = SecondEnumerateGateAdapter::new("serial:virtual:shutdown-order");
    let runtime = HalRuntime::builder()
        .serial_adapter(adapter.clone())
        .build();
    let mut events = runtime.subscribe();
    let broker = Broker::with_startup_token(runtime.clone(), StartupToken::from_bytes(TOKEN));
    let (server_io, client_io) = tokio::io::duplex(64 * 1024);
    let (shutdown_tx, shutdown_rx) = oneshot::channel();
    let (shutdown_observed_tx, shutdown_observed_rx) = oneshot::channel();
    let server = tokio::spawn(async move {
        broker
            .serve_connection_until(server_io, async {
                let _ = shutdown_rx.await;
                let _ = shutdown_observed_tx.send(());
            })
            .await
    });
    let mut client = Client::new(client_io);
    client.handshake().await;
    let session = open_virtual_serial(&mut client, 1_000).await;
    client
        .send(
            900,
            envelope::Payload::EnumerateSerialRequest(v1::EnumerateSerialRequest {}),
        )
        .await;
    adapter.wait_until_second_enumerate_started().await;

    shutdown_tx.send(()).unwrap();
    test_deadline(
        shutdown_observed_rx,
        "connection shutdown signal must be positively observed",
    )
    .await
    .unwrap();
    adapter.release_second_enumerate();
    adapter.wait_until_operation_dropped().await;
    adapter.wait_until_close_started().await;
    let outcome = test_deadline(
        server,
        "shutdown must join operations and finish connection tasks",
    )
    .await
    .unwrap();

    assert!(outcome.cleanup_error().is_none());
    assert!(
        adapter.operation_dropped.load(Ordering::Acquire),
        "the in-flight operation future must terminate"
    );
    assert!(
        adapter.close_after_operation_drop.load(Ordering::Acquire),
        "owner revoke must close sessions only after operation futures terminate"
    );

    let mut saw_closed = false;
    for _ in 0..2 {
        let event = test_deadline(events.recv(), "owner revoke must publish runtime events")
            .await
            .unwrap();
        if event.kind() == RuntimeEventKind::SessionClosed
            && event.session_id().as_str() == session.session_id
        {
            saw_closed = true;
        }
    }
    assert!(saw_closed, "owner revoke must publish session closure");

    let descriptor = runtime.enumerate_serial().await.unwrap().remove(0);
    runtime
        .open_serial(
            OwnerId::parse("broker-contract:shutdown-order-reuse").unwrap(),
            descriptor.selector(),
            SerialConfig::default(),
        )
        .await
        .unwrap()
        .close()
        .await
        .unwrap();
}

#[tokio::test]
async fn stalled_writer_cannot_delay_owner_revoke_or_resource_reuse() {
    let runtime = runtime();
    let broker = Broker::with_config(
        runtime.clone(),
        StartupToken::from_bytes(TOKEN),
        BrokerConfig::default()
            .with_request_queue_capacity(2_048)
            .with_response_queue_capacity(512)
            .with_max_in_flight_requests(2_048),
    );
    let (server_io, client_io) = tokio::io::duplex(16 * 1024);
    let server = tokio::spawn(async move { broker.serve_connection(server_io).await });
    let mut client = Client::new(client_io);
    client.handshake().await;
    let _session = open_virtual_serial(&mut client, 1_000).await;

    for request_id in 1_000..4_000 {
        if client
            .try_send(
                request_id,
                envelope::Payload::EnumerateSerialRequest(v1::EnumerateSerialRequest {}),
            )
            .await
            .is_err()
        {
            break;
        }
    }

    let outcome = tokio::time::timeout(std::time::Duration::from_secs(1), server)
        .await
        .expect("stalled output must not block connection teardown")
        .unwrap();
    assert!(outcome.cleanup_error().is_none());
    assert_eq!(
        outcome.connection_error().unwrap().name().as_str(),
        "runtime.queue.response_full"
    );

    let descriptor = runtime.enumerate_serial().await.unwrap().remove(0);
    runtime
        .open_serial(
            OwnerId::parse("broker-contract:stalled-writer-reuse").unwrap(),
            descriptor.selector(),
            SerialConfig::default(),
        )
        .await
        .unwrap()
        .close()
        .await
        .unwrap();

    drop(client);
}

#[tokio::test]
async fn response_queue_overflow_is_isolated_structured_and_cleans_up_owner() {
    const REQUEST_QUEUE_CAPACITY: usize = 8;
    const TASK_CAPACITY: usize = 8;
    const RESPONSE_QUEUE_CAPACITY: usize = 2;
    const BLOCKING_REQUEST_ID: u64 = 600;
    const FOLLOW_UP_REQUEST_IDS: [u64; 3] = [601, 602, 603];

    assert!(FOLLOW_UP_REQUEST_IDS.len() < REQUEST_QUEUE_CAPACITY);
    assert!(FOLLOW_UP_REQUEST_IDS.len() < TASK_CAPACITY);

    let runtime = runtime();
    let broker = Broker::with_config(
        runtime.clone(),
        StartupToken::from_bytes(TOKEN),
        BrokerConfig::default()
            .with_request_queue_capacity(REQUEST_QUEUE_CAPACITY)
            .with_response_queue_capacity(RESPONSE_QUEUE_CAPACITY)
            .with_max_in_flight_requests(TASK_CAPACITY),
    );
    let (server_io, client_io) = tokio::io::duplex(512);
    let (write_gate, server_io) = BlockingWriteGate::wrap(server_io, BLOCKING_REQUEST_ID);
    let server = tokio::spawn(async move { broker.serve_connection(server_io).await });
    let mut client = Client::new(client_io);
    client.handshake().await;
    let session = open_virtual_serial(&mut client, 1_000).await;

    let write = client
        .request(envelope::Payload::SerialWriteRequest(
            v1::SerialWriteRequest {
                session_id: session.session_id.clone(),
                lease: session.lease.clone(),
                data: vec![0x5a; 4_096],
            },
        ))
        .await;
    assert!(matches!(
        write.payload,
        Some(envelope::Payload::SerialWriteResponse(_))
    ));

    write_gate.arm();
    client
        .send(
            BLOCKING_REQUEST_ID,
            envelope::Payload::SerialReadRequest(v1::SerialReadRequest {
                session_id: session.session_id,
                lease: session.lease,
                max_bytes: 4_096,
            }),
        )
        .await;
    tokio::time::timeout(
        std::time::Duration::from_secs(1),
        write_gate.wait_until_blocked(),
    )
    .await
    .expect("writer must consume and block on the designated read response");

    for request_id in FOLLOW_UP_REQUEST_IDS {
        client
            .send(
                request_id,
                envelope::Payload::EnumerateSerialRequest(v1::EnumerateSerialRequest {}),
            )
            .await;
    }

    let outcome = tokio::time::timeout(std::time::Duration::from_secs(1), server)
        .await
        .expect("response queue overflow must tear down a stalled connection")
        .unwrap();
    assert!(outcome.cleanup_error().is_none());
    assert_eq!(
        outcome.connection_error().unwrap().name().as_str(),
        "runtime.queue.response_full"
    );

    let eof = tokio::time::timeout(std::time::Duration::from_secs(1), client.framed.next())
        .await
        .expect("response overflow must close the connection");
    assert!(eof.is_none(), "broker must initiate EOF after overflow");

    let descriptor = runtime.enumerate_serial().await.unwrap().remove(0);
    runtime
        .open_serial(
            OwnerId::parse("broker-contract:response-overflow-reuse").unwrap(),
            descriptor.selector(),
            SerialConfig::default(),
        )
        .await
        .unwrap()
        .close()
        .await
        .unwrap();
    drop(client);
}

#[tokio::test]
async fn task_queue_overflow_is_deterministic_and_structured() {
    let runtime = runtime();
    let broker = Broker::with_config(
        runtime,
        StartupToken::from_bytes(TOKEN),
        BrokerConfig::default().with_max_in_flight_requests(1),
    );
    let (server_io, client_io) = tokio::io::duplex(64 * 1024);
    let server = tokio::spawn(async move { broker.serve_connection(server_io).await });
    let mut client = Client::new(client_io);
    client.handshake().await;
    let session = open_virtual_serial(&mut client, 5_000).await;
    let request = v1::SerialReadRequest {
        session_id: session.session_id,
        lease: session.lease,
        max_bytes: 1,
    };

    client
        .send(100, envelope::Payload::SerialReadRequest(request.clone()))
        .await;
    client
        .send(101, envelope::Payload::SerialReadRequest(request))
        .await;

    loop {
        let response = client.recv().await;
        if response.request_id == 101 {
            assert_eq!(error_name(&response), "runtime.queue.full");
            break;
        }
    }
    drop(client);
    assert!(server.await.unwrap().cleanup_error().is_none());
}

#[tokio::test]
async fn request_queue_overflow_is_deterministic_and_structured() {
    let runtime = runtime();
    let broker = Broker::with_config(
        runtime,
        StartupToken::from_bytes(TOKEN),
        BrokerConfig::default().with_request_queue_capacity(1),
    );
    let (server_io, client_io) = tokio::io::duplex(16 * 1024);
    let mut client = Client::new(client_io);
    client
        .send(
            1,
            envelope::Payload::HandshakeRequest(valid_handshake(TOKEN.to_vec())),
        )
        .await;
    client
        .send(
            2,
            envelope::Payload::EnumerateSerialRequest(v1::EnumerateSerialRequest {}),
        )
        .await;
    client
        .send(
            3,
            envelope::Payload::EnumerateSerialRequest(v1::EnumerateSerialRequest {}),
        )
        .await;
    let server = tokio::spawn(async move { broker.serve_connection(server_io).await });

    let mut saw_queue_full = false;
    for _ in 0..3 {
        let response = client.recv().await;
        if matches!(response.payload, Some(envelope::Payload::Error(_)))
            && error_name(&response) == "runtime.queue.full"
        {
            saw_queue_full = true;
            break;
        }
    }
    assert!(
        saw_queue_full,
        "the bounded request queue must reject overflow"
    );
    drop(client);
    assert!(server.await.unwrap().cleanup_error().is_none());
}

#[tokio::test]
async fn duplicate_in_flight_request_ids_fail_closed() {
    let (server_io, client_io) = tokio::io::duplex(64 * 1024);
    let runtime = runtime();
    let broker = Broker::with_startup_token(runtime.clone(), StartupToken::from_bytes(TOKEN));
    let server = tokio::spawn(async move { broker.serve_connection(server_io).await });
    let mut client = Client::new(client_io);
    client.handshake().await;
    let session = open_virtual_serial(&mut client, 5_000).await;
    let request = v1::SerialReadRequest {
        session_id: session.session_id,
        lease: session.lease,
        max_bytes: 1,
    };

    client
        .send(200, envelope::Payload::SerialReadRequest(request.clone()))
        .await;
    client
        .send(200, envelope::Payload::SerialReadRequest(request))
        .await;
    loop {
        let response = client.recv().await;
        if response.request_id == 200
            && matches!(response.payload, Some(envelope::Payload::Error(_)))
        {
            assert_eq!(
                error_name(&response),
                "runtime.protocol.duplicate_request_id"
            );
            break;
        }
    }
    let eof = tokio::time::timeout(std::time::Duration::from_secs(1), client.framed.next())
        .await
        .expect("duplicate request IDs must terminate the connection");
    assert!(
        eof.is_none(),
        "broker must initiate EOF after duplicate IDs"
    );
    assert!(server.await.unwrap().cleanup_error().is_none());

    let descriptor = runtime.enumerate_serial().await.unwrap().remove(0);
    runtime
        .open_serial(
            OwnerId::parse("broker-contract:duplicate-id-reuse").unwrap(),
            descriptor.selector(),
            SerialConfig::default(),
        )
        .await
        .unwrap()
        .close()
        .await
        .unwrap();
}

#[tokio::test]
async fn malformed_operation_returns_a_structured_protocol_error() {
    let (server_io, client_io) = tokio::io::duplex(16 * 1024);
    let broker = broker();
    let server = tokio::spawn(async move { broker.serve_connection(server_io).await });
    let mut client = Client::new(client_io);
    client.handshake().await;

    let response = client
        .request(envelope::Payload::SerialFlushRequest(
            v1::SerialFlushRequest {
                session_id: "not a valid id".to_owned(),
                lease: None,
            },
        ))
        .await;
    let error = match response.payload.unwrap() {
        envelope::Payload::Error(error) => error,
        _ => panic!("expected structured error"),
    };
    assert_eq!(error.name, "runtime.protocol.invalid_message");
    assert_eq!(error.category, v1::ErrorCategory::InvalidArgument as i32);
    assert!(!error.operation.is_empty());

    drop(client);
    assert!(server.await.unwrap().cleanup_error().is_none());
}

#[test]
fn resource_descriptor_selector_conversion_is_canonical() {
    let runtime = runtime();
    let descriptor = tokio::runtime::Runtime::new()
        .unwrap()
        .block_on(runtime.enumerate_serial())
        .unwrap()
        .remove(0);
    let wire = v1::ResourceDescriptor::from(&descriptor);
    let selector = ResourceSelector::try_from(v1::ResourceSelector {
        resource_id: wire.resource_id,
        minimum_identity_quality: wire.identity_quality,
        transport: wire.transport,
    })
    .unwrap();
    assert_eq!(selector, descriptor.selector());
}

#[cfg(unix)]
#[tokio::test]
async fn unix_listener_uses_a_private_directory_and_socket() {
    use std::os::unix::fs::PermissionsExt;

    use seeed_hal_broker::listener::UnixBroker;

    let directory =
        std::path::Path::new("/tmp").join(format!("shb-contract-{}", std::process::id()));
    if directory.exists() {
        std::fs::remove_dir_all(&directory).unwrap();
    }
    let listener = UnixBroker::bind(broker(), &directory).await.unwrap();
    let socket_path = listener.socket_path().to_owned();

    assert_eq!(
        std::fs::metadata(&directory).unwrap().permissions().mode() & 0o777,
        0o700
    );
    assert_eq!(
        std::fs::metadata(&socket_path)
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o600
    );

    drop(listener);
    assert!(!socket_path.exists());
    std::fs::remove_dir(directory).unwrap();
}
