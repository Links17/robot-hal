use bytes::Bytes;
use seeed_hal_core::{
    CapabilitySet, HalResult, IdentityQuality, OwnerId, ResourceDescriptor, ResourceId,
    ResourceProperties, ResourceSelector, TransportKind,
};
use seeed_hal_runtime::{HalRuntime, UsbQueueObserver};
use seeed_hal_testkit::VirtualUsbAdapter;
use seeed_hal_usb::{
    MAX_USB_TRANSFER_BYTES, UsbAdapter, UsbInterfaceClaim, UsbInterfaceSession, UsbTransfer,
    usb_bulk_capability, usb_control_capability, usb_interrupt_capability,
};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{Notify, Semaphore};

#[derive(Clone)]
struct BlockingUsbAdapter {
    descriptor: ResourceDescriptor,
    transfer_started: Arc<Notify>,
    release_transfer: Arc<Semaphore>,
}

impl BlockingUsbAdapter {
    fn new(resource_id: &str) -> Self {
        let id = ResourceId::parse(resource_id).unwrap();
        Self {
            descriptor: ResourceDescriptor::new(
                id.clone(),
                seeed_hal_core::Endpoint::new(format!("virtual://usb/{}", id.as_str())).unwrap(),
                IdentityQuality::Strong,
                TransportKind::Usb,
                ResourceProperties::default(),
                CapabilitySet::new(vec![
                    usb_control_capability(),
                    usb_bulk_capability(),
                    usb_interrupt_capability(),
                ]),
            ),
            transfer_started: Arc::new(Notify::new()),
            release_transfer: Arc::new(Semaphore::new(0)),
        }
    }
}

#[async_trait::async_trait]
impl UsbAdapter for BlockingUsbAdapter {
    fn adapter_name(&self) -> &'static str {
        "test.usb.blocking"
    }

    async fn enumerate(&self) -> HalResult<Vec<ResourceDescriptor>> {
        Ok(vec![self.descriptor.clone()])
    }

    async fn open(
        &self,
        _: &ResourceSelector,
        claim: UsbInterfaceClaim,
    ) -> HalResult<Box<dyn UsbInterfaceSession>> {
        Ok(Box::new(BlockingUsbSession {
            descriptor: self.descriptor.clone(),
            claim,
            transfer_started: Arc::clone(&self.transfer_started),
            release_transfer: Arc::clone(&self.release_transfer),
        }))
    }
}

struct BlockingUsbSession {
    descriptor: ResourceDescriptor,
    claim: UsbInterfaceClaim,
    transfer_started: Arc<Notify>,
    release_transfer: Arc<Semaphore>,
}

#[async_trait::async_trait]
impl UsbInterfaceSession for BlockingUsbSession {
    fn descriptor(&self) -> &ResourceDescriptor {
        &self.descriptor
    }

    fn interface_claim(&self) -> UsbInterfaceClaim {
        self.claim
    }

    async fn transfer(&mut self, _: UsbTransfer, _: Duration) -> HalResult<Bytes> {
        self.transfer_started.notify_waiters();
        self.release_transfer.acquire().await.unwrap().forget();
        Ok(Bytes::new())
    }

    async fn close(&mut self) -> HalResult<()> {
        Ok(())
    }
}

#[tokio::test]
async fn usb_runtime_fences_stale_leases_after_close_and_reopen() {
    let adapter = VirtualUsbAdapter::loopback("usb:runtime:fencing");
    let runtime = HalRuntime::builder().usb_adapter(adapter).build();
    let descriptor = runtime.enumerate_usb().await.unwrap().remove(0);
    let owner = OwnerId::parse("owner:usb-runtime").unwrap();
    let mut first = runtime
        .open_usb(owner.clone(), descriptor.selector(), 0)
        .await
        .unwrap();
    first
        .transfer(
            UsbTransfer::bulk_out(1, Bytes::from_static(b"first")).unwrap(),
            Duration::ZERO,
        )
        .await
        .unwrap();
    let stale = first.lease_token().clone();
    let session = first.session_id();
    first.close().await.unwrap();
    let mut second = runtime
        .open_usb(owner, descriptor.selector(), 0)
        .await
        .unwrap();
    assert_eq!(
        runtime
            .usb_transfer(
                session,
                &stale,
                UsbTransfer::bulk_in(0x81, MAX_USB_TRANSFER_BYTES).unwrap(),
                Duration::ZERO
            )
            .await
            .unwrap_err()
            .name()
            .as_str(),
        "runtime.lease.stale_generation"
    );
    second.close().await.unwrap();
}

