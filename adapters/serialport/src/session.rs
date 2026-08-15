use std::io;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::thread;
use std::time::{Duration, Instant};

#[cfg(test)]
use std::sync::Condvar;

use async_trait::async_trait;
use bytes::Bytes;
use seeed_hal_core::{ErrorCategory, HalError, HalResult, ResourceDescriptor};
use seeed_hal_serial::{
    ControlLines, DataBits, FlowControl, Parity, SerialConfig, SerialSession, StopBits,
};
use tokio::sync::{oneshot, watch};

use crate::{internal, invalid_argument, map_io_error, session_closed, timeout};

const COMMAND_QUEUE_CAPACITY: usize = 1;
const IDLE_CLOSE_POLL: Duration = Duration::from_millis(5);
const FLUSH_POLL_INTERVAL: Duration = Duration::from_millis(5);
const MAX_NATIVE_IO_TIMEOUT: Duration = Duration::from_millis(100);
const LIFECYCLE_OPEN: u8 = 0;
const LIFECYCLE_CLOSING: u8 = 1;
const LIFECYCLE_CLOSED: u8 = 2;

type SerialStream = serial2::SerialPort;

trait SerialIo: Send {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize>;
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize>;
    fn set_read_timeout(&mut self, timeout: Duration) -> io::Result<()>;
    fn set_write_timeout(&mut self, timeout: Duration) -> io::Result<()>;
    fn set_control_lines(&mut self, lines: ControlLines) -> io::Result<()>;
    fn pending_output_bytes(&mut self) -> io::Result<u32>;
}

impl SerialIo for SerialStream {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        SerialStream::read(self, buffer)
    }

    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        SerialStream::write(self, bytes)
    }

    fn set_read_timeout(&mut self, timeout: Duration) -> io::Result<()> {
        SerialStream::set_read_timeout(self, timeout)
    }

    fn set_write_timeout(&mut self, timeout: Duration) -> io::Result<()> {
        SerialStream::set_write_timeout(self, timeout)
    }

    fn set_control_lines(&mut self, lines: ControlLines) -> io::Result<()> {
        self.set_dtr(lines.data_terminal_ready)?;
        self.set_rts(lines.request_to_send)
    }

    fn pending_output_bytes(&mut self) -> io::Result<u32> {
        native_pending_output_bytes(self)
    }
}

#[cfg(unix)]
fn native_pending_output_bytes(stream: &SerialStream) -> io::Result<u32> {
    use std::os::fd::AsRawFd;

    let mut pending: libc::c_int = 0;
    // This is the same TIOCOUTQ query used by serialport 4.9's TTYPort::bytes_to_write.
    // SAFETY: `stream` owns a live serial file descriptor for the duration of the call,
    // `TIOCOUTQ` only writes one `c_int`, and `pending` is a valid aligned output pointer.
    let result = unsafe {
        libc::ioctl(
            stream.as_raw_fd(),
            libc::TIOCOUTQ as _,
            std::ptr::addr_of_mut!(pending),
        )
    };
    if result == -1 {
        return Err(io::Error::last_os_error());
    }
    pending.try_into().map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            "serial output queue reported a negative byte count",
        )
    })
}

#[cfg(windows)]
fn native_pending_output_bytes(stream: &SerialStream) -> io::Result<u32> {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Devices::Communication::{COMSTAT, ClearCommError};

    let mut errors = 0;
    let mut status = COMSTAT::default();
    // This is the same ClearCommError/COMSTAT query used by serialport 4.9's
    // COMPort::bytes_to_write.
    // SAFETY: `stream` owns a live serial handle for the duration of the call and both
    // output pointers refer to initialized, correctly sized Windows API structures.
    let succeeded = unsafe {
        ClearCommError(
            stream.as_raw_handle(),
            std::ptr::addr_of_mut!(errors),
            std::ptr::addr_of_mut!(status),
        )
    };
    if succeeded == 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(status.cbOutQue)
    }
}

enum WorkerCommand {
    Read {
        max_bytes: usize,
        reply: oneshot::Sender<HalResult<Bytes>>,
    },
    Write {
        bytes: Bytes,
        reply: oneshot::Sender<HalResult<()>>,
    },
    Flush {
        reply: oneshot::Sender<HalResult<()>>,
    },
    SetControlLines {
        lines: ControlLines,
        reply: oneshot::Sender<HalResult<()>>,
    },
}

impl WorkerCommand {
    fn reject_closed(self) {
        match self {
            Self::Read { reply, .. } => {
                let _ = reply.send(Err(session_closed(
                    "serial.read",
                    "serial session is closing",
                )));
            }
            Self::Write { reply, .. } => {
                let _ = reply.send(Err(session_closed(
                    "serial.write",
                    "serial session is closing",
                )));
            }
            Self::Flush { reply } => {
                let _ = reply.send(Err(session_closed(
                    "serial.flush",
                    "serial session is closing",
                )));
            }
            Self::SetControlLines { reply, .. } => {
                let _ = reply.send(Err(session_closed(
                    "serial.set_control_lines",
                    "serial session is closing",
                )));
            }
        }
    }
}

struct WorkerControlInner {
    lifecycle: AtomicU8,
    terminal_error: Mutex<Option<HalError>>,
    completion_tx: watch::Sender<Option<HalResult<()>>>,
}

#[derive(Clone)]
struct WorkerControl {
    inner: Arc<WorkerControlInner>,
}

impl WorkerControl {
    fn new() -> (Self, watch::Receiver<Option<HalResult<()>>>) {
        let (completion_tx, completion_rx) = watch::channel(None);
        (
            Self {
                inner: Arc::new(WorkerControlInner {
                    lifecycle: AtomicU8::new(LIFECYCLE_OPEN),
                    terminal_error: Mutex::new(None),
                    completion_tx,
                }),
            },
            completion_rx,
        )
    }

