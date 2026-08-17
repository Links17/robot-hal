use bytes::Bytes;
use seeed_hal_testkit::VirtualUsbAdapter;
use seeed_hal_usb::{UsbAdapter, UsbInterfaceClaim, UsbTransfer};
use std::time::Duration;

#[tokio::test]
async fn virtual_usb_claims_exclusively_and_loopbacks_all_transfer_classes() {
    let adapter = VirtualUsbAdapter::loopback("usb:virtual:conformance");
    let descriptor = adapter.enumerate().await.unwrap().remove(0);
    let mut session = adapter
        .open(&descriptor.selector(), UsbInterfaceClaim::new(0).unwrap())
        .await
        .unwrap();
    assert_eq!(
        session
            .transfer(
                UsbTransfer::control_out(0x40, 1, 0, 0, Bytes::from_static(b"control")).unwrap(),
                Duration::ZERO
            )
            .await
            .unwrap(),
        Bytes::new()
    );
    assert_eq!(
        session
            .transfer(
                UsbTransfer::control_in(0xc0, 1, 0, 0, 16).unwrap(),
                Duration::ZERO
            )
            .await
            .unwrap(),
        Bytes::from_static(b"control")
    );
    assert_eq!(
        session
            .transfer(
                UsbTransfer::bulk_out(1, Bytes::from_static(b"bulk")).unwrap(),
                Duration::ZERO
            )
            .await
            .unwrap(),
        Bytes::new()
    );
    assert_eq!(
        session
            .transfer(UsbTransfer::bulk_in(0x81, 16).unwrap(), Duration::ZERO)
            .await
            .unwrap(),
        Bytes::from_static(b"bulk")
    );
    assert_eq!(
        session
            .transfer(
                UsbTransfer::interrupt_out(2, Bytes::from_static(b"interrupt")).unwrap(),
                Duration::ZERO
            )
            .await
            .unwrap(),
        Bytes::new()
    );
    assert_eq!(
        session
            .transfer(UsbTransfer::interrupt_in(0x82, 16).unwrap(), Duration::ZERO)
            .await
            .unwrap(),
        Bytes::from_static(b"interrupt")
    );
    let error = match adapter
        .open(&descriptor.selector(), UsbInterfaceClaim::new(0).unwrap())
        .await
    {
        Ok(_) => panic!("duplicate USB interface claim must fail"),
        Err(error) => error,
    };
    assert_eq!(error.name().as_str(), "runtime.adapter.conflict");
    session.close().await.unwrap();
}
