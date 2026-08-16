# Task 2 implementation report

## Files changed

- `crates/seeed-hal-testkit/Cargo.toml`
- `crates/seeed-hal-testkit/src/lib.rs`
- `crates/seeed-hal-testkit/src/virtual_can.rs`
- `crates/seeed-hal-testkit/tests/can_conformance.rs`

## Behavior implemented

- Added a bounded deterministic Virtual CAN loopback adapter with a Strong CAN resource descriptor and all five CAN v0.2 capabilities.
- Added bounded RX/TX deques, FIFO transmission inspection, loopback receive delivery, deterministic timeout behavior, and status reporting.
- Implemented Attach expectation verification and exclusive Configure opens with snapshot restoration on close.
- Added one-shot send/receive/status/close failure injection, frame/timestamp injection, bus-status mutation, transmitted-frame inspection, and finite-time transition waits.
- Preserved canonical resource IDs on adapter/channel failures and documented queue overflow behavior in the implementation.

## Focused tests written

- Reusable public-interface `run_can_adapter_conformance` helper.
- Classic, FD, remote, and error frame ordering/preservation.
- Timestamp and status hooks.
- Configure exclusivity and restoration.
- Timeout and closed-channel behavior.
- Descriptor identity, transport, and capability honesty.
- One-shot fault hooks and bounded transition waits.

## Commands actually run

- `sed`/`rg` inspection commands to read the Task 2 brief, Task 1 interfaces, existing testkit patterns, and design documentation.
- `git diff --check` (passed).
- `git status --short` and `git diff --stat` for review.

## Commands intentionally not run

Per the deferred-verification requirement, no tests, builds/checks, Clippy, rustfmt, or protocol verification commands were run. Cargo.lock was not regenerated or modified.

## Self-review findings

- Scope is limited to the four Task 2 testkit files.
- No unsafe code, product/device-protocol concepts, or unbounded channels were introduced.
- Public APIs use only Seeed HAL-owned types.
- Queue overflow and finite-time wait behavior are deterministic.

## Concerns

- Verification is intentionally deferred; compile and test failures, if any, will be found by the owning integration task.

## Fix round 1

Addressed the critical/important review findings:

- Expanded `run_can_adapter_conformance` into a capability-gated reusable suite covering frame classes/order, effective configuration, timestamps, timeout/close, capability-gated behavior, and Configure restoration.
- Added saturating RX dropped-frame accounting and structured `can.receive.lagged` errors with bounded `dropped_count` context before retained frames.
- Corrected transition waits to use a call-entry baseline, with tests covering repeated waits and finite timeouts.
- Added focused fail-next send/receive/status/close tests asserting canonical resource IDs, plus overflow/lag coverage.

Fix-round tests were written but execution remains deferred by instruction. No tests, builds, lint, formatting, or protocol checks were run; only static inspection and `git diff --check` are permitted for this round.

## Fix round 2

- Strengthened the reusable helper with exact five-capability-set assertions for the virtual subject, mandatory timestamp presence when the timestamp capability is advertised, and an incompatible Attach negative path asserting `can.configuration.mismatch` plus the canonical resource ID.
- Changed Configure coverage to require an actually changed effective configuration, explicitly exercise FD mode when advertised, and verify restoration after close.
- Added focused helper assertions/tests for the amended behavior; execution remains intentionally deferred.

## Fix round 3

- Added `assert_timestamp_if_advertised` and reused it for both the initial receive sequence and the configured-FD receive path, preventing timestamp-capability regressions in later helper extensions.
- Added the corresponding focused helper assertion; execution remains intentionally deferred.
