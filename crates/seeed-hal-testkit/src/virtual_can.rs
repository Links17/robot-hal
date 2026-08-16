use async_trait::async_trait;
use bytes::Bytes;
use seeed_hal_can::{
    can_classic_capability, can_configure_capability, can_error_frames_capability,
    can_fd_capability, can_rx_timestamp_capability, CanActiveConfig, CanAdapter, CanBitTiming,
    CanBusState, CanBusStatus, CanChannel, CanFrame, CanId, CanLinkExpectation, CanMode,
    CanOpenConfig, CanTimestamp, ReceivedCanFrame,
};
use seeed_hal_core::{
    CapabilitySet, ErrorCategory, HalError, HalResult, IdentityQuality, ResourceDescriptor,
    ResourceId, ResourceProperties, ResourceSelector, TransportKind, resolve_resource,
};
use std::collections::VecDeque;
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

const RX_CAPACITY: usize = 256;
const TX_CAPACITY: usize = 64;
const CLOCK_DOMAIN: &str = "virtual-can";

#[derive(Clone, Debug)]
pub struct VirtualCanAdapter {
    descriptor: ResourceDescriptor,
    state: Arc<SharedState>,
}

#[derive(Debug)]
struct SharedState {
    inner: Mutex<BusInner>,
    changed: Condvar,
}

#[derive(Debug)]
struct BusInner {
    rx: VecDeque<ReceivedCanFrame>,
    tx: VecDeque<CanFrame>,
    active: CanActiveConfig,
    status: CanBusStatus,
    configure_open: bool,
    open_count: usize,
    close_count: usize,
    next_send: Option<HalError>,
    next_receive: Option<HalError>,
    next_status: Option<HalError>,
    next_close: Option<HalError>,
}

impl VirtualCanAdapter {
    /// Creates a bounded, deterministic loopback channel.
    pub fn loopback(resource_id: impl Into<String>) -> Self {
        let id = ResourceId::parse(resource_id.into()).expect("valid virtual CAN resource id");
        let endpoint = format!("virtual://can/{}", id.as_str());
        let descriptor = ResourceDescriptor::new(
            id,
            seeed_hal_core::Endpoint::new(endpoint).expect("valid virtual CAN endpoint"),
            IdentityQuality::Strong,
            TransportKind::Can,
            ResourceProperties::new(
                [("adapter".to_owned(), "virtual".to_owned()),
                 ("mode".to_owned(), "loopback".to_owned())]
                    .into_iter()
                    .collect(),
            ),
            CapabilitySet::new(vec![
                can_classic_capability(),
                can_fd_capability(),
                can_configure_capability(),
                can_error_frames_capability(),
                can_rx_timestamp_capability(),
            ]),
        );
        let nominal = CanBitTiming::new(500_000, None, None)
            .expect("virtual CAN default timing is valid");
        let active = CanActiveConfig::new(
            CanMode::Classic,
            nominal,
            None,
            false,
            true,
            CLOCK_DOMAIN,
        )
        .expect("virtual CAN default configuration is valid");
        Self {
            descriptor,
            state: Arc::new(SharedState {
                inner: Mutex::new(BusInner {
                    rx: VecDeque::with_capacity(RX_CAPACITY),
                    tx: VecDeque::with_capacity(TX_CAPACITY),
                    active,
                    status: CanBusStatus::new(CanBusState::Active, Some(0), Some(0)),
                    configure_open: false,
                    open_count: 0,
                    close_count: 0,
                    next_send: None,
                    next_receive: None,
                    next_status: None,
                    next_close: None,
                }),
                changed: Condvar::new(),
            }),
        }
    }

    pub fn descriptor(&self) -> &ResourceDescriptor { &self.descriptor }

    /// Injects one received frame. A full RX queue drops the oldest frame.
    pub fn inject_received(&self, frame: CanFrame, timestamp: Option<CanTimestamp>) -> HalResult<()> {
        let mut inner = self.state.inner.lock().expect("virtual CAN mutex poisoned");
        if inner.rx.len() == RX_CAPACITY { inner.rx.pop_front(); }
        inner.rx.push_back(ReceivedCanFrame::new(frame, timestamp));
        self.state.changed.notify_all();
        Ok(())
    }

    pub fn transmitted_frames(&self) -> Vec<CanFrame> {
        self.state.inner.lock().expect("virtual CAN mutex poisoned").tx.iter().cloned().collect()
    }

    pub fn take_transmitted_frames(&self) -> Vec<CanFrame> {
        self.state.inner.lock().expect("virtual CAN mutex poisoned").tx.drain(..).collect()
    }

