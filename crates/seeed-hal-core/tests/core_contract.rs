use seeed_hal_core::{
    CapabilityId, Endpoint, IdentityQuality, LeaseMode, LeaseToken, ResourceId,
    ResourceSelector, TransportKind,
};

#[test]
fn resource_identity_is_not_an_endpoint() {
    let id = ResourceId::parse("serial:usb:10c4:ea60:ABC123").unwrap();
    let endpoint = Endpoint::new("/dev/ttyUSB0").unwrap();
    assert_eq!(id.as_str(), "serial:usb:10c4:ea60:ABC123");
    assert_eq!(endpoint.as_str(), "/dev/ttyUSB0");
    assert_ne!(id.as_str(), endpoint.as_str());
}

#[test]
fn capability_requires_contract_version() {
    assert!(CapabilityId::parse("serial.bytes/v1").is_ok());
    assert!(CapabilityId::parse("serial.bytes").is_err());
}

#[test]
fn lease_token_carries_fencing_generation() {
    let token = LeaseToken::new_for_test(7, LeaseMode::Control);
    assert_eq!(token.generation(), 7);
    assert_eq!(token.mode(), LeaseMode::Control);
}

#[test]
fn selector_can_require_identity_quality() {
    let selector = ResourceSelector::exact(
        ResourceId::parse("serial:usb:10c4:ea60:ABC123").unwrap(),
        IdentityQuality::Strong,
        TransportKind::Serial,
    );
    assert_eq!(selector.minimum_identity_quality(), IdentityQuality::Strong);
}

#[test]
fn error_decisions_do_not_require_message_parsing() {
    let error = seeed_hal_core::HalError::new(
        "runtime.lease.stale_generation",
        seeed_hal_core::ErrorCategory::Conflict,
        "serial.write",
        false,
        "generation 4 is older than 5",
    )
    .unwrap();
    assert_eq!(error.name().as_str(), "runtime.lease.stale_generation");
    assert_eq!(error.category(), seeed_hal_core::ErrorCategory::Conflict);
    assert!(!error.retryable());
    let json = serde_json::to_value(&error).unwrap();
    assert_eq!(json["name"], "runtime.lease.stale_generation");
    assert_eq!(json["category"], "Conflict");
    assert_eq!(json["operation"], "serial.write");
    assert_eq!(json["retryable"], false);
    assert!(json.get("debug_message").is_none());
}

#[test]
fn malformed_serialized_values_are_rejected() {
    assert!(serde_json::from_str::<seeed_hal_core::ResourceId>("\"\"").is_err());
    assert!(serde_json::from_str::<seeed_hal_core::Endpoint>("\"\"").is_err());
    assert!(serde_json::from_str::<seeed_hal_core::CapabilityId>("\"serial.bytes\"").is_err());
    assert!(serde_json::from_str::<seeed_hal_core::LeaseId>("\"\"").is_err());
    assert!(serde_json::from_str::<seeed_hal_core::ErrorName>("\"\"").is_err());
}

#[test]
fn public_error_construction_returns_result_instead_of_panicking() {
    let error = seeed_hal_core::HalError::new(
        "runtime.lease.stale_generation",
        seeed_hal_core::ErrorCategory::Conflict,
        "",
        false,
        "generation 4 is older than 5",
    );
    assert!(error.is_err());
}
