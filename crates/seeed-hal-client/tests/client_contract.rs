#[cfg(unix)]
use bytes::Bytes;
#[cfg(unix)]
use seeed_hal_client::{ConnectionOptions, HalClient};
#[cfg(unix)]
use seeed_hal_serial::SerialConfig;

#[cfg(unix)]
const TOKEN: [u8; 32] = [0x5a; 32];

#[cfg(unix)]
mod fake {
    use bytes::Bytes;
    use futures_util::{SinkExt, StreamExt};
    use prost::Message;
    use seeed_hal_protocol::v1::{self, envelope};
    use tokio::net::{UnixListener, UnixStream};
    use tokio_util::codec::{Framed, LengthDelimitedCodec};

    pub type Wire = Framed<UnixStream, LengthDelimitedCodec>;

    pub fn endpoint(label: &str) -> std::path::PathBuf {
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::path::PathBuf::from(format!(
            "/tmp/shc-{label}-{}-{nonce}.sock",
            std::process::id()
        ))
    }

    pub fn bind(path: &std::path::Path) -> UnixListener {
        UnixListener::bind(path).unwrap()
    }

    pub async fn accept_and_handshake(listener: UnixListener) -> Wire {
        let (io, _) = listener.accept().await.unwrap();
        let mut wire = Framed::new(io, codec());
        let request = recv(&mut wire).await;
        let handshake = match request.payload.unwrap() {
            envelope::Payload::HandshakeRequest(request) => request,
            _ => panic!("expected handshake request"),
        };
        assert_eq!(handshake.startup_token, super::TOKEN);
        send(
            &mut wire,
            v1::Envelope {
                request_id: request.request_id,
                payload: Some(envelope::Payload::HandshakeResponse(
                    v1::HandshakeResponse {
                        protocol_major: 1,
                        protocol_minor: 0,
                        capabilities: vec!["serial.bytes/v1".to_owned()],
                        max_frame_bytes: handshake.max_frame_bytes,
                        max_read_bytes: handshake.max_read_bytes,
                        max_write_bytes: handshake.max_write_bytes,
                    },
                )),
            },
        )
        .await;
        wire
    }

    pub async fn recv(wire: &mut Wire) -> v1::Envelope {
        let frame = wire.next().await.unwrap().unwrap();
        v1::Envelope::decode(frame).unwrap()
    }

    pub async fn send(wire: &mut Wire, envelope: v1::Envelope) {
        wire.send(Bytes::from(envelope.encode_to_vec()))
            .await
            .unwrap();
    }

    pub async fn send_raw(wire: &mut Wire, bytes: &'static [u8]) {
        wire.send(Bytes::from_static(bytes)).await.unwrap();
    }

    pub fn enumerate_response(request_id: u64, resource_id: &str) -> v1::Envelope {
        v1::Envelope {
            request_id,
            payload: Some(envelope::Payload::EnumerateSerialResponse(
                v1::EnumerateSerialResponse {
                    resources: vec![v1::ResourceDescriptor {
                        resource_id: resource_id.to_owned(),
                        endpoint: format!("virtual://{resource_id}"),
                        identity_quality: v1::IdentityQuality::Strong as i32,
                        transport: v1::TransportKind::Serial as i32,
                        properties: Default::default(),
                    }],
                },
            )),
        }
    }

    fn codec() -> LengthDelimitedCodec {
        LengthDelimitedCodec::builder()
            .max_frame_length(seeed_hal_protocol::MAX_FRAME_BYTES)
            .new_codec()
    }
}