#[tokio::test]
async fn usb_owner_revoke_releases_claim_for_another_owner() {
    let adapter = VirtualUsbAdapter::loopback("usb:runtime:revoke");
    let runtime = HalRuntime::builder().usb_adapter(adapter).build();
    let descriptor = runtime.enumerate_usb().await.unwrap().remove(0);
    let owner = OwnerId::parse("owner:usb-revoked").unwrap();
    let handle = runtime
        .open_usb(owner.clone(), descriptor.selector(), 0)
        .await
        .unwrap();
    runtime.revoke_owner(&owner).await.unwrap();
    assert_eq!(
        handle
            .transfer(UsbTransfer::bulk_in(0x81, 1).unwrap(), Duration::ZERO)
            .await
            .unwrap_err()
            .name()
            .as_str(),
        "runtime.session.closed"
    );
    let mut replacement = runtime
        .open_usb(
            OwnerId::parse("owner:usb-new").unwrap(),
            descriptor.selector(),
            0,
        )
        .await
        .unwrap();
    replacement.close().await.unwrap();
}

#[tokio::test]
async fn usb_runtime_rejects_full_queue_without_waiting_for_native_io() {
    let adapter = BlockingUsbAdapter::new("usb:runtime:queue");
    let runtime = HalRuntime::builder().usb_adapter(adapter.clone()).build();
    let descriptor = runtime.enumerate_usb().await.unwrap().remove(0);
    let mut handle = runtime
        .open_usb(
            OwnerId::parse("owner:usb-queue").unwrap(),
            descriptor.selector(),
            0,
        )
        .await
        .unwrap();
    let started = adapter.transfer_started.notified();
    let first = {
        let runtime = runtime.clone();
        let session = handle.session_id();
        let lease = handle.lease_token().clone();
        tokio::spawn(async move {
            runtime
                .usb_transfer(
                    session,
                    &lease,
                    UsbTransfer::bulk_in(0x81, 1).unwrap(),
                    Duration::from_secs(1),
                )
                .await
        })
    };
    started.await;
    let mut queued = Vec::new();
    for _ in 0..64 {
        let runtime = runtime.clone();
        let session = handle.session_id();
        let lease = handle.lease_token().clone();
        queued.push(tokio::spawn(async move {
            runtime
                .usb_transfer(
                    session,
                    &lease,
                    UsbTransfer::bulk_in(0x81, 1).unwrap(),
                    Duration::from_secs(1),
                )
                .await
        }));
        tokio::task::yield_now().await;
    }
    assert_eq!(
        runtime
            .usb_transfer(
                handle.session_id(),
                handle.lease_token(),
                UsbTransfer::bulk_in(0x81, 1).unwrap(),
                Duration::from_secs(1),
            )
            .await
            .unwrap_err()
            .name()
            .as_str(),
        "runtime.queue.full"
    );
    adapter.release_transfer.add_permits(65);
    first.await.unwrap().unwrap();
    for request in queued {
        request.await.unwrap().unwrap();
    }
    handle.close().await.unwrap();
}

