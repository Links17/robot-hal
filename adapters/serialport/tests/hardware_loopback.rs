#[tokio::test]
#[ignore = "requires SEEED_HAL_SERIAL_LOOPBACK"]
async fn physical_loopback_round_trip_preserves_byte_order() {
    #[cfg(not(feature = "hardware-loopback"))]
    {
        panic!("enable the hardware-tests feature and set SEEED_HAL_SERIAL_LOOPBACK");
    }

    #[cfg(feature = "hardware-loopback")]
    {
        use seeed_hal_adapter_serialport::SerialPortAdapter;
        use seeed_hal_serial::{SerialAdapter, SerialConfig};
        use std::time::{Duration, Instant};

        let endpoint = std::env::var("SEEED_HAL_SERIAL_LOOPBACK")
            .expect("SEEED_HAL_SERIAL_LOOPBACK must name the loopback serial port");
        let adapter = SerialPortAdapter::new();
        let descriptor = adapter
            .enumerate()
            .await
            .unwrap()
            .into_iter()
            .find(|descriptor| descriptor.endpoint().as_str() == endpoint)
            .expect("loopback endpoint must be present in serial enumeration");
        let config = SerialConfig {
            read_timeout: Duration::from_millis(250),
            ..SerialConfig::default()
        };
        let mut session = adapter.open(&descriptor.selector(), config).await.unwrap();
        let payload = b"seeed-hal-loopback";

        session.write_all(payload).await.unwrap();
        session.flush().await.unwrap();

        let deadline = Instant::now() + Duration::from_secs(2);
        let mut received = Vec::new();
        while received.len() < payload.len() && Instant::now() < deadline {
            match session.read(payload.len() - received.len()).await {
                Ok(bytes) => received.extend_from_slice(&bytes),
                Err(error)
                    if error.name().as_str() == "runtime.transport.timeout"
                        && Instant::now() < deadline => {}
                Err(error) => panic!("loopback read failed: {error}"),
            }
        }

        assert_eq!(received, payload);
        session.close().await.unwrap();
    }
}
