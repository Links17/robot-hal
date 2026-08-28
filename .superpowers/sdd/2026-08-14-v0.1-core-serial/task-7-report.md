# Task 7 Report: Rust broker client and executable

## Status

Implemented the v0.1 Rust broker client and the `robot-hal-broker` executable without adding product or device-protocol concepts. The implementation is Serial-only, library-first, Rust 2024/MSRV 1.85, and forbids unsafe code in the client and executable.

## Files

- Modified `Cargo.toml` to add the client and executable workspace members.
- Modified `Cargo.lock` for the existing workspace dependencies newly activated by the executable.
- Added `crates/robot-hal-client/Cargo.toml`.
- Added `crates/robot-hal-client/src/lib.rs`.
- Added `crates/robot-hal-client/src/connection.rs`.
- Added `crates/robot-hal-client/src/serial.rs`.
- Added `crates/robot-hal-client/tests/client_contract.rs`.
- Added `apps/robot-hal-broker/Cargo.toml`.
- Added `apps/robot-hal-broker/build.rs` to capture Cargo's actual target triple.
- Added `apps/robot-hal-broker/src/main.rs`.
- Added `apps/robot-hal-broker/src/manifest.rs`.
- Added `apps/robot-hal-broker/tests/manifest.rs`.
- Added this report.

## RED evidence

1. Initial client contract:

   ```text
   cargo test -p robot-hal-client --test client_contract
   error: package ID specification `robot-hal-client` did not match any packages
   ```

2. Cancellation/backpressure contract after the initial client implementation:

   ```text
   cargo test -p robot-hal-client --test client_contract
   cancelling_a_caller_releases_pending_capacity_and_discards_its_response ... FAILED
   runtime.queue.full
   ```

   The test was made deterministic by awaiting task cancellation, the point at which the request future's drop guard must release pending capacity.

3. Initial executable manifest contract:

   ```text
   cargo test -p robot-hal-broker-app --test manifest
   error: package ID specification `robot-hal-broker-app` did not match any packages
   ```

## GREEN evidence

- `cargo test -p robot-hal-client --test client_contract`: 10 passed.
- `cargo test -p robot-hal-broker-app`: 3 passed.
- `cargo fmt --all --check`: passed.
- `cargo clippy --workspace --all-targets --all-features -- -D warnings`: passed.
- `cargo test --workspace --all-features`: passed; the pre-existing physical loopback test remained explicitly ignored.
- `RUSTFLAGS='-D warnings' cargo check --workspace --all-targets --all-features --target x86_64-pc-windows-msvc`: passed.

## Design

### Client connection and correlation

- `HalClient` connects through Unix domain sockets or Windows Named Pipes and performs the existing protobuf handshake before becoming usable.
- A single bounded writer task serializes outbound envelopes. A single reader task decodes frames, dispatches ordered events, and correlates responses by unique nonzero request ID.
- Pending requests, cancelled-response tombstones, completed-response tombstones, writer admission, and event delivery are all bounded.
- Request IDs advance monotonically, never use zero, and fail closed after `u64` exhaustion rather than wrapping into reuse.
- Cancellation removes the pending sender and records a bounded tombstone so the eventual response is discarded without becoming an unknown-response protocol failure.
- Disconnect, writer loss, malformed frames, malformed error/event metadata, unknown responses, duplicate responses, response-kind mismatches, pending overflow, cancellation-tombstone overflow, and explicit close use stable structured `HalError` names and resolve all pending senders deterministically.

### Limits and teardown

- The client codec applies the exact 1 MiB hard frame cap.
- Negotiated frame/read/write limits are validated during handshake and enforced before outbound encoding or data copying and before inbound protobuf decode.
- Client task shutdown has a 100 ms deadline and abort fallback; no library creates a runtime, tracing subscriber, or signal handler.
- `RemoteSerialHandle::close(self)` sends one authenticated broker close. The broker's retained session/lease replay makes the operation idempotent within its documented window.
- Dropping `RemoteSerialHandle` deliberately does not spawn a task or create a runtime. The owning broker connection remains authoritative and revokes the session when the client connection closes.

### Executable