    fn is_open(&self) -> bool {
        self.inner.lifecycle.load(Ordering::Acquire) == LIFECYCLE_OPEN
    }

    fn request_close(&self) {
        let _ = self.inner.lifecycle.compare_exchange(
            LIFECYCLE_OPEN,
            LIFECYCLE_CLOSING,
            Ordering::AcqRel,
            Ordering::Acquire,
        );
    }

    fn record_terminal_error<T>(&self, result: &HalResult<T>) {
        if self.is_open() {
            return;
        }
        let Err(error) = result else {
            return;
        };
        if matches!(
            error.name().as_str(),
            "runtime.session.closed" | "runtime.transport.disconnected"
        ) {
            return;
        }

        let mut terminal_error = lock_unpoisoned(&self.inner.terminal_error);
        if terminal_error.is_none() {
            *terminal_error = Some(error.clone());
        }
    }

    fn take_terminal_error(&self) -> Option<HalError> {
        lock_unpoisoned(&self.inner.terminal_error).take()
    }

    fn finish_actor(&self, result: HalResult<()>) {
        self.inner
            .lifecycle
            .store(LIFECYCLE_CLOSED, Ordering::Release);
        let _ = self.inner.completion_tx.send(Some(result));
    }
}

struct OperationGuard {
    control: WorkerControl,
    completed: bool,
}

impl OperationGuard {
    fn new(control: WorkerControl) -> Self {
        Self {
            control,
            completed: false,
        }
    }

    fn complete(&mut self) {
        self.completed = true;
    }
}

impl Drop for OperationGuard {
    fn drop(&mut self) {
        if !self.completed {
            self.control.request_close();
        }
    }
}

pub(crate) struct NativeSerialSession {
    descriptor: ResourceDescriptor,
    command_tx: mpsc::SyncSender<WorkerCommand>,
    control: WorkerControl,
    completion_rx: watch::Receiver<Option<HalResult<()>>>,
}

impl NativeSerialSession {
    pub(crate) async fn open(
        descriptor: ResourceDescriptor,
        config: SerialConfig,
    ) -> HalResult<Self> {
        validate_config(&config)?;
        let endpoint = descriptor.endpoint().as_str().to_owned();

        run_blocking_open(move || {
            let stream = open_serial_stream(&endpoint, &config)?;
            spawn_session_worker(descriptor, config, Box::new(stream))
        })
        .await
    }

    fn ensure_open(&self, operation: &'static str) -> HalResult<()> {
        if self.control.is_open() {
            Ok(())
        } else {
            Err(session_closed(
                operation,
                "serial session is closing or already closed",
            ))
        }
    }

    fn enqueue(&self, command: WorkerCommand, operation: &'static str) -> HalResult<()> {
        self.command_tx
            .try_send(command)
            .map_err(|error| match error {
                mpsc::TrySendError::Full(_) => queue_full(
                    operation,
                    "the serial adapter worker queue has reached its one-command capacity",
                ),
                mpsc::TrySendError::Disconnected(_) => session_closed(
                    operation,
                    "the serial adapter worker is no longer available",
                ),
            })
    }

    async fn wait_closed(&mut self) -> HalResult<()> {
        loop {
            if let Some(result) = self.completion_rx.borrow().clone() {
                return result;
            }
            if self.completion_rx.changed().await.is_err() {
                return Err(internal(
                    "serial.close",
                    "serial worker exited without publishing terminal cleanup",
                ));
            }
        }
    }

    #[cfg(test)]
    fn from_io_for_test(
        descriptor: ResourceDescriptor,
        config: SerialConfig,
        io: Box<dyn SerialIo>,
    ) -> HalResult<Self> {
        spawn_session_worker(descriptor, config, io)
    }
}

impl Drop for NativeSerialSession {
    fn drop(&mut self) {
        self.control.request_close();
    }
}

#[async_trait]
impl SerialSession for NativeSerialSession {
    fn descriptor(&self) -> &ResourceDescriptor {
        &self.descriptor
    }

    async fn read(&mut self, max_bytes: usize) -> HalResult<Bytes> {
        self.ensure_open("serial.read")?;
        if max_bytes == 0 {
            return Err(invalid_argument(
                "serial.read",
                "max_bytes must be greater than zero",
            ));
        }

        let (reply, response) = oneshot::channel();
        self.enqueue(WorkerCommand::Read { max_bytes, reply }, "serial.read")?;
        let mut guard = OperationGuard::new(self.control.clone());
        let result = response.await.map_err(|_| {
            internal(
                "serial.read",
                "serial adapter worker dropped the read response",
            )
        });
        if result.is_ok() {
            guard.complete();
        }
        result?
    }

    async fn write_all(&mut self, bytes: &[u8]) -> HalResult<()> {
        self.ensure_open("serial.write")?;
        if bytes.is_empty() {
            return Ok(());
        }

        let (reply, response) = oneshot::channel();
        self.enqueue(
            WorkerCommand::Write {
                bytes: Bytes::copy_from_slice(bytes),
                reply,
            },
            "serial.write",
        )?;
        let mut guard = OperationGuard::new(self.control.clone());
        let result = response.await.map_err(|_| {
            internal(
                "serial.write",
                "serial adapter worker dropped the write response",
            )
        });
        if result.is_ok() {
            guard.complete();
        }
        result?
    }

