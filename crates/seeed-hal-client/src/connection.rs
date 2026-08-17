use std::collections::{HashMap, HashSet, VecDeque};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use bytes::{Bytes, BytesMut};
use futures_util::{SinkExt, StreamExt};
use prost::Message;
use seeed_hal_camera::{
    CAMERA_CAPTURE_CAPABILITY, CAMERA_CONTROLS_CAPABILITY, CAMERA_FRAMES_SHM_CAPABILITY,
    CameraRequest,
};
use seeed_hal_can::{
    CAN_CLASSIC_CAPABILITY, CAN_CONFIGURE_CAPABILITY, CAN_ERROR_FRAMES_CAPABILITY,
    CAN_FD_CAPABILITY, CAN_RX_TIMESTAMP_CAPABILITY, CanFilterSet, CanMode, CanOpenConfig,
    can_classic_capability, can_configure_capability, can_error_frames_capability,
    can_fd_capability,
};
use seeed_hal_core::{
    ErrorCategory, HalError, HalResult, LeaseMode, ResourceDescriptor, ResourceId,
    ResourceSelector, SessionId,
};
use seeed_hal_gpio::{
    GPIO_EDGES_CAPABILITY, GPIO_LINES_CAPABILITY, GpioLineConfig, MAX_GPIO_EVENTS,
};
use seeed_hal_protocol::v1::{self, envelope};
use seeed_hal_protocol::{
    MAX_FRAME_BYTES, PROTOCOL_MAJOR, PROTOCOL_MINOR, SERIAL_CAPABILITY,
    can_receive_response_from_proto, can_send_response_from_proto,
    enumerate_can_response_from_proto, enumerate_serial_response_from_proto, error_from_proto,
    get_can_bus_status_response_from_proto, gpio_next_edge_response_from_proto,
    gpio_read_response_from_proto, open_can_response_from_proto, open_gpio_response_from_proto,
    open_usb_response_from_proto, usb_transfer_response_from_proto,
};
use seeed_hal_protocol::{
    PROTOCOL_MINOR_MAXIMUM, PROTOCOL_MINOR_MINIMUM, handshake_response_minor_range,
};
use seeed_hal_serial::SerialConfig;
use seeed_hal_usb::{
    USB_BULK_CAPABILITY, USB_CONTROL_CAPABILITY, USB_INTERRUPT_CAPABILITY, UsbInterfaceClaim,
    UsbTransfer,
};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::{broadcast, mpsc, oneshot, watch};
use tokio::task::JoinHandle;
use tokio_util::codec::{Framed, LengthDelimitedCodec};
use zeroize::{Zeroize, ZeroizeOnDrop};

use crate::{
    RemoteCameraHandle, RemoteCanHandle, RemoteGpioHandle, RemoteSerialHandle, RemoteUsbHandle,
};

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
    shutdown: watch::Receiver<bool>,
}

impl EventSubscription {
    pub async fn recv(&mut self) -> HalResult<ClientEvent> {
        loop {
            match self.receiver.try_recv() {
                Ok(result) => return result,
                Err(broadcast::error::TryRecvError::Lagged(skipped)) => {
                    return Err(event_lagged_error(skipped));
                }
                Err(broadcast::error::TryRecvError::Closed) => {
                    return Err(event_closed_error());
                }
                Err(broadcast::error::TryRecvError::Empty) => {}
            }
            if *self.shutdown.borrow() {
                return Err(event_closed_error());
            }
            tokio::select! {
                biased;
                event = self.receiver.recv() => {
                    return match event {
                        Ok(result) => result,
                        Err(broadcast::error::RecvError::Closed) => Err(event_closed_error()),
                        Err(broadcast::error::RecvError::Lagged(skipped)) => {
                            Err(event_lagged_error(skipped))
                        }
                    };
                }
                _ = self.shutdown.changed() => {}
            }
        }
    }
}

fn event_lagged_error(skipped: u64) -> HalError {
    client_error(
        "runtime.event.lagged",
        ErrorCategory::Unavailable,
        "runtime.event.receive",
        true,
        format!("event subscriber fell behind by {skipped} events"),
    )
}

fn event_closed_error() -> HalError {
    client_error(
        "runtime.event.closed",
        ErrorCategory::Unavailable,
        "runtime.event.receive",
        false,
        "the client event stream is closed",
    )
}

