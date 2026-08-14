# Task 5 Report: Real cross-platform Serial adapter

## Status

Implemented `seeed-hal-adapter-serialport` under `adapters/serialport` and added it to the workspace.

## RED

Command:

```bash
cargo test -p seeed-hal-adapter-serialport --test metadata
```

Observed failure:

```text
error: package ID specification `seeed-hal-adapter-serialport` did not match any packages
```

This matched the task brief’s expected red state before the adapter package existed.

## GREEN

Implemented:

- `SerialPortAdapter` using `serialport::available_ports()` inside `tokio::task::spawn_blocking`.
- USB and endpoint identity normalization with explicit `Strong`, `Medium`, and `Weak` quality.
- Percent-encoded identity segments.
- Native async sessions via `tokio_serial::new(...).open_native_async()`.
- Explicit mapping for baud, data bits, parity, stop bits, flow control, DTR, and RTS.
- Stable HAL error names for not found, busy, permission denied, timeout, disconnected, and unsupported configuration, with platform details kept in debug diagnostics.
- Ignored opt-in physical loopback test gated by `hardware-loopback` and `SEEED_HAL_SERIAL_LOOPBACK`.

## Verification

```bash
cargo test -p seeed-hal-adapter-serialport --test metadata
cargo test -p seeed-hal-adapter-serialport
cargo fmt --all --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace --all-features
```

Results:

- Metadata tests: 2 passed.
- Adapter tests: metadata passed; hardware loopback ignored.
- Workspace tests: 30 passed; 1 ignored; doc tests passed.
- Fmt and clippy passed.

## Notes

- Current host only has the `aarch64-apple-darwin` Rust target installed, so no cross-target compile command was run.
- Default test commands do not open hardware; the physical loopback test remains ignored unless explicitly selected.

## Fix Round 1

### Files changed

- `adapters/serialport/src/identity.rs`
- `adapters/serialport/src/lib.rs`
- `adapters/serialport/src/session.rs`
- `adapters/serialport/tests/metadata.rs`
- `adapters/serialport/tests/hardware_loopback.rs`
- `.superpowers/sdd/2026-08-14-v0.1-core-serial/task-5-report.md`

### RED evidence

```bash
cargo test -p seeed-hal-adapter-serialport --test metadata
```

Output summary: failed as expected. New tests showed USB ports without serial numbers were still reported as `Medium` and same-model devices shared `serial:usb:10c4:ea60:meta:...` identities.

```bash
cargo test -p seeed-hal-adapter-serialport --lib
```

Output summary: failed as expected because `run_blocking_drain` did not exist yet; this covered the new drain-isolation seam before implementation.

### GREEN evidence

```bash
cargo test -p seeed-hal-adapter-serialport --test metadata
cargo test -p seeed-hal-adapter-serialport --lib
cargo test -p seeed-hal-adapter-serialport
for i in {1..20}; do cargo test -q -p seeed-hal-adapter-serialport --lib cancelled_ || exit 1; done
cargo fmt --all --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace --all-features
```

Output summary: metadata tests passed `5/5`; adapter unit tests passed `7/7`; package tests passed with the physical loopback test ignored; fmt passed; clippy passed with `-D warnings`; workspace tests passed with adapter loopback ignored.

### Fixes

- Serial-less USB metadata now falls back to percent-encoded weak endpoint identity; VID/PID/manufacturer/product no longer claim instance identity.
- `NoDevice` errors with upstream lock/busy descriptions map to `runtime.transport.busy`; access-denied descriptions map to `runtime.transport.permission_denied`; generic open/enumerate `NoDevice` remains `runtime.resource.not_found`.
- `map_io_error` diagnostics now include `raw_os_error=<code|none>`.
- Serial `flush()` and `close()` avoid Tokio `poll_flush`/`shutdown`; they move the stream through an explicitly owned `spawn_blocking` drain worker so Unix `tcdrain()` cannot block a Tokio executor worker.
- `read(0)` now returns `runtime.session.closed` after close.
- Hardware loopback read retries timeout results until the overall deadline.

## Fix Round 2

### Files changed

- `Cargo.toml`
- `Cargo.lock`
- `adapters/serialport/Cargo.toml`
- `adapters/serialport/src/lib.rs`
- `adapters/serialport/src/session.rs`
- `.superpowers/sdd/2026-08-14-v0.1-core-serial/task-5-report.md`

### RED evidence

