use seeed_hal_core::{
    CapabilityId, CapabilitySet, Endpoint, IdentityQuality, LeaseMode, LeaseToken,
    ResourceDescriptor, ResourceId, ResourceProperties, ResourceSelector, TransportKind,
    resolve_resource,
};

fn serial_descriptor(id: &str, endpoint: &str, quality: IdentityQuality) -> ResourceDescriptor {
    ResourceDescriptor::new(
        ResourceId::parse(id).unwrap(),
        Endpoint::new(endpoint).unwrap(),
        quality,
        TransportKind::Serial,
        ResourceProperties::default(),
        CapabilitySet::new(vec![CapabilityId::parse("serial.bytes/v1").unwrap()]),
    )
}

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

#[test]
fn identity_quality_is_an_ordered_minimum_threshold() {
    let capability = CapabilityId::parse("serial.bytes/v1").unwrap();
    let qualities = [
        IdentityQuality::Weak,
        IdentityQuality::Medium,
        IdentityQuality::Strong,
    ];

    for (descriptor_rank, descriptor_quality) in qualities.into_iter().enumerate() {
        let descriptor =
            serial_descriptor("serial:threshold", "/dev/threshold", descriptor_quality);
        for (minimum_rank, minimum_quality) in qualities.into_iter().enumerate() {
            let selector = ResourceSelector::exact(
                ResourceId::parse("serial:threshold").unwrap(),
                minimum_quality,
                TransportKind::Serial,
            );
            let result = resolve_resource(
                std::slice::from_ref(&descriptor),
                &selector,
                &capability,
                "serial.open",
            );
            assert_eq!(
                result.is_ok(),
                descriptor_rank >= minimum_rank,
                "descriptor {descriptor_quality:?}, minimum {minimum_quality:?}",
            );
        }
    }
}

#[test]
fn resolver_requires_transport_capability_and_exact_persisted_id() {
    let serial = CapabilityId::parse("serial.bytes/v1").unwrap();
    let other = CapabilityId::parse("serial.control/v1").unwrap();
    let descriptors = vec![
        serial_descriptor("serial:first", "/dev/shared", IdentityQuality::Strong),
        serial_descriptor("serial:second", "/dev/shared", IdentityQuality::Strong),
    ];
    let selector = descriptors[1].selector();

    let selected = resolve_resource(&descriptors, &selector, &serial, "serial.open").unwrap();
    assert_eq!(selected.id().as_str(), "serial:second");
    assert_eq!(selected.endpoint().as_str(), "/dev/shared");
    assert_eq!(
        resolve_resource(&descriptors, &selector, &other, "serial.open")
            .unwrap_err()
            .name()
            .as_str(),
        "runtime.resource.not_found",
    );
}

#[test]
fn duplicate_persisted_identity_is_ambiguous_even_when_endpoints_differ() {
    let capability = CapabilityId::parse("serial.bytes/v1").unwrap();
    let descriptors = vec![
        serial_descriptor("serial:duplicate", "/dev/first", IdentityQuality::Strong),
        serial_descriptor("serial:duplicate", "/dev/second", IdentityQuality::Strong),
    ];

    let error = resolve_resource(
        &descriptors,
        &descriptors[0].selector(),
        &capability,
        "serial.open",
    )
    .unwrap_err();

    assert_eq!(error.name().as_str(), "runtime.resource.ambiguous");
    assert_eq!(error.category(), seeed_hal_core::ErrorCategory::Conflict);
}

#[test]
fn serialized_hal_error_shape_deserializes_and_reserializes_compatibly() {
    let original = seeed_hal_core::HalError::new(
        "runtime.lease.stale_generation",
        seeed_hal_core::ErrorCategory::Conflict,
        "serial.write",
        false,
        "diagnostic text is intentionally output-only",
    )
    .unwrap();
    let json = serde_json::to_value(&original).unwrap();
    let restored: seeed_hal_core::HalError = serde_json::from_value(json.clone()).unwrap();

    assert_eq!(serde_json::to_value(restored).unwrap(), json);
}
