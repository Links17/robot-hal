use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
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
use seeed_hal_runtime::{HalRuntime, RuntimeEventKind};
use seeed_hal_serial::{ControlLines, SerialAdapter, SerialConfig, SerialSession};
use seeed_hal_testkit::{VirtualCanAdapter, VirtualSerialAdapter};

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

async fn wait_for_can_session_closed(handle: &seeed_hal_runtime::CanHandle) {
    tokio::time::timeout(Duration::from_secs(1), async {
        loop {
            match handle.receive(1, Duration::ZERO).await {
                Err(error) if error.name().as_str() == "runtime.session.closed" => return,
                Err(error) if error.name().as_str() == "runtime.actor.unavailable" => {
                    tokio::task::yield_now().await;
                }
                result => panic!("expected failed actor reconciliation, got {result:?}"),
            }
        }
    })
    .await
    .expect("failed CAN actor must be reconciled within the test deadline");
}

async fn enqueue_blocked_filter_command(
    runtime: &HalRuntime,
    handle: &seeed_hal_runtime::CanHandle,
) -> tokio::task::JoinHandle<HalResult<()>> {
    let runtime = runtime.clone();
    let session = handle.session_id();
    let token = handle.lease_token().clone();
    let mut operation = Box::pin(async move {
        runtime
            .replace_can_filters(session, &token, all_frames())
            .await
    });
    tokio::select! {
        biased;
        result = &mut operation => panic!("blocked CAN actor completed filter command unexpectedly: {result:?}"),
        _ = tokio::task::yield_now() => {}
    }
    tokio::spawn(operation)
}

async fn enqueue_blocked_receive(
    runtime: &HalRuntime,
    handle: &seeed_hal_runtime::CanHandle,
    timeout: Duration,
    attempts: Arc<AtomicUsize>,
) -> tokio::task::JoinHandle<(HalResult<Vec<ReceivedCanFrame>>, usize)> {
    let runtime = runtime.clone();
    let session = handle.session_id();
    let token = handle.lease_token().clone();
    let mut operation = Box::pin(async move {
        let result = runtime.receive_can(session, &token, 1, timeout).await;
        let attempts_at_completion = attempts.load(Ordering::Acquire);
        (result, attempts_at_completion)
    });
    tokio::select! {
        biased;
        result = &mut operation => panic!("blocked CAN actor completed receive unexpectedly: {result:?}"),
        _ = tokio::task::yield_now() => {}
    }
    tokio::spawn(operation)
}

async fn wait_for_close_admission(
    runtime: &HalRuntime,
    session: seeed_hal_core::SessionId,
    token: &seeed_hal_core::LeaseToken,
) {
    tokio::time::timeout(Duration::from_secs(1), async {
        loop {
            match runtime
                .receive_can(session.clone(), token, 1, Duration::ZERO)
                .await
            {
                Err(error) if error.name().as_str() == "runtime.session.closed" => return,
                Err(error) if error.name().as_str() == "runtime.queue.full" => {
                    tokio::task::yield_now().await;
                }
                result => panic!("expected queued drop cleanup admission, got {result:?}"),
            }
        }
    })
    .await
    .expect("dropped handle must attempt cleanup while the actor remains blocked");
}

#[tokio::test]
async fn multiple_observers_receive_independent_fanout() {
    let adapter = VirtualCanAdapter::loopback("can:runtime:fanout");
    let runtime = HalRuntime::builder().can_adapter(adapter.clone()).build();
    let first = open(&runtime, &adapter, "first", LeaseMode::Observe).await;
    let second = open(&runtime, &adapter, "second", LeaseMode::Observe).await;
    adapter.inject_received(frame(1), None).unwrap();
    assert_eq!(
        first.receive(1, Duration::from_millis(100)).await.unwrap()[0].frame(),
        &frame(1)
    );
    assert_eq!(
        second.receive(1, Duration::from_millis(100)).await.unwrap()[0].frame(),
        &frame(1)
    );
}

#[tokio::test]
async fn exactly_one_controller_can_share_with_observers() {
    let adapter = VirtualCanAdapter::loopback("can:runtime:controller");
    let runtime = HalRuntime::builder().can_adapter(adapter.clone()).build();
    let _observer = open(&runtime, &adapter, "observer", LeaseMode::Observe).await;
    let _controller = open(&runtime, &adapter, "controller", LeaseMode::Control).await;
    let error = runtime
        .open_can(
            owner("other"),
            adapter.descriptor().selector(),
            LeaseMode::Control,
            attach(),
            all_frames(),
        )
        .await
        .err()
        .unwrap();
    assert_eq!(error.name().as_str(), "runtime.lease.conflict");
}

