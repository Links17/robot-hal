#![forbid(unsafe_code)]

pub mod identity;

use async_trait::async_trait;
#[cfg(target_os = "linux")]
use robot_hal_core::{
    CapabilitySet, Endpoint, ResourceProperties, TransportKind, resolve_resource,
};
use robot_hal_core::{ErrorCategory, HalError, HalResult, ResourceDescriptor, ResourceSelector};
use robot_hal_gpio::{GpioAdapter, GpioLineConfig, GpioLineSession};
#[cfg(target_os = "linux")]
use robot_hal_gpio::{
    GpioBias, GpioDirection, GpioDrive, GpioEdge, GpioEdgeEvent, GpioEdgeRequest,
    gpio_edges_capability, gpio_lines_capability,
};
#[cfg(target_os = "linux")]
use std::{collections::BTreeMap, time::Duration};

#[cfg(target_os = "linux")]
use libgpiod::{
    chip::Chip,
    line::{
        Bias, Config as LineConfig, Direction, Drive, Edge, EdgeKind, EventClock, Settings, Value,
    },
    request::{Buffer, Config as RequestConfig, Request},
};

#[derive(Clone, Copy, Debug, Default)]
pub struct LinuxGpioAdapter;

impl LinuxGpioAdapter {
    #[must_use]
    pub const fn new() -> Self {
        Self
    }
}

#[async_trait]
impl GpioAdapter for LinuxGpioAdapter {
    fn adapter_name(&self) -> &'static str {
        "linux-gpio"
    }

    async fn enumerate(&self) -> HalResult<Vec<ResourceDescriptor>> {
        #[cfg(target_os = "linux")]
        {
            tokio::task::spawn_blocking(enumerate_sync)
                .await
                .map_err(|error| worker_failed("gpio.enumerate", error))?
        }
        #[cfg(not(target_os = "linux"))]
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
        #[cfg(target_os = "linux")]
        {
            open_sync(selector, lines, config)
        }
        #[cfg(not(target_os = "linux"))]
        {
            let _ = (lines, config);
            Err(unavailable("gpio.open").with_resource_id(selector.id().clone()))
        }
    }
}

#[cfg(target_os = "linux")]
fn enumerate_sync() -> HalResult<Vec<ResourceDescriptor>> {
    std::fs::read_dir("/dev")
        .map_err(|error| platform_error("gpio.enumerate", error))?
        .filter_map(Result::ok)
        .filter_map(|entry| {
            entry
                .file_name()
                .to_str()
                .filter(|name| name.starts_with("gpiochip"))
                .map(|_| entry.path())
        })
        .map(|path| descriptor_from_path(&path))
        .collect()
}

#[cfg(target_os = "linux")]
fn descriptor_from_path(path: &std::path::Path) -> HalResult<ResourceDescriptor> {
    let chip = Chip::open(&path).map_err(|error| platform_error("gpio.enumerate", error))?;
    let info = chip
        .info()
        .map_err(|error| platform_error("gpio.enumerate", error))?;
    let metadata = identity::GpioChipMetadata {
        path: path.to_string_lossy().into_owned(),
        kernel_name: info
            .name()
            .map_err(|error| platform_error("gpio.enumerate", error))?
            .to_owned(),
        label: info.label().ok().map(ToOwned::to_owned),
        line_count: info.num_lines(),
    };
    let identity = identity::identity_from_metadata(&metadata)?;
    let mut properties = BTreeMap::new();
    properties.insert("adapter".to_owned(), "linux-gpio".to_owned());
    properties.insert("endpoint".to_owned(), metadata.path.clone());
    properties.insert("gpio.chip_name".to_owned(), metadata.kernel_name);
    properties.insert(
        "gpio.line_count".to_owned(),
        metadata.line_count.to_string(),
    );
    if let Some(label) = metadata.label {
        properties.insert("gpio.label".to_owned(), label);
    }
    Ok(ResourceDescriptor::new(
        identity.id,
        Endpoint::new(metadata.path)?,
        identity.quality,
        TransportKind::Gpio,
        ResourceProperties::new(properties),
        CapabilitySet::new(vec![gpio_lines_capability(), gpio_edges_capability()]),
    ))
}

