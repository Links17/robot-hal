use prost::Message;
use seeed_hal_can::{
    CanActiveConfig, CanBatchSendError, CanBitTiming, CanBusState, CanBusStatus,
    CanConfigureConfig, CanErrorClass, CanFilter, CanFilterSet, CanFrame, CanFrameClasses, CanId,
    CanIdFormat, CanLinkExpectation, CanMode, CanOpenConfig, CanTimestamp, CanTimestampSource,
    ReceivedCanFrame,
};
use seeed_hal_core::{
    ErrorCategory, ErrorContext, HalError, IdentityQuality, LeaseId, LeaseMode, LeaseToken,
    ResourceId, ResourceSelector, TransportKind,
};
use seeed_hal_protocol::v1::{self, envelope};

fn envelope_with(payload: envelope::Payload) -> v1::Envelope {
    v1::Envelope {
        request_id: 7,
        payload: Some(payload),
    }
}

fn top_level_fields(encoded: &[u8]) -> Vec<u32> {
    let mut fields = Vec::new();
    let mut index = 0;
    while index < encoded.len() {
        let (key, next) = read_varint(encoded, index);
        index = next;
        fields.push((key >> 3) as u32);
        match key & 7 {
            0 => index = read_varint(encoded, index).1,
            1 => index += 8,
            2 => {
                let (len, next) = read_varint(encoded, index);
                index = next + len as usize;
            }
            5 => index += 4,
            wire => panic!("unsupported test wire type {wire}"),
        }
    }
    fields
}

fn read_varint(encoded: &[u8], mut index: usize) -> (u64, usize) {
    let mut value = 0;
    for shift in (0..70).step_by(7) {
        let byte = encoded[index];
        index += 1;
        value |= u64::from(byte & 0x7f) << shift;
        if byte & 0x80 == 0 {
            return (value, index);
        }
    }
    panic!("invalid test varint")
}

fn valid_error() -> v1::Error {
    v1::Error {
        name: "runtime.queue.full".to_owned(),
        category: v1::ErrorCategory::Unavailable as i32,
        operation: "serial.write".to_owned(),
        retryable: true,
        debug_message: "queue is full".to_owned(),
        ..Default::default()
    }
}

fn valid_lease() -> v1::LeaseToken {
    v1::LeaseToken {
        lease_id: "lease-can".to_owned(),
        generation: 1,
        mode: v1::LeaseMode::Control as i32,
    }
}

fn standard_id(value: u32) -> v1::CanId {
    v1::CanId {
        value,
        format: v1::CanIdFormat::Standard as i32,
    }
}

fn classic_frame() -> v1::CanFrame {
    v1::CanFrame {
        id: Some(standard_id(0x123)),
        kind: v1::CanFrameKind::ClassicData as i32,
        data: vec![1, 2, 3],
        ..Default::default()
    }
}

fn valid_timing() -> v1::CanBitTiming {
    v1::CanBitTiming {
        bitrate: 500_000,
        sample_point_permill: Some(875),
        sjw: Some(1),
    }
}

fn valid_filter() -> v1::CanFilter {
    v1::CanFilter {
        id: 0x120,
        mask: 0x7f0,
        format: v1::CanIdFormat::Standard as i32,
        classes: Some(v1::CanFrameClasses {
            data: true,
            remote: true,
            error: true,
        }),
    }
}

fn assert_tags<M: Message>(message: &M, expected: &[u32]) {
    assert_eq!(top_level_fields(&message.encode_to_vec()), expected);
}

fn assert_invalid_message(error: HalError, field: &str) {
    assert_eq!(error.name().as_str(), "runtime.protocol.invalid_message");
    assert!(error.debug_message().contains(field));
}

#[test]
fn every_v1_envelope_payload_field_number_is_locked() {
    let cases = [
        (
            10,
            envelope::Payload::HandshakeRequest(v1::HandshakeRequest::default()),
        ),
        (
            11,
            envelope::Payload::HandshakeResponse(v1::HandshakeResponse::default()),
        ),
        (
            20,
            envelope::Payload::EnumerateSerialRequest(v1::EnumerateSerialRequest {}),
        ),
        (
            21,
            envelope::Payload::EnumerateSerialResponse(v1::EnumerateSerialResponse::default()),
        ),
        (
            22,
            envelope::Payload::OpenSerialRequest(v1::OpenSerialRequest::default()),
        ),
        (
            23,
            envelope::Payload::OpenSerialResponse(v1::OpenSerialResponse::default()),
        ),
        (
            24,
            envelope::Payload::SerialReadRequest(v1::SerialReadRequest::default()),
        ),
        (
            25,
            envelope::Payload::SerialReadResponse(v1::SerialReadResponse::default()),
        ),
        (
            26,
            envelope::Payload::SerialWriteRequest(v1::SerialWriteRequest::default()),
        ),
        (27, envelope::Payload::SerialWriteResponse(v1::Empty {})),
        (
            28,
            envelope::Payload::SerialFlushRequest(v1::SerialFlushRequest::default()),
        ),
        (29, envelope::Payload::SerialFlushResponse(v1::Empty {})),
        (
            30,
            envelope::Payload::SetSerialControlLinesRequest(
                v1::SetSerialControlLinesRequest::default(),
            ),
        ),
        (
            31,
            envelope::Payload::SetSerialControlLinesResponse(v1::Empty {}),
        ),
        (
            32,
            envelope::Payload::CloseSessionRequest(v1::CloseSessionRequest::default()),
        ),
        (33, envelope::Payload::CloseSessionResponse(v1::Empty {})),
        (
            40,
            envelope::Payload::RuntimeEvent(v1::RuntimeEvent::default()),
        ),
        (
            50,
            envelope::Payload::EnumerateCanRequest(v1::EnumerateCanRequest {}),
        ),
        (
            51,
            envelope::Payload::EnumerateCanResponse(v1::EnumerateCanResponse::default()),
        ),
        (
            52,
            envelope::Payload::OpenCanRequest(v1::OpenCanRequest::default()),
        ),
        (
            53,
            envelope::Payload::OpenCanResponse(v1::OpenCanResponse::default()),
        ),
        (
            54,
            envelope::Payload::CanSendRequest(v1::CanSendRequest::default()),
        ),
        (
            55,
            envelope::Payload::CanSendResponse(v1::CanSendResponse::default()),
        ),
        (
            56,
            envelope::Payload::CanReceiveRequest(v1::CanReceiveRequest::default()),
        ),
        (
            57,
            envelope::Payload::CanReceiveResponse(v1::CanReceiveResponse::default()),
        ),
        (
            58,
            envelope::Payload::ReplaceCanFiltersRequest(v1::ReplaceCanFiltersRequest::default()),
        ),
        (
            59,
            envelope::Payload::ReplaceCanFiltersResponse(v1::Empty {}),
        ),
        (
            60,
            envelope::Payload::GetCanBusStatusRequest(v1::GetCanBusStatusRequest::default()),
        ),
        (
            61,
            envelope::Payload::GetCanBusStatusResponse(v1::GetCanBusStatusResponse::default()),
        ),
        (100, envelope::Payload::Error(v1::Error::default())),
    ];

    for (payload_tag, payload) in cases {
        let envelope = envelope_with(payload);
        let encoded = envelope.encode_to_vec();
        assert_eq!(top_level_fields(&encoded), vec![1, payload_tag]);
        assert_eq!(v1::Envelope::decode(encoded.as_slice()).unwrap(), envelope);
    }
}