#[tokio::test]
async fn maintenance_is_exclusive_and_restores_access_after_close() {
    let adapter = VirtualCanAdapter::loopback("can:runtime:maintenance");
    let runtime = HalRuntime::builder().can_adapter(adapter.clone()).build();
    let configured = CanOpenConfig::Configure(
        CanConfigureConfig::new(
            CanMode::Classic,
            CanBitTiming::new(250_000, None, None).unwrap(),
            None,
            false,
            false,
        )
        .unwrap(),
    );
    let mut maintenance = runtime
        .open_can(
            owner("maintenance"),
            adapter.descriptor().selector(),
            LeaseMode::Maintenance,
            configured,
            all_frames(),
        )
        .await
        .unwrap();
    assert!(
        runtime
            .open_can(
                owner("blocked"),
                adapter.descriptor().selector(),
                LeaseMode::Observe,
                attach(),
                all_frames()
            )
            .await
            .is_err()
    );
    maintenance.close().await.unwrap();
    let _restored = open(&runtime, &adapter, "restored", LeaseMode::Observe).await;
}

#[tokio::test]
async fn filter_replacement_is_session_local_and_atomic() {
    let adapter = VirtualCanAdapter::loopback("can:runtime:filters");
    let runtime = HalRuntime::builder().can_adapter(adapter.clone()).build();
    let first = open(&runtime, &adapter, "first-filter", LeaseMode::Observe).await;
    let second = open(&runtime, &adapter, "second-filter", LeaseMode::Observe).await;
    let only_two = CanFilterSet::new(vec![
        CanFilter::new(
            2,
            0x7ff,
            CanIdFormat::Standard,
            CanFrameClasses::data_only(),
        )
        .unwrap(),
    ])
    .unwrap();
    first.replace_filters(only_two).await.unwrap();
    adapter.inject_received(frame(1), None).unwrap();
    adapter.inject_received(frame(2), None).unwrap();
    assert_eq!(
        first
            .receive(2, Duration::from_millis(100))
            .await
            .unwrap()
            .len(),
        1
    );
    assert_eq!(
        second
            .receive(2, Duration::from_millis(100))
            .await
            .unwrap()
            .len(),
        2
    );
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
    assert_eq!(
        retained
            .iter()
            .map(|item| item.frame().id().unwrap().value())
            .collect::<Vec<_>>(),
        vec![2, 3]
    );
}

#[tokio::test]
async fn tx_batch_admission_is_atomic_against_frame_capacity() {
    let adapter = ScriptedAdapter::new("can:runtime:tx-admission");
    let gate = Arc::new(CallGate::default());
    adapter.state.lock().unwrap().send_gate = Some(Arc::clone(&gate));
    let runtime = HalRuntime::builder()
        .can_adapter(adapter.clone())
        .can_tx_capacity(2)
        .build();
    let control = runtime
        .open_can(
            owner("tx"),
            adapter.selector(),
            LeaseMode::Control,
            attach(),
            all_frames(),
        )
        .await
        .unwrap();
    let session = control.session_id();
    let token = control.lease_token().clone();
    let first_runtime = runtime.clone();
    let first_token = token.clone();
    let first = tokio::spawn(async move {
        first_runtime
            .send_can_batch(session, &first_token, vec![frame(1), frame(2)])
            .await
    });
    gate.wait_started().await;
    let error = runtime
        .send_can(control.session_id(), &token, frame(3))
        .await
        .unwrap_err();
    assert_eq!(error.error().name().as_str(), "runtime.queue.full");
    assert_eq!(error.committed(), 0);
    gate.release();
    first.await.unwrap().unwrap();
    assert_eq!(adapter.state.lock().unwrap().sent.len(), 2);
}

#[tokio::test]
async fn backend_failure_reports_exact_partial_send_count() {
    let adapter = ScriptedAdapter::new("can:runtime:partial");
    adapter.state.lock().unwrap().fail_send_at = Some(2);
    let runtime = HalRuntime::builder().can_adapter(adapter.clone()).build();
    let control = runtime
        .open_can(
            owner("partial"),
            adapter.selector(),
            LeaseMode::Control,
            attach(),
            all_frames(),
        )
        .await
        .unwrap();
    let error = control
        .send_batch(vec![frame(1), frame(2), frame(3)])
        .await
        .unwrap_err();
    assert_eq!(error.committed(), 2);
    assert_eq!(adapter.state.lock().unwrap().sent.len(), 2);
}

