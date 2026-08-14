use bytes::Bytes;
use futures_util::{SinkExt, StreamExt};
use prost::Message;
use seeed_hal_broker::{Broker, BrokerConfig, StartupToken};
use seeed_hal_core::{OwnerId, ResourceSelector};
use seeed_hal_protocol::v1::{self, envelope};
use seeed_hal_runtime::{HalRuntime, RuntimeEventKind};
use seeed_hal_serial::SerialConfig;
use seeed_hal_testkit::VirtualSerialAdapter;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio_util::codec::{Framed, LengthDelimitedCodec};

const TOKEN: [u8; 32] = [0xa5; 32];

struct Client<T> {
    framed: Framed<T, LengthDelimitedCodec>,
    next_request_id: u64,
}

impl<T> Client<T>
where
    T: AsyncRead + AsyncWrite + Unpin,
{
    fn new(io: T) -> Self {
        Self {
            framed: Framed::new(io, codec()),
            next_request_id: 1,
        }
    }

    async fn request(&mut self, payload: envelope::Payload) -> v1::Envelope {
        let request_id = self.next_request_id;
        self.next_request_id += 1;
        self.send(request_id, payload).await;
        loop {
            let response = self.recv().await;
            if response.request_id == request_id {
                return response;
            }
            assert_eq!(
                response.request_id, 0,
                "only runtime events are unsolicited"
            );
        }
    }

    async fn send(&mut self, request_id: u64, payload: envelope::Payload) {
        self.try_send(request_id, payload).await.unwrap();
    }

    async fn try_send(
        &mut self,
        request_id: u64,
        payload: envelope::Payload,
    ) -> std::io::Result<()> {
        let encoded = v1::Envelope {
            request_id,
            payload: Some(payload),
        }
        .encode_to_vec();
        self.framed.send(Bytes::from(encoded)).await
    }

    async fn recv(&mut self) -> v1::Envelope {
        let frame = self.framed.next().await.unwrap().unwrap();
        v1::Envelope::decode(frame).unwrap()
    }

    async fn handshake(&mut self) {
        let response = self
            .request(envelope::Payload::HandshakeRequest(valid_handshake(
                TOKEN.to_vec(),
            )))
            .await;
        assert!(matches!(
            response.payload,
            Some(envelope::Payload::HandshakeResponse(_))
        ));
    }
}

fn codec() -> LengthDelimitedCodec {
    LengthDelimitedCodec::builder()
        .max_frame_length(seeed_hal_protocol::MAX_FRAME_BYTES)
        .new_codec()
}

fn valid_handshake(token: Vec<u8>) -> v1::HandshakeRequest {
    v1::HandshakeRequest {
        startup_token: token,
        protocol_major: 1,
        protocol_minor: 0,
        required_capabilities: vec!["serial.bytes/v1".to_owned()],
        max_frame_bytes: 1024 * 1024,
        max_read_bytes: 64 * 1024,
        max_write_bytes: 64 * 1024,
    }
}

fn broker() -> Broker {
    let runtime = HalRuntime::builder()
        .serial_adapter(VirtualSerialAdapter::loopback("serial:virtual:broker"))
        .build();
    Broker::with_startup_token(runtime, StartupToken::from_bytes(TOKEN))
}

fn runtime() -> HalRuntime {
    HalRuntime::builder()
        .serial_adapter(VirtualSerialAdapter::loopback("serial:virtual:broker"))
        .build()
}

fn serial_config(read_timeout_ms: u64) -> v1::SerialConfig {
    v1::SerialConfig {
        baud_rate: 115_200,
        data_bits: v1::DataBits::Eight as i32,
        parity: v1::Parity::None as i32,
        stop_bits: v1::StopBits::One as i32,
        flow_control: v1::FlowControl::None as i32,
        read_timeout_ms,
    }
}

