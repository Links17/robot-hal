#[path = "../src/can_lease_table.rs"]
mod can_lease_table;

use can_lease_table::CanLeaseTable;
use robot_hal_core::{
    ErrorCategory, LeaseId, LeaseMode, LeaseToken, OwnerId, ResourceId, SessionId,
};

fn ids() -> (ResourceId, SessionId, OwnerId) {
    (
        ResourceId::parse("can:test").unwrap(),
        SessionId::parse("session-a").unwrap(),
        OwnerId::parse("owner-a").unwrap(),
    )
}

fn session(name: &str) -> SessionId {
    SessionId::parse(name).unwrap()
}
fn owner(name: &str) -> OwnerId {
    OwnerId::parse(name).unwrap()
}

#[test]
fn observe_fanout_and_control_are_compatible() {
    let (resource, first, first_owner) = ids();
    let mut table = CanLeaseTable::default();
    let observe = table
        .reserve(
            resource.clone(),
            first.clone(),
            first_owner.clone(),
            LeaseMode::Observe,
        )
        .unwrap();
    let observe_token = table.commit(observe).unwrap();
    let second = table
        .reserve(
            resource.clone(),
            session("session-b"),
            owner("owner-b"),
            LeaseMode::Observe,
        )
        .unwrap();
    let second_token = table.commit(second).unwrap();
    let control = table
        .reserve(
            resource.clone(),
            session("session-c"),
            owner("owner-c"),
            LeaseMode::Control,
        )
        .unwrap();
    let control_token = table.commit(control).unwrap();
    assert!(
        table
            .validate(
                &resource,
                &first,
                &first_owner,
                &observe_token,
                LeaseMode::Observe,
                "can.receive"
            )
            .is_ok()
    );
    assert!(
        table
            .validate(
                &resource,
                &session("session-c"),
                &owner("owner-c"),
                &control_token,
                LeaseMode::Control,
                "can.send"
            )
            .is_ok()
    );
    assert!(table.release(&resource, &session("session-b"), &second_token));
}

#[test]
fn compatibility_matrix_includes_provisional_leases() {
    let (resource, session_id, owner_id) = ids();
    let mut table = CanLeaseTable::default();
    let maintenance = table
        .reserve(
            resource.clone(),
            session_id.clone(),
            owner_id,
            LeaseMode::Maintenance,
        )
        .unwrap();
    assert!(
        table
            .reserve(
                resource.clone(),
                session("b"),
                owner("b"),
                LeaseMode::Observe
            )
            .is_err()
    );
    assert!(
        table
            .reserve(
                resource.clone(),
                session("c"),
                owner("c"),
                LeaseMode::Control
            )
            .is_err()
    );
    assert!(table.cancel(&maintenance));
    let observe = table
        .reserve(resource.clone(), session_id, owner("a"), LeaseMode::Observe)
        .unwrap();
    assert!(
        table
            .reserve(
                resource.clone(),
                session("c"),
                owner("c"),
                LeaseMode::Maintenance
            )
            .is_err()
    );
    assert!(table.cancel(&observe));
}

#[test]
fn active_maintenance_blocks_every_other_open_and_restores_after_release() {
    let (resource, session_id, owner_id) = ids();
    let mut table = CanLeaseTable::default();
    let reservation = table
        .reserve(
            resource.clone(),
            session_id.clone(),
            owner_id.clone(),
            LeaseMode::Maintenance,
        )
        .unwrap();
    let maintenance = table.commit(reservation).unwrap();
    for mode in [
        LeaseMode::Observe,
        LeaseMode::Control,
        LeaseMode::Maintenance,
    ] {
        assert!(
            table
                .reserve(resource.clone(), session("blocked"), owner("other"), mode)
                .is_err()
        );
    }
    assert!(table.release(&resource, &session_id, &maintenance));
    let reservation = table
        .reserve(
            resource.clone(),
            session("other"),
            owner("other"),
            LeaseMode::Observe,
        )
        .unwrap();
    let reopened = table.commit(reservation).unwrap();
    assert_eq!(reopened.generation(), 2);
}

