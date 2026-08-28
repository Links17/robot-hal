use std::time::Duration;

use robot_hal_serial::{
    CapabilityId, CapabilitySet, ControlLines, DataBits, Endpoint, FlowControl, IdentityQuality,
    Parity, ResourceDescriptor, ResourceId, ResourceProperties, SerialAdapter, SerialConfig,
    StopBits, TransportKind,
};
use robot_hal_testkit::VirtualSerialAdapter;

#[tokio::test]
async fn serial_round_trip_preserves_byte_order() {
    let adapter = VirtualSerialAdapter::loopback("serial:virtual:loopback-1");
    let descriptor = adapter.enumerate().await.unwrap().remove(0);
    let mut session = adapter
        .open(&descriptor.selector(), SerialConfig::default())
        .await
        .unwrap();

    session.write_all(b"abc\0xyz").await.unwrap();
    session.flush().await.unwrap();
    let bytes = session.read(7).await.unwrap();

    assert_eq!(&bytes[..], b"abc\0xyz");
    session.close().await.unwrap();
}

#[tokio::test]
async fn unsupported_configuration_is_rejected() {
    let adapter = VirtualSerialAdapter::loopback("serial:virtual:loopback-1");
    let selector = adapter.enumerate().await.unwrap().remove(0).selector();
    let config = SerialConfig {
        data_bits: DataBits::Seven,
        parity: Parity::Even,
        stop_bits: StopBits::Two,
        flow_control: FlowControl::Software,
        ..SerialConfig::default()
    };

    let error = match adapter.open(&selector, config).await {
        Ok(_) => panic!("unsupported configuration should fail"),
        Err(error) => error,
    };

    assert_eq!(
        error.name().as_str(),
        "runtime.transport.unsupported_configuration"
    );
}

#[tokio::test]
async fn read_timeout_is_reported() {
    let adapter = VirtualSerialAdapter::loopback("serial:virtual:loopback-1");
    let descriptor = adapter.enumerate().await.unwrap().remove(0);
    let config = SerialConfig {
        read_timeout: Duration::from_millis(25),
        ..SerialConfig::default()
    };
    let mut session = adapter.open(&descriptor.selector(), config).await.unwrap();

    let error = session.read(1).await.unwrap_err();

    assert_eq!(error.name().as_str(), "runtime.transport.timeout");
}

#[tokio::test]
async fn cancelled_read_does_not_poison_session() {
    let adapter = VirtualSerialAdapter::loopback("serial:virtual:loopback-1");
    let descriptor = adapter.enumerate().await.unwrap().remove(0);
    let config = SerialConfig {
        read_timeout: Duration::from_secs(1),
        ..SerialConfig::default()
    };
    let mut session = adapter.open(&descriptor.selector(), config).await.unwrap();

    {
        let read = session.read(1);
        tokio::pin!(read);
        tokio::select! {
            _ = &mut read => panic!("read should stay pending"),
            _ = tokio::time::sleep(Duration::from_millis(10)) => {}
        }
    }

    session.write_all(b"z").await.unwrap();
    session.flush().await.unwrap();
    let bytes = session.read(1).await.unwrap();

    assert_eq!(&bytes[..], b"z");
}

#[tokio::test]
async fn read_zero_bytes_is_invalid() {
    let adapter = VirtualSerialAdapter::loopback("serial:virtual:loopback-1");
    let descriptor = adapter.enumerate().await.unwrap().remove(0);
    let mut session = adapter
        .open(&descriptor.selector(), SerialConfig::default())
        .await
        .unwrap();

    let error = session.read(0).await.unwrap_err();

    assert_eq!(error.name().as_str(), "runtime.argument.invalid");
}

#[tokio::test]
async fn close_is_idempotent_and_blocks_future_operations() {
    let adapter = VirtualSerialAdapter::loopback("serial:virtual:loopback-1");
    let descriptor = adapter.enumerate().await.unwrap().remove(0);
    let mut session = adapter
        .open(&descriptor.selector(), SerialConfig::default())
        .await
        .unwrap();

    session.close().await.unwrap();
    session.close().await.unwrap();

    let error = session.read(1).await.unwrap_err();
    assert_eq!(error.name().as_str(), "runtime.session.closed");

    for result in [
        session.write_all(b"a").await,
        session.flush().await,
        session.set_control_lines(ControlLines::default()).await,
    ] {
        let error = result.unwrap_err();
        assert_eq!(error.name().as_str(), "runtime.session.closed");
    }
}

#[tokio::test]
async fn closed_state_takes_precedence_over_invalid_read_size() {
    let adapter = VirtualSerialAdapter::loopback("serial:virtual:loopback-1");
    let descriptor = adapter.enumerate().await.unwrap().remove(0);
    let mut session = adapter
        .open(&descriptor.selector(), SerialConfig::default())
        .await
        .unwrap();

    session.close().await.unwrap();

    let error = session.read(0).await.unwrap_err();
    assert_eq!(error.name().as_str(), "runtime.session.closed");
}

#[tokio::test]
async fn receive_queue_is_bounded() {
    let adapter = VirtualSerialAdapter::loopback("serial:virtual:loopback-1");
    let descriptor = adapter.enumerate().await.unwrap().remove(0);
    let mut session = adapter
        .open(&descriptor.selector(), SerialConfig::default())
        .await
        .unwrap();

    for _ in 0..64 {
        session.write_all(b"x").await.unwrap();
    }

    let error = session.write_all(b"y").await.unwrap_err();

    assert_eq!(error.name().as_str(), "runtime.queue.full");
}

#[tokio::test]
async fn ambiguous_virtual_open_fails_closed() {
    let capability = CapabilitySet::new(vec![CapabilityId::parse("serial.bytes/v1").unwrap()]);
    let descriptor = |endpoint: &str| {
        ResourceDescriptor::new(
            ResourceId::parse("serial:virtual:duplicate").unwrap(),
            Endpoint::new(endpoint).unwrap(),
            IdentityQuality::Strong,
            TransportKind::Serial,
            ResourceProperties::default(),
            capability.clone(),
        )
    };
    let adapter = VirtualSerialAdapter::from_descriptors(vec![
        descriptor("virtual://serial/first"),
        descriptor("virtual://serial/second"),
    ])
    .unwrap();
    let selector = adapter.enumerate().await.unwrap().remove(0).selector();

    let error = match adapter.open(&selector, SerialConfig::default()).await {
        Ok(_) => panic!("ambiguous open must fail closed"),
        Err(error) => error,
    };

    assert_eq!(error.name().as_str(), "runtime.resource.ambiguous");
}
