use robot_hal_core::{
    CapabilitySet, HalResult, IdentityQuality, OwnerId, ResourceDescriptor, ResourceId,
    ResourceProperties, ResourceSelector, TransportKind,
};
use robot_hal_gpio::{
    GpioAdapter, GpioBias, GpioEdgeEvent, GpioEdgeRequest, GpioLineConfig, GpioLineSession,
    gpio_edges_capability, gpio_lines_capability,
};
use robot_hal_runtime::{GpioQueueObserver, HalRuntime};
use robot_hal_testkit::VirtualGpioAdapter;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{Notify, Semaphore};

#[derive(Clone)]
struct BlockingGpioAdapter {
    descriptor: ResourceDescriptor,
    edge_started: Arc<Notify>,
    release_edge: Arc<Semaphore>,
    read_started: Arc<Notify>,
    release_read: Arc<Semaphore>,
}

impl BlockingGpioAdapter {
    fn new(resource_id: &str) -> Self {
        let id = ResourceId::parse(resource_id).unwrap();
        Self {
            descriptor: ResourceDescriptor::new(
                id.clone(),
                robot_hal_core::Endpoint::new(format!("virtual://gpio/{}", id.as_str())).unwrap(),
                IdentityQuality::Strong,
                TransportKind::Gpio,
                ResourceProperties::default(),
                CapabilitySet::new(vec![gpio_lines_capability(), gpio_edges_capability()]),
            ),
            edge_started: Arc::new(Notify::new()),
            release_edge: Arc::new(Semaphore::new(0)),
            read_started: Arc::new(Notify::new()),
            release_read: Arc::new(Semaphore::new(0)),
        }
    }
}

#[async_trait::async_trait]
impl GpioAdapter for BlockingGpioAdapter {
    fn adapter_name(&self) -> &'static str {
        "test.gpio.blocking"
    }

    async fn enumerate(&self) -> HalResult<Vec<ResourceDescriptor>> {
        Ok(vec![self.descriptor.clone()])
    }

    async fn open(
        &self,
        _: &ResourceSelector,
        lines: &[u32],
        config: GpioLineConfig,
    ) -> HalResult<Box<dyn GpioLineSession>> {
        Ok(Box::new(BlockingGpioSession {
            descriptor: self.descriptor.clone(),
            lines: lines.to_vec(),
            config,
            edge_started: Arc::clone(&self.edge_started),
            release_edge: Arc::clone(&self.release_edge),
            read_started: Arc::clone(&self.read_started),
            release_read: Arc::clone(&self.release_read),
        }))
    }
}

struct BlockingGpioSession {
    descriptor: ResourceDescriptor,
    lines: Vec<u32>,
    config: GpioLineConfig,
    edge_started: Arc<Notify>,
    release_edge: Arc<Semaphore>,
    read_started: Arc<Notify>,
    release_read: Arc<Semaphore>,
}

#[async_trait::async_trait]
impl GpioLineSession for BlockingGpioSession {
    fn descriptor(&self) -> &ResourceDescriptor {
        &self.descriptor
    }

    fn lines(&self) -> &[u32] {
        &self.lines
    }

    fn config(&self) -> GpioLineConfig {
        self.config
    }

    async fn read(&mut self) -> HalResult<Vec<bool>> {
        self.read_started.notify_waiters();
        self.release_read.acquire().await.unwrap().forget();
        Ok(vec![false; self.lines.len()])
    }

    async fn write(&mut self, _: &[bool]) -> HalResult<()> {
        Ok(())
    }

    async fn next_edge(
        &mut self,
        _: GpioEdgeRequest,
        _: Duration,
    ) -> HalResult<Option<GpioEdgeEvent>> {
        self.edge_started.notify_waiters();
        self.release_edge.acquire().await.unwrap().forget();
        Ok(None)
    }

    async fn close(&mut self) -> HalResult<()> {
        Ok(())
    }
}