#[cfg(unix)]
#[tokio::test]
async fn rust_client_round_trips_serial_through_broker() {
    use seeed_hal_broker::{Broker, StartupToken, listener::UnixBroker};
    use seeed_hal_runtime::HalRuntime;
    use seeed_hal_testkit::VirtualSerialAdapter;

    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let directory = std::path::PathBuf::from(format!("/tmp/shc-{}-{nonce}", std::process::id()));
    let runtime = HalRuntime::builder()
        .serial_adapter(VirtualSerialAdapter::loopback("serial:virtual:client"))
        .build();
    let broker = UnixBroker::bind(
        Broker::with_startup_token(runtime, StartupToken::from_bytes(TOKEN)),
        &directory,
    )
    .await
    .unwrap();
    let options = ConnectionOptions::new(broker.socket_path(), TOKEN);
    let server = tokio::spawn(async move { broker.serve_one().await.unwrap() });

    let client = HalClient::connect(options).await.unwrap();
    let descriptor = client.enumerate_serial().await.unwrap().remove(0);
    let serial = client
        .open_serial(descriptor.selector(), SerialConfig::default())
        .await
        .unwrap();
    serial.write(Bytes::from_static(b"hello")).await.unwrap();
    assert_eq!(&serial.read(5).await.unwrap()[..], b"hello");
    serial.close().await.unwrap();
    client.close().await.unwrap();

    assert!(server.await.unwrap().cleanup_error().is_none());
    std::fs::remove_dir_all(directory).unwrap();
}

#[cfg(unix)]
#[tokio::test]
async fn reversed_responses_stay_correlated_to_their_callers() {
    let endpoint = fake::endpoint("correlation");
    let listener = fake::bind(&endpoint);
    let server = tokio::spawn(async move {
        let mut wire = fake::accept_and_handshake(listener).await;
        let first = fake::recv(&mut wire).await;
        let second = fake::recv(&mut wire).await;
        fake::send(
            &mut wire,
            fake::enumerate_response(second.request_id, "serial:fake:second"),
        )
        .await;
        fake::send(
            &mut wire,
            fake::enumerate_response(first.request_id, "serial:fake:first"),
        )
        .await;
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    });
    let client = HalClient::connect(ConnectionOptions::new(&endpoint, TOKEN))
        .await
        .unwrap();
    let (first, second) = tokio::join!(client.enumerate_serial(), client.enumerate_serial());
    assert_eq!(first.unwrap()[0].id().as_str(), "serial:fake:first");
    assert_eq!(second.unwrap()[0].id().as_str(), "serial:fake:second");
    client.close().await.unwrap();
    server.await.unwrap();
    std::fs::remove_file(endpoint).unwrap();
}

#[cfg(unix)]
#[tokio::test]
async fn event_does_not_consume_a_pending_response() {
    use seeed_hal_protocol::v1::{self, envelope};

    let endpoint = fake::endpoint("event");
    let listener = fake::bind(&endpoint);
    let server = tokio::spawn(async move {
        let mut wire = fake::accept_and_handshake(listener).await;
        let request = fake::recv(&mut wire).await;
        fake::send(
            &mut wire,
            v1::Envelope {
                request_id: 0,
                payload: Some(envelope::Payload::RuntimeEvent(v1::RuntimeEvent {
                    sequence: 7,
                    kind: v1::RuntimeEventKind::SessionOpened as i32,
                    name: "session.opened".to_owned(),
                    resource_id: "serial:fake:event".to_owned(),
                    session_id: "session-fake-event".to_owned(),
                    owner_id: "owner-fake-event".to_owned(),
                    lease_generation: 3,
                })),
            },
        )
        .await;
        fake::send(
            &mut wire,
            v1::Envelope {
                request_id: 0,
                payload: Some(envelope::Payload::RuntimeEvent(v1::RuntimeEvent {
                    sequence: 8,
                    kind: v1::RuntimeEventKind::SessionClosed as i32,
                    name: "session.closed".to_owned(),
                    resource_id: "serial:fake:event".to_owned(),
                    session_id: "session-fake-event".to_owned(),
                    owner_id: "owner-fake-event".to_owned(),
                    lease_generation: 3,
                })),
            },
        )
        .await;
        fake::send(
            &mut wire,
            fake::enumerate_response(request.request_id, "serial:fake:event"),
        )
        .await;
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    });
    let client = HalClient::connect(ConnectionOptions::new(&endpoint, TOKEN))
        .await
        .unwrap();
    let mut events = client.subscribe();
    let resources = client.enumerate_serial().await.unwrap();
    let event = events.recv().await.unwrap();
    let next_event = events.recv().await.unwrap();
    assert_eq!(resources[0].id().as_str(), "serial:fake:event");
    assert_eq!(event.sequence(), 7);
    assert_eq!(event.name(), "session.opened");
    assert_eq!(next_event.sequence(), 8);
    assert_eq!(next_event.name(), "session.closed");
    client.close().await.unwrap();
    server.await.unwrap();
    std::fs::remove_file(endpoint).unwrap();
}

