#![cfg(target_os = "linux")]

use std::io;
use std::time::Duration;

use can_hal::{
    CanFdFrame as BackendFdFrame, CanFrame as BackendClassicFrame, CanId as BackendId, ReceiveFd,
    Transmit, TransmitFd,
};
use can_hal_socketcan::{SocketCanChannel as BackendChannel, SocketCanDriver, SocketCanError};
use seeed_hal_can::{
    CanActiveConfig, CanBusStatus, CanChannel, CanFrame, CanId, CanMode, ReceivedCanFrame,
};
use seeed_hal_core::{ErrorCategory, HalError, HalResult, ResourceDescriptor};
use socketcan::{
    CanAnyFrame, CanFdSocket, CanRemoteFrame, EmbeddedFrame, ExtendedId, Id, Socket, StandardId,
};

use crate::link::LinkLease;

const MAX_RECEIVE_POLL: Duration = Duration::from_millis(100);

pub(crate) struct NativeSocketCanChannel {
    descriptor: ResourceDescriptor,
    sockets: Option<NativeSockets>,
    link: LinkLease,
    closed: bool,
}

struct NativeSockets {
    backend: BackendChannel,
    receiver: CanFdSocket,
    remote_sender: CanFdSocket,
}

impl NativeSockets {
    fn open(interface: &str, descriptor: &ResourceDescriptor) -> HalResult<Self> {
        let mut backend = SocketCanDriver::new()
            .channel_by_name(interface)
            .connect()
            .map_err(|error| map_backend_error("can.open", error, descriptor))?;
        backend
            .try_receive_fd()
            .map_err(|error| map_backend_error("can.open", error, descriptor))?;
        let receiver = CanFdSocket::open(interface).map_err(|error| {
            map_io_error("can.open", error).with_resource_id(descriptor.id().clone())
        })?;
        let remote_sender = CanFdSocket::open(interface).map_err(|error| {
            map_io_error("can.open", error).with_resource_id(descriptor.id().clone())
        })?;
        remote_sender.set_nonblocking(true).map_err(|error| {
            map_io_error("can.open", error).with_resource_id(descriptor.id().clone())
        })?;
        Ok(Self {
            backend,
            receiver,
            remote_sender,
        })
    }
}

impl NativeSocketCanChannel {
    pub(crate) fn open(descriptor: ResourceDescriptor, mut link: LinkLease) -> HalResult<Self> {
        let interface = descriptor.endpoint().as_str().to_owned();
        let sockets = match NativeSockets::open(&interface, &descriptor) {
            Ok(sockets) => sockets,
            Err(primary) => {
                return Err(link.rollback_after_open_failure(&descriptor, primary));
            }
        };
        Ok(Self {
            descriptor,
            sockets: Some(sockets),
            link,
            closed: false,
        })
    }

    fn sockets(&mut self, operation: &'static str) -> HalResult<&mut NativeSockets> {
        if self.closed {
            return Err(closed(operation, &self.descriptor));
        }
        self.sockets
            .as_mut()
            .ok_or_else(|| closed(operation, &self.descriptor))
    }
}

impl Drop for NativeSocketCanChannel {
    fn drop(&mut self) {
        self.sockets.take();
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
        receive_from_socket(
            &mut self.sockets("can.receive")?.receiver,
            timeout,
            &descriptor,
        )
    }

    fn send(&mut self, frame: &CanFrame) -> HalResult<()> {
        let descriptor = self.descriptor.clone();
        let mode = self.link.active.mode();
        send_with_mode(self.sockets("can.send")?, frame, mode, &descriptor)
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
        self.sockets.take();
        let result = self.link.close(&self.descriptor);
        if result.is_ok() {
            self.closed = true;
        }
        result
    }
}

fn receive_from_socket(
    receiver: &mut CanFdSocket,
    timeout: Duration,
    descriptor: &ResourceDescriptor,
) -> HalResult<Option<ReceivedCanFrame>> {
    let poll = timeout.min(MAX_RECEIVE_POLL);
    let frame = if poll.is_zero() {
        receiver.set_nonblocking(true).map_err(|error| {
            map_io_error("can.receive", error).with_resource_id(descriptor.id().clone())
        })?;
        let result = receiver.read_frame();
        let restore = receiver.set_nonblocking(false);
        restore.map_err(|error| {
            map_io_error("can.receive", error).with_resource_id(descriptor.id().clone())
        })?;
        result
    } else {
        receiver.read_frame_timeout(poll)
    };
    match frame {
        Ok(frame) => from_socketcan_frame(frame)
            .map(|frame| Some(ReceivedCanFrame::new(frame, None)))
            .map_err(|error| error.with_resource_id(descriptor.id().clone())),
        Err(error)
            if error.kind() == io::ErrorKind::WouldBlock
                || error.kind() == io::ErrorKind::TimedOut =>
        {
            Ok(None)
        }
        Err(error) => {
            Err(map_io_error("can.receive", error).with_resource_id(descriptor.id().clone()))
        }
    }
}