- CLI supports exactly `--endpoint`, `--auth-token-file`, `--manifest`, and `--log-format <json|pretty>` in addition to Clap's standard help/version flags.
- Manifest mode is evaluated before tracing, token-file access, adapter construction, or endpoint binding.
- Normal startup asynchronously reads exactly 32 token bytes, rejects trailing bytes, and removes only the explicitly supplied token path after a successful exact read.
- The real `serialport` adapter is registered with `HalRuntime`; registration itself does not enumerate or open hardware.
- Unix startup requires a caller-private parent directory and creates a `0600` socket. Windows uses a local Named Pipe with remote clients rejected.
- Active executable connections are bounded at 64. Shutdown stops admission, aborts the bounded task set, and removes the Unix socket; process exit provides the final OS-handle cleanup boundary.
- Readiness is one JSON line containing only status and endpoint. Token bytes are neither printable nor included in logs, `Debug`, manifest, or readiness output.
- The deterministic manifest contains broker SemVer, wire major/minor range, Cargo's actual target triple, and the enabled `serialport` adapter.

## Tests

Client integration tests use only the virtual Serial adapter or controlled local fake brokers and cover:

- full enumerate/open/write/read/close round trip;
- reversed response correlation;
- ordered events interleaved with a pending response;
- structured disconnect resolution;
- pending-capacity backpressure;
- caller cancellation and late-response discard;
- unknown and duplicate response IDs;
- mismatched response payloads;
- malformed protobuf input.

Executable tests use temporary token paths and cover:

- exact 32-byte read followed by removal of only that file;
- oversized token rejection without deletion;
- deterministic hardware-free manifest behavior even when supplied an invalid token file and unusable endpoint;
- absence of the responsibility contract's forbidden business and vendor protocol terms from manifest output.

## Target coverage

- Native macOS: full format, Clippy, client/executable focused tests, and workspace tests.
- Windows MSVC (`x86_64-pc-windows-msvc`): full workspace compile check with warnings denied.
- Physical hardware: intentionally not accessed by default tests.

## Concerns

- Windows was compile-checked but not runtime-tested in this macOS environment.
- Normal executable startup with a physical Serial device was intentionally not exercised; default verification is hardware-free and the existing hardware-loopback test remains opt-in.

## Fix Round 1

### Status

Addressed all Round 1 findings while preserving the public Rust API, CLI flags, protobuf field
numbers, exact 1 MiB hard cap, Serial-only responsibility seam, Rust 2024/MSRV 1.85, and
hardware-free default verification.

### Root causes and changes

- Replaced executable `JoinSet::abort_all()` shutdown with cooperative per-connection shutdown.
  Every broker connection now drops dispatch work, calls `revoke_owner`, and performs bounded
  reader/writer teardown before its task returns. Completed connection tasks are reaped eagerly and
  retained task entries never exceed the configured connection bound.
- Moved the client handshake ahead of reader/writer task startup. The initial codec uses the
  client-offered limit, then tightens to the negotiated limit before any task can decode a
  post-handshake frame. Request ID 1 remains the handshake ID and normal allocation starts at 2.
- Added a zero-copy protobuf wire preflight before Prost decode. It identifies the response request
  ID and payload, and rejects oversized `SerialReadResponse.data` against both the negotiated read
  limit and the originating request's `max_bytes` before allocating that bytes field.
- Made request-ID exhaustion fail closed after using `u64::MAX` once, and expanded coverage for
  concurrency, writer/tombstone/event backpressure, disconnect/close fan-out, and negotiated write
  rejection before transmission.
- Expanded `--manifest` to every field required by `docs/contracts/versioning.md`: broker version,
  wire major/minor range, target triple/OS/architecture, enabled adapters/features, MSRV, current
  executable SHA-256, and required vendor runtime libraries. Checksum calculation streams the
  current executable on a blocking worker before token, endpoint, or hardware initialization.
- Hardened token-file consumption. Unix uses `O_NOFOLLOW | O_CLOEXEC`, exact parent/file mode and
  ownership/link checks, retained descriptors, and pathname identity revalidation. Windows uses
  non-reparse exclusive parent/file handles, current-process-user ownership, private DACL
  validation through safe `windows-permissions` APIs, and handle/path/security revalidation before
  deletion. A failed trust or exact-length check does not delete the supplied path.
- Added zeroize-on-drop secret wrappers for broker/client startup tokens and zeroized owned raw,
  protobuf, and temporary encoded handshake buffers at the feasible userspace boundary. Kernel,
  socket, and third-party codec copies cannot be guaranteed zeroized.
- Corrected the mismatched-response regression assertion to compare both calls against the literal
  `runtime.protocol.unexpected_response` contract.

### RED evidence

- Manifest integration test: `target.triple` was absent (`null`) under the original four-field
  manifest.
