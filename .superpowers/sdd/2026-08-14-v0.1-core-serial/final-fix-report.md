# Final fix report — v0.1 core and Serial

**Date:** 2026-08-15 (Asia/Shanghai)

**Baseline:** `4bad38a24c09ca75439a6971bcd3dbd2bb04a6f4`

**Implementation commit:** `d05ffe8a96c4ca1db1b1a7c7b21d205aca9480ea`

**Residual correction commit:** `7f383398823b5c9eda786ded66b50b1a5e902856`

**Host:** macOS arm64

**Status:** All final-review findings are implemented and all available local gates pass. Native
Windows, native Linux, remote CI, and physical Serial acceptance remain external gates.

## Finding results

### 1. Canonical fail-closed resource resolution

- RED: the baseline compared complete selectors for equality, so a stronger descriptor did not
  satisfy a weaker minimum and the first duplicate persisted ID could be selected by endpoint.
  Added `identity_quality_is_an_ordered_minimum_threshold`,
  `resolver_requires_transport_capability_and_exact_persisted_id`,
  `duplicate_persisted_identity_is_ambiguous_even_when_endpoints_differ`, and
  `ambiguous_virtual_open_fails_closed`.
- GREEN: `cargo test -p robot-hal-core --test core_contract` → 11 passed;
  `cargo test -p robot-hal-testkit --test serial_conformance` → 9 passed.
- Result: `Weak < Medium < Strong` is an inclusive threshold. The shared resolver matches persisted
  ID, transport, minimum quality, and required capability; endpoints never disambiguate. Zero and
  multiple matches use `runtime.resource.not_found` and `runtime.resource.ambiguous` respectively.
- Files: `crates/robot-hal-core/src/{capability,identity,lib}.rs`,
  `crates/robot-hal-serial/src/lib.rs`, `adapters/serialport/src/lib.rs`,
  `crates/robot-hal-testkit/src/virtual_serial.rs`, protocol descriptor conversion, and their tests.

### 2. Failed-open fencing-generation rollback

- RED: failed or cancelled opens permanently advanced and retained generations, including unique
  invalid selectors. Added `thousands_of_unique_failed_opens_do_not_retain_generation_entries`,
  `failed_reopen_after_exposure_does_not_erase_or_skip_the_last_generation`, and strengthened
  `cancelling_an_in_progress_open_releases_its_reservation`.
- GREEN: `cargo test -p robot-hal-runtime --test serial_runtime` → 16 passed, including 4,096
  distinct failed opens with zero retained identities.
- Result: reservations remain provisional until `finish_open` commits them. Rollback compare-checks
  the exact current reservation; generations already exposed to callers remain monotonic.
- Files: `crates/robot-hal-runtime/src/{lease_table,registry,lib}.rs` and
  `crates/robot-hal-runtime/tests/serial_runtime.rs`.

### 3. Inclusive protocol-minor negotiation

- RED: the baseline accepted only exact minor equality and used
  `runtime.protocol.incompatible_version`. Added range, legacy-default, no-overlap, broker-selection,
  and Rust/Python selected-minor tests.
- GREEN: `cargo test -p robot-hal-protocol --test protocol_contract` → 7 passed;
  `cargo test -p robot-hal-broker --test broker_contract
  broker_selects_highest_shared_minor_and_reports_its_supported_range` → 1 passed; the complete Rust
  and Python client suites also pass.
- Result: additive request tags 8/9 and response tags 7/8 carry inclusive ranges. Both zero range
  fields preserve legacy exact-minor behavior; range-aware requests set legacy `protocol_minor` to
  their maximum. The highest shared minor is selected; incompatibility uses
  `runtime.protocol.version_incompatible`.
- Files: `proto/seeed/hal/v1/hal.proto`, generated Python binding,
  `crates/robot-hal-protocol`, broker/client handshake code, Python client, manifest, and black-box
  runner.

### 4. Protected Windows Named Pipe DACL