fn send_with_mode(
    sockets: &mut NativeSockets,
    frame: &CanFrame,
    mode: CanMode,
    descriptor: &ResourceDescriptor,
) -> HalResult<()> {
    frame
        .validate()
        .map_err(|error| error.with_resource_id(descriptor.id().clone()))?;
    match frame {
        CanFrame::ClassicData { id, data } => {
            let frame = BackendClassicFrame::new(to_backend_id(*id), data).ok_or_else(|| {
                invalid_frame(
                    "can.send",
                    "failed to convert a validated Classical CAN frame",
                )
                .with_resource_id(descriptor.id().clone())
            })?;
            sockets
                .backend
                .transmit(&frame)
                .map_err(|error| map_send_backend_error(error, descriptor))
        }
        CanFrame::FdData {
            id,
            data,
            bitrate_switch,
            error_state_indicator,
        } => {
            if mode != CanMode::Fd {
                return Err(unsupported_frame(
                    "can.send",
                    "CAN FD transmission requires an FD-configured link",
                    descriptor,
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
            sockets
                .backend
                .transmit_fd(&frame)
                .map_err(|error| map_send_backend_error(error, descriptor))
        }
        CanFrame::ClassicRemote { .. } => {
            let CanFrame::ClassicRemote { id, dlc } = frame else {
                unreachable!()
            };
            let remote_id = to_socketcan_id(*id);
            let remote =
                CanRemoteFrame::new_remote(remote_id, usize::from(*dlc)).ok_or_else(|| {
                    invalid_frame("can.send", "failed to convert remote frame")
                        .with_resource_id(descriptor.id().clone())
                })?;
            sockets
                .remote_sender
                .write_frame(&remote)
                .map_err(|error| map_send_io_error(error, descriptor))
        }
        CanFrame::Error { .. } => Err(unsupported_frame(
            "can.send",
            "SocketCAN error frames are receive diagnostics and cannot be transmitted",
            descriptor,
        )),
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
        CanId::Standard(value) => {
            Id::Standard(StandardId::new(value).expect("validated standard CAN ID"))
        }
        CanId::Extended(value) => {
            Id::Extended(ExtendedId::new(value).expect("validated extended CAN ID"))
        }
    }
}

fn from_socketcan_frame(frame: CanAnyFrame) -> HalResult<CanFrame> {
    match frame {
        CanAnyFrame::Normal(frame) => {
            CanFrame::classic_data(from_embedded_id(frame.id())?, frame.data().to_vec())
        }
        CanAnyFrame::Fd(frame) => CanFrame::fd_data(
            from_embedded_id(frame.id())?,
            frame.data().to_vec(),
            frame.is_brs(),
            frame.is_esi(),
        ),
        CanAnyFrame::Remote(frame) => CanFrame::classic_remote(
            from_embedded_id(frame.id())?,
            u8::try_from(frame.dlc())
                .map_err(|_| invalid_frame("can.receive", "remote DLC exceeds u8"))?,
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
    if error.kind() == io::ErrorKind::WouldBlock || error.raw_os_error() == Some(libc::ENOBUFS) {
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

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU32, Ordering};

    use seeed_hal_can::{
        CanBusState, CanFilter, CanFilterSet, CanFrameClasses, CanIdFormat, IdentityQuality,
        can_classic_capability, can_fd_capability,
    };
    use seeed_hal_core::{CapabilitySet, Endpoint, ResourceId, ResourceProperties, TransportKind};
    use socketcan::nl::{CanInterface, Mtu};

    use super::*;

    static NEXT_INTERFACE: AtomicU32 = AtomicU32::new(0);

    struct VcanFixture {
        name: String,
        interface: Option<CanInterface>,
    }

    impl VcanFixture {
        fn create(fd: bool) -> Self {
            let suffix = NEXT_INTERFACE.fetch_add(1, Ordering::Relaxed) & 0xffff;
            let name = format!("st{:06x}{suffix:04x}", std::process::id() & 0x00ff_ffff);
            let interface = CanInterface::create_vcan(&name, None)
                .unwrap_or_else(|error| panic!("create {name}: {error}"));
            let mut fixture = Self {
                name,
                interface: Some(interface),
            };
            if fd {
                if let Err(error) = fixture
                    .interface
                    .as_ref()
                    .expect("new fixture interface")
                    .set_mtu(Mtu::Fd)
                {
                    let cleanup = fixture.delete().err();
                    panic!(
                        "set FD MTU on {}: {error}; cleanup={cleanup:?}",
                        fixture.name
                    );
                }
            }
            if let Err(error) = fixture
                .interface
                .as_ref()
                .expect("new fixture interface")
                .bring_up()
            {
                let cleanup = fixture.delete().err();
                panic!("bring up {}: {error}; cleanup={cleanup:?}", fixture.name);
            }
            fixture
        }

        fn delete(&mut self) -> Result<(), String> {
            let Some(interface) = self.interface.take() else {
                return Ok(());
            };
            match interface.delete() {
                Ok(()) => Ok(()),
                Err((interface, error)) => {
                    self.interface = Some(interface);
                    Err(error.to_string())
                }
            }
        }
    }

    impl Drop for VcanFixture {
        fn drop(&mut self) {
            let _ = self.delete();
        }
    }

    fn descriptor(name: &str, fd: bool) -> ResourceDescriptor {
        let mut capabilities = vec![can_classic_capability()];
        if fd {
            capabilities.push(can_fd_capability());
        }
        ResourceDescriptor::new(
            ResourceId::parse(format!("can:endpoint:{name}"))
                .expect("test interface name is a valid resource ID segment"),
            Endpoint::new(name.to_owned()).expect("test interface name is a valid endpoint"),
            IdentityQuality::Weak,
            TransportKind::Can,
            ResourceProperties::default(),
            CapabilitySet::new(capabilities),
        )
    }

    #[test]
    #[ignore = "requires Linux vcan and CAP_NET_ADMIN"]
    fn vcan_classic_loopback_filter_status_and_deletion_are_structured() {
        let mut fixture = VcanFixture::create(false);
        let descriptor = descriptor(&fixture.name, false);
        let mut sockets = NativeSockets::open(&fixture.name, &descriptor)
            .expect("open adapter-private vcan sockets");
        let accepted = CanFrame::classic_data(CanId::standard(0x123).expect("valid ID"), [1, 2, 3])
            .expect("valid Classical frame");
        let rejected = CanFrame::classic_data(CanId::standard(0x223).expect("valid ID"), [4, 5, 6])
            .expect("valid Classical frame");
        send_with_mode(&mut sockets, &accepted, CanMode::Classic, &descriptor)
            .expect("send accepted frame");
        send_with_mode(&mut sockets, &rejected, CanMode::Classic, &descriptor)
            .expect("send rejected frame");
        let received = [
            receive_from_socket(
                &mut sockets.receiver,
                Duration::from_millis(100),
                &descriptor,
            )
            .expect("receive first frame")
            .expect("first loopback frame"),
            receive_from_socket(
                &mut sockets.receiver,
                Duration::from_millis(100),
                &descriptor,
            )
            .expect("receive second frame")
            .expect("second loopback frame"),
        ];
        let filters = CanFilterSet::new(vec![
            CanFilter::new(
                0x100,
                0x700,
                CanIdFormat::Standard,
                CanFrameClasses::data_only(),
            )
            .expect("valid software filter"),
        ])
        .expect("valid filter set");
        let filtered = received
            .iter()
            .filter(|received| filters.matches(received.frame()))
            .collect::<Vec<_>>();
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].frame(), &accepted);

        let status = crate::link::bus_status_for_interface(&fixture.name, &descriptor)
            .expect("query vcan status");
        assert!(matches!(
            status.state(),
            CanBusState::Active | CanBusState::Unknown
        ));
        assert!(status.tx_error_counter().is_none_or(|value| value == 0));
        assert!(status.rx_error_counter().is_none_or(|value| value == 0));

        let wrong_descriptor = descriptor("identity-mismatch", false);
        let identity_error =
            crate::link::bus_status_for_interface(&fixture.name, &wrong_descriptor)
                .expect_err("status must reject a stale canonical resource identity");
        assert_eq!(identity_error.name().as_str(), "runtime.resource.not_found");
        assert_eq!(identity_error.operation().as_str(), "can.status");
        assert_eq!(identity_error.resource_id(), Some(wrong_descriptor.id()));

        fixture.delete().expect("delete vcan interface");
        let error = send_with_mode(&mut sockets, &accepted, CanMode::Classic, &descriptor)
            .expect_err("send after interface deletion must fail");
        assert_eq!(error.operation().as_str(), "can.send");
        assert_eq!(error.resource_id(), Some(descriptor.id()));
    }

    #[test]
    #[ignore = "requires Linux vcan and CAP_NET_ADMIN"]
    fn vcan_fd_loopback_uses_explicit_test_mode_without_timing_claims() {
        let fixture = VcanFixture::create(true);
        let descriptor = descriptor(&fixture.name, true);
        let mut sockets = NativeSockets::open(&fixture.name, &descriptor)
            .expect("open adapter-private FD vcan sockets");
        let frame = CanFrame::fd_data(
            CanId::extended(0x12345).expect("valid ID"),
            [0x5a; 12],
            true,
            true,
        )
        .expect("valid FD frame");

        send_with_mode(&mut sockets, &frame, CanMode::Fd, &descriptor).expect("send FD frame");
        let received = receive_from_socket(
            &mut sockets.receiver,
            Duration::from_millis(100),
            &descriptor,
        )
        .expect("receive FD frame")
        .expect("FD loopback frame");
        assert_eq!(received.frame(), &frame);
    }

    #[test]
    fn permission_errors_are_stable_and_resource_scoped() {
        let descriptor = descriptor("can-test", false);
        let error = map_io_error("can.open", io::Error::from_raw_os_error(libc::EPERM))
            .with_resource_id(descriptor.id().clone());

        assert_eq!(error.name().as_str(), "runtime.transport.permission_denied");
        assert_eq!(error.category(), ErrorCategory::Conflict);
        assert_eq!(error.operation().as_str(), "can.open");
        assert!(!error.retryable());
        assert_eq!(error.platform_code(), Some("1"));
        assert_eq!(error.resource_id(), Some(descriptor.id()));
    }
}