- Unix token trust tests: symlink, group/other-readable file, and public parent inputs succeeded
  under the original length-only implementation.
- Zeroization tests: `StartupToken` did not implement `Zeroize`, and the client owned no private
  zeroizing secret wrapper.
- Negotiated decoder test: a pipelined 300-byte frame prefix after a 256-byte negotiation waited for
  the missing body instead of failing immediately.
- Read preflight test: a 16-byte response to an 8-byte read reached Prost decode and later failed as
  `runtime.argument.invalid` rather than terminating the connection before allocation.
- Unknown-field preflight test: appending an unknown length-delimited envelope field after that
  oversized read response overwrote the scanner's payload candidate and again produced
  `runtime.argument.invalid`; the scanner now validates every field 25 occurrence independently.
- Shutdown-error cleanup test: returning an error from the executable shutdown future caused an
  immediate loop return, dropped the connection task, and left resource reuse failing with
  `runtime.lease.conflict`; loop errors are now returned only after cooperative cleanup and join.
- Request allocator test: the allocator helper was absent; the completed implementation uses
  `u64::MAX` once and then returns `runtime.protocol.request_id_exhausted`.
- Windows ACL compile-gated tests initially failed solely because
  `validate_private_security_descriptor` did not exist. The tests independently construct SDDL for
  wrong owner, missing DACL, broad-principal access, and the allowed user/System/Administrators set.

### GREEN evidence

- `cargo test -p robot-hal-client`: 21 passed, including 19 client contract tests.
- `cargo test -p robot-hal-broker`: 24 passed, including 23 broker contract tests.
- `cargo test -p robot-hal-broker-app`: 10 passed, including manifest, token trust, cooperative
  shutdown/resource reuse, and connection-retention-bound coverage.
- `cargo fmt --all --check`: passed.
- `cargo clippy --workspace --all-targets --all-features -- -D warnings`: passed.
- `cargo test --workspace --all-features`: 119 passed; the physical Serial loopback remained the
  single explicitly ignored test.
- `RUSTFLAGS='-D warnings' cargo check --workspace --all-targets --all-features --target
  x86_64-pc-windows-msvc`: passed, including the Windows-only ACL implementation and tests.

### Files

- Workspace/dependencies: `Cargo.toml`, `Cargo.lock`, broker/client/app Cargo manifests.
- Broker library: `crates/robot-hal-broker/src/lib.rs`,
  `crates/robot-hal-broker/src/connection.rs`.
- Rust client: `crates/robot-hal-client/src/connection.rs`,
  `crates/robot-hal-client/src/serial.rs`, `crates/robot-hal-client/tests/client_contract.rs`.
- Executable: `apps/robot-hal-broker/src/main.rs`, `apps/robot-hal-broker/src/manifest.rs`,
  `apps/robot-hal-broker/src/token.rs`, `apps/robot-hal-broker/tests/manifest.rs`.
- Architecture: `docs/architecture/hal-architecture.md`.

### Target coverage and residual concerns

- Native macOS ran focused and full default suites without physical hardware.
- Windows MSVC compiled the entire workspace, all targets/features, and Windows-only token ACL tests
  with warnings denied; those tests were not runtime-executed on Windows in this environment.
- Physical Serial and normal broker startup against a real device remain intentionally untested;
  the opt-in loopback test was not enabled.

## Fix Round 2

### Status

Addressed all Round 2 findings without changing public APIs, CLI flags, protobuf field numbers,
the exact 1 MiB hard cap, negotiated limits, the Serial-only responsibility seam, Rust 2024, or
MSRV 1.85. Default verification remains hardware-free.

### Root causes and changes

- Cooperative broker shutdown now signals connection cancellation and awaits the dispatch future's
  explicit `JoinSet::abort_all()` plus join drain before owner revocation. Reader/writer shutdown is
  still bounded after owner cleanup. A two-worker gated adapter proves that a running operation task
  terminates before session close/event publication/resource reuse and before the connection task
  returns.
- Cancelled client requests now retain their bounded `ExpectedResponse` metadata. Inbound preflight
  uses either pending or cancelled metadata, so a late response to cancelled `read(8)` cannot allocate
  or discard 12 bytes under a broader negotiated 16-byte limit; it terminates the connection with
  `runtime.protocol.frame_too_large`.
