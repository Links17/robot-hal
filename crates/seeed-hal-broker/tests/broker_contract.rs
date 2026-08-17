use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Condvar, Mutex};
use std::task::{Context, Poll};

use bytes::Bytes;
use futures_util::{SinkExt, StreamExt};
use prost::Message;
use seeed_hal_broker::{Broker, BrokerConfig, StartupToken};
use seeed_hal_can::{
    CanAdapter, CanBusState, CanBusStatus, CanChannel, CanFrame, CanId, CanLinkExpectation,
    CanMode, CanOpenConfig,
};
use seeed_hal_core::{
    CapabilityId, CapabilitySet, ErrorCategory, HalError, HalResult, OwnerId, ResourceDescriptor,
    ResourceSelector,
};
use seeed_hal_gpio::GpioEdge;
use seeed_hal_protocol::v1::{self, envelope};
use seeed_hal_runtime::{HalRuntime, RuntimeEventKind};
use seeed_hal_serial::{ControlLines, SerialAdapter, SerialConfig, SerialSession};
use seeed_hal_testkit::{
    VirtualCanAdapter, VirtualGpioAdapter, VirtualSerialAdapter, VirtualUsbAdapter,
};
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

#[derive(Clone)]
struct MaskedCanAdapter {
    inner: VirtualCanAdapter,
    descriptor: ResourceDescriptor,
}

impl MaskedCanAdapter {
    fn classic_only(resource_id: &str) -> Self {
        let inner = VirtualCanAdapter::loopback(resource_id);
        let source = inner.descriptor();
        let descriptor = ResourceDescriptor::new(
            source.id().clone(),
            source.endpoint().clone(),
            source.minimum_identity_quality(),
            source.transport(),
            source.properties().clone(),
            CapabilitySet::new(vec![CapabilityId::parse("can.classic/v1").unwrap()]),
        );
        Self { inner, descriptor }
    }
}

#[async_trait::async_trait]
impl CanAdapter for MaskedCanAdapter {
    fn adapter_name(&self) -> &'static str {
        "virtual.can.masked"
    }

    async fn enumerate(&self) -> HalResult<Vec<ResourceDescriptor>> {
        Ok(vec![self.descriptor.clone()])
    }

    async fn open(
        &self,
        selector: &ResourceSelector,
        config: &CanOpenConfig,
    ) -> HalResult<Box<dyn CanChannel>> {
        self.inner.open(selector, config).await
    }
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

fn can_runtime(adapter: VirtualCanAdapter) -> HalRuntime {
    HalRuntime::builder().can_adapter(adapter).build()
}

fn usb_gpio_broker() -> Broker {
    let runtime = HalRuntime::builder()
        .usb_adapter(VirtualUsbAdapter::loopback("usb:virtual:broker"))
        .gpio_adapter(VirtualGpioAdapter::line_bank("gpio:virtual:broker", 2))
        .build();
    Broker::with_startup_token(runtime, StartupToken::from_bytes(TOKEN))
}

fn usb_gpio_broker_with_adapters() -> (Broker, VirtualUsbAdapter, VirtualGpioAdapter) {
    let usb = VirtualUsbAdapter::loopback("usb:virtual:broker");
    let gpio = VirtualGpioAdapter::line_bank("gpio:virtual:broker", 2);
    usb_gpio_broker_with_adapters_inner(usb, gpio)
}

fn usb_gpio_broker_with_adapters_inner(
    usb: VirtualUsbAdapter,
    gpio: VirtualGpioAdapter,
) -> (Broker, VirtualUsbAdapter, VirtualGpioAdapter) {
    let runtime = HalRuntime::builder()
        .usb_adapter(usb.clone())
        .gpio_adapter(gpio.clone())
        .build();
    (
        Broker::with_startup_token(runtime, StartupToken::from_bytes(TOKEN)),
        usb,
        gpio,
    )
}

fn minor_two_handshake() -> v1::HandshakeRequest {
    let mut handshake = valid_handshake(TOKEN.to_vec());
    handshake.protocol_minor = 2;
    handshake.protocol_minor_minimum = 2;
    handshake.protocol_minor_maximum = 2;
    handshake
}

async fn enumerate_usb<T>(client: &mut Client<T>) -> v1::ResourceDescriptor
where
    T: AsyncRead + AsyncWrite + Unpin,
{
    match client
        .request(envelope::Payload::EnumerateUsbRequest(
            v1::EnumerateUsbRequest {},
        ))
        .await
        .payload
    {
        Some(envelope::Payload::EnumerateUsbResponse(response)) => {
            response.resources.into_iter().next().unwrap()
        }
        other => panic!("expected USB enumeration, got {other:?}"),
    }
}

async fn enumerate_gpio<T>(client: &mut Client<T>) -> v1::ResourceDescriptor
where
    T: AsyncRead + AsyncWrite + Unpin,
{
    match client
        .request(envelope::Payload::EnumerateGpioRequest(
            v1::EnumerateGpioRequest {},
        ))
        .await
        .payload
    {
        Some(envelope::Payload::EnumerateGpioResponse(response)) => {
            response.resources.into_iter().next().unwrap()
        }
        other => panic!("expected GPIO enumeration, got {other:?}"),
    }
}

fn selector(descriptor: &v1::ResourceDescriptor) -> v1::ResourceSelector {
    v1::ResourceSelector {
        resource_id: descriptor.resource_id.clone(),
        minimum_identity_quality: descriptor.identity_quality,
        transport: descriptor.transport,
    }
}

async fn open_usb<T>(
    client: &mut Client<T>,
    descriptor: &v1::ResourceDescriptor,
) -> v1::OpenUsbResponse
where
    T: AsyncRead + AsyncWrite + Unpin,
{
    match client
        .request(envelope::Payload::OpenUsbRequest(v1::OpenUsbRequest {
            selector: Some(selector(descriptor)),
            interface_number: 0,
        }))
        .await
        .payload
    {
        Some(envelope::Payload::OpenUsbResponse(response)) => response,
        other => panic!("expected USB open, got {other:?}"),
    }
}

async fn open_gpio<T>(
    client: &mut Client<T>,
    descriptor: &v1::ResourceDescriptor,
    line: u32,
    direction: v1::GpioDirection,
) -> v1::OpenGpioResponse
where
    T: AsyncRead + AsyncWrite + Unpin,
{
    match client
        .request(envelope::Payload::OpenGpioRequest(v1::OpenGpioRequest {
            selector: Some(selector(descriptor)),
            lines: vec![line],
            config: Some(v1::GpioLineConfig {
                direction: direction as i32,
                active_low: false,
                bias: v1::GpioBias::Disabled as i32,
                drive: match direction {
                    v1::GpioDirection::Output => v1::GpioDrive::PushPull as i32,
                    _ => v1::GpioDrive::Unspecified as i32,
                },
                initial_value: (direction == v1::GpioDirection::Output).then_some(false),
            }),
        }))
        .await
        .payload
    {
        Some(envelope::Payload::OpenGpioResponse(response)) => response,
        other => panic!("expected GPIO open, got {other:?}"),
    }
}

#[tokio::test]
async fn minor_two_handshake_enumerates_usb_and_gpio() {
    let (server_io, client_io) = tokio::io::duplex(16 * 1024);
    let server = tokio::spawn(async move { usb_gpio_broker().serve_connection(server_io).await });
    let mut client = Client::new(client_io);
    let mut handshake = valid_handshake(TOKEN.to_vec());
    handshake.protocol_minor = 2;
    handshake.protocol_minor_minimum = 2;
    handshake.protocol_minor_maximum = 2;
    client
        .request(envelope::Payload::HandshakeRequest(handshake))
        .await;
    let usb = client
        .request(envelope::Payload::EnumerateUsbRequest(
            v1::EnumerateUsbRequest {},
        ))
        .await;
    let usb = match usb.payload {
        Some(envelope::Payload::EnumerateUsbResponse(response)) => {
            response.resources.into_iter().next().unwrap()
        }
        other => panic!("expected USB enumeration, got {other:?}"),
    };
    let gpio = client
        .request(envelope::Payload::EnumerateGpioRequest(
            v1::EnumerateGpioRequest {},
        ))
        .await;
    let gpio = match gpio.payload {
        Some(envelope::Payload::EnumerateGpioResponse(response)) => {
            response.resources.into_iter().next().unwrap()
        }
        other => panic!("expected GPIO enumeration, got {other:?}"),
    };
    let usb_open = client
        .request(envelope::Payload::OpenUsbRequest(v1::OpenUsbRequest {
            selector: Some(v1::ResourceSelector {
                resource_id: usb.resource_id,
                minimum_identity_quality: usb.identity_quality,
                transport: usb.transport,
            }),
            interface_number: 0,
        }))
        .await;
    assert!(matches!(
        usb_open.payload,
        Some(envelope::Payload::OpenUsbResponse(_))
    ));
    let gpio_open = client
        .request(envelope::Payload::OpenGpioRequest(v1::OpenGpioRequest {
            selector: Some(v1::ResourceSelector {
                resource_id: gpio.resource_id,
                minimum_identity_quality: gpio.identity_quality,
                transport: gpio.transport,
            }),
            lines: vec![0],
            config: Some(v1::GpioLineConfig {
                direction: v1::GpioDirection::Input as i32,
                active_low: false,
                bias: v1::GpioBias::Disabled as i32,
                drive: v1::GpioDrive::Unspecified as i32,
                initial_value: None,
            }),
        }))
        .await;
    assert!(matches!(
        gpio_open.payload,
        Some(envelope::Payload::OpenGpioResponse(_))
    ));
    drop(client);
    assert!(server.await.unwrap().cleanup_error().is_none());
}

