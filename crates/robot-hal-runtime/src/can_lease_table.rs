use std::collections::HashMap;

use robot_hal_core::{
    ErrorCategory, HalError, HalResult, LeaseId, LeaseMode, LeaseToken, OwnerId, ResourceId,
    SessionId,
};

#[derive(Clone, Debug)]
pub(crate) struct CanReservation {
    resource_id: ResourceId,
    session_id: SessionId,
    owner_id: OwnerId,
    reservation_id: LeaseId,
    mode: LeaseMode,
}

impl CanReservation {
    #[allow(dead_code)] // The lease-table unit test includes this module without the runtime manager.
    pub(crate) fn owner_id(&self) -> &OwnerId {
        &self.owner_id
    }
}

#[derive(Clone)]
struct LeaseEntry {
    session_id: SessionId,
    owner_id: OwnerId,
    token: LeaseToken,
}

#[derive(Clone)]
struct PendingEntry {
    reservation_id: LeaseId,
    owner_id: OwnerId,
    mode: LeaseMode,
}

#[derive(Default)]
struct ResourceLeases {
    last_generation: u64,
    pending: HashMap<SessionId, PendingEntry>,
    observes: HashMap<SessionId, LeaseEntry>,
    control: Option<LeaseEntry>,
    maintenance: Option<LeaseEntry>,
}

impl ResourceLeases {
    fn is_empty(&self) -> bool {
        self.pending.is_empty()
            && self.observes.is_empty()
            && self.control.is_none()
            && self.maintenance.is_none()
    }
    fn has_any_lease(&self) -> bool {
        !self.is_empty()
    }
}

#[derive(Default)]
pub(crate) struct CanLeaseTable {
    resources: HashMap<ResourceId, ResourceLeases>,
}

impl CanLeaseTable {
    pub(crate) fn reserve(
        &mut self,
        resource_id: ResourceId,
        session_id: SessionId,
        owner_id: OwnerId,
        mode: LeaseMode,
    ) -> HalResult<CanReservation> {
        let leases = self.resources.entry(resource_id.clone()).or_default();
        if leases.pending.contains_key(&session_id)
            || leases.observes.contains_key(&session_id)
            || leases
                .control
                .as_ref()
                .is_some_and(|l| l.session_id == session_id)
            || leases
                .maintenance
                .as_ref()
                .is_some_and(|l| l.session_id == session_id)
        {
            return Err(conflict(
                resource_id,
                "the session already owns or is opening a CAN lease",
            ));
        }
        let incompatible = match mode {
            LeaseMode::Observe | LeaseMode::Control => {
                leases.maintenance.is_some()
                    || leases
                        .pending
                        .values()
                        .any(|p| p.mode == LeaseMode::Maintenance)
                    || (mode == LeaseMode::Control
                        && (leases.control.is_some()
                            || leases
                                .pending
                                .values()
                                .any(|p| p.mode == LeaseMode::Control)))
            }
            LeaseMode::Maintenance => leases.has_any_lease(),
        };
        if incompatible {
            return Err(conflict(
                resource_id,
                "the CAN lease is incompatible with an active or provisional lease",
            ));
        }
        let reservation_id = LeaseId::new();
        leases.pending.insert(
            session_id.clone(),
            PendingEntry {
                reservation_id: reservation_id.clone(),
                owner_id: owner_id.clone(),
                mode,
            },
        );
        Ok(CanReservation {
            resource_id,
            session_id,
            owner_id,
            reservation_id,
            mode,
        })
    }

    pub(crate) fn commit(&mut self, reservation: CanReservation) -> HalResult<LeaseToken> {
        let resource_id = reservation.resource_id.clone();
        let leases = self.resources.get_mut(&resource_id).ok_or_else(|| {
            conflict(
                resource_id.clone(),
                "the CAN reservation is no longer pending",
            )
        })?;
        let Some(pending) = leases.pending.get(&reservation.session_id) else {
            return Err(conflict(
                resource_id,
                "the CAN reservation is no longer pending",
            ));
        };
        if pending.reservation_id != reservation.reservation_id
            || pending.owner_id != reservation.owner_id
            || pending.mode != reservation.mode
        {
            return Err(conflict(
                resource_id,
                "the CAN reservation identity is invalid",
            ));
        }
        let Some(generation) = leases.last_generation.checked_add(1) else {
            leases.pending.remove(&reservation.session_id);
            return Err(HalError::new(
                "runtime.lease.generation_exhausted",
                ErrorCategory::Internal,
                "can.open",
                false,
                "the CAN lease generation reached u64::MAX",
            )
            .expect("static CAN lease error metadata is valid")
            .with_resource_id(resource_id));
        };
        leases.pending.remove(&reservation.session_id);
        leases.last_generation = generation;
        let token = LeaseToken::new(LeaseId::new(), generation, reservation.mode);
        let entry = LeaseEntry {
            session_id: reservation.session_id.clone(),
            owner_id: reservation.owner_id,
            token: token.clone(),
        };
        match reservation.mode {
            LeaseMode::Observe => {
                leases.observes.insert(reservation.session_id, entry);
            }
            LeaseMode::Control => leases.control = Some(entry),
            LeaseMode::Maintenance => leases.maintenance = Some(entry),
        }
        Ok(token)
    }

