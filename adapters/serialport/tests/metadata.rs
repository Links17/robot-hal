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
fn endpoint_only_identity_is_explicitly_weak() {
    let identity = identity_from_endpoint("/dev/ttyS0").unwrap();

    assert_eq!(identity.quality, IdentityQuality::Weak);
    assert_ne!(identity.id.as_str(), "/dev/ttyS0");
}
