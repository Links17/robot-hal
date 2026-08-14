# Task 8 report: cross-platform async Python broker client

## Status

Implemented the v0.1 Serial-only Python binding, real virtual-broker integration seam, deterministic
checked-in protobuf generation, and CI drift verification.

## Files

- `bindings/python/pyproject.toml`, `bindings/python/uv.lock`: Python >=3.11 package metadata,
  platform-conditional pywin32 dependency, pinned direct dependencies, and locked dev toolchain.
- `bindings/python/seeed_hal/{__init__,client,serial,errors}.py`: protobuf-independent public API,
  typed values, bounded async client, structured errors, events, and broker-owned Serial sessions.
- `bindings/python/seeed_hal/transport_{unix,windows}.py`: bounded Unix socket and local Named Pipe
  transports using the broker's big-endian 32-bit frame prefix.
- `bindings/python/seeed_hal/proto/{__init__,hal_pb2}.py`: private checked-in generated protobuf.
- `bindings/python/tests/{conftest,test_client_contract}.py`: protocol fault injection, mocked Windows
  delegation, and real subprocess broker/virtual-adapter integration coverage.
- `crates/seeed-hal-broker/examples/virtual_broker.rs`: test-only language-client broker process seam;
  no production CLI adapter-switch flag was added.
- `crates/seeed-hal-broker/Cargo.toml`, `Cargo.lock`: example-only test dependencies.
- `scripts/generate-protocol.sh`: executable uv/grpc_tools generator.
- `.github/workflows/ci.yml`: regeneration drift gate on every CI OS and frozen Python test execution.
- `docs/architecture/hal-architecture.md`: implemented Python client concurrency, framing, transport,
  limits, and zeroization boundary.
- `.gitignore`: Python environment/cache exclusions.

## TDD evidence

RED commands and observed results:

1. `cd bindings/python && uv run pytest tests/test_client_contract.py -q`
   - Collection failed with `ModuleNotFoundError: No module named 'seeed_hal'`.
2. `cd bindings/python && uv run pytest tests/test_client_contract.py::test_reversed_responses_remain_correlated -q`
   - Initially failed on the missing typed public API, then exposed a handshake bytes conversion bug
     before the correlation test could connect.
3. `cd bindings/python && uv run pytest tests/test_client_contract.py::test_python_client_round_trips_complete_serial_contract -q`
   - Setup failed with `no example target named virtual_broker` before the test-only launch seam existed.
4. `cd bindings/python && uv run pytest tests/test_client_contract.py::test_client_close_wakes_a_waiting_event_subscriber -q`
   - Timed out because terminal event delivery did not wake an already-waiting receiver.
5. `cd bindings/python && uv run --python 3.11 --frozen pytest tests/test_client_contract.py::test_invalid_python_arguments_use_stable_hal_errors -q`
   - Failed because a boolean byte limit reached transport connect and returned
     `runtime.broker.disconnected` instead of the stable local `runtime.argument.invalid` error.

GREEN commands and observed results:

- `cd bindings/python && uv run pytest tests/test_client_contract.py::test_reversed_responses_remain_correlated -q`
  - `1 passed`.
- `cd bindings/python && uv run pytest tests/test_client_contract.py::test_python_client_round_trips_complete_serial_contract -q`
  - `1 passed` through the real framed broker process and virtual Serial adapter.
- `cd bindings/python && uv run pytest tests/test_client_contract.py::test_client_close_wakes_a_waiting_event_subscriber -q`
  - `1 passed`.
- `cd bindings/python && uv run pytest -q`
  - `15 passed`.
- `cd bindings/python && uv run --python 3.11 --frozen pytest -q`
  - `15 passed` on the minimum supported Python line.

## Generated provenance

`scripts/generate-protocol.sh` runs frozen `uv` dependencies from `bindings/python/uv.lock`, invokes
`python -m grpc_tools.protoc` from grpcio-tools 1.75.1, uses
`proto/seeed/hal/v1/hal.proto` as `hal.proto`, and writes
`bindings/python/seeed_hal/proto/hal_pb2.py`. The generated header contains no absolute path or host
identity. Two consecutive generations produced the same SHA-256:
`0d17a1312ff3f0b9e797d44c94d6b19d72e5c0e7bfd78647d64b18a91428e3e7`.

## Architecture

- Handshake/authentication and offered-limit validation complete before general I/O tasks start.
- One bounded writer queue/task serializes every post-handshake outbound envelope; one reader task
  preserves inbound event order and correlates reversed responses by nonzero u64 request ID.
- Pending calls, writer admission, cancellation/completion tombstones, subscriptions, and per-
  subscription event delivery are bounded. Cancellation, overflow, malformed/unknown/duplicate
  responses, disconnect, and explicit close fan out stable `HalError` values.
- The 1 MiB hard cap and negotiated frame cap are checked from the prefix before reading a body.
  Raw protobuf preflight checks Serial read field lengths against requested and negotiated limits
  before protobuf decode. Serial writes pre-compute envelope overhead before copying the payload.
- Unix uses asyncio streams. Windows imports pywin32 lazily and delegates connect, state setup,
  read, write, and close operations through `asyncio.to_thread`; only `\\.\pipe\...` endpoints are
  accepted.
- Mutable client-owned token and encode buffers are wiped. CPython/protobuf/asyncio/kernel-created
  immutable or transient copies are outside Python's enforceable zeroization boundary and this is
  documented in the client module and architecture.

## Test and target coverage

- Real broker process with virtual adapter: enumerate, open, ordered session events, write, flush,
  control lines, read, ownership conflict as a structured broker error, close, and cleanup.
- Protocol/client: reversed correlation, nonzero/u64 exhaustion, pending backpressure, caller
  cancellation, disconnect/close fanout, unknown/duplicate/malformed responses, hard/negotiated
  frame limits, requested/negotiated read limits, write limits, and public protobuf isolation.
- Windows Python transport: compile/import on macOS and pywin32 call delegation via mocks.
- Rust: formatting, clippy, full workspace tests, and `x86_64-pc-windows-msvc` all-target check.

Final verification commands:

- `cargo fmt --all --check` — passed.
- `cargo clippy --workspace --all-targets --all-features -- -D warnings` — passed.
- `cargo test --workspace --all-features` — passed; all default hardware-free tests passed and the
  physical loopback test remained ignored as intended.
- `cargo check --workspace --all-targets --all-features --target x86_64-pc-windows-msvc` — passed.
- `uv run --project bindings/python --frozen python -m compileall -q bindings/python/seeed_hal bindings/python/tests`
  — passed.

## Concerns and limitations

- A real Windows Python/pywin32 Named Pipe runtime was not available in this macOS task. Windows
  behavior is covered by mocks, import/compile checks, the cross-target Rust broker build, and the
  Windows CI job, but still needs native Windows acceptance for OS/pywin32 edge behavior.
- Python cannot guarantee erasure of immutable bytes copied internally by protobuf, asyncio, or the
  OS. The binding does not retain or represent the token and wipes every mutable copy it owns.
- Physical Serial hardware is intentionally not exercised; default tests remain hardware-free.
