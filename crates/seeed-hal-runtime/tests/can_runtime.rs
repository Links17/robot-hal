use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use bytes::Bytes;
use seeed_hal_can::{
    CanActiveConfig, CanAdapter, CanBitTiming, CanBusState, CanBusStatus, CanChannel,
    CanConfigureConfig, CanFilter, CanFilterSet, CanFrame, CanFrameClasses, CanId, CanIdFormat,
    CanLinkExpectation, CanMode, CanOpenConfig, ReceivedCanFrame, can_classic_capability,
};
use seeed_hal_core::{
    CapabilitySet, Endpoint, ErrorCategory, HalError, HalResult, IdentityQuality, LeaseMode,
    OwnerId, ResourceDescriptor, ResourceId, ResourceProperties, ResourceSelector, TransportKind,
    resolve_resource,
};
use seeed_hal_runtime::HalRuntime;
use seeed_hal_testkit::VirtualCanAdapter;

fn owner(name: &str) -> OwnerId {
    OwnerId::parse(name).unwrap()
}

fn attach() -> CanOpenConfig {
    CanOpenConfig::Attach(CanLinkExpectation::new(None, None, None, None, None).unwrap())
}

fn all_frames() -> CanFilterSet {
    CanFilterSet::new(Vec::new()).unwrap()
}

fn frame(id: u16) -> CanFrame {
    CanFrame::classic_data(CanId::standard(id).unwrap(), Bytes::from(vec![id as u8])).unwrap()
}

async fn open(
    runtime: &HalRuntime,
    adapter: &VirtualCanAdapter,
    owner_id: &str,
    mode: LeaseMode,
) -> seeed_hal_runtime::CanHandle {
    runtime
        .open_can(
            owner(owner_id),
            adapter.descriptor().selector(),
            mode,
            attach(),
            all_frames(),
        )
        .await
        .unwrap()
}

#[tokio::test]
async fn multiple_observers_receive_independent_fanout() {
    let adapter = VirtualCanAdapter::loopback("can:runtime:fanout");
    let runtime = HalRuntime::builder().can_adapter(adapter.clone()).build();
    let first = open(&runtime, &adapter, "first", LeaseMode::Observe).await;
    let second = open(&runtime, &adapter, "second", LeaseMode::Observe).await;
    adapter.inject_received(frame(1), None).unwrap();
    assert_eq!(first.receive(1, Duration::from_millis(100)).await.unwrap()[0].frame(), &frame(1));
    assert_eq!(second.receive(1, Duration::from_millis(100)).await.unwrap()[0].frame(), &frame(1));
}

#[tokio::test]
async fn exactly_one_controller_can_share_with_observers() {
    let adapter = VirtualCanAdapter::loopback("can:runtime:controller");
    let runtime = HalRuntime::builder().can_adapter(adapter.clone()).build();
    let _observer = open(&runtime, &adapter, "observer", LeaseMode::Observe).await;
    let _controller = open(&runtime, &adapter, "controller", LeaseMode::Control).await;
    let error = runtime
        .open_can(owner("other"), adapter.descriptor().selector(), LeaseMode::Control, attach(), all_frames())
        .await
        .err()
        .unwrap();
    assert_eq!(error.name().as_str(), "runtime.lease.conflict");
}

#[tokio::test]
async fn maintenance_is_exclusive_and_restores_access_after_close() {
    let adapter = VirtualCanAdapter::loopback("can:runtime:maintenance");
    let runtime = HalRuntime::builder().can_adapter(adapter.clone()).build();
    let configured = CanOpenConfig::Configure(CanConfigureConfig::new(
        CanMode::Classic,
        CanBitTiming::new(250_000, None, None).unwrap(),
        None,
        false,
        false,
    ).unwrap());
    let mut maintenance = runtime.open_can(
        owner("maintenance"),
        adapter.descriptor().selector(),
        LeaseMode::Maintenance,
        configured,
        all_frames(),
    ).await.unwrap();
    assert!(runtime.open_can(owner("blocked"), adapter.descriptor().selector(), LeaseMode::Observe, attach(), all_frames()).await.is_err());
    maintenance.close().await.unwrap();
    let _restored = open(&runtime, &adapter, "restored", LeaseMode::Observe).await;
}

