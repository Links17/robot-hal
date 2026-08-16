#![cfg(target_os = "linux")]

use std::io;
use std::time::Duration;

use can_hal::{
    CanFdFrame as BackendFdFrame, CanFrame as BackendClassicFrame, CanId as BackendId,
    ReceiveFd, Transmit, TransmitFd,
};
use can_hal_socketcan::{SocketCanChannel as BackendChannel, SocketCanDriver, SocketCanError};
use socketcan::{CanAnyFrame, CanFdSocket, CanRemoteFrame, EmbeddedFrame, ExtendedId, Id,
    Socket, StandardId};
use seeed_hal_can::{
    CanActiveConfig, CanBusStatus, CanChannel, CanFrame, CanId, CanMode, ReceivedCanFrame,
};
use seeed_hal_core::{ErrorCategory, HalError, HalResult, ResourceDescriptor};

use crate::link::LinkLease;

const MAX_RECEIVE_POLL: Duration = Duration::from_millis(100);

pub(crate) struct NativeSocketCanChannel {
    descriptor: ResourceDescriptor,
    backend: Option<BackendChannel>,
    receiver: Option<CanFdSocket>,
    remote_sender: Option<CanFdSocket>,
    link: LinkLease,
    closed: bool,
}

impl NativeSocketCanChannel {
    pub(crate) fn open(
        descriptor: ResourceDescriptor,
        mut link: LinkLease,
    ) -> HalResult<Self> {
        let interface = descriptor.endpoint().as_str().to_owned();
        let mut backend = match SocketCanDriver::new()
            .channel_by_name(&interface)
            .connect()
        {
            Ok(backend) => backend,
            Err(error) => {
                let primary = map_backend_error("can.open", error, &descriptor);
                return Err(link.rollback_after_open_failure(&descriptor, primary));
            }
        };
        if let Err(error) = backend.try_receive_fd() {
            let primary = map_backend_error("can.open", error, &descriptor);
            return Err(link.rollback_after_open_failure(&descriptor, primary));
        }
        let receiver = match CanFdSocket::open(&interface) {
            Ok(receiver) => receiver,
            Err(error) => {
                let primary = map_io_error("can.open", error)
                    .with_resource_id(descriptor.id().clone());
                return Err(link.rollback_after_open_failure(&descriptor, primary));
            }
        };
        let remote_sender = match CanFdSocket::open(&interface) {
            Ok(sender) => sender,
            Err(error) => {
                let primary = map_io_error("can.open", error)
                    .with_resource_id(descriptor.id().clone());
                return Err(link.rollback_after_open_failure(&descriptor, primary));
            }
        };
        if let Err(error) = remote_sender.set_nonblocking(true) {
            let primary =
                map_io_error("can.open", error).with_resource_id(descriptor.id().clone());
            return Err(link.rollback_after_open_failure(&descriptor, primary));
        }
        Ok(Self {
            descriptor,
            backend: Some(backend),
            receiver: Some(receiver),
            remote_sender: Some(remote_sender),
            link,
            closed: false,
        })
    }

    fn backend(&mut self, operation: &'static str) -> HalResult<&mut BackendChannel> {
        if self.closed {
            return Err(closed(operation, &self.descriptor));
        }
        self.backend
            .as_mut()
            .ok_or_else(|| closed(operation, &self.descriptor))
    }

    fn receiver(&mut self, operation: &'static str) -> HalResult<&mut CanFdSocket> {
        if self.closed {
            return Err(closed(operation, &self.descriptor));
        }
        self.receiver
            .as_mut()
            .ok_or_else(|| closed(operation, &self.descriptor))
    }

    fn remote_sender(&mut self, operation: &'static str) -> HalResult<&mut CanFdSocket> {
        if self.closed {
            return Err(closed(operation, &self.descriptor));
        }
        self.remote_sender
            .as_mut()
            .ok_or_else(|| closed(operation, &self.descriptor))
    }
}

impl Drop for NativeSocketCanChannel {
    fn drop(&mut self) {
        self.backend.take();
        self.receiver.take();
        self.remote_sender.take();
        if !self.closed {
            let _ = self.link.close(&self.descriptor);
            self.closed = true;
        }
    }
}