async fn open_virtual_serial<T>(
    client: &mut Client<T>,
    read_timeout_ms: u64,
) -> v1::OpenSerialResponse
where
    T: AsyncRead + AsyncWrite + Unpin,
{
    let enumerate = client
        .request(envelope::Payload::EnumerateSerialRequest(
            v1::EnumerateSerialRequest {},
        ))
        .await;
    let descriptor = match enumerate.payload.unwrap() {
        envelope::Payload::EnumerateSerialResponse(response) => {
            response.resources.into_iter().next().unwrap()
        }
        _ => panic!("expected enumerate response"),
    };
    let response = client
        .request(envelope::Payload::OpenSerialRequest(
            v1::OpenSerialRequest {
                selector: Some(v1::ResourceSelector {
                    resource_id: descriptor.resource_id,
                    minimum_identity_quality: descriptor.identity_quality,
                    transport: descriptor.transport,
                }),
                config: Some(serial_config(read_timeout_ms)),
            },
        ))
        .await;
    match response.payload.unwrap() {
        envelope::Payload::OpenSerialResponse(response) => response,
        payload => panic!("expected open response, got {payload:?}"),
    }
}

fn error_name(envelope: &v1::Envelope) -> &str {
    match envelope.payload.as_ref() {
        Some(envelope::Payload::Error(error)) => &error.name,
        _ => panic!("expected an error envelope"),
    }
}

async fn rejected_handshake(request: v1::HandshakeRequest) -> String {
    let (server_io, client_io) = tokio::io::duplex(16 * 1024);
    let broker = broker();
    let server = tokio::spawn(async move { broker.serve_connection(server_io).await });
    let mut client = Client::new(client_io);
    let response = client
        .request(envelope::Payload::HandshakeRequest(request))
        .await;
    let name = error_name(&response).to_owned();
    drop(client);
    assert!(server.await.unwrap().cleanup_error().is_none());
    name
}

#[tokio::test]
async fn broker_rejects_operations_before_handshake() {
    let (server_io, client_io) = tokio::io::duplex(16 * 1024);
    let broker = broker();
    let server = tokio::spawn(async move { broker.serve_connection(server_io).await });
    let mut client = Client::new(client_io);

    let response = client
        .request(envelope::Payload::EnumerateSerialRequest(
            v1::EnumerateSerialRequest {},
        ))
        .await;
    assert_eq!(error_name(&response), "runtime.protocol.handshake_required");

    drop(client);
    assert!(server.await.unwrap().cleanup_error().is_none());
}

#[tokio::test]
async fn invalid_startup_token_fails_closed() {
    let (server_io, client_io) = tokio::io::duplex(16 * 1024);
    let broker = broker();
    let server = tokio::spawn(async move { broker.serve_connection(server_io).await });
    let mut client = Client::new(client_io);

    let response = client
        .request(envelope::Payload::HandshakeRequest(valid_handshake(vec![
            0;
            32
        ])))
        .await;
    assert_eq!(
        error_name(&response),
        "runtime.protocol.authentication_failed"
    );

    drop(client);
    assert!(server.await.unwrap().cleanup_error().is_none());
}

#[tokio::test]
async fn handshake_version_capability_and_byte_limits_fail_closed() {
    let mut wrong_version = valid_handshake(TOKEN.to_vec());
    wrong_version.protocol_major = 2;
    assert_eq!(
        rejected_handshake(wrong_version).await,
        "runtime.protocol.incompatible_version"
    );

    let mut unsupported_capability = valid_handshake(TOKEN.to_vec());
    unsupported_capability.required_capabilities = vec!["can.fd/v1".to_owned()];
    assert_eq!(
        rejected_handshake(unsupported_capability).await,
        "runtime.protocol.unsupported_capability"
    );

    let mut oversized_frame = valid_handshake(TOKEN.to_vec());
    oversized_frame.max_frame_bytes = (seeed_hal_protocol::MAX_FRAME_BYTES + 1) as u32;
    assert_eq!(
        rejected_handshake(oversized_frame).await,
        "runtime.protocol.invalid_message"
    );

    let mut missing_envelope_overhead = valid_handshake(TOKEN.to_vec());
    missing_envelope_overhead.max_frame_bytes = 128;
    missing_envelope_overhead.max_read_bytes = 128;
    missing_envelope_overhead.max_write_bytes = 1;
    assert_eq!(
        rejected_handshake(missing_envelope_overhead).await,
        "runtime.protocol.invalid_message"
    );
}

