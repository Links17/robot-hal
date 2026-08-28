use async_trait::async_trait;
use robot_hal_core::{
    CapabilitySet, ErrorCategory, ErrorContext, HalError, HalResult, IdentityQuality,
    ResourceDescriptor, ResourceId, ResourceProperties, ResourceSelector, TransportKind,
    resolve_resource,
};
use robot_hal_gpio::{
    DEFAULT_GPIO_EVENT_CAPACITY, GpioAdapter, GpioDirection, GpioEdge, GpioEdgeEvent,
    GpioEdgeRequest, GpioLineConfig, GpioLineSession, gpio_edges_capability, gpio_lines_capability,
};
use std::collections::{HashMap, HashSet, VecDeque};
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
    sessions: HashMap<u64, SessionState>,
    event_capacity: usize,
    next_session: u64,
    next_read: Option<HalError>,
}
#[derive(Debug, Default)]
struct SessionState {
    lines: Vec<u32>,
    events: VecDeque<(u32, GpioEdge, u64)>,
    dropped_events: u64,
    sequence: u64,
}
impl VirtualGpioAdapter {
    pub fn line_bank(resource_id: impl Into<String>, lines: usize) -> Self {
        Self::line_bank_with_event_capacity(resource_id, lines, DEFAULT_GPIO_EVENT_CAPACITY)
    }

    /// Creates a deterministic GPIO bank with a bounded, oldest-drop edge queue.
    pub fn line_bank_with_event_capacity(
        resource_id: impl Into<String>,
        lines: usize,
        event_capacity: usize,
    ) -> Self {
        assert!(
            (1..=robot_hal_gpio::MAX_GPIO_EVENTS).contains(&event_capacity),
            "virtual GPIO event capacity must be within public bounds"
        );
        let id = ResourceId::parse(resource_id.into()).expect("valid virtual GPIO resource id");
        let descriptor = ResourceDescriptor::new(
            id.clone(),
            robot_hal_core::Endpoint::new(format!("virtual://gpio/{}", id.as_str()))
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
                sessions: HashMap::new(),
                event_capacity,
                next_session: 0,
                next_read: None,
            })),
        }
    }
    pub fn inject_edge(&self, line: u32, edge: GpioEdge, monotonic_ns: u64) -> HalResult<()> {
        let mut s = self.state.lock().expect("virtual GPIO mutex poisoned");
        if line as usize >= s.values.len() {
            return Err(invalid("gpio.inject_edge"));
        }
        let capacity = s.event_capacity;
        for session in s
            .sessions
            .values_mut()
            .filter(|session| session.lines.contains(&line))
        {
            if session.events.len() == capacity {
                session.events.pop_front();
                session.dropped_events = session.dropped_events.saturating_add(1);
            }
            session.events.push_back((line, edge, monotonic_ns));
        }
        Ok(())
    }

    /// Returns the presently claimed lines in deterministic order.
    pub fn claimed_lines(&self) -> Vec<u32> {
        let mut lines = self
            .state
            .lock()
            .expect("virtual GPIO mutex poisoned")
            .claimed
            .iter()
            .copied()
            .collect::<Vec<_>>();
        lines.sort_unstable();
        lines
    }

    /// Makes exactly one subsequent GPIO read fail with the supplied error.
    pub fn fail_next_read(&self, error: HalError) {
        self.state
            .lock()
            .expect("virtual GPIO mutex poisoned")
            .next_read = Some(error);
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
        let session_id = s.next_session;
        s.next_session = s.next_session.saturating_add(1);
        s.sessions.insert(
            session_id,
            SessionState {
                lines: lines.to_vec(),
                ..Default::default()
            },
        );
        Ok(Box::new(VirtualGpioSession {
            descriptor: d,
            lines: lines.to_vec(),
            config,
            state: Arc::clone(&self.state),
            session_id,
            closed: false,
        }))
    }
}
struct VirtualGpioSession {
    descriptor: ResourceDescriptor,
    lines: Vec<u32>,
    config: GpioLineConfig,
    state: Arc<Mutex<State>>,
    session_id: u64,
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
        let mut s = self.state.lock().expect("virtual GPIO mutex poisoned");
        if let Some(error) = s.next_read.take() {
            return Err(error.with_resource_id(self.descriptor.id().clone()));
        }
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
        let session = s
            .sessions
            .get_mut(&self.session_id)
            .expect("open virtual GPIO session must retain queue state");
        if session.dropped_events > 0 {
            let dropped = std::mem::take(&mut session.dropped_events);
            return Err(lagged(dropped));
        }
        if let Some(i) = session
            .events
            .iter()
            .position(|(_, e, _)| request.edges().contains(*e))
        {
            let (_, e, t) = session.events.remove(i).expect("known event");
            session.sequence += 1;
            return Ok(Some(GpioEdgeEvent::new(e, t, session.sequence)));
        }
        Ok(None)
    }
    async fn close(&mut self) -> HalResult<()> {
        if !self.closed {
            let mut state = self.state.lock().expect("virtual GPIO mutex poisoned");
            for line in &self.lines {
                state.claimed.remove(line);
            }
            state.sessions.remove(&self.session_id);
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
            s.sessions.remove(&self.session_id);
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

fn lagged(dropped_count: u64) -> HalError {
    HalError::new(
        "gpio.edge.lagged",
        ErrorCategory::Unavailable,
        "gpio.next_edge",
        true,
        "the bounded virtual GPIO edge queue dropped oldest events",
    )
    .expect("valid error")
    .with_context(
        ErrorContext::new([("dropped_count", dropped_count.to_string())])
            .expect("static lag context is valid"),
    )
}
