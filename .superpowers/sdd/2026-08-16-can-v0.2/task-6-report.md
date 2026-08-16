# Task 6 report: broker CAN dispatch

## Files

- Added `crates/seeed-hal-broker/src/can_dispatch.rs`.
- Updated `crates/seeed-hal-broker/src/connection.rs` and `src/lib.rs`.
- Added the broker's `seeed-hal-can` dependency in `crates/seeed-hal-broker/Cargo.toml`.
- Extended `crates/seeed-hal-broker/tests/broker_contract.rs` with framed-I/O CAN coverage.

## Behavior

- Negotiated wire minor 0 rejects every CAN envelope as
  `runtime.protocol.capability_unsupported` before runtime dispatch. Minor 1 advertises the
  stable CAN hardware-class capabilities alongside Serial.
- Minor-1 dispatch covers CAN enumerate, open, bounded batch send, receive, filter replacement,
  status, and close. Existing response/event fail-closed handling remains in the connection.
- Open resolves the selected descriptor by stable identity and checks the exact advertised
  Classic/FD, Configure, and error-filter capabilities. Session capabilities are retained for
  send/receive/filter checks; error-frame and timestamp delivery are rejected when the resource
  does not advertise those capabilities.
- CAN send validates batch/data bounds before protocol conversion and maps backend partial
  progress to `CanSendResponse { committed_count, error }`. Receive validates negotiated
  read/frame bounds before calling runtime and preserves bounded runtime lag/error behavior.
- CAN sessions use the same owner and runtime cleanup path as Serial. Close replay remains
  routable as CAN within a bounded 256-entry closed-session retention window; disconnect still
  calls `HalRuntime::revoke_owner` once for both transport classes.
- Ordinary CAN frames and diagnostics remain correlated responses, never unsolicited runtime
  events.

## Tests added

- Minor-0 CAN rejection with Serial coexistence.
- Minor-1 capability negotiation, enumerate, open, close, Classic batch send/receive, filters,
  status, and unsolicited-event isolation.
- Nested partial send response and stale-generation fencing after resource reuse.
- FD Configure and FD frame dispatch.
- Negotiated receive bound rejection and disconnect cleanup with Serial/CAN coexistence.
- Selected-descriptor capability honesty for FD, Configure, error frames, timestamps, and error
  filters.
- One-shot CAN receive lag with newest-frame retention.

## Verification

Command run:

```text
git diff --check
```

Output: no output (success).

Per the task instruction, the following were deliberately deferred and not run: Cargo tests,
Cargo build/check, Clippy, rustfmt, protobuf generation/checks, and Python verification.

## Self-review and concerns

- No unsafe code, product/device-protocol concepts, raw native handles, or unbounded broker
  queues were introduced.
- Capability decisions use the selected descriptor/session capability set; they are not inferred
  from endpoint strings or normalized transport assumptions.
- The receive admission check uses a conservative pre-dispatch encoded-response bound so an
  impossible negotiated frame cannot dequeue runtime frames. Full compile/lint/test verification
  remains intentionally pending.
