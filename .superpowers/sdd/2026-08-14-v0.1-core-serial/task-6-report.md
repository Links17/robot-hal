# Task 6 Report: protobuf wire contract and local broker

## Status

Implemented the v0.1 protobuf contract and local IPC broker as a library-first adapter over
`HalRuntime`. The broker exposes only Serial operations, preserves runtime ownership/fencing/close
semantics, and has no physical-hardware dependency in its default tests.

## Files

- Added `proto/seeed/hal/v1/hal.proto` with the v1 envelope and typed Serial/domain messages.
- Added `crates/seeed-hal-protocol/` for generated protobuf types and validated domain conversion.
- Added `crates/seeed-hal-broker/` for handshake, framed connections, bounded dispatch, event
  forwarding, Unix sockets, Windows Named Pipes, and broker contract tests.
- Updated the workspace manifest and lockfile for the new crates and IPC dependencies.
- Added `ResourceProperties::iter` for lossless descriptor serialization.
- Added `SerialHandle::into_parts` as the runtime-owned broker handoff seam; broker operations then
  use only public session-ID and `LeaseToken` methods.
- Documented broker security, framing, queue, and overflow behavior in
  `docs/architecture/hal-architecture.md`.

## RED evidence

1. Command: `cargo test -p seeed-hal-protocol`
   - Exit 101.
   - Meaningful failure: `package ID specification 'seeed-hal-protocol' did not match any packages`.
2. After adding the protocol tracer bullet, command:
   `cargo test -p seeed-hal-broker --test broker_contract`
   - Exit 101.
   - Meaningful failure: unresolved public imports `seeed_hal_broker::Broker` and
     `seeed_hal_broker::StartupToken`.
3. During the listener slice, command: `cargo test -p seeed-hal-protocol -p seeed-hal-broker`
   - Exit 101 with 8 broker tests passing and the Unix permission test failing because the macOS
     Unix-socket path exceeded `SUN_LEN`.
   - The test was corrected to use an explicit short, caller-private `/tmp` directory; no broker
     behavior was weakened.

## GREEN evidence

1. Focused command: `cargo test -p seeed-hal-protocol -p seeed-hal-broker`
   - Exit 0.
   - 13 contract tests passed: 11 broker and 2 protocol; doc tests also passed.
2. Lint command: `cargo clippy --workspace --all-targets --all-features -- -D warnings`
   - Exit 0 with all workspace crates checked and no warnings.
3. Workspace command: `cargo test --workspace --all-features`
   - Exit 0.
   - All default unit, integration, and doc tests passed; the existing physical Serial loopback test
     remained ignored because `SEEED_HAL_SERIAL_LOOPBACK` was not supplied.
4. Formatting is verified with `cargo fmt --all --check` at handoff.

## Design notes

- The envelope field numbers are the exact values from the task brief. Protobuf additions remain
  additive and no field number is reused. The frame codec maximum is exactly 1,048,576 bytes.
- Each broker launch can generate a 32-byte startup token. The token type intentionally has no
  `Debug` implementation, comparisons use `subtle::ConstantTimeEq`, and errors/logs never include
  token bytes.
- Handshake authentication, protocol version, required capability, and negotiated frame/read/write
  limits are checked before enumeration or resource metadata is exposed. Invalid handshakes and
  duplicate in-flight request IDs terminate the connection after the structured error is admitted.
- Each connection has a validated `OwnerId`. Open uses `HalRuntime::open_serial`; the resulting
  handle is transferred into session-ID and lease-token credentials without triggering RAII close.
  Read, write, flush, control-line, and close requests call the matching public runtime methods, so
  lease generation fencing and the runtime's two-second close deadline/replay window stay
  authoritative.
- Teardown aborts connection tasks, then awaits `HalRuntime::revoke_owner`. Cleanup failures are
  preserved separately from connection failures in `ConnectionOutcome`.
- Request (32), executing-task (32), response (64), and runtime-event (64) queues are bounded.
  Request/task overflow is `runtime.queue.full`; response overflow closes the connection and is
  recorded; runtime event lag is surfaced as `runtime.event.lagged`.
- Unix binding creates/normalizes a caller-provided directory to `0700` and the unique socket to
  `0600`. The Windows cfg module creates a unique per-launch Named Pipe with
  `reject_remote_clients(true)`. Neither platform module uses unsafe code.

## Tests covered

- Stable handshake envelope field numbers and exact 1 MiB frame constant.
- Required enum/semantic-field conversion to `runtime.protocol.invalid_message`.
- Operation rejection before handshake; invalid-token, version, capability, and byte-limit
  handshake rejection.
- Virtual Serial enumerate, open, write, flush, control lines, read, and close.
- Unsolicited ordered runtime event envelopes.
- Connection-loss owner revocation, session-close event, and subsequent resource reuse.
- Deterministic task-queue overflow and duplicate in-flight request-ID rejection.
- Structured error conversion for malformed requests.
- Canonical descriptor-to-selector conversion.
- Unix private-directory and socket permissions.

## Commit

Planned/final subject: `feat(broker): expose HAL runtime over local IPC`.
The resulting SHA is recorded in the task handoff because a commit cannot contain its own final SHA.

## Concerns

- The Windows Named Pipe implementation was compiled only through its `cfg(windows)` source design
  on this macOS host; no Windows target/runtime acceptance test was available in this worktree.
- Default tests do not exercise physical hardware by design.
