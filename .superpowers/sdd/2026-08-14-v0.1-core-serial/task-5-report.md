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
