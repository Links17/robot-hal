use async_trait::async_trait;
use seeed_hal_core::{
    CapabilitySet, ErrorCategory, HalError, HalResult, IdentityQuality, ResourceDescriptor,
    ResourceId, ResourceProperties, ResourceSelector, TransportKind, resolve_resource,
};
use seeed_hal_gpio::{
    GpioAdapter, GpioDirection, GpioEdge, GpioEdgeEvent, GpioEdgeRequest, GpioLineConfig,
    GpioLineSession, gpio_edges_capability, gpio_lines_capability,
};
use std::collections::{HashSet, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::Duration;

#[derive(Clone, Debug)]
pub struct VirtualGpioAdapter {
    descriptor: ResourceDescriptor,
    state: Arc<Mutex<State>>,
}
#[derive(Debug)]
struct State {
    values: Vec<bool>,
    claimed: HashSet<u32>,
    events: VecDeque<(u32, GpioEdge, u64)>,
    sequence: u64,
}
impl VirtualGpioAdapter {
    pub fn line_bank(resource_id: impl Into<String>, lines: usize) -> Self {
        let id = ResourceId::parse(resource_id.into()).expect("valid virtual GPIO resource id");
        let descriptor = ResourceDescriptor::new(
            id.clone(),
            seeed_hal_core::Endpoint::new(format!("virtual://gpio/{}", id.as_str()))
                .expect("valid endpoint"),
            IdentityQuality::Strong,
            TransportKind::Gpio,
            ResourceProperties::default(),
            CapabilitySet::new(vec![gpio_lines_capability(), gpio_edges_capability()]),
        );
        Self {
            descriptor,
            state: Arc::new(Mutex::new(State {
                values: vec![false; lines],
                claimed: HashSet::new(),
                events: VecDeque::new(),
                sequence: 0,
            })),
        }
    }
    pub fn inject_edge(&self, line: u32, edge: GpioEdge, monotonic_ns: u64) -> HalResult<()> {
        let mut s = self.state.lock().expect("virtual GPIO mutex poisoned");
        if line as usize >= s.values.len() {
            return Err(invalid("gpio.inject_edge"));
        }
        s.events.push_back((line, edge, monotonic_ns));
        Ok(())
    }
}
#[async_trait]
impl GpioAdapter for VirtualGpioAdapter {
    fn adapter_name(&self) -> &'static str {
        "virtual.gpio.line_bank"
    }
    async fn enumerate(&self) -> HalResult<Vec<ResourceDescriptor>> {
        Ok(vec![self.descriptor.clone()])
    }
    async fn open(
        &self,
        selector: &ResourceSelector,
        lines: &[u32],
        config: GpioLineConfig,
    ) -> HalResult<Box<dyn GpioLineSession>> {
        let d = resolve_resource(
            std::slice::from_ref(&self.descriptor),
            selector,
            &gpio_lines_capability(),
            "gpio.open",
        )?
        .clone();
        let mut s = self.state.lock().expect("virtual GPIO mutex poisoned");
        if lines.is_empty()
            || lines
                .iter()
                .any(|l| *l as usize >= s.values.len() || s.claimed.contains(l))
        {
            return Err(conflict("gpio.open"));
        }
        for l in lines {
            s.claimed.insert(*l);
            if let Some(v) = config.initial_value() {
                s.values[*l as usize] = v;
            }
        }
        Ok(Box::new(VirtualGpioSession {
            descriptor: d,
            lines: lines.to_vec(),
            config,
            state: Arc::clone(&self.state),
            closed: false,
        }))
    }
}
struct VirtualGpioSession {
    descriptor: ResourceDescriptor,
    lines: Vec<u32>,
    config: GpioLineConfig,
    state: Arc<Mutex<State>>,
    closed: bool,
}
#[async_trait]
impl GpioLineSession for VirtualGpioSession {
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
        let s = self.state.lock().expect("virtual GPIO mutex poisoned");
        Ok(self.lines.iter().map(|l| s.values[*l as usize]).collect())
    }
    async fn write(&mut self, values: &[bool]) -> HalResult<()> {
        if self.config.direction() != GpioDirection::Output {
            return Err(direction("gpio.write"));
        }
        if values.len() != self.lines.len() {
            return Err(invalid("gpio.write"));
        }
        let mut s = self.state.lock().expect("virtual GPIO mutex poisoned");
        for (l, v) in self.lines.iter().zip(values) {
            s.values[*l as usize] = *v;
        }
        Ok(())
    }
    async fn next_edge(
        &mut self,
        request: GpioEdgeRequest,
        _: Duration,
    ) -> HalResult<Option<GpioEdgeEvent>> {
        let mut s = self.state.lock().expect("virtual GPIO mutex poisoned");
        if let Some(i) = s
            .events
            .iter()
            .position(|(l, e, _)| self.lines.contains(l) && request.edges().contains(*e))
        {
            let (_, e, t) = s.events.remove(i).expect("known event");
            s.sequence += 1;
            return Ok(Some(GpioEdgeEvent::new(e, t, s.sequence)));
        }
        Ok(None)
    }
    async fn close(&mut self) -> HalResult<()> {
        if !self.closed {
            for l in &self.lines {
                self.state
                    .lock()
                    .expect("virtual GPIO mutex poisoned")
                    .claimed
                    .remove(l);
            }
            self.closed = true;
        }
        Ok(())
    }
}
impl Drop for VirtualGpioSession {
    fn drop(&mut self) {
        if !self.closed {
            let mut s = self.state.lock().expect("virtual GPIO mutex poisoned");
            for l in &self.lines {
                s.claimed.remove(l);
            }
        }
    }
}
fn invalid(op: &'static str) -> HalError {
    HalError::new(
        "runtime.argument.invalid",
        ErrorCategory::InvalidArgument,
        op,
        false,
        "invalid virtual GPIO request",
    )
    .expect("valid error")
}
fn conflict(op: &'static str) -> HalError {
    HalError::new(
        "runtime.adapter.conflict",
        ErrorCategory::Conflict,
        op,
        false,
        "GPIO line is already claimed",
    )
    .expect("valid error")
}
fn direction(op: &'static str) -> HalError {
    HalError::new(
        "gpio.direction.invalid",
        ErrorCategory::InvalidArgument,
        op,
        false,
        "GPIO line is not configured for output",
    )
    .expect("valid error")
}
