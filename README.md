# Robot HAL

Robot HAL is a reusable, cross-platform Rust hardware abstraction runtime for
desktop and edge applications. It is an alpha reference implementation, not a
certified safety system.

It presents application-facing interfaces for hardware transports and standard hardware classes while keeping product and device-protocol semantics outside the HAL.

Target families:

- Serial
- CAN / CAN FD
- USB
- GPIO
- Camera

Robot HAL deliberately contains no robot kinematics, device-protocol business
logic, workflow topology or product semantics. The companion
[`dora-lerobot`](https://github.com/Links17/dora-lerobot) project composes this
resource/runtime layer with robot adapters, local safety gates and LeRobot
bridges.

The implementation is library-first. Rust applications link the library
directly; Python, Node, Electron, and multi-process applications use the same
implementation through a local broker.

## Architecture

Resource identity, exclusive leases and cancellation remain below robot
semantics. This separation lets non-Dora applications reuse the same runtime.

## Documentation

- [Architecture](docs/architecture/hal-architecture.md)
- [Responsibility contract](docs/contracts/hal-responsibility.md)
- [Versioning contract](docs/contracts/versioning.md)
- [Hardware capability matrix](docs/contracts/capability-matrix.md)
- [v0.1 implementation plan](docs/superpowers/plans/2026-08-14-v0.1-core-serial.md)
- [v0.1.0 acceptance evidence](docs/releases/v0.1.0-acceptance.md)
- [v0.2.0 acceptance evidence](docs/releases/v0.2.0-acceptance.md)
- [v0.3 USB/GPIO acceptance evidence](docs/releases/v0.3.0-acceptance.md)
- [v0.4 Camera acceptance evidence](docs/releases/v0.4.0-acceptance.md)
- [v0.4 Camera external qualification](docs/releases/v0.4.0-camera-qualification.md)
- [v0.5 RC release qualification](docs/releases/v0.5.0-rc-qualification.md)
- [v0.5 release artifact contract](docs/contracts/release-artifacts.md)
- [Physical Serial loopback runbook](docs/runbooks/serial-loopback.md)
- [Native USB qualification runbook](docs/runbooks/nusb-native.md)
- [Native Linux GPIO qualification runbook](docs/runbooks/linux-gpio-native.md)
- [Native Windows GPIO qualification runbook](docs/runbooks/windows-gpio-native.md)
- [Native macOS Camera qualification runbook](docs/runbooks/camera-avfoundation-native.md)
- [Native Linux Camera qualification runbook](docs/runbooks/camera-v4l2-native.md)
- [Native Windows Camera qualification runbook](docs/runbooks/camera-mediafoundation-native.md)

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
is registered. v0.4 adds Camera contracts, a bounded named shared-memory frame ring, virtual
conformance, wire-minor-3 broker operations, Rust/Python clients, and AVFoundation, V4L2, and Media
Foundation adapters. Camera frame bytes do not use protobuf IPC. Native camera qualification remains
an external per-platform gate recorded separately from the hardware-free acceptance evidence. Node
bindings and device protocols remain planned.

v0.5 adds release/conformance infrastructure: an exact three-platform artifact
contract, immutable candidate aggregation, per-host broker verification,
hardware-free virtual conformance, and a separately permissioned final
attestation/prerelease path. It does not add a HAL hardware-class interface.
The v0.5 qualification record distinguishes local candidates from pending
hosted and physical-hardware evidence.

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

Pull requests and pushes run a read-only GitHub Actions source gate, followed by native
macOS/Linux/Windows broker conformance. The hosted platform matrix is derived from
[`release/targets.toml`](release/targets.toml); each job verifies a production broker manifest
and separately qualifies a `virtual-adapters` broker for protocol minors 0 through 3. Hosted
conformance JSON is retained only as test evidence and is not a release artifact.

## Development

```bash
cargo fmt --all --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace --all-features
cd bindings/python && uv run --frozen pytest -q
```

See [`CONTRIBUTING.md`](CONTRIBUTING.md) for contribution boundaries and the
release checklist. Native hardware qualification remains an explicit external
gate documented under `docs/runbooks/`.

Run the Camera v0.4 hardware-free release gate with:

```bash
./scripts/check-camera-v0.4.sh
```

It deliberately leaves physical adapter tests ignored. Follow the platform Camera runbooks to
produce external qualification evidence on real devices.