    pub fn set_bus_status(&self, status: CanBusStatus) {
        self.state.inner.lock().expect("virtual CAN mutex poisoned").status = status;
        self.state.changed.notify_all();
    }

    pub fn fail_next_send(&self, error: HalError) { self.set_failure(|s| s.next_send = Some(error)); }
    pub fn fail_next_receive(&self, error: HalError) { self.set_failure(|s| s.next_receive = Some(error)); }
    pub fn fail_next_status(&self, error: HalError) { self.set_failure(|s| s.next_status = Some(error)); }
    pub fn fail_next_close(&self, error: HalError) { self.set_failure(|s| s.next_close = Some(error)); }

    fn set_failure(&self, set: impl FnOnce(&mut BusInner)) {
        let mut inner = self.state.inner.lock().expect("virtual CAN mutex poisoned");
        set(&mut inner);
        self.state.changed.notify_all();
    }

    pub fn wait_for_open(&self, timeout: Duration) -> bool { self.wait_transition(timeout, true) }
    pub fn wait_for_close(&self, timeout: Duration) -> bool { self.wait_transition(timeout, false) }

    fn wait_transition(&self, timeout: Duration, open: bool) -> bool {
        let deadline = Instant::now() + timeout;
        let mut guard = self.state.inner.lock().expect("virtual CAN mutex poisoned");
        loop {
            let current = if open { guard.open_count } else { guard.close_count };
            if current > 0 { return true; }
            let Some(remaining) = deadline.checked_duration_since(Instant::now()) else { return false; };
            let (next, result) = self.state.changed.wait_timeout(guard, remaining).expect("virtual CAN mutex poisoned");
            guard = next;
            if result.timed_out() { return false; }
        }
    }
}

#[async_trait]
impl CanAdapter for VirtualCanAdapter {
    fn adapter_name(&self) -> &'static str { "virtual.can.loopback" }

    async fn enumerate(&self) -> HalResult<Vec<ResourceDescriptor>> { Ok(vec![self.descriptor.clone()]) }

    async fn open(&self, selector: &ResourceSelector, config: &CanOpenConfig) -> HalResult<Box<dyn CanChannel>> {
        let descriptor = resolve_resource(
            std::slice::from_ref(&self.descriptor), selector, &can_classic_capability(), "can.open",
        )?.clone();
        let mut inner = self.state.inner.lock().expect("virtual CAN mutex poisoned");
        let (active, restore) = match config {
            CanOpenConfig::Attach(expectation) => {
                verify_expectation(expectation, &inner.active, &descriptor)?;
                (inner.active.clone(), None)
            }
            CanOpenConfig::Configure(request) => {
                if inner.configure_open {
                    return Err(conflict("can.open", "another Configure channel is open", &descriptor));
                }
                let requested = CanActiveConfig::new(
                    request.mode(), *request.nominal(), request.data().copied(),
                    request.listen_only(), request.loopback(), CLOCK_DOMAIN,
                ).map_err(|e| e.with_resource_id(descriptor.id().clone()))?;
                let old = inner.active.clone();
                inner.active = requested.clone();
                inner.configure_open = true;
                (requested, Some(old))
            }
        };
        inner.open_count = inner.open_count.saturating_add(1);
        self.state.changed.notify_all();
        drop(inner);
        Ok(Box::new(VirtualCanChannel {
            descriptor, state: Arc::clone(&self.state), active, restore, closed: false,
        }))
    }
}

struct VirtualCanChannel {
    descriptor: ResourceDescriptor,
    state: Arc<SharedState>,
    active: CanActiveConfig,
    restore: Option<CanActiveConfig>,
    closed: bool,
}

impl CanChannel for VirtualCanChannel {
    fn descriptor(&self) -> &ResourceDescriptor { &self.descriptor }
    fn active_config(&self) -> &CanActiveConfig { &self.active }

    fn receive(&mut self, timeout: Duration) -> HalResult<Option<ReceivedCanFrame>> {
        if self.closed { return Err(closed("can.receive", &self.descriptor)); }
        let deadline = Instant::now() + timeout;
        let mut guard = self.state.inner.lock().expect("virtual CAN mutex poisoned");
        if let Some(error) = guard.next_receive.take() { return Err(error.with_resource_id(self.descriptor.id().clone())); }
        loop {
            if let Some(frame) = guard.rx.pop_front() { return Ok(Some(frame)); }
            if timeout.is_zero() { return Ok(None); }
            let Some(remaining) = deadline.checked_duration_since(Instant::now()) else { return Ok(None); };
            let (next, result) = self.state.changed.wait_timeout(guard, remaining).expect("virtual CAN mutex poisoned");
            guard = next;
            if result.timed_out() { return Ok(None); }
        }
    }