#[tokio::test]
async fn negotiated_frame_limit_rejects_oversized_raw_inbound_frame() {
    let (server_io, client_io) = tokio::io::duplex(16 * 1024);
    let broker = broker();
    let server = tokio::spawn(async move { broker.serve_connection(server_io).await });
    let mut client = Client::new(client_io);
    let mut handshake = valid_handshake(TOKEN.to_vec());
    handshake.max_frame_bytes = 256;
    handshake.max_read_bytes = 64;
    handshake.max_write_bytes = 1;
    let response = client
        .request(envelope::Payload::HandshakeRequest(handshake))
        .await;
    assert!(matches!(
        response.payload,
        Some(envelope::Payload::HandshakeResponse(_))
    ));

    client
        .send(
            40,
            envelope::Payload::SerialFlushRequest(v1::SerialFlushRequest {
                session_id: "s".repeat(255),
                lease: Some(v1::LeaseToken {
                    lease_id: "l".repeat(255),
                    generation: 1,
                    mode: v1::LeaseMode::Control as i32,
                }),
            }),
        )
        .await;

    let next = tokio::time::timeout(std::time::Duration::from_secs(1), client.framed.next())
        .await
        .expect("negotiated frame violation must close the connection");
    assert!(
        next.is_none(),
        "broker must initiate EOF without dispatching"
    );
    let outcome = server.await.unwrap();
    assert_eq!(
        outcome.connection_error().unwrap().name().as_str(),
        "runtime.protocol.frame_too_large"
    );
}

#[tokio::test]
async fn negotiated_frame_limit_rejects_oversized_outbound_before_encoding() {
    let (server_io, client_io) = tokio::io::duplex(16 * 1024);
    let broker = broker();
    let server = tokio::spawn(async move { broker.serve_connection(server_io).await });
    let mut client = Client::new(client_io);
    let mut handshake = valid_handshake(TOKEN.to_vec());
    handshake.max_frame_bytes = 110;
    handshake.max_read_bytes = 1;
    handshake.max_write_bytes = 1;
    let response = client
        .request(envelope::Payload::HandshakeRequest(handshake))
        .await;
    assert!(matches!(
        response.payload,
        Some(envelope::Payload::HandshakeResponse(_))
    ));

    client
        .send(
            41,
            envelope::Payload::EnumerateSerialRequest(v1::EnumerateSerialRequest {}),
        )
        .await;
    let next = tokio::time::timeout(std::time::Duration::from_secs(1), client.framed.next())
        .await
        .expect("oversized outbound envelope must close the connection");
    assert!(
        next.is_none(),
        "broker must not write an oversized envelope: observed {next:?}"
    );
    let outcome = server.await.unwrap();
    assert_eq!(
        outcome.connection_error().unwrap().name().as_str(),
        "runtime.protocol.frame_too_large"
    );
}