#[tokio::test]
async fn minor_two_broker_dispatches_owner_scoped_usb_and_gpio_sessions() {
    let (broker, usb, gpio) = usb_gpio_broker_with_adapters();
    let (server_io, client_io) = tokio::io::duplex(64 * 1024);
    let server = tokio::spawn(async move { broker.serve_connection(server_io).await });
    let mut client = Client::new(client_io);
    let handshake = client
        .request(envelope::Payload::HandshakeRequest(minor_two_handshake()))
        .await;
    let handshake = match handshake.payload {
        Some(envelope::Payload::HandshakeResponse(response)) => response,
        other => panic!("expected minor-two handshake response, got {other:?}"),
    };
    for capability in [
        "usb.control/v1",
        "usb.bulk/v1",
        "usb.interrupt/v1",
        "gpio.lines/v1",
        "gpio.edges/v1",
    ] {
        assert!(
            handshake
                .capabilities
                .iter()
                .any(|value| value == capability),
            "minor-two handshake did not advertise {capability}"
        );
    }

    let usb_descriptor = match client
        .request(envelope::Payload::EnumerateUsbRequest(
            v1::EnumerateUsbRequest {},
        ))
        .await
        .payload
    {
        Some(envelope::Payload::EnumerateUsbResponse(response)) => {
            response.resources.into_iter().next().unwrap()
        }
        other => panic!("expected USB enumeration, got {other:?}"),
    };
    let usb_session = match client
        .request(envelope::Payload::OpenUsbRequest(v1::OpenUsbRequest {
            selector: Some(v1::ResourceSelector {
                resource_id: usb_descriptor.resource_id,
                minimum_identity_quality: usb_descriptor.identity_quality,
                transport: usb_descriptor.transport,
            }),
            interface_number: 0,
        }))
        .await
        .payload
    {
        Some(envelope::Payload::OpenUsbResponse(response)) => response,
        other => panic!("expected USB open, got {other:?}"),
    };
    let transfer = |kind, endpoint, data, max_bytes| v1::UsbTransferRequest {
        session_id: usb_session.session_id.clone(),
        lease: usb_session.lease.clone(),
        kind: kind as i32,
        endpoint,
        data,
        max_bytes,
        timeout_ms: 100,
        ..Default::default()
    };
    assert!(matches!(
        client
            .request(envelope::Payload::UsbTransferRequest(transfer(
                v1::UsbTransferKind::BulkOut,
                1,
                b"usb-loopback".to_vec(),
                0,
            )))
            .await
            .payload,
        Some(envelope::Payload::UsbTransferResponse(_))
    ));
    let usb_read = client
        .request(envelope::Payload::UsbTransferRequest(transfer(
            v1::UsbTransferKind::BulkIn,
            0x81,
            vec![],
            64,
        )))
        .await;
    match usb_read.payload {
        Some(envelope::Payload::UsbTransferResponse(response)) => {
            assert_eq!(response.data, b"usb-loopback");
        }
        other => panic!("expected USB transfer response, got {other:?}"),
    }

    let gpio_descriptor = match client
        .request(envelope::Payload::EnumerateGpioRequest(
            v1::EnumerateGpioRequest {},
        ))
        .await
        .payload
    {
        Some(envelope::Payload::EnumerateGpioResponse(response)) => {
            response.resources.into_iter().next().unwrap()
        }
        other => panic!("expected GPIO enumeration, got {other:?}"),
    };
    let selector = || v1::ResourceSelector {
        resource_id: gpio_descriptor.resource_id.clone(),
        minimum_identity_quality: gpio_descriptor.identity_quality,
        transport: gpio_descriptor.transport,
    };
    let gpio_output = match client
        .request(envelope::Payload::OpenGpioRequest(v1::OpenGpioRequest {
            selector: Some(selector()),
            lines: vec![0],
            config: Some(v1::GpioLineConfig {
                direction: v1::GpioDirection::Output as i32,
                active_low: false,
                bias: v1::GpioBias::Disabled as i32,
                drive: v1::GpioDrive::PushPull as i32,
                initial_value: Some(false),
            }),
        }))
        .await
        .payload
    {
        Some(envelope::Payload::OpenGpioResponse(response)) => response,
        other => panic!("expected GPIO output open, got {other:?}"),
    };
    assert!(matches!(
        client
            .request(envelope::Payload::GpioWriteRequest(v1::GpioWriteRequest {
                session_id: gpio_output.session_id.clone(),
                lease: gpio_output.lease.clone(),
                values: vec![true],
            }))
            .await
            .payload,
        Some(envelope::Payload::GpioWriteResponse(_))
    ));
    match client
        .request(envelope::Payload::GpioReadRequest(v1::GpioReadRequest {
            session_id: gpio_output.session_id.clone(),
            lease: gpio_output.lease.clone(),
        }))
        .await
        .payload
    {
        Some(envelope::Payload::GpioReadResponse(response)) => assert_eq!(response.values, [true]),
        other => panic!("expected GPIO read response, got {other:?}"),
    }
    assert!(matches!(
        client
            .request(envelope::Payload::CloseGpioRequest(v1::CloseGpioRequest {
                session_id: gpio_output.session_id,
                lease: gpio_output.lease,
            }))
            .await
            .payload,
        Some(envelope::Payload::CloseGpioResponse(_))
    ));
    let gpio_input = match client
        .request(envelope::Payload::OpenGpioRequest(v1::OpenGpioRequest {
            selector: Some(selector()),
            lines: vec![1],
            config: Some(v1::GpioLineConfig {
                direction: v1::GpioDirection::Input as i32,
                active_low: false,
                bias: v1::GpioBias::Disabled as i32,
                drive: v1::GpioDrive::Unspecified as i32,
                initial_value: None,
            }),
        }))
        .await
        .payload
    {
        Some(envelope::Payload::OpenGpioResponse(response)) => response,
        other => panic!("expected GPIO input open, got {other:?}"),
    };
    gpio.inject_edge(1, GpioEdge::Rising, 42).unwrap();
    match client
        .request(envelope::Payload::GpioNextEdgeRequest(
            v1::GpioNextEdgeRequest {
                session_id: gpio_input.session_id.clone(),
                lease: gpio_input.lease.clone(),
                rising: true,
                falling: false,
                capacity: 1,
                timeout_ms: 100,
            },
        ))
        .await
        .payload
    {
        Some(envelope::Payload::GpioNextEdgeResponse(response)) => {
            let event = response.event.expect("injected edge must be delivered");
            assert_eq!(event.edge, v1::GpioEdge::Rising as i32);
            assert_eq!(event.monotonic_ns, 42);
        }
        other => panic!("expected GPIO edge response, got {other:?}"),
    }

    assert!(matches!(
        client
            .request(envelope::Payload::CloseUsbRequest(v1::CloseUsbRequest {
                session_id: usb_session.session_id,
                lease: usb_session.lease,
            }))
            .await
            .payload,
        Some(envelope::Payload::CloseUsbResponse(_))
    ));
    assert!(matches!(
        client
            .request(envelope::Payload::CloseGpioRequest(v1::CloseGpioRequest {
                session_id: gpio_input.session_id,
                lease: gpio_input.lease,
            }))
            .await
            .payload,
        Some(envelope::Payload::CloseGpioResponse(_))
    ));
    assert!(usb.claimed_interfaces().is_empty());
    assert!(gpio.claimed_lines().is_empty());

    drop(client);
    assert!(server.await.unwrap().cleanup_error().is_none());
}

