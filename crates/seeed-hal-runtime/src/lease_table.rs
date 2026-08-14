use std::collections::HashMap;

use seeed_hal_core::{
    ErrorCategory, HalResult, LeaseId, LeaseMode, LeaseToken, OwnerId, ResourceId, SessionId,
};

use crate::runtime_error;

struct ActiveLease {
    session_id: SessionId,
    owner_id: OwnerId,
    token: LeaseToken,
    committed: bool,
}

#[derive(Default)]
pub(crate) struct LeaseTable {
    current_generations: HashMap<ResourceId, u64>,
    active: HashMap<ResourceId, ActiveLease>,
}

impl LeaseTable {
    pub(crate) fn reserve_control(
        &mut self,
        resource_id: ResourceId,
        session_id: SessionId,
        owner_id: OwnerId,
    ) -> HalResult<LeaseToken> {
        if self.active.contains_key(&resource_id) {
            return Err(runtime_error(
                "runtime.lease.conflict",
                ErrorCategory::Conflict,
                "serial.open",
                false,
                "the resource already has an active control lease",
            ));
        }

        let current = self
            .current_generations
            .entry(resource_id.clone())
            .or_default();
        *current = current.checked_add(1).ok_or_else(|| {
            runtime_error(
                "runtime.lease.generation_exhausted",
                ErrorCategory::Internal,
                "serial.open",
                false,
                "the resource lease generation reached u64::MAX",
            )
        })?;
        let token = LeaseToken::new(LeaseId::new(), *current, LeaseMode::Control);
        self.active.insert(
            resource_id,
            ActiveLease {
                session_id,
                owner_id,
                token: token.clone(),
                committed: false,
            },
        );
        Ok(token)
    }

    pub(crate) fn commit(
        &mut self,
        resource_id: &ResourceId,
        session_id: &SessionId,
        token: &LeaseToken,
    ) -> bool {
        let Some(active) = self.active.get_mut(resource_id) else {
            return false;
        };
        if &active.session_id != session_id || &active.token != token {
            return false;
        }
        active.committed = true;
        true
    }

    pub(crate) fn validate(
        &self,
        resource_id: &ResourceId,
        session_id: &SessionId,
        owner_id: &OwnerId,
        lease: &LeaseToken,
        operation: &'static str,
    ) -> HalResult<()> {
        let current_generation = self
            .current_generations
            .get(resource_id)
            .copied()
            .unwrap_or_default();
        if lease.generation() < current_generation {
            return Err(runtime_error(
                "runtime.lease.stale_generation",
                ErrorCategory::Conflict,
                operation,
                false,
                format!(
                    "lease generation {} is older than current generation {current_generation}",
                    lease.generation()
                ),
            ));
        }

        let active = self.active.get(resource_id).ok_or_else(|| {
            runtime_error(
                "runtime.session.closed",
                ErrorCategory::Conflict,
                operation,
                false,
                "the serial session is closed",
            )
        })?;
        if &active.session_id != session_id {
            return Err(runtime_error(
                "runtime.lease.invalid_token",
                ErrorCategory::Conflict,
                operation,
                false,
                "the lease does not belong to the requested session",
            ));
        }
        if &active.owner_id != owner_id {
            return Err(runtime_error(
                "runtime.lease.owner_mismatch",
                ErrorCategory::Conflict,
                operation,
                false,
                "the lease owner does not match the session owner",
            ));
        }
        if &active.token != lease {
            return Err(runtime_error(
                "runtime.lease.invalid_token",
                ErrorCategory::Conflict,
                operation,
                false,
                "the lease token does not match the active lease",
            ));
        }

        Ok(())
    }

    pub(crate) fn release(&mut self, resource_id: &ResourceId, session_id: &SessionId) -> bool {
        let Some(active) = self
            .active
            .get(resource_id)
            .filter(|lease| &lease.session_id == session_id)
        else {
            return false;
        };
        let committed = active.committed;
        let generation = active.token.generation();
        self.active.remove(resource_id);

        if !committed && self.current_generations.get(resource_id).copied() == Some(generation) {
            if generation == 1 {
                self.current_generations.remove(resource_id);
            } else if let Some(current) = self.current_generations.get_mut(resource_id) {
                *current = generation - 1;
            }
        }
        committed
    }

    pub(crate) fn retained_generation_count(&self) -> usize {
        self.current_generations.len()
    }
}
