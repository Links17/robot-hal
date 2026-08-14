use async_trait::async_trait;
use bytes::Bytes;
use seeed_hal_core::{HalError, HalResult, ResourceDescriptor};
use seeed_hal_serial::{
    ControlLines, DataBits, FlowControl, Parity, SerialConfig, SerialSession, StopBits,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio_serial::SerialPortBuilderExt;

use crate::{invalid_argument, map_io_error, map_serialport_error, session_closed, timeout};

pub(crate) struct NativeSerialSession {
    descriptor: ResourceDescriptor,
    config: SerialConfig,
    stream: Option<tokio_serial::SerialStream>,
}

impl NativeSerialSession {
    pub(crate) async fn open(
        descriptor: ResourceDescriptor,
        config: SerialConfig,
    ) -> HalResult<Self> {
        validate_config(&config)?;

        let stream = tokio_serial::new(descriptor.endpoint().as_str(), config.baud_rate)
            .data_bits(map_data_bits(config.data_bits))
            .parity(map_parity(config.parity))
            .stop_bits(map_stop_bits(config.stop_bits))
            .flow_control(map_flow_control(config.flow_control))
            .timeout(config.read_timeout)
            .open_native_async()
            .map_err(|error| map_serialport_error("serial.open", error))?;

        Ok(Self {
            descriptor,
            config,
            stream: Some(stream),
        })
    }

    fn stream_mut(
        &mut self,
        operation: &'static str,
    ) -> HalResult<&mut tokio_serial::SerialStream> {
        self.stream
            .as_mut()
            .ok_or_else(|| session_closed(operation, "serial session is already closed"))
    }
}

#[async_trait]
impl SerialSession for NativeSerialSession {
    fn descriptor(&self) -> &ResourceDescriptor {
        &self.descriptor
    }

    async fn read(&mut self, max_bytes: usize) -> HalResult<Bytes> {
        if max_bytes == 0 {
            return Err(invalid_argument(
                "serial.read",
                "max_bytes must be greater than zero",
            ));
        }

        let read_timeout = self.config.read_timeout;
        let stream = self.stream_mut("serial.read")?;
        let mut buffer = vec![0_u8; max_bytes];

        let read = tokio::time::timeout(read_timeout, stream.read(&mut buffer))
            .await
            .map_err(|_| {
                timeout(
                    "serial.read",
                    format!("read timed out after {read_timeout:?}"),
                )
            })?
            .map_err(|error| map_io_error("serial.read", error))?;

        if read == 0 {
            return Err(disconnected(
                "serial.read",
                "serial port returned end of stream",
            ));
        }

        buffer.truncate(read);
        Ok(Bytes::from(buffer))
    }

    async fn write_all(&mut self, bytes: &[u8]) -> HalResult<()> {
        if bytes.is_empty() {
            self.stream_mut("serial.write")?;
            return Ok(());
        }

        self.stream_mut("serial.write")?
            .write_all(bytes)
            .await
            .map_err(|error| map_io_error("serial.write", error))
    }

    async fn flush(&mut self) -> HalResult<()> {
        self.stream_mut("serial.flush")?
            .flush()
            .await
            .map_err(|error| map_io_error("serial.flush", error))
    }

    async fn set_control_lines(&mut self, lines: ControlLines) -> HalResult<()> {
        let stream = self.stream_mut("serial.set_control_lines")?;
        tokio_serial::SerialPort::write_data_terminal_ready(stream, lines.data_terminal_ready)
            .map_err(|error| map_serialport_error("serial.set_control_lines", error))?;
        tokio_serial::SerialPort::write_request_to_send(stream, lines.request_to_send)
            .map_err(|error| map_serialport_error("serial.set_control_lines", error))
    }

    async fn close(&mut self) -> HalResult<()> {
        let Some(mut stream) = self.stream.take() else {
            return Ok(());
        };

        match stream.shutdown().await {
            Ok(()) => Ok(()),
            Err(error) if is_disconnect_like(error.kind()) => Ok(()),
            Err(error) => Err(map_io_error("serial.close", error)),
        }
    }
}

fn validate_config(config: &SerialConfig) -> HalResult<()> {
    if config.baud_rate == 0 {
        return Err(invalid_argument(
            "serial.open",
            "baud_rate must be greater than zero",
        ));
    }

    if config.read_timeout.is_zero() {
        return Err(invalid_argument(
            "serial.open",
            "read_timeout must be greater than zero",
        ));
    }

    Ok(())
}

fn map_data_bits(data_bits: DataBits) -> tokio_serial::DataBits {
    match data_bits {
        DataBits::Five => tokio_serial::DataBits::Five,
        DataBits::Six => tokio_serial::DataBits::Six,
        DataBits::Seven => tokio_serial::DataBits::Seven,
        DataBits::Eight => tokio_serial::DataBits::Eight,
    }
}

fn map_parity(parity: Parity) -> tokio_serial::Parity {
    match parity {
        Parity::None => tokio_serial::Parity::None,
        Parity::Odd => tokio_serial::Parity::Odd,
        Parity::Even => tokio_serial::Parity::Even,
    }
}

fn map_stop_bits(stop_bits: StopBits) -> tokio_serial::StopBits {
    match stop_bits {
        StopBits::One => tokio_serial::StopBits::One,
        StopBits::Two => tokio_serial::StopBits::Two,
    }
}

fn map_flow_control(flow_control: FlowControl) -> tokio_serial::FlowControl {
    match flow_control {
        FlowControl::None => tokio_serial::FlowControl::None,
        FlowControl::Software => tokio_serial::FlowControl::Software,
        FlowControl::Hardware => tokio_serial::FlowControl::Hardware,
    }
}

fn disconnected(operation: &'static str, debug_message: impl Into<String>) -> HalError {
    HalError::new(
        "runtime.transport.disconnected",
        seeed_hal_core::ErrorCategory::Unavailable,
        operation,
        true,
        debug_message,
    )
    .expect("static serialport adapter error metadata must be valid")
}

fn is_disconnect_like(kind: std::io::ErrorKind) -> bool {
    matches!(
        kind,
        std::io::ErrorKind::NotConnected
            | std::io::ErrorKind::ConnectionAborted
            | std::io::ErrorKind::ConnectionReset
            | std::io::ErrorKind::BrokenPipe
            | std::io::ErrorKind::UnexpectedEof
    )
}