    async fn flush(&mut self) -> HalResult<()> {
        self.ensure_open("serial.flush")?;
        let (reply, response) = oneshot::channel();
        self.enqueue(WorkerCommand::Flush { reply }, "serial.flush")?;
        let mut guard = OperationGuard::new(self.control.clone());
        let result = response.await.map_err(|_| {
            internal(
                "serial.flush",
                "serial adapter worker dropped the flush response",
            )
        });
        if result.is_ok() {
            guard.complete();
        }
        result?
    }

    async fn set_control_lines(&mut self, lines: ControlLines) -> HalResult<()> {
        self.ensure_open("serial.set_control_lines")?;
        let (reply, response) = oneshot::channel();
        self.enqueue(
            WorkerCommand::SetControlLines { lines, reply },
            "serial.set_control_lines",
        )?;
        let mut guard = OperationGuard::new(self.control.clone());
        let result = response.await.map_err(|_| {
            internal(
                "serial.set_control_lines",
                "serial adapter worker dropped the control-line response",
            )
        });
        if result.is_ok() {
            guard.complete();
        }
        result?
    }

    async fn close(&mut self) -> HalResult<()> {
        self.control.request_close();
        self.wait_closed().await
    }
}

fn spawn_session_worker(
    descriptor: ResourceDescriptor,
    config: SerialConfig,
    io: Box<dyn SerialIo>,
) -> HalResult<NativeSerialSession> {
    let (command_tx, command_rx) = mpsc::sync_channel(COMMAND_QUEUE_CAPACITY);
    let (control, completion_rx) = WorkerControl::new();

    let actor_control = control.clone();
    if let Err(error) = thread::Builder::new()
        .name("seeed-hal-serial-worker".to_owned())
        .spawn(move || run_actor_guarded(io, command_rx, actor_control, config.read_timeout))
    {
        control.request_close();
        return Err(internal(
            "serial.open",
            format!("failed to start serial worker: {error}"),
        ));
    }

    Ok(NativeSerialSession {
        descriptor,
        command_tx,
        control,
        completion_rx,
    })
}

fn run_actor_guarded(
    io: Box<dyn SerialIo>,
    commands: mpsc::Receiver<WorkerCommand>,
    control: WorkerControl,
    operation_timeout: Duration,
) {
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        run_actor(io, commands, &control, operation_timeout)
    }));
    let result = match result {
        Ok(()) => control.take_terminal_error().map_or(Ok(()), Err),
        Err(_) => Err(internal("serial.close", "serial worker panicked")),
    };
    control.request_close();
    control.finish_actor(result);
}

fn run_actor(
    mut io: Box<dyn SerialIo>,
    commands: mpsc::Receiver<WorkerCommand>,
    control: &WorkerControl,
    operation_timeout: Duration,
) {
    while control.is_open() {
        let command = match commands.recv_timeout(IDLE_CLOSE_POLL) {
            Ok(command) => command,
            Err(mpsc::RecvTimeoutError::Timeout) => continue,
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                control.request_close();
                break;
            }
        };

        if !control.is_open() {
            command.reject_closed();
            break;
        }
        execute_command(io.as_mut(), command, control, operation_timeout);
    }

    while let Ok(command) = commands.try_recv() {
        command.reject_closed();
    }
    drop(io);
}

fn execute_command(
    io: &mut dyn SerialIo,
    command: WorkerCommand,
    control: &WorkerControl,
    operation_timeout: Duration,
) {
    match command {
        WorkerCommand::Read { max_bytes, reply } => {
            let deadline = operation_deadline(operation_timeout);
            let result = read_bounded(io, max_bytes, deadline, &control.inner.lifecycle);
            control.record_terminal_error(&result);
            let _ = reply.send(result);
        }
        WorkerCommand::Write { bytes, reply } => {
            let deadline = operation_deadline(operation_timeout);
            let result = write_all_bounded(io, &bytes, deadline, &control.inner.lifecycle);
            control.record_terminal_error(&result);
            let _ = reply.send(result);
        }
        WorkerCommand::Flush { reply } => {
            let deadline = operation_deadline(operation_timeout);
            let result = flush_bounded(io, deadline, &control.inner.lifecycle);
            if matches!(
                result.as_ref().err().map(|error| error.name().as_str()),
                Some("runtime.transport.timeout")
            ) {
                control.request_close();
            }
            control.record_terminal_error(&result);
            let _ = reply.send(result);
        }
        WorkerCommand::SetControlLines { lines, reply } => {
            let result = io
                .set_control_lines(lines)
                .map_err(|error| map_io_error("serial.set_control_lines", error));
            control.record_terminal_error(&result);
            let _ = reply.send(result);
        }
    }
}

fn write_all_bounded(
    io: &mut dyn SerialIo,
    bytes: &[u8],
    deadline: Instant,
    lifecycle: &AtomicU8,
) -> HalResult<()> {
    write_all_bounded_with_clock(io, bytes, deadline, lifecycle, Instant::now)
}

fn flush_bounded(io: &mut dyn SerialIo, deadline: Instant, lifecycle: &AtomicU8) -> HalResult<()> {
    flush_bounded_with_clock_and_wait(io, deadline, lifecycle, Instant::now, thread::sleep)
}

