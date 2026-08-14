use seeed_hal_adapter_serialport::identity::{
    UsbPortMetadata, identity_from_endpoint, identity_from_usb_metadata,
};
use seeed_hal_serial::IdentityQuality;

#[test]
fn usb_serial_number_produces_strong_identity() {
    let metadata = UsbPortMetadata {
        vid: 0x10c4,
        pid: 0xea60,
        serial_number: Some("ABC123".into()),
        manufacturer: Some("Silicon Labs".into()),
        product: Some("CP210x".into()),
    };

    let identity = identity_from_usb_metadata("COM7", &metadata).unwrap();

    assert_eq!(identity.id.as_str(), "serial:usb:10c4:ea60:ABC123");
    assert_eq!(identity.quality, IdentityQuality::Strong);
}

#[test]
fn usb_serial_number_segments_are_percent_encoded() {
    let metadata = UsbPortMetadata {
        vid: 0x10c4,
        pid: 0xea60,
        serial_number: Some("A/B C%:1".into()),
        manufacturer: None,
        product: None,
    };

    let identity = identity_from_usb_metadata("COM7", &metadata).unwrap();

    assert_eq!(
        identity.id.as_str(),
        "serial:usb:10c4:ea60:A%2FB%20C%25%3A1"
    );
    assert_eq!(identity.quality, IdentityQuality::Strong);
}

#[test]
fn usb_model_metadata_without_serial_falls_back_to_weak_endpoint_identity() {
    let metadata = UsbPortMetadata {
        vid: 0x10c4,
        pid: 0xea60,
        serial_number: None,
        manufacturer: Some("Silicon Labs".into()),
        product: Some("CP210x".into()),
    };

    let identity = identity_from_usb_metadata("/dev/cu.usbserial A%1", &metadata).unwrap();

    assert_eq!(
        identity.id.as_str(),
        "serial:endpoint:%2Fdev%2Fcu.usbserial%20A%251"
    );
    assert_eq!(identity.quality, IdentityQuality::Weak);
}

#[test]
fn same_model_usb_devices_without_serial_do_not_share_identity() {
    let metadata = UsbPortMetadata {
        vid: 0x10c4,
        pid: 0xea60,
        serial_number: None,
        manufacturer: Some("Silicon Labs".into()),
        product: Some("CP210x".into()),
    };

    let first = identity_from_usb_metadata("/dev/ttyUSB0", &metadata).unwrap();
    let second = identity_from_usb_metadata("/dev/ttyUSB1", &metadata).unwrap();

    assert_eq!(first.quality, IdentityQuality::Weak);
    assert_eq!(second.quality, IdentityQuality::Weak);
    assert_ne!(first.id, second.id);
}

#[test]
fn endpoint_only_identity_is_explicitly_weak() {
    let identity = identity_from_endpoint("/dev/ttyS0").unwrap();

    assert_eq!(identity.quality, IdentityQuality::Weak);
    assert_ne!(identity.id.as_str(), "/dev/ttyS0");
}
