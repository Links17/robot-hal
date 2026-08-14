use std::sync::{Arc, Mutex};

use seeed_hal_core::{ErrorCategory, HalError, HalResult, OwnerId, ResourceId, SessionId};
use tokio::sync::broadcast;

use crate::runtime_error;

const EVENT_QUEUE_CAPACITY: usize = 64;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RuntimeEventKind {
    SessionOpened,
    SessionClosed,
}

impl RuntimeEventKind {
    fn name(self) -> &'static str {
        match self {
            Self::SessionOpened => "session.opened",
            Self::SessionClosed => "session.closed",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RuntimeEvent {
    sequence: u64,
    kind: RuntimeEventKind,
    resource_id: ResourceId,
    session_id: SessionId,
    owner_id: OwnerId,
    lease_generation: u64,
}

impl RuntimeEvent {
    pub fn sequence(&self) -> u64 {
        self.sequence
    }

    pub fn kind(&self) -> RuntimeEventKind {
        self.kind
    }

    pub fn name(&self) -> &'static str {
        self.kind.name()
    }

    pub fn resource_id(&self) -> &ResourceId {
        &self.resource_id
    }

    pub fn session_id(&self) -> &SessionId {
        &self.session_id
    }

    pub fn owner_id(&self) -> &OwnerId {
        &self.owner_id
    }

    pub fn lease_generation(&self) -> u64 {
        self.lease_generation
    }
}

struct EventState {
    next_sequence: u64,
}

#[derive(Clone)]
pub(crate) struct EventPublisher {
    sender: broadcast::Sender<RuntimeEvent>,
    state: Arc<Mutex<EventState>>,
}

impl EventPublisher {
    pub(crate) fn new() -> Self {
        let (sender, _) = broadcast::channel(EVENT_QUEUE_CAPACITY);
        Self {
            sender,
            state: Arc::new(Mutex::new(EventState { next_sequence: 1 })),
        }
    }

    pub(crate) fn subscribe(&self) -> EventSubscription {
        EventSubscription {
            receiver: self.sender.subscribe(),
        }
    }

    pub(crate) fn publish(
        &self,
        kind: RuntimeEventKind,
        resource_id: ResourceId,
        session_id: SessionId,
        owner_id: OwnerId,
        lease_generation: u64,
    ) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let event = RuntimeEvent {
            sequence: state.next_sequence,
            kind,
            resource_id,
            session_id,
            owner_id,
            lease_generation,
        };
        state.next_sequence = state.next_sequence.saturating_add(1);
        let _ = self.sender.send(event);
    }
}

pub struct EventSubscription {
    receiver: broadcast::Receiver<RuntimeEvent>,
}

impl EventSubscription {
    pub async fn recv(&mut self) -> HalResult<RuntimeEvent> {
        self.receiver.recv().await.map_err(|error| match error {
            broadcast::error::RecvError::Closed => event_error(
                "runtime.event.closed",
                ErrorCategory::Unavailable,
                false,
                "the runtime event stream is closed",
            ),
            broadcast::error::RecvError::Lagged(skipped) => event_error(
                "runtime.event.lagged",
                ErrorCategory::Unavailable,
                true,
                format!("event subscriber fell behind by {skipped} events"),
            ),
        })
    }
}

fn event_error(
    name: &'static str,
    category: ErrorCategory,
    retryable: bool,
    debug_message: impl Into<String>,
) -> HalError {
    runtime_error(
        name,
        category,
        "runtime.event.receive",
        retryable,
        debug_message,
    )
}