- RED: both broker entry points used Tokio's default `ServerOptions::create`, so the implementation
  had no explicit protected-DACL contract. Policy tests were introduced before the adapter wrapper.
- GREEN: `cargo test -p robot-hal-windows-security` → 2 platform-neutral tests passed;
  `cargo clippy --target x86_64-pc-windows-msvc --workspace --all-targets --all-features -- -D
  warnings` → exit 0. The runtime-gated native inspection test compiles for Windows.
- Result: first and subsequent instances use a protected DACL with only the current user,
  LocalSystem, and built-in Administrators. The narrow adapter owns the only new `unsafe`, with a
  preceding `SAFETY` invariant and upstream citations. Token ACL validation now uses the same exact
  trustee set and no longer permits Owner Rights.
- Files: `adapters/windows-security/`, Windows broker listeners, executable pipe creation, token ACL
  validation, workspace manifests, and lockfile.
- Limitation: the DACL inspection test requires native Windows execution.

### 5. Cancellation-owned Python pywin32 I/O

- RED: `uv run --project bindings/python --frozen pytest -vv -x
  bindings/python/tests/test_client_hardening.py::test_windows_blocked_read_cancellation_transfers_close_to_owned_worker
  bindings/python/tests/test_client_hardening.py::test_windows_blocked_write_cancellation_transfers_close_to_owned_worker`
  reproduced cancellation completing while an executor worker still owned the handle.
- GREEN: the same focused command → 2 passed; adding
  `test_windows_transport_owns_steady_state_pywin32_calls_on_one_thread` → 3 passed; full Python
  suite → 99 passed.
- Result: setup uses one tracked `asyncio.to_thread`; one bounded actor thread exclusively owns
  steady-state `ReadFile`, `WriteFile`, and `CloseHandle`. The pipe uses byte-mode `PIPE_NOWAIT`;
  active-operation cancellation closes fail-closed and waits for actor termination. Capacity is four
  and close is idempotent.
- Files: `bindings/python/robot_hal/transport_windows.py` and Python contract/hardening tests.
- Limitation: real pywin32 `PIPE_NOWAIT` behavior and cancellation timing require native Windows.

The final scoped re-review then identified a Windows-only semantic gap in this finding: pywin32 311
raises `pywintypes.error` directly from `Exception`, so the original `OSError` handlers did not see
normal `ERROR_NO_DATA` polling results. The residual correction explicitly carries the native error
type through connect and the actor, retries only `ERROR_NO_DATA` (`232`), treats zero-byte writes as
backpressure, and fails closed for other native status codes. Faithful platform-neutral tests cover
empty-read and write-backpressure progress, cancellation, actor termination, repeated close, and
non-retryable errors. Per the current repository workflow, implementation and tests were completed
before the unified verification run; no per-case red-green cycle was used.

### 6. Compatible `HalError` serde

- RED: the derived tuple deserializer could not consume the decision-only serialized map.
  `serialized_hal_error_shape_deserializes_and_reserializes_compatibly` characterized the mismatch.
- GREEN: `cargo test -p robot-hal-core --test core_contract
  serialized_hal_error_shape_deserializes_and_reserializes_compatibly` → 1 passed.
- Result: custom deserialization accepts the stable decision map. `debug_message` remains output-only
  and is empty after deserialization.
- Files: `crates/robot-hal-core/src/error.rs` and core contract tests.

### 7. Constant-time-only startup-token comparison

- RED: `StartupToken` exposed ordinary derived `PartialEq/Eq` in addition to a local constant-time
  comparison.
- GREEN: `cargo test -p robot-hal-broker --lib
  startup_token_authentication_uses_the_explicit_secret_comparison` → 1 passed.
- Result: ordinary equality derives were removed. `StartupToken::authenticates` is the explicit
  constant-time API used by the handshake.
- Files: `crates/robot-hal-broker/src/{lib,connection}.rs`.

