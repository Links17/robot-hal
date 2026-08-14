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
                    }],
                },
            )),
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
async fn writer_queue_overflow_returns_structured_backpressure() {
    let endpoint = fake::endpoint("writer-overflow");
    let listener = fake::bind(&endpoint);
    let server = tokio::spawn(async move {
        let mut wire = fake::accept_and_handshake(listener).await;
        fake::respond_enumerate_and_open(&mut wire, "serial:fake:writer-overflow").await;
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    });
    let client = HalClient::connect(
        ConnectionOptions::new(&endpoint, TOKEN)
            .with_byte_limits(128 * 1024, 64 * 1024, 64 * 1024)
            .with_queue_capacities(128, 1, 4),
    )
    .await
    .unwrap();
    let descriptor = client.enumerate_serial().await.unwrap().remove(0);
    let serial = client
        .open_serial(descriptor.selector(), SerialConfig::default())
        .await
        .unwrap();
    let payload = Bytes::from(vec![0x44; 64 * 1024]);
    let results =
        futures_util::future::join_all((0..64).map(|_| serial.write(payload.clone()))).await;
    assert!(results.into_iter().any(|result| {
        result.is_err_and(|error| {
            error.name().as_str() == "runtime.queue.full"
                && error.operation().as_str() == "runtime.protocol.write"
        })
    }));
    client.close().await.unwrap();
    server.await.unwrap();
    std::fs::remove_file(endpoint).unwrap();
}

#[cfg(unix)]
#[tokio::test]
async fn cancellation_tombstone_overflow_fails_connection_closed() {
    let endpoint = fake::endpoint("cancel-overflow");
    let listener = fake::bind(&endpoint);
    let server = tokio::spawn(async move {
        let mut wire = fake::accept_and_handshake(listener).await;
        let _first = fake::recv(&mut wire).await;
        let _second = fake::recv(&mut wire).await;
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    });
    let client =
        HalClient::connect(ConnectionOptions::new(&endpoint, TOKEN).with_queue_capacities(1, 4, 4))
            .await
            .unwrap();
    for _ in 0..2 {
        let request_client = client.clone();
        let request = tokio::spawn(async move { request_client.enumerate_serial().await });
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        request.abort();
        assert!(request.await.unwrap_err().is_cancelled());
    }
    assert_eq!(
        client.enumerate_serial().await.unwrap_err().name().as_str(),
        "runtime.queue.cancelled_full"
    );
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
        events.recv().await.unwrap_err().name().as_str(),
        "runtime.event.lagged"
    );
    client.close().await.unwrap();
    server.await.unwrap();
    std::fs::remove_file(endpoint).unwrap();
}

#[cfg(unix)]
#[tokio::test]
async fn negotiated_write_limit_rejects_before_transmission() {
    use futures_util::StreamExt;

    let endpoint = fake::endpoint("write-limit");
    let listener = fake::bind(&endpoint);
    let server = tokio::spawn(async move {
        let mut wire = fake::accept_and_handshake(listener).await;
        fake::respond_enumerate_and_open(&mut wire, "serial:fake:write-limit").await;
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(50), wire.next())
                .await
                .is_err(),
            "oversized write must not reach the wire"
        );
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
    let endpoint = fake::endpoint("close-fanout");
    let listener = fake::bind(&endpoint);
    let server = tokio::spawn(async move {
        let mut wire = fake::accept_and_handshake(listener).await;
        let _first = fake::recv(&mut wire).await;
        let _second = fake::recv(&mut wire).await;
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    });
    let client = HalClient::connect(ConnectionOptions::new(&endpoint, TOKEN))
        .await
        .unwrap();
    let first_client = client.clone();
    let second_client = client.clone();
    let first = tokio::spawn(async move { first_client.enumerate_serial().await });
    let second = tokio::spawn(async move { second_client.enumerate_serial().await });
    tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    client.close().await.unwrap();
    for result in [first.await.unwrap(), second.await.unwrap()] {
        assert_eq!(result.unwrap_err().name().as_str(), "runtime.client.closed");
    }
    server.await.unwrap();
    std::fs::remove_file(endpoint).unwrap();
}