#[tokio::test]
async fn gpio_runtime_releases_owner_lines_and_fences_old_generation() {
    let adapter = VirtualGpioAdapter::line_bank("gpio:runtime:fencing", 2);
    let runtime = HalRuntime::builder().gpio_adapter(adapter).build();
    let descriptor = runtime.enumerate_gpio().await.unwrap().remove(0);
    let owner = OwnerId::parse("owner:gpio-runtime").unwrap();
    let mut first = runtime
        .open_gpio(
            owner.clone(),
            descriptor.selector(),
            vec![0],
            GpioLineConfig::input(false, GpioBias::Disabled).unwrap(),
        )
        .await
        .unwrap();
    let stale = first.lease_token().clone();
    let session = first.session_id();
    first.close().await.unwrap();
    let mut second = runtime
        .open_gpio(
            owner,
            descriptor.selector(),
            vec![0],
            GpioLineConfig::input(false, GpioBias::Disabled).unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        runtime
            .gpio_read(session, &stale)
            .await
            .unwrap_err()
            .name()
            .as_str(),
        "runtime.lease.stale_generation"
    );
    second.close().await.unwrap();
}

#[tokio::test]
async fn gpio_owner_revoke_releases_lines_for_another_owner() {
    let adapter = VirtualGpioAdapter::line_bank("gpio:runtime:revoke", 1);
    let runtime = HalRuntime::builder().gpio_adapter(adapter).build();
    let descriptor = runtime.enumerate_gpio().await.unwrap().remove(0);
    let owner = OwnerId::parse("owner:gpio-revoked").unwrap();
    let handle = runtime
        .open_gpio(
            owner.clone(),
            descriptor.selector(),
            vec![0],
            GpioLineConfig::input(false, GpioBias::Disabled).unwrap(),
        )
        .await
        .unwrap();
    runtime.revoke_owner(&owner).await.unwrap();
    assert_eq!(
        handle.read().await.unwrap_err().name().as_str(),
        "runtime.session.closed"
    );
    let mut replacement = runtime
        .open_gpio(
            OwnerId::parse("owner:gpio-new").unwrap(),
            descriptor.selector(),
            vec![0],
            GpioLineConfig::input(false, GpioBias::Disabled).unwrap(),
        )
        .await
        .unwrap();
    replacement.close().await.unwrap();
}

#[tokio::test]
async fn gpio_close_times_out_while_edge_waits_and_keeps_lines_exclusive() {
    let adapter = BlockingGpioAdapter::new("gpio:runtime:close-timeout");
    let runtime = HalRuntime::builder()
        .gpio_adapter(adapter.clone())
        .gpio_close_timeout(Duration::from_millis(20))
        .build();
    let descriptor = runtime.enumerate_gpio().await.unwrap().remove(0);
    let owner = OwnerId::parse("owner:gpio-close-timeout").unwrap();
    let mut handle = runtime
        .open_gpio(
            owner.clone(),
            descriptor.selector(),
            vec![0],
            GpioLineConfig::input(false, GpioBias::Disabled).unwrap(),
        )
        .await
        .unwrap();
    let started = adapter.edge_started.notified();
    let edge = {
        let runtime = runtime.clone();
        let session = handle.session_id();
        let lease = handle.lease_token().clone();
        tokio::spawn(async move {
            runtime
                .gpio_next_edge(
                    session,
                    &lease,
                    GpioEdgeRequest::new(robot_hal_gpio::EdgeMask::BOTH, 1).unwrap(),
                    Duration::from_secs(60),
                )
                .await
        })
    };
    started.await;

    assert_eq!(
        handle.close().await.unwrap_err().name().as_str(),
        "runtime.session.close_timeout"
    );
    let reuse = runtime
        .open_gpio(
            OwnerId::parse("owner:gpio-reuse-before-worker-exits").unwrap(),
            descriptor.selector(),
            vec![0],
            GpioLineConfig::input(false, GpioBias::Disabled).unwrap(),
        )
        .await;
    assert_eq!(
        reuse.err().unwrap().name().as_str(),
        "runtime.lease.conflict"
    );
    assert_eq!(
        tokio::time::timeout(Duration::from_millis(20), edge)
            .await
            .unwrap()
            .unwrap()
            .unwrap_err()
            .name()
            .as_str(),
        "runtime.session.closed"
    );

    adapter.release_edge.add_permits(1);
    let mut replacement = tokio::time::timeout(Duration::from_secs(1), async {
        loop {
            match runtime
                .open_gpio(
                    OwnerId::parse("owner:gpio-reuse-after-worker-exits").unwrap(),
                    descriptor.selector(),
                    vec![0],
                    GpioLineConfig::input(false, GpioBias::Disabled).unwrap(),
                )
                .await
            {
                Ok(handle) => return handle,
                Err(error) if error.name().as_str() == "runtime.lease.conflict" => {
                    tokio::task::yield_now().await;
                }
                Err(error) => panic!("unexpected open error: {error:?}"),
            }
        }
    })
    .await
    .unwrap();
    replacement.close().await.unwrap();
}

