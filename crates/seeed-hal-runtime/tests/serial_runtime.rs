use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Duration;

use bytes::Bytes;
use seeed_hal_core::{
    ErrorCategory, HalError, HalResult, IdentityQuality, LeaseId, LeaseMode, LeaseToken, OwnerId,
    ResourceDescriptor, ResourceId, ResourceSelector, SessionId, TransportKind,
};
use seeed_hal_runtime::HalRuntime;
use seeed_hal_serial::{ControlLines, SerialAdapter, SerialConfig, SerialSession};
use seeed_hal_testkit::VirtualSerialAdapter;
use tokio::sync::{Notify, Semaphore};

fn owner(value: &str) -> OwnerId {
    OwnerId::parse(value).unwrap()
}

#[derive(Clone)]
struct OpenGateAdapter {
    inner: VirtualSerialAdapter,
    open_started: Arc<Notify>,
    open_permits: Arc<Semaphore>,
}

impl OpenGateAdapter {
    fn new(resource_id: &str) -> Self {
        Self {
            inner: VirtualSerialAdapter::loopback(resource_id),
            open_started: Arc::new(Notify::new()),
            open_permits: Arc::new(Semaphore::new(0)),
        }
    }

    async fn wait_until_open_started(&self) {
        self.open_started.notified().await;
    }

    fn allow_one_open(&self) {
        self.open_permits.add_permits(1);
    }
}

#[async_trait::async_trait]
impl SerialAdapter for OpenGateAdapter {
    fn adapter_name(&self) -> &'static str {
        "virtual.serial.open-gate"
    }

    async fn enumerate(&self) -> HalResult<Vec<ResourceDescriptor>> {
        self.inner.enumerate().await
    }

    async fn open(
        &self,
        selector: &ResourceSelector,
        config: SerialConfig,
    ) -> HalResult<Box<dyn SerialSession>> {
        self.open_started.notify_one();
        self.open_permits
            .acquire()
            .await
            .expect("the test open gate remains available")
            .forget();
        self.inner.open(selector, config).await
    }
}

#[derive(Clone)]
struct FirstCloseHangsAdapter {
    inner: VirtualSerialAdapter,
    close_attempts: Arc<AtomicUsize>,
    close_started: Arc<Notify>,
}

impl FirstCloseHangsAdapter {
    fn new(resource_id: &str) -> Self {
        Self {
            inner: VirtualSerialAdapter::loopback(resource_id),
            close_attempts: Arc::new(AtomicUsize::new(0)),
            close_started: Arc::new(Notify::new()),
        }
    }

    async fn wait_until_close_started(&self) {
        self.close_started.notified().await;
    }
}

#[async_trait::async_trait]
impl SerialAdapter for FirstCloseHangsAdapter {
    fn adapter_name(&self) -> &'static str {
        "virtual.serial.first-close-hangs"
    }

    async fn enumerate(&self) -> HalResult<Vec<ResourceDescriptor>> {
        self.inner.enumerate().await
    }

    async fn open(
        &self,
        selector: &ResourceSelector,
        config: SerialConfig,
    ) -> HalResult<Box<dyn SerialSession>> {
        let inner = self.inner.open(selector, config).await?;
        Ok(Box::new(FirstCloseHangsSession {
            inner,
            close_attempts: self.close_attempts.clone(),
            close_started: self.close_started.clone(),
        }))
    }
}

struct FirstCloseHangsSession {
    inner: Box<dyn SerialSession>,
    close_attempts: Arc<AtomicUsize>,
    close_started: Arc<Notify>,
}

#[derive(Clone)]
struct FirstReadBlocksAdapter {
    inner: VirtualSerialAdapter,
    read_started: Arc<Notify>,
    release_read: Arc<Notify>,
}

impl FirstReadBlocksAdapter {
    fn new(resource_id: &str) -> Self {
        Self {
            inner: VirtualSerialAdapter::loopback(resource_id),
            read_started: Arc::new(Notify::new()),
            release_read: Arc::new(Notify::new()),
        }
    }

    async fn wait_until_read_started(&self) {
        self.read_started.notified().await;
    }
}

#[async_trait::async_trait]
impl SerialAdapter for FirstReadBlocksAdapter {
    fn adapter_name(&self) -> &'static str {
        "virtual.serial.first-read-blocks"
    }

    async fn enumerate(&self) -> HalResult<Vec<ResourceDescriptor>> {
        self.inner.enumerate().await
    }

    async fn open(
        &self,
        selector: &ResourceSelector,
        config: SerialConfig,
    ) -> HalResult<Box<dyn SerialSession>> {
        let inner = self.inner.open(selector, config).await?;
        Ok(Box::new(FirstReadBlocksSession {
            inner,
            read_started: self.read_started.clone(),
            release_read: self.release_read.clone(),
        }))
    }
}

