use std::collections::{HashMap, VecDeque};

use seeed_hal_core::{ErrorCategory, HalResult, LeaseToken, OwnerId, ResourceId, SessionId};
use tokio::sync::watch;

use crate::events::{EventPublisher, RuntimeEventKind};
use crate::lease_table::LeaseTable;
use crate::runtime_error;
use crate::serial_actor::{ActorHandle, ActorMetadata, SerialCommand};
use crate::session_lifecycle::SessionLifecycle;

const CLOSED_SESSION_CAPACITY: usize = 256;

struct SessionEntry {
    resource_id: ResourceId,
    owner_id: OwnerId,
    lease: LeaseToken,
    state: SessionLifecycle,
    actor: Option<ActorHandle>,
    done: watch::Sender<Option<HalResult<()>>>,
    open_cancel: watch::Sender<bool>,
}

struct ClosedSession {
    lease: LeaseToken,
}

pub(crate) struct RevokeTarget {
    pub(crate) actor: Option<ActorHandle>,
    pub(crate) done: watch::Receiver<Option<HalResult<()>>>,
    pub(crate) resource_id: ResourceId,
}

pub(crate) struct OpenReservation {
    pub(crate) lease: LeaseToken,
    pub(crate) cancellation: watch::Receiver<bool>,
}

pub(crate) enum CloseAction {
    AlreadyClosed,
    Wait(ActorHandle, ResourceId),
}

#[derive(Default)]
pub(crate) struct Registry {
    leases: LeaseTable,
    sessions: HashMap<SessionId, SessionEntry>,
    closed_sessions: HashMap<SessionId, ClosedSession>,
    closed_order: VecDeque<SessionId>,
}

impl Registry {
    pub(crate) fn reserve_open(
        &mut self,
        resource_id: ResourceId,
        session_id: SessionId,
        owner_id: OwnerId,
    ) -> HalResult<OpenReservation> {
        let lease = self.leases.reserve_control(
            resource_id.clone(),
            session_id.clone(),
            owner_id.clone(),
        )?;
        let (done, _) = watch::channel(None);
        let (open_cancel, cancellation) = watch::channel(false);
        self.sessions.insert(
            session_id,
            SessionEntry {
                resource_id,
                owner_id,
                lease: lease.clone(),
                state: SessionLifecycle::Opening,
                actor: None,
                done,
                open_cancel,
            },
        );
        Ok(OpenReservation {
            lease,
            cancellation,
        })
    }

    pub(crate) fn finish_open(
        &mut self,
        metadata: &ActorMetadata,
        actor: ActorHandle,
        events: &EventPublisher,
    ) -> bool {
        let Some(entry) = self.sessions.get_mut(&metadata.session_id) else {
            return false;
        };
        entry.actor = Some(actor);
        if entry.state.is_closing() {
            return false;
        }

        if !self
            .leases
            .commit(&entry.resource_id, &metadata.session_id, &entry.lease)
        {
            return false;
        }

        entry.state = match entry.state.commit_open("serial.open") {
            Ok(state) => state,
            Err(_) => return false,
        };
        events.publish(
            RuntimeEventKind::SessionOpened,
            entry.resource_id.clone(),
            metadata.session_id.clone(),
            entry.owner_id.clone(),
            entry.lease.generation(),
        );
        true
    }

    pub(crate) fn cancel_open(&mut self, session_id: &SessionId) {
        if let Some(entry) = self.sessions.remove(session_id) {
            self.leases.release(&entry.resource_id, session_id);
            entry.done.send_replace(Some(Ok(())));
        }
    }

    pub(crate) fn enqueue(
        &self,
        session_id: &SessionId,
        lease: &LeaseToken,
        command: SerialCommand,
        operation: &'static str,
    ) -> HalResult<ResourceId> {
        let Some(entry) = self.sessions.get(session_id) else {
            return Err(self.missing_session_error(session_id, lease, operation));
        };
        self.leases.validate(
            &entry.resource_id,
            session_id,
            &entry.owner_id,
            lease,
            operation,
        )?;
        entry
            .state
            .admit_io(operation)
            .map_err(|error| error.with_resource_id(entry.resource_id.clone()))?;
        let actor = entry.actor.as_ref().ok_or_else(|| {
            runtime_error(
                "runtime.actor.unavailable",
                ErrorCategory::Internal,
                operation,
                false,
                "the active serial session has no actor",
            )
            .with_resource_id(entry.resource_id.clone())
        })?;
        actor
            .try_enqueue(command, operation)
            .map_err(|error| error.with_resource_id(entry.resource_id.clone()))?;
        Ok(entry.resource_id.clone())
    }

