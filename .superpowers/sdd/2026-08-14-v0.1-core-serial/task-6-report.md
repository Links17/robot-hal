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

## Fix Round 1

### Status and files

Resolved all critical and important round-1 findings in:

- `crates/seeed-hal-broker/src/connection.rs`
- `crates/seeed-hal-broker/tests/broker_contract.rs`
- `docs/architecture/hal-architecture.md`

The protobuf schema and its field numbers were unchanged.

### RED evidence

1. `cargo test -p seeed-hal-broker --test broker_contract
   stalled_writer_cannot_delay_owner_revoke_or_resource_reuse -- --nocapture`
   - Exit 101 after 1.02 seconds.
   - Failed with `stalled output must not block connection teardown: Elapsed(())`, reproducing the
     writer-before-revoke deadlock.
2. `cargo test -p seeed-hal-broker --test broker_contract
   handshake_version_capability_and_byte_limits_fail_closed -- --nocapture`
   - Exit 101.
   - A 128-byte frame with a 128-byte read payload was incorrectly accepted, so the test observed a
     handshake response where it required `runtime.protocol.invalid_message`.
3. `cargo test -p seeed-hal-broker --test broker_contract
   negotiated_frame_limit_rejects_oversized_raw_inbound_frame -- --nocapture`
   - Exit 101.
   - The oversized post-handshake frame was dispatched instead of causing broker-initiated EOF.
4. `cargo test -p seeed-hal-broker --test broker_contract
   negotiated_frame_limit_rejects_oversized_outbound_before_encoding -- --nocapture`
   - Exit 101.
   - The broker wrote an enumerate envelope larger than the negotiated limit.
5. `cargo test -p seeed-hal-broker --test broker_contract
   runtime_events_are_filtered_to_the_connection_owner -- --nocapture`
   - Exit 101.
   - A second authenticated connection received the first connection's session event.

The request-queue, event-lag, and strengthened duplicate-ID tests were coverage additions for
already-present observable behavior and began green; no artificial production change was made to
manufacture a red state for those characterization cases.

### GREEN behavior and named tests

- `stalled_writer_cannot_delay_owner_revoke_or_resource_reuse`: response backpressure cannot delay
  owner revocation or resource reuse, even while the peer remains connected and unread.
- `negotiated_frame_limit_rejects_oversized_raw_inbound_frame`: raw inbound frame length is retained
  through admission and enforced before dispatch, including pipelined post-handshake requests.
- `negotiated_frame_limit_rejects_oversized_outbound_before_encoding`: encoded length is checked
  against both negotiated and hard limits before `BytesMut` allocation.
- `handshake_version_capability_and_byte_limits_fail_closed`: negotiated read/write payloads include
  worst-case protobuf envelope and field overhead, and the handshake response itself must fit.
- `runtime_events_are_filtered_to_the_connection_owner`: session events are visible only when the
  event `OwnerId` matches the connection owner.
- `request_queue_overflow_is_deterministic_and_structured`,
  `task_queue_overflow_is_deterministic_and_structured`,
  `stalled_writer_cannot_delay_owner_revoke_or_resource_reuse`, and
  `runtime_event_queue_lag_is_reported_structurally`: all bounded queue results are explicit.
- `duplicate_in_flight_request_ids_fail_closed`: verifies structured rejection, broker-initiated
  EOF, owner cleanup, and immediate resource reuse without the client first disconnecting.

### Design decisions

- `revoke_owner` now runs immediately after dispatch stops and before any socket-task join. Reader
  cancellation follows cleanup; the writer gets a bounded 100 ms drain and is aborted/recorded as
  `runtime.protocol.task_shutdown_timeout` if it remains stalled.
- Each inbound queue item retains the raw frame length. Each outbound queue item carries the
  negotiated frame maximum. The writer calculates `encoded_len`, rejects anything over the
  negotiated or 1 MiB hard maximum, and only then allocates the encode buffer.
- Handshake negotiation accounts for protobuf tags, length varints, envelope/request correlation,
  the runtime's UUID session/lease identifiers, and the handshake response envelope.
- Runtime session events are explicitly classified as owner-scoped. The exhaustive
  `RuntimeEventKind` match makes any future genuinely global event require a deliberate visibility
  rule. Lag errors contain no session/resource/lease metadata.
- Dispatch uses Tokio's fair `select!`; the prior event-biased branch was removed so continuously
  ready events cannot indefinitely starve requests or cancellation.

### Verification evidence

- `cargo fmt --all --check`: exit 0, no output.
- `cargo test -p seeed-hal-protocol -p seeed-hal-broker`: exit 0; 17 broker tests and 2 protocol
  tests passed, plus doc tests.
- `cargo clippy --workspace --all-targets --all-features -- -D warnings`: exit 0, no warnings.
- `cargo test --workspace --all-features`: exit 0; all default tests and doc tests passed; the
  existing physical loopback test remained intentionally ignored without
  `SEEED_HAL_SERIAL_LOOPBACK`.
- `cargo check --workspace --all-targets --all-features --target x86_64-pc-windows-msvc`: exit 0;
  the installed Windows target compiled the workspace, including the Named Pipe module.

### Commit and remaining concerns

Fix commit subject: `fix(broker): bound teardown and negotiated frames`. The SHA is recorded in the
handoff because the commit cannot contain its own final SHA.

Windows target compilation is verified; Windows runtime Named Pipe acceptance was not executable on
the macOS host. Physical Serial hardware remains outside default tests.