struct FirstReadBlocksSession {
    inner: Box<dyn SerialSession>,
    read_started: Arc<Notify>,
    release_read: Arc<Notify>,
}

#[derive(Clone)]
struct FailNextOpenAdapter {
    inner: VirtualSerialAdapter,
    fail_next: Arc<AtomicBool>,
}

impl FailNextOpenAdapter {
    fn new(resource_id: &str) -> Self {
        Self {
            inner: VirtualSerialAdapter::loopback(resource_id),
            fail_next: Arc::new(AtomicBool::new(false)),
        }
    }

    fn fail_next(&self) {
        self.fail_next.store(true, Ordering::Release);
    }
}

#[async_trait::async_trait]
impl SerialAdapter for FailNextOpenAdapter {
    fn adapter_name(&self) -> &'static str {
        "virtual.serial.fail-next-open"
    }

    async fn enumerate(&self) -> HalResult<Vec<ResourceDescriptor>> {
        self.inner.enumerate().await
    }

    async fn open(
        &self,
        selector: &ResourceSelector,
        config: SerialConfig,
    ) -> HalResult<Box<dyn SerialSession>> {
        if self.fail_next.swap(false, Ordering::AcqRel) {
            return Err(HalError::new(
                "runtime.transport.unavailable",
                ErrorCategory::Unavailable,
                "serial.open",
                true,
                "injected open failure",
            )?);
        }
        self.inner.open(selector, config).await
    }
}

#[async_trait::async_trait]
impl SerialSession for FirstReadBlocksSession {
    fn descriptor(&self) -> &ResourceDescriptor {
        self.inner.descriptor()
    }

    async fn read(&mut self, max_bytes: usize) -> HalResult<Bytes> {
        self.read_started.notify_one();
        self.release_read.notified().await;
        self.inner.read(max_bytes).await
    }

    async fn write_all(&mut self, bytes: &[u8]) -> HalResult<()> {
        self.inner.write_all(bytes).await
    }

    async fn flush(&mut self) -> HalResult<()> {
        self.inner.flush().await
    }

    async fn set_control_lines(&mut self, lines: ControlLines) -> HalResult<()> {
        self.inner.set_control_lines(lines).await
    }

    async fn close(&mut self) -> HalResult<()> {
        self.inner.close().await
    }
}

#[async_trait::async_trait]
impl SerialSession for FirstCloseHangsSession {
    fn descriptor(&self) -> &ResourceDescriptor {
        self.inner.descriptor()
    }

    async fn read(&mut self, max_bytes: usize) -> HalResult<Bytes> {
        self.inner.read(max_bytes).await
    }

    async fn write_all(&mut self, bytes: &[u8]) -> HalResult<()> {
        self.inner.write_all(bytes).await
    }

    async fn flush(&mut self) -> HalResult<()> {
        self.inner.flush().await
    }

    async fn set_control_lines(&mut self, lines: ControlLines) -> HalResult<()> {
        self.inner.set_control_lines(lines).await
    }

    async fn close(&mut self) -> HalResult<()> {
        self.close_started.notify_one();
        if self.close_attempts.fetch_add(1, Ordering::SeqCst) == 0 {
            std::future::pending().await
        } else {
            self.inner.close().await
        }
    }
}

