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
- [Physical Serial loopback runbook](docs/runbooks/serial-loopback.md)

## Status

The v0.1 core and Serial vertical slice is implemented: platform-neutral identity, capabilities,
leases, structured errors and events; the library-first runtime; virtual and native Serial adapters;
the local broker; and Rust and Python clients. The implementation remains independent of robot,
device-protocol, workflow, and product behavior.

Linux, macOS, and Windows are target platforms. Local macOS and cross-compile evidence is recorded in
the v0.1 acceptance document; native Linux/Windows CI, physical Serial loopback, and release
qualification remain pending external gates. CAN/CAN FD, USB, GPIO, Camera, Node bindings, and the
camera frame data plane remain planned.

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