fn camera_request_to_proto(request: &CameraRequest) -> v1::CameraRequest {
    v1::CameraRequest {
        format: Some(v1::CameraFormat {
            pixel_format: match request.format().pixel_format() {
                seeed_hal_camera::CameraPixelFormat::Nv12 => v1::CameraPixelFormat::Nv12,
                seeed_hal_camera::CameraPixelFormat::Yuyv => v1::CameraPixelFormat::Yuyv,
                seeed_hal_camera::CameraPixelFormat::Mjpeg => v1::CameraPixelFormat::Mjpeg,
            } as i32,
            width: request.format().width(),
            height: request.format().height(),
        }),
        slot_count: request.slot_count() as u32,
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct CanSessionProfile {
    pub(crate) mode: CanMode,
    pub(crate) classic_frames: bool,
    pub(crate) fd_frames: bool,
    pub(crate) error_frames: bool,
    pub(crate) timestamps: bool,
    pub(crate) resource_id: ResourceId,
    pub(crate) session_id: SessionId,
}

#[derive(Clone, Eq, PartialEq)]
pub(crate) enum ExpectedResponse {
    EnumerateSerial,
    OpenSerial,
    SerialRead {
        max_bytes: usize,
    },
    SerialWrite,
    SerialFlush,
    SetControlLines,
    CloseSession,
    CloseCan {
        profile: CanSessionProfile,
    },
    EnumerateCan,
    OpenCan {
        mode: LeaseMode,
        resource_id: ResourceId,
    },
    CanSend {
        input_count: usize,
        profile: CanSessionProfile,
    },
    CanReceive {
        max_frames: usize,
        max_read_bytes: usize,
        profile: CanSessionProfile,
    },
    ReplaceCanFilters {
        profile: CanSessionProfile,
    },
    CanBusStatus {
        profile: CanSessionProfile,
    },
    EnumerateUsb,
    OpenUsb {
        resource_id: ResourceId,
    },
    UsbTransfer {
        max_read_bytes: usize,
        resource_id: ResourceId,
    },
    CloseUsb {
        resource_id: ResourceId,
    },
    EnumerateGpio,
    OpenGpio {
        line_count: usize,
        resource_id: ResourceId,
    },
    GpioRead {
        line_count: usize,
        resource_id: ResourceId,
    },
    GpioWrite {
        resource_id: ResourceId,
    },
    GpioNextEdge {
        resource_id: ResourceId,
    },
    CloseGpio {
        resource_id: ResourceId,
    },
    EnumerateCamera,
    OpenCamera {
        resource_id: ResourceId,
    },
    CaptureCamera {
        resource_id: ResourceId,
    },
    CameraMappingDescriptor {
        resource_id: ResourceId,
    },
    CameraNextFrameLease {
        resource_id: ResourceId,
    },
    CameraDroppedCount {
        resource_id: ResourceId,
    },
    CameraGetControl {
        resource_id: ResourceId,
    },
    CameraSetControl {
        resource_id: ResourceId,
    },
    CameraSetAuto {
        resource_id: ResourceId,
    },
    CloseCamera {
        resource_id: ResourceId,
    },
}

impl ExpectedResponse {
    fn resource_id(&self) -> Option<&ResourceId> {
        match self {
            Self::OpenCan { resource_id, .. } => Some(resource_id),
            Self::CanSend { profile, .. }
            | Self::CanReceive { profile, .. }
            | Self::ReplaceCanFilters { profile }
            | Self::CanBusStatus { profile }
            | Self::CloseCan { profile } => Some(&profile.resource_id),
            Self::OpenUsb { resource_id }
            | Self::UsbTransfer { resource_id, .. }
            | Self::CloseUsb { resource_id }
            | Self::OpenGpio { resource_id, .. }
            | Self::GpioRead { resource_id, .. }
            | Self::GpioWrite { resource_id }
            | Self::GpioNextEdge { resource_id }
            | Self::CloseGpio { resource_id }
            | Self::OpenCamera { resource_id }
            | Self::CaptureCamera { resource_id }
            | Self::CameraMappingDescriptor { resource_id }
            | Self::CameraNextFrameLease { resource_id }
            | Self::CameraDroppedCount { resource_id }
            | Self::CameraGetControl { resource_id }
            | Self::CameraSetControl { resource_id }
            | Self::CameraSetAuto { resource_id }
            | Self::CloseCamera { resource_id } => Some(resource_id),
            _ => None,
        }
    }
}

struct PendingRequest {
    expected: ExpectedResponse,
    reply: oneshot::Sender<HalResult<envelope::Payload>>,
}

enum CorrelatedResponse {
    Pending(PendingRequest),
    Cancelled(ExpectedResponse),
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
    protocol_minor: u32,
    frame: usize,
    read: usize,
    write: usize,
    can_classic: bool,
    can_fd: bool,
    can_configure: bool,
    can_error_frames: bool,
    #[allow(dead_code)]
    // Retained negotiated limit for the forthcoming timestamped receive surface.
    can_rx_timestamp: bool,
    usb_control: bool,
    usb_bulk: bool,
    usb_interrupt: bool,
    gpio_lines: bool,
    gpio_edges: bool,
    camera_capture: bool,
    camera_frames_shm: bool,
    camera_controls: bool,
}

struct Shared {
    requests: Mutex<RequestState>,
    limits: Mutex<Limits>,
    pending_capacity: usize,
    tombstone_capacity: usize,
    writer: mpsc::Sender<Outbound>,
    events: broadcast::Sender<HalResult<ClientEvent>>,
    shutdown: watch::Sender<bool>,
    #[cfg(test)]
    inbound_test_hooks: Option<Arc<InboundTestHooks>>,
}

#[cfg(test)]
struct InboundTestGate {
    reached: tokio::sync::Semaphore,
    release: tokio::sync::Semaphore,
}

#[cfg(test)]
impl InboundTestGate {
    fn new() -> Self {
        Self {
            reached: tokio::sync::Semaphore::new(0),
            release: tokio::sync::Semaphore::new(0),
        }
    }

    async fn pause(&self) {
        self.reached.add_permits(1);
        self.release
            .acquire()
            .await
            .expect("inbound test gate remains open")
            .forget();
    }

    async fn wait_until_reached(&self) {
        self.reached
            .acquire()
            .await
            .expect("inbound test gate remains open")
            .forget();
    }

    fn release(&self) {
        self.release.add_permits(1);
    }
}

#[cfg(test)]
struct InboundTestHooks {
    after_frame: Option<Arc<InboundTestGate>>,
    after_preflight: Option<Arc<InboundTestGate>>,
    decode_calls: std::sync::atomic::AtomicUsize,
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

    pub fn protocol_minor(&self) -> u32 {
        self.inner
            .shared
            .limits
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .protocol_minor
    }

    async fn from_io<T>(io: T, options: ConnectionOptions) -> HalResult<Self>
    where
        T: AsyncRead + AsyncWrite + Unpin + Send + 'static,
    {
        let requested = Limits {
            protocol_minor: PROTOCOL_MINOR_MAXIMUM,
            frame: options.max_frame_bytes,
            read: options.max_read_bytes,
            write: options.max_write_bytes,
            can_classic: false,
            can_fd: false,
            can_configure: false,
            can_error_frames: false,
            can_rx_timestamp: false,
            usb_control: false,
            usb_bulk: false,
            usb_interrupt: false,
            gpio_lines: false,
            gpio_edges: false,
            camera_capture: false,
            camera_frames_shm: false,
            camera_controls: false,
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
            #[cfg(test)]
            inbound_test_hooks: None,
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
        let result = enumerate_serial_response_from_proto(response);
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
        if selector.transport() != seeed_hal_core::TransportKind::Serial {
            return Err(client_error(
                "runtime.argument.invalid",
                ErrorCategory::InvalidArgument,
                "serial.open",
                false,
                "serial resource selector transport must be Serial",
            )
            .with_resource_id(selector.id().clone()));
        }
        let payload = self
            .request(
                envelope::Payload::OpenSerialRequest(v1::OpenSerialRequest {
                    selector: Some((&selector).try_into()?),
                    config: Some((&config).into()),
                }),
                ExpectedResponse::OpenSerial,
            )
            .await?;
        let envelope::Payload::OpenSerialResponse(response) = payload else {
            unreachable!()
        };
        let result =
            RemoteSerialHandle::from_response(self.clone(), selector.id().clone(), response);
        if let Err(error) = &result {
            terminate(&self.inner.shared, error.clone());
        }
        result
    }

    pub async fn enumerate_can(&self) -> HalResult<Vec<ResourceDescriptor>> {
        self.require_can_capability("can.enumerate", None, |limits| {
            limits.can_classic || limits.can_fd
        })?;
        let payload = self
            .request(
                envelope::Payload::EnumerateCanRequest(v1::EnumerateCanRequest {}),
                ExpectedResponse::EnumerateCan,
            )
            .await?;
        let envelope::Payload::EnumerateCanResponse(response) = payload else {
            unreachable!()
        };
        // The reader already validated this response before correlation. Keep
        // conversion here as the single protobuf-to-public-type mapping.
        let result = enumerate_can_response_from_proto(response);
        if let Err(error) = &result {
            self.fail(error.clone());
        }
        result
    }

    pub async fn enumerate_usb(&self) -> HalResult<Vec<ResourceDescriptor>> {
        self.require_minor_two("usb.enumerate", None, |limits| limits.usb_control)?;
        let payload = self
            .send(
                envelope::Payload::EnumerateUsbRequest(v1::EnumerateUsbRequest {}),
                ExpectedResponse::EnumerateUsb,
            )
            .await?;
        let envelope::Payload::EnumerateUsbResponse(response) = payload else {
            unreachable!()
        };
        let result: HalResult<Vec<ResourceDescriptor>> = response
            .resources
            .into_iter()
            .map(TryInto::try_into)
            .collect();
        if let Err(error) = &result {
            self.fail(error.clone());
        }
        result
    }

    pub async fn open_usb(
        &self,
        selector: ResourceSelector,
        claim: UsbInterfaceClaim,
    ) -> HalResult<RemoteUsbHandle> {
        if selector.transport() != seeed_hal_core::TransportKind::Usb {
            return Err(client_error(
                "runtime.argument.invalid",
                ErrorCategory::InvalidArgument,
                "usb.open",
                false,
                "USB resource selector transport must be Usb",
            )
            .with_resource_id(selector.id().clone()));
        }
        self.require_minor_two("usb.open", Some(selector.id()), |limits| limits.usb_control)?;
        let payload = envelope::Payload::OpenUsbRequest(v1::OpenUsbRequest {
            selector: Some((&selector).try_into()?),
            interface_number: u32::from(claim.number()),
        });
        self.ensure_payload_for_resource(&payload, "usb.open", selector.id())?;
        let payload = self
            .send(
                payload,
                ExpectedResponse::OpenUsb {
                    resource_id: selector.id().clone(),
                },
            )
            .await?;
        let envelope::Payload::OpenUsbResponse(response) = payload else {
            unreachable!()
        };
        RemoteUsbHandle::from_response(self.clone(), selector.id().clone(), response).inspect_err(
            |error| {
                self.fail(error.clone());
            },
        )
    }

    pub async fn enumerate_gpio(&self) -> HalResult<Vec<ResourceDescriptor>> {
        self.require_minor_two("gpio.enumerate", None, |limits| limits.gpio_lines)?;
        let payload = self
            .send(
                envelope::Payload::EnumerateGpioRequest(v1::EnumerateGpioRequest {}),
                ExpectedResponse::EnumerateGpio,
            )
            .await?;
        let envelope::Payload::EnumerateGpioResponse(response) = payload else {
            unreachable!()
        };
        let result: HalResult<Vec<ResourceDescriptor>> = response
            .resources
            .into_iter()
            .map(TryInto::try_into)
            .collect();
        if let Err(error) = &result {
            self.fail(error.clone());
        }
        result
    }

    pub async fn open_gpio(
        &self,
        selector: ResourceSelector,
        lines: Vec<u32>,
        config: GpioLineConfig,
    ) -> HalResult<RemoteGpioHandle> {
        if selector.transport() != seeed_hal_core::TransportKind::Gpio
            || lines.is_empty()
            || lines.len() > MAX_GPIO_EVENTS
        {
            return Err(client_error(
                "runtime.argument.invalid",
                ErrorCategory::InvalidArgument,
                "gpio.open",
                false,
                "GPIO selector or lines are invalid",
            )
            .with_resource_id(selector.id().clone()));
        }
        self.require_minor_two("gpio.open", Some(selector.id()), |limits| limits.gpio_lines)?;
        let payload = envelope::Payload::OpenGpioRequest(v1::OpenGpioRequest {
            selector: Some((&selector).try_into()?),
            lines: lines.clone(),
            config: Some((&config).into()),
        });
        self.ensure_payload_for_resource(&payload, "gpio.open", selector.id())?;
        let payload = self
            .send(
                payload,
                ExpectedResponse::OpenGpio {
                    line_count: lines.len(),
                    resource_id: selector.id().clone(),
                },
            )
            .await?;
        let envelope::Payload::OpenGpioResponse(response) = payload else {
            unreachable!()
        };
        RemoteGpioHandle::from_response(self.clone(), selector.id().clone(), lines.len(), response)
            .inspect_err(|error| {
                self.fail(error.clone());
            })
    }

    pub async fn enumerate_camera(&self) -> HalResult<Vec<ResourceDescriptor>> {
        self.require_camera_capability("camera.enumerate", None, |limits| limits.camera_capture)?;
        let payload = self
            .send(
                envelope::Payload::EnumerateCameraRequest(v1::EnumerateCameraRequest {}),
                ExpectedResponse::EnumerateCamera,
            )
            .await?;
        let envelope::Payload::EnumerateCameraResponse(response) = payload else {
            unreachable!()
        };
        let result: HalResult<Vec<ResourceDescriptor>> = response
            .resources
            .into_iter()
            .map(TryInto::try_into)
            .collect();
        if let Err(error) = &result {
            self.fail(error.clone());
        }
        result
    }

    pub async fn open_camera(
        &self,
        selector: ResourceSelector,
        request: CameraRequest,
    ) -> HalResult<RemoteCameraHandle> {
        if selector.transport() != seeed_hal_core::TransportKind::Camera {
            return Err(client_error(
                "runtime.argument.invalid",
                ErrorCategory::InvalidArgument,
                "camera.open",
                false,
                "camera resource selector transport must be Camera",
            )
            .with_resource_id(selector.id().clone()));
        }
        self.require_camera_capability("camera.open", Some(selector.id()), |limits| {
            limits.camera_capture
        })?;
        let payload = envelope::Payload::OpenCameraRequest(v1::OpenCameraRequest {
            selector: Some((&selector).try_into()?),
            request: Some(camera_request_to_proto(&request)),
        });
        self.ensure_payload_for_resource(&payload, "camera.open", selector.id())?;
        let payload = self
            .send(
                payload,
                ExpectedResponse::OpenCamera {
                    resource_id: selector.id().clone(),
                },
            )
            .await?;
        let envelope::Payload::OpenCameraResponse(response) = payload else {
            unreachable!()
        };
        RemoteCameraHandle::from_response(self.clone(), selector.id().clone(), response)
            .inspect_err(|error| self.fail(error.clone()))
    }

    pub async fn open_can(
        &self,
        selector: ResourceSelector,
        mode: LeaseMode,
        config: CanOpenConfig,
        filters: CanFilterSet,
    ) -> HalResult<RemoteCanHandle> {
        if selector.transport() != seeed_hal_core::TransportKind::Can {
            return Err(client_error(
                "runtime.argument.invalid",
                ErrorCategory::InvalidArgument,
                "can.open",
                false,
                "CAN resource selector transport must be Can",
            )
            .with_resource_id(selector.id().clone()));
        }
        self.require_can_capability("can.open", Some(selector.id()), |limits| {
            let mode_supported = match &config {
                CanOpenConfig::Attach(expectation) => match expectation.mode() {
                    Some(CanMode::Classic) => limits.can_classic,
                    Some(CanMode::Fd) => limits.can_fd,
                    None => limits.can_classic || limits.can_fd,
                },
                CanOpenConfig::Configure(config) => {
                    limits.can_configure
                        && match config.mode() {
                            CanMode::Classic => limits.can_classic,
                            CanMode::Fd => limits.can_fd,
                        }
                }
            };
            let filters_supported = !filters
                .as_slice()
                .iter()
                .any(|filter| filter.classes().error())
                || limits.can_error_frames;
            mode_supported && filters_supported
        })?;
        let resources = self.enumerate_can().await?;
        let descriptor = select_can_descriptor(&resources, &selector)?.clone();
        validate_can_open_capabilities(&descriptor, &config, &filters)?;

        let request = v1::OpenCanRequest {
            selector: Some((&selector).try_into()?),
            mode: lease_mode_to_proto(mode) as i32,
            config: Some((&config).into()),
            filters: Some((&filters).into()),
        };
        self.ensure_payload_fits(
            &envelope::Payload::OpenCanRequest(request.clone()),
            "can.open",
            Some(selector.id()),
        )?;
        let payload = self
            .request(
                envelope::Payload::OpenCanRequest(request),
                ExpectedResponse::OpenCan {
                    mode,
                    resource_id: selector.id().clone(),
                },
            )
            .await
            .map_err(|error| attach_resource(error, selector.id()))?;
        let envelope::Payload::OpenCanResponse(response) = payload else {
            unreachable!()
        };
        let result = RemoteCanHandle::from_response(
            self.clone(),
            selector.id().clone(),
            mode,
            &config,
            &descriptor,
            response,
        );
        if let Err(error) = &result {
            self.fail(error.clone());
        }
        result
    }

    pub fn subscribe(&self) -> EventSubscription {
        EventSubscription {
            receiver: self.inner.shared.events.subscribe(),
            shutdown: self.inner.shared.shutdown.subscribe(),
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

    pub(crate) fn ensure_can_payload_fits(
        &self,
        payload: &envelope::Payload,
        operation: &'static str,
        resource_id: &seeed_hal_core::ResourceId,
    ) -> HalResult<()> {
        self.ensure_payload_fits(payload, operation, Some(resource_id))
    }

    pub(crate) fn ensure_payload_for_resource(
        &self,
        payload: &envelope::Payload,
        operation: &'static str,
        resource_id: &ResourceId,
    ) -> HalResult<()> {
        self.ensure_payload_fits(payload, operation, Some(resource_id))
    }

    pub(crate) fn require_usb_transfer(
        &self,
        operation: &'static str,
        resource_id: &ResourceId,
        transfer: &UsbTransfer,
    ) -> HalResult<()> {
        self.require_minor_two(operation, Some(resource_id), |limits| match transfer {
            UsbTransfer::ControlOut { .. } | UsbTransfer::ControlIn { .. } => limits.usb_control,
            UsbTransfer::BulkOut { .. } | UsbTransfer::BulkIn { .. } => limits.usb_bulk,
            UsbTransfer::InterruptOut { .. } | UsbTransfer::InterruptIn { .. } => {
                limits.usb_interrupt
            }
        })
    }

    pub(crate) fn require_gpio_edges(
        &self,
        operation: &'static str,
        resource_id: &ResourceId,
    ) -> HalResult<()> {
        self.require_minor_two(operation, Some(resource_id), |limits| limits.gpio_edges)
    }

    pub(crate) fn require_camera_capture(
        &self,
        operation: &'static str,
        resource_id: &ResourceId,
    ) -> HalResult<()> {
        self.require_camera_capability(operation, Some(resource_id), |limits| limits.camera_capture)
    }

    pub(crate) fn require_camera_frames_shm(
        &self,
        operation: &'static str,
        resource_id: &ResourceId,
    ) -> HalResult<()> {
        self.require_camera_capability(operation, Some(resource_id), |limits| {
            limits.camera_frames_shm
        })
    }

    pub(crate) fn require_camera_controls(
        &self,
        operation: &'static str,
        resource_id: &ResourceId,
    ) -> HalResult<()> {
        self.require_camera_capability(operation, Some(resource_id), |limits| {
            limits.camera_controls
        })
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

    fn require_can_capability(
        &self,
        operation: &'static str,
        resource_id: Option<&seeed_hal_core::ResourceId>,
        supported: impl FnOnce(Limits) -> bool,
    ) -> HalResult<()> {
        let limits = *self
            .inner
            .shared
            .limits
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        if limits.protocol_minor >= 1 && supported(limits) {
            return Ok(());
        }
        let error = client_error(
            "runtime.protocol.capability_unsupported",
            ErrorCategory::Conflict,
            operation,
            false,
            "the negotiated broker protocol does not support this CAN operation",
        );
        Err(resource_id.map_or(error.clone(), |id| error.with_resource_id(id.clone())))
    }

    fn require_minor_two(
        &self,
        operation: &'static str,
        resource_id: Option<&ResourceId>,
        supported: impl FnOnce(Limits) -> bool,
    ) -> HalResult<()> {
        let limits = *self
            .inner
            .shared
            .limits
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        if limits.protocol_minor >= 2 && supported(limits) {
            return Ok(());
        }
        let error = client_error(
            "runtime.protocol.capability_unsupported",
            ErrorCategory::Conflict,
            operation,
            false,
            "the negotiated broker protocol does not support this USB/GPIO operation",
        );
        Err(resource_id.map_or(error.clone(), |id| error.with_resource_id(id.clone())))
    }

    fn require_camera_capability(
        &self,
        operation: &'static str,
        resource_id: Option<&ResourceId>,
        supported: impl FnOnce(Limits) -> bool,
    ) -> HalResult<()> {
        let limits = *self
            .inner
            .shared
            .limits
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        if limits.protocol_minor >= 3 && supported(limits) {
            return Ok(());
        }
        let error = client_error(
            "runtime.protocol.capability_unsupported",
            ErrorCategory::Conflict,
            operation,
            false,
            "the negotiated broker protocol does not support this Camera operation",
        );
        Err(resource_id.map_or(error.clone(), |id| error.with_resource_id(id.clone())))
    }

    fn ensure_payload_fits(
        &self,
        payload: &envelope::Payload,
        operation: &'static str,
        resource_id: Option<&seeed_hal_core::ResourceId>,
    ) -> HalResult<()> {
        let frame_limit = self.limits().0;
        let envelope = v1::Envelope {
            request_id: u64::MAX,
            payload: Some(payload.clone()),
        };
        if envelope.encoded_len() <= frame_limit && envelope.encoded_len() <= MAX_FRAME_BYTES {
            return Ok(());
        }
        let error = client_error(
            "runtime.protocol.frame_too_large",
            ErrorCategory::InvalidArgument,
            operation,
            false,
            "CAN request envelope exceeds the negotiated frame maximum",
        );
        Err(resource_id.map_or(error.clone(), |id| error.with_resource_id(id.clone())))
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
                let error = client_error(
                    "runtime.queue.cancelled_full",
                    ErrorCategory::Unavailable,
                    "runtime.client.cancel",
                    false,
                    "cancelled request tracking is full",
                );
                let replies =
                    begin_termination(&mut state, error.clone(), Some((self.request_id, pending)));
                drop(state);
                if let Some(replies) = replies {
                    finish_termination(&self.shared, error, replies);
                }
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
            protocol_minor_minimum: PROTOCOL_MINOR_MINIMUM,
            protocol_minor_maximum: PROTOCOL_MINOR_MAXIMUM,
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
                protocol_minor: response.protocol_minor,
                frame: response.max_frame_bytes as usize,
                read: response.max_read_bytes as usize,
                write: response.max_write_bytes as usize,
                can_classic: response
                    .capabilities
                    .iter()
                    .any(|value| value == CAN_CLASSIC_CAPABILITY),
                can_fd: response
                    .capabilities
                    .iter()
                    .any(|value| value == CAN_FD_CAPABILITY),
                can_configure: response
                    .capabilities
                    .iter()
                    .any(|value| value == CAN_CONFIGURE_CAPABILITY),
                can_error_frames: response
                    .capabilities
                    .iter()
                    .any(|value| value == CAN_ERROR_FRAMES_CAPABILITY),
                can_rx_timestamp: response
                    .capabilities
                    .iter()
                    .any(|value| value == CAN_RX_TIMESTAMP_CAPABILITY),
                usb_control: response
                    .capabilities
                    .iter()
                    .any(|value| value == USB_CONTROL_CAPABILITY),
                usb_bulk: response
                    .capabilities
                    .iter()
                    .any(|value| value == USB_BULK_CAPABILITY),
                usb_interrupt: response
                    .capabilities
                    .iter()
                    .any(|value| value == USB_INTERRUPT_CAPABILITY),
                gpio_lines: response
                    .capabilities
                    .iter()
                    .any(|value| value == GPIO_LINES_CAPABILITY),
                gpio_edges: response
                    .capabilities
                    .iter()
                    .any(|value| value == GPIO_EDGES_CAPABILITY),
                camera_capture: response
                    .capabilities
                    .iter()
                    .any(|value| value == CAMERA_CAPTURE_CAPABILITY),
                camera_frames_shm: response
                    .capabilities
                    .iter()
                    .any(|value| value == CAMERA_FRAMES_SHM_CAPABILITY),
                camera_controls: response
                    .capabilities
                    .iter()
                    .any(|value| value == CAMERA_CONTROLS_CAPABILITY),
            })
        }
        Some(envelope::Payload::Error(error)) => Err(error_from_proto(error)?),
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
        if connection_is_terminal(&shared) {
            return;
        }
        let frame = tokio::select! {
            biased;
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
        #[cfg(test)]
        if let Some(gate) = shared
            .inbound_test_hooks
            .as_ref()
            .and_then(|hooks| hooks.after_frame.as_ref())
        {
            gate.pause().await;
        }
        if connection_is_terminal(&shared) {
            return;
        }
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
        #[cfg(test)]
        if let Some(gate) = shared
            .inbound_test_hooks
            .as_ref()
            .and_then(|hooks| hooks.after_preflight.as_ref())
        {
            gate.pause().await;
        }
        let decode = {
            let state = shared.requests.lock().unwrap_or_else(|p| p.into_inner());
            if state.terminal.is_some() {
                return;
            }
            #[cfg(test)]
            if let Some(hooks) = &shared.inbound_test_hooks {
                hooks
                    .decode_calls
                    .fetch_add(1, std::sync::atomic::Ordering::AcqRel);
            }
            v1::Envelope::decode(frame)
        };
        let envelope = match decode {
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
                Some(envelope::Payload::Error(error)) => match error_from_proto(error) {
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

        let correlated = {
            let mut state = shared.requests.lock().unwrap_or_else(|p| p.into_inner());
            if state.terminal.is_some() {
                return;
            }
            if let Some(pending) = state.pending.remove(&envelope.request_id) {
                remember_completed(&mut state, envelope.request_id, shared.tombstone_capacity);
                CorrelatedResponse::Pending(pending)
            } else if let Some(expected) = state.cancelled.remove(&envelope.request_id) {
                remember_completed(&mut state, envelope.request_id, shared.tombstone_capacity);
                CorrelatedResponse::Cancelled(expected)
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
        let pending = match correlated {
            CorrelatedResponse::Pending(pending) => pending,
            CorrelatedResponse::Cancelled(expected) => {
                let validation = match envelope.payload {
                    Some(envelope::Payload::Error(error)) => error_from_proto(error)
                        .map(|_| ())
                        .map_err(|error| attach_expected(error, &expected)),
                    Some(payload) => validate_response(expected.clone(), &payload),
                    None => Err(expected
                        .resource_id()
                        .map_or_else(unexpected_response, |resource_id| {
                            unexpected_response().with_resource_id(resource_id.clone())
                        })),
                };
                if let Err(error) = validation {
                    terminate(&shared, error);
                    return;
                }
                continue;
            }
        };
        let result = match envelope.payload {
            Some(envelope::Payload::Error(error)) => error_from_proto(error)
                .map(|error| {
                    pending
                        .expected
                        .resource_id()
                        .map_or(error.clone(), |resource_id| {
                            attach_resource(error, resource_id)
                        })
                })
                .and_then(Err),
            Some(payload) => {
                validate_response(pending.expected.clone(), &payload).map(|()| payload)
            }
            None => Err(unexpected_response()),
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

fn connection_is_terminal(shared: &Shared) -> bool {
    shared
        .requests
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .terminal
        .is_some()
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
    let expected = {
        let state = shared
            .requests
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        state
            .pending
            .get(&request_id)
            .map(|pending| pending.expected.clone())
            .or_else(|| state.cancelled.get(&request_id).cloned())
    };
    let requested_read = expected.as_ref().and_then(|expected| match expected {
        ExpectedResponse::SerialRead { max_bytes } => Some(*max_bytes),
        _ => None,
    });
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
    })?;

    let requested_can = expected.as_ref().and_then(|expected| match expected {
        ExpectedResponse::CanReceive {
            max_frames,
            max_read_bytes,
            profile,
        } => Some((*max_frames, *max_read_bytes, profile)),
        _ => None,
    });
    visit_fields(frame, |field, wire| {
        if let (57, WireValue::Bytes(receive_response)) = (field, wire) {
            let mut frame_count = 0_usize;
            let mut payload_bytes = 0_usize;
            let mut has_timestamp = false;
            visit_fields(receive_response, |field, wire| {
                if let (1, WireValue::Bytes(received)) = (field, wire) {
                    frame_count = frame_count.checked_add(1).ok_or_else(|| {
                        invalid_wire("CAN receive response frame count overflows usize")
                    })?;
                    visit_fields(received, |field, wire| {
                        match (field, wire) {
                            (1, WireValue::Bytes(can_frame)) => {
                                visit_fields(can_frame, |field, wire| {
                                    if let (3, WireValue::Bytes(data)) = (field, wire) {
                                        payload_bytes = payload_bytes
                                            .checked_add(data.len())
                                            .ok_or_else(|| {
                                                invalid_wire(
                                                    "CAN receive response payload overflows usize",
                                                )
                                            })?;
                                    }
                                    Ok(())
                                })?;
                            }
                            (2, WireValue::Bytes(_)) => has_timestamp = true,
                            _ => {}
                        }
                        Ok(())
                    })?;
                }
                Ok(())
            })?;
            if let Some((max_frames, max_read_bytes, profile)) = requested_can {
                if frame_count > max_frames || payload_bytes > max_read_bytes {
                    return Err(attach_profile(
                        invalid_wire(
                            "CAN receive response exceeds the requested frame or byte bound",
                        ),
                        profile,
                    ));
                }
                if has_timestamp && !profile.timestamps {
                    return Err(attach_profile(
                        invalid_wire("CAN receive response contains an unadvertised timestamp"),
                        profile,
                    ));
                }
            }
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

fn validate_response(expected: ExpectedResponse, payload: &envelope::Payload) -> HalResult<()> {
    let resource_id = expected.resource_id().cloned();
    match (expected, payload) {
        (ExpectedResponse::EnumerateSerial, envelope::Payload::EnumerateSerialResponse(_))
        | (ExpectedResponse::OpenSerial, envelope::Payload::OpenSerialResponse(_))
        | (ExpectedResponse::SerialRead { .. }, envelope::Payload::SerialReadResponse(_))
        | (ExpectedResponse::SerialWrite, envelope::Payload::SerialWriteResponse(_))
        | (ExpectedResponse::SerialFlush, envelope::Payload::SerialFlushResponse(_))
        | (
            ExpectedResponse::SetControlLines,
            envelope::Payload::SetSerialControlLinesResponse(_),
        )
        | (ExpectedResponse::CloseSession, envelope::Payload::CloseSessionResponse(_)) => Ok(()),
        (ExpectedResponse::CloseCan { .. }, envelope::Payload::CloseSessionResponse(_))
        | (
            ExpectedResponse::ReplaceCanFilters { .. },
            envelope::Payload::ReplaceCanFiltersResponse(_),
        ) => Ok(()),
        (ExpectedResponse::EnumerateCan, envelope::Payload::EnumerateCanResponse(response)) => {
            enumerate_can_response_from_proto(response.clone()).map(|_| ())
        }
        (
            ExpectedResponse::OpenCan { mode, resource_id },
            envelope::Payload::OpenCanResponse(response),
        ) => open_can_response_from_proto(response.clone(), mode)
            .map(|_| ())
            .map_err(|error| attach_resource(error, &resource_id)),
        (
            ExpectedResponse::CanSend {
                input_count,
                profile,
            },
            envelope::Payload::CanSendResponse(response),
        ) => can_send_response_from_proto(response.clone(), input_count)
            .map(|_| ())
            .map_err(|error| attach_profile(error, &profile)),
        (
            ExpectedResponse::CanReceive {
                max_frames,
                max_read_bytes,
                profile,
            },
            envelope::Payload::CanReceiveResponse(response),
        ) => {
            let frames = can_receive_response_from_proto(response.clone(), max_frames)
                .map_err(|error| attach_profile(error, &profile))?;
            let payload_bytes = frames.iter().try_fold(0_usize, |total, received| {
                total
                    .checked_add(received.frame().data().len())
                    .ok_or_else(|| {
                        invalid_profile_message(
                            &profile,
                            "CAN receive response payload byte count overflows usize",
                        )
                    })
            })?;
            if payload_bytes > max_read_bytes {
                return Err(invalid_profile_message(
                    &profile,
                    "CAN receive response exceeds the negotiated read maximum",
                ));
            }
            validate_received_profile(&frames, &profile)
        }
        (
            ExpectedResponse::CanBusStatus { profile },
            envelope::Payload::GetCanBusStatusResponse(response),
        ) => get_can_bus_status_response_from_proto(*response)
            .map(|_| ())
            .map_err(|error| attach_profile(error, &profile)),
        (ExpectedResponse::EnumerateUsb, envelope::Payload::EnumerateUsbResponse(response)) => {
            response.resources.iter().try_for_each(|resource| {
                let descriptor: HalResult<ResourceDescriptor> = resource.clone().try_into();
                descriptor.and_then(|descriptor| {
                    (descriptor.transport() == seeed_hal_core::TransportKind::Usb)
                        .then_some(())
                        .ok_or_else(|| {
                            seeed_hal_protocol::invalid_message(
                                "USB enumeration returned a non-USB descriptor",
                            )
                        })
                })
            })
        }
        (
            ExpectedResponse::OpenUsb { resource_id },
            envelope::Payload::OpenUsbResponse(response),
        ) => open_usb_response_from_proto(response.clone())
            .map(|_| ())
            .map_err(|error| attach_resource(error, &resource_id)),
        (
            ExpectedResponse::UsbTransfer {
                max_read_bytes,
                resource_id,
            },
            envelope::Payload::UsbTransferResponse(response),
        ) => {
            let data = usb_transfer_response_from_proto(response.clone())
                .map_err(|error| attach_resource(error, &resource_id))?;
            (data.len() <= max_read_bytes).then_some(()).ok_or_else(|| {
                attach_resource(
                    seeed_hal_protocol::invalid_message(
                        "USB response exceeds negotiated read maximum",
                    ),
                    &resource_id,
                )
            })
        }
        (ExpectedResponse::CloseUsb { .. }, envelope::Payload::CloseUsbResponse(_)) => Ok(()),
        (ExpectedResponse::EnumerateGpio, envelope::Payload::EnumerateGpioResponse(response)) => {
            response.resources.iter().try_for_each(|resource| {
                let descriptor: HalResult<ResourceDescriptor> = resource.clone().try_into();
                descriptor.and_then(|descriptor| {
                    (descriptor.transport() == seeed_hal_core::TransportKind::Gpio)
                        .then_some(())
                        .ok_or_else(|| {
                            seeed_hal_protocol::invalid_message(
                                "GPIO enumeration returned a non-GPIO descriptor",
                            )
                        })
                })
            })
        }
        (
            ExpectedResponse::OpenGpio {
                line_count,
                resource_id,
            },
            envelope::Payload::OpenGpioResponse(response),
        ) => {
            (line_count > 0).then_some(()).ok_or_else(|| {
                attach_resource(
                    seeed_hal_protocol::invalid_message("GPIO open line count is invalid"),
                    &resource_id,
                )
            })?;
            open_gpio_response_from_proto(response.clone())
                .map(|_| ())
                .map_err(|error| attach_resource(error, &resource_id))
        }
        (
            ExpectedResponse::GpioRead {
                line_count,
                resource_id,
            },
            envelope::Payload::GpioReadResponse(response),
        ) => {
            let values = gpio_read_response_from_proto(response.clone())
                .map_err(|error| attach_resource(error, &resource_id))?;
            (values.len() == line_count).then_some(()).ok_or_else(|| {
                attach_resource(
                    seeed_hal_protocol::invalid_message(
                        "GPIO read response length does not match opened lines",
                    ),
                    &resource_id,
                )
            })
        }
        (ExpectedResponse::GpioWrite { .. }, envelope::Payload::GpioWriteResponse(_)) => Ok(()),
        (
            ExpectedResponse::GpioNextEdge { resource_id },
            envelope::Payload::GpioNextEdgeResponse(response),
        ) => gpio_next_edge_response_from_proto(*response)
            .map(|_| ())
            .map_err(|error| attach_resource(error, &resource_id)),
        (ExpectedResponse::CloseGpio { .. }, envelope::Payload::CloseGpioResponse(_)) => Ok(()),
        (
            ExpectedResponse::EnumerateCamera,
            envelope::Payload::EnumerateCameraResponse(response),
        ) => response.resources.iter().try_for_each(|resource| {
            let descriptor: HalResult<ResourceDescriptor> = resource.clone().try_into();
            descriptor.and_then(|descriptor| {
                (descriptor.transport() == seeed_hal_core::TransportKind::Camera)
                    .then_some(())
                    .ok_or_else(|| {
                        seeed_hal_protocol::invalid_message(
                            "Camera enumeration returned a non-Camera descriptor",
                        )
                    })
            })
        }),
        (
            ExpectedResponse::OpenCamera { resource_id },
            envelope::Payload::OpenCameraResponse(response),
        ) => seeed_hal_protocol::camera_open_response_from_proto(response.clone())
            .map(|_| ())
            .map_err(|error| attach_resource(error, &resource_id)),
        (ExpectedResponse::CaptureCamera { .. }, envelope::Payload::CaptureCameraResponse(_))
        | (
            ExpectedResponse::CameraMappingDescriptor { .. },
            envelope::Payload::CameraMappingDescriptorResponse(_),
        )
        | (
            ExpectedResponse::CameraNextFrameLease { .. },
            envelope::Payload::CameraNextFrameLeaseResponse(_),
        )
        | (
            ExpectedResponse::CameraDroppedCount { .. },
            envelope::Payload::CameraDroppedCountResponse(_),
        )
        | (
            ExpectedResponse::CameraGetControl { .. },
            envelope::Payload::CameraGetControlResponse(_),
        )
        | (
            ExpectedResponse::CameraSetControl { .. },
            envelope::Payload::CameraSetControlResponse(_),
        )
        | (ExpectedResponse::CameraSetAuto { .. }, envelope::Payload::CameraSetAutoResponse(_))
        | (ExpectedResponse::CloseCamera { .. }, envelope::Payload::CloseCameraResponse(_)) => {
            Ok(())
        }
        _ => Err(resource_id.map_or_else(unexpected_response, |resource_id| {
            unexpected_response().with_resource_id(resource_id)
        })),
    }
}

fn validate_received_profile(
    frames: &[seeed_hal_can::ReceivedCanFrame],
    profile: &CanSessionProfile,
) -> HalResult<()> {
    for received in frames {
        let frame_allowed = match received.frame() {
            seeed_hal_can::CanFrame::ClassicData { .. }
            | seeed_hal_can::CanFrame::ClassicRemote { .. } => profile.classic_frames,
            seeed_hal_can::CanFrame::FdData { .. } => {
                profile.fd_frames && profile.mode == CanMode::Fd
            }
            seeed_hal_can::CanFrame::Error { .. } => profile.error_frames,
        };
        if !frame_allowed {
            return Err(invalid_profile_message(
                profile,
                "CAN receive response contains a frame outside the active session profile",
            ));
        }
        if received.timestamp().is_some() && !profile.timestamps {
            return Err(invalid_profile_message(
                profile,
                "CAN receive response contains an unadvertised timestamp",
            ));
        }
    }
    Ok(())
}

fn attach_profile(error: HalError, profile: &CanSessionProfile) -> HalError {
    attach_resource(error, &profile.resource_id)
}

fn attach_expected(error: HalError, expected: &ExpectedResponse) -> HalError {
    expected.resource_id().map_or(error.clone(), |resource_id| {
        attach_resource(error, resource_id)
    })
}

fn invalid_profile_message(profile: &CanSessionProfile, message: &'static str) -> HalError {
    attach_profile(
        seeed_hal_protocol::invalid_message(format!(
            "{message} for correlated session {}",
            profile.session_id.as_str()
        )),
        profile,
    )
}

fn unexpected_response() -> HalError {
    client_error(
        "runtime.protocol.unexpected_response",
        ErrorCategory::InvalidArgument,
        "runtime.protocol.read",
        false,
        "response payload does not match its request",
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
        begin_termination(&mut state, error.clone(), None)
    };
    let Some(replies) = replies else { return };
    finish_termination(shared, error, replies);
}

fn begin_termination(
    state: &mut RequestState,
    error: HalError,
    extra: Option<(u64, PendingRequest)>,
) -> Option<Vec<oneshot::Sender<HalResult<envelope::Payload>>>> {
    if state.terminal.is_some() {
        return None;
    }
    state.terminal = Some(error);
    let pending = state.pending.drain().collect::<Vec<_>>();
    let mut replies = Vec::with_capacity(pending.len() + usize::from(extra.is_some()));
    for (request_id, pending) in pending.into_iter().chain(extra) {
        state.cancelled.insert(request_id, pending.expected);
        replies.push(pending.reply);
    }
    Some(replies)
}

fn finish_termination(
    shared: &Shared,
    error: HalError,
    replies: Vec<oneshot::Sender<HalResult<envelope::Payload>>>,
) {
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
    let broker_range = handshake_response_minor_range(response).map_err(|_| {
        client_error(
            "runtime.protocol.invalid_handshake",
            ErrorCategory::Conflict,
            "runtime.protocol.handshake",
            false,
            "broker returned an invalid supported protocol minor range",
        )
    })?;
    if response.protocol_major != PROTOCOL_MAJOR
        || !(PROTOCOL_MINOR_MINIMUM..=PROTOCOL_MINOR_MAXIMUM).contains(&response.protocol_minor)
        || !(broker_range.0..=broker_range.1).contains(&response.protocol_minor)
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

fn lease_mode_to_proto(mode: LeaseMode) -> v1::LeaseMode {
    match mode {
        LeaseMode::Observe => v1::LeaseMode::Observe,
        LeaseMode::Control => v1::LeaseMode::Control,
        LeaseMode::Maintenance => v1::LeaseMode::Maintenance,
    }
}

fn attach_resource(error: HalError, resource_id: &seeed_hal_core::ResourceId) -> HalError {
    if error.resource_id().is_some() {
        error
    } else {
        error.with_resource_id(resource_id.clone())
    }
}

fn select_can_descriptor<'a>(
    resources: &'a [ResourceDescriptor],
    selector: &ResourceSelector,
) -> HalResult<&'a ResourceDescriptor> {
    let mut matches = resources.iter().filter(|resource| {
        resource.id() == selector.id()
            && resource.transport() == seeed_hal_core::TransportKind::Can
            && resource
                .minimum_identity_quality()
                .satisfies(selector.minimum_identity_quality())
    });
    let Some(selected) = matches.next() else {
        return Err(client_error(
            "runtime.resource.not_found",
            ErrorCategory::NotFound,
            "can.open",
            false,
            "CAN resource selector did not match an enumerated descriptor",
        )
        .with_resource_id(selector.id().clone()));
    };
    if matches.next().is_some() {
        return Err(client_error(
            "runtime.resource.ambiguous",
            ErrorCategory::Conflict,
            "can.open",
            false,
            "CAN resource selector matched more than one enumerated descriptor",
        )
        .with_resource_id(selector.id().clone()));
    }
    Ok(selected)
}

fn validate_can_open_capabilities(
    descriptor: &ResourceDescriptor,
    config: &CanOpenConfig,
    filters: &CanFilterSet,
) -> HalResult<()> {
    let capabilities = descriptor.capabilities();
    let mode_supported = match config {
        CanOpenConfig::Attach(expectation) => match expectation.mode() {
            Some(CanMode::Classic) => capabilities.contains(&can_classic_capability()),
            Some(CanMode::Fd) => capabilities.contains(&can_fd_capability()),
            None => {
                capabilities.contains(&can_classic_capability())
                    || capabilities.contains(&can_fd_capability())
            }
        },
        CanOpenConfig::Configure(config) => {
            capabilities.contains(&can_configure_capability())
                && match config.mode() {
                    CanMode::Classic => capabilities.contains(&can_classic_capability()),
                    CanMode::Fd => capabilities.contains(&can_fd_capability()),
                }
        }
    };
    let filters_supported = !filters
        .as_slice()
        .iter()
        .any(|filter| filter.classes().error())
        || capabilities.contains(&can_error_frames_capability());
    if mode_supported && filters_supported {
        return Ok(());
    }
    Err(client_error(
        "runtime.protocol.capability_unsupported",
        ErrorCategory::Conflict,
        "can.open",
        false,
        "the selected CAN resource does not support the requested configuration or filters",
    )
    .with_resource_id(descriptor.id().clone()))
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
    use std::pin::Pin;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::task::{Context, Poll};

    use bytes::{Bytes, BytesMut};
    use futures_util::task::AtomicWaker;
    use futures_util::{SinkExt, StreamExt};
    use prost::Message;
    use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
    use tokio::sync::{broadcast, mpsc, watch};
    use zeroize::Zeroize;

    use super::{
        CancellationGuard, ConnectionOptions, ExpectedResponse, HalClient, InboundTestGate,
        InboundTestHooks, Limits, PendingRequest, RequestState, SecretToken, Shared, WireValue,
        begin_termination, client_error, frame_codec, reader_task, visit_fields,
    };
    use seeed_hal_core::ErrorCategory;
    use seeed_hal_protocol::PROTOCOL_MINOR_MAXIMUM;
    use seeed_hal_protocol::v1::{self, envelope};

    #[derive(Clone, Default)]
    struct WriterTestGate {
        state: Arc<WriterTestGateState>,
    }

    #[derive(Default)]
    struct WriterTestGateState {
        armed: AtomicBool,
        blocked: AtomicBool,
        released: AtomicBool,
        blocked_notify: tokio::sync::Notify,
        write_waker: AtomicWaker,
    }

    struct WriterGatedIo<T> {
        inner: T,
        gate: WriterTestGate,
    }

    impl WriterTestGate {
        fn wrap<T>(inner: T) -> (Self, WriterGatedIo<T>) {
            let gate = Self::default();
            (gate.clone(), WriterGatedIo { inner, gate })
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

        fn release(&self) {
            self.state.released.store(true, Ordering::Release);
            self.state.write_waker.wake();
        }
    }

    impl<T> AsyncRead for WriterGatedIo<T>
    where
        T: AsyncRead + Unpin,
    {
        fn poll_read(
            mut self: Pin<&mut Self>,
            cx: &mut Context<'_>,
            buffer: &mut ReadBuf<'_>,
        ) -> Poll<std::io::Result<()>> {
            Pin::new(&mut self.inner).poll_read(cx, buffer)
        }
    }

    impl<T> AsyncWrite for WriterGatedIo<T>
    where
        T: AsyncWrite + Unpin,
    {
        fn poll_write(
            mut self: Pin<&mut Self>,
            cx: &mut Context<'_>,
            buffer: &[u8],
        ) -> Poll<std::io::Result<usize>> {
            if self.gate.state.armed.load(Ordering::Acquire)
                && !self.gate.state.released.load(Ordering::Acquire)
            {
                self.gate.state.write_waker.register(cx.waker());
                self.gate.state.blocked.store(true, Ordering::Release);
                self.gate.state.blocked_notify.notify_waiters();
                if !self.gate.state.released.load(Ordering::Acquire) {
                    return Poll::Pending;
                }
            }
            Pin::new(&mut self.inner).poll_write(cx, buffer)
        }

        fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
            Pin::new(&mut self.inner).poll_flush(cx)
        }

        fn poll_shutdown(
            mut self: Pin<&mut Self>,
            cx: &mut Context<'_>,
        ) -> Poll<std::io::Result<()>> {
            Pin::new(&mut self.inner).poll_shutdown(cx)
        }
    }

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
    fn terminal_transition_retains_all_expected_metadata_and_drains_replies_once() {
        let (first_reply, mut first_rx) = tokio::sync::oneshot::channel();
        let (overflow_reply, mut overflow_rx) = tokio::sync::oneshot::channel();
        let mut state = RequestState {
            next_request_id: 5,
            pending: HashMap::from([(
                4,
                PendingRequest {
                    expected: ExpectedResponse::SerialRead { max_bytes: 8 },
                    reply: first_reply,
                },
            )]),
            cancelled: HashMap::from([(2, ExpectedResponse::EnumerateSerial)]),
            completed: HashSet::new(),
            completed_order: VecDeque::new(),
            terminal: None,
        };
        let error = client_error(
            "runtime.queue.cancelled_full",
            ErrorCategory::Unavailable,
            "runtime.client.cancel",
            false,
            "cancelled request tracking is full",
        );

        let replies = begin_termination(
            &mut state,
            error.clone(),
            Some((
                3,
                PendingRequest {
                    expected: ExpectedResponse::SerialRead { max_bytes: 7 },
                    reply: overflow_reply,
                },
            )),
        )
        .unwrap();

        assert_eq!(state.terminal.as_ref().unwrap().name(), error.name());
        assert!(matches!(
            state.cancelled.get(&3),
            Some(ExpectedResponse::SerialRead { max_bytes: 7 })
        ));
        assert!(matches!(
            state.cancelled.get(&4),
            Some(ExpectedResponse::SerialRead { max_bytes: 8 })
        ));
        assert!(state.pending.is_empty());
        assert_eq!(replies.len(), 2);
        for reply in replies {
            reply.send(Err(error.clone())).unwrap();
        }
        assert_eq!(
            first_rx.try_recv().unwrap().unwrap_err().name(),
            error.name()
        );
        assert_eq!(
            overflow_rx.try_recv().unwrap().unwrap_err().name(),
            error.name()
        );
        assert!(begin_termination(&mut state, error, None).is_none());
    }

    #[tokio::test]
    async fn writer_queue_overflow_uses_a_positively_blocked_writer() {
        let (client_io, server_io) = tokio::io::duplex(64 * 1024);
        let (writer_gate, client_io) = WriterTestGate::wrap(client_io);
        let server = tokio::spawn(async move {
            let mut wire = tokio_util::codec::Framed::new(
                server_io,
                frame_codec(seeed_hal_protocol::MAX_FRAME_BYTES),
            );
            let handshake_frame = wire.next().await.unwrap().unwrap();
            let handshake = v1::Envelope::decode(handshake_frame).unwrap();
            let request = match handshake.payload.unwrap() {
                envelope::Payload::HandshakeRequest(request) => request,
                _ => panic!("expected handshake request"),
            };
            wire.send(Bytes::from(
                v1::Envelope {
                    request_id: handshake.request_id,
                    payload: Some(envelope::Payload::HandshakeResponse(
                        v1::HandshakeResponse {
                            protocol_major: 1,
                            protocol_minor: 0,
                            capabilities: vec!["serial.bytes/v1".to_owned()],
                            max_frame_bytes: request.max_frame_bytes,
                            max_read_bytes: request.max_read_bytes,
                            max_write_bytes: request.max_write_bytes,
                            protocol_minor_minimum: 0,
                            protocol_minor_maximum: 0,
                        },
                    )),
                }
                .encode_to_vec(),
            ))
            .await
            .unwrap();
            while let Some(frame) = wire.next().await {
                let request = v1::Envelope::decode(frame.unwrap()).unwrap();
                if wire
                    .send(Bytes::from(
                        v1::Envelope {
                            request_id: request.request_id,
                            payload: Some(envelope::Payload::EnumerateSerialResponse(
                                v1::EnumerateSerialResponse {
                                    resources: Vec::new(),
                                },
                            )),
                        }
                        .encode_to_vec(),
                    ))
                    .await
                    .is_err()
                {
                    break;
                }
            }
        });
        let client = HalClient::from_io(
            client_io,
            ConnectionOptions::new("unused", [0x5a; 32]).with_queue_capacities(8, 1, 1),
        )
        .await
        .unwrap();
        writer_gate.arm();

        let first_client = client.clone();
        let first = tokio::spawn(async move { first_client.enumerate_serial().await });
        tokio::time::timeout(
            std::time::Duration::from_secs(1),
            writer_gate.wait_until_blocked(),
        )
        .await
        .expect("writer must positively report its blocked state");

        let second_client = client.clone();
        let second = tokio::spawn(async move { second_client.enumerate_serial().await });
        tokio::time::timeout(std::time::Duration::from_secs(1), async {
            loop {
                if client
                    .inner
                    .shared
                    .requests
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .pending
                    .len()
                    == 2
                {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("second request must positively occupy the bounded writer queue");

        let error = client.enumerate_serial().await.unwrap_err();
        assert_eq!(error.name().as_str(), "runtime.queue.full");
        assert_eq!(error.operation().as_str(), "runtime.protocol.write");

        first.abort();
        second.abort();
        writer_gate.release();
        client.close().await.unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(1), server)
            .await
            .expect("test server must stop after client closure")
            .unwrap();
    }

    #[tokio::test]
    async fn retryable_close_admission_failure_retains_handle_until_close_succeeds() {
        use seeed_hal_serial::SerialConfig;

        let (client_io, server_io) = tokio::io::duplex(64 * 1024);
        let (writer_gate, client_io) = WriterTestGate::wrap(client_io);
        let server = tokio::spawn(async move {
            let mut wire = tokio_util::codec::Framed::new(
                server_io,
                frame_codec(seeed_hal_protocol::MAX_FRAME_BYTES),
            );
            let handshake_frame = wire.next().await.unwrap().unwrap();
            let handshake = v1::Envelope::decode(handshake_frame).unwrap();
            let offered = match handshake.payload.unwrap() {
                envelope::Payload::HandshakeRequest(request) => request,
                _ => panic!("expected handshake request"),
            };
            wire.send(Bytes::from(
                v1::Envelope {
                    request_id: handshake.request_id,
                    payload: Some(envelope::Payload::HandshakeResponse(
                        v1::HandshakeResponse {
                            protocol_major: 1,
                            protocol_minor: 0,
                            capabilities: vec!["serial.bytes/v1".to_owned()],
                            max_frame_bytes: offered.max_frame_bytes,
                            max_read_bytes: offered.max_read_bytes,
                            max_write_bytes: offered.max_write_bytes,
                            protocol_minor_minimum: 0,
                            protocol_minor_maximum: 0,
                        },
                    )),
                }
                .encode_to_vec(),
            ))
            .await
            .unwrap();

            let mut open = false;
            let mut generation = 0_u64;
            while let Some(frame) = wire.next().await {
                let request = v1::Envelope::decode(frame.unwrap()).unwrap();
                let response = match request.payload.unwrap() {
                    envelope::Payload::EnumerateSerialRequest(_) => {
                        envelope::Payload::EnumerateSerialResponse(v1::EnumerateSerialResponse {
                            resources: vec![v1::ResourceDescriptor {
                                resource_id: "serial:fake:retry-close".to_owned(),
                                endpoint: "virtual://retry-close".to_owned(),
                                identity_quality: v1::IdentityQuality::Strong as i32,
                                transport: v1::TransportKind::Serial as i32,
                                properties: Default::default(),
                                capabilities: vec!["serial.bytes/v1".to_owned()],
                            }],
                        })
                    }
                    envelope::Payload::OpenSerialRequest(_) => {
                        assert!(
                            !open,
                            "resource cannot reopen until close reaches the broker"
                        );
                        open = true;
                        generation += 1;
                        envelope::Payload::OpenSerialResponse(v1::OpenSerialResponse {
                            session_id: format!("session-retry-close-{generation}"),
                            lease: Some(v1::LeaseToken {
                                lease_id: format!("lease-retry-close-{generation}"),
                                generation,
                                mode: v1::LeaseMode::Control as i32,
                            }),
                        })
                    }
                    envelope::Payload::CloseSessionRequest(_) => {
                        assert!(open, "only an open resource can be closed");
                        open = false;
                        envelope::Payload::CloseSessionResponse(v1::Empty {})
                    }
                    payload => panic!("unexpected request after successful close: {payload:?}"),
                };
                if wire
                    .send(Bytes::from(
                        v1::Envelope {
                            request_id: request.request_id,
                            payload: Some(response),
                        }
                        .encode_to_vec(),
                    ))
                    .await
                    .is_err()
                {
                    break;
                }
            }
            assert!(
                !open,
                "test must close the reopened resource before disconnect"
            );
        });
        let client = HalClient::from_io(
            client_io,
            ConnectionOptions::new("unused", [0x5a; 32]).with_queue_capacities(8, 1, 1),
        )
        .await
        .unwrap();
        let descriptor = client.enumerate_serial().await.unwrap().remove(0);
        let mut serial = client
            .open_serial(descriptor.selector(), SerialConfig::default())
            .await
            .unwrap();
        writer_gate.arm();

        let first_client = client.clone();
        let first = tokio::spawn(async move { first_client.enumerate_serial().await });
        tokio::time::timeout(
            std::time::Duration::from_secs(1),
            writer_gate.wait_until_blocked(),
        )
        .await
        .expect("writer must positively report its blocked state");

        let second_client = client.clone();
        let second = tokio::spawn(async move { second_client.enumerate_serial().await });
        tokio::time::timeout(std::time::Duration::from_secs(1), async {
            loop {
                if client
                    .inner
                    .shared
                    .requests
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .pending
                    .len()
                    == 2
                {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("second request must positively occupy the bounded writer queue");

        let error = serial.close().await.unwrap_err();
        assert_eq!(error.name().as_str(), "runtime.queue.full");
        assert_eq!(error.operation().as_str(), "runtime.protocol.write");
        assert_eq!(error.resource_id(), Some(descriptor.id()));

        writer_gate.release();
        first.await.unwrap().unwrap();
        second.await.unwrap().unwrap();
        serial.close().await.unwrap();

        let closed = serial.flush().await.unwrap_err();
        assert_eq!(closed.name().as_str(), "runtime.session.closed");
        assert_eq!(closed.resource_id(), Some(descriptor.id()));

        let mut reopened = client
            .open_serial(descriptor.selector(), SerialConfig::default())
            .await
            .unwrap();
        reopened.close().await.unwrap();
        client.close().await.unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(1), server)
            .await
            .expect("test server must stop after client closure")
            .unwrap();
    }

    #[tokio::test]
    async fn tombstone_overflow_stops_ready_frames_before_prost_decode_at_each_boundary() {
        for (iteration, boundary) in (0..32)
            .flat_map(|iteration| ["frame", "preflight"].map(move |boundary| (iteration, boundary)))
        {
            let gate = Arc::new(InboundTestGate::new());
            let hooks = Arc::new(InboundTestHooks {
                after_frame: (boundary == "frame").then(|| gate.clone()),
                after_preflight: (boundary == "preflight").then(|| gate.clone()),
                decode_calls: AtomicUsize::new(0),
            });
            let (overflow_reply, overflow_rx) = tokio::sync::oneshot::channel();
            let (read_reply, read_rx) = tokio::sync::oneshot::channel();
            let (writer, _) = mpsc::channel(1);
            let (events, _) = broadcast::channel(1);
            let (shutdown, shutdown_rx) = watch::channel(false);
            let shared = Arc::new(Shared {
                requests: std::sync::Mutex::new(RequestState {
                    next_request_id: 5,
                    pending: HashMap::from([
                        (
                            3,
                            PendingRequest {
                                expected: ExpectedResponse::EnumerateSerial,
                                reply: overflow_reply,
                            },
                        ),
                        (
                            4,
                            PendingRequest {
                                expected: ExpectedResponse::SerialRead { max_bytes: 8 },
                                reply: read_reply,
                            },
                        ),
                    ]),
                    cancelled: HashMap::from([(2, ExpectedResponse::EnumerateSerial)]),
                    completed: HashSet::new(),
                    completed_order: VecDeque::new(),
                    terminal: None,
                }),
                limits: std::sync::Mutex::new(Limits {
                    protocol_minor: PROTOCOL_MINOR_MAXIMUM,
                    frame: 512,
                    read: 16,
                    write: 16,
                    can_classic: true,
                    can_fd: true,
                    can_configure: true,
                    can_error_frames: true,
                    can_rx_timestamp: true,
                    usb_control: true,
                    usb_bulk: true,
                    usb_interrupt: true,
                    gpio_lines: true,
                    gpio_edges: true,
                    camera_capture: true,
                    camera_frames_shm: true,
                    camera_controls: true,
                }),
                pending_capacity: 2,
                tombstone_capacity: 1,
                writer,
                events,
                shutdown,
                inbound_test_hooks: Some(hooks.clone()),
            });
            let data_len = if boundary == "frame" { 12 } else { 8 };
            let frame = v1::Envelope {
                request_id: 4,
                payload: Some(envelope::Payload::SerialReadResponse(
                    v1::SerialReadResponse {
                        data: vec![0x91; data_len],
                    },
                )),
            }
            .encode_to_vec();
            let stream = futures_util::stream::iter(vec![Ok(BytesMut::from(frame.as_slice()))]);
            let reader_shared = shared.clone();
            let reader = tokio::spawn(reader_task(stream, shutdown_rx, reader_shared));

            tokio::time::timeout(std::time::Duration::from_secs(1), gate.wait_until_reached())
                .await
                .unwrap_or_else(|_| {
                    panic!(
                        "reader must reach the configured {boundary} boundary on iteration {iteration}"
                    )
                });
            drop(CancellationGuard {
                shared: shared.clone(),
                request_id: 3,
                armed: true,
            });
            for reply in [overflow_rx, read_rx] {
                let error = tokio::time::timeout(std::time::Duration::from_secs(1), reply)
                    .await
                    .expect("terminal transition must resolve every pending reply")
                    .unwrap()
                    .unwrap_err();
                assert_eq!(error.name().as_str(), "runtime.queue.cancelled_full");
            }
            gate.release();
            tokio::time::timeout(std::time::Duration::from_secs(1), reader)
                .await
                .expect("reader must stop after terminal transition")
                .unwrap();
            assert_eq!(hooks.decode_calls.load(Ordering::Acquire), 0);
        }
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