#[tokio::test(start_paused = true)]
async fn serial_close_uses_a_two_second_default_deadline() {
    let adapter = FirstCloseHangsAdapter::new("serial:virtual:default-close-deadline");
    let runtime = HalRuntime::builder()
        .serial_adapter(adapter.clone())
        .build();
    let descriptor = runtime.enumerate_serial().await.unwrap().remove(0);
    let handle = runtime
        .open_serial(
            owner("client-a"),
            descriptor.selector(),
            SerialConfig::default(),
        )
        .await
        .unwrap();
    let close = tokio::spawn(handle.close());
    adapter.wait_until_close_started().await;

    tokio::time::advance(Duration::from_millis(1_999)).await;
    tokio::task::yield_now().await;
    assert!(!close.is_finished());

    tokio::time::advance(Duration::from_millis(1)).await;
    tokio::task::yield_now().await;
    if !close.is_finished() {
        close.abort();
        panic!("the default serial close deadline must expire at two seconds");
    }
    let error = match close.await.unwrap() {
        Ok(()) => panic!("a stuck close must report its deadline"),
        Err(error) => error,
    };
    assert_eq!(error.name().as_str(), "runtime.session.close_timeout");

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

#[tokio::test(start_paused = true)]
async fn serial_close_deadline_is_runtime_configurable() {
    let adapter = FirstCloseHangsAdapter::new("serial:virtual:custom-close-deadline");
    let runtime = HalRuntime::builder()
        .serial_adapter(adapter.clone())
        .serial_close_timeout(Duration::from_millis(25))
        .build();
    let descriptor = runtime.enumerate_serial().await.unwrap().remove(0);
    let handle = runtime
        .open_serial(
            owner("client-a"),
            descriptor.selector(),
            SerialConfig::default(),
        )
        .await
        .unwrap();
    let close = tokio::spawn(handle.close());
    adapter.wait_until_close_started().await;

    tokio::time::advance(Duration::from_millis(24)).await;
    tokio::task::yield_now().await;
    assert!(!close.is_finished());

    tokio::time::advance(Duration::from_millis(1)).await;
    let error = match close.await.unwrap() {
        Ok(()) => panic!("a stuck close must report the configured deadline"),
        Err(error) => error,
    };
    assert_eq!(error.name().as_str(), "runtime.session.close_timeout");
}

#[tokio::test(start_paused = true)]
async fn owner_revoke_reports_close_timeout_and_releases_the_resource() {
    let adapter = FirstCloseHangsAdapter::new("serial:virtual:revoke-close-deadline");
    let runtime = HalRuntime::builder()
        .serial_adapter(adapter.clone())
        .serial_close_timeout(Duration::from_millis(25))
        .build();
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
    let revoking_runtime = runtime.clone();
    let revoke = tokio::spawn(async move { revoking_runtime.revoke_owner(&first_owner).await });
    adapter.wait_until_close_started().await;

    tokio::time::advance(Duration::from_millis(25)).await;
    let error = revoke.await.unwrap().unwrap_err();
    assert_eq!(error.name().as_str(), "runtime.session.close_timeout");

    let second = runtime
        .open_serial(
            owner("client-b"),
            descriptor.selector(),
            SerialConfig::default(),
        )
        .await
        .unwrap();
    second.close().await.unwrap();
    drop(first);
}

#[tokio::test]
async fn cancelling_an_in_progress_open_releases_its_reservation() {
    let adapter = OpenGateAdapter::new("serial:virtual:cancel-open");
    let runtime = HalRuntime::builder()
        .serial_adapter(adapter.clone())
        .build();
    let descriptor = runtime.enumerate_serial().await.unwrap().remove(0);
    let opening_runtime = runtime.clone();
    let opening_selector = descriptor.selector();
    let opening = tokio::spawn(async move {
        opening_runtime
            .open_serial(owner("client-a"), opening_selector, SerialConfig::default())
            .await
    });
    adapter.wait_until_open_started().await;

    opening.abort();
    match opening.await {
        Err(error) => assert!(error.is_cancelled()),
        Ok(_) => panic!("aborted open task must be cancelled"),
    }

    adapter.allow_one_open();
    let replacement = tokio::time::timeout(
        Duration::from_millis(250),
        runtime.open_serial(
            owner("client-b"),
            descriptor.selector(),
            SerialConfig::default(),
        ),
    )
    .await
    .expect("cancelled open cleanup must not hang")
    .unwrap();
    assert_eq!(replacement.lease_token().generation(), 1);
    assert_eq!(runtime.retained_generation_count().await, 1);
    replacement.close().await.unwrap();
}

#[tokio::test]
async fn thousands_of_unique_failed_opens_do_not_retain_generation_entries() {
    let runtime = HalRuntime::builder()
        .serial_adapter(VirtualSerialAdapter::loopback("serial:virtual:present"))
        .build();

    for index in 0..4_096 {
        let selector = ResourceSelector::exact(
            ResourceId::parse(format!("serial:missing:{index}")).unwrap(),
            IdentityQuality::Weak,
            TransportKind::Serial,
        );
        let error = match runtime
            .open_serial(owner("client-a"), selector, SerialConfig::default())
            .await
        {
            Ok(_) => panic!("missing selector must fail"),
            Err(error) => error,
        };
        assert_eq!(error.name().as_str(), "runtime.resource.not_found");
    }

    assert_eq!(runtime.retained_generation_count().await, 0);
}

#[tokio::test]
async fn failed_reopen_after_exposure_does_not_erase_or_skip_the_last_generation() {
    let adapter = FailNextOpenAdapter::new("serial:virtual:reopen-failure");
    let runtime = HalRuntime::builder()
        .serial_adapter(adapter.clone())
        .build();
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
    first.close().await.unwrap();

    adapter.fail_next();
    let error = match runtime
        .open_serial(
            owner("client-b"),
            descriptor.selector(),
            SerialConfig::default(),
        )
        .await
    {
        Ok(_) => panic!("injected open failure must fail"),
        Err(error) => error,
    };
    assert_eq!(error.name().as_str(), "runtime.transport.unavailable");
    assert_eq!(runtime.retained_generation_count().await, 1);

    let reopened = runtime
        .open_serial(
            owner("client-c"),
            descriptor.selector(),
            SerialConfig::default(),
        )
        .await
        .unwrap();
    assert_eq!(reopened.lease_token().generation(), first_generation + 1);
    reopened.close().await.unwrap();
}

#[tokio::test]
async fn revoking_an_owner_cancels_its_in_progress_open() {
    let adapter = OpenGateAdapter::new("serial:virtual:revoke-open");
    let runtime = HalRuntime::builder()
        .serial_adapter(adapter.clone())
        .build();
    let descriptor = runtime.enumerate_serial().await.unwrap().remove(0);
    let first_owner = owner("client-a");
    let opening_runtime = runtime.clone();
    let opening_owner = first_owner.clone();
    let opening_selector = descriptor.selector();
    let opening = tokio::spawn(async move {
        opening_runtime
            .open_serial(opening_owner, opening_selector, SerialConfig::default())
            .await
    });
    adapter.wait_until_open_started().await;

    tokio::time::timeout(
        Duration::from_millis(250),
        runtime.revoke_owner(&first_owner),
    )
    .await
    .expect("owner revocation must not wait forever for adapter open")
    .unwrap();
    let error = match opening.await.unwrap() {
        Ok(_) => panic!("revoked in-progress open must not produce a handle"),
        Err(error) => error,
    };
    assert_eq!(error.name().as_str(), "runtime.session.closed");

    adapter.allow_one_open();
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

#[tokio::test(start_paused = true)]
async fn operation_queue_admits_exactly_64_waiters_then_rejects_the_next() {
    let adapter = FirstReadBlocksAdapter::new("serial:virtual:runtime-queue");
    let runtime = Arc::new(
        HalRuntime::builder()
            .serial_adapter(adapter.clone())
            .build(),
    );
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
    let in_flight_runtime = Arc::clone(&runtime);
    let in_flight_session = session_id.clone();
    let in_flight_lease = lease.clone();
    let in_flight = tokio::spawn(async move {
        in_flight_runtime
            .read_serial(in_flight_session, &in_flight_lease, 1)
            .await
    });
    adapter.wait_until_read_started().await;

    let (started_tx, mut started_rx) = tokio::sync::mpsc::unbounded_channel();
    let mut queued = Vec::new();

    for slot in 0..64 {
        let runtime = Arc::clone(&runtime);
        let session_id = session_id.clone();
        let lease = lease.clone();
        let started_tx = started_tx.clone();
        queued.push(tokio::spawn(async move {
            started_tx.send(slot).unwrap();
            runtime.read_serial(session_id, &lease, 1).await
        }));
        assert_eq!(started_rx.recv().await.unwrap(), slot);
        tokio::task::yield_now().await;
    }

    let overflow = tokio::time::timeout(
        Duration::from_millis(1),
        runtime.read_serial(session_id, &lease, 1),
    )
    .await
    .expect("the 65th queued request must reject immediately")
    .unwrap_err();
    assert_eq!(overflow.name().as_str(), "runtime.queue.full");

    handle.close().await.unwrap();
    let error = in_flight.await.unwrap().unwrap_err();
    assert_eq!(error.name().as_str(), "runtime.session.closed");
    for task in queued {
        let error = task.await.unwrap().unwrap_err();
        assert_eq!(error.name().as_str(), "runtime.session.closed");
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
async fn close_replay_window_retains_256_sessions_then_evicts_the_oldest() {
    let adapter = VirtualSerialAdapter::loopback("serial:virtual:close-replay-window");
    let runtime = HalRuntime::builder().serial_adapter(adapter).build();
    let descriptor = runtime.enumerate_serial().await.unwrap().remove(0);
    let mut oldest = None;

    for _ in 0..256 {
        let handle = runtime
            .open_serial(
                owner("client-a"),
                descriptor.selector(),
                SerialConfig::default(),
            )
            .await
            .unwrap();
        if oldest.is_none() {
            oldest = Some((handle.session_id(), handle.lease_token().clone()));
        }
        handle.close().await.unwrap();
    }

    let (oldest_session, oldest_lease) = oldest.unwrap();
    runtime
        .close_serial(oldest_session.clone(), &oldest_lease)
        .await
        .unwrap();

    runtime
        .open_serial(
            owner("client-a"),
            descriptor.selector(),
            SerialConfig::default(),
        )
        .await
        .unwrap()
        .close()
        .await
        .unwrap();

    let error = runtime
        .close_serial(oldest_session, &oldest_lease)
        .await
        .unwrap_err();
    assert_eq!(error.name().as_str(), "runtime.session.not_found");
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