    pub(crate) fn cancel(&mut self, reservation: &CanReservation) -> bool {
        let remove_resource = {
            let Some(leases) = self.resources.get_mut(&reservation.resource_id) else {
                return false;
            };
            let removed = leases
                .pending
                .get(&reservation.session_id)
                .is_some_and(|p| {
                    p.reservation_id == reservation.reservation_id
                        && p.owner_id == reservation.owner_id
                        && p.mode == reservation.mode
                });
            if !removed {
                return false;
            }
            leases.pending.remove(&reservation.session_id);
            leases.last_generation == 0 && leases.is_empty()
        };
        if remove_resource {
            self.resources.remove(&reservation.resource_id);
        }
        true
    }

    pub(crate) fn release(
        &mut self,
        resource_id: &ResourceId,
        session_id: &SessionId,
        token: &LeaseToken,
    ) -> bool {
        let Some(leases) = self.resources.get_mut(resource_id) else {
            return false;
        };
        if leases
            .observes
            .get(session_id)
            .is_some_and(|l| l.token == *token)
        {
            return leases.observes.remove(session_id).is_some();
        }
        if leases
            .control
            .as_ref()
            .is_some_and(|l| l.session_id == *session_id && l.token == *token)
        {
            return leases.control.take().is_some();
        }
        if leases
            .maintenance
            .as_ref()
            .is_some_and(|l| l.session_id == *session_id && l.token == *token)
        {
            return leases.maintenance.take().is_some();
        }
        false
    }

    pub(crate) fn validate(
        &self,
        resource_id: &ResourceId,
        session_id: &SessionId,
        owner_id: &OwnerId,
        token: &LeaseToken,
        required_mode: LeaseMode,
        operation: &'static str,
    ) -> HalResult<()> {
        let Some(leases) = self.resources.get(resource_id) else {
            return Err(session_closed(resource_id.clone(), operation));
        };
        let active = leases
            .observes
            .get(session_id)
            .or_else(|| {
                leases
                    .control
                    .as_ref()
                    .filter(|l| &l.session_id == session_id)
            })
            .or_else(|| {
                leases
                    .maintenance
                    .as_ref()
                    .filter(|l| &l.session_id == session_id)
            });
        let Some(active) = active else {
            if token.generation() < leases.last_generation {
                return Err(HalError::new(
                    "runtime.lease.stale_generation",
                    ErrorCategory::Conflict,
                    operation,
                    false,
                    "the lease generation is no longer active",
                )
                .expect("static CAN lease error metadata is valid")
                .with_resource_id(resource_id.clone()));
            }
            return Err(session_closed(resource_id.clone(), operation));
        };
        if active.owner_id != *owner_id {
            return Err(HalError::new(
                "runtime.lease.owner_mismatch",
                ErrorCategory::Conflict,
                operation,
                false,
                "the CAN lease owner does not match the session owner",
            )
            .expect("static CAN lease error metadata is valid")
            .with_resource_id(resource_id.clone()));
        }
        if active.token != *token {
            let name = if token.generation() < active.token.generation() {
                "runtime.lease.stale_generation"
            } else {
                "runtime.lease.invalid_token"
            };
            return Err(HalError::new(
                name,
                ErrorCategory::Conflict,
                operation,
                false,
                "the CAN lease token does not match the active lease",
            )
            .expect("static CAN lease error metadata is valid")
            .with_resource_id(resource_id.clone()));
        }
        if !mode_allows(active.token.mode(), required_mode) {
            return Err(HalError::new(
                "runtime.lease.mode_denied",
                ErrorCategory::Conflict,
                operation,
                false,
                "the CAN lease mode does not permit this operation",
            )
            .expect("static CAN lease error metadata is valid")
            .with_resource_id(resource_id.clone()));
        }
        Ok(())
    }

    pub(crate) fn retained_generation_count(&self) -> usize {
        self.resources
            .values()
            .filter(|l| l.last_generation > 0)
            .count()
    }

    #[cfg(test)]
    #[allow(dead_code)]
    pub(crate) fn pending_count(&self) -> usize {
        self.resources
            .values()
            .map(|lease| lease.pending.len())
            .sum()
    }
}

fn mode_allows(actual: LeaseMode, required: LeaseMode) -> bool {
    matches!(
        (actual, required),
        (LeaseMode::Observe, LeaseMode::Observe)
            | (LeaseMode::Control, LeaseMode::Observe | LeaseMode::Control)
            | (
                LeaseMode::Maintenance,
                LeaseMode::Observe | LeaseMode::Control | LeaseMode::Maintenance
            )
    )
}

fn conflict(resource_id: ResourceId, message: &'static str) -> robot_hal_core::HalError {
    HalError::new(
        "runtime.lease.conflict",
        ErrorCategory::Conflict,
        "can.open",
        false,
        message,
    )
    .expect("static CAN lease error metadata is valid")
    .with_resource_id(resource_id)
}

fn session_closed(resource_id: ResourceId, operation: &'static str) -> robot_hal_core::HalError {
    HalError::new(
        "runtime.session.closed",
        ErrorCategory::Conflict,
        operation,
        false,
        "the CAN session is closed",
    )
    .expect("static CAN lease error metadata is valid")
    .with_resource_id(resource_id)
}