    pub(crate) fn begin_close(
        &mut self,
        session_id: &SessionId,
        lease: &LeaseToken,
    ) -> HalResult<CloseAction> {
        let Some(entry) = self.sessions.get_mut(session_id) else {
            return self.closed_close_result(session_id, lease);
        };
        self.leases.validate(
            &entry.resource_id,
            session_id,
            &entry.owner_id,
            lease,
            "serial.close",
        )?;
        entry.state = entry.state.begin_close("serial.close")?;
        let actor = entry.actor.clone().ok_or_else(|| {
            runtime_error(
                "runtime.actor.unavailable",
                ErrorCategory::Internal,
                "serial.close",
                false,
                "the serial session is still opening and has no actor",
            )
            .with_resource_id(entry.resource_id.clone())
        })?;
        Ok(CloseAction::Wait(actor, entry.resource_id.clone()))
    }

    pub(crate) fn begin_revoke(&mut self, owner_id: &OwnerId) -> Vec<RevokeTarget> {
        self.sessions
            .values_mut()
            .filter(|entry| &entry.owner_id == owner_id)
            .map(|entry| {
                entry.state = entry
                    .state
                    .begin_close("runtime.owner.revoke")
                    .unwrap_or(SessionLifecycle::Closing);
                if entry.actor.is_none() {
                    entry.open_cancel.send_replace(true);
                }
                RevokeTarget {
                    actor: entry.actor.clone(),
                    done: entry.done.subscribe(),
                    resource_id: entry.resource_id.clone(),
                }
            })
            .collect()
    }

    pub(crate) fn finish_close(
        &mut self,
        metadata: &ActorMetadata,
        events: &EventPublisher,
        close_result: &HalResult<()>,
    ) {
        let Some(entry) = self.sessions.remove(&metadata.session_id) else {
            return;
        };
        let exposed = self
            .leases
            .release(&entry.resource_id, &metadata.session_id);
        if exposed {
            self.remember_closed(metadata.session_id.clone(), entry.lease.clone());
            events.publish(
                RuntimeEventKind::SessionClosed,
                entry.resource_id,
                metadata.session_id.clone(),
                entry.owner_id,
                entry.lease.generation(),
            );
        }
        entry.done.send_replace(Some(close_result.clone()));
    }

    pub(crate) fn retained_generation_count(&self) -> usize {
        self.leases.retained_generation_count()
    }

    fn remember_closed(&mut self, session_id: SessionId, lease: LeaseToken) {
        if self.closed_sessions.contains_key(&session_id) {
            return;
        }
        while self.closed_order.len() >= CLOSED_SESSION_CAPACITY {
            if let Some(expired) = self.closed_order.pop_front() {
                self.closed_sessions.remove(&expired);
            }
        }
        self.closed_order.push_back(session_id.clone());
        self.closed_sessions
            .insert(session_id, ClosedSession { lease });
    }

    fn closed_close_result(
        &self,
        session_id: &SessionId,
        lease: &LeaseToken,
    ) -> HalResult<CloseAction> {
        let Some(closed) = self.closed_sessions.get(session_id) else {
            return Err(session_not_found("serial.close"));
        };
        validate_closed_lease(&closed.lease, lease, "serial.close")?;
        Ok(CloseAction::AlreadyClosed)
    }

    fn missing_session_error(
        &self,
        session_id: &SessionId,
        lease: &LeaseToken,
        operation: &'static str,
    ) -> seeed_hal_core::HalError {
        let Some(closed) = self.closed_sessions.get(session_id) else {
            return session_not_found(operation);
        };
        match validate_closed_lease(&closed.lease, lease, operation) {
            Ok(()) => session_closed(operation),
            Err(error) => error,
        }
    }
}

fn validate_closed_lease(
    closed: &LeaseToken,
    supplied: &LeaseToken,
    operation: &'static str,
) -> HalResult<()> {
    if supplied.generation() < closed.generation() {
        return Err(runtime_error(
            "runtime.lease.stale_generation",
            ErrorCategory::Conflict,
            operation,
            false,
            "the supplied lease generation predates the closed session generation",
        ));
    }
    if supplied != closed {
        return Err(runtime_error(
            "runtime.lease.invalid_token",
            ErrorCategory::Conflict,
            operation,
            false,
            "the supplied lease token does not match the closed session",
        ));
    }
    Ok(())
}

fn session_not_found(operation: &'static str) -> seeed_hal_core::HalError {
    runtime_error(
        "runtime.session.not_found",
        ErrorCategory::NotFound,
        operation,
        false,
        "the serial session ID is unknown",
    )
}

fn session_closed(operation: &'static str) -> seeed_hal_core::HalError {
    runtime_error(
        "runtime.session.closed",
        ErrorCategory::Conflict,
        operation,
        false,
        "the serial session is closed",
    )
}
