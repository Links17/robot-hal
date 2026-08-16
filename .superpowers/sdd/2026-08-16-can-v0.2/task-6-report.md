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

## Fix round 1/5

Review fixes applied on top of `592a18b`:

- Exhaustively owner-gated session and all six CAN bus-health runtime event kinds. Ordinary CAN
  frames and diagnostics remain correlated responses only.
- Added the canonical `MAX_CAN_ERROR_CLASSES = 10` model bound, preserving caller class order and
  duplicates, with exact-limit and one-over-limit CAN/protocol contract coverage.
- Replaced the approximate receive-response admission constant with schema-derived protobuf
  maxima: 82 bytes per canonical CAN frame, 271 per timestamp, 358 per received frame, and 376 or
  23,120 bytes for one- or 64-frame response envelopes.
- Made CAN dispatch fail closed from a connection-local registry containing resource identity,
  exact lease token, capabilities, and closed state. Send, receive, filter replacement, status,
  and close now reject unknown, foreign, stale, invalid-mode, and closed sessions before runtime
  dispatch as applicable.
- Preserved idempotent CAN close replay with bounded 256-entry retention and added direct replay
  coverage. Added a similarly bounded Serial session registry so the shared close envelope cannot
  reach the Serial runtime for a foreign or unknown session.
- Added broker coverage for response-frame admission without dequeue, configured-CAN restoration
  plus CAN/Serial resource reuse after disconnect, usable advertised error-frame/filter
  capabilities, cross-connection fail-closed dispatch, and owner-scoped CAN health events.

Static inspection confirmed the receive-bound arithmetic against
`proto/seeed/hal/v1/hal.proto`, including envelope field 57 and the canonical variant invariants.
Per the fix-round instruction, compile, test, lint, format, generated-protocol, and Python gates
remain deliberately deferred.
