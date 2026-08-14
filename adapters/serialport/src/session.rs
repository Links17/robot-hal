use async_trait::async_trait;
use bytes::Bytes;
use seeed_hal_core::{HalError, HalResult, ResourceDescriptor};
use seeed_hal_serial::{
    ControlLines, DataBits, FlowControl, Parity, SerialConfig, SerialSession, StopBits,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::task::JoinHandle;
use tokio_serial::SerialPortBuilderExt;

use crate::{
    capture_native_open_error, internal, invalid_argument, map_io_error, map_serialport_error,
    map_serialport_open_error, session_closed, timeout,
};

type DrainWorkerResult = HalResult<(tokio_serial::SerialStream, HalResult<()>)>;

struct DrainTask {
    operation: &'static str,
    join: JoinHandle<DrainWorkerResult>,
}

trait DrainStrategy: Send + Sync {
    fn spawn(&self, operation: &'static str, stream: tokio_serial::SerialStream) -> DrainTask;
}

struct BlockingDrainStrategy;

impl DrainStrategy for BlockingDrainStrategy {
    fn spawn(&self, operation: &'static str, mut stream: tokio_serial::SerialStream) -> DrainTask {
        let join = tokio::task::spawn_blocking(move || {
            let result =
                std::io::Write::flush(&mut stream).map_err(|error| map_io_error(operation, error));
            Ok((stream, result))
        });

        DrainTask { operation, join }
    }
}

enum SessionState {
    Ready(tokio_serial::SerialStream),
    Draining(DrainTask),
    Closing(DrainTask),
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
                    let result = self.finish_tracked_drain().await?;
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

    fn ready_stream_mut(
        &mut self,
        operation: &'static str,
    ) -> HalResult<&mut tokio_serial::SerialStream> {
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

    fn take_ready_stream(
        &mut self,
        operation: &'static str,
    ) -> HalResult<tokio_serial::SerialStream> {
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
        self.state = SessionState::Draining(self.drain_strategy.spawn(operation, stream));
        Ok(())
    }

    fn begin_tracked_close(&mut self) -> HalResult<()> {
        let stream = self.take_ready_stream("serial.close")?;
        self.state = SessionState::Closing(self.drain_strategy.spawn("serial.close", stream));
        Ok(())
    }

    async fn finish_tracked_drain(&mut self) -> HalResult<HalResult<()>> {
        let (was_closing, operation, join_result) = match &mut self.state {
            SessionState::Draining(task) => (false, task.operation, (&mut task.join).await),
            SessionState::Closing(task) => (true, task.operation, (&mut task.join).await),
            SessionState::Ready(_) => return Ok(Ok(())),
            SessionState::Closed => {
                return Err(session_closed(
                    "serial.drain",
                    "serial session is already closed",
                ));
            }
        };

        match join_result {
            Ok(Ok((stream, drain_result))) => {
                self.state = if was_closing {
                    SessionState::Closed
                } else {
                    SessionState::Ready(stream)
                };
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
        let stream = self.ready_stream_mut("serial.read")?;
        let mut buffer = vec![0_u8; max_bytes];

        let read = tokio::time::timeout(read_timeout, stream.read(&mut buffer))
            .await
            .map_err(|_| {
                timeout(
                    "serial.read",
                    format!("read timed out after {read_timeout:?}"),
                )
            })?
            .map_err(|error| map_io_error("serial.read", error))?;

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

        self.ready_stream_mut("serial.write")?
            .write_all(bytes)
            .await
            .map_err(|error| map_io_error("serial.write", error))
    }

    async fn flush(&mut self) -> HalResult<()> {
        self.ensure_ready("serial.flush").await?;
        self.begin_tracked_drain("serial.flush")?;
        self.finish_tracked_drain().await?
    }

    async fn set_control_lines(&mut self, lines: ControlLines) -> HalResult<()> {
        self.ensure_ready("serial.set_control_lines").await?;
        let stream = self.ready_stream_mut("serial.set_control_lines")?;
        tokio_serial::SerialPort::write_data_terminal_ready(stream, lines.data_terminal_ready)
            .map_err(|error| map_serialport_error("serial.set_control_lines", error))?;
        tokio_serial::SerialPort::write_request_to_send(stream, lines.request_to_send)
            .map_err(|error| map_serialport_error("serial.set_control_lines", error))
    }

    async fn close(&mut self) -> HalResult<()> {
        loop {
            match self.state {
                SessionState::Ready(_) => self.begin_tracked_close()?,
                SessionState::Draining(_) | SessionState::Closing(_) => {
                    let drain_result = self.finish_tracked_drain().await?;
                    match drain_result {
                        Ok(()) => {}
                        Err(error) if error.name().as_str() == "runtime.transport.disconnected" => {
                        }
                        Err(error) => return Err(error),
                    }
                }
                SessionState::Closed => return Ok(()),
            }
        }
    }
}

fn open_serial_stream(
    endpoint: &str,
    config: &SerialConfig,
) -> HalResult<tokio_serial::SerialStream> {
    tokio_serial::new(endpoint, config.baud_rate)
        .data_bits(map_data_bits(config.data_bits))
        .parity(map_parity(config.parity))
        .stop_bits(map_stop_bits(config.stop_bits))
        .flow_control(map_flow_control(config.flow_control))
        .timeout(config.read_timeout)
        .open_native_async()
        .map_err(|error| {
            map_serialport_open_error(
                "serial.open",
                endpoint,
                error,
                capture_native_open_error(endpoint),
            )
        })
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
fn drain_join_failure(operation: &'static str) -> DrainTask {
    let join = tokio::task::spawn_blocking(|| -> DrainWorkerResult {
        panic!("intentional serial drain worker panic for test")
    });

    DrainTask { operation, join }
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
    async fn finish_tracked_drain_for_test(&mut self) -> HalResult<HalResult<()>> {
        self.finish_tracked_drain().await
    }

    #[cfg(test)]
    fn set_join_failure_for_test(&mut self, operation: &'static str) {
        self.state = SessionState::Closing(drain_join_failure(operation));
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

fn map_data_bits(data_bits: DataBits) -> tokio_serial::DataBits {
    match data_bits {
        DataBits::Five => tokio_serial::DataBits::Five,
        DataBits::Six => tokio_serial::DataBits::Six,
        DataBits::Seven => tokio_serial::DataBits::Seven,
        DataBits::Eight => tokio_serial::DataBits::Eight,
    }
}

fn map_parity(parity: Parity) -> tokio_serial::Parity {
    match parity {
        Parity::None => tokio_serial::Parity::None,
        Parity::Odd => tokio_serial::Parity::Odd,
        Parity::Even => tokio_serial::Parity::Even,
    }
}

fn map_stop_bits(stop_bits: StopBits) -> tokio_serial::StopBits {
    match stop_bits {
        StopBits::One => tokio_serial::StopBits::One,
        StopBits::Two => tokio_serial::StopBits::Two,
    }
}

fn map_flow_control(flow_control: FlowControl) -> tokio_serial::FlowControl {
    match flow_control {
        FlowControl::None => tokio_serial::FlowControl::None,
        FlowControl::Software => tokio_serial::FlowControl::Software,
        FlowControl::Hardware => tokio_serial::FlowControl::Hardware,
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
        let (_master, slave) = tokio_serial::SerialStream::pair().unwrap();
        let mut session = test_session(slave, Arc::new(BlockingDrainStrategy));

        session.close().await.unwrap();
        let error = session.read(0).await.unwrap_err();

        assert_eq!(error.name().as_str(), "runtime.session.closed");
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn dropping_flush_future_leaves_tracked_drain_and_recovers_session() {
        let (_master, slave) = tokio_serial::SerialStream::pair().unwrap();
        let gate = Arc::new(DrainGate::new());
        let mut session = test_session(
            slave,
            Arc::new(GatedDrainStrategy {
                gate: Arc::clone(&gate),
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
        let (_master, slave) = tokio_serial::SerialStream::pair().unwrap();
        let gate = Arc::new(DrainGate::new());
        let mut session = test_session(
            slave,
            Arc::new(GatedDrainStrategy {
                gate: Arc::clone(&gate),
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
    #[tokio::test]
    async fn drain_join_failure_closes_session_deterministically() {
        let (_master, slave) = tokio_serial::SerialStream::pair().unwrap();
        let mut session = test_session(slave, Arc::new(BlockingDrainStrategy));
        session.set_join_failure_for_test("serial.close");

        let error = session.finish_tracked_drain_for_test().await.unwrap_err();

        assert_eq!(error.name().as_str(), "runtime.internal");
        assert!(session.is_closed_for_test());
    }

    #[test]
    fn maps_serial_config_to_tokio_serial_types() {
        assert_eq!(map_data_bits(DataBits::Five), tokio_serial::DataBits::Five);
        assert_eq!(map_data_bits(DataBits::Six), tokio_serial::DataBits::Six);
        assert_eq!(
            map_data_bits(DataBits::Seven),
            tokio_serial::DataBits::Seven
        );
        assert_eq!(
            map_data_bits(DataBits::Eight),
            tokio_serial::DataBits::Eight
        );
        assert_eq!(map_parity(Parity::None), tokio_serial::Parity::None);
        assert_eq!(map_parity(Parity::Odd), tokio_serial::Parity::Odd);
        assert_eq!(map_parity(Parity::Even), tokio_serial::Parity::Even);
        assert_eq!(map_stop_bits(StopBits::One), tokio_serial::StopBits::One);
        assert_eq!(map_stop_bits(StopBits::Two), tokio_serial::StopBits::Two);
        assert_eq!(
            map_flow_control(FlowControl::None),
            tokio_serial::FlowControl::None
        );
        assert_eq!(
            map_flow_control(FlowControl::Software),
            tokio_serial::FlowControl::Software
        );
        assert_eq!(
            map_flow_control(FlowControl::Hardware),
            tokio_serial::FlowControl::Hardware
        );
    }

    fn test_session(
        stream: tokio_serial::SerialStream,
        drain_strategy: Arc<dyn DrainStrategy>,
    ) -> NativeSerialSession {
        NativeSerialSession {
            descriptor: descriptor(),
            config: SerialConfig::default(),
            state: SessionState::Ready(stream),
            drain_strategy,
        }
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
    }

    impl DrainGate {
        fn new() -> Self {
            Self {
                entered: AtomicBool::new(false),
                released: AtomicBool::new(false),
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
    }

    struct GatedDrainStrategy {
        gate: Arc<DrainGate>,
    }

    impl DrainStrategy for GatedDrainStrategy {
        fn spawn(
            &self,
            operation: &'static str,
            mut stream: tokio_serial::SerialStream,
        ) -> DrainTask {
            let gate = Arc::clone(&self.gate);
            let join = tokio::task::spawn_blocking(move || {
                gate.entered.store(true, Ordering::Release);
                gate.wait_for_release();
                let result = if operation == "serial.close" {
                    Ok(())
                } else {
                    std::io::Write::flush(&mut stream)
                }
                .map_err(|error| map_io_error(operation, error));
                Ok((stream, result))
            });

            DrainTask { operation, join }
        }
    }
}