### 8. Complete v1 protobuf tag locks

- RED: the previous contract locked only the handshake request tag.
- GREEN: `cargo test -p robot-hal-protocol --test protocol_contract
  every_v1_envelope_payload_field_number_is_locked` → 1 passed; the complete protocol suite passed.
- Result: every v1 envelope payload tag is locked (`10`, `11`, `20..33`, `40`, `100`), new range
  fields are locked, and an unknown additive handshake field is tolerated.
- Files: `crates/robot-hal-protocol/tests/protocol_contract.rs`.

### 9. Structured connection-outcome logging

- RED: successful connection tasks discarded both `connection_error` and `cleanup_error` outcomes.
- GREEN: `cargo test -p robot-hal-broker-app
  structured_connection_logging_excludes_diagnostics_and_secrets` → 1 passed.
- Result: the executable records kind, stable name, category, operation, and retryability for both
  outcome types. Debug strings, raw payloads, and credentials are excluded.
- Files: `apps/robot-hal-broker/src/main.rs` and its tests.

### 10. Immediate Unix socket cleanup ownership

- RED: cleanup guards were created after a fallible permission change, so that failure could strand
  the bound socket.
- GREEN: `cargo test -p robot-hal-broker-app unix_socket_cleanup_is_armed_immediately_after_bind`
  → 1 passed; `cargo test -p robot-hal-broker --test broker_contract
  unix_listener_uses_a_private_directory_and_socket` → 1 passed.
- Result: executable and library guards are created immediately after bind and remove their socket
  on drop, including permission-error paths.
- Files: `apps/robot-hal-broker/src/main.rs`,
  `crates/robot-hal-broker/src/listener/unix.rs`, and broker contract tests.

## Final verification

| Gate | Command | Result |
| --- | --- | --- |
| Format | `cargo fmt --all --check` | Passed |
| Rust lint | `cargo clippy --workspace --all-targets --all-features -- -D warnings` | Passed |
| Rust suite | `cargo test --workspace --all-features` | 150 passed, 1 ignored physical test |
| Python suite | `uv run --project bindings/python --python 3.11 --frozen pytest -q` | 119 passed |
| Generated protocol | `./scripts/check-generated-protocol.sh` twice | Passed twice |
| Windows cross-target lint | `cargo clippy --target x86_64-pc-windows-msvc --workspace --all-targets --all-features -- -D warnings` | Passed |
| Production manifest | `cargo test -p robot-hal-broker-app --no-default-features --test manifest` | 1 passed |
| Virtual manifest | `cargo test -p robot-hal-broker-app --features virtual-adapter --test manifest` | 1 passed |
| Broker build | `cargo build -p robot-hal-broker-app --features virtual-adapter` | Passed |
| Local black-box | `uv run --project bindings/python --frozen python tests/conformance/run-broker-conformance.py --broker target/debug/robot-hal-broker` | 9 checks passed |
| Manifest CLI | `target/debug/robot-hal-broker --manifest` | v0.1.0, wire 1.0 inclusive range, virtual adapter, SHA-256 present |
| Linux cross-target lint | `cargo clippy --target x86_64-unknown-linux-gnu --workspace --all-targets --all-features -- -D warnings` | Blocked by missing target libudev pkg-config sysroot/wrapper |

Toolchain: `rustc 1.85.1`, `cargo 1.85.1`, `uv 0.8.22`, Python `3.11.13`.

## External gates and concerns

- Native Windows CI must execute the Named Pipe DACL inspection test, real pywin32 `PIPE_NOWAIT`
  client path, Ctrl-Break handling, token ACL checks, and Windows broker black-box suite.
- Native Linux CI or a configured target sysroot is required; local cross-target lint stops in
  `libudev-sys v0.1.4` before checking the complete workspace.
- Physical Serial loopback and unplug/replug cleanup remain pending under the documented runbook.
- No remote workflow was triggered, no physical hardware was used, and no push was performed.
