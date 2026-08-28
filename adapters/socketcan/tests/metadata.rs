use robot_hal_adapter_socketcan::identity::{CanInterfaceMetadata, identity_from_metadata};
use robot_hal_can::IdentityQuality;

#[test]
fn serial_identity_is_strong_and_percent_encoded() {
    let metadata = CanInterfaceMetadata {
        interface: "can0".to_owned(),
        serial: Some("SER/IAL 1%".to_owned()),
        stable_path: None,
        topology: None,
        virtual_interface: false,
    };

    let identity = identity_from_metadata(&metadata).unwrap();

    assert_eq!(identity.id.as_str(), "can:serial:SER%2FIAL%201%25");
    assert_eq!(identity.quality, IdentityQuality::Strong);
}

#[test]
fn stable_path_identity_is_medium_without_serial() {
    let metadata = CanInterfaceMetadata {
        interface: "can0".to_owned(),
        serial: None,
        stable_path: Some("/devices/pci 0000:01/0000:01:00.0".to_owned()),
        topology: None,
        virtual_interface: false,
    };

    let identity = identity_from_metadata(&metadata).unwrap();

    assert_eq!(
        identity.id.as_str(),
        "can:path:%2Fdevices%2Fpci%200000%3A01%2F0000%3A01%3A00.0"
    );
    assert_eq!(identity.quality, IdentityQuality::Medium);
}

#[test]
fn virtual_interfaces_are_always_weak_endpoint_identities() {
    let metadata = CanInterfaceMetadata {
        interface: "vcan 0".to_owned(),
        serial: Some("should-not-be-used".to_owned()),
        stable_path: Some("/sys/devices/virtual/net/vcan 0".to_owned()),
        topology: Some("virtual".to_owned()),
        virtual_interface: true,
    };

    let identity = identity_from_metadata(&metadata).unwrap();

    assert_eq!(identity.id.as_str(), "can:endpoint:vcan%200");
    assert_eq!(identity.quality, IdentityQuality::Weak);
}

#[test]
fn endpoint_identity_is_distinct_for_transient_interface_names() {
    let first = CanInterfaceMetadata {
        interface: "can0".to_owned(),
        serial: None,
        stable_path: None,
        topology: None,
        virtual_interface: false,
    };
    let second = CanInterfaceMetadata {
        interface: "can1".to_owned(),
        ..first.clone()
    };

    let first_identity = identity_from_metadata(&first).unwrap();
    let second_identity = identity_from_metadata(&second).unwrap();

    assert_eq!(first_identity.quality, IdentityQuality::Weak);
    assert_eq!(second_identity.quality, IdentityQuality::Weak);
    assert_ne!(first_identity.id, second_identity.id);
}