- The zero-copy protobuf scanner now skips unknown start/end groups with matching field numbers,
  bounds nesting at 64, keeps grouped fields out of the enclosing visitor, and rejects unexpected,
  mismatched, unterminated, overly deep, malformed-length, and malformed-varint input. Every
  top-level field 25 and nested bytes field 1 remains independently checked.
- Unix token consumption now resolves a bare token parent as `.`, requires parent and file UID to
  equal the effective broker UID, holds a no-follow parent directory descriptor, opens/revalidates
  the token with `openat`/`fstat`/`statat`, requires one hard link, and deletes with descriptor-relative
  `unlinkat`. Replacement, hard-link, bare-relative, and privileged wrong-owner cases are covered.
- Windows token identity now uses the safe `winapi-util` handle wrapper because Rust 1.85 keeps the
  equivalent `MetadataExt` methods unstable. Volume serial plus file index identify both parent and
  token, exactly one hard link is required, and a fresh path handle is compared with the retained
  handle before deletion. Current-user owner/private-DACL checks remain in force.
- Decoded broker envelopes now enter a private RAII `SensitiveEnvelope` immediately after Prost
  decode. Its `Drop` zeroizes handshake-token bytes on request-ID-zero, duplicate, queue-full/closed,
  second-handshake, panic, and other early-return paths. Handshake validation borrows the request and
  moves the token into `Zeroizing`; only proven non-handshake payloads are moved into operation tasks.
- Replaced writer-overflow, cancellation-tombstone-overflow, and client-close correctness sleeps with
  explicit server-received/release channels. Executable endpoint readiness loops have a one-second
  deadline, and `RemoteSerialHandle` is `#[must_use]`.
- Security documentation now limits the zeroization guarantee strictly to buffers explicitly owned
  by this code and describes the actual Unix descriptor-relative and Windows handle-identity checks.

### RED evidence

- Broker shutdown ordering test failed with `owner revoke must wait for the in-flight operation task
  to join`; the original `drop(dispatch)` let owner close begin while a gated operation was still
  running on another Tokio worker.
- Cancelled-read integration test failed because the follow-up enumerate returned `Ok(...)`; the
  original tombstone retained only the request ID and preflight therefore lost the original 8-byte
  limit.
- Bare-relative Unix token test failed with `No such file or directory`; the original empty parent
  path was passed to metadata lookup instead of resolving to `.`.
- The decoded-handshake RAII test initially failed to compile because `SensitiveEnvelope` did not
  exist. Its controlled Drop observer now sees 32 zero bytes for representative early-drop IDs.
- The first Windows strong-identity cross-check failed because Rust 1.85 reports
  `MetadataExt::{volume_serial_number,file_index,number_of_links}` as unstable. The implementation
  moved to the safe, MSRV-compatible `winapi-util` wrapper and the warnings-denied check passed.
- Native Clippy initially rejected an unnecessary `st_ino` cast; the portable rustix stat conversion
  was corrected before final verification.

### GREEN evidence

- `cargo test -p robot-hal-client`: 24 passed, including 20 client contract tests.
- `cargo test -p robot-hal-broker`: 26 passed, including 24 broker contract tests.
- `cargo test -p robot-hal-broker-app`: 13 passed, including Unix token trust, cooperative shutdown,
  connection retention, and manifest coverage.
- `cargo fmt --all --check`: passed.
- `cargo clippy --workspace --all-targets --all-features -- -D warnings`: passed.
- `cargo test --workspace --all-features`: 127 passed; the physical Serial loopback remained the
  single explicitly ignored test.
- `RUSTFLAGS='-D warnings' cargo check --workspace --all-targets --all-features --target
  x86_64-pc-windows-msvc`: passed, including Windows-only strong identity/link and ACL tests.
- `git diff --check`: passed.

### Files

- Workspace/dependencies: `Cargo.toml`, `Cargo.lock`, broker app and broker crate Cargo manifests.
- Broker lifecycle/security: `crates/robot-hal-broker/src/connection.rs`,
  `crates/robot-hal-broker/tests/broker_contract.rs`.
- Client cancellation/scanner/tests: `crates/robot-hal-client/src/connection.rs`,
  `crates/robot-hal-client/src/serial.rs`, `crates/robot-hal-client/tests/client_contract.rs`.
- Executable token/readiness: `apps/robot-hal-broker/src/token.rs`,
  `apps/robot-hal-broker/src/main.rs`.
- Architecture: `docs/architecture/hal-architecture.md`.

### Target coverage and residual concerns

- Native macOS ran all focused suites plus format, Clippy, and the complete workspace suite without
  physical hardware.