#[tokio::test]
async fn gpio_cancelled_queued_read_does_not_start_native_io() {
    let adapter = BlockingGpioAdapter::new("gpio:runtime:cancelled-queue");
    let (queue_observer, mut queued) = GpioQueueObserver::new();
    let runtime = HalRuntime::builder()
        .gpio_adapter(adapter.clone())
        .gpio_queue_observer(queue_observer)
        .build();
    let descriptor = runtime.enumerate_gpio().await.unwrap().remove(0);
    let mut handle = runtime
        .open_gpio(
            OwnerId::parse("owner:gpio-cancelled-queue").unwrap(),
            descriptor.selector(),
            vec![0],
            GpioLineConfig::input(false, GpioBias::Disabled).unwrap(),
        )
        .await
        .unwrap();
    let started = adapter.read_started.notified();
    let first = {
        let runtime = runtime.clone();
        let session = handle.session_id();
        let lease = handle.lease_token().clone();
        tokio::spawn(async move { runtime.gpio_read(session, &lease).await })
    };
    started.await;
    assert_eq!(
        *queued.borrow_and_update(),
        1,
        "the first blocked native read must be the only admitted command"
    );
    let cancelled = {
        let runtime = runtime.clone();
        let session = handle.session_id();
        let lease = handle.lease_token().clone();
        tokio::spawn(async move { runtime.gpio_read(session, &lease).await })
    };
    queued
        .changed()
        .await
        .expect("GPIO queue observer remains connected");
    assert_eq!(
        *queued.borrow_and_update(),
        2,
        "the second read must enter the GPIO command queue before cancellation"
    );
    cancelled.abort();
    assert!(
        cancelled.await.unwrap_err().is_cancelled(),
        "the queued read task must finish cancellation before native I/O is released"
    );
    adapter.release_read.add_permits(1);
    first.await.unwrap().unwrap();
    assert!(
        tokio::time::timeout(Duration::from_millis(20), adapter.read_started.notified())
            .await
            .is_err(),
        "a cancelled queued read must not reach the native session"
    );
    handle.close().await.unwrap();
}

#[tokio::test]
async fn gpio_rejects_full_queue_without_waiting_for_native_io() {
    let adapter = BlockingGpioAdapter::new("gpio:runtime:queue");
    let runtime = HalRuntime::builder().gpio_adapter(adapter.clone()).build();
    let descriptor = runtime.enumerate_gpio().await.unwrap().remove(0);
    let mut handle = runtime
        .open_gpio(
            OwnerId::parse("owner:gpio-queue").unwrap(),
            descriptor.selector(),
            vec![0],
            GpioLineConfig::input(false, GpioBias::Disabled).unwrap(),
        )
        .await
        .unwrap();
    let started = adapter.read_started.notified();
    let first = {
        let runtime = runtime.clone();
        let session = handle.session_id();
        let lease = handle.lease_token().clone();
        tokio::spawn(async move { runtime.gpio_read(session, &lease).await })
    };
    started.await;
    let mut queued = Vec::new();
    for _ in 0..64 {
        let runtime = runtime.clone();
        let session = handle.session_id();
        let lease = handle.lease_token().clone();
        queued.push(tokio::spawn(async move {
            runtime.gpio_read(session, &lease).await
        }));
        tokio::task::yield_now().await;
    }
    assert_eq!(
        runtime
            .gpio_read(handle.session_id(), handle.lease_token())
            .await
            .unwrap_err()
            .name()
            .as_str(),
        "runtime.queue.full"
    );
    adapter.release_read.add_permits(65);
    first.await.unwrap().unwrap();
    for request in queued {
        request.await.unwrap().unwrap();
    }
    handle.close().await.unwrap();
}