#[test]
fn pending_control_rejects_duplicate_control_but_allows_observe() {
    let (resource, session_id, owner_id) = ids();
    let mut table = CanLeaseTable::default();
    let pending_control = table
        .reserve(resource.clone(), session_id, owner_id, LeaseMode::Control)
        .unwrap();
    assert!(
        table
            .reserve(
                resource.clone(),
                session("control-2"),
                owner("owner-2"),
                LeaseMode::Control
            )
            .is_err()
    );
    let observer = table
        .reserve(
            resource.clone(),
            session("observer"),
            owner("observer"),
            LeaseMode::Observe,
        )
        .unwrap();
    assert!(table.cancel(&observer));
    assert!(table.cancel(&pending_control));
}

#[test]
fn active_control_rejects_new_control_with_canonical_conflict() {
    let (resource, session_id, owner_id) = ids();
    let mut table = CanLeaseTable::default();
    let reservation = table
        .reserve(
            resource.clone(),
            session_id.clone(),
            owner_id.clone(),
            LeaseMode::Control,
        )
        .unwrap();
    let active = table.commit(reservation).unwrap();
    let error = table
        .reserve(
            resource.clone(),
            session("control-2"),
            owner("owner-2"),
            LeaseMode::Control,
        )
        .unwrap_err();
    assert_eq!(error.name().as_str(), "runtime.lease.conflict");
    assert_eq!(error.resource_id(), Some(&resource));
    assert!(table.release(&resource, &session_id, &active));
}

#[test]
fn generations_are_monotonic_and_cancel_does_not_advance() {
    let (resource, session_id, owner_id) = ids();
    let mut table = CanLeaseTable::default();
    let pending = table
        .reserve(
            resource.clone(),
            session_id.clone(),
            owner_id.clone(),
            LeaseMode::Control,
        )
        .unwrap();
    assert!(table.cancel(&pending));
    let reservation = table
        .reserve(
            resource.clone(),
            session_id.clone(),
            owner_id.clone(),
            LeaseMode::Control,
        )
        .unwrap();
    let first = table.commit(reservation).unwrap();
    assert_eq!(first.generation(), 1);
    assert!(table.release(&resource, &session_id, &first));
    let reservation = table
        .reserve(resource.clone(), session_id, owner_id, LeaseMode::Control)
        .unwrap();
    let second = table.commit(reservation).unwrap();
    assert_eq!(second.generation(), 2);
}

#[test]
fn old_observe_remains_valid_after_newer_compatible_open() {
    let (resource, session_id, owner_id) = ids();
    let mut table = CanLeaseTable::default();
    let reservation = table
        .reserve(
            resource.clone(),
            session_id.clone(),
            owner_id.clone(),
            LeaseMode::Observe,
        )
        .unwrap();
    let old = table.commit(reservation).unwrap();
    let reservation = table
        .reserve(
            resource.clone(),
            session("b"),
            owner("b"),
            LeaseMode::Control,
        )
        .unwrap();
    let newer = table.commit(reservation).unwrap();
    assert_eq!(newer.generation(), 2);
    assert!(
        table
            .validate(
                &resource,
                &session_id,
                &owner_id,
                &old,
                LeaseMode::Observe,
                "can.receive"
            )
            .is_ok()
    );
}

#[test]
fn validation_checks_owner_token_and_operation_mode() {
    let (resource, session_id, owner_id) = ids();
    let mut table = CanLeaseTable::default();
    let reservation = table
        .reserve(
            resource.clone(),
            session_id.clone(),
            owner_id.clone(),
            LeaseMode::Observe,
        )
        .unwrap();
    let token = table.commit(reservation).unwrap();
    assert!(
        table
            .validate(
                &resource,
                &session_id,
                &owner("wrong"),
                &token,
                LeaseMode::Observe,
                "can.receive"
            )
            .is_err()
    );
    assert!(
        table
            .validate(
                &resource,
                &session_id,
                &owner_id,
                &token,
                LeaseMode::Control,
                "can.send"
            )
            .is_err()
    );
    let forged = LeaseToken::new(LeaseId::new(), token.generation(), LeaseMode::Observe);
    let token_error = table
        .validate(
            &resource,
            &session_id,
            &owner_id,
            &forged,
            LeaseMode::Observe,
            "can.receive",
        )
        .unwrap_err();
    assert_eq!(token_error.name().as_str(), "runtime.lease.invalid_token");
    assert_eq!(token_error.resource_id(), Some(&resource));
    assert!(
        table
            .validate(
                &resource,
                &session_id,
                &owner_id,
                &token,
                LeaseMode::Observe,
                "can.receive"
            )
            .is_ok()
    );
}

