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

## Status

Architecture and implementation planning only. No production implementation exists yet.

## Verification

The workspace uses Rust 1.85 and Rust 2024. Run the canonical checks from the repository root:

```bash
cargo fmt --all --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace --all-features
```

When Python bindings are present, run their tests with:

```bash
uv run pytest bindings/python/tests
```