#[tokio::test]
async fn usb_and_gpio_sessions_reject_cross_connection_and_stale_leases_before_adapter_io() {
    let (broker, usb, gpio) = usb_gpio_broker_with_adapters();
    let (first_server_io, first_client_io) = tokio::io::duplex(64 * 1024);
    let first_server = tokio::spawn(async move { broker.serve_connection(first_server_io).await });
    let mut first = Client::new(first_client_io);
    first
        .request(envelope::Payload::HandshakeRequest(minor_two_handshake()))
        .await;
    let usb_descriptor = enumerate_usb(&mut first).await;
    let gpio_descriptor = enumerate_gpio(&mut first).await;
    let usb_session = open_usb(&mut first, &usb_descriptor).await;
    let gpio_session = open_gpio(&mut first, &gpio_descriptor, 0, v1::GpioDirection::Output).await;

    let (second_server_io, second_client_io) = tokio::io::duplex(64 * 1024);
    let second_broker = Broker::with_startup_token(
        HalRuntime::builder()
            .usb_adapter(usb.clone())
            .gpio_adapter(gpio.clone())
            .build(),
        StartupToken::from_bytes(TOKEN),
    );
    let second_server =
        tokio::spawn(async move { second_broker.serve_connection(second_server_io).await });
    let mut second = Client::new(second_client_io);
    second
        .request(envelope::Payload::HandshakeRequest(minor_two_handshake()))
        .await;

    usb.fail_next_transfer(
        HalError::new(
            "test.usb.adapter_touched",
            ErrorCategory::Internal,
            "usb.transfer",
            false,
            "foreign USB request reached the adapter",
        )
        .unwrap(),
    );
    let foreign_usb = second
        .request(envelope::Payload::UsbTransferRequest(
            v1::UsbTransferRequest {
                session_id: usb_session.session_id.clone(),
                lease: usb_session.lease.clone(),
                kind: v1::UsbTransferKind::BulkOut as i32,
                endpoint: 1,
                data: b"forbidden".to_vec(),
                timeout_ms: 10,
                ..Default::default()
            },
        ))
        .await;
    assert_eq!(error_name(&foreign_usb), "runtime.session.not_found");
    let owner_usb = first
        .request(envelope::Payload::UsbTransferRequest(
            v1::UsbTransferRequest {
                session_id: usb_session.session_id.clone(),
                lease: usb_session.lease.clone(),
                kind: v1::UsbTransferKind::BulkOut as i32,
                endpoint: 1,
                data: vec![],
                timeout_ms: 10,
                ..Default::default()
            },
        ))
        .await;
    assert_eq!(error_name(&owner_usb), "test.usb.adapter_touched");
    gpio.fail_next_read(
        HalError::new(
            "test.gpio.adapter_touched",
            ErrorCategory::Internal,
            "gpio.read",
            false,
            "foreign GPIO request reached the adapter",
        )
        .unwrap(),
    );
    let foreign_gpio = second
        .request(envelope::Payload::GpioWriteRequest(v1::GpioWriteRequest {
            session_id: gpio_session.session_id.clone(),
            lease: gpio_session.lease.clone(),
            values: vec![true],
        }))
        .await;
    assert_eq!(error_name(&foreign_gpio), "runtime.session.not_found");
    let owner_gpio = first
        .request(envelope::Payload::GpioReadRequest(v1::GpioReadRequest {
            session_id: gpio_session.session_id.clone(),
            lease: gpio_session.lease.clone(),
        }))
        .await;
    assert_eq!(error_name(&owner_gpio), "test.gpio.adapter_touched");

    let stale_usb_lease = usb_session.lease.clone().unwrap();
    assert!(matches!(
        first
            .request(envelope::Payload::CloseUsbRequest(v1::CloseUsbRequest {
                session_id: usb_session.session_id,
                lease: usb_session.lease,
            }))
            .await
            .payload,
        Some(envelope::Payload::CloseUsbResponse(_))
    ));
    let reopened_usb = open_usb(&mut first, &usb_descriptor).await;
    let stale_usb = first
        .request(envelope::Payload::UsbTransferRequest(
            v1::UsbTransferRequest {
                session_id: reopened_usb.session_id,
                lease: Some(stale_usb_lease),
                kind: v1::UsbTransferKind::BulkOut as i32,
                endpoint: 1,
                data: b"stale".to_vec(),
                timeout_ms: 10,
                ..Default::default()
            },
        ))
        .await;
    assert_eq!(error_name(&stale_usb), "runtime.lease.stale_generation");

    let stale_gpio_lease = gpio_session.lease.clone().unwrap();
    assert!(matches!(
        first
            .request(envelope::Payload::CloseGpioRequest(v1::CloseGpioRequest {
                session_id: gpio_session.session_id,
                lease: gpio_session.lease,
            }))
            .await
            .payload,
        Some(envelope::Payload::CloseGpioResponse(_))
    ));
    let reopened_gpio = open_gpio(&mut first, &gpio_descriptor, 0, v1::GpioDirection::Output).await;
    let stale_gpio = first
        .request(envelope::Payload::GpioWriteRequest(v1::GpioWriteRequest {
            session_id: reopened_gpio.session_id,
            lease: Some(stale_gpio_lease),
            values: vec![true],
        }))
        .await;
    assert_eq!(error_name(&stale_gpio), "runtime.lease.stale_generation");

    drop(first);
    drop(second);
    assert!(first_server.await.unwrap().cleanup_error().is_none());
    assert!(second_server.await.unwrap().cleanup_error().is_none());
}

#[tokio::test]
async fn gpio_lag_is_delivered_as_a_structured_broker_error() {
    let usb = VirtualUsbAdapter::loopback("usb:virtual:broker-lag");
    let gpio = VirtualGpioAdapter::line_bank_with_event_capacity("gpio:virtual:broker-lag", 1, 1);
    let (broker, _, gpio) = usb_gpio_broker_with_adapters_inner(usb, gpio);
    let (server_io, client_io) = tokio::io::duplex(64 * 1024);
    let server = tokio::spawn(async move { broker.serve_connection(server_io).await });
    let mut client = Client::new(client_io);
    client
        .request(envelope::Payload::HandshakeRequest(minor_two_handshake()))
        .await;
    let descriptor = enumerate_gpio(&mut client).await;
    let session = open_gpio(&mut client, &descriptor, 0, v1::GpioDirection::Input).await;
    gpio.inject_edge(0, GpioEdge::Rising, 1).unwrap();
    gpio.inject_edge(0, GpioEdge::Falling, 2).unwrap();

    let lagged = client
        .request(envelope::Payload::GpioNextEdgeRequest(
            v1::GpioNextEdgeRequest {
                session_id: session.session_id.clone(),
                lease: session.lease.clone(),
                rising: true,
                falling: true,
                capacity: 1,
                timeout_ms: 100,
            },
        ))
        .await;
    assert_eq!(error_name(&lagged), "gpio.edge.lagged");
    let error = match lagged.payload {
        Some(envelope::Payload::Error(error)) => error,
        other => panic!("expected lag error, got {other:?}"),
    };
    assert_eq!(
        error.context.get("dropped_count").map(String::as_str),
        Some("1")
    );
    let retained = client
        .request(envelope::Payload::GpioNextEdgeRequest(
            v1::GpioNextEdgeRequest {
                session_id: session.session_id,
                lease: session.lease,
                rising: true,
                falling: true,
                capacity: 1,
                timeout_ms: 100,
            },
        ))
        .await;
    match retained.payload {
        Some(envelope::Payload::GpioNextEdgeResponse(response)) => {
            assert_eq!(response.event.unwrap().monotonic_ns, 2);
        }
        other => panic!("expected retained GPIO edge, got {other:?}"),
    }
    drop(client);
    assert!(server.await.unwrap().cleanup_error().is_none());
}

#[tokio::test]
async fn disconnect_releases_usb_and_gpio_for_a_new_broker_connection() {
    let (broker, usb, gpio) = usb_gpio_broker_with_adapters();
    let (first_server_io, first_client_io) = tokio::io::duplex(64 * 1024);
    let first_server = tokio::spawn(async move { broker.serve_connection(first_server_io).await });
    let mut first = Client::new(first_client_io);
    first
        .request(envelope::Payload::HandshakeRequest(minor_two_handshake()))
        .await;
    let usb_descriptor = enumerate_usb(&mut first).await;
    let gpio_descriptor = enumerate_gpio(&mut first).await;
    let _usb = open_usb(&mut first, &usb_descriptor).await;
    let _gpio = open_gpio(&mut first, &gpio_descriptor, 0, v1::GpioDirection::Output).await;
    drop(first);
    assert!(first_server.await.unwrap().cleanup_error().is_none());
    assert!(usb.claimed_interfaces().is_empty());
    assert!(gpio.claimed_lines().is_empty());

    let second_broker = Broker::with_startup_token(
        HalRuntime::builder()
            .usb_adapter(usb.clone())
            .gpio_adapter(gpio.clone())
            .build(),
        StartupToken::from_bytes(TOKEN),
    );
    let (second_server_io, second_client_io) = tokio::io::duplex(64 * 1024);
    let second_server =
        tokio::spawn(async move { second_broker.serve_connection(second_server_io).await });
    let mut second = Client::new(second_client_io);
    second
        .request(envelope::Payload::HandshakeRequest(minor_two_handshake()))
        .await;
    assert!(matches!(
        open_usb(&mut second, &usb_descriptor).await,
        v1::OpenUsbResponse { .. }
    ));
    assert!(matches!(
        open_gpio(&mut second, &gpio_descriptor, 0, v1::GpioDirection::Output).await,
        v1::OpenGpioResponse { .. }
    ));
    drop(second);
    assert!(second_server.await.unwrap().cleanup_error().is_none());
}

