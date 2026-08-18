# Broker black-box conformance

This suite qualifies the built broker executable without physical hardware. The executable must be
compiled with the test-only `virtual-adapters` feature; there is no runtime switch that replaces the
production adapters.

```bash
cargo build -p seeed-hal-broker-app --features virtual-adapters
uv run --project bindings/python --python 3.11 --frozen pytest -q \
  tests/conformance/test_runner_contract.py \
  tests/conformance/test_minor_matrix.py
uv run --project bindings/python --frozen python \
  tests/conformance/run-broker-conformance.py \
  --broker target/debug/seeed-hal-broker \
  --protocol-minor 3
```

On Windows, pass `target/debug/seeed-hal-broker.exe`. The Python 3.11 runner creates a unique local
endpoint and startup token, waits with bounded deadlines, exchanges length-prefixed protobuf
frames, and removes its token, endpoint, temporary directory, and child process. `--protocol-minor`
is an exact offer and must be one of 0, 1, 2, or 3. Profiles are additive: minor 0 exercises Serial;
minor 1 adds CAN; minor 2 adds USB and GPIO; minor 3 adds Camera. Minor 0, 1, and 2 each send one
request introduced by the next minor and require the broker dispatcher's exact stable fail-closed
error: CAN currently uses `runtime.protocol.capability_unsupported`, while USB/GPIO and Camera use
`runtime.protocol.unsupported_capability`.

By default the handshake requires every capability defined for the selected profile. Repeating
`--require-capability CAPABILITY` replaces that default requirement set with exactly the capabilities
listed by the user; it is useful for qualifying a deliberately narrower broker build without
silently re-adding profile defaults.

The minor 3 profile retains complete virtual CAN/USB/GPIO enumeration and session operations,
Serial enumeration/open/write/read/flush/control lines, and Camera
enumerate/exclusive-open/capture, shared-memory descriptor/frame lease/drop count, controls
descriptor/get/set/auto, close/reopen, and stale-generation rejection. Camera frame bytes are not
sent through protobuf. Every profile also covers disconnect owner cleanup with resource reuse and
cooperative process shutdown. It is virtual broker evidence only, not physical camera
qualification.

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