#[cfg(target_os = "linux")]
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
    let chip =
        Chip::open(&std::path::Path::new(descriptor.endpoint().as_str())).map_err(|error| {
            platform_error("gpio.open", error).with_resource_id(descriptor.id().clone())
        })?;
    let request = request_lines(&chip, lines, config)
        .map_err(|error| error.with_resource_id(descriptor.id().clone()))?;
    Ok(Box::new(LinuxGpioSession {
        descriptor,
        lines: lines.to_vec(),
        config,
        request,
        closed: false,
    }))
}

#[cfg(target_os = "linux")]
fn request_lines(chip: &Chip, lines: &[u32], config: GpioLineConfig) -> HalResult<Request> {
    let mut settings = Settings::new().map_err(|error| platform_error("gpio.open", error))?;
    settings
        .set_direction(match config.direction() {
            GpioDirection::Input => Direction::Input,
            GpioDirection::Output => Direction::Output,
        })
        .map_err(|error| platform_error("gpio.open", error))?
        .set_bias(Some(match config.bias() {
            GpioBias::Disabled => Bias::Disabled,
            GpioBias::PullUp => Bias::PullUp,
            GpioBias::PullDown => Bias::PullDown,
        }))
        .map_err(|error| platform_error("gpio.open", error))?
        .set_active_low(config.active_low());
    if let Some(drive) = config.drive() {
        settings
            .set_drive(match drive {
                GpioDrive::PushPull => Drive::PushPull,
                GpioDrive::OpenDrain => Drive::OpenDrain,
                GpioDrive::OpenSource => Drive::OpenSource,
            })
            .map_err(|error| platform_error("gpio.open", error))?;
    }
    if let Some(value) = config.initial_value() {
        settings
            .set_output_value(value_to_native(value))
            .map_err(|error| platform_error("gpio.open", error))?;
    }
    let mut line_config = LineConfig::new().map_err(|error| platform_error("gpio.open", error))?;
    line_config
        .add_line_settings(lines, settings)
        .map_err(|error| platform_error("gpio.open", error))?;
    let mut request_config =
        RequestConfig::new().map_err(|error| platform_error("gpio.open", error))?;
    request_config
        .set_consumer("robot-hal")
        .map_err(|error| platform_error("gpio.open", error))?;
    chip.request_lines(Some(&request_config), &line_config)
        .map_err(|error| platform_error("gpio.open", error))
}

#[cfg(target_os = "linux")]
struct LinuxGpioSession {
    descriptor: ResourceDescriptor,
    lines: Vec<u32>,
    config: GpioLineConfig,
    request: Request,
    closed: bool,
}

