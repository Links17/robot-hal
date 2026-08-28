use robot_hal_adapter_nusb::identity::{UsbDeviceMetadata, identity_from_metadata};
use robot_hal_usb::IdentityQuality;

#[test]
fn serial_number_produces_strong_usb_identity() {
    let metadata = UsbDeviceMetadata {
        vendor_id: 0x2886,
        product_id: 0x802f,
        serial_number: Some("SN/42".to_owned()),
        topology: "1-2.3".to_owned(),
    };

    let identity = identity_from_metadata(&metadata).expect("metadata is valid");

    assert_eq!(identity.id.as_str(), "usb:device:2886:802f:SN%2F42");
    assert_eq!(identity.quality, IdentityQuality::Strong);
}

#[test]
fn topology_is_weak_identity_and_endpoint_is_separate() {
    let metadata = UsbDeviceMetadata {
        vendor_id: 0x2886,
        product_id: 0x802f,
        serial_number: None,
        topology: "1-2.3".to_owned(),
    };

    let identity = identity_from_metadata(&metadata).expect("metadata is valid");

    assert_eq!(identity.id.as_str(), "usb:topology:1-2.3");
    assert_eq!(identity.quality, IdentityQuality::Weak);
}

#[test]
fn bus_address_is_never_used_as_an_identity() {
    let first = UsbDeviceMetadata {
        vendor_id: 0x2886,
        product_id: 0x802f,
        serial_number: None,
        topology: "1-2.3".to_owned(),
    };
    let second = UsbDeviceMetadata {
        topology: "1-2.4".to_owned(),
        ..first.clone()
    };

    assert_ne!(
        identity_from_metadata(&first).unwrap().id,
        identity_from_metadata(&second).unwrap().id
    );
}
