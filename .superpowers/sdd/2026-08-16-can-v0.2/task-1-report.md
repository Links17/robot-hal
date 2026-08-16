# Task 1 report: stable CAN/CAN FD contract

## Status

Implemented the v0.2 public CAN/CAN FD contract in the core and new
`seeed-hal-can` crate.

## Implementation

- Added additive `TransportKind::Can` and `LeaseMode::Maintenance` variants,
  with core serialization/round-trip contract coverage and legacy-name checks.
- Added all requested CAN limits, capability constants/constructors, validated
  CAN identifiers, Classical/FD/remote/error frames, timestamp metadata,
  received-frame wrapper, and redacted typed batch-send errors.
- Added timing, attach/configure, filter/classification, bus status, active
  configuration, adapter, and channel seams with the requested invariants.
- Added focused public contract tests covering limits, validation, matching,
  capability spelling, timestamp domains, redaction, and core enum variants.

## Tests

NOT RUN (per task constraint). Test/build/lint/fmt commands are deferred to the
final implementation gate after the remaining v0.2 tasks are complete.

## Static self-check

- `git diff --check` passed.
- Reviewed changed files against the task brief and CAN v0.2 design spec.
- No generator, runtime, protocol, broker, client, or adapter files were
  touched.

## Files

Changed: `Cargo.toml`, `crates/seeed-hal-core/src/identity.rs`,
`crates/seeed-hal-core/src/lease.rs`, and
`crates/seeed-hal-core/tests/core_contract.rs`.

Created: `crates/seeed-hal-can/Cargo.toml`, `src/lib.rs`, `src/frame.rs`,
`src/config.rs`, `src/filter.rs`, `src/adapter.rs`, and
`tests/can_contract.rs`.

## Concerns

- Downstream protocol/runtime exhaustive matches will need their planned v0.2
  updates before the workspace can compile with the new core enum variants.
- Full verification remains deferred by instruction.