```bash
cargo test -p seeed-hal-adapter-serialport --lib
```

Output summary: failed as expected before implementation. The focused tests referenced missing round-2 seams and behavior: `SessionState`, `DrainTask`, `DrainStrategy`, `map_serialport_open_error`, and the Unix `libc` raw OS error dependency were not yet present.

### GREEN evidence

```bash
cargo test -p seeed-hal-adapter-serialport --lib
cargo test -p seeed-hal-adapter-serialport --test metadata
cargo test -p seeed-hal-adapter-serialport
cargo fmt --all --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace --all-features
```

Output summary: adapter unit tests passed `12/12`; metadata tests passed; package tests passed with the physical loopback test ignored; fmt passed; clippy passed with `-D warnings`; workspace tests passed with the physical loopback test ignored.

### Fixes

- Serial session ownership now uses explicit `Ready`, `Draining`, `Closing`, and `Closed` states instead of dropping the stream while a drain/close future is cancellable.
- `flush()` and `close()` move the stream into a tracked blocking drain task before awaiting it; if the future is dropped, a later operation awaits the same task and either restores readiness or reaches a terminal closed state.
- Drain worker join failures now close the session deterministically and report `runtime.internal`.
- `serial.open` error mapping now preserves native open diagnostics when available, including `raw_os_error=<code>`.
- Serialport `NoDevice` messages that discard platform details now infer stable raw busy/access-denied codes where possible (`EBUSY` on Unix, Windows `5`/`170`).

## Fix Round 3

### Files changed

- `Cargo.toml`
- `Cargo.lock`
- `adapters/serialport/Cargo.toml`
- `adapters/serialport/src/lib.rs`
- `adapters/serialport/src/session.rs`
- `.superpowers/sdd/2026-08-14-v0.1-core-serial/task-5-report.md`

### RED evidence

```bash
cargo test -p seeed-hal-adapter-serialport --lib
```

Output summary: failed as expected before implementation. New focused tests required split close/flush worker ownership and failed to compile with `spawn_flush` / `spawn_close` not members of `DrainStrategy` and missing `CloseTask`.

Focused test names added:

- `dropping_close_future_releases_stream_without_later_session_poll`
- `close_attempts_terminal_close_after_cancelled_flush_error`
- `actual_open_missing_endpoint_carries_io_raw_os_error_from_open_path`

### GREEN evidence

```bash
cargo test -p seeed-hal-adapter-serialport --lib
cargo test -p seeed-hal-adapter-serialport --test metadata
cargo test -p seeed-hal-adapter-serialport
cargo fmt --all --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace --all-features
```

Output summary: adapter unit tests passed `13/13`; metadata tests passed `5/5`; adapter package tests passed with physical loopback ignored; fmt passed; clippy passed with `-D warnings`; workspace tests passed, including serial runtime and shared serial conformance tests, with physical loopback ignored.

### Fixes

- Split drain ownership into `DrainTask` for flush and `CloseTask` for close.
- Close workers now own and drop the serial stream inside the blocking worker; a cancelled `close()` future no longer leaves the stream retained in an unpolled join result.
- `close()` now reconciles a cancelled prior flush, records any non-disconnected flush error, then still proceeds to terminal close. If terminal close succeeds, the prior flush error is returned; terminal close errors take precedence.
- Replaced `tokio-serial` session opening with `serial2::SerialPort::open()`, a safe cross-platform open/configure path returning the actual `std::io::Error` from the failing operation.
- Removed the separate native open probe and all synthesized raw OS error inference; open diagnostics now come from the same failing open path and preserve `raw_os_error=<code>` through `map_io_error`.
- Session read/write methods remain async HAL calls by dispatching blocking `serial2` I/O onto Tokio’s blocking pool; default tests still do not open physical serial hardware.

## Fix Round 4

### Files changed

- `adapters/serialport/src/session.rs`
- `docs/architecture/hal-architecture.md`
- `.superpowers/sdd/2026-08-14-v0.1-core-serial/task-5-report.md`

### RED evidence

The round started by adding deterministic hardware-free tests for the newly reported cancellation paths and running:

```bash
cargo test -p seeed-hal-adapter-serialport --lib round4_red_tests
```

The test target failed to compile for the expected missing architecture seams: `SerialIo`, `run_blocking_open`, and `write_all_bounded` did not exist. This proved that the prior per-call `spawn_blocking` implementation could not satisfy the new tests through its existing ownership model.

