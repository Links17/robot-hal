# Task 2 report: Camera public contract and virtual conformance

## Commits

- `63c59f9 feat(camera): add public capture contract`
- `fix(camera): enforce reviewed camera contract bounds`

## Reviewer remediation

- `CameraFrameMetadata` requires exactly two non-overlapping NV12 planes and exactly one YUYV or MJPEG plane. Uncompressed layouts require a non-zero, format-appropriate stride and enough bytes for their dimensions; all layout endpoints are overflow-checked.
- `CameraFrame` rejects payloads over 24 MiB and layouts extending outside the payload. The virtual adapter's NV12 and YUYV layouts remain valid.
- Capability identifiers permit non-empty dotted contract segments, including `camera.frames.shm/v1`, and reject empty segments.
- Resource selector and descriptor serialization is fallible. `TransportKind::Camera` now returns `runtime.protocol.invalid_message` until a later additive protocol minor defines Camera wire support; it is never downgraded to `Unspecified`.
- The virtual adapter validates presence while holding its state lock for frame and control publication. Its deterministic unplug-before-publication seam proves both paths fail closed.

## Modified files

- `crates/robot-hal-broker/src/can_dispatch.rs`
- `crates/robot-hal-broker/src/connection.rs`
- `crates/robot-hal-broker/src/usb_gpio_dispatch.rs`
- `crates/robot-hal-broker/tests/broker_contract.rs`
- `crates/robot-hal-camera/src/lib.rs`
- `crates/robot-hal-client/src/connection.rs`
- `crates/robot-hal-core/src/capability.rs`
- `crates/robot-hal-core/tests/core_contract.rs`
- `crates/robot-hal-protocol/src/conversion.rs`
- `crates/robot-hal-protocol/tests/protocol_contract.rs`
- `crates/robot-hal-testkit/src/virtual_camera.rs`
- `crates/robot-hal-testkit/tests/camera_conformance.rs`

## Verification

- RED: `cargo test -p robot-hal-core --test core_contract && cargo test -p robot-hal-protocol --test protocol_contract && cargo test -p robot-hal-testkit --test camera_conformance` — stopped as expected at `capability_accepts_nonempty_multi_segment_contract_names`; `camera..capture/v1` was incorrectly accepted.
- GREEN: `cargo fmt --all && cargo test -p robot-hal-core --test core_contract && cargo test -p robot-hal-camera && cargo test -p robot-hal-testkit --test camera_conformance && cargo test -p robot-hal-protocol --test protocol_contract` — passed (22 core contract tests, 6 camera conformance tests, 36 protocol contract tests).
- `cargo test -p robot-hal-broker --test broker_contract && cargo test -p robot-hal-client --test client_contract && cargo test -p robot-hal-client --test usb_gpio_contract && cargo fmt --all --check && cargo clippy --workspace --all-targets --all-features -- -D warnings` — passed (45 broker, 37 client, and 3 USB/GPIO client tests).
- `git diff --check` — passed.

## Boundary and concerns

- No Camera protobuf enum, message, envelope, broker operation, client API, frame shared-memory transport, native adapter, or hardware qualification was added.
- Camera descriptors and selectors intentionally cannot traverse protocol minor 2. They fail closed with the structured `runtime.protocol.invalid_message` error until the later additive protocol task defines a Camera wire representation.
