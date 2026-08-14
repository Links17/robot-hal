use bytes::Bytes;
use seeed_hal_core::{
    ErrorCategory, HalError, HalResult, IdentityQuality, ResourceDescriptor, ResourceId,
    ResourceProperties, ResourceSelector, TransportKind,
};
use seeed_hal_serial::{
    ControlLines, DataBits, FlowControl, Parity, SerialAdapter, SerialConfig, SerialSession,
    StopBits,
};
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use tokio::sync::{Mutex, Notify, mpsc};

const QUEUE_CAPACITY: usize = 64;
const DEFAULT_ENDPOINT_PREFIX: &str = "virtual://serial/";

#[derive(Clone, Debug)]
pub struct VirtualSerialAdapter {
    descriptor: ResourceDescriptor,
}

impl VirtualSerialAdapter {
    pub fn loopback(resource_id: impl Into<String>) -> Self {
        let resource_id = ResourceId::parse(resource_id.into())
            .expect("loopback resource id must be a valid HAL resource id");
        let endpoint = format!("{DEFAULT_ENDPOINT_PREFIX}{}", resource_id.as_str());

        let mut properties = std::collections::BTreeMap::new();
        properties.insert("adapter".to_owned(), "virtual".to_owned());
        properties.insert("mode".to_owned(), "loopback".to_owned());

        Self {
            descriptor: resource_descriptor(
                resource_id,
                endpoint,
                IdentityQuality::Strong,
                ResourceProperties::new(properties),
            ),
        }
    }
}

#[async_trait::async_trait]
impl SerialAdapter for VirtualSerialAdapter {
    fn adapter_name(&self) -> &'static str {
        "virtual.serial.loopback"
    }

    async fn enumerate(&self) -> HalResult<Vec<ResourceDescriptor>> {
        Ok(vec![self.descriptor.clone()])
    }

    async fn open(
        &self,
        selector: &ResourceSelector,
        config: SerialConfig,
    ) -> HalResult<Box<dyn SerialSession>> {
        if selector != &self.descriptor.selector() {
            return Err(not_found(
                "serial.open",
                "selector did not match the loopback descriptor",
            ));
        }

        validate_config(&config)?;

        let (tx, rx) = mpsc::channel(QUEUE_CAPACITY);
        Ok(Box::new(VirtualSerialSession::new(
            self.descriptor.clone(),
            config,
            tx,
            rx,
        )))
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

    if config.data_bits != DataBits::Eight
        || config.parity != Parity::None
        || config.stop_bits != StopBits::One
        || config.flow_control != FlowControl::None
    {
        return Err(unsupported_configuration(
            "serial.open",
            "virtual loopback only accepts the default framing configuration",
        ));
    }

    Ok(())
}

struct VirtualSerialSession {
    descriptor: ResourceDescriptor,
    config: SerialConfig,
    closed: AtomicBool,
    close_notify: Notify,
    sender: mpsc::Sender<Bytes>,
    receiver: Mutex<mpsc::Receiver<Bytes>>,
    pending: Mutex<VecDeque<Bytes>>,
    control_lines: Mutex<ControlLines>,
}

impl VirtualSerialSession {
    fn new(
        descriptor: ResourceDescriptor,
        config: SerialConfig,
        sender: mpsc::Sender<Bytes>,
        receiver: mpsc::Receiver<Bytes>,
    ) -> Self {
        Self {
            descriptor,
            config,
            closed: AtomicBool::new(false),
            close_notify: Notify::new(),
            sender,
            receiver: Mutex::new(receiver),
            pending: Mutex::new(VecDeque::new()),
            control_lines: Mutex::new(ControlLines::default()),
        }
    }

    fn is_closed(&self) -> bool {
        self.closed.load(Ordering::Acquire)
    }

    fn closed_error(&self, operation: &'static str) -> HalError {
        session_closed(operation, "serial session is already closed")
    }

    fn timeout_error(&self, operation: &'static str) -> HalError {
        timeout(
            operation,
            format!(
                "no bytes were available within {:?}",
                self.config.read_timeout
            ),
        )
    }

    fn queue_full_error(&self, operation: &'static str) -> HalError {
        queue_full(
            operation,
            "the bounded virtual receive queue has reached capacity",
        )
    }

    async fn next_bytes(&self) -> HalResult<Bytes> {
        if let Some(bytes) = self.pending.lock().await.pop_front() {
            return Ok(bytes);
        }

        let mut receiver = self.receiver.lock().await;
        let timeout = tokio::time::sleep(self.config.read_timeout);
        tokio::pin!(timeout);

        tokio::select! {
            _ = self.close_notify.notified() => Err(self.closed_error("serial.read")),
            maybe = receiver.recv() => match maybe {
                Some(bytes) => Ok(bytes),
                None => Err(self.closed_error("serial.read")),
            },
            _ = &mut timeout => Err(self.timeout_error("serial.read")),
        }
    }

