# Task 2 report: Camera public contract and virtual conformance

## Commit

`63c59f9 feat(camera): add public capture contract`

## Modified files

- `Cargo.toml`
- `Cargo.lock`
- `crates/seeed-hal-core/src/capability.rs`
- `crates/seeed-hal-core/src/identity.rs`
- `crates/seeed-hal-camera/Cargo.toml`
- `crates/seeed-hal-camera/src/lib.rs`
- `crates/seeed-hal-protocol/src/conversion.rs`
- `crates/seeed-hal-testkit/Cargo.toml`
- `crates/seeed-hal-testkit/src/lib.rs`
- `crates/seeed-hal-testkit/src/virtual_camera.rs`
- `crates/seeed-hal-testkit/tests/camera_conformance.rs`

## Verification

- RED: `cargo test -p seeed-hal-testkit --test camera_conformance` — failed as expected before implementation because `seeed_hal_camera`, `VirtualCameraAdapter`, and `run_camera_adapter_conformance` did not exist.
- GREEN: `cargo test -p seeed-hal-testkit --test camera_conformance` — passed, 4 tests.
- `cargo fmt --all --check` — passed.
- `cargo clippy --workspace --all-targets --all-features -- -D warnings` — passed.
- `cargo test --workspace --all-features` — passed; hardware-dependent tests remained explicitly ignored.
- `git diff --check` — passed.

## Boundary and concerns

- This task supplies only public, native-capture/control adapter contracts and a deterministic virtual adapter. It does not implement runtime, protobuf camera operations, broker/client paths, shared-memory transport, native adapters, or hardware qualification.
- Camera adds `TransportKind::Camera`. Current protocol v1 has no Camera enum member, so its existing conversion maps that transport to `Unspecified` only to keep legacy protocol code exhaustive and buildable. Camera protocol serialization must be added additively by the later protocol/broker task before camera descriptors traverse IPC.
- Core capability parsing now accepts multi-segment names such as `camera.frames.shm/v1`, required by the approved camera capability identifiers while retaining version and non-empty namespace/name validation.
