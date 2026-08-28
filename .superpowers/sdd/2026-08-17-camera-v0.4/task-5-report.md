# Camera v0.4 Task 5 report

## Implemented

- Wire major 1 now advertises additive minor 3. Camera has dedicated transport,
  request, response, mapping descriptor, frame lease, and control messages.
  Camera frame payload bytes are deliberately absent from the protobuf schema.
- Protocol conversion validates Camera selectors, requested formats, bounded slot
  counts, control leases, control values, mapping descriptor fields, and frame
  lease metadata. Mapping capability tokens are redacted by the shared-memory
  descriptor's `Debug` implementation.
- The broker dispatches Camera operations only for minor 3 connections,
  maintains connection-scoped sessions, requires declared resource capabilities,
  and relies on owner revocation when a connection finishes. Shared-memory
  descriptors require a control lease and the `camera.frames.shm/v1` capability.
- Rust exposes `RemoteCameraHandle`, which locally gates each operation on minor
  3 and its negotiated Camera capability. It opens the existing
  `ReadOnlyMapping` safe, copy-only API and never returns a mapping-backed frame
  slice.
- Python exposes immutable Camera value types and an asynchronous Camera
  session. Mapping descriptor representations redact identity and capability
  tokens; `BorrowedFrame.copy_bytes()` checks the session generation and becomes
  unusable on close.

## Verification

- Added protocol tag-locking and Camera transport conversion coverage.
- Added Python fake-broker Camera RPC coverage for enumerate, open, capture,
  mapping descriptor, next lease, controls, auto mode, and close.
- Added Rust public API coverage for `RemoteCameraHandle`.
- Ran focused Rust protocol, broker, and client test suites; Python bindings
  suite; formatting; and clippy for the changed Rust crates.

## Limitations

- This task does not claim a native camera adapter or physical-camera
  verification. The existing virtual/runtime camera and shared-memory ring are
  the exercised implementation boundary.
- The Rust client intentionally accepts a mapping descriptor separately when
  converting a wire frame lease, because the wire lease contains no mapping
  identity. This preserves the ring's identity fencing and prevents a
  descriptor-independent unsafe view.

## Task 5 defect follow-up

### API behavior

- The protocol decodes a Camera wire lease into `WireFrameLease`, which validates
  non-zero sequence and generation and retains only wire fields. A caller can
  bind it to an already-authenticated `MappingDescriptor`; protocol decoding
  never manufactures an identity-less public `FrameLease`.
- Rust validates mapping descriptors, next-frame leases, controls lists, and
  control-get values at the connection response boundary. Any invalid associated
  response terminates the client. `ReadOnlyMapping::slot_count()` exposes only
  validated layout metadata, and `RemoteCameraHandle::next_frame_lease` opens
  and validates the actual mapping layout before accepting the lease slot.
- Python removes `BorrowedFrame` from the public surface. `CameraSession.next_frame()`
  is the sole frame-borrow acquisition API; it tracks the verified descriptor and
  borrow epoch, invalidates prior borrows, and returns only copy access. As there
  is no native Python shared-memory reader, `copy_bytes()` fails closed with
  `shared_memory.unavailable` rather than accepting caller-provided callbacks.
  `next_frame_lease()` remains low-level control metadata only.

### TDD commands

- Red: `cargo test -p robot-hal-protocol camera_next_frame_lease_decoder_rejects_zero_sequence_and_preserves_validated_wire_fields`
  failed because zero sequence returned `Ok(None)`.
- Red: `cargo test -p robot-hal-client camera_response_validation_rejects_invalid_associated_payloads`
  failed because malformed Camera responses returned `Ok(())`.
- Red: `cargo test -p robot-hal-adapter-shared-memory independently_reopened_mapping_only_returns_an_owned_copy`
  failed to compile because validated mapping layout exposed no `slot_count`.
- Green: reran those focused commands successfully after the minimum fixes.

### Important defect fixes

- Rust `RemoteCameraHandle` now retains the mapping identity returned by its own
  `mapping_descriptor()` call. `open_mapping()` and `next_frame_lease()` reject
  a descriptor with any other identity as `runtime.argument.invalid` before
  admitting `camera_next_frame_lease_request`; cloning the current session's
  authenticated descriptor continues to work. The mapping remains copy-only.
- Python keeps the mapping-reader implementation internal. Without the built-in
  verified local reader, both public acquisition APIs (`next_frame()` and
  `next_frame_lease()`) now fail as `shared_memory.unavailable` before their
  lease RPC, so they cannot pin a broker slot that Python cannot read.

### Defect TDD commands

- Rust red:
  `cargo test -p robot-hal-client --test client_contract camera_rejects_another_session_mapping_before_frame_lease_admission`
  failed because the fake broker observed `CameraNextFrameLeaseRequest`.
- Rust green: reran the same command successfully after binding the accepted
  mapping identity to each `RemoteCameraHandle`.
- Python red:
  `uv run pytest tests/test_camera_contract.py -k 'frame_acquisition_without_a_local_mapping_reader'`
  failed because both public paths sent `camera_next_frame_lease_request`.
- Python green: reran the same command successfully after adding the local
  reader precondition.
- Coverage:
  `cargo fmt --all --check`;
  `cargo clippy -p robot-hal-client --tests -- -D warnings`;
  `cargo test -p robot-hal-client --test client_contract`;
  `cargo test -p robot-hal-protocol`;
  `cargo test -p robot-hal-adapter-shared-memory`; and
  `uv run pytest tests/test_camera_contract.py`.

## Python controls discovery follow-up

### API decision

- Python now exposes immutable `CameraControlDescriptor`, `ControlRange`, and
  `ControlEnumValues` values from `robot_hal`. A descriptor keeps the
  standardized `ControlKind`, access and auto flags, one validated range or
  enum domain, current-value availability, and an optional adapter diagnostic.
- `CameraSession.controls()` is the public discovery API. It delegates only to
  the private `HalClient._camera_controls()` RPC path; existing `get_control`,
  `set_control`, and `set_auto` behavior remains unchanged.
- Discovery requires negotiated minor 3 and `camera.controls/v1`. It validates
  every returned descriptor with the same contract as Rust/protobuf: at most
  four descriptors; a known kind; exactly one values representation; ordered
  ranges with positive steps; nonempty enumerations of at most 64 valid
  `ControlValue` values; ASCII diagnostics of at most 255 bytes; and no
  current value for an unreadable control. Invalid responses terminate the
  Python client through its established protocol-invalid-message path.

### TDD and verification

- Red:
  `uv run pytest tests/test_camera_contract.py -k 'control_discovery or fake_broker_wires_capture_mapping_lease_controls_and_close or malformed_camera_controls_response'`
  failed at collection because `CameraControlDescriptor` was absent from the
  public `robot_hal` API.
- Green: reran that command successfully after the minimum public models,
  controls RPC, and strict response conversion were added: 3 passed.
- Focused Camera coverage:
  `uv run pytest tests/test_camera_contract.py` — 8 passed.
- Python bindings suite:
  `uv run pytest tests` — 187 passed.
- Rust workspace formatting:
  `cargo fmt --all --check` — passed. No Rust code changed for this follow-up.
- Relevant edited-file lint diagnostics — no errors.

### Scope retained

- No Camera frame payload protobuf data, Python reader/native adapter, or
  unrelated cleanup was added.