    async fn split_and_store_remainder(&self, bytes: Bytes, max_bytes: usize) -> Bytes {
        if bytes.len() <= max_bytes {
            return bytes;
        }

        let head = bytes.slice(..max_bytes);
        let tail = bytes.slice(max_bytes..);
        self.pending.lock().await.push_front(tail);
        head
    }
}

#[async_trait::async_trait]
impl SerialSession for VirtualSerialSession {
    fn descriptor(&self) -> &ResourceDescriptor {
        &self.descriptor
    }

    async fn read(&mut self, max_bytes: usize) -> HalResult<Bytes> {
        if self.is_closed() {
            return Err(self.closed_error("serial.read"));
        }

        if max_bytes == 0 {
            return Err(invalid_argument(
                "serial.read",
                "max_bytes must be greater than zero",
            ));
        }

        let bytes = self.next_bytes().await?;

        if self.is_closed() {
            return Err(self.closed_error("serial.read"));
        }

        Ok(self.split_and_store_remainder(bytes, max_bytes).await)
    }

    async fn write_all(&mut self, bytes: &[u8]) -> HalResult<()> {
        if self.is_closed() {
            return Err(self.closed_error("serial.write"));
        }

        if bytes.is_empty() {
            return Ok(());
        }

        match self.sender.try_send(Bytes::copy_from_slice(bytes)) {
            Ok(()) => Ok(()),
            Err(mpsc::error::TrySendError::Full(_)) => Err(self.queue_full_error("serial.write")),
            Err(mpsc::error::TrySendError::Closed(_)) => Err(self.closed_error("serial.write")),
        }
    }

    async fn flush(&mut self) -> HalResult<()> {
        if self.is_closed() {
            return Err(self.closed_error("serial.flush"));
        }

        Ok(())
    }

    async fn set_control_lines(&mut self, lines: ControlLines) -> HalResult<()> {
        if self.is_closed() {
            return Err(self.closed_error("serial.set_control_lines"));
        }

        *self.control_lines.lock().await = lines;
        Ok(())
    }

    async fn close(&mut self) -> HalResult<()> {
        if self.closed.swap(true, Ordering::AcqRel) {
            return Ok(());
        }

        self.close_notify.notify_waiters();
        Ok(())
    }
}

fn invalid_argument(operation: &'static str, debug_message: impl Into<String>) -> HalError {
    HalError::new(
        "runtime.argument.invalid",
        ErrorCategory::InvalidArgument,
        operation,
        false,
        debug_message,
    )
    .expect("static invalid argument error metadata must be valid")
}

fn not_found(operation: &'static str, debug_message: impl Into<String>) -> HalError {
    HalError::new(
        "runtime.resource.not_found",
        ErrorCategory::NotFound,
        operation,
        false,
        debug_message,
    )
    .expect("static not found error metadata must be valid")
}

fn unsupported_configuration(
    operation: &'static str,
    debug_message: impl Into<String>,
) -> HalError {
    HalError::new(
        "runtime.transport.unsupported_configuration",
        ErrorCategory::InvalidArgument,
        operation,
        false,
        debug_message,
    )
    .expect("static unsupported configuration error metadata must be valid")
}

fn timeout(operation: &'static str, debug_message: impl Into<String>) -> HalError {
    HalError::new(
        "runtime.transport.timeout",
        ErrorCategory::Unavailable,
        operation,
        true,
        debug_message,
    )
    .expect("static timeout error metadata must be valid")
}

fn queue_full(operation: &'static str, debug_message: impl Into<String>) -> HalError {
    HalError::new(
        "runtime.queue.full",
        ErrorCategory::Unavailable,
        operation,
        true,
        debug_message,
    )
    .expect("static queue full error metadata must be valid")
}

fn session_closed(operation: &'static str, debug_message: impl Into<String>) -> HalError {
    HalError::new(
        "runtime.session.closed",
        ErrorCategory::Conflict,
        operation,
        false,
        debug_message,
    )
    .expect("static session closed error metadata must be valid")
}

fn resource_descriptor(
    id: ResourceId,
    endpoint: impl Into<String>,
    identity_quality: IdentityQuality,
    properties: ResourceProperties,
) -> ResourceDescriptor {
    ResourceDescriptor::new(
        id,
        seeed_hal_core::Endpoint::new(endpoint).expect("loopback endpoint must be valid"),
        identity_quality,
        TransportKind::Serial,
        properties,
    )
}
