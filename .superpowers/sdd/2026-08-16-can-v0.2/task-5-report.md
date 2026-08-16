# Task 5 report: CAN wire minor one

## Status

Implemented the additive Seeed HAL protocol-major-1, wire-minor-1 CAN contract,
generated the Python protobuf binding, and added the fail-closed Rust conversion
surface and protocol contract tests required by the brief.

## Files

- Modified `proto/seeed/hal/v1/hal.proto`.
- Modified `crates/seeed-hal-protocol/Cargo.toml`.
- Modified `crates/seeed-hal-protocol/src/lib.rs`.
- Modified `crates/seeed-hal-protocol/src/conversion.rs`.
- Created `crates/seeed-hal-protocol/src/can_conversion.rs`.
- Modified `crates/seeed-hal-protocol/tests/protocol_contract.rs`.
- Regenerated `bindings/python/seeed_hal/proto/hal_pb2.py`.
- Created this report.

## Wire contract

- Kept protocol major 1 and supported minimum minor 0; advanced the preferred
  and supported maximum minor to 1 without changing legacy exact-minor
  negotiation.
- Appended `TRANSPORT_KIND_CAN = 2` and `LEASE_MODE_MAINTENANCE = 3`.
- Added CAN envelope payloads only at 50 through 61, while retaining generic
  close at 32/33 and every existing field number.
- Added CAN IDs, all frame kinds and error classes, receive timestamps,
  bit-timing, Attach/Configure oneof configuration, effective configuration,
  filters, status, and all Task 4 request/response shapes.
- Used proto3 presence for every optional expectation, optional timing value,
  optional restart, and optional status counter; zero is never used as an
  absence sentinel.
- Added Task 4 CAN bus-health runtime event values additively after the legacy
  event values.

## Conversion and validation

- Added symmetric conversions for every Task 1 CAN value type using only core
  Seeed HAL types and generated wire types.
- Centralized operation decoding for enumeration, open, send, receive, filter
  replacement, and bus status.
- Rejects unknown/unspecified required enums, invalid/missing nested values,
  non-CAN resources in CAN operations, mismatched open-response lease modes,
  out-of-range IDs, invalid frame field combinations and lengths, invalid
  timestamps/domains, invalid timing/configuration, invalid filters, batches
  outside 1..=64, receive limits outside 1..=64, and oversized receive
  responses.
- Send success requires `committed_count == input_count`; an error requires a
  strict committed prefix. Nested wire errors continue through the existing
  structured-error decoder.
- Every malformed peer value is remapped to
  `runtime.protocol.invalid_message` with diagnostics containing field names
  but no peer-supplied values. No input is truncated or normalized.

## Tests written

- Locks every legacy and CAN envelope tag, every populated legacy and CAN
  nested message field number, the supported minor range, and additive enum
  numeric values.
- Covers legacy exact-minor 0 negotiation and highest-shared wire 1 selection.
- Covers round trips for all CAN frame variants, timestamp presence, Attach and
  Configure values (including explicit `Some(false)`), active configuration,
  filters, status, CAN resource selectors, and Maintenance leases.
- Covers unknown additive field tolerance and malformed enum values.
- Covers standard/extended ID limits, Classical data/DLC limits, every invalid
  CAN FD payload-length gap, timestamp/domain limits, timing bounds,
  configuration consistency, filter widths/classes/count, batch count,
  receive count/response limits, and required nested values.
- Covers successful sends, valid partial backend errors, inconsistent success
  counts, non-prefix error counts, and malformed nested structured errors.

## Generator

The only implementation command run was:

```text
./scripts/generate-protocol.sh
```

The first successful run exited 0 and reported:

```text
Using CPython 3.13.2 interpreter at: /Users/links/miniconda/bin/python3
Creating virtual environment at: bindings/python/.venv
   Building seeed-hal @ file:///Users/links/Documents/Project/Seeed/robot/links/seeed-robotic/seeed-hal/.worktrees/v0.2-can/bindings/python
      Built seeed-hal @ file:///Users/links/Documents/Project/Seeed/robot/links/seeed-robotic/seeed-hal/.worktrees/v0.2-can/bindings/python
Installed 12 packages in 51ms
```

After the final additive event enum update, the same generator was run again;
it exited 0 with no output. The generated `hal_pb2.py` is the only tracked
generated file changed. The generator created an ignored local `.venv`; it did
not change `Cargo.lock` or any other tracked file.

## Deferred verification

Per the task constraint, none of these commands were run:

```text
cargo fmt --all --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace --all-features
uv run pytest bindings/python/tests
./scripts/check-generated.sh
```

No build, check, lint, test, generated-code check, or formatting command was
run. Static inspection and `git diff --check` were used; `git diff --check`
completed successfully.

## Self-review

- Compared the schema line by line with the brief: old tags remain unchanged,
  new envelope tags are exactly 50..61, and Close remains 32/33.
- Reviewed every conversion pair for symmetry, including explicit optional
  presence and all enum variants.
- Reviewed peer decoding paths to ensure constructors' domain errors are not
  leaked and every malformed value becomes the stable protocol error.
- Reviewed send response handling against the originating input length and
  receive response handling against the requested maximum.
- Confirmed changes introduce no unsafe code, product/device-protocol concepts,
  third-party CAN types, unbounded queues, or mutable global state.
- Confirmed the tracked diff is limited to the task files plus this required
  report.

## Concerns

- Compilation, Clippy, formatting, and test execution remain intentionally
  unverified until the owning integration gate runs the deferred commands.
- `Cargo.lock` was not changed because the permitted generator did not update
  it and the brief excludes it; the later Cargo verification gate may add the
  already-present `seeed-hal-can` package to the protocol package's dependency
  list in the lockfile.

## Fix round 1

- Added explicit Serial operation decoders and migrated the broker/client paths:
  Serial enumeration rejects CAN descriptors, Serial open rejects CAN
  selectors, Serial request leases require Control, and Serial open responses
  reject Observe/Maintenance or otherwise incompatible leases.
- Kept generic selector/descriptor/lease conversions capable of both Serial
  and CAN for shared Close/CAN contexts.
- Removed the empty-capability CAN fallback. Empty capabilities remain the
  legacy Serial-only fallback; empty CAN descriptors now fail with
  `runtime.protocol.invalid_message`.
- Added regression tests for cross-transport selectors/descriptors,
  Maintenance Serial responses, empty CAN capabilities, all enum numeric
  values (including RuntimeEventKind 4..7 and CanErrorClass 2..9), and both
  Attach=1 and Configure=2 oneof tags.
- Fix-round changes additionally touch
  `crates/seeed-hal-client/src/serial.rs`,
  `crates/seeed-hal-client/src/connection.rs`, and
  `crates/seeed-hal-broker/src/connection.rs` to migrate the existing Serial
  call sites. No protobuf generator was needed because the schema is unchanged.
- Tests, builds, lint, formatting, generated-code checks, and Python tests
  remain intentionally unexecuted; only static inspection and
  `git diff --check` are permitted.