The partial-write termination case then ran independently:

```bash
cargo test -p seeed-hal-adapter-serialport --lib write_all_deadline_terminates_repeated_partial_writes
```

It failed to compile with the expected missing `write_all_bounded_with_clock` seam before the deadline-aware partial-write loop was implemented.

Full-diff self-review also restored the round-3 cancelled-flush diagnostic contract. Before the terminal-error slot was added:

```bash
cargo test -p seeed-hal-adapter-serialport --lib close_reports_cancelled_flush_error_after_terminal_release
```

The test failed as expected because `close()` returned `Ok(())` instead of the flush `runtime.transport.permission_denied` error after releasing the port.

### Architecture decision

- Replaced stream migration and per-call cloned blocking tasks with one session-owned blocking actor. That actor is the only normal read/write/flush/control access path and consumes a bounded one-command queue.
- Added a terminal cancellation watchdog holding the only cloned serial handle. It does not perform normal I/O. It watches the active flush deadline and, only for an active cancelled/timed-out flush, purges the OS buffers so the actor can leave the otherwise unbounded native drain. Normal close just drops both handles; it no longer calls an unbounded `flush()`.
- Every admitted async operation owns a cancellation guard. Dropping the future atomically makes the native adapter session terminal, rejects later commands, and leaves any already-running call tracked by the owned actor until its configured OS deadline or flush interrupt completes. This is deliberately fail-closed because safely resuming after an abandoned native operation would reintroduce concurrent access.
- `close()` requests terminal shutdown before its first await. Cancelling `close()` therefore cannot retract cleanup, including when the actor is already draining a previously cancelled flush. Completion is published only after both the actor’s port and the watchdog’s interrupt clone have been dropped.
- The actor retains the first non-disconnect error produced by an operation after terminal cancellation, so a later `close()` still reports the round-3 cancelled-flush diagnostic after cleanup completes.
- The existing `SerialConfig::read_timeout` continues to configure the native read/write timeouts and now also bounds the complete partial-write loop and flush drain. Each partial write receives only the remaining operation budget.
- `open_serial_stream()` still performs the real safe cross-platform `serial2::SerialPort::open` and configuration operation. The whole open/configure/session-worker construction closure now runs through `spawn_blocking`; if its async caller is cancelled, Tokio drops the eventual late result and its session `Drop` starts autonomous cleanup.
- The public HAL traits, configuration shape, identity behavior, error names, hardware-test gate, and adapter scope did not change. The adapter-specific fail-closed cancellation behavior is recorded in the architecture document.

### Named deterministic tests

- `open_and_configure_run_outside_tokio_worker`
- `cancelled_open_future_drops_late_opened_port`
- `cancelled_read_closes_worker_before_next_access`
- `cancelled_write_all_closes_worker_before_next_access`
- `cancelled_close_while_flush_is_in_flight_releases_port_autonomously`
- `close_reports_cancelled_flush_error_after_terminal_release`
- `normal_commands_are_serialized_on_one_owned_worker`
- `write_all_stops_at_operation_deadline`
- `write_all_deadline_terminates_repeated_partial_writes`
- `actual_open_missing_endpoint_carries_io_raw_os_error_from_open_path` (retained regression coverage)

### GREEN evidence

```bash
cargo test -p seeed-hal-adapter-serialport --lib
cargo test -p seeed-hal-adapter-serialport
cargo fmt --all --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace --all-features
```

Output summary:

- Adapter unit tests passed `16/16`.
- The five cancellation regressions passed 20 consecutive repetitions (`100/100`).
- Metadata tests passed `5/5`; the opt-in physical loopback remained ignored.
- Formatting passed.
- Workspace clippy passed with `-D warnings`.
- Workspace tests passed: adapter `16`, metadata `5`, core contract `7`, runtime unit `1`, runtime integration `14`, and shared Serial conformance `8`; the one physical loopback test remained ignored and all doc tests passed.

### Concerns and platform coverage

- Tests are hardware-free and deterministic. The active-flush cancellation path is exercised with a fake that blocks drain until the watchdog’s purge arrives and verifies autonomous release after both the flush and close futures are dropped.
- The current host only has the macOS target available. Linux and Windows were not cross-compiled or physically exercised in this round; the production abort path uses `serial2`’s safe cross-platform `discard_buffers()` implementation (`tcflush` on Unix and `PurgeComm` on Windows).