#[tokio::test]
async fn concurrent_batch_commands_preserve_actor_fifo() {
    let adapter = ScriptedAdapter::new("can:runtime:fifo");
    let runtime = HalRuntime::builder().can_adapter(adapter.clone()).build();
    let control = runtime
        .open_can(
            owner("fifo"),
            adapter.selector(),
            LeaseMode::Control,
            attach(),
            all_frames(),
        )
        .await
        .unwrap();
    let (session, token) = control.into_parts();
    let barrier = Arc::new(tokio::sync::Barrier::new(3));
    let first = {
        let runtime = runtime.clone();
        let barrier = Arc::clone(&barrier);
        let session = session.clone();
        let token = token.clone();
        tokio::spawn(async move {
            barrier.wait().await;
            runtime
                .send_can_batch(session, &token, vec![frame(1), frame(2)])
                .await
        })
    };
    let second = {
        let runtime = runtime.clone();
        let barrier = Arc::clone(&barrier);
        let session = session.clone();
        let token = token.clone();
        tokio::spawn(async move {
            barrier.wait().await;
            runtime
                .send_can_batch(session, &token, vec![frame(3), frame(4)])
                .await
        })
    };
    barrier.wait().await;
    first.await.unwrap().unwrap();
    second.await.unwrap().unwrap();
    let sent = adapter
        .state
        .lock()
        .unwrap()
        .sent
        .iter()
        .map(|item| item.id().unwrap().value())
        .collect::<Vec<_>>();
    assert!(sent == vec![1, 2, 3, 4] || sent == vec![3, 4, 1, 2]);
    runtime.close_can(session, &token).await.unwrap();
}

#[tokio::test]
async fn receive_timeout_returns_empty_within_a_finite_deadline() {
    let adapter = VirtualCanAdapter::loopback("can:runtime:timeout");
    let runtime = HalRuntime::builder().can_adapter(adapter.clone()).build();
    let observer = open(&runtime, &adapter, "timeout", LeaseMode::Observe).await;
    let started = Instant::now();
    assert!(
        observer
            .receive(1, Duration::from_millis(10))
            .await
            .unwrap()
            .is_empty()
    );
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
        receive_runtime
            .receive_can(session, &receive_token, 1, Duration::from_secs(30))
            .await
    });
    tokio::time::sleep(Duration::from_millis(10)).await;
    cancelled.abort();
    let _ = cancelled.await;
    tokio::time::sleep(Duration::from_millis(5)).await;
    adapter.inject_received(frame(7), None).unwrap();
    assert_eq!(
        runtime
            .receive_can(observer.session_id(), &token, 1, Duration::from_millis(100))
            .await
            .unwrap()[0]
            .frame(),
        &frame(7)
    );
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
    let error = runtime
        .send_can(new.session_id(), &old_token, frame(1))
        .await
        .unwrap_err();
    assert_eq!(
        error.error().name().as_str(),
        "runtime.lease.stale_generation"
    );
    assert_eq!(
        runtime
            .send_can(old_session, &old_token, frame(1))
            .await
            .unwrap_err()
            .error()
            .name()
            .as_str(),
        "runtime.session.closed"
    );
}

#[tokio::test]
async fn owner_revoke_closes_all_can_sessions() {
    let first = VirtualCanAdapter::loopback("can:runtime:revoke-a");
    let second = VirtualCanAdapter::loopback("can:runtime:revoke-b");
    let runtime = HalRuntime::builder()
        .can_adapter(first.clone())
        .can_adapter(second.clone())
        .build();
    let owner_id = owner("revoked");
    let first_handle = runtime
        .open_can(
            owner_id.clone(),
            first.descriptor().selector(),
            LeaseMode::Observe,
            attach(),
            all_frames(),
        )
        .await
        .unwrap();
    let second_handle = runtime
        .open_can(
            owner_id.clone(),
            second.descriptor().selector(),
            LeaseMode::Control,
            attach(),
            all_frames(),
        )
        .await
        .unwrap();
    runtime.revoke_owner(&owner_id).await.unwrap();
    assert_eq!(
        first_handle
            .receive(1, Duration::ZERO)
            .await
            .unwrap_err()
            .name()
            .as_str(),
        "runtime.session.closed"
    );
    assert_eq!(
        second_handle
            .send(frame(1))
            .await
            .unwrap_err()
            .error()
            .name()
            .as_str(),
        "runtime.session.closed"
    );
}

#[tokio::test]
async fn owner_revoke_continues_can_cleanup_after_serial_error() {
    let serial = FailingCloseSerialAdapter(VirtualSerialAdapter::loopback(
        "serial:runtime:mixed-revoke",
    ));
    let can = VirtualCanAdapter::loopback("can:runtime:mixed-revoke");
    let runtime = HalRuntime::builder()
        .serial_adapter(serial.clone())
        .can_adapter(can.clone())
        .build();
    let owner_id = owner("mixed-revoke");
    let serial_descriptor = serial.enumerate().await.unwrap().remove(0);
    let serial_handle = runtime
        .open_serial(
            owner_id.clone(),
            serial_descriptor.selector(),
            SerialConfig::default(),
        )
        .await
        .unwrap();
    let can_handle = runtime
        .open_can(
            owner_id.clone(),
            can.descriptor().selector(),
            LeaseMode::Observe,
            attach(),
            all_frames(),
        )
        .await
        .unwrap();
    let error = runtime.revoke_owner(&owner_id).await.unwrap_err();
    assert_eq!(error.name().as_str(), "test.serial.close");
    assert_eq!(
        serial_handle.read(1).await.unwrap_err().name().as_str(),
        "runtime.session.closed"
    );
    assert_eq!(
        can_handle
            .receive(1, Duration::ZERO)
            .await
            .unwrap_err()
            .name()
            .as_str(),
        "runtime.session.closed"
    );
}

