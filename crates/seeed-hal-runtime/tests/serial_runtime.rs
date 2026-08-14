use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use seeed_hal_core::{LeaseId, LeaseMode, LeaseToken, OwnerId, SessionId};
use seeed_hal_runtime::HalRuntime;
use seeed_hal_serial::{ControlLines, SerialConfig};
use seeed_hal_testkit::VirtualSerialAdapter;

fn owner(value: &str) -> OwnerId {
    OwnerId::parse(value).unwrap()
}

#[tokio::test]
async fn stale_generation_never_reaches_the_adapter() {
    let adapter = VirtualSerialAdapter::loopback("serial:virtual:fenced");
    let runtime = HalRuntime::builder().serial_adapter(adapter).build();
    let descriptor = runtime.enumerate_serial().await.unwrap().remove(0);
    let config = SerialConfig {
        read_timeout: Duration::from_millis(25),
        ..SerialConfig::default()
    };

    let first = runtime
        .open_serial(owner("client-a"), descriptor.selector(), config.clone())
        .await
        .unwrap();
    let stale = first.lease_token().clone();
    first.close().await.unwrap();

    let second = runtime
        .open_serial(owner("client-b"), descriptor.selector(), config)
        .await
        .unwrap();
    let error = runtime
        .write_serial(second.session_id(), &stale, Bytes::from_static(b"stale"))
        .await
        .unwrap_err();

    assert_eq!(error.name().as_str(), "runtime.lease.stale_generation");
    assert!(second.lease_token().generation() > stale.generation());

    let error = second.read(1).await.unwrap_err();
    assert_eq!(error.name().as_str(), "runtime.transport.timeout");

    second.write(Bytes::from_static(b"y")).await.unwrap();
    assert_eq!(second.read(1).await.unwrap(), Bytes::from_static(b"y"));
    second.close().await.unwrap();
}

#[tokio::test]
async fn control_lease_is_exclusive_until_the_session_closes() {
    let adapter = VirtualSerialAdapter::loopback("serial:virtual:exclusive");
    let runtime = HalRuntime::builder().serial_adapter(adapter).build();
    let descriptor = runtime.enumerate_serial().await.unwrap().remove(0);

    let first = runtime
        .open_serial(
            owner("client-a"),
            descriptor.selector(),
            SerialConfig::default(),
        )
        .await
        .unwrap();
    let error = match runtime
        .open_serial(
            owner("client-b"),
            descriptor.selector(),
            SerialConfig::default(),
        )
        .await
    {
        Ok(_) => panic!("an active control lease must remain exclusive"),
        Err(error) => error,
    };

    assert_eq!(error.name().as_str(), "runtime.lease.conflict");

    first.close().await.unwrap();
    runtime
        .open_serial(
            owner("client-b"),
            descriptor.selector(),
            SerialConfig::default(),
        )
        .await
        .unwrap()
        .close()
        .await
        .unwrap();
}

#[tokio::test]
async fn revoking_an_owner_closes_its_sessions_and_releases_resources() {
    let adapter = VirtualSerialAdapter::loopback("serial:virtual:revoke");
    let runtime = HalRuntime::builder().serial_adapter(adapter).build();
    let descriptor = runtime.enumerate_serial().await.unwrap().remove(0);
    let first_owner = owner("client-a");
    let first = runtime
        .open_serial(
            first_owner.clone(),
            descriptor.selector(),
            SerialConfig::default(),
        )
        .await
        .unwrap();
    let first_generation = first.lease_token().generation();

    runtime.revoke_owner(&first_owner).await.unwrap();

    let error = first.write(Bytes::from_static(b"x")).await.unwrap_err();
    assert_eq!(error.name().as_str(), "runtime.session.closed");

    let second = runtime
        .open_serial(
            owner("client-b"),
            descriptor.selector(),
            SerialConfig::default(),
        )
        .await
        .unwrap();
    assert!(second.lease_token().generation() > first_generation);
    second.close().await.unwrap();
}

#[tokio::test]
async fn dropping_a_handle_schedules_best_effort_resource_cleanup() {
    let adapter = VirtualSerialAdapter::loopback("serial:virtual:drop-cleanup");
    let runtime = HalRuntime::builder().serial_adapter(adapter).build();
    let descriptor = runtime.enumerate_serial().await.unwrap().remove(0);
    let first = runtime
        .open_serial(
            owner("client-a"),
            descriptor.selector(),
            SerialConfig::default(),
        )
        .await
        .unwrap();
    let first_generation = first.lease_token().generation();

    drop(first);

    let second = tokio::time::timeout(Duration::from_millis(250), async {
        loop {
            match runtime
                .open_serial(
                    owner("client-b"),
                    descriptor.selector(),
                    SerialConfig::default(),
                )
                .await
            {
                Ok(handle) => break handle,
                Err(error) if error.name().as_str() == "runtime.lease.conflict" => {
                    tokio::task::yield_now().await;
                }
                Err(error) => panic!("unexpected reopen error: {error}"),
            }
        }
    })
    .await
    .expect("dropped handle cleanup must eventually release its lease");
    assert!(second.lease_token().generation() > first_generation);
    second.close().await.unwrap();
}