    fn send(&mut self, frame: &CanFrame) -> HalResult<()> {
        if self.closed { return Err(closed("can.send", &self.descriptor)); }
        let mut guard = self.state.inner.lock().expect("virtual CAN mutex poisoned");
        if let Some(error) = guard.next_send.take() { return Err(error.with_resource_id(self.descriptor.id().clone())); }
        if guard.tx.len() >= TX_CAPACITY { return Err(queue_full("can.send", &self.descriptor)); }
        guard.tx.push_back(frame.clone());
        if guard.rx.len() == RX_CAPACITY { guard.rx.pop_front(); }
        let timestamp = CanTimestamp::new(0, seeed_hal_can::CanTimestampSource::HostMonotonic, CLOCK_DOMAIN)
            .expect("virtual timestamp is valid");
        guard.rx.push_back(ReceivedCanFrame::new(frame.clone(), Some(timestamp)));
        self.state.changed.notify_all();
        Ok(())
    }

    fn bus_status(&mut self) -> HalResult<CanBusStatus> {
        if self.closed { return Err(closed("can.status", &self.descriptor)); }
        let mut guard = self.state.inner.lock().expect("virtual CAN mutex poisoned");
        if let Some(error) = guard.next_status.take() { return Err(error.with_resource_id(self.descriptor.id().clone())); }
        Ok(guard.status.clone())
    }

    fn close(&mut self) -> HalResult<()> {
        if self.closed { return Ok(()); }
        let mut guard = self.state.inner.lock().expect("virtual CAN mutex poisoned");
        let failure = guard.next_close.take().map(|error| error.with_resource_id(self.descriptor.id().clone()));
        if let Some(snapshot) = self.restore.take() {
            guard.active = snapshot;
            guard.configure_open = false;
        }
        self.closed = true;
        guard.close_count = guard.close_count.saturating_add(1);
        self.state.changed.notify_all();
        failure.map_or(Ok(()), Err)
    }
}

fn verify_expectation(expectation: &CanLinkExpectation, active: &CanActiveConfig, descriptor: &ResourceDescriptor) -> HalResult<()> {
    let mismatch = expectation.mode().is_some_and(|v| v != active.mode())
        || expectation.nominal_bitrate().is_some_and(|v| v != active.nominal().bitrate())
        || expectation.data_bitrate().is_some_and(|v| active.data().is_none_or(|d| d.bitrate() != v))
        || expectation.listen_only().is_some_and(|v| v != active.listen_only())
        || expectation.loopback().is_some_and(|v| v != active.loopback());
    if mismatch { return Err(HalError::new("can.configuration.mismatch", ErrorCategory::Conflict, "can.open", false, "Attach expectations do not match active configuration").expect("valid error").with_resource_id(descriptor.id().clone())); }
    Ok(())
}

fn conflict(operation: &'static str, message: &'static str, descriptor: &ResourceDescriptor) -> HalError {
    HalError::new("runtime.adapter.conflict", ErrorCategory::Conflict, operation, false, message).expect("valid error").with_resource_id(descriptor.id().clone())
}
fn queue_full(operation: &'static str, descriptor: &ResourceDescriptor) -> HalError {
    HalError::new("runtime.queue.full", ErrorCategory::Unavailable, operation, true, "virtual CAN transmit queue is full").expect("valid error").with_resource_id(descriptor.id().clone())
}
fn closed(operation: &'static str, descriptor: &ResourceDescriptor) -> HalError {
    HalError::new("runtime.session.closed", ErrorCategory::Conflict, operation, false, "CAN channel is closed").expect("valid error").with_resource_id(descriptor.id().clone())
}

/// Basic reusable adapter checks. Physical adapters may call this helper for the
/// capabilities they advertise; it intentionally uses only public interfaces.
pub async fn run_can_adapter_conformance<A: CanAdapter>(adapter: &A) -> HalResult<()> {
    let descriptors = adapter.enumerate().await?;
    assert!(!descriptors.is_empty());
    let selector = descriptors[0].selector();
    let expectation = CanLinkExpectation::new(Some(CanMode::Classic), Some(500_000), None, Some(false), Some(true))?;
    let mut channel = adapter.open(&selector, &CanOpenConfig::Attach(expectation)).await?;
    let id = CanId::standard(0x123)?;
    let frame = CanFrame::classic_data(id, Bytes::from_static(&[1, 2, 3]))?;
    channel.send(&frame)?;
    let received = channel.receive(Duration::from_millis(20))?.expect("loopback frame");
    assert_eq!(received.frame(), &frame);
    assert_eq!(channel.bus_status()?.state(), CanBusState::Active);
    channel.close()
}