#[tokio::test]
async fn usb_close_times_out_while_transfer_blocks_and_keeps_claim_exclusive() {
    let adapter = BlockingUsbAdapter::new("usb:runtime:close-timeout");
    let runtime = HalRuntime::builder()
        .usb_adapter(adapter.clone())
        .usb_close_timeout(Duration::from_millis(20))
        .build();
    let descriptor = runtime.enumerate_usb().await.unwrap().remove(0);
    let owner = OwnerId::parse("owner:usb-close-timeout").unwrap();
    let mut handle = runtime
        .open_usb(owner.clone(), descriptor.selector(), 0)
        .await
        .unwrap();
    let started = adapter.transfer_started.notified();
    let transfer = {
        let runtime = runtime.clone();
        let session = handle.session_id();
        let lease = handle.lease_token().clone();
        tokio::spawn(async move {
            runtime
                .usb_transfer(
                    session,
                    &lease,
                    UsbTransfer::bulk_in(0x81, 1).unwrap(),
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
        .open_usb(
            OwnerId::parse("owner:usb-reuse-before-worker-exits").unwrap(),
            descriptor.selector(),
            0,
        )
        .await;
    assert_eq!(
        reuse.err().unwrap().name().as_str(),
        "runtime.lease.conflict"
    );
    assert_eq!(
        tokio::time::timeout(Duration::from_millis(20), transfer)
            .await
            .unwrap()
            .unwrap()
            .unwrap_err()
            .name()
            .as_str(),
        "runtime.session.closed"
    );

    adapter.release_transfer.add_permits(1);
    let mut replacement = tokio::time::timeout(Duration::from_secs(1), async {
        loop {
            match runtime
                .open_usb(
                    OwnerId::parse("owner:usb-reuse-after-worker-exits").unwrap(),
                    descriptor.selector(),
                    0,
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
async fn usb_cancelled_queued_transfer_does_not_start_native_io() {
    let adapter = BlockingUsbAdapter::new("usb:runtime:cancelled-queue");
    let (queue_observer, mut queued) = UsbQueueObserver::new();
    let runtime = HalRuntime::builder()
        .usb_adapter(adapter.clone())
        .usb_queue_observer(queue_observer)
        .build();
    let descriptor = runtime.enumerate_usb().await.unwrap().remove(0);
    let mut handle = runtime
        .open_usb(
            OwnerId::parse("owner:usb-cancelled-queue").unwrap(),
            descriptor.selector(),
            0,
        )
        .await
        .unwrap();
    let started = adapter.transfer_started.notified();
    let first = {
        let runtime = runtime.clone();
        let session = handle.session_id();
        let lease = handle.lease_token().clone();
        tokio::spawn(async move {
            runtime
                .usb_transfer(
                    session,
                    &lease,
                    UsbTransfer::bulk_in(0x81, 1).unwrap(),
                    Duration::from_secs(60),
                )
                .await
        })
    };
    started.await;
    assert_eq!(
        *queued.borrow_and_update(),
        1,
        "the first blocked native transfer must be the only admitted command"
    );
    let cancelled = {
        let runtime = runtime.clone();
        let session = handle.session_id();
        let lease = handle.lease_token().clone();
        tokio::spawn(async move {
            runtime
                .usb_transfer(
                    session,
                    &lease,
                    UsbTransfer::bulk_in(0x81, 1).unwrap(),
                    Duration::from_secs(60),
                )
                .await
        })
    };
    queued
        .changed()
        .await
        .expect("USB queue observer remains connected");
    assert_eq!(
        *queued.borrow_and_update(),
        2,
        "the second transfer must enter the USB command queue before cancellation"
    );
    cancelled.abort();
    assert!(
        cancelled.await.unwrap_err().is_cancelled(),
        "the queued transfer task must finish cancellation before native I/O is released"
    );
    adapter.release_transfer.add_permits(1);
    first.await.unwrap().unwrap();
    assert!(
        tokio::time::timeout(
            Duration::from_millis(20),
            adapter.transfer_started.notified()
        )
        .await
        .is_err(),
        "a cancelled queued transfer must not reach the native session"
    );
    handle.close().await.unwrap();
}
