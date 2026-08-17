use bytes::Bytes;
use seeed_hal_core::OwnerId;
use seeed_hal_runtime::HalRuntime;
use seeed_hal_testkit::VirtualUsbAdapter;
use seeed_hal_usb::{MAX_USB_TRANSFER_BYTES, UsbTransfer};
use std::time::Duration;

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