#[tokio::test]
async fn virtual_serial_supports_the_complete_v1_operation_set() {
    let (server_io, client_io) = tokio::io::duplex(64 * 1024);
    let broker = broker();
    let server = tokio::spawn(async move { broker.serve_connection(server_io).await });
    let mut client = Client::new(client_io);
    client.handshake().await;
    let session = open_virtual_serial(&mut client, 1_000).await;
    let lease = session.lease.clone();

    let write = client
        .request(envelope::Payload::SerialWriteRequest(
            v1::SerialWriteRequest {
                session_id: session.session_id.clone(),
                lease: lease.clone(),
                data: b"loopback".to_vec(),
            },
        ))
        .await;
    assert!(matches!(
        write.payload,
        Some(envelope::Payload::SerialWriteResponse(_))
    ));

    let flush = client
        .request(envelope::Payload::SerialFlushRequest(
            v1::SerialFlushRequest {
                session_id: session.session_id.clone(),
                lease: lease.clone(),
            },
        ))
        .await;
    assert!(matches!(
        flush.payload,
        Some(envelope::Payload::SerialFlushResponse(_))
    ));

    let control = client
        .request(envelope::Payload::SetSerialControlLinesRequest(
            v1::SetSerialControlLinesRequest {
                session_id: session.session_id.clone(),
                lease: lease.clone(),
                data_terminal_ready: true,
                request_to_send: true,
            },
        ))
        .await;
    assert!(matches!(
        control.payload,
        Some(envelope::Payload::SetSerialControlLinesResponse(_))
    ));

    let read = client
        .request(envelope::Payload::SerialReadRequest(
            v1::SerialReadRequest {
                session_id: session.session_id.clone(),
                lease: lease.clone(),
                max_bytes: 64,
            },
        ))
        .await;
    match read.payload.unwrap() {
        envelope::Payload::SerialReadResponse(response) => {
            assert_eq!(response.data, b"loopback")
        }
        _ => panic!("expected read response"),
    }

    let close = client
        .request(envelope::Payload::CloseSessionRequest(
            v1::CloseSessionRequest {
                session_id: session.session_id,
                lease,
            },
        ))
        .await;
    assert!(matches!(
        close.payload,
        Some(envelope::Payload::CloseSessionResponse(_))
    ));

    drop(client);
    assert!(server.await.unwrap().cleanup_error().is_none());
}

#[tokio::test]
async fn runtime_events_are_forwarded_as_unsolicited_envelopes() {
    let (server_io, client_io) = tokio::io::duplex(64 * 1024);
    let broker = broker();
    let server = tokio::spawn(async move { broker.serve_connection(server_io).await });
    let mut client = Client::new(client_io);
    client.handshake().await;
    let enumerate = client
        .request(envelope::Payload::EnumerateSerialRequest(
            v1::EnumerateSerialRequest {},
        ))
        .await;
    let descriptor = match enumerate.payload.unwrap() {
        envelope::Payload::EnumerateSerialResponse(response) => {
            response.resources.into_iter().next().unwrap()
        }
        _ => panic!("expected enumerate response"),
    };
    client
        .send(
            50,
            envelope::Payload::OpenSerialRequest(v1::OpenSerialRequest {
                selector: Some(v1::ResourceSelector {
                    resource_id: descriptor.resource_id,
                    minimum_identity_quality: descriptor.identity_quality,
                    transport: descriptor.transport,
                }),
                config: Some(serial_config(1_000)),
            }),
        )
        .await;

    let mut saw_response = false;
    let mut saw_event = false;
    while !saw_response || !saw_event {
        let envelope = client.recv().await;
        match envelope.payload.unwrap() {
            envelope::Payload::OpenSerialResponse(_) => {
                assert_eq!(envelope.request_id, 50);
                saw_response = true;
            }
            envelope::Payload::RuntimeEvent(event) => {
                assert_eq!(envelope.request_id, 0);
                assert_eq!(event.name, "session.opened");
                saw_event = true;
            }
            _ => panic!("unexpected payload while opening serial"),
        }
    }

    drop(client);
    assert!(server.await.unwrap().cleanup_error().is_none());
}

#[tokio::test]
async fn runtime_events_are_filtered_to_the_connection_owner() {
    let runtime = runtime();
    let broker = Broker::with_startup_token(runtime, StartupToken::from_bytes(TOKEN));
    let (server_one_io, client_one_io) = tokio::io::duplex(64 * 1024);
    let (server_two_io, client_two_io) = tokio::io::duplex(64 * 1024);
    let server_one_broker = broker.clone();
    let server_one =
        tokio::spawn(async move { server_one_broker.serve_connection(server_one_io).await });
    let server_two = tokio::spawn(async move { broker.serve_connection(server_two_io).await });
    let mut client_one = Client::new(client_one_io);
    let mut client_two = Client::new(client_two_io);
    client_one.handshake().await;
    client_two.handshake().await;

    let _session = open_virtual_serial(&mut client_one, 1_000).await;
    let leaked = tokio::time::timeout(
        std::time::Duration::from_millis(50),
        client_two.framed.next(),
    )
    .await;
    assert!(
        leaked.is_err(),
        "a connection must not observe another owner's runtime event"
    );

    drop(client_one);
    drop(client_two);
    assert!(server_one.await.unwrap().cleanup_error().is_none());
    assert!(server_two.await.unwrap().cleanup_error().is_none());
}

