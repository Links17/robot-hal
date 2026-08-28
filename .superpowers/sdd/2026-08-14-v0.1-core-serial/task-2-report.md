# Task 2 Report

Status: done.

RED:
`cargo test -p robot-hal-core --test core_contract`
Output: `error: package ID specification 'robot-hal-core' did not match any packages`
Why expected: the workspace initially had no `robot-hal-core` member.

GREEN:
`cargo test -p robot-hal-core --test core_contract`
`cargo test`
Output: 5 contract tests passed; workspace test run passed; doc-tests passed.

Files changed:
`Cargo.toml`, `Cargo.lock`, `crates/robot-hal-core/Cargo.toml`,
`crates/robot-hal-core/src/{lib.rs,capability.rs,error.rs,identity.rs,lease.rs}`,
`crates/robot-hal-core/tests/core_contract.rs`

Self-review:
core identifiers are validated; `ResourceDescriptor::selector()` preserves id, transport, and minimum identity quality; `HalError` serializes only decision fields.

Concerns:
No broader consumer integration was exercised; the core API is intentionally strict on identifier syntax.

## Fix round 1

Finding 1: derived deserialization bypassed validation for validated string newtypes.
Fix: replaced derived `Deserialize` with validating custom deserializers for `CapabilityId`, `ResourceId`, `Endpoint`, `ErrorName`, `OperationName`, `LeaseId`, `OwnerId`, and `SessionId`.

Finding 2: the public error helper could panic on caller input.
Fix: replaced the public panic path with crate-private `HalError::invalid_argument_error(...)`; public `HalError::new(...)` remains fallible.

RED:
`cargo test -p robot-hal-core --test core_contract`
Output: `malformed_serialized_values_are_rejected` failed because empty serialized strings still constructed `ResourceId`; `public_error_construction_returns_result_instead_of_panicking` initially used a valid ASCII operation string.

GREEN:
`cargo test -p robot-hal-core --test core_contract`
`cargo test`
Output: 7 contract tests passed; workspace test run passed; doc-tests passed.

Updated concerns:
No broader consumer integration was exercised; `HalError` remains output-only for serialization but round-trips on deserialization through validated fields.
