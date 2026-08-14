use prost::Message;
use seeed_hal_protocol::v1::{self, envelope};

#[test]
fn handshake_uses_stable_envelope_field_numbers() {
    assert_eq!(seeed_hal_protocol::MAX_FRAME_BYTES, 1_048_576);
    let envelope = v1::Envelope {
        request_id: 7,
        payload: Some(envelope::Payload::HandshakeRequest(v1::HandshakeRequest {
            startup_token: vec![0x5a; 32],
            protocol_major: 1,
            protocol_minor: 0,
            required_capabilities: vec!["serial.bytes/v1".to_owned()],
            max_frame_bytes: 1024 * 1024,
            max_read_bytes: 4096,
            max_write_bytes: 4096,
        })),
    };

    let encoded = envelope.encode_to_vec();
    assert_eq!(encoded[0], 0x08, "request_id must remain field 1");
    assert!(
        encoded.windows(1).any(|bytes| bytes == [0x52]),
        "handshake_request must remain field 10"
    );
    assert_eq!(v1::Envelope::decode(encoded.as_slice()).unwrap(), envelope);
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