#[tokio::test]
async fn actor_panic_disconnect_is_reported_without_hanging() {
    let adapter = ScriptedAdapter::new("can:runtime:panic");
    let gate = Arc::new(CallGate::default());
    adapter.state.lock().unwrap().panic_receives_remaining = 1;
    adapter.state.lock().unwrap().receive_gate = Some(Arc::clone(&gate));
    let runtime = HalRuntime::builder().can_adapter(adapter.clone()).build();
    let observer = runtime
        .open_can(
            owner("panic"),
            adapter.selector(),
            LeaseMode::Observe,
            attach(),
            all_frames(),
        )
        .await
        .unwrap();
    gate.release();
    wait_for_can_session_closed(&observer).await;
}

#[tokio::test]
async fn close_timeout_is_finite_and_structured() {
    let adapter = ScriptedAdapter::new("can:runtime:close-timeout");
    adapter.state.lock().unwrap().close_delay = Duration::from_millis(100);
    let runtime = HalRuntime::builder()
        .can_adapter(adapter.clone())
        .can_close_timeout(Duration::from_millis(10))
        .build();
    let mut observer = runtime
        .open_can(
            owner("slow-close"),
            adapter.selector(),
            LeaseMode::Observe,
            attach(),
            all_frames(),
        )
        .await
        .unwrap();
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
    let runtime = HalRuntime::builder()
        .can_adapter(first.clone())
        .can_adapter(second.clone())
        .can_adapter(duplicate)
        .build();
    let descriptors = runtime.enumerate_can().await.unwrap();
    assert_eq!(
        descriptors
            .iter()
            .map(|item| item.id().as_str())
            .collect::<Vec<_>>(),
        vec![
            "can:runtime:adapter-a",
            "can:runtime:adapter-a",
            "can:runtime:adapter-b"
        ]
    );
    let error = runtime
        .open_can(
            owner("ambiguous"),
            first.descriptor().selector(),
            LeaseMode::Observe,
            attach(),
            all_frames(),
        )
        .await
        .err()
        .unwrap();
    assert_eq!(error.name().as_str(), "runtime.resource.ambiguous");
    let _second = runtime
        .open_can(
            owner("unique"),
            second.descriptor().selector(),
            LeaseMode::Observe,
            attach(),
            all_frames(),
        )
        .await
        .unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn backend_methods_never_run_on_tokio_workers() {
    let adapter = ScriptedAdapter::new("can:runtime:thread-affinity");
    let runtime = HalRuntime::builder().can_adapter(adapter.clone()).build();
    let mut control = runtime
        .open_can(
            owner("thread"),
            adapter.selector(),
            LeaseMode::Control,
            attach(),
            all_frames(),
        )
        .await
        .unwrap();
    control.send(frame(1)).await.unwrap();
    control.bus_status().await.unwrap();
    control.close().await.unwrap();
    let state = adapter.state.lock().unwrap();
    assert!(!state.thread_names.is_empty());
    assert!(
        state
            .thread_names
            .iter()
            .all(|name| name.starts_with("seeed-hal-can-"))
    );
}

#[tokio::test]
async fn failed_actor_is_reconciled_before_resource_reuse() {
    let adapter = ScriptedAdapter::new("can:runtime:failed-reopen");
    let gate = Arc::new(CallGate::default());
    adapter.state.lock().unwrap().panic_receives_remaining = 1;
    adapter.state.lock().unwrap().receive_gate = Some(Arc::clone(&gate));
    let runtime = HalRuntime::builder().can_adapter(adapter.clone()).build();
    let first = runtime
        .open_can(
            owner("failed-first"),
            adapter.selector(),
            LeaseMode::Observe,
            attach(),
            all_frames(),
        )
        .await
        .unwrap();
    gate.release();
    wait_for_can_session_closed(&first).await;
    let second = runtime
        .open_can(
            owner("failed-second"),
            adapter.selector(),
            LeaseMode::Observe,
            attach(),
            all_frames(),
        )
        .await
        .unwrap();
    assert_eq!(
        first
            .receive(1, Duration::ZERO)
            .await
            .unwrap_err()
            .name()
            .as_str(),
        "runtime.session.closed"
    );
    adapter
        .state
        .lock()
        .unwrap()
        .received
        .push_back(ReceivedCanFrame::new(frame(9), None));
    assert_eq!(
        second.receive(1, Duration::from_millis(100)).await.unwrap()[0].frame(),
        &frame(9)
    );
    assert_eq!(adapter.state.lock().unwrap().open_count, 2);
}

#[tokio::test]
async fn provisional_cancellation_survives_saturated_management_queue() {
    let adapter = ScriptedAdapter::new("can:runtime:provisional-saturation");
    let gate = Arc::new(CallGate::default());
    adapter.state.lock().unwrap().status_gate = Some(Arc::clone(&gate));
    let runtime = HalRuntime::builder()
        .can_adapter(adapter.clone())
        .can_close_timeout(Duration::from_millis(20))
        .build();
    let first = runtime
        .open_can(
            owner("saturation-first"),
            adapter.selector(),
            LeaseMode::Observe,
            attach(),
            all_frames(),
        )
        .await
        .unwrap();
    let status_runtime = runtime.clone();
    let status_session = first.session_id();
    let status_token = first.lease_token().clone();
    let blocked = tokio::spawn(async move {
        status_runtime
            .can_bus_status(status_session, &status_token)
            .await
    });
    gate.wait_started().await;
    let opening_runtime = runtime.clone();
    let opening_selector = adapter.selector();
    let opening = tokio::spawn(async move {
        opening_runtime
            .open_can(
                owner("cancelled-open"),
                opening_selector,
                LeaseMode::Observe,
                attach(),
                all_frames(),
            )
            .await
    });
    tokio::task::yield_now().await;
    let mut pressure = Vec::new();
    for _ in 0..63 {
        pressure.push(enqueue_blocked_filter_command(&runtime, &first).await);
    }
    let saturated = runtime
        .replace_can_filters(first.session_id(), first.lease_token(), all_frames())
        .await
        .unwrap_err();
    assert_eq!(saturated.name().as_str(), "runtime.queue.full");
    assert!(opening.await.unwrap().is_err());
    gate.release();
    let _ = blocked.await;
    for task in pressure {
        let _ = task.await;
    }
    tokio::time::sleep(Duration::from_millis(20)).await;
    let (session, token) = first.into_parts();
    runtime.close_can(session, &token).await.unwrap();
    assert_eq!(adapter.state.lock().unwrap().close_count, 1);
}

#[tokio::test]
async fn drop_cleanup_survives_saturated_management_queue() {
    let adapter = ScriptedAdapter::new("can:runtime:drop-saturation");
    let gate = Arc::new(CallGate::default());
    adapter.state.lock().unwrap().status_gate = Some(Arc::clone(&gate));
    let runtime = HalRuntime::builder()
        .can_adapter(adapter.clone())
        .can_close_timeout(Duration::from_millis(100))
        .build();
    let first = runtime
        .open_can(
            owner("drop-first"),
            adapter.selector(),
            LeaseMode::Observe,
            attach(),
            all_frames(),
        )
        .await
        .unwrap();
    let dropped = runtime
        .open_can(
            owner("drop-second"),
            adapter.selector(),
            LeaseMode::Observe,
            attach(),
            all_frames(),
        )
        .await
        .unwrap();
    let dropped_session = dropped.session_id();
    let dropped_token = dropped.lease_token().clone();
    let status_runtime = runtime.clone();
    let status_session = first.session_id();
    let status_token = first.lease_token().clone();
    let blocked = tokio::spawn(async move {
        status_runtime
            .can_bus_status(status_session, &status_token)
            .await
    });
    gate.wait_started().await;
    let mut pressure = Vec::new();
    for _ in 0..64 {
        pressure.push(enqueue_blocked_filter_command(&runtime, &first).await);
    }
    drop(dropped);
    wait_for_close_admission(&runtime, dropped_session, &dropped_token).await;
    gate.release();
    let _ = blocked.await;
    for task in pressure {
        let _ = task.await;
    }
    let (session, token) = first.into_parts();
    runtime.close_can(session, &token).await.unwrap();
    assert_eq!(adapter.state.lock().unwrap().close_count, 1);
}

#[tokio::test]
async fn management_pressure_does_not_starve_receive_deadline() {
    let adapter = ScriptedAdapter::new("can:runtime:command-budget");
    let gate = Arc::new(CallGate::default());
    adapter.state.lock().unwrap().receive_gate = Some(Arc::clone(&gate));
    let runtime = HalRuntime::builder().can_adapter(adapter.clone()).build();
    let observer = runtime
        .open_can(
            owner("budget"),
            adapter.selector(),
            LeaseMode::Observe,
            attach(),
            all_frames(),
        )
        .await
        .unwrap();
    gate.wait_started().await;
    let started = Instant::now();
    let stop = Arc::new(AtomicBool::new(false));
    let attempts = Arc::new(AtomicUsize::new(0));
    let receive = enqueue_blocked_receive(
        &runtime,
        &observer,
        Duration::from_millis(20),
        Arc::clone(&attempts),
    )
    .await;
    let mut producers = Vec::new();
    for _ in 0..64 {
        let runtime = runtime.clone();
        let session = observer.session_id();
        let token = observer.lease_token().clone();
        let stop = Arc::clone(&stop);
        let attempts = Arc::clone(&attempts);
        producers.push(tokio::spawn(async move {
            while !stop.load(Ordering::Acquire) {
                attempts.fetch_add(1, Ordering::AcqRel);
                let _ = runtime
                    .replace_can_filters(session.clone(), &token, all_frames())
                    .await;
            }
        }));
    }
    while attempts.load(Ordering::Acquire) < 64 {
        tokio::task::yield_now().await;
    }
    gate.release();
    let attempts_after_release = attempts.load(Ordering::Acquire);
    let (received, attempts_at_completion) = receive.await.unwrap();
    assert!(received.unwrap().is_empty());
    assert!(attempts_at_completion > attempts_after_release);
    assert!(started.elapsed() < Duration::from_millis(100));
    stop.store(true, Ordering::Release);
    for producer in producers {
        producer.await.unwrap();
    }
}

#[tokio::test]
async fn capability_mismatched_duplicate_ids_fail_closed_across_adapters() {
    let classic = DescriptorAdapter::single(descriptor_with_capability(
        "can:runtime:duplicate-cross",
        can_classic_capability(),
    ));
    let fd = DescriptorAdapter::single(descriptor_with_capability(
        "can:runtime:duplicate-cross",
        seeed_hal_can::can_fd_capability(),
    ));
    let runtime = HalRuntime::builder()
        .can_adapter(classic.clone())
        .can_adapter(fd)
        .build();
    let error = runtime
        .open_can(
            owner("duplicate"),
            classic.descriptors[0].selector(),
            LeaseMode::Observe,
            attach(),
            all_frames(),
        )
        .await
        .err()
        .unwrap();
    assert_eq!(error.name().as_str(), "runtime.resource.ambiguous");
}

#[tokio::test]
async fn capability_mismatched_duplicate_ids_fail_closed_within_adapter() {
    let adapter = DescriptorAdapter::new(vec![
        descriptor_with_capability("can:runtime:duplicate-within", can_classic_capability()),
        descriptor_with_capability(
            "can:runtime:duplicate-within",
            seeed_hal_can::can_fd_capability(),
        ),
    ]);
    let runtime = HalRuntime::builder().can_adapter(adapter.clone()).build();
    let error = runtime
        .open_can(
            owner("duplicate"),
            adapter.descriptors[0].selector(),
            LeaseMode::Observe,
            attach(),
            all_frames(),
        )
        .await
        .err()
        .unwrap();
    assert_eq!(error.name().as_str(), "runtime.resource.ambiguous");
}

#[tokio::test]
async fn hung_enumeration_future_is_dropped_inside_worker_deadline() {
    let adapter = HangingAdapter::enumerate("can:runtime:hung-enumerate");
    let dropped = Arc::clone(&adapter.future_dropped);
    let runtime = HalRuntime::builder()
        .can_adapter(adapter)
        .can_close_timeout(Duration::from_millis(10))
        .build();
    assert_eq!(
        runtime.enumerate_can().await.unwrap_err().name().as_str(),
        "runtime.transport.timeout"
    );
    assert!(dropped.load(Ordering::Acquire));
}

#[tokio::test]
async fn hung_open_future_is_dropped_inside_actor_worker_deadline() {
    let adapter = HangingAdapter::open("can:runtime:hung-open");
    let selector = adapter.descriptor.selector();
    let dropped = Arc::clone(&adapter.future_dropped);
    let runtime = HalRuntime::builder()
        .can_adapter(adapter)
        .can_close_timeout(Duration::from_millis(10))
        .build();
    assert!(
        runtime
            .open_can(
                owner("hung"),
                selector,
                LeaseMode::Observe,
                attach(),
                all_frames()
            )
            .await
            .is_err()
    );
    assert!(dropped.load(Ordering::Acquire));
}

#[tokio::test]
async fn open_revoke_race_has_monotonic_lifecycle_events() {
    let adapter = ScriptedAdapter::new("can:runtime:open-revoke-events");
    let gate = Arc::new(CallGate::default());
    adapter.state.lock().unwrap().open_gate = Some(Arc::clone(&gate));
    let runtime = HalRuntime::builder()
        .can_adapter(adapter.clone())
        .can_close_timeout(Duration::from_millis(100))
        .build();
    let mut events = runtime.subscribe();
    let owner_id = owner("open-revoke");
    let opening = {
        let runtime = runtime.clone();
        let owner_id = owner_id.clone();
        let selector = adapter.selector();
        tokio::spawn(async move {
            runtime
                .open_can(
                    owner_id,
                    selector,
                    LeaseMode::Observe,
                    attach(),
                    all_frames(),
                )
                .await
        })
    };
    gate.wait_started().await;
    let revoke = runtime.revoke_owner(&owner_id);
    tokio::pin!(revoke);
    tokio::select! {
        biased;
        result = &mut revoke => panic!("revocation completed before provisional actor cleanup: {result:?}"),
        _ = tokio::task::yield_now() => {}
    }
    gate.release();
    assert!(opening.await.unwrap().is_err());
    revoke.await.unwrap();
    assert!(
        tokio::time::timeout(Duration::from_millis(10), events.recv())
            .await
            .is_err()
    );
}

#[tokio::test]
async fn can_health_event_follows_open_and_precedes_close() {
    let adapter = VirtualCanAdapter::loopback("can:runtime:health-events");
    let runtime = HalRuntime::builder().can_adapter(adapter.clone()).build();
    let mut events = runtime.subscribe();
    let mut observer = open(&runtime, &adapter, "health", LeaseMode::Observe).await;
    observer.bus_status().await.unwrap();
    adapter.set_bus_status(CanBusStatus::new(CanBusState::Warning, Some(1), Some(2)));
    observer.bus_status().await.unwrap();
    observer.close().await.unwrap();
    assert_eq!(
        events.recv().await.unwrap().kind(),
        RuntimeEventKind::SessionOpened
    );
    assert_eq!(
        events.recv().await.unwrap().kind(),
        RuntimeEventKind::CanBusWarning
    );
    assert_eq!(
        events.recv().await.unwrap().kind(),
        RuntimeEventKind::SessionClosed
    );
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
    panic_receives_remaining: usize,
    close_delay: Duration,
    thread_names: Vec<String>,
    send_gate: Option<Arc<CallGate>>,
    status_gate: Option<Arc<CallGate>>,
    open_gate: Option<Arc<CallGate>>,
    receive_gate: Option<Arc<CallGate>>,
    open_count: usize,
    close_count: usize,
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

    async fn open(
        &self,
        selector: &ResourceSelector,
        _config: &CanOpenConfig,
    ) -> HalResult<Box<dyn CanChannel>> {
        record_thread(&self.state);
        let gate = self.state.lock().unwrap().open_gate.clone();
        if let Some(gate) = gate {
            gate.enter();
        }
        let descriptor = resolve_resource(
            std::slice::from_ref(&self.descriptor),
            selector,
            &can_classic_capability(),
            "can.open",
        )?
        .clone();
        let active = CanActiveConfig::new(
            CanMode::Classic,
            CanBitTiming::new(500_000, None, None)?,
            None,
            false,
            false,
            "scripted",
        )
        .unwrap();
        self.state.lock().unwrap().open_count += 1;
        Ok(Box::new(ScriptedChannel {
            descriptor,
            active,
            state: Arc::clone(&self.state),
        }))
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
        let gate = self.state.lock().unwrap().receive_gate.clone();
        if let Some(gate) = gate {
            gate.enter();
        }
        let mut state = self.state.lock().unwrap();
        if state.panic_receives_remaining > 0 {
            state.panic_receives_remaining -= 1;
            drop(state);
            panic!("injected receive panic");
        }
        let frame = state.received.pop_front();
        drop(state);
        if frame.is_none() {
            std::thread::sleep(timeout);
        }
        Ok(frame)
    }

    fn send(&mut self, frame: &CanFrame) -> HalResult<()> {
        record_thread(&self.state);
        let gate = self.state.lock().unwrap().send_gate.clone();
        if let Some(gate) = gate {
            gate.enter();
        }
        let mut state = self.state.lock().unwrap();
        let attempt = state.send_attempts;
        state.send_attempts += 1;
        if state.fail_send_at == Some(attempt) {
            state.fail_send_at = None;
            return Err(HalError::new(
                "can.bus.off",
                ErrorCategory::Unavailable,
                "can.send",
                false,
                "injected send failure",
            )?
            .with_resource_id(self.descriptor.id().clone()));
        }
        state.sent.push(frame.clone());
        Ok(())
    }

    fn bus_status(&mut self) -> HalResult<CanBusStatus> {
        record_thread(&self.state);
        let gate = self.state.lock().unwrap().status_gate.clone();
        if let Some(gate) = gate {
            gate.enter();
        }
        Ok(CanBusStatus::new(CanBusState::Active, None, None))
    }

    fn close(&mut self) -> HalResult<()> {
        record_thread(&self.state);
        let delay = self.state.lock().unwrap().close_delay;
        std::thread::sleep(delay);
        self.state.lock().unwrap().close_count += 1;
        Ok(())
    }
}

fn record_thread(state: &Arc<Mutex<ScriptedState>>) {
    state.lock().unwrap().thread_names.push(
        std::thread::current()
            .name()
            .unwrap_or("unnamed")
            .to_owned(),
    );
}

#[derive(Default)]
struct CallGate {
    started: AtomicBool,
    released: Mutex<bool>,
    changed: Condvar,
}

impl CallGate {
    fn enter(&self) {
        self.started.store(true, Ordering::Release);
        let mut released = self.released.lock().unwrap();
        while !*released {
            released = self.changed.wait(released).unwrap();
        }
    }

    async fn wait_started(&self) {
        while !self.started.load(Ordering::Acquire) {
            tokio::task::yield_now().await;
        }
    }

    fn release(&self) {
        *self.released.lock().unwrap() = true;
        self.changed.notify_all();
    }
}

#[derive(Clone)]
struct FailingCloseSerialAdapter(VirtualSerialAdapter);

#[async_trait]
impl SerialAdapter for FailingCloseSerialAdapter {
    fn adapter_name(&self) -> &'static str {
        "test.failing-close.serial"
    }
    async fn enumerate(&self) -> HalResult<Vec<ResourceDescriptor>> {
        self.0.enumerate().await
    }
    async fn open(
        &self,
        selector: &ResourceSelector,
        config: SerialConfig,
    ) -> HalResult<Box<dyn SerialSession>> {
        Ok(Box::new(FailingCloseSerialSession(
            self.0.open(selector, config).await?,
        )))
    }
}