#[cfg(unix)]
#[tokio::test]
async fn disconnect_resolves_pending_request_structurally() {
    let endpoint = fake::endpoint("disconnect");
    let listener = fake::bind(&endpoint);
    let server = tokio::spawn(async move {
        let mut wire = fake::accept_and_handshake(listener).await;
        let _request = fake::recv(&mut wire).await;
    });
    let client = HalClient::connect(ConnectionOptions::new(&endpoint, TOKEN))
        .await
        .unwrap();
    let error = client.enumerate_serial().await.unwrap_err();
    assert_eq!(error.name().as_str(), "runtime.broker.disconnected");
    client.close().await.unwrap();
    server.await.unwrap();
    std::fs::remove_file(endpoint).unwrap();
}

#[cfg(unix)]
#[tokio::test]
async fn pending_capacity_rejects_overflow_without_unbounded_waiting() {
    let endpoint = fake::endpoint("backpressure");
    let listener = fake::bind(&endpoint);
    let server = tokio::spawn(async move {
        let mut wire = fake::accept_and_handshake(listener).await;
        let _request = fake::recv(&mut wire).await;
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    });
    let client =
        HalClient::connect(ConnectionOptions::new(&endpoint, TOKEN).with_queue_capacities(1, 4, 4))
            .await
            .unwrap();
    let pending_client = client.clone();
    let pending = tokio::spawn(async move { pending_client.enumerate_serial().await });
    tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    let error = client.enumerate_serial().await.unwrap_err();
    assert_eq!(error.name().as_str(), "runtime.queue.full");
    assert_eq!(
        pending.await.unwrap().unwrap_err().name().as_str(),
        "runtime.broker.disconnected"
    );
    client.close().await.unwrap();
    server.await.unwrap();
    std::fs::remove_file(endpoint).unwrap();
}

#[cfg(unix)]
#[tokio::test]
async fn cancelling_a_caller_releases_pending_capacity_and_discards_its_response() {
    let endpoint = fake::endpoint("cancel");
    let listener = fake::bind(&endpoint);
    let server = tokio::spawn(async move {
        let mut wire = fake::accept_and_handshake(listener).await;
        let cancelled = fake::recv(&mut wire).await;
        let next = fake::recv(&mut wire).await;
        fake::send(
            &mut wire,
            fake::enumerate_response(next.request_id, "serial:fake:next"),
        )
        .await;
        fake::send(
            &mut wire,
            fake::enumerate_response(cancelled.request_id, "serial:fake:cancelled"),
        )
        .await;
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    });
    let client =
        HalClient::connect(ConnectionOptions::new(&endpoint, TOKEN).with_queue_capacities(1, 4, 4))
            .await
            .unwrap();
    let cancelled_client = client.clone();
    let cancelled = tokio::spawn(async move { cancelled_client.enumerate_serial().await });
    tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    cancelled.abort();
    assert!(cancelled.await.unwrap_err().is_cancelled());
    let resources = client.enumerate_serial().await.unwrap();
    assert_eq!(resources[0].id().as_str(), "serial:fake:next");
    tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    client.close().await.unwrap();
    server.await.unwrap();
    std::fs::remove_file(endpoint).unwrap();
}