#[tokio::test]
async fn runtime_event_queue_lag_is_reported_structurally() {
    let runtime = runtime();
    let broker = Broker::with_startup_token(runtime.clone(), StartupToken::from_bytes(TOKEN));
    let (server_io, client_io) = tokio::io::duplex(64 * 1024);
    let server = tokio::spawn(async move { broker.serve_connection(server_io).await });
    let mut client = Client::new(client_io);

    let before_handshake = client
        .request(envelope::Payload::EnumerateSerialRequest(
            v1::EnumerateSerialRequest {},
        ))
        .await;
    assert_eq!(
        error_name(&before_handshake),
        "runtime.protocol.handshake_required"
    );

    let descriptor = runtime.enumerate_serial().await.unwrap().remove(0);
    for index in 0..33 {
        runtime
            .open_serial(
                OwnerId::parse(format!("broker-contract:event-lag-{index}")).unwrap(),
                descriptor.selector(),
                SerialConfig::default(),
            )
            .await
            .unwrap()
            .close()
            .await
            .unwrap();
    }

    client
        .send(
            500,
            envelope::Payload::HandshakeRequest(valid_handshake(TOKEN.to_vec())),
        )
        .await;
    let mut saw_handshake = false;
    let mut saw_lag = false;
    while !saw_handshake || !saw_lag {
        let response = client.recv().await;
        match response.payload.as_ref() {
            Some(envelope::Payload::HandshakeResponse(_)) => saw_handshake = true,
            Some(envelope::Payload::Error(error))
                if error.name == "runtime.event.lagged" && response.request_id == 0 =>
            {
                saw_lag = true;
            }
            _ => panic!("unexpected response while observing event lag"),
        }
    }

    drop(client);
    assert!(server.await.unwrap().cleanup_error().is_none());
}

#[tokio::test]
async fn disconnect_revokes_owned_sessions() {
    let runtime = runtime();
    let mut events = runtime.subscribe();
    let broker = Broker::with_startup_token(runtime.clone(), StartupToken::from_bytes(TOKEN));
    let (server_io, client_io) = tokio::io::duplex(64 * 1024);
    let server = tokio::spawn(async move { broker.serve_connection(server_io).await });
    let mut client = Client::new(client_io);
    client.handshake().await;
    let session = open_virtual_serial(&mut client, 1_000).await;
    let expected_session_id = session.session_id;
    drop(client);

    let outcome = server.await.unwrap();
    assert!(outcome.cleanup_error().is_none());
    let mut closed = false;
    for _ in 0..2 {
        let event = events.recv().await.unwrap();
        if event.kind() == RuntimeEventKind::SessionClosed
            && event.session_id().as_str() == expected_session_id
        {
            closed = true;
        }
    }
    assert!(
        closed,
        "disconnect must publish closure for the owned session"
    );

    let descriptor = runtime.enumerate_serial().await.unwrap().remove(0);
    runtime
        .open_serial(
            OwnerId::parse("broker-contract:next-owner").unwrap(),
            descriptor.selector(),
            SerialConfig::default(),
        )
        .await
        .unwrap()
        .close()
        .await
        .unwrap();
}