#[tokio::test]
async fn operation_queue_rejects_the_request_beyond_its_64_slots() {
    let adapter = VirtualSerialAdapter::loopback("serial:virtual:runtime-queue");
    let runtime = Arc::new(HalRuntime::builder().serial_adapter(adapter).build());
    let descriptor = runtime.enumerate_serial().await.unwrap().remove(0);
    let config = SerialConfig {
        read_timeout: Duration::from_secs(5),
        ..SerialConfig::default()
    };
    let handle = runtime
        .open_serial(owner("client-a"), descriptor.selector(), config)
        .await
        .unwrap();
    let session_id = handle.session_id();
    let lease = handle.lease_token().clone();
    let (result_tx, mut result_rx) = tokio::sync::mpsc::unbounded_channel();
    let mut tasks = Vec::new();

    for _ in 0..66 {
        let runtime = Arc::clone(&runtime);
        let session_id = session_id.clone();
        let lease = lease.clone();
        let result_tx = result_tx.clone();
        tasks.push(tokio::spawn(async move {
            let name = runtime
                .read_serial(session_id, &lease, 1)
                .await
                .unwrap_err()
                .name()
                .as_str()
                .to_owned();
            let _ = result_tx.send(name);
        }));
    }
    drop(result_tx);

    let first_result = tokio::time::timeout(Duration::from_millis(500), result_rx.recv())
        .await
        .expect("an overflowing request must fail without waiting")
        .unwrap();
    assert_eq!(first_result, "runtime.queue.full");

    handle.close().await.unwrap();
    for task in tasks {
        task.await.unwrap();
    }
}

#[tokio::test]
async fn serial_handle_delegates_operations_and_explicit_close_is_idempotent() {
    let adapter = VirtualSerialAdapter::loopback("serial:virtual:handle");
    let runtime = HalRuntime::builder().serial_adapter(adapter).build();
    let descriptor = runtime.enumerate_serial().await.unwrap().remove(0);
    let handle = runtime
        .open_serial(
            owner("client-a"),
            descriptor.selector(),
            SerialConfig::default(),
        )
        .await
        .unwrap();
    let session_id = handle.session_id();
    let lease = handle.lease_token().clone();

    handle
        .set_control_lines(ControlLines {
            data_terminal_ready: true,
            request_to_send: true,
        })
        .await
        .unwrap();
    handle.write(Bytes::from_static(b"abc")).await.unwrap();
    handle.flush().await.unwrap();
    assert_eq!(handle.read(3).await.unwrap(), Bytes::from_static(b"abc"));
    handle.close().await.unwrap();

    runtime
        .close_serial(session_id.clone(), &lease)
        .await
        .unwrap();
    let error = runtime.flush_serial(session_id, &lease).await.unwrap_err();
    assert_eq!(error.name().as_str(), "runtime.session.closed");
}

#[tokio::test]
async fn session_lifecycle_events_are_ordered_and_describe_the_same_session() {
    let adapter = VirtualSerialAdapter::loopback("serial:virtual:events");
    let runtime = HalRuntime::builder().serial_adapter(adapter).build();
    let descriptor = runtime.enumerate_serial().await.unwrap().remove(0);
    let expected_owner = owner("client-a");
    let mut events = runtime.subscribe();

    let handle = runtime
        .open_serial(
            expected_owner.clone(),
            descriptor.selector(),
            SerialConfig::default(),
        )
        .await
        .unwrap();
    let session_id = handle.session_id();
    let generation = handle.lease_token().generation();
    let opened = events.recv().await.unwrap();
    handle.close().await.unwrap();
    let closed = events.recv().await.unwrap();

    assert_eq!(opened.name(), "session.opened");
    assert_eq!(closed.name(), "session.closed");
    assert_eq!(closed.sequence(), opened.sequence() + 1);
    assert_eq!(opened.resource_id(), descriptor.id());
    assert_eq!(closed.resource_id(), descriptor.id());
    assert_eq!(opened.session_id(), &session_id);
    assert_eq!(closed.session_id(), &session_id);
    assert_eq!(opened.owner_id(), &expected_owner);
    assert_eq!(closed.owner_id(), &expected_owner);
    assert_eq!(opened.lease_generation(), generation);
    assert_eq!(closed.lease_generation(), generation);
}

#[tokio::test]
async fn invalid_session_and_lease_are_rejected_before_serial_io() {
    let adapter = VirtualSerialAdapter::loopback("serial:virtual:validation");
    let runtime = HalRuntime::builder().serial_adapter(adapter).build();
    let descriptor = runtime.enumerate_serial().await.unwrap().remove(0);
    let config = SerialConfig {
        read_timeout: Duration::from_millis(25),
        ..SerialConfig::default()
    };
    let handle = runtime
        .open_serial(owner("client-a"), descriptor.selector(), config)
        .await
        .unwrap();
    let lease = handle.lease_token().clone();
    let forged = LeaseToken::new(LeaseId::new(), lease.generation(), LeaseMode::Control);

    let error = runtime
        .write_serial(handle.session_id(), &forged, Bytes::from_static(b"forged"))
        .await
        .unwrap_err();
    assert_eq!(error.name().as_str(), "runtime.lease.invalid_token");

    let error = runtime
        .write_serial(
            SessionId::parse("session:missing").unwrap(),
            &lease,
            Bytes::from_static(b"missing"),
        )
        .await
        .unwrap_err();
    assert_eq!(error.name().as_str(), "runtime.session.not_found");

    let error = handle.read(1).await.unwrap_err();
    assert_eq!(error.name().as_str(), "runtime.transport.timeout");
    handle.close().await.unwrap();
}
