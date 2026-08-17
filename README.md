# Seeed HAL

Seeed HAL is a reusable, cross-platform Rust hardware abstraction runtime for desktop and edge applications.

It presents application-facing interfaces for hardware transports and standard hardware classes while keeping product and device-protocol semantics outside the HAL.

Target families:

- Serial
- CAN / CAN FD
- USB
- GPIO
- Camera

The implementation is library-first. Rust applications link the library directly; Python, Node, Electron, and multi-process applications use the same implementation through a local broker.

## Documentation

- [Architecture](docs/architecture/hal-architecture.md)
- [Responsibility contract](docs/contracts/hal-responsibility.md)
- [Versioning contract](docs/contracts/versioning.md)
- [v0.1 implementation plan](docs/superpowers/plans/2026-08-14-v0.1-core-serial.md)
- [v0.1.0 acceptance evidence](docs/releases/v0.1.0-acceptance.md)
- [v0.2.0 acceptance evidence](docs/releases/v0.2.0-acceptance.md)
- [v0.3 USB/GPIO acceptance evidence](docs/releases/v0.3.0-acceptance.md)
- [Physical Serial loopback runbook](docs/runbooks/serial-loopback.md)
- [Native USB qualification runbook](docs/runbooks/nusb-native.md)
- [Native Linux GPIO qualification runbook](docs/runbooks/linux-gpio-native.md)
- [Native Windows GPIO qualification runbook](docs/runbooks/windows-gpio-native.md)

## Status

The v0.1 core and Serial vertical slice is implemented: platform-neutral identity, capabilities,
leases, structured errors and events; the library-first runtime; virtual and native Serial adapters;
the local broker; and Rust and Python clients. The implementation remains independent of robot,
device-protocol, workflow, and product behavior.

Linux, macOS, and Windows are target platforms. Local macOS and cross-compile evidence is recorded in
the v0.1 acceptance document; native Linux/Windows CI, physical Serial loopback, and release
qualification remain pending external gates. v0.2 adds CAN/CAN FD contracts, virtual conformance,
broker and Rust/Python clients, Linux SocketCAN, and optional PCAN-Basic adapters. Native CAN
hardware qualification remains an external gate. v0.3 adds USB Control/Bulk/Interrupt and GPIO
line/edge APIs, bounded runtime workers, wire-minor-2 broker operations, Rust and Python sessions,
virtual conformance adapters, and opt-in nusb, Linux GPIO, and Windows GPIO adapters. Native USB
and GPIO qualification remain external gates; macOS GPIO fails closed because no native GPIO adapter
is registered. Camera, Node bindings, and the camera frame data plane remain planned.

## Verification

The workspace uses Rust 1.85 and Rust 2024. Run the full Rust gate with:

```bash
cargo fmt --all --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace --all-features
```

Run the frozen Python 3.11 binding suite with:

```bash
cd bindings/python && uv run --frozen pytest -q
```

The hardware-free executable conformance command is documented in
[`tests/conformance/README.md`](tests/conformance/README.md).