#[cfg(unix)]
#[tokio::test]
async fn unknown_response_id_fails_the_connection_closed() {
    let endpoint = fake::endpoint("unknown-response");
    let listener = fake::bind(&endpoint);
    let server = tokio::spawn(async move {
        let mut wire = fake::accept_and_handshake(listener).await;
        fake::send(
            &mut wire,
            fake::enumerate_response(999, "serial:fake:unknown"),
        )
        .await;
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    });
    let client = HalClient::connect(ConnectionOptions::new(&endpoint, TOKEN))
        .await
        .unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    let error = client.enumerate_serial().await.unwrap_err();
    assert_eq!(error.name().as_str(), "runtime.protocol.unknown_response");
    client.close().await.unwrap();
    server.await.unwrap();
    std::fs::remove_file(endpoint).unwrap();
}

#[cfg(unix)]
#[tokio::test]
async fn duplicate_response_fails_the_connection_closed() {
    use seeed_hal_protocol::v1::{self, envelope};

    let endpoint = fake::endpoint("duplicate-response");
    let listener = fake::bind(&endpoint);
    let server = tokio::spawn(async move {
        let mut wire = fake::accept_and_handshake(listener).await;
        fake::send(
            &mut wire,
            v1::Envelope {
                request_id: 1,
                payload: Some(envelope::Payload::HandshakeResponse(
                    v1::HandshakeResponse {
                        protocol_major: 1,
                        protocol_minor: 0,
                        capabilities: vec!["serial.bytes/v1".to_owned()],
                        max_frame_bytes: seeed_hal_protocol::MAX_FRAME_BYTES as u32,
                        max_read_bytes: 64 * 1024,
                        max_write_bytes: 64 * 1024,
                    },
                )),
            },
        )
        .await;
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    });
    let client = HalClient::connect(ConnectionOptions::new(&endpoint, TOKEN))
        .await
        .unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    let error = client.enumerate_serial().await.unwrap_err();
    assert_eq!(error.name().as_str(), "runtime.protocol.duplicate_response");
    client.close().await.unwrap();
    server.await.unwrap();
    std::fs::remove_file(endpoint).unwrap();
}

#[cfg(unix)]
#[tokio::test]
async fn mismatched_response_payload_fails_pending_and_connection() {
    use seeed_hal_protocol::v1::{self, envelope};

    let endpoint = fake::endpoint("mismatch");
    let listener = fake::bind(&endpoint);
    let server = tokio::spawn(async move {
        let mut wire = fake::accept_and_handshake(listener).await;
        let request = fake::recv(&mut wire).await;
        fake::send(
            &mut wire,
            v1::Envelope {
                request_id: request.request_id,
                payload: Some(envelope::Payload::SerialWriteResponse(v1::Empty {})),
            },
        )
        .await;
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    });
    let client = HalClient::connect(ConnectionOptions::new(&endpoint, TOKEN))
        .await
        .unwrap();
    let error = client.enumerate_serial().await.unwrap_err();
    assert_eq!(
        error.name().as_str(),
        "runtime.protocol.unexpected_response"
    );
    assert_eq!(
        client.enumerate_serial().await.unwrap_err().name().as_str(),
        "runtime.protocol.unexpected_response"
    );
    client.close().await.unwrap();
    server.await.unwrap();
    std::fs::remove_file(endpoint).unwrap();
}

#[cfg(unix)]
#[tokio::test]
async fn malformed_protobuf_fails_pending_requests_structurally() {
    let endpoint = fake::endpoint("malformed");
    let listener = fake::bind(&endpoint);
    let server = tokio::spawn(async move {
        let mut wire = fake::accept_and_handshake(listener).await;
        let _request = fake::recv(&mut wire).await;
        fake::send_raw(&mut wire, &[0xff]).await;
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    });
    let client = HalClient::connect(ConnectionOptions::new(&endpoint, TOKEN))
        .await
        .unwrap();
    let error = client.enumerate_serial().await.unwrap_err();
    assert_eq!(error.name().as_str(), "runtime.protocol.invalid_message");
    client.close().await.unwrap();
    server.await.unwrap();
    std::fs::remove_file(endpoint).unwrap();
}
