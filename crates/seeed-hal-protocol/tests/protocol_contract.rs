use prost::Message;
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
