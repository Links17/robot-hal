use seeed_hal_core::{
    CapabilityId, CapabilitySet, Endpoint, ErrorContext, IdentityQuality, LeaseMode, LeaseToken,
    ResourceDescriptor, ResourceId, ResourceProperties, ResourceSelector, TransportKind,
    resolve_resource,
};
use std::collections::BTreeMap;

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
fn structured_error_details_are_validated_and_preserved() {
    let context = ErrorContext::new([
        ("queueDepth".to_owned(), "64".to_owned()),
        ("limit_bytes".to_owned(), "1024".to_owned()),
    ])
    .unwrap();
    let error = seeed_hal_core::HalError::new(
        "runtime.queue.full",
        seeed_hal_core::ErrorCategory::Unavailable,
        "serial.write",
        true,
        "queue is full",
    )
    .unwrap()
    .with_resource_id(ResourceId::parse("serial:virtual:0").unwrap())
    .with_platform_code("11")
    .unwrap()
    .with_vendor_code("VENDOR_BUSY")
    .unwrap()
    .with_context(context);

    assert_eq!(error.resource_id().unwrap().as_str(), "serial:virtual:0");
    assert_eq!(error.platform_code(), Some("11"));
    assert_eq!(error.vendor_code(), Some("VENDOR_BUSY"));
    assert_eq!(
        error.context().iter().collect::<Vec<_>>(),
        vec![("limit_bytes", "1024"), ("queueDepth", "64")]
    );
}

#[test]
fn legacy_error_constructor_has_empty_details() {
    let error = seeed_hal_core::HalError::new(
        "runtime.session.closed",
        seeed_hal_core::ErrorCategory::Conflict,
        "serial.read",
        false,
        "closed",
    )
    .unwrap();
    assert!(error.resource_id().is_none());
    assert!(error.platform_code().is_none());
    assert!(error.vendor_code().is_none());
    assert!(error.context().is_empty());
}

#[test]
fn error_context_rejects_duplicate_and_invalid_keys() {
    let duplicate = ErrorContext::new([
        ("queueDepth", "64"),
        ("queueDepth", "65"),
    ])
    .unwrap_err();
    assert_eq!(duplicate.name().as_str(), "error.context.duplicate_key");

    for key in ["", "QueueDepth", "queue.depth", "queue depth", "é"] {
        assert_eq!(
            ErrorContext::new([(key, "value")]).unwrap_err().name().as_str(),
            if key.is_empty() {
                "error.context.key.empty"
            } else if !key.is_ascii() {
                "error.context.key.non_ascii"
            } else {
                "error.context.key.invalid"
            }
        );
    }
}

#[test]
fn error_context_accepts_entry_key_value_and_aggregate_limits() {
    let entries = (0..16)
        .map(|index| (format!("k{index}"), String::new()))
        .collect::<Vec<_>>();
    assert!(ErrorContext::new(entries).is_ok());

    let key = format!("a{}", "x".repeat(63));
    assert_eq!(key.len(), 64);
    assert!(ErrorContext::new([(key, "value")]).is_ok());
    assert!(ErrorContext::new([("a", "x".repeat(1024))]).is_ok());

    let exact_total = (0..8)
        .map(|index| (format!("k{index}"), "x".repeat(1022)))
        .collect::<Vec<_>>();
    assert_eq!(exact_total.iter().map(|(k, v)| k.len() + v.len()).sum::<usize>(), 8192);
    assert!(ErrorContext::new(exact_total).is_ok());
}

#[test]
fn error_context_rejects_one_byte_over_limits() {
    let entries = (0..17)
        .map(|index| (format!("k{index}"), String::new()))
        .collect::<Vec<_>>();
    assert_eq!(
        ErrorContext::new(entries).unwrap_err().name().as_str(),
        "error.context.too_many_entries"
    );

    assert_eq!(
        ErrorContext::new([("a".to_owned() + &"x".repeat(64), "value")])
            .unwrap_err()
            .name()
            .as_str(),
        "error.context.key.too_long"
    );
    assert_eq!(
        ErrorContext::new([("a", "x".repeat(1025))])
            .unwrap_err()
            .name()
            .as_str(),
        "error.context.value.too_long"
    );

    let over_total = (0..8)
        .map(|index| {
            (
                format!("k{index}"),
                "x".repeat(if index == 0 { 1023 } else { 1022 }),
            )
        })
        .collect::<Vec<_>>();
    assert_eq!(
        over_total
            .iter()
            .map(|(key, value)| key.len() + value.len())
            .sum::<usize>(),
        8193
    );
    assert_eq!(
        ErrorContext::new(over_total).unwrap_err().name().as_str(),
        "error.context.too_large"
    );
}

#[test]
fn error_codes_reuse_identifier_validation() {
    let error = seeed_hal_core::HalError::new(
        "runtime.queue.full",
        seeed_hal_core::ErrorCategory::Unavailable,
        "serial.write",
        true,
        "queue is full",
    )
    .unwrap();
    let exact_code = "x".repeat(255);
    assert!(error.clone().with_platform_code(exact_code.clone()).is_ok());
    assert!(error.clone().with_vendor_code(exact_code).is_ok());
    for code in [String::new(), "é".to_owned(), "x".repeat(256)] {
        assert!(error.clone().with_platform_code(code.clone()).is_err());
        assert!(error.clone().with_vendor_code(code).is_err());
    }
}

#[test]
fn hal_error_debug_is_redacted() {
    let context = ErrorContext::new([("secret_key", "secretValue")]).unwrap();
    let error = seeed_hal_core::HalError::new(
        "runtime.queue.full",
        seeed_hal_core::ErrorCategory::Unavailable,
        "serial.write",
        true,
        "private diagnostic",
    )
    .unwrap()
    .with_resource_id(ResourceId::parse("serial:private:resource").unwrap())
    .with_platform_code("11")
    .unwrap()
    .with_vendor_code("VENDOR_PRIVATE")
    .unwrap()
    .with_context(context);

    let debug = format!("{error:?}");
    assert!(!debug.contains("serial:private:resource"));
    assert!(!debug.contains("11"));
    assert!(!debug.contains("VENDOR_PRIVATE"));
    assert!(!debug.contains("secret_key"));
    assert!(!debug.contains("secretValue"));
    assert!(!debug.contains("private diagnostic"));
    assert!(debug.contains("runtime.queue.full"));
}

#[test]
fn enriched_error_serde_remains_decision_only() {
    let context = ErrorContext::new([(String::from("queueDepth"), String::from("64"))]).unwrap();
    let error = seeed_hal_core::HalError::new(
        "runtime.queue.full",
        seeed_hal_core::ErrorCategory::Unavailable,
        "serial.write",
        true,
        "private diagnostic",
    )
    .unwrap()
    .with_resource_id(ResourceId::parse("serial:virtual:0").unwrap())
    .with_platform_code("11")
    .unwrap()
    .with_vendor_code("VENDOR_BUSY")
    .unwrap()
    .with_context(context);
    let json = serde_json::to_value(error).unwrap();
    let expected = BTreeMap::from([
        ("category", serde_json::json!("Unavailable")),
        ("name", serde_json::json!("runtime.queue.full")),
        ("operation", serde_json::json!("serial.write")),
        ("retryable", serde_json::json!(true)),
    ]);
    assert_eq!(json, serde_json::to_value(expected).unwrap());
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
