#![forbid(unsafe_code)]

use async_trait::async_trait;
#[cfg(windows)]
use robot_hal_core::{
    CapabilitySet, Endpoint, IdentityQuality, ResourceId, ResourceProperties, TransportKind,
    resolve_resource,
};
use robot_hal_core::{ErrorCategory, HalError, HalResult, ResourceDescriptor, ResourceSelector};
use robot_hal_gpio::{GpioAdapter, GpioLineConfig, GpioLineSession};
#[cfg(windows)]
use robot_hal_gpio::{GpioDirection, GpioEdgeEvent, GpioEdgeRequest, gpio_lines_capability};
#[cfg(windows)]
use std::{
    collections::BTreeMap,
    time::{Duration, Instant},
};

#[cfg(windows)]
use windows::Devices::Gpio::{GpioController, GpioPin, GpioPinDriveMode, GpioPinValue};

#[derive(Clone, Copy, Debug, Default)]
pub struct WindowsGpioAdapter;

impl WindowsGpioAdapter {
    #[must_use]
    pub const fn new() -> Self {
        Self
    }
}

#[async_trait]
impl GpioAdapter for WindowsGpioAdapter {
    fn adapter_name(&self) -> &'static str {
        "windows-gpio"
    }

    async fn enumerate(&self) -> HalResult<Vec<ResourceDescriptor>> {
        #[cfg(windows)]
        {
            tokio::task::spawn_blocking(enumerate_sync)
                .await
                .map_err(|error| worker_failed("gpio.enumerate", error))?
        }
        #[cfg(not(windows))]
        {
            Err(unavailable("gpio.enumerate"))
        }
    }

    async fn open(
        &self,
        selector: &ResourceSelector,
        lines: &[u32],
        config: GpioLineConfig,
    ) -> HalResult<Box<dyn GpioLineSession>> {
        #[cfg(windows)]
        {
            open_sync(selector, lines, config)
        }
        #[cfg(not(windows))]
        {
            let _ = (lines, config);
            Err(unavailable("gpio.open").with_resource_id(selector.id().clone()))
        }
    }
}

#[cfg(windows)]
fn enumerate_sync() -> HalResult<Vec<ResourceDescriptor>> {
    let controller =
        GpioController::GetDefault().map_err(|error| platform_error("gpio.enumerate", error))?;
    let line_count = controller
        .PinCount()
        .map_err(|error| platform_error("gpio.enumerate", error))?;
    let id = ResourceId::parse("gpio:windows:default")?;
    let mut properties = BTreeMap::new();
    properties.insert("adapter".to_owned(), "windows-gpio".to_owned());
    properties.insert("endpoint".to_owned(), "winrt://default".to_owned());
    properties.insert("gpio.line_count".to_owned(), line_count.to_string());
    properties.insert(
        "gpio.identity_scope".to_owned(),
        "default-controller".to_owned(),
    );
    Ok(vec![ResourceDescriptor::new(
        id,
        Endpoint::new("winrt://default")?,
        // WinRT does not expose a persistent controller serial/path. This
        // identity is intentionally weak rather than fabricating one.
        IdentityQuality::Weak,
        TransportKind::Gpio,
        ResourceProperties::new(properties),
        // WinRT callbacks expose no platform monotonic timestamp; advertising
        // `gpio.edges/v1` would falsely promise the core edge-event contract.
        CapabilitySet::new(vec![gpio_lines_capability()]),
    )])
}

#[cfg(windows)]
fn open_sync(
    selector: &ResourceSelector,
    lines: &[u32],
    config: GpioLineConfig,
) -> HalResult<Box<dyn GpioLineSession>> {
    if lines.is_empty() {
        return Err(invalid("gpio.open", "at least one GPIO line is required"));
    }
    let descriptors = enumerate_sync()?;
    let descriptor = resolve_resource(
        &descriptors,
        selector,
        &gpio_lines_capability(),
        "gpio.open",
    )?
    .clone();
    let controller =
        GpioController::GetDefault().map_err(|error| platform_error("gpio.open", error))?;
    let pins = lines
        .iter()
        .map(|line| {
            let line = i32::try_from(*line)
                .map_err(|_| invalid("gpio.open", "GPIO line number exceeds i32"))?;
            let pin = controller
                .OpenPin(line)
                .map_err(|error| platform_error("gpio.open", error))?;
            configure_pin(&pin, config)?;
            Ok(pin)
        })
        .collect::<HalResult<Vec<_>>>()?;
    Ok(Box::new(WindowsGpioSession {
        descriptor,
        lines: lines.to_vec(),
        config,
        pins,
        epoch: Instant::now(),
        next_sequence: 0,
        closed: false,
    }))
}

#[cfg(windows)]
fn configure_pin(pin: &GpioPin, config: GpioLineConfig) -> HalResult<()> {
    let mode = match config.direction() {
        GpioDirection::Input => GpioPinDriveMode::Input,
        GpioDirection::Output => GpioPinDriveMode::Output,
    };
    pin.SetDriveMode(mode)
        .map_err(|error| platform_error("gpio.open", error))?;
    if let Some(value) = config.initial_value() {
        pin.Write(native_value(value))
            .map_err(|error| platform_error("gpio.open", error))?;
    }
    Ok(())
}