fn can_handshake() -> v1::HandshakeRequest {
    let mut handshake = valid_handshake(TOKEN.to_vec());
    handshake.protocol_minor = 1;
    handshake.protocol_minor_minimum = 1;
    handshake.protocol_minor_maximum = 1;
    handshake.required_capabilities = vec!["can.classic/v1".to_owned()];
    handshake
}

#[tokio::test]
async fn usb_and_gpio_envelopes_require_wire_minor_two() {
    let (server_io, client_io) = tokio::io::duplex(16 * 1024);
    let server = tokio::spawn(async move { broker().serve_connection(server_io).await });
    let mut client = Client::new(client_io);
    client.handshake().await;
    for payload in [
        envelope::Payload::EnumerateUsbRequest(v1::EnumerateUsbRequest {}),
        envelope::Payload::EnumerateGpioRequest(v1::EnumerateGpioRequest {}),
    ] {
        let response = client.request(payload).await;
        let error = match response.payload {
            Some(envelope::Payload::Error(error)) => error,
            other => panic!("expected protocol error, got {other:?}"),
        };
        assert_eq!(error.name, "runtime.protocol.unsupported_capability");
    }
    drop(client);
    assert!(server.await.unwrap().cleanup_error().is_none());
}

async fn handshake_can<T>(client: &mut Client<T>) -> v1::HandshakeResponse
where
    T: AsyncRead + AsyncWrite + Unpin,
{
    let response = client
        .request(envelope::Payload::HandshakeRequest(can_handshake()))
        .await;
    match response.payload.unwrap() {
        envelope::Payload::HandshakeResponse(response) => response,
        other => panic!("expected CAN handshake response, got {other:?}"),
    }
}

fn can_attach(mode: Option<v1::CanMode>) -> v1::CanOpenConfig {
    v1::CanOpenConfig {
        config: Some(v1::can_open_config::Config::Attach(
            v1::CanLinkExpectation {
                mode: mode.map(|value| value as i32),
                nominal_bitrate: None,
                data_bitrate: None,
                listen_only: None,
                loopback: None,
            },
        )),
    }
}

fn can_filter_data() -> v1::CanFilterSet {
    v1::CanFilterSet { filters: vec![] }
}

fn classic_can_frame(byte: u8) -> v1::CanFrame {
    v1::CanFrame {
        id: Some(v1::CanId {
            value: 0x123,
            format: v1::CanIdFormat::Standard as i32,
        }),
        kind: v1::CanFrameKind::ClassicData as i32,
        data: vec![byte],
        ..Default::default()
    }
}

async fn open_virtual_can<T>(
    client: &mut Client<T>,
    mode: Option<v1::CanMode>,
) -> v1::OpenCanResponse
where
    T: AsyncRead + AsyncWrite + Unpin,
{
    let enumerate = client
        .request(envelope::Payload::EnumerateCanRequest(
            v1::EnumerateCanRequest {},
        ))
        .await;
    let descriptor = match enumerate.payload.unwrap() {
        envelope::Payload::EnumerateCanResponse(response) => {
            response.resources.into_iter().next().unwrap()
        }
        other => panic!("expected CAN enumerate response, got {other:?}"),
    };
    let response = client
        .request(envelope::Payload::OpenCanRequest(v1::OpenCanRequest {
            selector: Some(v1::ResourceSelector {
                resource_id: descriptor.resource_id,
                minimum_identity_quality: descriptor.identity_quality,
                transport: descriptor.transport,
            }),
            mode: v1::LeaseMode::Control as i32,
            config: Some(can_attach(mode)),
            filters: Some(can_filter_data()),
        }))
        .await;
    match response.payload.unwrap() {
        envelope::Payload::OpenCanResponse(response) => response,
        other => panic!("expected CAN open response, got {other:?}"),
    }
}

async fn open_configured_classic_can<T>(client: &mut Client<T>) -> v1::OpenCanResponse
where
    T: AsyncRead + AsyncWrite + Unpin,
{
    let enumerate = client
        .request(envelope::Payload::EnumerateCanRequest(
            v1::EnumerateCanRequest {},
        ))
        .await;
    let descriptor = match enumerate.payload.unwrap() {
        envelope::Payload::EnumerateCanResponse(response) => {
            response.resources.into_iter().next().unwrap()
        }
        other => panic!("expected CAN enumerate response, got {other:?}"),
    };
    let response = client
        .request(envelope::Payload::OpenCanRequest(v1::OpenCanRequest {
            selector: Some(v1::ResourceSelector {
                resource_id: descriptor.resource_id,
                minimum_identity_quality: descriptor.identity_quality,
                transport: descriptor.transport,
            }),
            mode: v1::LeaseMode::Maintenance as i32,
            config: Some(v1::CanOpenConfig {
                config: Some(v1::can_open_config::Config::Configure(
                    v1::CanConfigureConfig {
                        mode: v1::CanMode::Classic as i32,
                        nominal: Some(v1::CanBitTiming {
                            bitrate: 250_000,
                            sample_point_permill: None,
                            sjw: None,
                        }),
                        data: None,
                        listen_only: true,
                        loopback: false,
                        restart_ms: None,
                    },
                )),
            }),
            filters: Some(can_filter_data()),
        }))
        .await;
    match response.payload.unwrap() {
        envelope::Payload::OpenCanResponse(response) => response,
        other => panic!("expected configured CAN open response, got {other:?}"),
    }
}