#[tokio::test]
async fn filter_replacement_is_session_local_and_atomic() {
    let adapter = VirtualCanAdapter::loopback("can:runtime:filters");
    let runtime = HalRuntime::builder().can_adapter(adapter.clone()).build();
    let first = open(&runtime, &adapter, "first-filter", LeaseMode::Observe).await;
    let second = open(&runtime, &adapter, "second-filter", LeaseMode::Observe).await;
    let only_two = CanFilterSet::new(vec![CanFilter::new(
        2,
        0x7ff,
        CanIdFormat::Standard,
        CanFrameClasses::data_only(),
    ).unwrap()]).unwrap();
    first.replace_filters(only_two).await.unwrap();
    adapter.inject_received(frame(1), None).unwrap();
    adapter.inject_received(frame(2), None).unwrap();
    assert_eq!(first.receive(2, Duration::from_millis(100)).await.unwrap().len(), 1);
    assert_eq!(second.receive(2, Duration::from_millis(100)).await.unwrap().len(), 2);
}

#[tokio::test]
async fn rx_overflow_drops_oldest_and_reports_lag_once() {
    let adapter = VirtualCanAdapter::loopback("can:runtime:rx-lag");
    let runtime = HalRuntime::builder()
        .can_adapter(adapter.clone())
        .can_rx_capacity(2)
        .build();
    let observer = open(&runtime, &adapter, "lagged", LeaseMode::Observe).await;
    for id in 1..=3 {
        adapter.inject_received(frame(id), None).unwrap();
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    let lag = observer.receive(2, Duration::ZERO).await.unwrap_err();
    assert_eq!(lag.name().as_str(), "can.receive.lagged");
    let retained = observer.receive(2, Duration::ZERO).await.unwrap();
    assert_eq!(retained.iter().map(|item| item.frame().id().unwrap().value()).collect::<Vec<_>>(), vec![2, 3]);
}

#[tokio::test]
async fn tx_batch_admission_is_atomic_against_frame_capacity() {
    let adapter = VirtualCanAdapter::loopback("can:runtime:tx-admission");
    let runtime = HalRuntime::builder()
        .can_adapter(adapter.clone())
        .can_tx_capacity(2)
        .build();
    let control = open(&runtime, &adapter, "tx", LeaseMode::Control).await;
    let error = control.send_batch(vec![frame(1), frame(2), frame(3)]).await.unwrap_err();
    assert_eq!(error.error().name().as_str(), "runtime.queue.full");
    assert_eq!(error.committed(), 0);
    assert!(adapter.transmitted_frames().is_empty());
}

#[tokio::test]
async fn backend_failure_reports_exact_partial_send_count() {
    let adapter = ScriptedAdapter::new("can:runtime:partial");
    adapter.state.lock().unwrap().fail_send_at = Some(2);
    let runtime = HalRuntime::builder().can_adapter(adapter.clone()).build();
    let control = runtime.open_can(owner("partial"), adapter.selector(), LeaseMode::Control, attach(), all_frames()).await.unwrap();
    let error = control.send_batch(vec![frame(1), frame(2), frame(3)]).await.unwrap_err();
    assert_eq!(error.committed(), 2);
    assert_eq!(adapter.state.lock().unwrap().sent.len(), 2);
}

#[tokio::test]
async fn concurrent_batch_commands_preserve_actor_fifo() {
    let adapter = ScriptedAdapter::new("can:runtime:fifo");
    let runtime = HalRuntime::builder().can_adapter(adapter.clone()).build();
    let control = runtime.open_can(owner("fifo"), adapter.selector(), LeaseMode::Control, attach(), all_frames()).await.unwrap();
    control.send_batch(vec![frame(1), frame(2)]).await.unwrap();
    control.send_batch(vec![frame(3), frame(4)]).await.unwrap();
    assert_eq!(adapter.state.lock().unwrap().sent.iter().map(|item| item.id().unwrap().value()).collect::<Vec<_>>(), vec![1, 2, 3, 4]);
}

#[tokio::test]
async fn receive_timeout_returns_empty_within_a_finite_deadline() {
    let adapter = VirtualCanAdapter::loopback("can:runtime:timeout");
    let runtime = HalRuntime::builder().can_adapter(adapter.clone()).build();
    let observer = open(&runtime, &adapter, "timeout", LeaseMode::Observe).await;
    let started = Instant::now();
    assert!(observer.receive(1, Duration::from_millis(10)).await.unwrap().is_empty());
    assert!(started.elapsed() < Duration::from_secs(1));
}

#[tokio::test]
async fn cancelled_receive_does_not_consume_a_later_frame() {
    let adapter = VirtualCanAdapter::loopback("can:runtime:cancel");
    let runtime = HalRuntime::builder().can_adapter(adapter.clone()).build();
    let observer = open(&runtime, &adapter, "cancel", LeaseMode::Observe).await;
    let session = observer.session_id();
    let token = observer.lease_token().clone();
    let receive_runtime = runtime.clone();
    let receive_token = token.clone();
    let cancelled = tokio::spawn(async move {
        receive_runtime.receive_can(session, &receive_token, 1, Duration::from_secs(30)).await
    });
    tokio::time::sleep(Duration::from_millis(10)).await;
    cancelled.abort();
    let _ = cancelled.await;
    tokio::time::sleep(Duration::from_millis(5)).await;
    adapter.inject_received(frame(7), None).unwrap();
    assert_eq!(runtime.receive_can(observer.session_id(), &token, 1, Duration::from_millis(100)).await.unwrap()[0].frame(), &frame(7));
}

#[tokio::test]
async fn stale_can_lease_is_fenced_after_resource_reuse() {
    let adapter = VirtualCanAdapter::loopback("can:runtime:fencing");
    let runtime = HalRuntime::builder().can_adapter(adapter.clone()).build();
    let old = open(&runtime, &adapter, "old", LeaseMode::Control).await;
    let old_session = old.session_id();
    let old_token = old.lease_token().clone();
    let (session, token) = old.into_parts();
    runtime.close_can(session, &token).await.unwrap();
    let new = open(&runtime, &adapter, "new", LeaseMode::Control).await;
    let error = runtime.send_can(new.session_id(), &old_token, frame(1)).await.unwrap_err();
    assert_eq!(error.error().name().as_str(), "runtime.lease.stale_generation");
    assert_eq!(runtime.send_can(old_session, &old_token, frame(1)).await.unwrap_err().error().name().as_str(), "runtime.session.closed");
}

#[tokio::test]
async fn owner_revoke_closes_all_can_sessions() {
    let first = VirtualCanAdapter::loopback("can:runtime:revoke-a");
    let second = VirtualCanAdapter::loopback("can:runtime:revoke-b");
    let runtime = HalRuntime::builder().can_adapter(first.clone()).can_adapter(second.clone()).build();
    let owner_id = owner("revoked");
    let first_handle = runtime.open_can(owner_id.clone(), first.descriptor().selector(), LeaseMode::Observe, attach(), all_frames()).await.unwrap();
    let second_handle = runtime.open_can(owner_id.clone(), second.descriptor().selector(), LeaseMode::Control, attach(), all_frames()).await.unwrap();
    runtime.revoke_owner(&owner_id).await.unwrap();
    assert_eq!(first_handle.receive(1, Duration::ZERO).await.unwrap_err().name().as_str(), "runtime.session.closed");
    assert_eq!(second_handle.send(frame(1)).await.unwrap_err().error().name().as_str(), "runtime.session.closed");
}

#[tokio::test]
async fn actor_panic_disconnect_is_reported_without_hanging() {
    let adapter = ScriptedAdapter::new("can:runtime:panic");
    adapter.state.lock().unwrap().panic_receive = true;
    let runtime = HalRuntime::builder().can_adapter(adapter.clone()).build();
    let observer = runtime.open_can(owner("panic"), adapter.selector(), LeaseMode::Observe, attach(), all_frames()).await.unwrap();
    tokio::time::sleep(Duration::from_millis(10)).await;
    let error = observer.receive(1, Duration::from_millis(20)).await.unwrap_err();
    assert_eq!(error.name().as_str(), "runtime.actor.unavailable");
}

#[tokio::test]
async fn close_timeout_is_finite_and_structured() {
    let adapter = ScriptedAdapter::new("can:runtime:close-timeout");
    adapter.state.lock().unwrap().close_delay = Duration::from_millis(100);
    let runtime = HalRuntime::builder().can_adapter(adapter.clone()).can_close_timeout(Duration::from_millis(10)).build();
    let mut observer = runtime.open_can(owner("slow-close"), adapter.selector(), LeaseMode::Observe, attach(), all_frames()).await.unwrap();
    let started = Instant::now();
    let error = observer.close().await.unwrap_err();
    assert_eq!(error.name().as_str(), "runtime.session.close_timeout");
    assert!(started.elapsed() < Duration::from_millis(80));
}

#[tokio::test]
async fn closed_physical_resource_can_be_reopened() {
    let adapter = VirtualCanAdapter::loopback("can:runtime:reuse");
    let runtime = HalRuntime::builder().can_adapter(adapter.clone()).build();
    let mut first = open(&runtime, &adapter, "reuse-first", LeaseMode::Observe).await;
    first.close().await.unwrap();
    let second = open(&runtime, &adapter, "reuse-second", LeaseMode::Observe).await;
    assert!(second.lease_token().generation() > first.lease_token().generation());
}

#[tokio::test]
async fn adapters_coexist_and_duplicate_identity_remains_ambiguous() {
    let first = VirtualCanAdapter::loopback("can:runtime:adapter-a");
    let second = VirtualCanAdapter::loopback("can:runtime:adapter-b");
    let duplicate = VirtualCanAdapter::loopback("can:runtime:adapter-a");
    let runtime = HalRuntime::builder().can_adapter(first.clone()).can_adapter(second.clone()).can_adapter(duplicate).build();
    let descriptors = runtime.enumerate_can().await.unwrap();
    assert_eq!(descriptors.iter().map(|item| item.id().as_str()).collect::<Vec<_>>(), vec!["can:runtime:adapter-a", "can:runtime:adapter-a", "can:runtime:adapter-b"]);
    let error = runtime.open_can(owner("ambiguous"), first.descriptor().selector(), LeaseMode::Observe, attach(), all_frames()).await.err().unwrap();
    assert_eq!(error.name().as_str(), "runtime.resource.ambiguous");
    let _second = runtime.open_can(owner("unique"), second.descriptor().selector(), LeaseMode::Observe, attach(), all_frames()).await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn backend_methods_never_run_on_tokio_workers() {
    let adapter = ScriptedAdapter::new("can:runtime:thread-affinity");
    let runtime = HalRuntime::builder().can_adapter(adapter.clone()).build();
    let mut control = runtime.open_can(owner("thread"), adapter.selector(), LeaseMode::Control, attach(), all_frames()).await.unwrap();
    control.send(frame(1)).await.unwrap();
    control.bus_status().await.unwrap();
    control.close().await.unwrap();
    let state = adapter.state.lock().unwrap();
    assert!(!state.thread_names.is_empty());
    assert!(state.thread_names.iter().all(|name| name.starts_with("seeed-hal-can-")));
}

#[derive(Clone)]
struct ScriptedAdapter {
    descriptor: ResourceDescriptor,
    state: Arc<Mutex<ScriptedState>>,
}

#[derive(Default)]
struct ScriptedState {
    received: VecDeque<ReceivedCanFrame>,
    sent: Vec<CanFrame>,
    send_attempts: usize,
    fail_send_at: Option<usize>,
    panic_receive: bool,
    close_delay: Duration,
    thread_names: Vec<String>,
}

impl ScriptedAdapter {
    fn new(resource_id: &str) -> Self {
        let id = ResourceId::parse(resource_id).unwrap();
        Self {
            descriptor: ResourceDescriptor::new(
                id,
                Endpoint::new(format!("scripted://{resource_id}")).unwrap(),
                IdentityQuality::Strong,
                TransportKind::Can,
                ResourceProperties::default(),
                CapabilitySet::new(vec![can_classic_capability()]),
            ),
            state: Arc::new(Mutex::new(ScriptedState::default())),
        }
    }

    fn selector(&self) -> ResourceSelector {
        self.descriptor.selector()
    }
}

#[async_trait]
impl CanAdapter for ScriptedAdapter {
    fn adapter_name(&self) -> &'static str {
        "test.scripted.can"
    }

    async fn enumerate(&self) -> HalResult<Vec<ResourceDescriptor>> {
        record_thread(&self.state);
        Ok(vec![self.descriptor.clone()])
    }

    async fn open(&self, selector: &ResourceSelector, _config: &CanOpenConfig) -> HalResult<Box<dyn CanChannel>> {
        record_thread(&self.state);
        let descriptor = resolve_resource(std::slice::from_ref(&self.descriptor), selector, &can_classic_capability(), "can.open")?.clone();
        let active = CanActiveConfig::new(CanMode::Classic, CanBitTiming::new(500_000, None, None)?, None, false, false, "scripted").unwrap();
        Ok(Box::new(ScriptedChannel { descriptor, active, state: Arc::clone(&self.state) }))
    }
}

struct ScriptedChannel {
    descriptor: ResourceDescriptor,
    active: CanActiveConfig,
    state: Arc<Mutex<ScriptedState>>,
}

impl CanChannel for ScriptedChannel {
    fn descriptor(&self) -> &ResourceDescriptor {
        &self.descriptor
    }

    fn active_config(&self) -> &CanActiveConfig {
        &self.active
    }

    fn receive(&mut self, timeout: Duration) -> HalResult<Option<ReceivedCanFrame>> {
        record_thread(&self.state);
        let mut state = self.state.lock().unwrap();
        assert!(!state.panic_receive, "injected receive panic");
        let frame = state.received.pop_front();
        drop(state);
        if frame.is_none() {
            std::thread::sleep(timeout);
        }
        Ok(frame)
    }

    fn send(&mut self, frame: &CanFrame) -> HalResult<()> {
        record_thread(&self.state);
        let mut state = self.state.lock().unwrap();
        let attempt = state.send_attempts;
        state.send_attempts += 1;
        if state.fail_send_at == Some(attempt) {
            state.fail_send_at = None;
            return Err(HalError::new("can.bus.off", ErrorCategory::Unavailable, "can.send", false, "injected send failure")?.with_resource_id(self.descriptor.id().clone()));
        }
        state.sent.push(frame.clone());
        Ok(())
    }

    fn bus_status(&mut self) -> HalResult<CanBusStatus> {
        record_thread(&self.state);
        Ok(CanBusStatus::new(CanBusState::Active, None, None))
    }

    fn close(&mut self) -> HalResult<()> {
        record_thread(&self.state);
        let delay = self.state.lock().unwrap().close_delay;
        std::thread::sleep(delay);
        Ok(())
    }
}

fn record_thread(state: &Arc<Mutex<ScriptedState>>) {
    state.lock().unwrap().thread_names.push(
        std::thread::current().name().unwrap_or("unnamed").to_owned(),
    );
}