#[tokio::test]
async fn stalled_writer_cannot_delay_owner_revoke_or_resource_reuse() {
    let runtime = runtime();
    let broker = Broker::with_config(
        runtime.clone(),
        StartupToken::from_bytes(TOKEN),
        BrokerConfig::default()
            .with_request_queue_capacity(2_048)
            .with_response_queue_capacity(512)
            .with_max_in_flight_requests(2_048),
    );
    let (server_io, client_io) = tokio::io::duplex(16 * 1024);
    let server = tokio::spawn(async move { broker.serve_connection(server_io).await });
    let mut client = Client::new(client_io);
    client.handshake().await;
    let _session = open_virtual_serial(&mut client, 1_000).await;

    for request_id in 1_000..4_000 {
        if client
            .try_send(
                request_id,
                envelope::Payload::EnumerateSerialRequest(v1::EnumerateSerialRequest {}),
            )
            .await
            .is_err()
        {
            break;
        }
    }

    let outcome = tokio::time::timeout(std::time::Duration::from_secs(1), server)
        .await
        .expect("stalled output must not block connection teardown")
        .unwrap();
    assert!(outcome.cleanup_error().is_none());
    assert_eq!(
        outcome.connection_error().unwrap().name().as_str(),
        "runtime.queue.full"
    );

    let descriptor = runtime.enumerate_serial().await.unwrap().remove(0);
    runtime
        .open_serial(
            OwnerId::parse("broker-contract:stalled-writer-reuse").unwrap(),
            descriptor.selector(),
            SerialConfig::default(),
        )
        .await
        .unwrap()
        .close()
        .await
        .unwrap();

    drop(client);
}

#[tokio::test]
async fn task_queue_overflow_is_deterministic_and_structured() {
    let runtime = runtime();
    let broker = Broker::with_config(
        runtime,
        StartupToken::from_bytes(TOKEN),
        BrokerConfig::default().with_max_in_flight_requests(1),
    );
    let (server_io, client_io) = tokio::io::duplex(64 * 1024);
    let server = tokio::spawn(async move { broker.serve_connection(server_io).await });
    let mut client = Client::new(client_io);
    client.handshake().await;
    let session = open_virtual_serial(&mut client, 5_000).await;
    let request = v1::SerialReadRequest {
        session_id: session.session_id,
        lease: session.lease,
        max_bytes: 1,
    };

    client
        .send(100, envelope::Payload::SerialReadRequest(request.clone()))
        .await;
    client
        .send(101, envelope::Payload::SerialReadRequest(request))
        .await;

    loop {
        let response = client.recv().await;
        if response.request_id == 101 {
            assert_eq!(error_name(&response), "runtime.queue.full");
            break;
        }
    }
    drop(client);
    assert!(server.await.unwrap().cleanup_error().is_none());
}

#[tokio::test]
async fn request_queue_overflow_is_deterministic_and_structured() {
    let runtime = runtime();
    let broker = Broker::with_config(
        runtime,
        StartupToken::from_bytes(TOKEN),
        BrokerConfig::default().with_request_queue_capacity(1),
    );
    let (server_io, client_io) = tokio::io::duplex(16 * 1024);
    let mut client = Client::new(client_io);
    client
        .send(
            1,
            envelope::Payload::HandshakeRequest(valid_handshake(TOKEN.to_vec())),
        )
        .await;
    client
        .send(
            2,
            envelope::Payload::EnumerateSerialRequest(v1::EnumerateSerialRequest {}),
        )
        .await;
    client
        .send(
            3,
            envelope::Payload::EnumerateSerialRequest(v1::EnumerateSerialRequest {}),
        )
        .await;
    let server = tokio::spawn(async move { broker.serve_connection(server_io).await });

    let mut saw_queue_full = false;
    for _ in 0..3 {
        let response = client.recv().await;
        if matches!(response.payload, Some(envelope::Payload::Error(_)))
            && error_name(&response) == "runtime.queue.full"
        {
            saw_queue_full = true;
            break;
        }
    }
    assert!(
        saw_queue_full,
        "the bounded request queue must reject overflow"
    );
    drop(client);
    assert!(server.await.unwrap().cleanup_error().is_none());
}