impl CanChannel for NativeSocketCanChannel {
    fn descriptor(&self) -> &ResourceDescriptor {
        &self.descriptor
    }

    fn active_config(&self) -> &CanActiveConfig {
        &self.link.active
    }

    fn receive(&mut self, timeout: Duration) -> HalResult<Option<ReceivedCanFrame>> {
        let descriptor = self.descriptor.clone();
        let poll = timeout.min(MAX_RECEIVE_POLL);
        let frame = if poll.is_zero() {
            let receiver = self.receiver("can.receive")?;
            receiver
                .set_nonblocking(true)
                .map_err(|error| {
                    map_io_error("can.receive", error)
                        .with_resource_id(descriptor.id().clone())
                })?;
            let result = receiver.read_frame();
            let restore = receiver.set_nonblocking(false);
            restore.map_err(|error| {
                map_io_error("can.receive", error)
                    .with_resource_id(descriptor.id().clone())
            })?;
            result
        } else {
            self.receiver("can.receive")?.read_frame_timeout(poll)
        };
        match frame {
            Ok(frame) => from_socketcan_frame(frame)
                .map(|frame| Some(ReceivedCanFrame::new(frame, None)))
                .map_err(|error| error.with_resource_id(descriptor.id().clone())),
            Err(error) if error.kind() == io::ErrorKind::WouldBlock || error.kind() == io::ErrorKind::TimedOut => Ok(None),
            Err(error) => Err(map_io_error("can.receive", error)
                .with_resource_id(descriptor.id().clone())),
        }
    }

    fn send(&mut self, frame: &CanFrame) -> HalResult<()> {
        let descriptor = self.descriptor.clone();
        frame
            .validate()
            .map_err(|error| error.with_resource_id(descriptor.id().clone()))?;
        match frame {
            CanFrame::ClassicData { id, data } => {
                let frame = BackendClassicFrame::new(to_backend_id(*id), data).ok_or_else(|| {
                    invalid_frame("can.send", "failed to convert a validated Classical CAN frame")
                        .with_resource_id(descriptor.id().clone())
                })?;
                self.backend("can.send")?
                    .transmit(&frame)
                    .map_err(|error| map_send_backend_error(error, &descriptor))
            }
            CanFrame::FdData {
                id,
                data,
                bitrate_switch,
                error_state_indicator,
            } => {
                if self.link.active.mode() != CanMode::Fd {
                    return Err(unsupported_frame(
                        "can.send",
                        "CAN FD transmission requires an FD-configured link",
                        &descriptor,
                    ));
                }
                let frame = BackendFdFrame::new(
                    to_backend_id(*id),
                    data,
                    *bitrate_switch,
                    *error_state_indicator,
                )
                .ok_or_else(|| {
                    invalid_frame("can.send", "failed to convert a validated CAN FD frame")
                        .with_resource_id(descriptor.id().clone())
                })?;
                self.backend("can.send")?
                    .transmit_fd(&frame)
                    .map_err(|error| map_send_backend_error(error, &descriptor))
            }
            CanFrame::ClassicRemote { .. } => {
                let CanFrame::ClassicRemote { id, dlc } = frame else {
                    unreachable!()
                };
                let remote_id = to_socketcan_id(*id);
                let remote = CanRemoteFrame::new_remote(remote_id, usize::from(*dlc))
                    .ok_or_else(|| {
                        invalid_frame("can.send", "failed to convert remote frame")
                            .with_resource_id(descriptor.id().clone())
                    })?;
                self.remote_sender("can.send")?
                    .write_frame(&remote)
                    .map_err(|error| map_send_io_error(error, &descriptor))
            }
            CanFrame::Error { .. } => {
                return Err(unsupported_frame(
                    "can.send",
                    "SocketCAN error frames are receive diagnostics and cannot be transmitted",
                    &descriptor,
                ));
            }
        }
    }

    fn bus_status(&mut self) -> HalResult<CanBusStatus> {
        if self.closed {
            return Err(closed("can.status", &self.descriptor));
        }
        self.link.bus_status(&self.descriptor)
    }

    fn close(&mut self) -> HalResult<()> {
        if self.closed {
            return Ok(());
        }
        self.backend.take();
        self.receiver.take();
        self.remote_sender.take();
        let result = self.link.close(&self.descriptor);
        if result.is_ok() {
            self.closed = true;
        }
        result
    }
}