struct FailingCloseSerialSession(Box<dyn SerialSession>);

#[async_trait]
impl SerialSession for FailingCloseSerialSession {
    fn descriptor(&self) -> &ResourceDescriptor {
        self.0.descriptor()
    }
    async fn read(&mut self, max_bytes: usize) -> HalResult<Bytes> {
        self.0.read(max_bytes).await
    }
    async fn write_all(&mut self, bytes: &[u8]) -> HalResult<()> {
        self.0.write_all(bytes).await
    }
    async fn flush(&mut self) -> HalResult<()> {
        self.0.flush().await
    }
    async fn set_control_lines(&mut self, lines: ControlLines) -> HalResult<()> {
        self.0.set_control_lines(lines).await
    }
    async fn close(&mut self) -> HalResult<()> {
        let _ = self.0.close().await;
        Err(HalError::new(
            "test.serial.close",
            ErrorCategory::Unavailable,
            "serial.close",
            false,
            "injected serial cleanup failure",
        )?)
    }
}

#[derive(Clone)]
struct DescriptorAdapter {
    descriptors: Vec<ResourceDescriptor>,
}

impl DescriptorAdapter {
    fn new(descriptors: Vec<ResourceDescriptor>) -> Self {
        Self { descriptors }
    }
    fn single(descriptor: ResourceDescriptor) -> Self {
        Self::new(vec![descriptor])
    }
}