#[tokio::test]
async fn duplicate_in_flight_request_ids_fail_closed() {
    let (server_io, client_io) = tokio::io::duplex(64 * 1024);
    let runtime = runtime();
    let broker = Broker::with_startup_token(runtime.clone(), StartupToken::from_bytes(TOKEN));
    let server = tokio::spawn(async move { broker.serve_connection(server_io).await });
    let mut client = Client::new(client_io);
    client.handshake().await;
    let session = open_virtual_serial(&mut client, 5_000).await;
    let request = v1::SerialReadRequest {
        session_id: session.session_id,
        lease: session.lease,
        max_bytes: 1,
    };

    client
        .send(200, envelope::Payload::SerialReadRequest(request.clone()))
        .await;
    client
        .send(200, envelope::Payload::SerialReadRequest(request))
        .await;
    loop {
        let response = client.recv().await;
        if response.request_id == 200
            && matches!(response.payload, Some(envelope::Payload::Error(_)))
        {
            assert_eq!(
                error_name(&response),
                "runtime.protocol.duplicate_request_id"
            );
            break;
        }
    }
    let eof = tokio::time::timeout(std::time::Duration::from_secs(1), client.framed.next())
        .await
        .expect("duplicate request IDs must terminate the connection");
    assert!(
        eof.is_none(),
        "broker must initiate EOF after duplicate IDs"
    );
    assert!(server.await.unwrap().cleanup_error().is_none());

    let descriptor = runtime.enumerate_serial().await.unwrap().remove(0);
    runtime
        .open_serial(
            OwnerId::parse("broker-contract:duplicate-id-reuse").unwrap(),
            descriptor.selector(),
            SerialConfig::default(),
        )
        .await
        .unwrap()
        .close()
        .await
        .unwrap();
}

#[tokio::test]
async fn malformed_operation_returns_a_structured_protocol_error() {
    let (server_io, client_io) = tokio::io::duplex(16 * 1024);
    let broker = broker();
    let server = tokio::spawn(async move { broker.serve_connection(server_io).await });
    let mut client = Client::new(client_io);
    client.handshake().await;

    let response = client
        .request(envelope::Payload::SerialFlushRequest(
            v1::SerialFlushRequest {
                session_id: "not a valid id".to_owned(),
                lease: None,
            },
        ))
        .await;
    let error = match response.payload.unwrap() {
        envelope::Payload::Error(error) => error,
        _ => panic!("expected structured error"),
    };
    assert_eq!(error.name, "runtime.protocol.invalid_message");
    assert_eq!(error.category, v1::ErrorCategory::InvalidArgument as i32);
    assert!(!error.operation.is_empty());

    drop(client);
    assert!(server.await.unwrap().cleanup_error().is_none());
}

#[test]
fn resource_descriptor_selector_conversion_is_canonical() {
    let runtime = runtime();
    let descriptor = tokio::runtime::Runtime::new()
        .unwrap()
        .block_on(runtime.enumerate_serial())
        .unwrap()
        .remove(0);
    let wire = v1::ResourceDescriptor::from(&descriptor);
    let selector = ResourceSelector::try_from(v1::ResourceSelector {
        resource_id: wire.resource_id,
        minimum_identity_quality: wire.identity_quality,
        transport: wire.transport,
    })
    .unwrap();
    assert_eq!(selector, descriptor.selector());
}

#[cfg(unix)]
#[tokio::test]
async fn unix_listener_uses_a_private_directory_and_socket() {
    use std::os::unix::fs::PermissionsExt;

    use seeed_hal_broker::listener::UnixBroker;

    let directory =
        std::path::Path::new("/tmp").join(format!("shb-contract-{}", std::process::id()));
    if directory.exists() {
        std::fs::remove_dir_all(&directory).unwrap();
    }
    let listener = UnixBroker::bind(broker(), &directory).await.unwrap();
    let socket_path = listener.socket_path().to_owned();

    assert_eq!(
        std::fs::metadata(&directory).unwrap().permissions().mode() & 0o777,
        0o700
    );
    assert_eq!(
        std::fs::metadata(&socket_path)
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o600
    );

    drop(listener);
    std::fs::remove_file(socket_path).unwrap();
    std::fs::remove_dir(directory).unwrap();
}