#[cfg(windows)]
struct WindowsGpioSession {
    descriptor: ResourceDescriptor,
    lines: Vec<u32>,
    config: GpioLineConfig,
    pins: Vec<GpioPin>,
    epoch: Instant,
    next_sequence: u64,
    closed: bool,
}

#[cfg(windows)]
#[async_trait]
impl GpioLineSession for WindowsGpioSession {
    fn descriptor(&self) -> &ResourceDescriptor {
        &self.descriptor
    }
    fn lines(&self) -> &[u32] {
        &self.lines
    }
    fn config(&self) -> GpioLineConfig {
        self.config
    }

    async fn read(&mut self) -> HalResult<Vec<bool>> {
        ensure_open(self.closed, "gpio.read")?;
        self.pins
            .iter()
            .map(|pin| {
                pin.Read()
                    .map(|value| value == GpioPinValue::High)
                    .map_err(|error| platform_error("gpio.read", error))
            })
            .collect()
    }

    async fn write(&mut self, values: &[bool]) -> HalResult<()> {
        ensure_open(self.closed, "gpio.write")?;
        if self.config.direction() != GpioDirection::Output {
            return Err(unsupported(
                "gpio.write",
                "GPIO lines were requested as inputs",
            ));
        }
        if values.len() != self.pins.len() {
            return Err(invalid(
                "gpio.write",
                "value count must equal requested line count",
            ));
        }
        for (pin, value) in self.pins.iter().zip(values) {
            pin.Write(native_value(*value))
                .map_err(|error| platform_error("gpio.write", error))?;
        }
        Ok(())
    }

    async fn next_edge(
        &mut self,
        _request: GpioEdgeRequest,
        _timeout: Duration,
    ) -> HalResult<Option<GpioEdgeEvent>> {
        ensure_open(self.closed, "gpio.next_edge")?;
        // WinRT GPIO callbacks have no monotonic timestamp. Exposing a wall-clock
        // approximation would violate the GPIO contract, so event delivery fails
        // closed until a timestamped platform source is available.
        let _ = (&self.epoch, &self.next_sequence);
        Err(unsupported(
            "gpio.next_edge",
            "WinRT GPIO does not provide monotonic edge timestamps",
        ))
    }

    async fn close(&mut self) -> HalResult<()> {
        self.closed = true;
        for pin in &self.pins {
            pin.Close()
                .map_err(|error| platform_error("gpio.close", error))?;
        }
        Ok(())
    }
}

#[cfg(windows)]
fn native_value(value: bool) -> GpioPinValue {
    if value {
        GpioPinValue::High
    } else {
        GpioPinValue::Low
    }
}

#[cfg(not(windows))]
fn unavailable(operation: &'static str) -> HalError {
    HalError::new(
        "runtime.adapter.unavailable",
        ErrorCategory::Unavailable,
        operation,
        false,
        "Windows GPIO is unavailable on this platform or controller",
    )
    .expect("static Windows GPIO adapter error metadata is valid")
}

#[cfg(windows)]
fn invalid(operation: &'static str, message: &'static str) -> HalError {
    HalError::new(
        "runtime.argument.invalid",
        ErrorCategory::InvalidArgument,
        operation,
        false,
        message,
    )
    .expect("static Windows GPIO invalid error metadata is valid")
}
#[cfg(windows)]
fn unsupported(operation: &'static str, message: &'static str) -> HalError {
    HalError::new(
        "runtime.protocol.capability_unsupported",
        ErrorCategory::Conflict,
        operation,
        false,
        message,
    )
    .expect("static Windows GPIO unsupported error metadata is valid")
}
#[cfg(windows)]
fn ensure_open(closed: bool, operation: &'static str) -> HalResult<()> {
    if closed {
        Err(HalError::new(
            "runtime.session.closed",
            ErrorCategory::Conflict,
            operation,
            false,
            "GPIO session is closed",
        )
        .expect("static Windows GPIO session error metadata is valid"))
    } else {
        Ok(())
    }
}
#[cfg(windows)]
fn platform_error(operation: &'static str, error: impl std::error::Error) -> HalError {
    HalError::new(
        "runtime.transport.unavailable",
        ErrorCategory::Unavailable,
        operation,
        true,
        format!("WinRT GPIO error: {error}"),
    )
    .expect("static Windows GPIO platform error metadata is valid")
}
#[cfg(windows)]
fn worker_failed(operation: &'static str, error: tokio::task::JoinError) -> HalError {
    HalError::new(
        "runtime.internal.worker_failed",
        ErrorCategory::Internal,
        operation,
        false,
        format!("Windows GPIO worker failed: {error}"),
    )
    .expect("static Windows GPIO worker error metadata is valid")
}
