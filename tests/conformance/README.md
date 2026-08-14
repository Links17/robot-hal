# Broker black-box conformance

This suite qualifies the built broker executable without physical hardware. The executable must be
compiled with the test-only `virtual-adapter` feature; there is no runtime switch that replaces the
production adapter.

```bash
cargo build -p seeed-hal-broker-app --features virtual-adapter
uv run --project bindings/python --frozen pytest -q tests/conformance/test_runner_contract.py
uv run --project bindings/python --frozen python \
  tests/conformance/run-broker-conformance.py \
  --broker target/debug/seeed-hal-broker
```

On Windows, pass `target/debug/seeed-hal-broker.exe`. The Python 3.11 runner creates a unique local
endpoint and startup token, waits with bounded deadlines, exchanges length-prefixed protobuf
frames, and removes its token, endpoint, temporary directory, and child process. It covers
handshake, Serial enumeration/open/write/read/flush/control lines, stale-generation rejection,
disconnect owner cleanup with resource reuse, and cooperative process shutdown.

Unix uses asyncio Unix sockets. Windows uses the binding's async pywin32 Named Pipe transport, which
delegates connect/read/write/close calls through `asyncio.to_thread`; the binding tests exercise that
off-event-loop contract deterministically on non-Windows hosts. Native Windows execution remains an
external acceptance gate until its CI job runs.