#[test]
fn maintenance_authorizes_receive_send_status_and_configure_modes() {
    let (resource, session_id, owner_id) = ids();
    let mut table = CanLeaseTable::default();
    let reservation = table
        .reserve(
            resource.clone(),
            session_id.clone(),
            owner_id.clone(),
            LeaseMode::Maintenance,
        )
        .unwrap();
    let token = table.commit(reservation).unwrap();
    for (required, operation) in [
        (LeaseMode::Observe, "can.receive"),
        (LeaseMode::Observe, "can.status"),
        (LeaseMode::Control, "can.send"),
        (LeaseMode::Maintenance, "can.configure"),
    ] {
        assert!(
            table
                .validate(
                    &resource,
                    &session_id,
                    &owner_id,
                    &token,
                    required,
                    operation
                )
                .is_ok()
        );
    }
}

#[test]
fn stale_generation_after_release_and_newer_open_is_fenced() {
    let (resource, session_id, owner_id) = ids();
    let mut table = CanLeaseTable::default();
    let reservation = table
        .reserve(
            resource.clone(),
            session_id.clone(),
            owner_id.clone(),
            LeaseMode::Observe,
        )
        .unwrap();
    let old = table.commit(reservation).unwrap();
    assert!(table.release(&resource, &session_id, &old));
    let reservation = table
        .reserve(
            resource.clone(),
            session("new"),
            owner("new"),
            LeaseMode::Observe,
        )
        .unwrap();
    let newer = table.commit(reservation).unwrap();
    assert_eq!(newer.generation(), old.generation() + 1);
    let stale = table
        .validate(
            &resource,
            &session_id,
            &owner_id,
            &old,
            LeaseMode::Observe,
            "can.receive",
        )
        .unwrap_err();
    assert_eq!(stale.name().as_str(), "runtime.lease.stale_generation");
    assert_eq!(stale.category(), ErrorCategory::Conflict);
    assert_eq!(stale.resource_id(), Some(&resource));
}

#[test]
fn failed_reopen_cancellation_does_not_consume_generation() {
    let (resource, session_id, owner_id) = ids();
    let mut table = CanLeaseTable::default();
    let reservation = table
        .reserve(
            resource.clone(),
            session_id.clone(),
            owner_id.clone(),
            LeaseMode::Control,
        )
        .unwrap();
    let first = table.commit(reservation).unwrap();
    assert!(table.release(&resource, &session_id, &first));
    let failed_reopen = table
        .reserve(
            resource.clone(),
            session_id.clone(),
            owner_id.clone(),
            LeaseMode::Control,
        )
        .unwrap();
    assert!(table.cancel(&failed_reopen));
    let reservation = table
        .reserve(resource, session_id, owner_id, LeaseMode::Control)
        .unwrap();
    let reopened = table.commit(reservation).unwrap();
    assert_eq!(reopened.generation(), 2);
}

#[test]
fn conflicts_include_canonical_resource_and_distinguish_two_owners() {
    let (resource, session_id, owner_id) = ids();
    let mut table = CanLeaseTable::default();
    let reservation = table
        .reserve(resource.clone(), session_id, owner_id, LeaseMode::Control)
        .unwrap();
    let _active = table.commit(reservation).unwrap();
    let error = table
        .reserve(
            resource.clone(),
            session("other-session"),
            owner("other-owner"),
            LeaseMode::Control,
        )
        .unwrap_err();
    assert_eq!(error.name().as_str(), "runtime.lease.conflict");
    assert_eq!(error.category(), ErrorCategory::Conflict);
    assert_eq!(error.resource_id(), Some(&resource));
}

#[test]
fn thousands_of_failed_reservations_leave_no_pending_entries() {
    let (resource, session_id, owner_id) = ids();
    let mut table = CanLeaseTable::default();
    for _ in 0..4096 {
        let reservation = table
            .reserve(
                resource.clone(),
                session_id.clone(),
                owner_id.clone(),
                LeaseMode::Control,
            )
            .unwrap();
        assert!(table.cancel(&reservation));
    }
    assert_eq!(table.pending_count(), 0);
    assert_eq!(table.retained_generation_count(), 0);
}