#[test]
fn inclusive_minor_range_field_numbers_are_additive_and_locked() {
    let request = v1::HandshakeRequest {
        protocol_minor_minimum: 2,
        protocol_minor_maximum: 4,
        ..Default::default()
    };
    let response = v1::HandshakeResponse {
        protocol_minor_minimum: 1,
        protocol_minor_maximum: 5,
        ..Default::default()
    };

    assert!(top_level_fields(&request.encode_to_vec()).contains(&8));
    assert!(top_level_fields(&request.encode_to_vec()).contains(&9));
    assert!(top_level_fields(&response.encode_to_vec()).contains(&7));
    assert!(top_level_fields(&response.encode_to_vec()).contains(&8));
}

#[test]
fn legacy_exact_minor_defaults_to_a_single_value_range() {
    let request = v1::HandshakeRequest {
        protocol_major: 1,
        protocol_minor: 0,
        ..Default::default()
    };

    assert_eq!(
        seeed_hal_protocol::handshake_minor_range(&request).unwrap(),
        (0, 0)
    );
}

#[test]
fn highest_shared_minor_is_selected_from_inclusive_ranges() {
    assert_eq!(
        seeed_hal_protocol::negotiate_protocol_minor(1, 1, 3, 1, 2, 4).unwrap(),
        3,
    );
}

#[test]
fn no_overlap_and_major_mismatch_use_the_stable_compatibility_error() {
    for result in [
        seeed_hal_protocol::negotiate_protocol_minor(1, 0, 1, 1, 2, 3),
        seeed_hal_protocol::negotiate_protocol_minor(1, 0, 3, 2, 0, 3),
    ] {
        assert_eq!(
            result.unwrap_err().name().as_str(),
            "runtime.protocol.version_incompatible",
        );
    }
}

#[test]
fn unknown_additive_handshake_fields_are_ignored_safely() {
    let request = v1::HandshakeRequest {
        protocol_major: 1,
        ..Default::default()
    };
    let mut encoded = request.encode_to_vec();
    encoded.extend_from_slice(&[0xa0, 0x06, 0x01]);

    let decoded = v1::HandshakeRequest::decode(encoded.as_slice()).unwrap();
    assert_eq!(
        seeed_hal_protocol::handshake_minor_range(&decoded).unwrap(),
        (0, 0)
    );
}

#[test]
fn invalid_required_enum_is_a_structured_protocol_error() {
    let selector = v1::ResourceSelector {
        resource_id: "serial:test".to_owned(),
        minimum_identity_quality: 0,
        transport: v1::TransportKind::Serial as i32,
    };

    let error = seeed_hal_core::ResourceSelector::try_from(selector).unwrap_err();
    assert_eq!(error.name().as_str(), "runtime.protocol.invalid_message");
}

#[test]
fn error_fields_are_additive_and_locked() {
    let encoded = v1::Error {
        resource_id: "serial:virtual:0".to_owned(),
        platform_code: "11".to_owned(),
        vendor_code: "VENDOR_BUSY".to_owned(),
        context: [("queueDepth".to_owned(), "64".to_owned())].into(),
        ..valid_error()
    }
    .encode_to_vec();

    assert_eq!(top_level_fields(&encoded), vec![1, 2, 3, 4, 5, 6, 7, 8, 9]);
}