fn flush_bounded_with_clock_and_wait(
    io: &mut dyn SerialIo,
    deadline: Instant,
    lifecycle: &AtomicU8,
    mut now: impl FnMut() -> Instant,
    mut wait: impl FnMut(Duration),
) -> HalResult<()> {
    loop {
        if lifecycle.load(Ordering::Acquire) != LIFECYCLE_OPEN {
            return Err(session_closed("serial.flush", "serial session is closing"));
        }

        let Some(remaining) = deadline.checked_duration_since(now()) else {
            return Err(timeout(
                "serial.flush",
                "flush exceeded its configured operation deadline",
            ));
        };
        if remaining.is_zero() {
            return Err(timeout(
                "serial.flush",
                "flush exceeded its configured operation deadline",
            ));
        }

        let pending = io
            .pending_output_bytes()
            .map_err(|error| map_io_error("serial.flush", error))?;
        if pending == 0 {
            return Ok(());
        }

        wait(remaining.min(FLUSH_POLL_INTERVAL));
    }
}

fn read_bounded(
    io: &mut dyn SerialIo,
    max_bytes: usize,
    deadline: Instant,
    lifecycle: &AtomicU8,
) -> HalResult<Bytes> {
    read_bounded_with_clock(io, max_bytes, deadline, lifecycle, Instant::now)
}

fn read_bounded_with_clock(
    io: &mut dyn SerialIo,
    max_bytes: usize,
    deadline: Instant,
    lifecycle: &AtomicU8,
    mut now: impl FnMut() -> Instant,
) -> HalResult<Bytes> {
    loop {
        if lifecycle.load(Ordering::Acquire) != LIFECYCLE_OPEN {
            return Err(session_closed("serial.read", "serial session is closing"));
        }

        let Some(remaining) = deadline.checked_duration_since(now()) else {
            return Err(timeout(
                "serial.read",
                "read exceeded its configured operation deadline",
            ));
        };
        if remaining.is_zero() {
            return Err(timeout(
                "serial.read",
                "read exceeded its configured operation deadline",
            ));
        }

        io.set_read_timeout(normalize_native_timeout(remaining))
            .map_err(|error| map_io_error("serial.read", error))?;
        let mut buffer = vec![0_u8; max_bytes];
        match io.read(&mut buffer) {
            Ok(0) => {
                return Err(disconnected(
                    "serial.read",
                    "serial port returned end of stream",
                ));
            }
            Ok(read) => {
                buffer.truncate(read);
                return Ok(Bytes::from(buffer));
            }
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::Interrupted | io::ErrorKind::TimedOut
                ) => {}
            Err(error) => return Err(map_io_error("serial.read", error)),
        }
    }
}

fn write_all_bounded_with_clock(
    io: &mut dyn SerialIo,
    mut bytes: &[u8],
    deadline: Instant,
    lifecycle: &AtomicU8,
    mut now: impl FnMut() -> Instant,
) -> HalResult<()> {
    while !bytes.is_empty() {
        if lifecycle.load(Ordering::Acquire) != LIFECYCLE_OPEN {
            return Err(session_closed("serial.write", "serial session is closing"));
        }

        let Some(remaining) = deadline.checked_duration_since(now()) else {
            return Err(timeout(
                "serial.write",
                "write exceeded its configured operation deadline",
            ));
        };
        if remaining.is_zero() {
            return Err(timeout(
                "serial.write",
                "write exceeded its configured operation deadline",
            ));
        }

        io.set_write_timeout(normalize_native_timeout(remaining))
            .map_err(|error| map_io_error("serial.write", error))?;
        match io.write(bytes) {
            Ok(0) => {
                return Err(disconnected(
                    "serial.write",
                    "serial port made no progress while writing",
                ));
            }
            Ok(written) => bytes = &bytes[written..],
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::Interrupted | io::ErrorKind::TimedOut
                ) => {}
            Err(error) => return Err(map_io_error("serial.write", error)),
        }
    }
    Ok(())
}

async fn run_blocking_open<T, F>(open: F) -> HalResult<T>
where
    T: Send + 'static,
    F: FnOnce() -> HalResult<T> + Send + 'static,
{
    tokio::task::spawn_blocking(open).await.map_err(|error| {
        internal(
            "serial.open",
            format!("serial open blocking worker failed: {error}"),
        )
    })?
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
        .set_read_timeout(normalize_native_timeout(config.read_timeout))
        .map_err(|error| map_io_error("serial.open", error))?;
    stream
        .set_write_timeout(normalize_native_timeout(config.read_timeout))
        .map_err(|error| map_io_error("serial.open", error))?;

    Ok(stream)
}

fn operation_deadline(timeout: Duration) -> Instant {
    Instant::now()
        .checked_add(timeout)
        .expect("validated serial operation timeout must fit in Instant")
}

