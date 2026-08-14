use async_trait::async_trait;
use bytes::Bytes;
use seeed_hal_core::{HalError, HalResult, ResourceDescriptor};
use seeed_hal_serial::{
    ControlLines, DataBits, FlowControl, Parity, SerialConfig, SerialSession, StopBits,
};
use tokio::task::JoinHandle;

use crate::{internal, invalid_argument, map_io_error, session_closed, timeout};

type SerialStream = serial2::SerialPort;
type DrainWorkerResult = HalResult<(SerialStream, HalResult<()>)>;
type CloseWorkerResult = HalResult<HalResult<()>>;

struct DrainTask {
    operation: &'static str,
    join: JoinHandle<DrainWorkerResult>,
}

struct CloseTask {
    operation: &'static str,
    join: JoinHandle<CloseWorkerResult>,
}

trait DrainStrategy: Send + Sync {
    fn spawn_flush(&self, operation: &'static str, stream: SerialStream) -> DrainTask;
    fn spawn_close(&self, operation: &'static str, stream: SerialStream) -> CloseTask;
}

struct BlockingDrainStrategy;

impl DrainStrategy for BlockingDrainStrategy {
    fn spawn_flush(&self, operation: &'static str, stream: SerialStream) -> DrainTask {
        let join = tokio::task::spawn_blocking(move || {
            let result = stream
                .flush()
                .map_err(|error| map_io_error(operation, error));
            Ok((stream, result))
        });

        DrainTask { operation, join }
    }

    fn spawn_close(&self, operation: &'static str, stream: SerialStream) -> CloseTask {
        let join = tokio::task::spawn_blocking(move || {
            let result = stream
                .flush()
                .map_err(|error| map_io_error(operation, error));
            drop(stream);
            Ok(result)
        });

        CloseTask { operation, join }
    }
}

enum SessionState {
    Ready(SerialStream),
    Draining(DrainTask),
    Closing(CloseTask),
    Closed,
}

pub(crate) struct NativeSerialSession {
    descriptor: ResourceDescriptor,
    config: SerialConfig,
    state: SessionState,
    drain_strategy: std::sync::Arc<dyn DrainStrategy>,
}

impl NativeSerialSession {
    pub(crate) async fn open(
        descriptor: ResourceDescriptor,
        config: SerialConfig,
    ) -> HalResult<Self> {
        validate_config(&config)?;

        let stream = open_serial_stream(descriptor.endpoint().as_str(), &config)?;

        Ok(Self {
            descriptor,
            config,
            state: SessionState::Ready(stream),
            drain_strategy: std::sync::Arc::new(BlockingDrainStrategy),
        })
    }

    async fn ensure_ready(&mut self, operation: &'static str) -> HalResult<()> {
        loop {
            match self.state {
                SessionState::Ready(_) => return Ok(()),
                SessionState::Closed => {
                    return Err(session_closed(
                        operation,
                        "serial session is already closed",
                    ));
                }
                SessionState::Draining(_) => {
                    self.finish_tracked_drain().await??;
                }
                SessionState::Closing(_) => {
                    let result = self.finish_tracked_close().await?;
                    if let Err(error) = result {
                        if error.name().as_str() != "runtime.transport.disconnected" {
                            return Err(error);
                        }
                    }
                    return Err(session_closed(operation, "serial session is closing"));
                }
            }
        }
    }

    fn ready_stream_mut(&mut self, operation: &'static str) -> HalResult<&mut SerialStream> {
        match &mut self.state {
            SessionState::Ready(stream) => Ok(stream),
            SessionState::Closed => Err(session_closed(
                operation,
                "serial session is already closed",
            )),
            SessionState::Draining(_) => Err(session_closed(
                operation,
                "serial session drain is still in progress",
            )),
            SessionState::Closing(_) => Err(session_closed(operation, "serial session is closing")),
        }
    }

    fn take_ready_stream(&mut self, operation: &'static str) -> HalResult<SerialStream> {
        match std::mem::replace(&mut self.state, SessionState::Closed) {
            SessionState::Ready(stream) => Ok(stream),
            state => {
                self.state = state;
                Err(session_closed(operation, "serial session is not ready"))
            }
        }
    }

    fn begin_tracked_drain(&mut self, operation: &'static str) -> HalResult<()> {
        let stream = self.take_ready_stream(operation)?;
        self.state = SessionState::Draining(self.drain_strategy.spawn_flush(operation, stream));
        Ok(())
    }

    fn begin_tracked_close(&mut self) -> HalResult<()> {
        let stream = self.take_ready_stream("serial.close")?;
        self.state = SessionState::Closing(self.drain_strategy.spawn_close("serial.close", stream));
        Ok(())
    }

    async fn finish_tracked_drain(&mut self) -> HalResult<HalResult<()>> {
        let (operation, join_result) = match &mut self.state {
            SessionState::Draining(task) => (task.operation, (&mut task.join).await),
            SessionState::Ready(_) => return Ok(Ok(())),
            SessionState::Closing(_) => {
                return Err(session_closed("serial.drain", "serial session is closing"));
            }
            SessionState::Closed => {
                return Err(session_closed(
                    "serial.drain",
                    "serial session is already closed",
                ));
            }
        };

        match join_result {
            Ok(Ok((stream, drain_result))) => {
                self.state = SessionState::Ready(stream);
                Ok(drain_result)
            }
            Ok(Err(error)) => {
                self.state = SessionState::Closed;
                Err(error)
            }
            Err(error) => {
                self.state = SessionState::Closed;
                Err(internal(
                    operation,
                    format!("serial drain blocking worker failed: {error}"),
                ))
            }
        }
    }

    async fn finish_tracked_close(&mut self) -> HalResult<HalResult<()>> {
        let (operation, join_result) = match &mut self.state {
            SessionState::Closing(task) => (task.operation, (&mut task.join).await),
            SessionState::Closed => return Ok(Ok(())),
            SessionState::Ready(_) | SessionState::Draining(_) => {
                return Err(session_closed(
                    "serial.close",
                    "serial session is not closing",
                ));
            }
        };

        self.state = SessionState::Closed;
        match join_result {
            Ok(result) => result,
            Err(error) => Err(internal(
                operation,
                format!("serial close blocking worker failed: {error}"),
            )),
        }
    }
}

#[async_trait]
impl SerialSession for NativeSerialSession {
    fn descriptor(&self) -> &ResourceDescriptor {
        &self.descriptor
    }

    async fn read(&mut self, max_bytes: usize) -> HalResult<Bytes> {
        self.ensure_ready("serial.read").await?;

        if max_bytes == 0 {
            return Err(invalid_argument(
                "serial.read",
                "max_bytes must be greater than zero",
            ));
        }

        let read_timeout = self.config.read_timeout;
        let stream = self
            .ready_stream_mut("serial.read")?
            .try_clone()
            .map_err(|error| map_io_error("serial.read", error))?;
        let mut buffer = vec![0_u8; max_bytes];

        let (read, mut buffer) = tokio::time::timeout(
            read_timeout,
            tokio::task::spawn_blocking(move || {
                stream
                    .read(&mut buffer)
                    .map(|read| (read, buffer))
                    .map_err(|error| map_io_error("serial.read", error))
            }),
        )
        .await
        .map_err(|_| {
            timeout(
                "serial.read",
                format!("read timed out after {read_timeout:?}"),
            )
        })?
        .map_err(|error| {
            internal(
                "serial.read",
                format!("serial read blocking worker failed: {error}"),
            )
        })??;

        if read == 0 {
            return Err(disconnected(
                "serial.read",
                "serial port returned end of stream",
            ));
        }

        buffer.truncate(read);
        Ok(Bytes::from(buffer))
    }

    async fn write_all(&mut self, bytes: &[u8]) -> HalResult<()> {
        self.ensure_ready("serial.write").await?;

        if bytes.is_empty() {
            return Ok(());
        }

        let stream = self
            .ready_stream_mut("serial.write")?
            .try_clone()
            .map_err(|error| map_io_error("serial.write", error))?;
        let bytes = bytes.to_vec();

        tokio::task::spawn_blocking(move || {
            stream
                .write_all(&bytes)
                .map_err(|error| map_io_error("serial.write", error))
        })
        .await
        .map_err(|error| {
            internal(
                "serial.write",
                format!("serial write blocking worker failed: {error}"),
            )
        })?
    }

    async fn flush(&mut self) -> HalResult<()> {
        self.ensure_ready("serial.flush").await?;
        self.begin_tracked_drain("serial.flush")?;
        self.finish_tracked_drain().await?
    }

    async fn set_control_lines(&mut self, lines: ControlLines) -> HalResult<()> {
        self.ensure_ready("serial.set_control_lines").await?;
        let stream = self.ready_stream_mut("serial.set_control_lines")?;
        stream
            .set_dtr(lines.data_terminal_ready)
            .map_err(|error| map_io_error("serial.set_control_lines", error))?;
        stream
            .set_rts(lines.request_to_send)
            .map_err(|error| map_io_error("serial.set_control_lines", error))
    }

    async fn close(&mut self) -> HalResult<()> {
        let mut pending_flush_error = None;

        loop {
            match self.state {
                SessionState::Ready(_) => self.begin_tracked_close()?,
                SessionState::Draining(_) => {
                    let drain_result = self.finish_tracked_drain().await?;
                    match drain_result {
                        Ok(()) => {}
                        Err(error) if error.name().as_str() == "runtime.transport.disconnected" => {
                        }
                        Err(error) => {
                            if pending_flush_error.is_none() {
                                pending_flush_error = Some(error);
                            }
                        }
                    }
                }
                SessionState::Closing(_) => {
                    let close_result = self.finish_tracked_close().await?;
                    match close_result {
                        Ok(()) => {}
                        Err(error) if error.name().as_str() == "runtime.transport.disconnected" => {
                        }
                        Err(error) => return Err(error),
                    }

                    return pending_flush_error.map_or(Ok(()), Err);
                }
                SessionState::Closed => return pending_flush_error.map_or(Ok(()), Err),
            }
        }
    }
}

fn open_serial_stream(endpoint: &str, config: &SerialConfig) -> HalResult<SerialStream> {
    let mut stream = serial2::SerialPort::open(endpoint, |mut settings: serial2::Settings| {
        settings.set_raw();
        settings.set_baud_rate(config.baud_rate)?;
        settings.set_char_size(map_data_bits(config.data_bits));
        settings.set_parity(map_parity(config.parity));
        settings.set_stop_bits(map_stop_bits(config.stop_bits));
        settings.set_flow_control(map_flow_control(config.flow_control));
        Ok(settings)
    })
    .map_err(|error| map_io_error("serial.open", error))?;

    stream
        .set_read_timeout(config.read_timeout)
        .map_err(|error| map_io_error("serial.open", error))?;
    stream
        .set_write_timeout(config.read_timeout)
        .map_err(|error| map_io_error("serial.open", error))?;

    Ok(stream)
}

#[cfg(test)]
async fn run_blocking_drain<T, F>(drain: F) -> HalResult<T>
where
    T: Send + 'static,
    F: FnOnce() -> HalResult<T> + Send + 'static,
{
    tokio::task::spawn_blocking(drain).await.map_err(|error| {
        internal(
            "serial.drain",
            format!("serial drain blocking worker failed: {error}"),
        )
    })?
}

#[cfg(test)]
fn close_join_failure(operation: &'static str) -> CloseTask {
    let join = tokio::task::spawn_blocking(|| -> CloseWorkerResult {
        panic!("intentional serial drain worker panic for test")
    });

    CloseTask { operation, join }
}

impl NativeSerialSession {
    #[cfg(test)]
    fn has_tracked_drain_for_test(&self) -> bool {
        matches!(self.state, SessionState::Draining(_))
    }

    #[cfg(test)]
    fn has_tracked_close_for_test(&self) -> bool {
        matches!(self.state, SessionState::Closing(_))
    }

    #[cfg(test)]
    fn is_ready_for_test(&self) -> bool {
        matches!(self.state, SessionState::Ready(_))
    }

    #[cfg(test)]
    fn is_closed_for_test(&self) -> bool {
        matches!(self.state, SessionState::Closed)
    }

    #[cfg(test)]
    async fn finish_tracked_close_for_test(&mut self) -> HalResult<HalResult<()>> {
        self.finish_tracked_close().await
    }

    #[cfg(test)]
    fn set_join_failure_for_test(&mut self, operation: &'static str) {
        self.state = SessionState::Closing(close_join_failure(operation));
    }
}

fn validate_config(config: &SerialConfig) -> HalResult<()> {
    if config.baud_rate == 0 {
        return Err(invalid_argument(
            "serial.open",
            "baud_rate must be greater than zero",
        ));
    }

    if config.read_timeout.is_zero() {
        return Err(invalid_argument(
            "serial.open",
            "read_timeout must be greater than zero",
        ));
    }

    Ok(())
}

fn map_data_bits(data_bits: DataBits) -> serial2::CharSize {
    match data_bits {
        DataBits::Five => serial2::CharSize::Bits5,
        DataBits::Six => serial2::CharSize::Bits6,
        DataBits::Seven => serial2::CharSize::Bits7,
        DataBits::Eight => serial2::CharSize::Bits8,
    }
}

fn map_parity(parity: Parity) -> serial2::Parity {
    match parity {
        Parity::None => serial2::Parity::None,
        Parity::Odd => serial2::Parity::Odd,
        Parity::Even => serial2::Parity::Even,
    }
}

fn map_stop_bits(stop_bits: StopBits) -> serial2::StopBits {
    match stop_bits {
        StopBits::One => serial2::StopBits::One,
        StopBits::Two => serial2::StopBits::Two,
    }
}

fn map_flow_control(flow_control: FlowControl) -> serial2::FlowControl {
    match flow_control {
        FlowControl::None => serial2::FlowControl::None,
        FlowControl::Software => serial2::FlowControl::XonXoff,
        FlowControl::Hardware => serial2::FlowControl::RtsCts,
    }
}

fn disconnected(operation: &'static str, debug_message: impl Into<String>) -> HalError {
    HalError::new(
        "runtime.transport.disconnected",
        seeed_hal_core::ErrorCategory::Unavailable,
        operation,
        true,
        debug_message,
    )
    .expect("static serialport adapter error metadata must be valid")
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::thread;

    use seeed_hal_core::{
        Endpoint, IdentityQuality, ResourceDescriptor, ResourceId, ResourceProperties,
        TransportKind,
    };

    use super::*;

    #[tokio::test(flavor = "multi_thread", worker_threads = 1)]
    async fn blocking_drain_runs_outside_tokio_worker() {
        let caller_thread = thread::current().id();

        let drain_thread = run_blocking_drain(|| Ok(thread::current().id()))
            .await
            .unwrap();

        assert_ne!(drain_thread, caller_thread);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn closed_state_takes_precedence_over_invalid_read_size() {
        let (_master, slave) = test_pair();
        let mut session = test_session(slave, Arc::new(BlockingDrainStrategy));

        session.close().await.unwrap();
        let error = session.read(0).await.unwrap_err();

        assert_eq!(error.name().as_str(), "runtime.session.closed");
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn dropping_flush_future_leaves_tracked_drain_and_recovers_session() {
        let (_master, slave) = test_pair();
        let gate = Arc::new(DrainGate::new());
        let mut session = test_session(
            slave,
            Arc::new(GatedDrainStrategy {
                gate: Arc::clone(&gate),
                flush_error: false,
            }),
        );

        let mut flush = Box::pin(session.flush());
        tokio::select! {
            result = &mut flush => panic!("flush should remain blocked, got {result:?}"),
            () = gate.wait_until_entered() => {}
        }
        drop(flush);

        assert!(session.has_tracked_drain_for_test());

        gate.release();
        let error = session.read(0).await.unwrap_err();

        assert_eq!(error.name().as_str(), "runtime.argument.invalid");
        assert!(session.is_ready_for_test());
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn dropping_close_future_leaves_tracked_close_and_terminal_state() {
        let (_master, slave) = test_pair();
        let gate = Arc::new(DrainGate::new());
        let mut session = test_session(
            slave,
            Arc::new(GatedDrainStrategy {
                gate: Arc::clone(&gate),
                flush_error: false,
            }),
        );

        let mut close = Box::pin(session.close());
        tokio::select! {
            result = &mut close => panic!("close should remain blocked, got {result:?}"),
            () = gate.wait_until_entered() => {}
        }
        drop(close);

        assert!(session.has_tracked_close_for_test());

        gate.release();
        session.close().await.unwrap();

        assert!(session.is_closed_for_test());
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn dropping_close_future_releases_stream_without_later_session_poll() {
        let (_master, slave) = test_pair();
        let gate = Arc::new(DrainGate::new());
        let mut session = test_session(
            slave,
            Arc::new(GatedDrainStrategy {
                gate: Arc::clone(&gate),
                flush_error: false,
            }),
        );

        let mut close = Box::pin(session.close());
        tokio::select! {
            result = &mut close => panic!("close should remain blocked, got {result:?}"),
            () = gate.wait_until_entered() => {}
        }
        drop(close);

        gate.release();
        gate.wait_until_stream_released().await;

        assert!(gate.stream_released());
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn close_attempts_terminal_close_after_cancelled_flush_error() {
        let (_master, slave) = test_pair();
        let gate = Arc::new(DrainGate::new());
        let mut session = test_session(
            slave,
            Arc::new(GatedDrainStrategy {
                gate: Arc::clone(&gate),
                flush_error: true,
            }),
        );

        let mut flush = Box::pin(session.flush());
        tokio::select! {
            result = &mut flush => panic!("flush should remain blocked, got {result:?}"),
            () = gate.wait_until_entered() => {}
        }
        drop(flush);

        gate.release();
        let error = session.close().await.unwrap_err();

        assert_eq!(error.name().as_str(), "runtime.transport.permission_denied");
        assert!(gate.close_completed());
        assert!(session.is_closed_for_test());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn drain_join_failure_closes_session_deterministically() {
        let (_master, slave) = test_pair();
        let mut session = test_session(slave, Arc::new(BlockingDrainStrategy));
        session.set_join_failure_for_test("serial.close");

        let error = session.finish_tracked_close_for_test().await.unwrap_err();

        assert_eq!(error.name().as_str(), "runtime.internal");
        assert!(session.is_closed_for_test());
    }

    #[test]
    fn maps_serial_config_to_serial2_types() {
        assert_eq!(map_data_bits(DataBits::Five), serial2::CharSize::Bits5);
        assert_eq!(map_data_bits(DataBits::Six), serial2::CharSize::Bits6);
        assert_eq!(map_data_bits(DataBits::Seven), serial2::CharSize::Bits7);
        assert_eq!(map_data_bits(DataBits::Eight), serial2::CharSize::Bits8);
        assert_eq!(map_parity(Parity::None), serial2::Parity::None);
        assert_eq!(map_parity(Parity::Odd), serial2::Parity::Odd);
        assert_eq!(map_parity(Parity::Even), serial2::Parity::Even);
        assert_eq!(map_stop_bits(StopBits::One), serial2::StopBits::One);
        assert_eq!(map_stop_bits(StopBits::Two), serial2::StopBits::Two);
        assert_eq!(
            map_flow_control(FlowControl::None),
            serial2::FlowControl::None
        );
        assert_eq!(
            map_flow_control(FlowControl::Software),
            serial2::FlowControl::XonXoff
        );
        assert_eq!(
            map_flow_control(FlowControl::Hardware),
            serial2::FlowControl::RtsCts
        );
    }

    #[test]
    fn actual_open_missing_endpoint_carries_io_raw_os_error_from_open_path() {
        let endpoint = std::env::temp_dir()
            .join(format!("seeed-hal-missing-serial-{}", std::process::id()))
            .display()
            .to_string();
        let expected_raw_os_error = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&endpoint)
            .unwrap_err()
            .raw_os_error()
            .expect("missing path should carry a platform raw OS error");

        let error = open_serial_stream(&endpoint, &SerialConfig::default()).unwrap_err();

        assert_eq!(error.name().as_str(), "runtime.resource.not_found");
        assert!(error.debug_message().contains("io error kind=NotFound"));
        assert!(
            error
                .debug_message()
                .contains(&format!("raw_os_error={expected_raw_os_error}"))
        );
        assert!(!error.debug_message().contains("serialport error"));
        assert!(!error.debug_message().contains("native_open_error"));
    }

    fn test_session(
        stream: SerialStream,
        drain_strategy: Arc<dyn DrainStrategy>,
    ) -> NativeSerialSession {
        NativeSerialSession {
            descriptor: descriptor(),
            config: SerialConfig::default(),
            state: SessionState::Ready(stream),
            drain_strategy,
        }
    }

    #[cfg(unix)]
    fn test_pair() -> ((), SerialStream) {
        use std::os::fd::OwnedFd;

        let path = std::env::temp_dir().join(format!(
            "seeed-hal-serial-session-test-{}-{:?}",
            std::process::id(),
            thread::current().id()
        ));
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(&path)
            .unwrap();
        std::fs::remove_file(&path).unwrap();

        let owned: OwnedFd = file.into();
        ((), serial2::SerialPort::from(owned))
    }

    fn descriptor() -> ResourceDescriptor {
        ResourceDescriptor::new(
            ResourceId::parse("serial:test:loopback").unwrap(),
            Endpoint::new("test://loopback").unwrap(),
            IdentityQuality::Weak,
            TransportKind::Serial,
            ResourceProperties::default(),
        )
    }

    struct DrainGate {
        entered: AtomicBool,
        released: AtomicBool,
        stream_released: AtomicBool,
        close_completed: AtomicBool,
    }

    impl DrainGate {
        fn new() -> Self {
            Self {
                entered: AtomicBool::new(false),
                released: AtomicBool::new(false),
                stream_released: AtomicBool::new(false),
                close_completed: AtomicBool::new(false),
            }
        }

        async fn wait_until_entered(&self) {
            tokio::time::timeout(std::time::Duration::from_secs(1), async {
                while !self.entered.load(Ordering::Acquire) {
                    tokio::task::yield_now().await;
                }
            })
            .await
            .expect("drain worker should start");
        }

        fn release(&self) {
            self.released.store(true, Ordering::Release);
        }

        fn wait_for_release(&self) {
            while !self.released.load(Ordering::Acquire) {
                thread::yield_now();
            }
        }

        async fn wait_until_stream_released(&self) {
            tokio::time::timeout(std::time::Duration::from_secs(1), async {
                while !self.stream_released() {
                    tokio::task::yield_now().await;
                }
            })
            .await
            .expect("close worker should release the stream without another session poll");
        }

        fn stream_released(&self) -> bool {
            self.stream_released.load(Ordering::Acquire)
        }

        fn mark_stream_released(&self) {
            self.stream_released.store(true, Ordering::Release);
        }

        fn close_completed(&self) -> bool {
            self.close_completed.load(Ordering::Acquire)
        }

        fn mark_close_completed(&self) {
            self.close_completed.store(true, Ordering::Release);
        }
    }

    struct GatedDrainStrategy {
        gate: Arc<DrainGate>,
        flush_error: bool,
    }

    impl DrainStrategy for GatedDrainStrategy {
        fn spawn_flush(&self, operation: &'static str, stream: SerialStream) -> DrainTask {
            let gate = Arc::clone(&self.gate);
            let flush_error = self.flush_error;
            let join = tokio::task::spawn_blocking(move || {
                gate.entered.store(true, Ordering::Release);
                gate.wait_for_release();
                let result = if flush_error {
                    Err(map_io_error(
                        operation,
                        std::io::Error::from_raw_os_error(13),
                    ))
                } else {
                    Ok(())
                };
                Ok((stream, result))
            });

            DrainTask { operation, join }
        }

        fn spawn_close(&self, operation: &'static str, stream: SerialStream) -> CloseTask {
            let gate = Arc::clone(&self.gate);
            let join = tokio::task::spawn_blocking(move || {
                gate.entered.store(true, Ordering::Release);
                gate.wait_for_release();
                let result = Ok(());
                drop(stream);
                gate.mark_stream_released();
                gate.mark_close_completed();
                Ok(result)
            });

            CloseTask { operation, join }
        }
    }
}