#[async_trait]
impl CanAdapter for DescriptorAdapter {
    fn adapter_name(&self) -> &'static str {
        "test.descriptor.can"
    }
    async fn enumerate(&self) -> HalResult<Vec<ResourceDescriptor>> {
        Ok(self.descriptors.clone())
    }
    async fn open(
        &self,
        _selector: &ResourceSelector,
        _config: &CanOpenConfig,
    ) -> HalResult<Box<dyn CanChannel>> {
        Err(HalError::new(
            "test.unexpected_open",
            ErrorCategory::Internal,
            "can.open",
            false,
            "duplicate identity must fail before adapter open",
        )?)
    }
}

fn descriptor_with_capability(
    resource_id: &str,
    capability: seeed_hal_core::CapabilityId,
) -> ResourceDescriptor {
    ResourceDescriptor::new(
        ResourceId::parse(resource_id).unwrap(),
        Endpoint::new(format!(
            "descriptor://{resource_id}/{}",
            capability.as_str()
        ))
        .unwrap(),
        IdentityQuality::Strong,
        TransportKind::Can,
        ResourceProperties::default(),
        CapabilitySet::new(vec![capability]),
    )
}

#[derive(Clone)]
struct HangingAdapter {
    descriptor: ResourceDescriptor,
    hang_enumerate: bool,
    hang_open: bool,
    future_dropped: Arc<AtomicBool>,
}

