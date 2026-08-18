use bytes::Bytes;
use seeed_hal_usb::*;

#[test]
fn transfers_enforce_kind_endpoint_and_payload_bounds() {
    assert!(UsbTransfer::control_out(0x40, 1, 0, 0, Bytes::new()).is_ok());
    assert!(UsbTransfer::bulk_out(1, Bytes::from(vec![0; MAX_USB_TRANSFER_BYTES])).is_ok());
    assert!(UsbTransfer::bulk_out(0, Bytes::new()).is_err());
    assert!(UsbTransfer::bulk_out(1, Bytes::from(vec![0; MAX_USB_TRANSFER_BYTES + 1])).is_err());
    assert!(UsbTransfer::interrupt_in(0x81, MAX_USB_TRANSFER_BYTES).is_ok());
    assert!(UsbTransfer::interrupt_in(0x01, 1).is_err());
}

#[test]
fn interface_claim_and_capabilities_are_hardware_class_stable() {
    assert!(UsbInterfaceClaim::new(0).is_ok());
    assert!(UsbInterfaceClaim::new(MAX_USB_INTERFACE_NUMBER + 1).is_err());
    assert_eq!(usb_control_capability().as_str(), USB_CONTROL_CAPABILITY);
    assert_eq!(usb_bulk_capability().as_str(), USB_BULK_CAPABILITY);
    assert_eq!(
        usb_interrupt_capability().as_str(),
        USB_INTERRUPT_CAPABILITY
    );
    assert_eq!(MAX_USB_PENDING_TRANSFERS, 64);
}