- Windows MSVC compiled the entire workspace, all targets/features, and Windows-only token identity,
  hard-link, and ACL tests with warnings denied; those tests were not runtime-executed on Windows in
  this environment.
- The effective-UID mismatch integration runs only when the host test process has privilege to create
  a wrong-owner file; other Unix trust-boundary cases run unconditionally.
- Physical Serial and normal broker startup against a real device remain intentionally untested; the
  opt-in loopback test was not enabled.

## Fix Round 3

### Status

Addressed all Round 3 findings without changing public Rust APIs, CLI flags, protobuf field numbers,
the exact 1 MiB hard cap, negotiated byte limits, the Serial-only responsibility seam, Rust 2024, or
MSRV 1.85. Default verification remains deterministic and hardware-free.

### Root causes and changes

- Cancellation-tombstone overflow removed the overflowing request's `ExpectedResponse`, released the
  request-state lock, and only then terminated the connection. A ready reader could pass preflight or
  enter Prost decode after that transition without the request-specific read limit. Terminal entry is
  now one locked transition that retains all pending and overflowing response metadata, drains reply
  senders exactly once, and only performs wakeup/shutdown work after unlocking. The reader gives a
  ready shutdown signal priority and rechecks terminal state after frame acquisition, after preflight,
  under the decode-admission lock, and before correlation.
- Windows token cleanup validated one handle but deleted by pathname. The executable now opens the
  primary validated file with delete access, keeps delete sharing denied throughout validation, and
  only after every type, ACL, ownership, link-count, and identity check calls a narrow safe adapter
  that marks that same handle for deletion with `SetFileInformationByHandle(FileDispositionInfo)`.
  The unsafe call is isolated to the platform adapter with its synchronous pointer/handle invariant;
  the broker app remains unsafe-free.
- The writer-overflow contract test depended on scheduler timing and Unix socket capacity. A private
  gated transport now positively reports that the real client writer task is blocked, the test
  positively observes the second request occupying the single-slot writer queue, and the next real
  request deterministically returns `runtime.queue.full` for `runtime.protocol.write`.
- The broker shutdown-order test used 100 ms of silence as evidence that owner revocation had not
  started. It now positively observes the shutdown signal, releases the gated operation, observes its
  Drop/join signal, then observes session close and connection completion. The existing ordering flag
  still proves close began only after the operation future dropped.
- Fake-broker reads, gate notifications, channel receives, and runtime-event receives now have
  one-second failure caps while correctness comes from positive state signals. The negotiated-write
  test uses a subsequent valid request as a wire sentinel instead of treating a negative timeout as
  proof. The bare-relative Unix token test serializes cwd mutation and restores the original cwd with
  a panic-safe Drop guard.

### RED evidence

- Terminal-metadata test initially failed to compile because `begin_termination` did not exist. The
  completed transition retains the cancelled, overflowing, and other pending `ExpectedResponse`
  entries and resolves reply senders once.
- Exact inbound-boundary race coverage initially failed to compile because the frame-acquired and
  preflight-complete gates did not exist. The completed test drives both boundaries 32 times each,
  triggers tombstone overflow, resolves every pending call as `runtime.queue.cancelled_full`, and
  observes zero Prost decode calls.
- Deterministic writer-overflow coverage initially failed to compile because `WriterTestGate` did not
  exist. The completed gated transport drives the real writer task and bounded request admission.
- Windows warnings-denied adapter checking initially failed because `mark_delete_on_close` did not
  exist. The completed adapter and broker app compile for `x86_64-pc-windows-msvc` with warnings
  denied.

### GREEN evidence

- `cargo test -p robot-hal-client`: 26 passed, including 7 unit and 19 client contract tests.
- `cargo test -p robot-hal-broker`: 26 passed, including 24 broker contract tests.
- `cargo test -p robot-hal-broker-app`: 13 passed, including token, shutdown, retention, and manifest
  coverage.
- `cargo test -p robot-hal-windows-file`: passed on the native host; its Windows-only test was
  compile-checked by the cross-target gate.
- `cargo fmt --all --check`: passed.
- `cargo clippy --workspace --all-targets --all-features -- -D warnings`: passed.
- `cargo test --workspace --all-features`: 129 passed; the physical Serial loopback remained the
  single explicitly ignored test.
- `RUSTFLAGS='-D warnings' cargo check --workspace --all-targets --all-features --target
  x86_64-pc-windows-msvc`: passed, including the Windows-only adapter and broker integration.
