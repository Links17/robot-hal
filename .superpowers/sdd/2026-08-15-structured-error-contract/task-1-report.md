# Task 1 report: structured error details

## Implementation

- Added validated, bounded `ErrorContext` backed by `BTreeMap`, including key syntax, duplicate, entry-count, per-value, and aggregate-byte checks.
- Replaced the tuple `HalError` representation with named private fields for canonical resource identity, platform/vendor codes, and context.
- Added consuming enrichment methods and read-only detail accessors while preserving `HalError::new`, decision-only serde, and legacy deserialization behavior.
- Exported `ErrorContext` from `seeed-hal-core`.
- Added focused core contract tests for enrichment, all requested limits and one-byte-over failures, identifier validation, legacy defaults, and decision-only serde.

## Tests

Not run. Verification is explicitly deferred by the user until the unified verification task.

## Changed files

- `crates/seeed-hal-core/src/error.rs`
- `crates/seeed-hal-core/src/lib.rs`
- `crates/seeed-hal-core/tests/core_contract.rs`

## Static self-check

- Inspected the changed source and tests manually.
- Ran `git diff --check` only.
- Did not run tests, lint, build, cargo check, rustfmt, or formatting checks.

## Concerns

- Validation error names for context limit/key failures are newly defined under `error.context.*`; duplicate keys use the explicitly required `error.context.duplicate_key` name.