fn normalize_native_timeout(timeout: Duration) -> Duration {
    let rounded_millis = timeout
        .as_millis()
        .saturating_add(u128::from(timeout.subsec_nanos() % 1_000_000 != 0));
    let capped_millis = rounded_millis.clamp(1, MAX_NATIVE_IO_TIMEOUT.as_millis());
    Duration::from_millis(capped_millis as u64)
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

    if Instant::now().checked_add(config.read_timeout).is_none() {
        return Err(invalid_argument(
            "serial.open",
            "read_timeout is too large for the platform clock",
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

fn queue_full(operation: &'static str, debug_message: impl Into<String>) -> HalError {
    HalError::new(
        "runtime.queue.full",
        ErrorCategory::Unavailable,
        operation,
        true,
        debug_message,
    )
    .expect("static serialport adapter error metadata must be valid")
}

fn disconnected(operation: &'static str, debug_message: impl Into<String>) -> HalError {
    HalError::new(
        "runtime.transport.disconnected",
        ErrorCategory::Unavailable,
        operation,
        true,
        debug_message,
    )
    .expect("static serialport adapter error metadata must be valid")
}

fn lock_unpoisoned<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

#[cfg(test)]
fn wait_unpoisoned<'a, T>(
    condvar: &Condvar,
    guard: std::sync::MutexGuard<'a, T>,
) -> std::sync::MutexGuard<'a, T> {
    condvar
        .wait(guard)
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicBool, AtomicUsize};

    use seeed_hal_core::{
        Endpoint, IdentityQuality, ResourceId, ResourceProperties, TransportKind,
    };

    use super::*;

    #[tokio::test(flavor = "multi_thread", worker_threads = 1)]
    async fn open_and_configure_run_outside_tokio_worker() {
        let caller = thread::current().id();
        let opened_on = run_blocking_open(move || Ok(thread::current().id()))
            .await
            .unwrap();

        assert_ne!(opened_on, caller);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn cancelled_open_future_drops_late_opened_port() {
        let gate = Arc::new(Gate::default());
        let dropped = Arc::new(AtomicBool::new(false));
        let open_gate = Arc::clone(&gate);
        let open_dropped = Arc::clone(&dropped);
        let mut open = Box::pin(run_blocking_open(move || {
            open_gate.enter_and_wait();
            Ok(DropProbe(open_dropped))
        }));

        tokio::select! {
            result = &mut open => panic!("open should remain blocked, got {result:?}"),
            () = gate.wait_until_entered() => {}
        }
        drop(open);
        gate.release();

        wait_until(
            || dropped.load(Ordering::Acquire),
            "late opened port was not dropped",
        )
        .await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn bounded_flush_session_does_not_require_an_interrupt_clone() {
        let probe = Arc::new(IoProbe::new(BlockedOperation::None));
        let mut session = test_actor_session(Arc::clone(&probe));

        assert_eq!(probe.handles.load(Ordering::Acquire), 1);

        session.close().await.unwrap();

        probe.wait_until_all_handles_dropped().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn cancelled_read_closes_worker_before_next_access() {
        let probe = Arc::new(IoProbe::new(BlockedOperation::Read));
        let mut session = test_actor_session(Arc::clone(&probe));
        let mut read = Box::pin(session.read(8));

        tokio::select! {
            result = &mut read => panic!("read should remain blocked, got {result:?}"),
            () = probe.wait_until_operation_entered() => {}
        }
        drop(read);
        probe.release_operation();
        probe.wait_until_all_handles_dropped().await;

        let error = session.write_all(b"later").await.unwrap_err();
        assert_eq!(error.name().as_str(), "runtime.session.closed");
        assert_eq!(probe.write_calls.load(Ordering::Acquire), 0);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn cancelled_write_all_closes_worker_before_next_access() {
        let probe = Arc::new(IoProbe::new(BlockedOperation::Write));
        let mut session = test_actor_session(Arc::clone(&probe));
        let mut write = Box::pin(session.write_all(b"blocked"));

        tokio::select! {
            result = &mut write => panic!("write should remain blocked, got {result:?}"),
            () = probe.wait_until_operation_entered() => {}
        }
        drop(write);
        probe.release_operation();
        probe.wait_until_all_handles_dropped().await;

        let error = session.read(1).await.unwrap_err();
        assert_eq!(error.name().as_str(), "runtime.session.closed");
        assert_eq!(probe.max_active.load(Ordering::Acquire), 1);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn cancelled_flush_releases_port_without_later_session_poll() {
        let probe = Arc::new(IoProbe::new_with_pending_output());
        let mut session = test_actor_session(Arc::clone(&probe));
        let mut flush = Box::pin(session.flush());

        tokio::select! {
            result = &mut flush => panic!("flush should remain blocked, got {result:?}"),
            () = probe.wait_until_output_queried() => {}
        }
        drop(flush);

        probe.wait_until_all_handles_dropped().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn dropping_polled_close_future_still_releases_port_autonomously() {
        let probe = Arc::new(IoProbe::new(BlockedOperation::None));
        let mut session = test_actor_session(Arc::clone(&probe));
        let mut close = Box::pin(session.close());

        tokio::select! {
            _ = &mut close => {}
            () = tokio::task::yield_now() => {}
        }
        drop(close);

        probe.wait_until_all_handles_dropped().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn output_queue_poll_panic_releases_port_without_later_session_poll() {
        let probe = Arc::new(IoProbe::new_with_output_query_panic());
        let mut session = test_actor_session(Arc::clone(&probe));

        let error = session.flush().await.unwrap_err();

        assert_eq!(error.name().as_str(), "runtime.internal");
        probe.wait_until_all_handles_dropped().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn flush_timeout_returns_structured_error_and_releases_port() {
        let probe = Arc::new(IoProbe::new_with_pending_output());
        let mut session =
            test_actor_session_with_timeout(Arc::clone(&probe), Duration::from_nanos(1));

        let error = session.flush().await.unwrap_err();

        assert_eq!(error.name().as_str(), "runtime.transport.timeout");
        probe.wait_until_all_handles_dropped().await;
        let error = session.read(1).await.unwrap_err();
        assert_eq!(error.name().as_str(), "runtime.session.closed");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn close_reports_cancelled_flush_error_after_terminal_release() {
        let probe = Arc::new(IoProbe::new_with_flush_error());
        let mut session = test_actor_session(Arc::clone(&probe));
        let mut flush = Box::pin(session.flush());

        tokio::select! {
            result = &mut flush => panic!("flush should remain blocked, got {result:?}"),
            () = probe.wait_until_operation_entered() => {}
        }
        drop(flush);
        probe.release_operation();

        let error = session.close().await.unwrap_err();

        assert_eq!(error.name().as_str(), "runtime.transport.permission_denied");
        probe.wait_until_all_handles_dropped().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn normal_commands_are_serialized_on_one_owned_worker() {
        let probe = Arc::new(IoProbe::new(BlockedOperation::None));
        let mut session = test_actor_session(Arc::clone(&probe));

        session.write_all(b"abc").await.unwrap();
        session.flush().await.unwrap();
        session
            .set_control_lines(ControlLines::default())
            .await
            .unwrap();
        session.close().await.unwrap();

        assert_eq!(probe.max_active.load(Ordering::Acquire), 1);
        assert_eq!(probe.worker_threads.lock().unwrap().len(), 1);
    }

    #[test]
    fn write_all_stops_at_operation_deadline() {
        let probe = Arc::new(IoProbe::new(BlockedOperation::None));
        let mut io = TestIo::new(Arc::clone(&probe));
        let lifecycle = AtomicU8::new(LIFECYCLE_OPEN);

        let error =
            write_all_bounded(&mut io, b"never admitted", Instant::now(), &lifecycle).unwrap_err();

        assert_eq!(error.name().as_str(), "runtime.transport.timeout");
        assert_eq!(probe.write_calls.load(Ordering::Acquire), 0);
    }

    #[test]
    fn write_all_deadline_terminates_repeated_partial_writes() {
        let probe = Arc::new(IoProbe::new(BlockedOperation::None));
        let mut io = TestIo::new(Arc::clone(&probe));
        let lifecycle = AtomicU8::new(LIFECYCLE_OPEN);
        let started = Instant::now();
        let mut ticks = [
            started,
            started + Duration::from_millis(1),
            started + Duration::from_millis(2),
            started + Duration::from_millis(11),
        ]
        .into_iter();

        let error = write_all_bounded_with_clock(
            &mut io,
            b"more than three bytes",
            started + Duration::from_millis(10),
            &lifecycle,
            || ticks.next().unwrap(),
        )
        .unwrap_err();

        assert_eq!(error.name().as_str(), "runtime.transport.timeout");
        assert_eq!(probe.write_calls.load(Ordering::Acquire), 3);
    }

    #[test]
    fn flush_succeeds_only_after_the_native_output_queue_is_empty() {
        let started = Instant::now();
        let lifecycle = AtomicU8::new(LIFECYCLE_OPEN);
        let mut io = OutputQueueIo::new([Ok(2), Ok(1), Ok(0)]);
        let mut ticks = [
            started,
            started + Duration::from_millis(1),
            started + Duration::from_millis(2),
        ]
        .into_iter();
        let mut waits = Vec::new();

        flush_bounded_with_clock_and_wait(
            &mut io,
            started + Duration::from_secs(1),
            &lifecycle,
            || ticks.next().unwrap(),
            |duration| waits.push(duration),
        )
        .unwrap();

        assert_eq!(io.queries, 3);
        assert_eq!(waits.len(), 2);
        assert!(waits.iter().all(|duration| !duration.is_zero()));
    }

    #[test]
    fn flush_polling_stops_at_the_logical_deadline_without_native_flush() {
        let started = Instant::now();
        let lifecycle = AtomicU8::new(LIFECYCLE_OPEN);
        let mut io = OutputQueueIo::new([Ok(1)]);
        let mut ticks = [started, started + Duration::from_millis(11)].into_iter();

        let error = flush_bounded_with_clock_and_wait(
            &mut io,
            started + Duration::from_millis(10),
            &lifecycle,
            || ticks.next().unwrap(),
            |_| {},
        )
        .unwrap_err();

        assert_eq!(error.name().as_str(), "runtime.transport.timeout");
        assert_eq!(io.queries, 1);
    }

    #[test]
    fn native_timeout_rounds_every_positive_sub_millisecond_value_up() {
        assert_eq!(
            normalize_native_timeout(Duration::from_nanos(1)),
            Duration::from_millis(1)
        );
        assert_eq!(
            normalize_native_timeout(Duration::from_micros(999)),
            Duration::from_millis(1)
        );
        assert_eq!(
            normalize_native_timeout(Duration::from_micros(1_001)),
            Duration::from_millis(2)
        );
    }

    #[test]
    fn native_timeout_clamps_extreme_values_before_platform_conversion() {
        assert_eq!(
            normalize_native_timeout(Duration::MAX),
            Duration::from_millis(100)
        );
    }

    #[test]
    fn read_rounds_sub_millisecond_remainder_to_nonzero_native_timeout() {
        let started = Instant::now();
        let lifecycle = AtomicU8::new(LIFECYCLE_OPEN);
        let mut io = NativeTimeoutIo::with_reads([Ok(1)]);
        let mut ticks = [started].into_iter();

        let bytes = read_bounded_with_clock(
            &mut io,
            1,
            started + Duration::from_micros(500),
            &lifecycle,
            || ticks.next().unwrap(),
        )
        .unwrap();

        assert_eq!(bytes.as_ref(), b"x");
        assert_eq!(io.read_timeouts, [Duration::from_millis(1)]);
    }

    #[test]
    fn read_retries_native_slices_until_logical_deadline() {
        let started = Instant::now();
        let lifecycle = AtomicU8::new(LIFECYCLE_OPEN);
        let mut io = NativeTimeoutIo::with_reads([
            Err(io::ErrorKind::TimedOut.into()),
            Err(io::ErrorKind::TimedOut.into()),
        ]);
        let mut ticks = [
            started,
            started + Duration::from_millis(99),
            started + Duration::from_millis(151),
        ]
        .into_iter();

        let error = read_bounded_with_clock(
            &mut io,
            1,
            started + Duration::from_millis(150),
            &lifecycle,
            || ticks.next().unwrap(),
        )
        .unwrap_err();

        assert_eq!(error.name().as_str(), "runtime.transport.timeout");
        assert_eq!(
            io.read_timeouts,
            [Duration::from_millis(100), Duration::from_millis(51)]
        );
    }

    #[test]
    fn write_rounds_sub_millisecond_remainder_to_nonzero_native_timeout() {
        let started = Instant::now();
        let lifecycle = AtomicU8::new(LIFECYCLE_OPEN);
        let mut io = NativeTimeoutIo::with_writes([Ok(1)]);
        let mut ticks = [started].into_iter();

        write_all_bounded_with_clock(
            &mut io,
            b"x",
            started + Duration::from_micros(500),
            &lifecycle,
            || ticks.next().unwrap(),
        )
        .unwrap();

        assert_eq!(io.write_timeouts, [Duration::from_millis(1)]);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn closed_state_takes_precedence_over_invalid_read_size() {
        let probe = Arc::new(IoProbe::new(BlockedOperation::None));
        let mut session = test_actor_session(probe);

        session.close().await.unwrap();
        let error = session.read(0).await.unwrap_err();

        assert_eq!(error.name().as_str(), "runtime.session.closed");
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
        let expected_platform_code = expected_raw_os_error.to_string();

        let error = open_serial_stream(&endpoint, &SerialConfig::default()).unwrap_err();

        assert_eq!(error.name().as_str(), "runtime.resource.not_found");
        assert_eq!(error.platform_code(), Some(expected_platform_code.as_str()));
        assert!(error.debug_message().contains("io error kind=NotFound"));
        assert!(
            error
                .debug_message()
                .contains(&format!("raw_os_error={expected_raw_os_error}"))
        );
        assert!(!error.debug_message().contains("serialport error"));
        assert!(!error.debug_message().contains("native_open_error"));
    }

    fn test_actor_session(probe: Arc<IoProbe>) -> NativeSerialSession {
        test_actor_session_with_timeout(probe, Duration::from_secs(5))
    }

    fn test_actor_session_with_timeout(
        probe: Arc<IoProbe>,
        read_timeout: Duration,
    ) -> NativeSerialSession {
        NativeSerialSession::from_io_for_test(
            descriptor(),
            SerialConfig {
                read_timeout,
                ..SerialConfig::default()
            },
            Box::new(TestIo::new(probe)),
        )
        .unwrap()
    }

    fn descriptor() -> ResourceDescriptor {
        ResourceDescriptor::new(
            ResourceId::parse("serial:test:actor").unwrap(),
            Endpoint::new("test://actor").unwrap(),
            IdentityQuality::Weak,
            TransportKind::Serial,
            ResourceProperties::default(),
            seeed_hal_core::CapabilitySet::new(vec![seeed_hal_serial::serial_bytes_capability()]),
        )
    }

    async fn wait_until(mut predicate: impl FnMut() -> bool, message: &str) {
        tokio::time::timeout(Duration::from_secs(1), async {
            while !predicate() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect(message);
    }

    #[derive(Default)]
    struct Gate {
        state: Mutex<(bool, bool)>,
        changed: Condvar,
    }

    impl Gate {
        fn enter_and_wait(&self) {
            let mut state = lock_unpoisoned(&self.state);
            state.0 = true;
            self.changed.notify_all();
            while !state.1 {
                state = wait_unpoisoned(&self.changed, state);
            }
        }

        async fn wait_until_entered(&self) {
            wait_until(
                || lock_unpoisoned(&self.state).0,
                "blocking operation did not start",
            )
            .await;
        }

        fn release(&self) {
            let mut state = lock_unpoisoned(&self.state);
            state.1 = true;
            self.changed.notify_all();
        }
    }

    struct DropProbe(Arc<AtomicBool>);

    impl std::fmt::Debug for DropProbe {
        fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            formatter.debug_struct("DropProbe").finish()
        }
    }

    impl Drop for DropProbe {
        fn drop(&mut self) {
            self.0.store(true, Ordering::Release);
        }
    }

    #[derive(Clone, Copy, Eq, PartialEq)]
    enum BlockedOperation {
        None,
        Read,
        Write,
        Flush,
    }

    struct IoProbe {
        blocked: BlockedOperation,
        operation_gate: Gate,
        output_queries: AtomicUsize,
        pending_output_bytes: AtomicUsize,
        output_query_panics: AtomicBool,
        handles: AtomicUsize,
        max_active: AtomicUsize,
        active: AtomicUsize,
        write_calls: AtomicUsize,
        flush_error: bool,
        worker_threads: Mutex<std::collections::HashSet<thread::ThreadId>>,
    }

    impl IoProbe {
        fn new(blocked: BlockedOperation) -> Self {
            Self {
                blocked,
                operation_gate: Gate::default(),
                output_queries: AtomicUsize::new(0),
                pending_output_bytes: AtomicUsize::new(0),
                output_query_panics: AtomicBool::new(false),
                handles: AtomicUsize::new(0),
                max_active: AtomicUsize::new(0),
                active: AtomicUsize::new(0),
                write_calls: AtomicUsize::new(0),
                flush_error: false,
                worker_threads: Mutex::new(std::collections::HashSet::new()),
            }
        }

        fn new_with_flush_error() -> Self {
            Self {
                flush_error: true,
                ..Self::new(BlockedOperation::Flush)
            }
        }

        fn new_with_pending_output() -> Self {
            Self {
                pending_output_bytes: AtomicUsize::new(1),
                ..Self::new(BlockedOperation::None)
            }
        }

        fn new_with_output_query_panic() -> Self {
            Self {
                output_query_panics: AtomicBool::new(true),
                ..Self::new(BlockedOperation::None)
            }
        }

        fn enter(&self, operation: BlockedOperation) {
            lock_unpoisoned(&self.worker_threads).insert(thread::current().id());
            let active = self.active.fetch_add(1, Ordering::AcqRel) + 1;
            self.max_active.fetch_max(active, Ordering::AcqRel);
            if self.blocked != BlockedOperation::None && self.blocked == operation {
                self.operation_gate.enter_and_wait();
            }
            self.active.fetch_sub(1, Ordering::AcqRel);
        }

        async fn wait_until_operation_entered(&self) {
            self.operation_gate.wait_until_entered().await;
        }

        async fn wait_until_output_queried(&self) {
            wait_until(
                || self.output_queries.load(Ordering::Acquire) > 0,
                "serial output queue was not queried",
            )
            .await;
        }

        fn release_operation(&self) {
            self.operation_gate.release();
        }

        async fn wait_until_all_handles_dropped(&self) {
            wait_until(
                || self.handles.load(Ordering::Acquire) == 0,
                "worker retained a serial handle",
            )
            .await;
        }
    }

    struct TestIo {
        probe: Arc<IoProbe>,
    }

    impl TestIo {
        fn new(probe: Arc<IoProbe>) -> Self {
            probe.handles.fetch_add(1, Ordering::AcqRel);
            Self { probe }
        }
    }

    struct NativeTimeoutIo {
        reads: std::collections::VecDeque<io::Result<usize>>,
        writes: std::collections::VecDeque<io::Result<usize>>,
        read_timeouts: Vec<Duration>,
        write_timeouts: Vec<Duration>,
    }

    struct OutputQueueIo {
        pending: std::collections::VecDeque<io::Result<u32>>,
        queries: usize,
    }

    impl OutputQueueIo {
        fn new(pending: impl IntoIterator<Item = io::Result<u32>>) -> Self {
            Self {
                pending: pending.into_iter().collect(),
                queries: 0,
            }
        }
    }

    impl SerialIo for OutputQueueIo {
        fn read(&mut self, _buffer: &mut [u8]) -> io::Result<usize> {
            unreachable!("flush test must not read")
        }

        fn write(&mut self, _bytes: &[u8]) -> io::Result<usize> {
            unreachable!("flush test must not write")
        }

        fn set_read_timeout(&mut self, _timeout: Duration) -> io::Result<()> {
            Ok(())
        }

        fn set_write_timeout(&mut self, _timeout: Duration) -> io::Result<()> {
            Ok(())
        }

        fn set_control_lines(&mut self, _lines: ControlLines) -> io::Result<()> {
            Ok(())
        }

        fn pending_output_bytes(&mut self) -> io::Result<u32> {
            self.queries += 1;
            self.pending
                .pop_front()
                .expect("unexpected output queue query")
        }
    }

    impl NativeTimeoutIo {
        fn with_reads(reads: impl IntoIterator<Item = io::Result<usize>>) -> Self {
            Self {
                reads: reads.into_iter().collect(),
                writes: std::collections::VecDeque::new(),
                read_timeouts: Vec::new(),
                write_timeouts: Vec::new(),
            }
        }

        fn with_writes(writes: impl IntoIterator<Item = io::Result<usize>>) -> Self {
            Self {
                reads: std::collections::VecDeque::new(),
                writes: writes.into_iter().collect(),
                read_timeouts: Vec::new(),
                write_timeouts: Vec::new(),
            }
        }
    }

    impl SerialIo for NativeTimeoutIo {
        fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
            let result = self.reads.pop_front().expect("unexpected read call")?;
            if result > 0 {
                buffer[..result].fill(b'x');
            }
            Ok(result)
        }

        fn write(&mut self, _bytes: &[u8]) -> io::Result<usize> {
            self.writes.pop_front().expect("unexpected write call")
        }

        fn set_read_timeout(&mut self, timeout: Duration) -> io::Result<()> {
            self.read_timeouts.push(timeout);
            Ok(())
        }

        fn set_write_timeout(&mut self, timeout: Duration) -> io::Result<()> {
            self.write_timeouts.push(timeout);
            Ok(())
        }

        fn set_control_lines(&mut self, _lines: ControlLines) -> io::Result<()> {
            Ok(())
        }

        fn pending_output_bytes(&mut self) -> io::Result<u32> {
            Ok(0)
        }
    }

    impl Drop for TestIo {
        fn drop(&mut self) {
            self.probe.handles.fetch_sub(1, Ordering::AcqRel);
        }
    }

    impl SerialIo for TestIo {
        fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
            self.probe.enter(BlockedOperation::Read);
            buffer[0] = b'x';
            Ok(1)
        }

        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.probe.write_calls.fetch_add(1, Ordering::AcqRel);
            self.probe.enter(BlockedOperation::Write);
            Ok(bytes.len().min(1))
        }

        fn set_read_timeout(&mut self, _timeout: Duration) -> io::Result<()> {
            Ok(())
        }

        fn set_write_timeout(&mut self, _timeout: Duration) -> io::Result<()> {
            Ok(())
        }

        fn set_control_lines(&mut self, _lines: ControlLines) -> io::Result<()> {
            self.probe.enter(BlockedOperation::None);
            Ok(())
        }

        fn pending_output_bytes(&mut self) -> io::Result<u32> {
            self.probe.output_queries.fetch_add(1, Ordering::AcqRel);
            if self.probe.output_query_panics.load(Ordering::Acquire) {
                panic!("output queue query panic")
            }
            self.probe.enter(BlockedOperation::Flush);
            if self.probe.flush_error {
                Err(io::ErrorKind::PermissionDenied.into())
            } else {
                Ok(self.probe.pending_output_bytes.load(Ordering::Acquire) as u32)
            }
        }
    }
}
