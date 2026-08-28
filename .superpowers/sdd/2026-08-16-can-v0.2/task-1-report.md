# Task 1 report: stable CAN/CAN FD contract

## Status

Implemented the v0.2 public CAN/CAN FD contract in the core and new
`robot-hal-can` crate.

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

Changed: `Cargo.toml`, `crates/robot-hal-core/src/identity.rs`,
`crates/robot-hal-core/src/lease.rs`, and
`crates/robot-hal-core/tests/core_contract.rs`.

Created: `crates/robot-hal-can/Cargo.toml`, `src/lib.rs`, `src/frame.rs`,
`src/config.rs`, `src/filter.rs`, `src/adapter.rs`, and
`tests/can_contract.rs`.

## Concerns

- Downstream protocol/runtime exhaustive matches will need their planned v0.2
  updates before the workspace can compile with the new core enum variants.
- Full verification remains deferred by instruction.

## Fix round 1

- Added exact CAN FD payload-length validation for the permitted wire lengths;
  `frame_limits_and_flags_are_enforced` now covers every rejected gap.
- Changed local batch-admission error construction to always report
  `committed() == 0`; backend-prefix construction is explicit and separately
  covered by `batch_error_preserves_prefix_and_redacts_debug`.
- Added optional nonzero `restart_ms` to Configure, with accessor and rejection
  coverage in `configure_restart_ms_is_optional_and_nonzero`.

Tests for this fix round were written but NOT RUN due to the deferred
verification rule. Builds, lint, formatting, and protocol checks remain
deferred as well.