impl HangingAdapter {
    fn enumerate(resource_id: &str) -> Self {
        Self::new(resource_id, true, false)
    }
    fn open(resource_id: &str) -> Self {
        Self::new(resource_id, false, true)
    }
    fn new(resource_id: &str, hang_enumerate: bool, hang_open: bool) -> Self {
        Self {
            descriptor: descriptor_with_capability(resource_id, can_classic_capability()),
            hang_enumerate,
            hang_open,
            future_dropped: Arc::new(AtomicBool::new(false)),
        }
    }
}

struct FutureDropFlag(Arc<AtomicBool>);

impl Drop for FutureDropFlag {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Release);
    }
}

#[async_trait]
impl CanAdapter for HangingAdapter {
    fn adapter_name(&self) -> &'static str {
        "test.hanging.can"
    }
    async fn enumerate(&self) -> HalResult<Vec<ResourceDescriptor>> {
        if self.hang_enumerate {
            let _drop = FutureDropFlag(Arc::clone(&self.future_dropped));
            std::future::pending::<()>().await;
        }
        Ok(vec![self.descriptor.clone()])
    }
    async fn open(
        &self,
        _selector: &ResourceSelector,
        _config: &CanOpenConfig,
    ) -> HalResult<Box<dyn CanChannel>> {
        if self.hang_open {
            let _drop = FutureDropFlag(Arc::clone(&self.future_dropped));
            std::future::pending::<()>().await;
        }
        unreachable!("non-hanging open is not used by these tests")
    }
}