- `git diff --check`: passed.

### Files

- Workspace/dependencies: `Cargo.toml`, `Cargo.lock`, broker app Cargo manifest.
- Windows handle adapter: `adapters/windows-file/Cargo.toml`,
  `adapters/windows-file/src/lib.rs`.
- Client terminal/read/write tests: `crates/robot-hal-client/src/connection.rs`,
  `crates/robot-hal-client/tests/client_contract.rs`.
- Broker lifecycle tests: `crates/robot-hal-broker/tests/broker_contract.rs`.
- Executable token and shutdown tests: `apps/robot-hal-broker/src/token.rs`,
  `apps/robot-hal-broker/src/main.rs`.
- Architecture: `docs/architecture/hal-architecture.md`.

### Target coverage and residual concerns

- Native macOS ran all focused suites plus formatting, warnings-denied Clippy, and the complete
  workspace suite without physical hardware.
- Windows MSVC compiled the entire workspace, all targets/features, and the Windows-only
  handle-bound deletion test with warnings denied; that test was not runtime-executed on Windows in
  this macOS environment.
- Physical Serial and normal broker startup against a real device remain intentionally untested; the
  opt-in loopback test was not enabled.

## Fix Round 4

### Status

Corrected the final Windows token-handle sharing conflict without changing public APIs, CLI flags,
the token trust policy, or handle-bound deletion. The broker app and client remain unsafe-free; the
existing adapter-local `SetFileInformationByHandle` wrapper remains the only narrow unsafe boundary.

### Root cause and changes

- Windows share checks are bidirectional. The primary validated token handle requests `DELETE`
  access while sharing reads only. The later read-only identity re-open also shared reads only, so
  Windows rejected that second open because its share mode did not permit the primary handle's
  existing `DELETE` access.
- The identity re-open now shares `FILE_SHARE_READ | FILE_SHARE_DELETE`. The primary handle still
  shares only `FILE_SHARE_READ`, so a new pathname replacement or deletion open that requests
  `DELETE` remains denied throughout validation. Identity is still checked through the second handle,
  and deletion is still armed on the original validated handle before that handle is closed.
- Added a platform-independent unit seam for the exact primary and identity-reopen share-mode
  composition, so native tests protect the Windows options rather than relying only on cross-target
  compilation.
- Added a Windows-only broker-app runtime test that creates a current-user-owned token and parent
  with a protected private DACL, invokes the real `read_and_remove_token` path, verifies the token
  bytes, and verifies the path is gone after handle-bound deletion.

### RED evidence

- Command:

  ```text
  cargo test -p robot-hal-broker-app identity_reopen_shares_delete_without_weakening_the_primary_handle
  ```

  Result: failed to compile with `E0432` because `windows_token_share_modes` did not exist. This was
  the expected failure for the new production seam that composes the primary and identity-reopen
  share modes.

### GREEN evidence

- `cargo test -p robot-hal-broker-app identity_reopen_shares_delete_without_weakening_the_primary_handle`:
  1 passed.
- `cargo test -p robot-hal-broker-app -p robot-hal-windows-file`: broker app 13 unit tests and 1
  manifest integration test passed; the adapter has no native-host tests because its runtime test is
  Windows-only.
- `cargo fmt --all --check`: passed.
- `cargo clippy --workspace --all-targets --all-features -- -D warnings`: passed.
- `cargo test --workspace --all-features`: 130 passed; the physical Serial loopback remained the
  single explicitly ignored test.
- `RUSTFLAGS='-D warnings' cargo check --workspace --all-targets --all-features --target
  x86_64-pc-windows-msvc`: passed, including compilation of the Windows-only broker-app runtime test
  and adapter test.
- `git diff --check`: passed.

### Files

- Windows token open/re-open policy and tests: `apps/robot-hal-broker/src/token.rs`.
- Fix evidence: `.superpowers/sdd/2026-08-14-v0.1-core-serial/task-7-report.md`.

### Target coverage and residual concerns

- Native macOS ran the focused suites, exact share-mode seam, formatting, warnings-denied Clippy,
  and complete workspace suite without physical hardware.
- Windows MSVC compiled all workspace targets/features and both Windows-only token deletion tests
  with warnings denied. The new broker-app integration is intended to execute on a Windows runner;
  it could not be runtime-executed on this macOS host.
- Physical Serial and normal broker startup against a real device remain intentionally untested; the
  opt-in loopback test was not enabled.
