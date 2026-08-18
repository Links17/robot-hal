# Broker black-box conformance

This suite qualifies the built broker executable without physical hardware. The executable must be
compiled with the test-only `virtual-adapters` feature; there is no runtime switch that replaces the
production adapters.

```bash
cargo build -p seeed-hal-broker-app --features virtual-adapters
uv run --project bindings/python --frozen pytest -q tests/conformance/test_runner_contract.py
uv run --project bindings/python --frozen python \
  tests/conformance/run-broker-conformance.py \
  --broker target/debug/seeed-hal-broker
```

On Windows, pass `target/debug/seeed-hal-broker.exe`. The Python 3.11 runner creates a unique local
endpoint and startup token, waits with bounded deadlines, exchanges length-prefixed protobuf
frames, and removes its token, endpoint, temporary directory, and child process. It covers
wire-major-1/minor-3 handshake, virtual CAN/USB/GPIO enumeration and session operations, Serial
enumeration/open/write/read/flush/control lines, and Camera enumerate/exclusive-open/capture,
shared-memory descriptor/frame lease/drop count, controls descriptor/get/set/auto, close/reopen,
and stale-generation rejection. Camera frame bytes are not sent through protobuf. The suite also
covers disconnect owner cleanup with resource reuse and cooperative process shutdown. It is virtual
broker evidence only, not physical camera qualification.

One monotonic deadline bounds each complete request even when runtime events are interleaved. Process
startup/readiness, transport connection, disconnect cleanup, cooperative shutdown, kill fallback,
process wait, and diagnostic capture are also bounded. Stderr is drained concurrently and only its
last 64 KiB is retained for a failing run.

Unix uses asyncio Unix sockets. Windows uses the binding's async pywin32 Named Pipe transport, which
delegates connect/read/write/close calls through `asyncio.to_thread`; the binding tests exercise that
off-event-loop contract deterministically on non-Windows hosts. Before launch, the Windows runner
uses pywin32 to apply protected DACLs to the temporary parent and token, granting access only to the
current user, SYSTEM, and built-in Administrators. Native Windows execution remains an external
acceptance gate until its CI job runs.
