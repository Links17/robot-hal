#[cfg(unix)]
use bytes::Bytes;
#[cfg(unix)]
use seeed_hal_client::{ConnectionOptions, HalClient};
#[cfg(unix)]
use seeed_hal_serial::SerialConfig;

#[cfg(unix)]
const TOKEN: [u8; 32] = [0x5a; 32];

#[cfg(unix)]
async fn test_deadline<T>(
    future: impl std::future::Future<Output = T>,
    message: &'static str,
) -> T {
    tokio::time::timeout(std::time::Duration::from_secs(1), future)
        .await
        .expect(message)
}

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
        let (io, _) = super::test_deadline(listener.accept(), "fake broker must accept the client")
            .await
            .unwrap();
        let mut wire = Framed::new(io, codec());
        let request = recv(&mut wire).await;
        let handshake = match request.payload.unwrap() {
            envelope::Payload::HandshakeRequest(request) => request,
            _ => panic!("expected handshake request"),
        };
        assert_eq!(handshake.startup_token, super::TOKEN);
        assert_eq!(handshake.protocol_minor_minimum, 0);
        assert_eq!(handshake.protocol_minor_maximum, 0);
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
                        protocol_minor_minimum: 0,
                        protocol_minor_maximum: 0,
                    },
                )),
            },
        )
        .await;
        wire
    }

    pub async fn recv(wire: &mut Wire) -> v1::Envelope {
        let frame = super::test_deadline(wire.next(), "fake broker must receive the next frame")
            .await
            .unwrap()
            .unwrap();
        v1::Envelope::decode(frame).unwrap()
    }

    pub async fn send(wire: &mut Wire, envelope: v1::Envelope) {
        wire.send(Bytes::from(envelope.encode_to_vec()))
            .await
            .unwrap();
    }

    pub async fn send_raw(wire: &mut Wire, bytes: &[u8]) {
        wire.send(Bytes::copy_from_slice(bytes)).await.unwrap();
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
                        capabilities: vec!["serial.bytes/v1".to_owned()],
                    }],
                },
            )),
        }
    }

    pub fn error_response(request_id: u64, error: v1::Error) -> v1::Envelope {
        v1::Envelope {
            request_id,
            payload: Some(envelope::Payload::Error(error)),
        }
    }

    pub async fn respond_enumerate_and_open(wire: &mut Wire, resource_id: &str) {
        let enumerate = recv(wire).await;
        send(wire, enumerate_response(enumerate.request_id, resource_id)).await;
        let open = recv(wire).await;
        send(
            wire,
            v1::Envelope {
                request_id: open.request_id,
                payload: Some(envelope::Payload::OpenSerialResponse(
                    v1::OpenSerialResponse {
                        session_id: format!("session-{resource_id}"),
                        lease: Some(v1::LeaseToken {
                            lease_id: format!("lease-{resource_id}"),
                            generation: 1,
                            mode: v1::LeaseMode::Control as i32,
                        }),
                    },
                )),
            },
        )
        .await;
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
    assert_eq!(client.protocol_minor(), 0);
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
async fn rich_broker_error_preserves_all_wire_details() {
    use seeed_hal_core::ErrorCategory;
    use seeed_hal_protocol::v1;
    use std::collections::HashMap;

    let endpoint = fake::endpoint("rich-error");
    let listener = fake::bind(&endpoint);
    let server = tokio::spawn(async move {
        let mut wire = fake::accept_and_handshake(listener).await;
        let request = fake::recv(&mut wire).await;
        fake::send(
            &mut wire,
            fake::error_response(
                request.request_id,
                v1::Error {
                    name: "runtime.resource.failed".to_owned(),
                    category: v1::ErrorCategory::Unavailable as i32,
                    operation: "runtime.serial.open".to_owned(),
                    retryable: true,
                    debug_message: "adapter failed to open resource".to_owned(),
                    resource_id: "serial:virtual:rich".to_owned(),
                    platform_code: "platform-code".to_owned(),
                    vendor_code: "vendor-code".to_owned(),
                    context: HashMap::from([
                        ("attempt".to_owned(), "2".to_owned()),
                        ("phase".to_owned(), "open".to_owned()),
                    ]),
                },
            ),
        )
        .await;
    });
    let client = HalClient::connect(ConnectionOptions::new(&endpoint, TOKEN))
        .await
        .unwrap();
    let error = client.enumerate_serial().await.unwrap_err();
    assert_eq!(error.name().as_str(), "runtime.resource.failed");
    assert_eq!(error.category(), ErrorCategory::Unavailable);
    assert_eq!(error.operation().as_str(), "runtime.serial.open");
    assert!(error.retryable());
    assert_eq!(error.debug_message(), "adapter failed to open resource");
    assert_eq!(error.resource_id().unwrap().as_str(), "serial:virtual:rich");
    assert_eq!(error.platform_code(), Some("platform-code"));
    assert_eq!(error.vendor_code(), Some("vendor-code"));
    assert_eq!(
        error.context().iter().collect::<Vec<_>>(),
        vec![("attempt", "2"), ("phase", "open")]
    );
    client.close().await.unwrap();
    server.await.unwrap();
    std::fs::remove_file(endpoint).unwrap();
}

#[cfg(unix)]
#[tokio::test]
async fn legacy_broker_error_preserves_fields_one_through_five() {
    use seeed_hal_core::ErrorCategory;
    use seeed_hal_protocol::v1;

    let endpoint = fake::endpoint("legacy-error");
    let listener = fake::bind(&endpoint);
    let server = tokio::spawn(async move {
        let mut wire = fake::accept_and_handshake(listener).await;
        let request = fake::recv(&mut wire).await;
        fake::send(
            &mut wire,
            fake::error_response(
                request.request_id,
                v1::Error {
                    name: "runtime.resource.not_found".to_owned(),
                    category: v1::ErrorCategory::NotFound as i32,
                    operation: "runtime.serial.enumerate".to_owned(),
                    retryable: false,
                    debug_message: "resource was not found".to_owned(),
                    ..Default::default()
                },
            ),
        )
        .await;
    });
    let client = HalClient::connect(ConnectionOptions::new(&endpoint, TOKEN))
        .await
        .unwrap();
    let error = client.enumerate_serial().await.unwrap_err();
    assert_eq!(error.name().as_str(), "runtime.resource.not_found");
    assert_eq!(error.category(), ErrorCategory::NotFound);
    assert_eq!(error.operation().as_str(), "runtime.serial.enumerate");
    assert!(!error.retryable());
    assert_eq!(error.debug_message(), "resource was not found");
    assert!(error.resource_id().is_none());
    assert!(error.platform_code().is_none());
    assert!(error.vendor_code().is_none());
    assert!(error.context().is_empty());
    client.close().await.unwrap();
    server.await.unwrap();
    std::fs::remove_file(endpoint).unwrap();
}

#[cfg(unix)]
#[tokio::test]
async fn malformed_normal_error_terminates_and_fans_out_to_pending_requests() {
    use seeed_hal_protocol::v1;
    use std::collections::HashMap;

    let endpoint = fake::endpoint("malformed-error");
    let listener = fake::bind(&endpoint);
    let server = tokio::spawn(async move {
        let mut wire = fake::accept_and_handshake(listener).await;
        let first = fake::recv(&mut wire).await;
        let _second = fake::recv(&mut wire).await;
        fake::send(
            &mut wire,
            fake::error_response(
                first.request_id,
                v1::Error {
                    name: "runtime.resource.failed".to_owned(),
                    category: v1::ErrorCategory::Internal as i32,
                    operation: "runtime.serial.enumerate".to_owned(),
                    context: HashMap::from([("InvalidKey".to_owned(), "value".to_owned())]),
                    ..Default::default()
                },
            ),
        )
        .await;
    });
    let client = HalClient::connect(ConnectionOptions::new(&endpoint, TOKEN))
        .await
        .unwrap();
    let first_client = client.clone();
    let second_client = client.clone();
    let first = tokio::spawn(async move { first_client.enumerate_serial().await });
    let second = tokio::spawn(async move { second_client.enumerate_serial().await });
    for result in [first.await.unwrap(), second.await.unwrap()] {
        assert_eq!(
            result.unwrap_err().name().as_str(),
            "runtime.protocol.invalid_message"
        );
    }
    client.close().await.unwrap();
    server.await.unwrap();
    std::fs::remove_file(endpoint).unwrap();
}

#[cfg(unix)]
#[tokio::test]
async fn malformed_unsolicited_error_terminates_and_fans_out_to_pending_requests() {
    use seeed_hal_protocol::v1::{self, envelope};
    use std::collections::HashMap;

    let endpoint = fake::endpoint("malformed-unsolicited-error");
    let listener = fake::bind(&endpoint);
    let server = tokio::spawn(async move {
        let mut wire = fake::accept_and_handshake(listener).await;
        let _first = fake::recv(&mut wire).await;
        let _second = fake::recv(&mut wire).await;
        fake::send(
            &mut wire,
            v1::Envelope {
                request_id: 0,
                payload: Some(envelope::Payload::Error(v1::Error {
                    name: "runtime.resource.failed".to_owned(),
                    category: v1::ErrorCategory::Internal as i32,
                    operation: "runtime.serial.enumerate".to_owned(),
                    context: HashMap::from([("InvalidKey".to_owned(), "value".to_owned())]),
                    ..Default::default()
                })),
            },
        )
        .await;
    });
    let client = HalClient::connect(ConnectionOptions::new(&endpoint, TOKEN))
        .await
        .unwrap();
    let first_client = client.clone();
    let second_client = client.clone();
    let first = tokio::spawn(async move { first_client.enumerate_serial().await });
    let second = tokio::spawn(async move { second_client.enumerate_serial().await });
    for result in [first.await.unwrap(), second.await.unwrap()] {
        assert_eq!(
            result.unwrap_err().name().as_str(),
            "runtime.protocol.invalid_message"
        );
    }
    client.close().await.unwrap();
    server.await.unwrap();
    std::fs::remove_file(endpoint).unwrap();
}

#[cfg(unix)]
#[tokio::test]
async fn broker_round_trip_preserves_missing_resource_id() {
    use seeed_hal_broker::{Broker, StartupToken, listener::UnixBroker};
    use seeed_hal_core::{IdentityQuality, ResourceId, ResourceSelector, TransportKind};
    use seeed_hal_runtime::HalRuntime;
    use seeed_hal_testkit::VirtualSerialAdapter;

    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let directory =
        std::path::PathBuf::from(format!("/tmp/shc-missing-{}-{nonce}", std::process::id()));
    let runtime = HalRuntime::builder()
        .serial_adapter(VirtualSerialAdapter::loopback("serial:virtual:present"))
        .build();
    let broker = UnixBroker::bind(
        Broker::with_startup_token(runtime, StartupToken::from_bytes(TOKEN)),
        &directory,
    )
    .await
    .unwrap();
    let socket_path = broker.socket_path().to_owned();
    let server = tokio::spawn(async move { broker.serve_one().await.unwrap() });
    let client = HalClient::connect(ConnectionOptions::new(socket_path, TOKEN))
        .await
        .unwrap();
    let selector = ResourceSelector::exact(
        ResourceId::parse("serial:virtual:missing").unwrap(),
        IdentityQuality::Strong,
        TransportKind::Serial,
    );
    let error = match client.open_serial(selector, SerialConfig::default()).await {
        Ok(_) => panic!("opening an absent resource must fail"),
        Err(error) => error,
    };
    assert_eq!(error.name().as_str(), "runtime.resource.not_found");
    assert_eq!(
        error.resource_id().unwrap().as_str(),
        "serial:virtual:missing"
    );
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
    let event = test_deadline(events.recv(), "client must publish the first runtime event")
        .await
        .unwrap();
    let next_event = test_deadline(
        events.recv(),
        "client must publish the second runtime event",
    )
    .await
    .unwrap();
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
                        protocol_minor_minimum: 0,
                        protocol_minor_maximum: 0,
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

#[cfg(unix)]
#[tokio::test]
async fn pipelined_post_handshake_frame_prefix_uses_negotiated_decoder_limit() {
    use prost::Message;
    use seeed_hal_protocol::v1::{self, envelope};
    use tokio::io::AsyncWriteExt;
    use tokio_util::codec::{Framed, LengthDelimitedCodec};

    let endpoint = fake::endpoint("pipelined-limit");
    let listener = fake::bind(&endpoint);
    let server = tokio::spawn(async move {
        use futures_util::StreamExt;

        let (io, _) = listener.accept().await.unwrap();
        let mut wire = Framed::new(
            io,
            LengthDelimitedCodec::builder()
                .max_frame_length(seeed_hal_protocol::MAX_FRAME_BYTES)
                .new_codec(),
        );
        let request_frame = wire.next().await.unwrap().unwrap();
        let request = v1::Envelope::decode(request_frame).unwrap();
        let response = v1::Envelope {
            request_id: request.request_id,
            payload: Some(envelope::Payload::HandshakeResponse(
                v1::HandshakeResponse {
                    protocol_major: 1,
                    protocol_minor: 0,
                    capabilities: vec!["serial.bytes/v1".to_owned()],
                    max_frame_bytes: 256,
                    max_read_bytes: 16,
                    max_write_bytes: 16,
                    protocol_minor_minimum: 0,
                    protocol_minor_maximum: 0,
                },
            )),
        }
        .encode_to_vec();
        let mut pipelined = Vec::with_capacity(4 + response.len() + 4);
        pipelined.extend_from_slice(&(response.len() as u32).to_be_bytes());
        pipelined.extend_from_slice(&response);
        pipelined.extend_from_slice(&300_u32.to_be_bytes());
        wire.get_mut().write_all(&pipelined).await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    });
    let client =
        HalClient::connect(ConnectionOptions::new(&endpoint, TOKEN).with_byte_limits(256, 16, 16))
            .await
            .unwrap();
    let error = tokio::time::timeout(
        std::time::Duration::from_millis(150),
        client.enumerate_serial(),
    )
    .await
    .expect("oversized prefix must be rejected without waiting for its body")
    .unwrap_err();
    assert_eq!(error.name().as_str(), "runtime.protocol.frame_too_large");
    client.close().await.unwrap();
    server.await.unwrap();
    std::fs::remove_file(endpoint).unwrap();
}

#[cfg(unix)]
#[tokio::test]
async fn oversized_read_field_is_rejected_before_decode_and_fans_out() {
    use prost::Message;
    use seeed_hal_protocol::v1::{self, envelope};

    let endpoint = fake::endpoint("read-field-limit");
    let listener = fake::bind(&endpoint);
    let server = tokio::spawn(async move {
        let mut wire = fake::accept_and_handshake(listener).await;
        let enumerate = fake::recv(&mut wire).await;
        fake::send(
            &mut wire,
            fake::enumerate_response(enumerate.request_id, "serial:fake:read-limit"),
        )
        .await;
        let open = fake::recv(&mut wire).await;
        fake::send(
            &mut wire,
            v1::Envelope {
                request_id: open.request_id,
                payload: Some(envelope::Payload::OpenSerialResponse(
                    v1::OpenSerialResponse {
                        session_id: "session-fake-read-limit".to_owned(),
                        lease: Some(v1::LeaseToken {
                            lease_id: "lease-fake-read-limit".to_owned(),
                            generation: 1,
                            mode: v1::LeaseMode::Control as i32,
                        }),
                    },
                )),
            },
        )
        .await;
        let first = fake::recv(&mut wire).await;
        let second = fake::recv(&mut wire).await;
        let read_id = [first, second]
            .into_iter()
            .find(|request| {
                matches!(
                    request.payload,
                    Some(envelope::Payload::SerialReadRequest(_))
                )
            })
            .unwrap()
            .request_id;
        let mut response = v1::Envelope {
            request_id: read_id,
            payload: Some(envelope::Payload::SerialReadResponse(
                v1::SerialReadResponse {
                    data: vec![0x88; 16],
                },
            )),
        }
        .encode_to_vec();
        response.extend_from_slice(&[0x92, 0x03, 0x00]);
        fake::send_raw(&mut wire, &response).await;
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    });
    let client =
        HalClient::connect(ConnectionOptions::new(&endpoint, TOKEN).with_byte_limits(512, 16, 16))
            .await
            .unwrap();
    let descriptor = client.enumerate_serial().await.unwrap().remove(0);
    let serial = client
        .open_serial(descriptor.selector(), SerialConfig::default())
        .await
        .unwrap();
    let (read, enumerate) = tokio::join!(serial.read(8), client.enumerate_serial());
    assert_eq!(
        read.unwrap_err().name().as_str(),
        "runtime.protocol.frame_too_large"
    );
    assert_eq!(
        enumerate.unwrap_err().name().as_str(),
        "runtime.protocol.frame_too_large"
    );
    client.close().await.unwrap();
    server.await.unwrap();
    std::fs::remove_file(endpoint).unwrap();
}

#[cfg(unix)]
#[tokio::test]
async fn cancelled_read_keeps_its_requested_limit_until_response_is_discarded() {
    use seeed_hal_protocol::v1::{self, envelope};
    use tokio::sync::oneshot;

    let endpoint = fake::endpoint("cancelled-read-limit");
    let listener = fake::bind(&endpoint);
    let (read_received_tx, read_received_rx) = oneshot::channel();
    let server = tokio::spawn(async move {
        let mut wire = fake::accept_and_handshake(listener).await;
        fake::respond_enumerate_and_open(&mut wire, "serial:fake:cancelled-read-limit").await;
        let cancelled_read = fake::recv(&mut wire).await;
        assert!(matches!(
            cancelled_read.payload,
            Some(envelope::Payload::SerialReadRequest(_))
        ));
        read_received_tx.send(()).unwrap();
        let enumerate = fake::recv(&mut wire).await;
        assert!(matches!(
            enumerate.payload,
            Some(envelope::Payload::EnumerateSerialRequest(_))
        ));
        fake::send(
            &mut wire,
            v1::Envelope {
                request_id: cancelled_read.request_id,
                payload: Some(envelope::Payload::SerialReadResponse(
                    v1::SerialReadResponse {
                        data: vec![0x77; 12],
                    },
                )),
            },
        )
        .await;
        fake::send(
            &mut wire,
            fake::enumerate_response(enumerate.request_id, "serial:fake:next"),
        )
        .await;
    });
    let client =
        HalClient::connect(ConnectionOptions::new(&endpoint, TOKEN).with_byte_limits(512, 16, 16))
            .await
            .unwrap();
    let descriptor = client.enumerate_serial().await.unwrap().remove(0);
    let serial = client
        .open_serial(descriptor.selector(), SerialConfig::default())
        .await
        .unwrap();
    let read = tokio::spawn(async move { serial.read(8).await });
    read_received_rx.await.unwrap();
    read.abort();
    assert!(read.await.unwrap_err().is_cancelled());

    let error = client.enumerate_serial().await.unwrap_err();
    assert_eq!(error.name().as_str(), "runtime.protocol.frame_too_large");
    client.close().await.unwrap();
    server.await.unwrap();
    std::fs::remove_file(endpoint).unwrap();
}

#[cfg(unix)]
#[tokio::test]
async fn concurrent_requests_use_unique_nonzero_ids() {
    let endpoint = fake::endpoint("unique-ids");
    let listener = fake::bind(&endpoint);
    let server = tokio::spawn(async move {
        let mut wire = fake::accept_and_handshake(listener).await;
        let mut requests = Vec::new();
        for _ in 0..32 {
            requests.push(fake::recv(&mut wire).await);
        }
        let ids = requests
            .iter()
            .map(|request| request.request_id)
            .collect::<std::collections::HashSet<_>>();
        assert_eq!(ids.len(), 32);
        assert!(!ids.contains(&0));
        for request in requests.into_iter().rev() {
            fake::send(
                &mut wire,
                fake::enumerate_response(request.request_id, "serial:fake:unique"),
            )
            .await;
        }
    });
    let client = HalClient::connect(ConnectionOptions::new(&endpoint, TOKEN))
        .await
        .unwrap();
    let results = futures_util::future::join_all((0..32).map(|_| client.enumerate_serial())).await;
    assert!(results.into_iter().all(|result| result.is_ok()));
    client.close().await.unwrap();
    server.await.unwrap();
    std::fs::remove_file(endpoint).unwrap();
}

#[cfg(unix)]
#[tokio::test]
async fn cancellation_tombstone_overflow_fails_connection_closed() {
    use tokio::sync::{mpsc, oneshot};

    let endpoint = fake::endpoint("cancel-overflow");
    let listener = fake::bind(&endpoint);
    let (received_tx, mut received_rx) = mpsc::channel(2);
    let (release_tx, release_rx) = oneshot::channel();
    let server = tokio::spawn(async move {
        let mut wire = fake::accept_and_handshake(listener).await;
        for _ in 0..2 {
            let request = fake::recv(&mut wire).await;
            received_tx.send(request.request_id).await.unwrap();
        }
        test_deadline(release_rx, "cancellation test server must be released")
            .await
            .unwrap();
    });
    let client =
        HalClient::connect(ConnectionOptions::new(&endpoint, TOKEN).with_queue_capacities(1, 4, 4))
            .await
            .unwrap();
    for _ in 0..2 {
        let request_client = client.clone();
        let request = tokio::spawn(async move { request_client.enumerate_serial().await });
        assert_ne!(
            test_deadline(
                received_rx.recv(),
                "fake broker must positively observe the cancelled request",
            )
            .await
            .unwrap(),
            0
        );
        request.abort();
        assert!(request.await.unwrap_err().is_cancelled());
    }
    assert_eq!(
        client.enumerate_serial().await.unwrap_err().name().as_str(),
        "runtime.queue.cancelled_full"
    );
    release_tx.send(()).unwrap();
    client.close().await.unwrap();
    server.await.unwrap();
    std::fs::remove_file(endpoint).unwrap();
}

#[cfg(unix)]
#[tokio::test]
async fn event_subscription_reports_bounded_lag() {
    use seeed_hal_protocol::v1::{self, envelope};

    let endpoint = fake::endpoint("event-lag");
    let listener = fake::bind(&endpoint);
    let server = tokio::spawn(async move {
        let mut wire = fake::accept_and_handshake(listener).await;
        let request = fake::recv(&mut wire).await;
        for sequence in 1..=4 {
            fake::send(
                &mut wire,
                v1::Envelope {
                    request_id: 0,
                    payload: Some(envelope::Payload::RuntimeEvent(v1::RuntimeEvent {
                        sequence,
                        kind: v1::RuntimeEventKind::SessionOpened as i32,
                        name: "session.opened".to_owned(),
                        resource_id: "serial:fake:event-lag".to_owned(),
                        session_id: format!("session-event-lag-{sequence}"),
                        owner_id: "owner-event-lag".to_owned(),
                        lease_generation: sequence,
                    })),
                },
            )
            .await;
        }
        fake::send(
            &mut wire,
            fake::enumerate_response(request.request_id, "serial:fake:event-lag"),
        )
        .await;
    });
    let client =
        HalClient::connect(ConnectionOptions::new(&endpoint, TOKEN).with_queue_capacities(4, 4, 1))
            .await
            .unwrap();
    let mut events = client.subscribe();
    client.enumerate_serial().await.unwrap();
    assert_eq!(
        test_deadline(events.recv(), "client must report bounded event lag")
            .await
            .unwrap_err()
            .name()
            .as_str(),
        "runtime.event.lagged"
    );
    client.close().await.unwrap();
    server.await.unwrap();
    std::fs::remove_file(endpoint).unwrap();
}

#[cfg(unix)]
#[tokio::test]
async fn negotiated_write_limit_rejects_before_transmission() {
    use seeed_hal_protocol::v1::envelope;

    let endpoint = fake::endpoint("write-limit");
    let listener = fake::bind(&endpoint);
    let server = tokio::spawn(async move {
        let mut wire = fake::accept_and_handshake(listener).await;
        fake::respond_enumerate_and_open(&mut wire, "serial:fake:write-limit").await;
        let next = fake::recv(&mut wire).await;
        assert!(matches!(
            next.payload,
            Some(envelope::Payload::EnumerateSerialRequest(_))
        ));
        fake::send(
            &mut wire,
            fake::enumerate_response(next.request_id, "serial:fake:write-limit"),
        )
        .await;
    });
    let client =
        HalClient::connect(ConnectionOptions::new(&endpoint, TOKEN).with_byte_limits(512, 16, 16))
            .await
            .unwrap();
    let descriptor = client.enumerate_serial().await.unwrap().remove(0);
    let serial = client
        .open_serial(descriptor.selector(), SerialConfig::default())
        .await
        .unwrap();
    assert_eq!(
        serial
            .write(Bytes::from_static(&[0x99; 17]))
            .await
            .unwrap_err()
            .name()
            .as_str(),
        "runtime.argument.invalid"
    );
    client.enumerate_serial().await.unwrap();
    server.await.unwrap();
    client.close().await.unwrap();
    std::fs::remove_file(endpoint).unwrap();
}

#[cfg(unix)]
#[tokio::test]
async fn disconnect_fans_out_to_multiple_pending_requests() {
    let endpoint = fake::endpoint("disconnect-fanout");
    let listener = fake::bind(&endpoint);
    let server = tokio::spawn(async move {
        let mut wire = fake::accept_and_handshake(listener).await;
        let _first = fake::recv(&mut wire).await;
        let _second = fake::recv(&mut wire).await;
    });
    let client = HalClient::connect(ConnectionOptions::new(&endpoint, TOKEN))
        .await
        .unwrap();
    let (first, second) = tokio::join!(client.enumerate_serial(), client.enumerate_serial());
    for error in [first.unwrap_err(), second.unwrap_err()] {
        assert_eq!(error.name().as_str(), "runtime.broker.disconnected");
    }
    client.close().await.unwrap();
    server.await.unwrap();
    std::fs::remove_file(endpoint).unwrap();
}

#[cfg(unix)]
#[tokio::test]
async fn client_close_fans_out_to_multiple_pending_requests() {
    use tokio::sync::{mpsc, oneshot};

    let endpoint = fake::endpoint("close-fanout");
    let listener = fake::bind(&endpoint);
    let (received_tx, mut received_rx) = mpsc::channel(2);
    let (release_tx, release_rx) = oneshot::channel();
    let server = tokio::spawn(async move {
        let mut wire = fake::accept_and_handshake(listener).await;
        for _ in 0..2 {
            let request = fake::recv(&mut wire).await;
            received_tx.send(request.request_id).await.unwrap();
        }
        test_deadline(release_rx, "client-close test server must be released")
            .await
            .unwrap();
    });
    let client = HalClient::connect(ConnectionOptions::new(&endpoint, TOKEN))
        .await
        .unwrap();
    let first_client = client.clone();
    let second_client = client.clone();
    let first = tokio::spawn(async move { first_client.enumerate_serial().await });
    let second = tokio::spawn(async move { second_client.enumerate_serial().await });
    assert_ne!(
        test_deadline(
            received_rx.recv(),
            "fake broker must receive the first request"
        )
        .await
        .unwrap(),
        0
    );
    assert_ne!(
        test_deadline(
            received_rx.recv(),
            "fake broker must receive the second request"
        )
        .await
        .unwrap(),
        0
    );
    client.close().await.unwrap();
    for result in [first.await.unwrap(), second.await.unwrap()] {
        assert_eq!(result.unwrap_err().name().as_str(), "runtime.client.closed");
    }
    release_tx.send(()).unwrap();
    server.await.unwrap();
    std::fs::remove_file(endpoint).unwrap();
}