#[test]
fn rich_error_round_trips_all_structured_details() {
    let context = ErrorContext::new([("queueDepth", "64"), ("portName", "ttyUSB0")]).unwrap();
    let error = HalError::new(
        "runtime.queue.full",
        ErrorCategory::Unavailable,
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

    let decoded = seeed_hal_protocol::error_from_proto(v1::Error::from(&error)).unwrap();
    assert_eq!(decoded.name().as_str(), error.name().as_str());
    assert_eq!(decoded.category(), error.category());
    assert_eq!(decoded.operation().as_str(), error.operation().as_str());
    assert_eq!(decoded.retryable(), error.retryable());
    assert_eq!(decoded.debug_message(), error.debug_message());
    assert_eq!(decoded.resource_id(), error.resource_id());
    assert_eq!(decoded.platform_code(), error.platform_code());
    assert_eq!(decoded.vendor_code(), error.vendor_code());
    assert_eq!(
        decoded.context().iter().collect::<Vec<_>>(),
        error.context().iter().collect::<Vec<_>>()
    );
}

#[test]
fn legacy_error_round_trip_has_empty_structured_details() {
    let decoded = seeed_hal_protocol::error_from_proto(valid_error()).unwrap();
    assert!(decoded.resource_id().is_none());
    assert!(decoded.platform_code().is_none());
    assert!(decoded.vendor_code().is_none());
    assert!(decoded.context().is_empty());
}

#[test]
fn malformed_error_details_are_invalid_messages() {
    let invalid_resource = seeed_hal_protocol::error_from_proto(v1::Error {
        resource_id: "é".to_owned(),
        ..valid_error()
    })
    .unwrap_err();
    assert_eq!(
        invalid_resource.name().as_str(),
        "runtime.protocol.invalid_message"
    );
    assert!(invalid_resource.debug_message().contains("resource_id"));

    for (field, value) in [("platform_code", "é"), ("vendor_code", "é")] {
        let result = if field == "platform_code" {
            v1::Error {
                platform_code: value.to_owned(),
                ..valid_error()
            }
        } else {
            v1::Error {
                vendor_code: value.to_owned(),
                ..valid_error()
            }
        };
        let error = seeed_hal_protocol::error_from_proto(result).unwrap_err();
        assert_eq!(error.name().as_str(), "runtime.protocol.invalid_message");
        assert!(error.debug_message().contains(field));
    }

    let error = seeed_hal_protocol::error_from_proto(v1::Error {
        context: [("QueueDepth".to_owned(), "64".to_owned())].into(),
        ..valid_error()
    })
    .unwrap_err();
    assert_eq!(error.name().as_str(), "runtime.protocol.invalid_message");
    assert!(error.debug_message().contains("context"));
}

#[test]
fn malformed_error_details_reject_every_size_and_count_bound() {
    let cases = [
        v1::Error {
            platform_code: "x".repeat(256),
            ..valid_error()
        },
        v1::Error {
            vendor_code: "x".repeat(256),
            ..valid_error()
        },
        v1::Error {
            context: [("k".repeat(65), "x".to_owned())].into(),
            ..valid_error()
        },
        v1::Error {
            context: [("k".to_owned(), "x".repeat(1025))].into(),
            ..valid_error()
        },
        v1::Error {
            context: (0..17)
                .map(|index| (format!("key{index}"), "x".to_owned()))
                .collect(),
            ..valid_error()
        },
        v1::Error {
            context: (0..8)
                .map(|index| {
                    (
                        format!("k{index}"),
                        "x".repeat(if index == 0 { 1023 } else { 1022 }),
                    )
                })
                .collect(),
            ..valid_error()
        },
    ];

    for value in cases {
        let error = seeed_hal_protocol::error_from_proto(value).unwrap_err();
        assert_eq!(error.name().as_str(), "runtime.protocol.invalid_message");
    }
}

#[test]
fn wire_minor_one_range_and_additive_enum_values_are_locked() {
    assert_eq!(seeed_hal_protocol::PROTOCOL_MAJOR, 1);
    assert_eq!(seeed_hal_protocol::PROTOCOL_MINOR_MINIMUM, 0);
    assert_eq!(seeed_hal_protocol::PROTOCOL_MINOR_MAXIMUM, 1);
    assert_eq!(seeed_hal_protocol::PROTOCOL_MINOR, 1);
    assert_eq!(v1::IdentityQuality::Unspecified as i32, 0);
    assert_eq!(v1::IdentityQuality::Weak as i32, 1);
    assert_eq!(v1::IdentityQuality::Medium as i32, 2);
    assert_eq!(v1::IdentityQuality::Strong as i32, 3);
    assert_eq!(v1::TransportKind::Unspecified as i32, 0);
    assert_eq!(v1::TransportKind::Serial as i32, 1);
    assert_eq!(v1::TransportKind::Can as i32, 2);
    assert_eq!(v1::DataBits::Unspecified as i32, 0);
    assert_eq!(v1::DataBits::Five as i32, 1);
    assert_eq!(v1::DataBits::Six as i32, 2);
    assert_eq!(v1::DataBits::Seven as i32, 3);
    assert_eq!(v1::DataBits::Eight as i32, 4);
    assert_eq!(v1::Parity::Unspecified as i32, 0);
    assert_eq!(v1::Parity::None as i32, 1);
    assert_eq!(v1::Parity::Odd as i32, 2);
    assert_eq!(v1::Parity::Even as i32, 3);
    assert_eq!(v1::StopBits::Unspecified as i32, 0);
    assert_eq!(v1::StopBits::One as i32, 1);
    assert_eq!(v1::StopBits::Two as i32, 2);
    assert_eq!(v1::FlowControl::Unspecified as i32, 0);
    assert_eq!(v1::FlowControl::None as i32, 1);
    assert_eq!(v1::FlowControl::Software as i32, 2);
    assert_eq!(v1::FlowControl::Hardware as i32, 3);
    assert_eq!(v1::LeaseMode::Unspecified as i32, 0);
    assert_eq!(v1::LeaseMode::Observe as i32, 1);
    assert_eq!(v1::LeaseMode::Control as i32, 2);
    assert_eq!(v1::LeaseMode::Maintenance as i32, 3);
    assert_eq!(v1::ErrorCategory::Unspecified as i32, 0);
    assert_eq!(v1::ErrorCategory::InvalidArgument as i32, 1);
    assert_eq!(v1::ErrorCategory::NotFound as i32, 2);
    assert_eq!(v1::ErrorCategory::Conflict as i32, 3);
    assert_eq!(v1::ErrorCategory::Unavailable as i32, 4);
    assert_eq!(v1::ErrorCategory::Internal as i32, 5);
    assert_eq!(v1::RuntimeEventKind::Unspecified as i32, 0);
    assert_eq!(v1::RuntimeEventKind::SessionOpened as i32, 1);
    assert_eq!(v1::RuntimeEventKind::SessionClosed as i32, 2);
    assert_eq!(v1::RuntimeEventKind::CanBusActive as i32, 3);
    assert_eq!(v1::RuntimeEventKind::CanBusWarning as i32, 4);
    assert_eq!(v1::RuntimeEventKind::CanBusPassive as i32, 5);
    assert_eq!(v1::RuntimeEventKind::CanBusOff as i32, 6);
    assert_eq!(v1::RuntimeEventKind::CanBusStopped as i32, 7);
    assert_eq!(v1::RuntimeEventKind::CanBusUnknown as i32, 8);
    assert_eq!(v1::CanIdFormat::Unspecified as i32, 0);
    assert_eq!(v1::CanIdFormat::Standard as i32, 1);
    assert_eq!(v1::CanIdFormat::Extended as i32, 2);
    assert_eq!(v1::CanIdFormat::Either as i32, 3);
    assert_eq!(v1::CanFrameKind::Unspecified as i32, 0);
    assert_eq!(v1::CanFrameKind::ClassicData as i32, 1);
    assert_eq!(v1::CanFrameKind::ClassicRemote as i32, 2);
    assert_eq!(v1::CanFrameKind::FdData as i32, 3);
    assert_eq!(v1::CanFrameKind::Error as i32, 4);
    assert_eq!(v1::CanErrorClass::Unspecified as i32, 0);
    assert_eq!(v1::CanErrorClass::TxTimeout as i32, 1);
    assert_eq!(v1::CanErrorClass::LostArbitration as i32, 2);
    assert_eq!(v1::CanErrorClass::Controller as i32, 3);
    assert_eq!(v1::CanErrorClass::Protocol as i32, 4);
    assert_eq!(v1::CanErrorClass::Transceiver as i32, 5);
    assert_eq!(v1::CanErrorClass::NoAcknowledgement as i32, 6);
    assert_eq!(v1::CanErrorClass::BusOff as i32, 7);
    assert_eq!(v1::CanErrorClass::BusError as i32, 8);
    assert_eq!(v1::CanErrorClass::Restarted as i32, 9);
    assert_eq!(v1::CanErrorClass::Other as i32, 10);
    assert_eq!(v1::CanTimestampSource::Unspecified as i32, 0);
    assert_eq!(v1::CanTimestampSource::Hardware as i32, 1);
    assert_eq!(v1::CanTimestampSource::Kernel as i32, 2);
    assert_eq!(v1::CanTimestampSource::HostMonotonic as i32, 3);
    assert_eq!(v1::CanMode::Classic as i32, 1);
    assert_eq!(v1::CanMode::Fd as i32, 2);
    assert_eq!(v1::CanMode::Unspecified as i32, 0);
    assert_eq!(v1::CanBusState::Unspecified as i32, 0);
    assert_eq!(v1::CanBusState::Active as i32, 1);
    assert_eq!(v1::CanBusState::Warning as i32, 2);
    assert_eq!(v1::CanBusState::Passive as i32, 3);
    assert_eq!(v1::CanBusState::BusOff as i32, 4);
    assert_eq!(v1::CanBusState::Stopped as i32, 5);
    assert_eq!(v1::CanBusState::Unknown as i32, 6);
}

#[test]
fn every_nested_wire_field_number_is_locked() {
    assert_tags(&v1::Empty {}, &[]);
    assert_tags(&v1::EnumerateSerialRequest {}, &[]);
    assert_tags(&v1::EnumerateCanRequest {}, &[]);
    assert_tags(
        &v1::HandshakeRequest {
            startup_token: vec![1],
            protocol_major: 1,
            protocol_minor: 1,
            required_capabilities: vec!["can.classic/v1".to_owned()],
            max_frame_bytes: 1,
            max_read_bytes: 1,
            max_write_bytes: 1,
            protocol_minor_minimum: 1,
            protocol_minor_maximum: 1,
        },
        &[1, 2, 3, 4, 5, 6, 7, 8, 9],
    );
    assert_tags(
        &v1::HandshakeResponse {
            protocol_major: 1,
            protocol_minor: 1,
            capabilities: vec!["can.classic/v1".to_owned()],
            max_frame_bytes: 1,
            max_read_bytes: 1,
            max_write_bytes: 1,
            protocol_minor_minimum: 1,
            protocol_minor_maximum: 1,
        },
        &[1, 2, 3, 4, 5, 6, 7, 8],
    );
    let descriptor = v1::ResourceDescriptor {
        resource_id: "can:test".to_owned(),
        endpoint: "virtual:can".to_owned(),
        identity_quality: v1::IdentityQuality::Strong as i32,
        transport: v1::TransportKind::Can as i32,
        properties: [("driver".to_owned(), "virtual".to_owned())].into(),
        capabilities: vec!["can.classic/v1".to_owned()],
    };
    assert_tags(&descriptor, &[1, 2, 3, 4, 5, 6]);
    let selector = v1::ResourceSelector {
        resource_id: "can:test".to_owned(),
        minimum_identity_quality: v1::IdentityQuality::Strong as i32,
        transport: v1::TransportKind::Can as i32,
    };
    assert_tags(&selector, &[1, 2, 3]);
    assert_tags(
        &v1::SerialConfig {
            baud_rate: 115_200,
            data_bits: v1::DataBits::Eight as i32,
            parity: v1::Parity::None as i32,
            stop_bits: v1::StopBits::One as i32,
            flow_control: v1::FlowControl::None as i32,
            read_timeout_ms: 1,
        },
        &[1, 2, 3, 4, 5, 6],
    );
    assert_tags(&valid_lease(), &[1, 2, 3]);
    assert_tags(
        &v1::EnumerateSerialResponse {
            resources: vec![descriptor.clone()],
        },
        &[1],
    );
    assert_tags(
        &v1::OpenSerialRequest {
            selector: Some(selector.clone()),
            config: Some(v1::SerialConfig {
                baud_rate: 1,
                ..Default::default()
            }),
        },
        &[1, 2],
    );
    assert_tags(
        &v1::OpenSerialResponse {
            session_id: "session".to_owned(),
            lease: Some(valid_lease()),
        },
        &[1, 2],
    );
    assert_tags(
        &v1::SerialReadRequest {
            session_id: "session".to_owned(),
            lease: Some(valid_lease()),
            max_bytes: 1,
        },
        &[1, 2, 3],
    );
    assert_tags(&v1::SerialReadResponse { data: vec![1] }, &[1]);
    assert_tags(
        &v1::SerialWriteRequest {
            session_id: "session".to_owned(),
            lease: Some(valid_lease()),
            data: vec![1],
        },
        &[1, 2, 3],
    );
    assert_tags(
        &v1::SerialFlushRequest {
            session_id: "session".to_owned(),
            lease: Some(valid_lease()),
        },
        &[1, 2],
    );
    assert_tags(
        &v1::SetSerialControlLinesRequest {
            session_id: "session".to_owned(),
            lease: Some(valid_lease()),
            data_terminal_ready: true,
            request_to_send: true,
        },
        &[1, 2, 3, 4],
    );
    assert_tags(
        &v1::CloseSessionRequest {
            session_id: "session".to_owned(),
            lease: Some(valid_lease()),
        },
        &[1, 2],
    );
    assert_tags(
        &v1::RuntimeEvent {
            sequence: 1,
            kind: v1::RuntimeEventKind::CanBusActive as i32,
            name: "can.bus.active".to_owned(),
            resource_id: "can:test".to_owned(),
            session_id: "session".to_owned(),
            owner_id: "owner".to_owned(),
            lease_generation: 1,
        },
        &[1, 2, 3, 4, 5, 6, 7],
    );
    assert_tags(
        &v1::CanId {
            value: 1,
            format: v1::CanIdFormat::Standard as i32,
        },
        &[1, 2],
    );
    assert_tags(
        &v1::CanFrame {
            id: Some(standard_id(1)),
            kind: v1::CanFrameKind::FdData as i32,
            data: vec![1],
            remote_dlc: 1,
            bitrate_switch: true,
            error_state_indicator: true,
            error_classes: vec![v1::CanErrorClass::Controller as i32],
        },
        &[1, 2, 3, 4, 5, 6, 7],
    );
    assert_tags(
        &v1::CanTimestamp {
            timestamp_ns: 1,
            source: v1::CanTimestampSource::Hardware as i32,
            clock_domain: "clock".to_owned(),
        },
        &[1, 2, 3],
    );
    assert_tags(
        &v1::ReceivedCanFrame {
            frame: Some(classic_frame()),
            timestamp: Some(v1::CanTimestamp {
                timestamp_ns: 1,
                source: v1::CanTimestampSource::Kernel as i32,
                clock_domain: "clock".to_owned(),
            }),
        },
        &[1, 2],
    );
    assert_tags(&valid_timing(), &[1, 2, 3]);
    assert_tags(
        &v1::CanLinkExpectation {
            mode: Some(v1::CanMode::Fd as i32),
            nominal_bitrate: Some(1),
            data_bitrate: Some(1),
            listen_only: Some(false),
            loopback: Some(false),
        },
        &[1, 2, 3, 4, 5],
    );
    let configure = v1::CanConfigureConfig {
        mode: v1::CanMode::Fd as i32,
        nominal: Some(valid_timing()),
        data: Some(valid_timing()),
        listen_only: true,
        loopback: true,
        restart_ms: Some(1),
    };
    assert_tags(&configure, &[1, 2, 3, 4, 5, 6]);
    assert_tags(
        &v1::CanOpenConfig {
            config: Some(v1::can_open_config::Config::Attach(
                v1::CanLinkExpectation::default(),
            )),
        },
        &[1],
    );
    assert_tags(
        &v1::CanOpenConfig {
            config: Some(v1::can_open_config::Config::Configure(configure)),
        },
        &[2],
    );
    assert_tags(
        &v1::CanActiveConfig {
            mode: v1::CanMode::Fd as i32,
            nominal: Some(valid_timing()),
            data: Some(valid_timing()),
            listen_only: true,
            loopback: true,
            clock_domain: "clock".to_owned(),
        },
        &[1, 2, 3, 4, 5, 6],
    );
    assert_tags(
        &v1::CanFrameClasses {
            data: true,
            remote: true,
            error: true,
        },
        &[1, 2, 3],
    );
    assert_tags(&valid_filter(), &[1, 2, 3, 4]);
    assert_tags(
        &v1::CanFilterSet {
            filters: vec![valid_filter()],
        },
        &[1],
    );
    assert_tags(
        &v1::CanBusStatus {
            state: v1::CanBusState::Warning as i32,
            tx_error_counter: Some(1),
            rx_error_counter: Some(1),
        },
        &[1, 2, 3],
    );
    assert_tags(
        &v1::EnumerateCanResponse {
            resources: vec![descriptor],
        },
        &[1],
    );
    assert_tags(
        &v1::OpenCanRequest {
            selector: Some(selector),
            mode: v1::LeaseMode::Maintenance as i32,
            config: Some(v1::CanOpenConfig {
                config: Some(v1::can_open_config::Config::Configure(configure)),
            }),
            filters: Some(v1::CanFilterSet {
                filters: vec![valid_filter()],
            }),
        },
        &[1, 2, 3, 4],
    );
    assert_tags(
        &v1::OpenCanResponse {
            session_id: "session".to_owned(),
            lease: Some(valid_lease()),
        },
        &[1, 2],
    );
    assert_tags(
        &v1::CanSendRequest {
            session_id: "session".to_owned(),
            lease: Some(valid_lease()),
            frames: vec![classic_frame()],
        },
        &[1, 2, 3],
    );
    assert_tags(
        &v1::CanSendResponse {
            committed_count: 1,
            error: Some(valid_error()),
        },
        &[1, 2],
    );
    assert_tags(
        &v1::CanReceiveRequest {
            session_id: "session".to_owned(),
            lease: Some(valid_lease()),
            max_frames: 1,
            timeout_ms: 1,
        },
        &[1, 2, 3, 4],
    );
    assert_tags(
        &v1::CanReceiveResponse {
            frames: vec![v1::ReceivedCanFrame {
                frame: Some(classic_frame()),
                timestamp: None,
            }],
        },
        &[1],
    );
    assert_tags(
        &v1::ReplaceCanFiltersRequest {
            session_id: "session".to_owned(),
            lease: Some(valid_lease()),
            filters: Some(v1::CanFilterSet {
                filters: vec![valid_filter()],
            }),
        },
        &[1, 2, 3],
    );
    assert_tags(
        &v1::GetCanBusStatusRequest {
            session_id: "session".to_owned(),
            lease: Some(valid_lease()),
        },
        &[1, 2],
    );
    assert_tags(
        &v1::GetCanBusStatusResponse {
            status: Some(v1::CanBusStatus {
                state: v1::CanBusState::Active as i32,
                tx_error_counter: None,
                rx_error_counter: None,
            }),
        },
        &[1],
    );
}

#[test]
fn wire_one_keeps_legacy_exact_minor_zero_negotiation() {
    let legacy = v1::HandshakeRequest {
        protocol_major: 1,
        protocol_minor: 0,
        ..Default::default()
    };
    assert_eq!(
        seeed_hal_protocol::handshake_minor_range(&legacy).unwrap(),
        (0, 0)
    );
    assert_eq!(
        seeed_hal_protocol::negotiate_protocol_minor(
            legacy.protocol_major,
            0,
            0,
            seeed_hal_protocol::PROTOCOL_MAJOR,
            seeed_hal_protocol::PROTOCOL_MINOR_MINIMUM,
            seeed_hal_protocol::PROTOCOL_MINOR_MAXIMUM,
        )
        .unwrap(),
        0,
    );
    assert_eq!(
        seeed_hal_protocol::negotiate_protocol_minor(1, 0, 1, 1, 0, 1).unwrap(),
        1,
    );
}

#[test]
fn can_types_round_trip_without_losing_presence_or_variants() {
    let frames = vec![
        CanFrame::classic_data(CanId::standard(0x123).unwrap(), vec![1, 2]).unwrap(),
        CanFrame::classic_remote(CanId::extended(0x1fff).unwrap(), 0).unwrap(),
        CanFrame::fd_data(CanId::extended(0x12345).unwrap(), vec![0; 12], true, true).unwrap(),
        CanFrame::error(vec![CanErrorClass::BusOff, CanErrorClass::Other], vec![9]).unwrap(),
    ];
    for frame in frames {
        assert_eq!(
            CanFrame::try_from(v1::CanFrame::from(&frame)).unwrap(),
            frame
        );
    }

    let timestamp = CanTimestamp::new(0, CanTimestampSource::HostMonotonic, "clock-a").unwrap();
    assert_eq!(
        CanTimestamp::try_from(v1::CanTimestamp::from(&timestamp)).unwrap(),
        timestamp,
    );
    let received = ReceivedCanFrame::new(
        CanFrame::classic_data(CanId::standard(1).unwrap(), vec![]).unwrap(),
        Some(timestamp),
    );
    assert_eq!(
        ReceivedCanFrame::try_from(v1::ReceivedCanFrame::from(&received)).unwrap(),
        received,
    );

    let expectation = CanLinkExpectation::new(
        Some(CanMode::Fd),
        Some(500_000),
        Some(2_000_000),
        Some(false),
        Some(true),
    )
    .unwrap();
    assert_eq!(
        CanLinkExpectation::try_from(v1::CanLinkExpectation::from(&expectation)).unwrap(),
        expectation,
    );
    let nominal = CanBitTiming::new(500_000, Some(875), Some(1)).unwrap();
    let data = CanBitTiming::new(2_000_000, None, Some(2)).unwrap();
    let configure = CanConfigureConfig::new_with_restart(
        CanMode::Fd,
        nominal,
        Some(data),
        false,
        true,
        Some(100),
    )
    .unwrap();
    let open = CanOpenConfig::Configure(configure.clone());
    assert_eq!(
        CanOpenConfig::try_from(v1::CanOpenConfig::from(&open)).unwrap(),
        open,
    );
    let active =
        CanActiveConfig::new(CanMode::Fd, nominal, Some(data), false, true, "clock-a").unwrap();
    assert_eq!(
        CanActiveConfig::try_from(v1::CanActiveConfig::from(&active)).unwrap(),
        active,
    );
    let filters = CanFilterSet::new(vec![
        CanFilter::new(
            0x120,
            0x7f0,
            CanIdFormat::Either,
            CanFrameClasses::new(true, true, true),
        )
        .unwrap(),
    ])
    .unwrap();
    assert_eq!(
        CanFilterSet::try_from(v1::CanFilterSet::from(&filters)).unwrap(),
        filters,
    );
    let status = CanBusStatus::new(CanBusState::Unknown, Some(0), None);
    assert_eq!(
        CanBusStatus::try_from(v1::CanBusStatus::from(&status)).unwrap(),
        status,
    );
}

#[test]
fn can_transport_and_maintenance_lease_round_trip_through_legacy_types() {
    let selector = ResourceSelector::exact(
        ResourceId::parse("can:test").unwrap(),
        IdentityQuality::Strong,
        TransportKind::Can,
    );
    assert_eq!(
        ResourceSelector::try_from(v1::ResourceSelector::from(&selector)).unwrap(),
        selector,
    );
    let lease = LeaseToken::new(
        LeaseId::parse("lease-can").unwrap(),
        7,
        LeaseMode::Maintenance,
    );
    assert_eq!(
        LeaseToken::try_from(v1::LeaseToken::from(&lease)).unwrap(),
        lease,
    );
}

#[test]
fn serial_operation_decoders_reject_can_and_maintenance_values() {
    let can_selector = v1::ResourceSelector {
        resource_id: "can:test".to_owned(),
        minimum_identity_quality: v1::IdentityQuality::Strong as i32,
        transport: v1::TransportKind::Can as i32,
    };
    assert_invalid_message(
        seeed_hal_protocol::serial_selector_from_proto(can_selector).unwrap_err(),
        "serial resource selector",
    );

    let can_descriptor = v1::ResourceDescriptor {
        resource_id: "can:test".to_owned(),
        endpoint: "virtual:can".to_owned(),
        identity_quality: v1::IdentityQuality::Strong as i32,
        transport: v1::TransportKind::Can as i32,
        capabilities: vec!["can.classic/v1".to_owned()],
        ..Default::default()
    };
    assert_invalid_message(
        seeed_hal_protocol::enumerate_serial_response_from_proto(v1::EnumerateSerialResponse {
            resources: vec![can_descriptor],
        })
        .unwrap_err(),
        "enumerate_serial resource",
    );

    let empty_can_descriptor = v1::ResourceDescriptor {
        resource_id: "can:test".to_owned(),
        endpoint: "virtual:can".to_owned(),
        identity_quality: v1::IdentityQuality::Strong as i32,
        transport: v1::TransportKind::Can as i32,
        ..Default::default()
    };
    assert_invalid_message(
        seeed_hal_core::ResourceDescriptor::try_from(empty_can_descriptor).unwrap_err(),
        "CAN resource descriptor",
    );

    let maintenance = v1::OpenSerialResponse {
        session_id: "session".to_owned(),
        lease: Some(v1::LeaseToken {
            mode: v1::LeaseMode::Maintenance as i32,
            ..valid_lease()
        }),
    };
    assert_invalid_message(
        seeed_hal_protocol::open_serial_response_from_proto(maintenance).unwrap_err(),
        "Serial session lease",
    );
}

#[test]
fn unknown_additive_can_fields_are_ignored_safely() {
    let timing = valid_timing();
    let mut encoded = timing.encode_to_vec();
    encoded.extend_from_slice(&[0xa0, 0x06, 0x01]);
    let decoded = v1::CanBitTiming::decode(encoded.as_slice()).unwrap();
    assert_eq!(CanBitTiming::try_from(decoded).unwrap().bitrate(), 500_000);
}

#[test]
fn malformed_ids_and_frame_combinations_fail_closed() {
    for id in [
        v1::CanId {
            value: 0x800,
            format: v1::CanIdFormat::Standard as i32,
        },
        v1::CanId {
            value: 0x2000_0000,
            format: v1::CanIdFormat::Extended as i32,
        },
        v1::CanId {
            value: 1,
            format: v1::CanIdFormat::Either as i32,
        },
        v1::CanId {
            value: 1,
            format: 99,
        },
    ] {
        assert_invalid_message(CanId::try_from(id).unwrap_err(), "can_id");
    }

    let mut cases = Vec::new();
    cases.push(v1::CanFrame {
        kind: v1::CanFrameKind::ClassicData as i32,
        ..Default::default()
    });
    cases.push(v1::CanFrame {
        id: Some(standard_id(1)),
        kind: v1::CanFrameKind::ClassicData as i32,
        data: vec![0; 9],
        ..Default::default()
    });
    cases.push(v1::CanFrame {
        id: Some(standard_id(1)),
        kind: v1::CanFrameKind::ClassicRemote as i32,
        data: vec![1],
        ..Default::default()
    });
    cases.push(v1::CanFrame {
        id: Some(standard_id(1)),
        kind: v1::CanFrameKind::ClassicRemote as i32,
        remote_dlc: 9,
        ..Default::default()
    });
    cases.push(v1::CanFrame {
        id: Some(standard_id(1)),
        kind: v1::CanFrameKind::FdData as i32,
        data: vec![0; 9],
        ..Default::default()
    });
    cases.push(v1::CanFrame {
        kind: v1::CanFrameKind::Error as i32,
        data: vec![0; 9],
        error_classes: vec![v1::CanErrorClass::BusError as i32],
        ..Default::default()
    });
    cases.push(v1::CanFrame {
        kind: v1::CanFrameKind::Error as i32,
        error_classes: vec![v1::CanErrorClass::Unspecified as i32],
        ..Default::default()
    });
    cases.push(v1::CanFrame {
        kind: v1::CanFrameKind::Error as i32,
        id: Some(standard_id(1)),
        error_classes: vec![v1::CanErrorClass::BusError as i32],
        ..Default::default()
    });
    cases.push(v1::CanFrame {
        kind: 99,
        ..Default::default()
    });
    for frame in cases {
        assert_invalid_message(CanFrame::try_from(frame).unwrap_err(), "can_frame");
    }
}

#[test]
fn exact_can_wire_bounds_accept_limits_and_reject_every_fd_gap() {
    assert!(
        CanId::try_from(v1::CanId {
            value: 0x7ff,
            format: v1::CanIdFormat::Standard as i32,
        })
        .is_ok()
    );
    assert!(
        CanId::try_from(v1::CanId {
            value: 0x1fff_ffff,
            format: v1::CanIdFormat::Extended as i32,
        })
        .is_ok()
    );
    assert!(
        CanFrame::try_from(v1::CanFrame {
            id: Some(standard_id(1)),
            kind: v1::CanFrameKind::ClassicData as i32,
            data: vec![0; 8],
            ..Default::default()
        })
        .is_ok()
    );
    assert!(
        CanFrame::try_from(v1::CanFrame {
            id: Some(standard_id(1)),
            kind: v1::CanFrameKind::ClassicRemote as i32,
            remote_dlc: 8,
            ..Default::default()
        })
        .is_ok()
    );
    for length in [0, 1, 8, 12, 16, 20, 24, 32, 48, 64] {
        assert!(
            CanFrame::try_from(v1::CanFrame {
                id: Some(standard_id(1)),
                kind: v1::CanFrameKind::FdData as i32,
                data: vec![0; length],
                ..Default::default()
            })
            .is_ok()
        );
    }
    for length in
        (0..=65).filter(|&length| !matches!(length, 0..=8 | 12 | 16 | 20 | 24 | 32 | 48 | 64))
    {
        assert_invalid_message(
            CanFrame::try_from(v1::CanFrame {
                id: Some(standard_id(1)),
                kind: v1::CanFrameKind::FdData as i32,
                data: vec![0; length],
                ..Default::default()
            })
            .unwrap_err(),
            "can_frame.data",
        );
    }
    assert!(
        CanFrame::try_from(v1::CanFrame {
            kind: v1::CanFrameKind::Error as i32,
            error_classes: vec![
                v1::CanErrorClass::Other as i32;
                seeed_hal_can::MAX_CAN_ERROR_CLASSES
            ],
            ..Default::default()
        })
        .is_ok()
    );
    assert_invalid_message(
        CanFrame::try_from(v1::CanFrame {
            kind: v1::CanFrameKind::Error as i32,
            error_classes: vec![
                v1::CanErrorClass::Other as i32;
                seeed_hal_can::MAX_CAN_ERROR_CLASSES + 1
            ],
            ..Default::default()
        })
        .unwrap_err(),
        "can_frame.error_classes/data",
    );
    assert!(
        CanTimestamp::try_from(v1::CanTimestamp {
            timestamp_ns: u64::MAX,
            source: v1::CanTimestampSource::Hardware as i32,
            clock_domain: "x".repeat(255),
        })
        .is_ok()
    );
    assert!(
        CanBitTiming::try_from(v1::CanBitTiming {
            bitrate: u32::MAX,
            sample_point_permill: Some(1),
            sjw: Some(u32::from(u16::MAX)),
        })
        .is_ok()
    );
    assert!(
        CanBitTiming::try_from(v1::CanBitTiming {
            bitrate: 1,
            sample_point_permill: Some(999),
            sjw: Some(1),
        })
        .is_ok()
    );
    assert!(
        CanFilterSet::try_from(v1::CanFilterSet {
            filters: vec![valid_filter(); 64],
        })
        .is_ok()
    );
    assert!(seeed_hal_protocol::send_can_frames_from_proto(vec![classic_frame(); 64]).is_ok());
    assert!(seeed_hal_protocol::can_receive_parameters(64, u64::MAX).is_ok());
}

#[test]
fn malformed_timestamps_timings_and_configurations_fail_closed() {
    for timestamp in [
        v1::CanTimestamp {
            timestamp_ns: 1,
            source: v1::CanTimestampSource::Unspecified as i32,
            clock_domain: "clock".to_owned(),
        },
        v1::CanTimestamp {
            timestamp_ns: 1,
            source: 99,
            clock_domain: "clock".to_owned(),
        },
        v1::CanTimestamp {
            timestamp_ns: 1,
            source: v1::CanTimestampSource::Hardware as i32,
            clock_domain: String::new(),
        },
        v1::CanTimestamp {
            timestamp_ns: 1,
            source: v1::CanTimestampSource::Hardware as i32,
            clock_domain: "é".to_owned(),
        },
        v1::CanTimestamp {
            timestamp_ns: 1,
            source: v1::CanTimestampSource::Hardware as i32,
            clock_domain: "x".repeat(256),
        },
    ] {
        assert_invalid_message(
            CanTimestamp::try_from(timestamp).unwrap_err(),
            "can_timestamp",
        );
    }

    for timing in [
        v1::CanBitTiming {
            bitrate: 0,
            ..Default::default()
        },
        v1::CanBitTiming {
            bitrate: 1,
            sample_point_permill: Some(0),
            sjw: None,
        },
        v1::CanBitTiming {
            bitrate: 1,
            sample_point_permill: Some(1000),
            sjw: None,
        },
        v1::CanBitTiming {
            bitrate: 1,
            sample_point_permill: None,
            sjw: Some(0),
        },
        v1::CanBitTiming {
            bitrate: 1,
            sample_point_permill: None,
            sjw: Some(u32::from(u16::MAX) + 1),
        },
    ] {
        assert_invalid_message(
            CanBitTiming::try_from(timing).unwrap_err(),
            "can_bit_timing",
        );
    }

    let invalid_configs = [
        v1::CanOpenConfig { config: None },
        v1::CanOpenConfig {
            config: Some(v1::can_open_config::Config::Configure(
                v1::CanConfigureConfig {
                    mode: v1::CanMode::Classic as i32,
                    nominal: Some(valid_timing()),
                    data: Some(valid_timing()),
                    ..Default::default()
                },
            )),
        },
        v1::CanOpenConfig {
            config: Some(v1::can_open_config::Config::Configure(
                v1::CanConfigureConfig {
                    mode: v1::CanMode::Fd as i32,
                    nominal: Some(valid_timing()),
                    data: None,
                    ..Default::default()
                },
            )),
        },
        v1::CanOpenConfig {
            config: Some(v1::can_open_config::Config::Configure(
                v1::CanConfigureConfig {
                    mode: v1::CanMode::Classic as i32,
                    nominal: None,
                    ..Default::default()
                },
            )),
        },
        v1::CanOpenConfig {
            config: Some(v1::can_open_config::Config::Configure(
                v1::CanConfigureConfig {
                    mode: v1::CanMode::Classic as i32,
                    nominal: Some(valid_timing()),
                    restart_ms: Some(0),
                    ..Default::default()
                },
            )),
        },
        v1::CanOpenConfig {
            config: Some(v1::can_open_config::Config::Attach(
                v1::CanLinkExpectation {
                    mode: Some(v1::CanMode::Classic as i32),
                    data_bitrate: Some(1),
                    ..Default::default()
                },
            )),
        },
        v1::CanOpenConfig {
            config: Some(v1::can_open_config::Config::Attach(
                v1::CanLinkExpectation {
                    nominal_bitrate: Some(0),
                    ..Default::default()
                },
            )),
        },
    ];
    for config in invalid_configs {
        assert_invalid_message(CanOpenConfig::try_from(config).unwrap_err(), "can_");
    }

    let invalid_active = v1::CanActiveConfig {
        mode: v1::CanMode::Classic as i32,
        nominal: Some(valid_timing()),
        data: None,
        clock_domain: String::new(),
        ..Default::default()
    };
    assert_invalid_message(
        CanActiveConfig::try_from(invalid_active).unwrap_err(),
        "can_active_config",
    );
}

#[test]
fn filter_batch_receive_and_required_nested_bounds_fail_closed() {
    let missing_classes = v1::CanFilter {
        classes: None,
        ..valid_filter()
    };
    assert_invalid_message(
        CanFilter::try_from(missing_classes).unwrap_err(),
        "can_filter.classes",
    );
    for filter in [
        v1::CanFilter {
            id: 0x800,
            ..valid_filter()
        },
        v1::CanFilter {
            mask: 0x800,
            ..valid_filter()
        },
        v1::CanFilter {
            classes: Some(v1::CanFrameClasses::default()),
            ..valid_filter()
        },
        v1::CanFilter {
            format: 99,
            ..valid_filter()
        },
    ] {
        assert_invalid_message(CanFilter::try_from(filter).unwrap_err(), "can_filter");
    }
    assert_invalid_message(
        CanFilterSet::try_from(v1::CanFilterSet {
            filters: vec![valid_filter(); 65],
        })
        .unwrap_err(),
        "can_filter_set.filters",
    );
    assert_invalid_message(
        seeed_hal_protocol::send_can_frames_from_proto(Vec::new()).unwrap_err(),
        "can_send.frames",
    );
    assert_invalid_message(
        seeed_hal_protocol::send_can_frames_from_proto(vec![classic_frame(); 65]).unwrap_err(),
        "can_send.frames",
    );
    for max_frames in [0, 65] {
        assert_invalid_message(
            seeed_hal_protocol::can_receive_parameters(max_frames, 0).unwrap_err(),
            "can_receive.max_frames",
        );
    }
    assert_eq!(
        seeed_hal_protocol::can_receive_parameters(1, 0).unwrap().1,
        std::time::Duration::ZERO,
    );
    assert_invalid_message(
        seeed_hal_protocol::received_can_frames_from_proto(
            vec![
                v1::ReceivedCanFrame {
                    frame: Some(classic_frame()),
                    timestamp: None,
                };
                2
            ],
            1,
        )
        .unwrap_err(),
        "can_receive_response.frames",
    );
    assert_invalid_message(
        ReceivedCanFrame::try_from(v1::ReceivedCanFrame::default()).unwrap_err(),
        "received_can_frame.frame",
    );
    assert_invalid_message(
        CanBusStatus::try_from(v1::CanBusStatus::default()).unwrap_err(),
        "can_bus_status.state",
    );
    assert_invalid_message(
        CanBusStatus::try_from(v1::CanBusStatus {
            state: 99,
            ..Default::default()
        })
        .unwrap_err(),
        "can_bus_status.state",
    );
    assert_invalid_message(
        seeed_hal_protocol::get_can_bus_status_response_from_proto(
            v1::GetCanBusStatusResponse::default(),
        )
        .unwrap_err(),
        "get_can_bus_status.status",
    );
}

#[test]
fn open_request_rejects_missing_values_and_non_can_transport() {
    let attach = v1::CanOpenConfig {
        config: Some(v1::can_open_config::Config::Attach(
            v1::CanLinkExpectation::default(),
        )),
    };
    let filters = v1::CanFilterSet::default();
    for request in [
        v1::OpenCanRequest {
            selector: None,
            mode: v1::LeaseMode::Observe as i32,
            config: Some(attach),
            filters: Some(filters.clone()),
        },
        v1::OpenCanRequest {
            selector: Some(v1::ResourceSelector {
                resource_id: "serial:test".to_owned(),
                minimum_identity_quality: v1::IdentityQuality::Strong as i32,
                transport: v1::TransportKind::Serial as i32,
            }),
            mode: v1::LeaseMode::Observe as i32,
            config: Some(attach),
            filters: Some(filters.clone()),
        },
        v1::OpenCanRequest {
            selector: Some(v1::ResourceSelector {
                resource_id: "can:test".to_owned(),
                minimum_identity_quality: v1::IdentityQuality::Strong as i32,
                transport: v1::TransportKind::Can as i32,
            }),
            mode: v1::LeaseMode::Unspecified as i32,
            config: Some(attach),
            filters: Some(filters.clone()),
        },
        v1::OpenCanRequest {
            selector: Some(v1::ResourceSelector {
                resource_id: "can:test".to_owned(),
                minimum_identity_quality: v1::IdentityQuality::Strong as i32,
                transport: v1::TransportKind::Can as i32,
            }),
            mode: v1::LeaseMode::Observe as i32,
            config: None,
            filters: Some(filters.clone()),
        },
        v1::OpenCanRequest {
            selector: Some(v1::ResourceSelector {
                resource_id: "can:test".to_owned(),
                minimum_identity_quality: v1::IdentityQuality::Strong as i32,
                transport: v1::TransportKind::Can as i32,
            }),
            mode: v1::LeaseMode::Observe as i32,
            config: Some(attach),
            filters: None,
        },
    ] {
        assert_invalid_message(
            seeed_hal_protocol::open_can_request_from_proto(request).unwrap_err(),
            "open_can",
        );
    }

    assert_invalid_message(
        seeed_hal_protocol::enumerate_can_response_from_proto(v1::EnumerateCanResponse {
            resources: vec![v1::ResourceDescriptor {
                resource_id: "serial:test".to_owned(),
                endpoint: "virtual:serial".to_owned(),
                identity_quality: v1::IdentityQuality::Strong as i32,
                transport: v1::TransportKind::Serial as i32,
                properties: Default::default(),
                capabilities: vec!["serial.bytes/v1".to_owned()],
            }],
        })
        .unwrap_err(),
        "enumerate_can.resources.transport",
    );

    assert_invalid_message(
        seeed_hal_protocol::open_can_response_from_proto(
            v1::OpenCanResponse {
                session_id: "session".to_owned(),
                lease: Some(valid_lease()),
            },
            LeaseMode::Observe,
        )
        .unwrap_err(),
        "open_can_response.lease.mode",
    );
}

#[test]
fn send_response_validates_success_and_partial_error_against_input_length() {
    let success = seeed_hal_protocol::can_send_response_from_proto(
        v1::CanSendResponse {
            committed_count: 2,
            error: None,
        },
        2,
    )
    .unwrap();
    assert!(success.is_ok());

    for response in [
        v1::CanSendResponse {
            committed_count: 1,
            error: None,
        },
        v1::CanSendResponse {
            committed_count: 2,
            error: Some(valid_error()),
        },
        v1::CanSendResponse {
            committed_count: 3,
            error: Some(valid_error()),
        },
    ] {
        assert_invalid_message(
            seeed_hal_protocol::can_send_response_from_proto(response, 2).unwrap_err(),
            "committed_count",
        );
    }

    let error = HalError::new(
        "runtime.transport.failed",
        ErrorCategory::Unavailable,
        "can.send_batch",
        true,
        "backend failed",
    )
    .unwrap();
    let partial = CanBatchSendError::backend_prefix(error, 1);
    let encoded = seeed_hal_protocol::can_send_response_to_proto(Err(&partial), 2).unwrap();
    assert_eq!(encoded.committed_count, 1);
    let decoded = seeed_hal_protocol::can_send_response_from_proto(encoded, 2)
        .unwrap()
        .unwrap_err();
    assert_eq!(decoded.committed(), 1);
    assert_eq!(decoded.error().name().as_str(), "runtime.transport.failed");

    let invalid_nested = seeed_hal_protocol::can_send_response_from_proto(
        v1::CanSendResponse {
            committed_count: 0,
            error: Some(v1::Error::default()),
        },
        1,
    )
    .unwrap_err();
    assert_invalid_message(invalid_nested, "category");
}