#[cfg(target_os = "linux")]
#[async_trait]
impl GpioLineSession for LinuxGpioSession {
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
        let values = self
            .request
            .values()
            .map_err(|error| platform_error("gpio.read", error))?;
        self.lines
            .iter()
            .map(|line| {
                values
                    .get(*line)
                    .copied()
                    .map(native_to_value)
                    .ok_or_else(|| invalid("gpio.read", "requested GPIO line disappeared"))
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
        if values.len() != self.lines.len() {
            return Err(invalid(
                "gpio.write",
                "value count must equal requested line count",
            ));
        }
        self.request
            .set_values(
                &values
                    .iter()
                    .copied()
                    .map(value_to_native)
                    .collect::<Vec<_>>(),
            )
            .map_err(|error| platform_error("gpio.write", error))?;
        Ok(())
    }

    async fn next_edge(
        &mut self,
        request: GpioEdgeRequest,
        timeout: Duration,
    ) -> HalResult<Option<GpioEdgeEvent>> {
        ensure_open(self.closed, "gpio.next_edge")?;
        if self.config.direction() != GpioDirection::Input {
            return Err(unsupported(
                "gpio.next_edge",
                "GPIO edges require input lines",
            ));
        }
        let mut settings =
            Settings::new().map_err(|error| platform_error("gpio.next_edge", error))?;
        settings
            .set_direction(Direction::Input)
            .map_err(|error| platform_error("gpio.next_edge", error))?
            .set_active_low(self.config.active_low())
            .set_event_clock(EventClock::Monotonic)
            .map_err(|error| platform_error("gpio.next_edge", error))?
            .set_edge_detection(Some(
                match (
                    request.edges().contains(GpioEdge::Rising),
                    request.edges().contains(GpioEdge::Falling),
                ) {
                    (true, true) => Edge::Both,
                    (true, false) => Edge::Rising,
                    (false, true) => Edge::Falling,
                    (false, false) => return Err(invalid("gpio.next_edge", "no edge selected")),
                },
            ))
            .map_err(|error| platform_error("gpio.next_edge", error))?;
        let mut config =
            LineConfig::new().map_err(|error| platform_error("gpio.next_edge", error))?;
        config
            .add_line_settings(&self.lines, settings)
            .map_err(|error| platform_error("gpio.next_edge", error))?;
        self.request
            .reconfigure_lines(&config)
            .map_err(|error| platform_error("gpio.next_edge", error))?;
        if !self
            .request
            .wait_edge_events(Some(timeout))
            .map_err(|error| platform_error("gpio.next_edge", error))?
        {
            return Ok(None);
        }
        let mut buffer = Buffer::new(request.capacity())
            .map_err(|error| platform_error("gpio.next_edge", error))?;
        let event = self
            .request
            .read_edge_events(&mut buffer)
            .map_err(|error| platform_error("gpio.next_edge", error))?
            .next()
            .transpose()
            .map_err(|error| platform_error("gpio.next_edge", error))?
            .ok_or_else(|| {
                platform_error(
                    "gpio.next_edge",
                    std::io::Error::other("edge wakeup without event"),
                )
            })?;
        Ok(Some(GpioEdgeEvent::new(
            match event
                .event_type()
                .map_err(|error| platform_error("gpio.next_edge", error))?
            {
                EdgeKind::Rising => GpioEdge::Rising,
                EdgeKind::Falling => GpioEdge::Falling,
            },
            u64::try_from(event.timestamp().as_nanos()).unwrap_or(u64::MAX),
            event.global_seqno() as u64,
        )))
    }

    async fn close(&mut self) -> HalResult<()> {
        self.closed = true;
        Ok(())
    }
}

#[cfg(target_os = "linux")]
fn value_to_native(value: bool) -> Value {
    if value {
        Value::Active
    } else {
        Value::InActive
    }
}
#[cfg(target_os = "linux")]
fn native_to_value(value: Value) -> bool {
    value == Value::Active
}
#[cfg(target_os = "linux")]
fn ensure_open(closed: bool, operation: &'static str) -> HalResult<()> {
    if closed {
        Err(session_closed(operation))
    } else {
        Ok(())
    }
}

#[cfg(not(target_os = "linux"))]
fn unavailable(operation: &'static str) -> HalError {
    HalError::new(
        "runtime.adapter.unavailable",
        ErrorCategory::Unavailable,
        operation,
        false,
        "native Linux GPIO session support is not yet available",
    )
    .expect("static Linux GPIO adapter error metadata is valid")
}

#[cfg(target_os = "linux")]
fn invalid(operation: &'static str, message: &'static str) -> HalError {
    HalError::new(
        "runtime.argument.invalid",
        ErrorCategory::InvalidArgument,
        operation,
        false,
        message,
    )
    .expect("static GPIO invalid error metadata is valid")
}
#[cfg(target_os = "linux")]
fn unsupported(operation: &'static str, message: &'static str) -> HalError {
    HalError::new(
        "runtime.protocol.capability_unsupported",
        ErrorCategory::Conflict,
        operation,
        false,
        message,
    )
    .expect("static GPIO unsupported error metadata is valid")
}
#[cfg(target_os = "linux")]
fn session_closed(operation: &'static str) -> HalError {
    HalError::new(
        "runtime.session.closed",
        ErrorCategory::Conflict,
        operation,
        false,
        "GPIO session is closed",
    )
    .expect("static GPIO session error metadata is valid")
}
#[cfg(target_os = "linux")]
fn platform_error(operation: &'static str, error: impl std::error::Error) -> HalError {
    HalError::new(
        "runtime.transport.unavailable",
        ErrorCategory::Unavailable,
        operation,
        true,
        format!("libgpiod error: {error}"),
    )
    .expect("static GPIO platform error metadata is valid")
}
#[cfg(target_os = "linux")]
fn worker_failed(operation: &'static str, error: tokio::task::JoinError) -> HalError {
    HalError::new(
        "runtime.internal.worker_failed",
        ErrorCategory::Internal,
        operation,
        false,
        format!("GPIO worker failed: {error}"),
    )
    .expect("static GPIO worker error metadata is valid")
}