fn to_backend_id(id: CanId) -> BackendId {
    match id {
        CanId::Standard(value) => BackendId::Standard(value),
        CanId::Extended(value) => BackendId::Extended(value),
    }
}

fn to_socketcan_id(id: CanId) -> Id {
    match id {
        CanId::Standard(value) => Id::Standard(StandardId::new(value).expect("validated standard CAN ID")),
        CanId::Extended(value) => Id::Extended(ExtendedId::new(value).expect("validated extended CAN ID")),
    }
}

fn from_socketcan_frame(frame: CanAnyFrame) -> HalResult<CanFrame> {
    match frame {
        CanAnyFrame::Normal(frame) => CanFrame::classic_data(
            from_embedded_id(frame.id())?,
            frame.data().to_vec(),
        ),
        CanAnyFrame::Fd(frame) => CanFrame::fd_data(
            from_embedded_id(frame.id())?,
            frame.data().to_vec(),
            frame.is_brs(),
            frame.is_esi(),
        ),
        CanAnyFrame::Remote(frame) => CanFrame::classic_remote(
            from_embedded_id(frame.id())?,
            u8::try_from(frame.dlc()).map_err(|_| invalid_frame("can.receive", "remote DLC exceeds u8"))?,
        ),
        CanAnyFrame::Error(frame) => {
            let bits = frame.error_bits();
            let mut classes = Vec::new();
            for (mask, class) in [
                (0x0001, seeed_hal_can::CanErrorClass::TxTimeout),
                (0x0002, seeed_hal_can::CanErrorClass::LostArbitration),
                (0x0004, seeed_hal_can::CanErrorClass::Controller),
                (0x0008, seeed_hal_can::CanErrorClass::Protocol),
                (0x0010, seeed_hal_can::CanErrorClass::Transceiver),
                (0x0020, seeed_hal_can::CanErrorClass::NoAcknowledgement),
                (0x0040, seeed_hal_can::CanErrorClass::BusOff),
                (0x0080, seeed_hal_can::CanErrorClass::BusError),
                (0x0100, seeed_hal_can::CanErrorClass::Restarted),
            ] {
                if bits & mask != 0 {
                    classes.push(class);
                }
            }
            if classes.is_empty() {
                classes.push(seeed_hal_can::CanErrorClass::Other);
            }
            CanFrame::error(classes, frame.data().to_vec())
        }
    }
}

fn from_embedded_id(id: Id) -> HalResult<CanId> {
    match id {
        Id::Standard(value) => CanId::standard(value.as_raw()),
        Id::Extended(value) => CanId::extended(value.as_raw()),
    }
}

fn map_backend_error(
    operation: &'static str,
    error: SocketCanError,
    descriptor: &ResourceDescriptor,
) -> HalError {
    let mapped = match error {
        SocketCanError::Io(error) => map_io_error(operation, error),
        SocketCanError::InvalidFrame(message) => invalid_frame(operation, message),
        SocketCanError::InvalidInterface(message) => HalError::new(
            "runtime.resource.not_found",
            ErrorCategory::NotFound,
            operation,
            false,
            format!("SocketCAN interface is unavailable: {message}"),
        )
        .expect("static SocketCAN error metadata is valid"),
        _ => HalError::new(
            "runtime.transport.unavailable",
            ErrorCategory::Unavailable,
            operation,
            true,
            "SocketCAN backend returned an unknown error",
        )
        .expect("static SocketCAN error metadata is valid"),
    };
    mapped.with_resource_id(descriptor.id().clone())
}

fn map_send_backend_error(error: SocketCanError, descriptor: &ResourceDescriptor) -> HalError {
    match error {
        SocketCanError::Io(error) => map_send_io_error(error, descriptor),
        error => map_backend_error("can.send", error, descriptor),
    }
}

fn map_send_io_error(error: io::Error, descriptor: &ResourceDescriptor) -> HalError {
    if error.kind() == io::ErrorKind::WouldBlock
        || error.raw_os_error() == Some(libc::ENOBUFS)
    {
        return queue_full(error.raw_os_error(), descriptor);
    }
    map_io_error("can.send", error).with_resource_id(descriptor.id().clone())
}

