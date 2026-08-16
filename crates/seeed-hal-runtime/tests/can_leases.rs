#[path = "../src/can_lease_table.rs"]
mod can_lease_table;

use can_lease_table::CanLeaseTable;
use seeed_hal_core::{LeaseMode, OwnerId, ResourceId, SessionId};

fn ids() -> (ResourceId, SessionId, OwnerId) {
    (
        ResourceId::parse("can:test").unwrap(),
        SessionId::parse("session-a").unwrap(),
        OwnerId::parse("owner-a").unwrap(),
    )
}

fn session(name: &str) -> SessionId { SessionId::parse(name).unwrap() }
fn owner(name: &str) -> OwnerId { OwnerId::parse(name).unwrap() }

#[test]
fn observe_fanout_and_control_are_compatible() {
    let (resource, first, first_owner) = ids();
    let mut table = CanLeaseTable::default();
    let observe = table.reserve(resource.clone(), first.clone(), first_owner.clone(), LeaseMode::Observe).unwrap();
    let observe_token = table.commit(observe).unwrap();
    let second = table.reserve(resource.clone(), session("session-b"), owner("owner-b"), LeaseMode::Observe).unwrap();
    let second_token = table.commit(second).unwrap();
    let control = table.reserve(resource.clone(), session("session-c"), owner("owner-c"), LeaseMode::Control).unwrap();
    let control_token = table.commit(control).unwrap();
    assert!(table.validate(&resource, &first, &first_owner, &observe_token, LeaseMode::Observe, "can.receive").is_ok());
    assert!(table.validate(&resource, &session("session-c"), &owner("owner-c"), &control_token, LeaseMode::Control, "can.send").is_ok());
    assert!(table.release(&resource, &session("session-b"), &second_token));
}

#[test]
fn compatibility_matrix_includes_provisional_leases() {
    let (resource, session_id, owner_id) = ids();
    let mut table = CanLeaseTable::default();
    let maintenance = table.reserve(resource.clone(), session_id.clone(), owner_id, LeaseMode::Maintenance).unwrap();
    assert!(table.reserve(resource.clone(), session("b"), owner("b"), LeaseMode::Observe).is_err());
    assert!(table.reserve(resource.clone(), session("c"), owner("c"), LeaseMode::Control).is_err());
    assert!(table.cancel(&maintenance));
    let observe = table.reserve(resource.clone(), session_id, owner("a"), LeaseMode::Observe).unwrap();
    assert!(table.reserve(resource.clone(), session("c"), owner("c"), LeaseMode::Maintenance).is_err());
    assert!(table.cancel(&observe));
}

#[test]
fn generations_are_monotonic_and_cancel_does_not_advance() {
    let (resource, session_id, owner_id) = ids();
    let mut table = CanLeaseTable::default();
    let pending = table.reserve(resource.clone(), session_id.clone(), owner_id.clone(), LeaseMode::Control).unwrap();
    assert!(table.cancel(&pending));
    let first = table.commit(table.reserve(resource.clone(), session_id.clone(), owner_id.clone(), LeaseMode::Control).unwrap()).unwrap();
    assert_eq!(first.generation(), 1);
    assert!(table.release(&resource, &session_id, &first));
    let second = table.commit(table.reserve(resource.clone(), session_id, owner_id, LeaseMode::Control).unwrap()).unwrap();
    assert_eq!(second.generation(), 2);
}

#[test]
fn old_observe_remains_valid_after_newer_compatible_open() {
    let (resource, session_id, owner_id) = ids();
    let mut table = CanLeaseTable::default();
    let old = table.commit(table.reserve(resource.clone(), session_id.clone(), owner_id.clone(), LeaseMode::Observe).unwrap()).unwrap();
    let newer = table.commit(table.reserve(resource.clone(), session("b"), owner("b"), LeaseMode::Control).unwrap()).unwrap();
    assert_eq!(newer.generation(), 2);
    assert!(table.validate(&resource, &session_id, &owner_id, &old, LeaseMode::Observe, "can.receive").is_ok());
}

#[test]
fn validation_checks_owner_token_and_operation_mode() {
    let (resource, session_id, owner_id) = ids();
    let mut table = CanLeaseTable::default();
    let token = table.commit(table.reserve(resource.clone(), session_id.clone(), owner_id.clone(), LeaseMode::Observe).unwrap()).unwrap();
    assert!(table.validate(&resource, &session_id, &owner("wrong"), &token, LeaseMode::Observe, "can.receive").is_err());
    assert!(table.validate(&resource, &session_id, &owner_id, &token, LeaseMode::Control, "can.send").is_err());
    assert!(table.validate(&resource, &session_id, &owner_id, &token, LeaseMode::Observe, "can.receive").is_ok());
}

#[test]
fn thousands_of_failed_reservations_leave_no_pending_entries() {
    let (resource, session_id, owner_id) = ids();
    let mut table = CanLeaseTable::default();
    for _ in 0..4096 {
        let reservation = table.reserve(resource.clone(), session_id.clone(), owner_id.clone(), LeaseMode::Control).unwrap();
        assert!(table.cancel(&reservation));
    }
    assert_eq!(table.pending_count(), 0);
    assert_eq!(table.retained_generation_count(), 0);
}