async fn open_configured_fd_can<T>(client: &mut Client<T>) -> v1::OpenCanResponse
where
    T: AsyncRead + AsyncWrite + Unpin,
{
    let enumerate = client
        .request(envelope::Payload::EnumerateCanRequest(
            v1::EnumerateCanRequest {},
        ))
        .await;
    let descriptor = match enumerate.payload.unwrap() {
        envelope::Payload::EnumerateCanResponse(response) => {
            response.resources.into_iter().next().unwrap()
        }
        other => panic!("expected CAN enumerate response, got {other:?}"),
    };
    let response = client
        .request(envelope::Payload::OpenCanRequest(v1::OpenCanRequest {
            selector: Some(v1::ResourceSelector {
                resource_id: descriptor.resource_id,
                minimum_identity_quality: descriptor.identity_quality,
                transport: descriptor.transport,
            }),
            mode: v1::LeaseMode::Maintenance as i32,
            config: Some(v1::CanOpenConfig {
                config: Some(v1::can_open_config::Config::Configure(
                    v1::CanConfigureConfig {
                        mode: v1::CanMode::Fd as i32,
                        nominal: Some(v1::CanBitTiming {
                            bitrate: 500_000,
                            sample_point_permill: None,
                            sjw: None,
                        }),
                        data: Some(v1::CanBitTiming {
                            bitrate: 2_000_000,
                            sample_point_permill: None,
                            sjw: None,
                        }),
                        listen_only: false,
                        loopback: true,
                        restart_ms: None,
                    },
                )),
            }),
            filters: Some(can_filter_data()),
        }))
        .await;
    match response.payload.unwrap() {
        envelope::Payload::OpenCanResponse(response) => response,
        other => panic!("expected configured CAN FD open response, got {other:?}"),
    }
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
    assert_eq!(accepted.protocol_minor, 2);
    assert_eq!(accepted.protocol_minor_minimum, 0);
    assert_eq!(accepted.protocol_minor_maximum, 2);

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

#[tokio::test]
async fn can_envelopes_require_wire_minor_one_without_affecting_serial() {
    let adapter = VirtualCanAdapter::loopback("can:virtual:minor-gate");
    let runtime = HalRuntime::builder()
        .serial_adapter(VirtualSerialAdapter::loopback("serial:virtual:minor-gate"))
        .can_adapter(adapter)
        .build();
    let broker = Broker::with_startup_token(runtime, StartupToken::from_bytes(TOKEN));
    let (server_io, client_io) = tokio::io::duplex(64 * 1024);
    let server = tokio::spawn(async move { broker.serve_connection(server_io).await });
    let mut client = Client::new(client_io);
    client.handshake().await;

    let rejected = client
        .request(envelope::Payload::EnumerateCanRequest(
            v1::EnumerateCanRequest {},
        ))
        .await;
    assert_eq!(
        error_name(&rejected),
        "runtime.protocol.capability_unsupported"
    );
    let serial = client
        .request(envelope::Payload::EnumerateSerialRequest(
            v1::EnumerateSerialRequest {},
        ))
        .await;
    assert!(matches!(
        serial.payload,
        Some(envelope::Payload::EnumerateSerialResponse(_))
    ));

    drop(client);
    assert!(server.await.unwrap().cleanup_error().is_none());
}

#[tokio::test]
async fn minor_one_handshake_enumerates_and_opens_can_with_exact_capabilities() {
    let adapter = VirtualCanAdapter::loopback("can:virtual:broker-open");
    let broker = Broker::with_startup_token(can_runtime(adapter), StartupToken::from_bytes(TOKEN));
    let (server_io, client_io) = tokio::io::duplex(64 * 1024);
    let server = tokio::spawn(async move { broker.serve_connection(server_io).await });
    let mut client = Client::new(client_io);

    let handshake = handshake_can(&mut client).await;
    assert_eq!(handshake.protocol_minor, 1);
    for capability in [
        "can.classic/v1",
        "can.fd/v1",
        "can.configure/v1",
        "can.error-frames/v1",
        "can.rx-timestamp/v1",
    ] {
        assert!(
            handshake
                .capabilities
                .iter()
                .any(|value| value == capability)
        );
    }
    let session = open_virtual_can(&mut client, Some(v1::CanMode::Classic)).await;
    assert_eq!(
        session.lease.as_ref().unwrap().mode,
        v1::LeaseMode::Control as i32
    );

    let close_request = v1::CloseSessionRequest {
        session_id: session.session_id,
        lease: session.lease,
    };
    let close = client
        .request(envelope::Payload::CloseSessionRequest(
            close_request.clone(),
        ))
        .await;
    assert!(matches!(
        close.payload,
        Some(envelope::Payload::CloseSessionResponse(_))
    ));
    let replay = client
        .request(envelope::Payload::CloseSessionRequest(close_request))
        .await;
    assert!(matches!(
        replay.payload,
        Some(envelope::Payload::CloseSessionResponse(_))
    ));
    drop(client);
    assert!(server.await.unwrap().cleanup_error().is_none());
}

#[tokio::test]
async fn can_classic_batch_receive_filter_and_status_use_correlated_responses_only() {
    let adapter = VirtualCanAdapter::loopback("can:virtual:broker-operations");
    let broker = Broker::with_startup_token(can_runtime(adapter), StartupToken::from_bytes(TOKEN));
    let (server_io, client_io) = tokio::io::duplex(64 * 1024);
    let server = tokio::spawn(async move { broker.serve_connection(server_io).await });
    let mut client = Client::new(client_io);
    handshake_can(&mut client).await;
    let session = open_virtual_can(&mut client, Some(v1::CanMode::Classic)).await;

    let send = client
        .request(envelope::Payload::CanSendRequest(v1::CanSendRequest {
            session_id: session.session_id.clone(),
            lease: session.lease.clone(),
            frames: vec![classic_can_frame(1), classic_can_frame(2)],
        }))
        .await;
    match send.payload.unwrap() {
        envelope::Payload::CanSendResponse(response) => {
            assert_eq!(response.committed_count, 2);
            assert!(response.error.is_none());
        }
        other => panic!("expected CAN send response, got {other:?}"),
    }

    let receive = client
        .request(envelope::Payload::CanReceiveRequest(
            v1::CanReceiveRequest {
                session_id: session.session_id.clone(),
                lease: session.lease.clone(),
                max_frames: 2,
                timeout_ms: 100,
            },
        ))
        .await;
    match receive.payload.unwrap() {
        envelope::Payload::CanReceiveResponse(response) => {
            assert_eq!(response.frames.len(), 2);
            assert_eq!(response.frames[0].frame.as_ref().unwrap().data, [1]);
            assert_eq!(response.frames[1].frame.as_ref().unwrap().data, [2]);
            assert!(
                response
                    .frames
                    .iter()
                    .all(|frame| frame.timestamp.is_some())
            );
        }
        other => panic!("expected CAN receive response, got {other:?}"),
    }

    let replace = client
        .request(envelope::Payload::ReplaceCanFiltersRequest(
            v1::ReplaceCanFiltersRequest {
                session_id: session.session_id.clone(),
                lease: session.lease.clone(),
                filters: Some(v1::CanFilterSet {
                    filters: vec![v1::CanFilter {
                        id: 0x123,
                        mask: 0x7ff,
                        format: v1::CanIdFormat::Standard as i32,
                        classes: Some(v1::CanFrameClasses {
                            data: true,
                            remote: false,
                            error: false,
                        }),
                    }],
                }),
            },
        ))
        .await;
    assert!(matches!(
        replace.payload,
        Some(envelope::Payload::ReplaceCanFiltersResponse(_))
    ));

    let status = client
        .request(envelope::Payload::GetCanBusStatusRequest(
            v1::GetCanBusStatusRequest {
                session_id: session.session_id,
                lease: session.lease,
            },
        ))
        .await;
    match status.payload.unwrap() {
        envelope::Payload::GetCanBusStatusResponse(response) => assert_eq!(
            response.status.unwrap().state,
            v1::CanBusState::Active as i32
        ),
        other => panic!("expected CAN status response, got {other:?}"),
    }

    let unsolicited =
        tokio::time::timeout(std::time::Duration::from_millis(50), client.framed.next()).await;
    assert!(
        unsolicited.is_err(),
        "ordinary CAN frames and diagnostics must never use the event path"
    );
    drop(client);
    assert!(server.await.unwrap().cleanup_error().is_none());
}

#[tokio::test]
async fn can_send_partial_progress_is_nested_and_stale_leases_are_fenced() {
    let adapter = VirtualCanAdapter::loopback("can:virtual:broker-partial");
    let broker = Broker::with_startup_token(can_runtime(adapter), StartupToken::from_bytes(TOKEN));
    let (server_io, client_io) = tokio::io::duplex(128 * 1024);
    let server = tokio::spawn(async move { broker.serve_connection(server_io).await });
    let mut client = Client::new(client_io);
    handshake_can(&mut client).await;
    let session = open_virtual_can(&mut client, Some(v1::CanMode::Classic)).await;

    let fill = client
        .request(envelope::Payload::CanSendRequest(v1::CanSendRequest {
            session_id: session.session_id.clone(),
            lease: session.lease.clone(),
            frames: (0..63).map(classic_can_frame).collect(),
        }))
        .await;
    assert!(matches!(
        fill.payload,
        Some(envelope::Payload::CanSendResponse(v1::CanSendResponse {
            committed_count: 63,
            error: None,
        }))
    ));

    let partial = client
        .request(envelope::Payload::CanSendRequest(v1::CanSendRequest {
            session_id: session.session_id.clone(),
            lease: session.lease.clone(),
            frames: vec![classic_can_frame(64), classic_can_frame(65)],
        }))
        .await;
    match partial.payload.unwrap() {
        envelope::Payload::CanSendResponse(response) => {
            assert_eq!(response.committed_count, 1);
            assert_eq!(response.error.unwrap().name, "runtime.queue.full");
        }
        other => panic!("partial progress must be a CAN send response, got {other:?}"),
    }

    let stale = session.lease.clone().unwrap();
    let closed = client
        .request(envelope::Payload::CloseSessionRequest(
            v1::CloseSessionRequest {
                session_id: session.session_id,
                lease: session.lease,
            },
        ))
        .await;
    assert!(matches!(
        closed.payload,
        Some(envelope::Payload::CloseSessionResponse(_))
    ));
    let reopened = open_virtual_can(&mut client, Some(v1::CanMode::Classic)).await;
    let rejected = client
        .request(envelope::Payload::CanReceiveRequest(
            v1::CanReceiveRequest {
                session_id: reopened.session_id,
                lease: Some(stale),
                max_frames: 1,
                timeout_ms: 0,
            },
        ))
        .await;
    assert_eq!(error_name(&rejected), "runtime.lease.stale_generation");

    drop(client);
    assert!(server.await.unwrap().cleanup_error().is_none());
}

#[tokio::test]
async fn can_fd_configuration_and_frames_use_resource_capabilities() {
    let adapter = VirtualCanAdapter::loopback("can:virtual:broker-fd");
    let broker = Broker::with_startup_token(can_runtime(adapter), StartupToken::from_bytes(TOKEN));
    let (server_io, client_io) = tokio::io::duplex(64 * 1024);
    let server = tokio::spawn(async move { broker.serve_connection(server_io).await });
    let mut client = Client::new(client_io);
    handshake_can(&mut client).await;
    let enumerate = client
        .request(envelope::Payload::EnumerateCanRequest(
            v1::EnumerateCanRequest {},
        ))
        .await;
    let descriptor = match enumerate.payload.unwrap() {
        envelope::Payload::EnumerateCanResponse(response) => {
            response.resources.into_iter().next().unwrap()
        }
        other => panic!("expected CAN enumerate response, got {other:?}"),
    };
    let opened = client
        .request(envelope::Payload::OpenCanRequest(v1::OpenCanRequest {
            selector: Some(v1::ResourceSelector {
                resource_id: descriptor.resource_id,
                minimum_identity_quality: descriptor.identity_quality,
                transport: descriptor.transport,
            }),
            mode: v1::LeaseMode::Maintenance as i32,
            config: Some(v1::CanOpenConfig {
                config: Some(v1::can_open_config::Config::Configure(
                    v1::CanConfigureConfig {
                        mode: v1::CanMode::Fd as i32,
                        nominal: Some(v1::CanBitTiming {
                            bitrate: 500_000,
                            sample_point_permill: None,
                            sjw: None,
                        }),
                        data: Some(v1::CanBitTiming {
                            bitrate: 2_000_000,
                            sample_point_permill: None,
                            sjw: None,
                        }),
                        listen_only: false,
                        loopback: true,
                        restart_ms: None,
                    },
                )),
            }),
            filters: Some(can_filter_data()),
        }))
        .await;
    let session = match opened.payload.unwrap() {
        envelope::Payload::OpenCanResponse(response) => response,
        other => panic!("expected configured CAN response, got {other:?}"),
    };
    let sent = client
        .request(envelope::Payload::CanSendRequest(v1::CanSendRequest {
            session_id: session.session_id,
            lease: session.lease,
            frames: vec![v1::CanFrame {
                id: Some(v1::CanId {
                    value: 0x1abc,
                    format: v1::CanIdFormat::Extended as i32,
                }),
                kind: v1::CanFrameKind::FdData as i32,
                data: vec![0xa5; 12],
                bitrate_switch: true,
                error_state_indicator: false,
                ..Default::default()
            }],
        }))
        .await;
    assert!(matches!(
        sent.payload,
        Some(envelope::Payload::CanSendResponse(v1::CanSendResponse {
            committed_count: 1,
            error: None,
        }))
    ));
    drop(client);
    assert!(server.await.unwrap().cleanup_error().is_none());
}

#[tokio::test]
async fn can_fd_timestamp_response_frame_limit_rejects_before_dequeue() {
    let can_adapter = VirtualCanAdapter::loopback("can:virtual:broker-cleanup");
    let runtime = HalRuntime::builder()
        .can_adapter(can_adapter.clone())
        .build();
    let broker = Broker::with_startup_token(runtime.clone(), StartupToken::from_bytes(TOKEN));
    let (server_io, client_io) = tokio::io::duplex(64 * 1024);
    let server = tokio::spawn(async move { broker.serve_connection(server_io).await });
    let mut client = Client::new(client_io);
    let mut handshake = can_handshake();
    handshake.max_frame_bytes = 512;
    handshake.max_read_bytes = 128;
    handshake.max_write_bytes = 64;
    let accepted = client
        .request(envelope::Payload::HandshakeRequest(handshake))
        .await;
    assert!(matches!(
        accepted.payload,
        Some(envelope::Payload::HandshakeResponse(_))
    ));
    let can_session = open_configured_fd_can(&mut client).await;
    can_adapter
        .inject_received(
            CanFrame::fd_data(
                CanId::standard(0x123).unwrap(),
                Bytes::from(vec![7; 64]),
                true,
                true,
            )
            .unwrap(),
            None,
        )
        .unwrap();

    let oversized = client
        .request(envelope::Payload::CanReceiveRequest(
            v1::CanReceiveRequest {
                session_id: can_session.session_id.clone(),
                lease: can_session.lease.clone(),
                max_frames: 2,
                timeout_ms: 0,
            },
        ))
        .await;
    assert_eq!(error_name(&oversized), "runtime.protocol.invalid_message");

    let still_queued = client
        .request(envelope::Payload::CanReceiveRequest(
            v1::CanReceiveRequest {
                session_id: can_session.session_id,
                lease: can_session.lease,
                max_frames: 1,
                timeout_ms: 0,
            },
        ))
        .await;
    assert!(matches!(
        still_queued.payload,
        Some(envelope::Payload::CanReceiveResponse(_))
    ));

    drop(client);
    let outcome = server.await.unwrap();
    assert!(outcome.cleanup_error().is_none());
}

#[tokio::test]
async fn tight_classic_without_timestamp_receive_limits_are_admitted() {
    let adapter = MaskedCanAdapter::classic_only("can:virtual:broker-classic-tight");
    let injector = adapter.inner.clone();
    let runtime = HalRuntime::builder().can_adapter(adapter).build();
    let broker = Broker::with_startup_token(runtime, StartupToken::from_bytes(TOKEN));
    let (server_io, client_io) = tokio::io::duplex(64 * 1024);
    let server = tokio::spawn(async move { broker.serve_connection(server_io).await });
    let mut client = Client::new(client_io);
    let mut handshake = can_handshake();
    handshake.max_frame_bytes = 256;
    handshake.max_read_bytes = 8;
    handshake.max_write_bytes = 8;
    let accepted = client
        .request(envelope::Payload::HandshakeRequest(handshake))
        .await;
    assert!(matches!(
        accepted.payload,
        Some(envelope::Payload::HandshakeResponse(_))
    ));
    let session = open_virtual_can(&mut client, Some(v1::CanMode::Classic)).await;
    injector
        .inject_received(
            CanFrame::classic_data(CanId::standard(0x123).unwrap(), Bytes::from(vec![7; 8]))
                .unwrap(),
            None,
        )
        .unwrap();

    let receive = client
        .request(envelope::Payload::CanReceiveRequest(
            v1::CanReceiveRequest {
                session_id: session.session_id,
                lease: session.lease,
                max_frames: 1,
                timeout_ms: 0,
            },
        ))
        .await;
    match receive.payload.unwrap() {
        envelope::Payload::CanReceiveResponse(response) => {
            assert_eq!(response.frames.len(), 1);
            assert_eq!(response.frames[0].frame.as_ref().unwrap().data.len(), 8);
            assert!(response.frames[0].timestamp.is_none());
        }
        other => panic!("expected tight Classical CAN response, got {other:?}"),
    }

    drop(client);
    assert!(server.await.unwrap().cleanup_error().is_none());
}

#[tokio::test]
async fn nonconforming_direct_variant_from_adapter_is_rejected_before_wire_response() {
    let adapter = VirtualCanAdapter::loopback("can:virtual:broker-invalid-direct-frame");
    let runtime = can_runtime(adapter.clone());
    let broker = Broker::with_startup_token(runtime, StartupToken::from_bytes(TOKEN));
    let (server_io, client_io) = tokio::io::duplex(64 * 1024);
    let server = tokio::spawn(async move { broker.serve_connection(server_io).await });
    let mut client = Client::new(client_io);
    handshake_can(&mut client).await;
    let session = open_virtual_can(&mut client, Some(v1::CanMode::Classic)).await;
    adapter
        .inject_received(
            CanFrame::ClassicData {
                id: CanId::Standard(0x800),
                data: Bytes::from(vec![0xa5; 9]),
            },
            None,
        )
        .unwrap();

    let rejected = client
        .request(envelope::Payload::CanReceiveRequest(
            v1::CanReceiveRequest {
                session_id: session.session_id.clone(),
                lease: session.lease.clone(),
                max_frames: 1,
                timeout_ms: 100,
            },
        ))
        .await;
    assert_eq!(error_name(&rejected), "can.frame.invalid");

    adapter
        .inject_received(
            CanFrame::classic_data(CanId::standard(0x123).unwrap(), Bytes::from_static(&[7]))
                .unwrap(),
            None,
        )
        .unwrap();
    let valid = client
        .request(envelope::Payload::CanReceiveRequest(
            v1::CanReceiveRequest {
                session_id: session.session_id,
                lease: session.lease,
                max_frames: 1,
                timeout_ms: 100,
            },
        ))
        .await;
    match valid.payload.unwrap() {
        envelope::Payload::CanReceiveResponse(response) => {
            assert_eq!(response.frames.len(), 1);
            assert_eq!(response.frames[0].frame.as_ref().unwrap().data, [7]);
        }
        other => panic!("expected valid frame after rejected adapter frame, got {other:?}"),
    }

    drop(client);
    assert!(server.await.unwrap().cleanup_error().is_none());
}

#[tokio::test]
async fn disconnect_restores_configured_can_and_releases_can_and_serial() {
    let can_adapter = VirtualCanAdapter::loopback("can:virtual:broker-disconnect-configure");
    let runtime = HalRuntime::builder()
        .serial_adapter(VirtualSerialAdapter::loopback(
            "serial:virtual:broker-disconnect-configure",
        ))
        .can_adapter(can_adapter)
        .build();
    let broker = Broker::with_startup_token(runtime.clone(), StartupToken::from_bytes(TOKEN));
    let (server_io, client_io) = tokio::io::duplex(64 * 1024);
    let server = tokio::spawn(async move { broker.serve_connection(server_io).await });
    let mut client = Client::new(client_io);
    handshake_can(&mut client).await;
    let _configured_can = open_configured_classic_can(&mut client).await;
    let _serial = open_virtual_serial(&mut client, 1_000).await;

    drop(client);
    let outcome = server.await.unwrap();
    assert!(outcome.cleanup_error().is_none());

    let can_descriptor = runtime.enumerate_can().await.unwrap().remove(0);
    runtime
        .open_can(
            OwnerId::parse("broker-contract:disconnect-can-reuse").unwrap(),
            can_descriptor.selector(),
            seeed_hal_core::LeaseMode::Control,
            CanOpenConfig::Attach(
                CanLinkExpectation::new(
                    Some(CanMode::Classic),
                    Some(500_000),
                    None,
                    Some(false),
                    Some(true),
                )
                .unwrap(),
            ),
            seeed_hal_can::CanFilterSet::new(vec![]).unwrap(),
        )
        .await
        .unwrap()
        .close()
        .await
        .unwrap();

    let serial_descriptor = runtime.enumerate_serial().await.unwrap().remove(0);
    runtime
        .open_serial(
            OwnerId::parse("broker-contract:disconnect-serial-reuse").unwrap(),
            serial_descriptor.selector(),
            SerialConfig::default(),
        )
        .await
        .unwrap()
        .close()
        .await
        .unwrap();
}

#[tokio::test]
async fn broker_uses_selected_descriptor_capabilities_without_normalizing_them() {
    let adapter = MaskedCanAdapter::classic_only("can:virtual:broker-capabilities");
    let broker = Broker::with_startup_token(
        HalRuntime::builder().can_adapter(adapter).build(),
        StartupToken::from_bytes(TOKEN),
    );
    let (server_io, client_io) = tokio::io::duplex(64 * 1024);
    let server = tokio::spawn(async move { broker.serve_connection(server_io).await });
    let mut client = Client::new(client_io);
    handshake_can(&mut client).await;
    let enumerate = client
        .request(envelope::Payload::EnumerateCanRequest(
            v1::EnumerateCanRequest {},
        ))
        .await;
    let descriptor = match enumerate.payload.unwrap() {
        envelope::Payload::EnumerateCanResponse(response) => {
            response.resources.into_iter().next().unwrap()
        }
        other => panic!("expected CAN enumerate response, got {other:?}"),
    };

    let fd_open = client
        .request(envelope::Payload::OpenCanRequest(v1::OpenCanRequest {
            selector: Some(v1::ResourceSelector {
                resource_id: descriptor.resource_id.clone(),
                minimum_identity_quality: descriptor.identity_quality,
                transport: descriptor.transport,
            }),
            mode: v1::LeaseMode::Control as i32,
            config: Some(can_attach(Some(v1::CanMode::Fd))),
            filters: Some(can_filter_data()),
        }))
        .await;
    assert_eq!(
        error_name(&fd_open),
        "runtime.protocol.capability_unsupported"
    );

    let configure_open = client
        .request(envelope::Payload::OpenCanRequest(v1::OpenCanRequest {
            selector: Some(v1::ResourceSelector {
                resource_id: descriptor.resource_id,
                minimum_identity_quality: descriptor.identity_quality,
                transport: descriptor.transport,
            }),
            mode: v1::LeaseMode::Maintenance as i32,
            config: Some(v1::CanOpenConfig {
                config: Some(v1::can_open_config::Config::Configure(
                    v1::CanConfigureConfig {
                        mode: v1::CanMode::Classic as i32,
                        nominal: Some(v1::CanBitTiming {
                            bitrate: 500_000,
                            sample_point_permill: None,
                            sjw: None,
                        }),
                        data: None,
                        listen_only: false,
                        loopback: true,
                        restart_ms: None,
                    },
                )),
            }),
            filters: Some(can_filter_data()),
        }))
        .await;
    assert_eq!(
        error_name(&configure_open),
        "runtime.protocol.capability_unsupported"
    );

    let session = open_virtual_can(&mut client, Some(v1::CanMode::Classic)).await;
    let error_send = client
        .request(envelope::Payload::CanSendRequest(v1::CanSendRequest {
            session_id: session.session_id.clone(),
            lease: session.lease.clone(),
            frames: vec![v1::CanFrame {
                kind: v1::CanFrameKind::Error as i32,
                data: vec![1],
                error_classes: vec![v1::CanErrorClass::BusError as i32],
                ..Default::default()
            }],
        }))
        .await;
    assert_eq!(
        error_name(&error_send),
        "runtime.protocol.capability_unsupported"
    );

    let timestamped = client
        .request(envelope::Payload::CanSendRequest(v1::CanSendRequest {
            session_id: session.session_id.clone(),
            lease: session.lease.clone(),
            frames: vec![classic_can_frame(7)],
        }))
        .await;
    assert!(matches!(
        timestamped.payload,
        Some(envelope::Payload::CanSendResponse(_))
    ));
    let timestamp_delivery = client
        .request(envelope::Payload::CanReceiveRequest(
            v1::CanReceiveRequest {
                session_id: session.session_id.clone(),
                lease: session.lease.clone(),
                max_frames: 1,
                timeout_ms: 100,
            },
        ))
        .await;
    assert_eq!(
        error_name(&timestamp_delivery),
        "runtime.protocol.capability_unsupported"
    );

    let error_filter = client
        .request(envelope::Payload::ReplaceCanFiltersRequest(
            v1::ReplaceCanFiltersRequest {
                session_id: session.session_id,
                lease: session.lease,
                filters: Some(v1::CanFilterSet {
                    filters: vec![v1::CanFilter {
                        id: 0,
                        mask: 0,
                        format: v1::CanIdFormat::Either as i32,
                        classes: Some(v1::CanFrameClasses {
                            data: false,
                            remote: false,
                            error: true,
                        }),
                    }],
                }),
            },
        ))
        .await;
    assert_eq!(
        error_name(&error_filter),
        "runtime.protocol.capability_unsupported"
    );

    drop(client);
    assert!(server.await.unwrap().cleanup_error().is_none());
}

#[tokio::test]
async fn advertised_error_frame_and_filter_capabilities_are_usable() {
    let adapter = VirtualCanAdapter::loopback("can:virtual:broker-error-frames");
    let broker = Broker::with_startup_token(can_runtime(adapter), StartupToken::from_bytes(TOKEN));
    let (server_io, client_io) = tokio::io::duplex(64 * 1024);
    let server = tokio::spawn(async move { broker.serve_connection(server_io).await });
    let mut client = Client::new(client_io);
    handshake_can(&mut client).await;
    let session = open_virtual_can(&mut client, Some(v1::CanMode::Classic)).await;
    let filters = client
        .request(envelope::Payload::ReplaceCanFiltersRequest(
            v1::ReplaceCanFiltersRequest {
                session_id: session.session_id.clone(),
                lease: session.lease.clone(),
                filters: Some(v1::CanFilterSet {
                    filters: vec![v1::CanFilter {
                        id: 0,
                        mask: 0,
                        format: v1::CanIdFormat::Either as i32,
                        classes: Some(v1::CanFrameClasses {
                            data: false,
                            remote: false,
                            error: true,
                        }),
                    }],
                }),
            },
        ))
        .await;
    assert!(matches!(
        filters.payload,
        Some(envelope::Payload::ReplaceCanFiltersResponse(_))
    ));
    let send = client
        .request(envelope::Payload::CanSendRequest(v1::CanSendRequest {
            session_id: session.session_id.clone(),
            lease: session.lease.clone(),
            frames: vec![v1::CanFrame {
                kind: v1::CanFrameKind::Error as i32,
                data: vec![0xa5],
                error_classes: vec![v1::CanErrorClass::BusError as i32],
                ..Default::default()
            }],
        }))
        .await;
    assert!(matches!(
        send.payload,
        Some(envelope::Payload::CanSendResponse(v1::CanSendResponse {
            committed_count: 1,
            error: None,
        }))
    ));
    let receive = client
        .request(envelope::Payload::CanReceiveRequest(
            v1::CanReceiveRequest {
                session_id: session.session_id,
                lease: session.lease,
                max_frames: 1,
                timeout_ms: 100,
            },
        ))
        .await;
    match receive.payload.unwrap() {
        envelope::Payload::CanReceiveResponse(response) => {
            assert_eq!(
                response.frames[0].frame.as_ref().unwrap().kind,
                v1::CanFrameKind::Error as i32
            );
        }
        other => panic!("expected error-frame receive response, got {other:?}"),
    }
    drop(client);
    assert!(server.await.unwrap().cleanup_error().is_none());
}

#[tokio::test]
async fn can_sessions_from_another_connection_fail_closed_before_runtime() {
    let adapter = VirtualCanAdapter::loopback("can:virtual:broker-owner-registry");
    let runtime = can_runtime(adapter.clone());
    let broker = Broker::with_startup_token(runtime, StartupToken::from_bytes(TOKEN));
    let (server_one_io, client_one_io) = tokio::io::duplex(64 * 1024);
    let (server_two_io, client_two_io) = tokio::io::duplex(64 * 1024);
    let broker_one = broker.clone();
    let server_one = tokio::spawn(async move { broker_one.serve_connection(server_one_io).await });
    let server_two = tokio::spawn(async move { broker.serve_connection(server_two_io).await });
    let mut client_one = Client::new(client_one_io);
    let mut client_two = Client::new(client_two_io);
    handshake_can(&mut client_one).await;
    handshake_can(&mut client_two).await;
    let foreign = open_virtual_can(&mut client_one, Some(v1::CanMode::Classic)).await;

    let requests = [
        envelope::Payload::CanSendRequest(v1::CanSendRequest {
            session_id: foreign.session_id.clone(),
            lease: foreign.lease.clone(),
            frames: vec![classic_can_frame(1)],
        }),
        envelope::Payload::CanReceiveRequest(v1::CanReceiveRequest {
            session_id: foreign.session_id.clone(),
            lease: foreign.lease.clone(),
            max_frames: 1,
            timeout_ms: 0,
        }),
        envelope::Payload::ReplaceCanFiltersRequest(v1::ReplaceCanFiltersRequest {
            session_id: foreign.session_id.clone(),
            lease: foreign.lease.clone(),
            filters: Some(can_filter_data()),
        }),
        envelope::Payload::GetCanBusStatusRequest(v1::GetCanBusStatusRequest {
            session_id: foreign.session_id.clone(),
            lease: foreign.lease.clone(),
        }),
    ];
    for request in requests {
        let rejected = client_two.request(request).await;
        assert_eq!(error_name(&rejected), "runtime.session.not_found");
    }
    assert!(
        adapter.transmitted_frames().is_empty(),
        "foreign broker sessions must not reach the CAN adapter"
    );
    let foreign_close = client_two
        .request(envelope::Payload::CloseSessionRequest(
            v1::CloseSessionRequest {
                session_id: foreign.session_id.clone(),
                lease: foreign.lease.clone(),
            },
        ))
        .await;
    assert_eq!(error_name(&foreign_close), "runtime.session.not_found");
    let owner_status = client_one
        .request(envelope::Payload::GetCanBusStatusRequest(
            v1::GetCanBusStatusRequest {
                session_id: foreign.session_id,
                lease: foreign.lease,
            },
        ))
        .await;
    assert!(matches!(
        owner_status.payload,
        Some(envelope::Payload::GetCanBusStatusResponse(_))
    ));

    drop(client_one);
    drop(client_two);
    assert!(server_one.await.unwrap().cleanup_error().is_none());
    assert!(server_two.await.unwrap().cleanup_error().is_none());
}

#[tokio::test]
async fn can_health_events_are_exhaustively_owner_scoped() {
    let adapter = VirtualCanAdapter::loopback("can:virtual:broker-health-events");
    let runtime = can_runtime(adapter.clone());
    let broker = Broker::with_startup_token(runtime, StartupToken::from_bytes(TOKEN));
    let (server_one_io, client_one_io) = tokio::io::duplex(64 * 1024);
    let (server_two_io, client_two_io) = tokio::io::duplex(64 * 1024);
    let broker_one = broker.clone();
    let server_one = tokio::spawn(async move { broker_one.serve_connection(server_one_io).await });
    let server_two = tokio::spawn(async move { broker.serve_connection(server_two_io).await });
    let mut client_one = Client::new(client_one_io);
    let mut client_two = Client::new(client_two_io);
    handshake_can(&mut client_one).await;
    handshake_can(&mut client_two).await;
    let session = open_virtual_can(&mut client_one, Some(v1::CanMode::Classic)).await;
    let baseline = client_one
        .request(envelope::Payload::GetCanBusStatusRequest(
            v1::GetCanBusStatusRequest {
                session_id: session.session_id.clone(),
                lease: session.lease.clone(),
            },
        ))
        .await;
    assert!(matches!(
        baseline.payload,
        Some(envelope::Payload::GetCanBusStatusResponse(_))
    ));
    adapter.set_bus_status(CanBusStatus::new(CanBusState::Warning, Some(1), Some(2)));
    client_one
        .send(
            700,
            envelope::Payload::GetCanBusStatusRequest(v1::GetCanBusStatusRequest {
                session_id: session.session_id,
                lease: session.lease,
            }),
        )
        .await;
    let mut saw_response = false;
    let mut saw_health = false;
    while !saw_response || !saw_health {
        let response = client_one.recv().await;
        match response.payload.unwrap() {
            envelope::Payload::GetCanBusStatusResponse(_) => {
                assert_eq!(response.request_id, 700);
                saw_response = true;
            }
            envelope::Payload::RuntimeEvent(event) => {
                assert_eq!(response.request_id, 0);
                assert_eq!(event.name, "can.bus.warning");
                saw_health = true;
            }
            other => panic!("unexpected health-event payload: {other:?}"),
        }
    }
    assert!(
        tokio::time::timeout(
            std::time::Duration::from_millis(50),
            client_two.framed.next(),
        )
        .await
        .is_err(),
        "CAN health events must not leak to another connection owner"
    );

    drop(client_one);
    drop(client_two);
    assert!(server_one.await.unwrap().cleanup_error().is_none());
    assert!(server_two.await.unwrap().cleanup_error().is_none());
}

#[tokio::test]
async fn can_receive_lag_is_reported_once_and_newest_frame_remains_available() {
    let adapter = VirtualCanAdapter::loopback("can:virtual:broker-lag");
    let runtime = HalRuntime::builder()
        .can_adapter(adapter)
        .can_rx_capacity(1)
        .build();
    let broker = Broker::with_startup_token(runtime, StartupToken::from_bytes(TOKEN));
    let (server_io, client_io) = tokio::io::duplex(64 * 1024);
    let server = tokio::spawn(async move { broker.serve_connection(server_io).await });
    let mut client = Client::new(client_io);
    handshake_can(&mut client).await;
    let session = open_virtual_can(&mut client, Some(v1::CanMode::Classic)).await;
    let send = client
        .request(envelope::Payload::CanSendRequest(v1::CanSendRequest {
            session_id: session.session_id.clone(),
            lease: session.lease.clone(),
            frames: vec![
                classic_can_frame(1),
                classic_can_frame(2),
                classic_can_frame(3),
            ],
        }))
        .await;
    assert!(matches!(
        send.payload,
        Some(envelope::Payload::CanSendResponse(_))
    ));
    let lagged = client
        .request(envelope::Payload::CanReceiveRequest(
            v1::CanReceiveRequest {
                session_id: session.session_id.clone(),
                lease: session.lease.clone(),
                max_frames: 1,
                timeout_ms: 100,
            },
        ))
        .await;
    assert_eq!(error_name(&lagged), "can.receive.lagged");
    let newest = client
        .request(envelope::Payload::CanReceiveRequest(
            v1::CanReceiveRequest {
                session_id: session.session_id,
                lease: session.lease,
                max_frames: 1,
                timeout_ms: 100,
            },
        ))
        .await;
    match newest.payload.unwrap() {
        envelope::Payload::CanReceiveResponse(response) => {
            assert_eq!(response.frames[0].frame.as_ref().unwrap().data, [3]);
        }
        other => panic!("expected newest CAN frame after lag, got {other:?}"),
    }
    drop(client);
    assert!(server.await.unwrap().cleanup_error().is_none());
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