fn map_io_error(operation: &'static str, error: io::Error) -> HalError {
    let raw_code = error.raw_os_error();
    let (name, category, retryable) = match raw_code {
        Some(libc::ENODEV | libc::ENOENT | libc::ENXIO) => {
            ("runtime.resource.not_found", ErrorCategory::NotFound, false)
        }
        Some(libc::EPERM | libc::EACCES) => (
            "runtime.transport.permission_denied",
            ErrorCategory::Conflict,
            false,
        ),
        Some(libc::EINVAL | libc::EOPNOTSUPP | libc::ERANGE | libc::EMSGSIZE) => (
            "runtime.transport.unsupported_configuration",
            ErrorCategory::InvalidArgument,
            false,
        ),
        Some(
            libc::ENETDOWN
            | libc::ENETUNREACH
            | libc::ENOTCONN
            | libc::EPIPE
            | libc::ECONNABORTED
            | libc::ECONNRESET,
        ) => (
            "runtime.transport.disconnected",
            ErrorCategory::Unavailable,
            true,
        ),
        _ => match error.kind() {
            io::ErrorKind::NotFound => {
                ("runtime.resource.not_found", ErrorCategory::NotFound, false)
            }
            io::ErrorKind::PermissionDenied => (
                "runtime.transport.permission_denied",
                ErrorCategory::Conflict,
                false,
            ),
            io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut => (
                "runtime.transport.timeout",
                ErrorCategory::Unavailable,
                true,
            ),
            io::ErrorKind::InvalidInput | io::ErrorKind::InvalidData => (
                "runtime.transport.unsupported_configuration",
                ErrorCategory::InvalidArgument,
                false,
            ),
            io::ErrorKind::BrokenPipe
            | io::ErrorKind::ConnectionAborted
            | io::ErrorKind::ConnectionReset
            | io::ErrorKind::NotConnected
            | io::ErrorKind::UnexpectedEof => (
                "runtime.transport.disconnected",
                ErrorCategory::Unavailable,
                true,
            ),
            _ => (
                "runtime.transport.unavailable",
                ErrorCategory::Unavailable,
                true,
            ),
        },
    };
    let mut mapped = HalError::new(
        name,
        category,
        operation,
        retryable,
        format!(
            "SocketCAN I/O failed kind={:?} raw_os_error={}: {error}",
            error.kind(),
            raw_code.map_or_else(|| "none".to_owned(), |value| value.to_string())
        ),
    )
    .expect("static SocketCAN error metadata is valid");
    if let Some(raw_code) = raw_code {
        mapped = mapped
            .with_platform_code(raw_code.to_string())
            .expect("decimal OS error code is a valid platform code");
    }
    mapped
}

fn invalid_frame(operation: &'static str, message: impl Into<String>) -> HalError {
    HalError::new(
        "can.frame.invalid",
        ErrorCategory::InvalidArgument,
        operation,
        false,
        message,
    )
    .expect("static SocketCAN error metadata is valid")
}

fn unsupported_frame(
    operation: &'static str,
    message: &'static str,
    descriptor: &ResourceDescriptor,
) -> HalError {
    HalError::new(
        "runtime.transport.unsupported_configuration",
        ErrorCategory::InvalidArgument,
        operation,
        false,
        message,
    )
    .expect("static SocketCAN error metadata is valid")
    .with_resource_id(descriptor.id().clone())
}

fn queue_full(raw_code: Option<i32>, descriptor: &ResourceDescriptor) -> HalError {
    let error = HalError::new(
        "runtime.queue.full",
        ErrorCategory::Unavailable,
        "can.send",
        true,
        "SocketCAN transmit queue is full",
    )
    .expect("static SocketCAN error metadata is valid")
    .with_resource_id(descriptor.id().clone());
    match raw_code {
        Some(raw_code) => error
            .with_platform_code(raw_code.to_string())
            .expect("decimal OS error code is a valid platform code"),
        None => error,
    }
}

fn closed(operation: &'static str, descriptor: &ResourceDescriptor) -> HalError {
    HalError::new(
        "runtime.session.closed",
        ErrorCategory::Conflict,
        operation,
        false,
        "SocketCAN channel is already closed",
    )
    .expect("static SocketCAN error metadata is valid")
    .with_resource_id(descriptor.id().clone())
}
