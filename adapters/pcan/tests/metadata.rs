use seeed_hal_adapter_pcan::identity::{
    PcanChannelMetadata, identity_from_metadata,
};
use seeed_hal_can::IdentityQuality;

#[test]
fn vendor_device_identity_is_strong_and_channel_specific() {
    let metadata = PcanChannelMetadata {
        handle: 0x51,
        device_type: 0x05,
        controller_number: 1,
        device_name: Some("PCAN-USB FD".to_owned()),
        device_id: Some(0x1234),
    };

    let identity = identity_from_metadata(&metadata).unwrap();

    assert_eq!(identity.id.as_str(), "can:pcan:device:05:00001234:01");
    assert_eq!(identity.quality, IdentityQuality::Strong);
}

#[test]
fn hardware_model_without_instance_evidence_is_weak() {
    let metadata = PcanChannelMetadata {
        handle: 0x52,
        device_type: 0x05,
        controller_number: 0,
        device_name: Some("PCAN/USB 1%".to_owned()),
        device_id: None,
    };

    let identity = identity_from_metadata(&metadata).unwrap();

    assert_eq!(identity.id.as_str(), "can:pcan:handle:0052");
    assert_eq!(identity.quality, IdentityQuality::Weak);
}

#[test]
fn handle_only_identity_is_weak() {
    let metadata = PcanChannelMetadata {
        handle: 0x801,
        device_type: 0x08,
        controller_number: 0,
        device_name: None,
        device_id: None,
    };

    let identity = identity_from_metadata(&metadata).unwrap();

    assert_eq!(identity.id.as_str(), "can:pcan:handle:0801");
    assert_eq!(identity.quality, IdentityQuality::Weak);
}

#[test]
fn zero_vendor_device_id_is_not_treated_as_stable_evidence() {
    let metadata = PcanChannelMetadata {
        handle: 0x51,
        device_type: 0x05,
        controller_number: 0,
        device_name: None,
        device_id: Some(0),
    };

    let identity = identity_from_metadata(&metadata).unwrap();

    assert_eq!(identity.quality, IdentityQuality::Weak);
    assert_eq!(identity.id.as_str(), "can:pcan:handle:0051");
}

#[cfg(not(any(target_os = "windows", target_os = "linux")))]
#[test]
fn unsupported_platform_load_is_structured() {
    let error = seeed_hal_adapter_pcan::PcanAdapter::load()
        .expect_err("PCAN must be unavailable on unsupported platforms");

    assert_eq!(error.name().as_str(), "can.adapter.unavailable");
    assert_eq!(error.operation().as_str(), "can.adapter.load");
}
