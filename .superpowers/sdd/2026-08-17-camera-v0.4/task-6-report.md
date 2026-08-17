# Camera v0.4 Task 6 report

## Native adapter prerequisite

Camera adapter conformance now reads the selected descriptor's
`camera.controls/v1` capability before validating controls. An adapter that
advertises the capability continues to be checked for discovery and readable
exposure, gain, white-balance, and focus controls. An adapter without it is
checked to ensure discovery, get, set, and auto all fail closed with the
stable `camera.control.unsupported` / `InvalidArgument` error instead of
pretending that controls exist.

`VirtualCameraAdapter::capture_only` is a minimal native-style fixture. It
advertises only `camera.capture/v1` and `camera.frames.shm/v1`, produces
normal captures, and rejects every control operation. The existing
`VirtualCameraAdapter::pattern` retains its complete four-control coverage.
Capture negotiation, frame metadata/layout checks, exclusive session handling,
close behavior, and hot-unplug coverage were not relaxed.

## TDD evidence

- Red:
  `cargo test -p seeed-hal-testkit --test camera_conformance capture_only_camera_passes_public_adapter_conformance`
  first failed because the capture-only fixture did not exist; after adding
  only that fixture, it failed at the old runner's unconditional assertion
  that all four control descriptors were present.
- Green:
  `cargo test -p seeed-hal-testkit --test camera_conformance capture_only_camera_passes_public_adapter_conformance`
  passed after capability-gating the runner and enforcing unsupported control
  operations for the capture-only fixture.
- Regression:
  `cargo test -p seeed-hal-testkit --test camera_conformance virtual_camera_passes_public_adapter_conformance`
  passed, retaining the full-controls branch.

## macOS AVFoundation adapter slice

- Added the workspace package `seeed-hal-adapter-avfoundation`, with
  target-specific official `objc2` AVFoundation, CoreMedia, CoreVideo,
  Foundation, and dispatch bindings pinned to the Rust 1.85-compatible
  `0.3.1` binding line.
- On macOS, discovery calls AVFoundation for actual video devices. Resource IDs
  safely percent-encode `AVCaptureDevice.uniqueID`; the AVFoundation device
  endpoint is retained only as a transient property. Descriptors advertise
  `camera.capture/v1` and `camera.frames.shm/v1`, never controls.
- Opening re-enumerates and resolves the selected strong identity, checks
  connection and camera authorization, requests device-native samples, and
  fences one active adapter session per resource with
  `runtime.adapter.conflict`.
- The serial AVFoundation delegate copies a delivered read-locked pixel buffer
  once into a latest-frame mailbox. Publication requires exact requested
  dimensions and verified native NV12 or YUYV layouts; unsupported output,
  including MJPEG in this binding slice, fails closed as
  `camera.format.unsupported`. Closing detaches the delegate, stops and tears
  down the capture graph, releases the claim, and subsequent methods return
  `runtime.session.closed`.
- Non-macOS calls return `runtime.adapter.unavailable` without invoking native
  discovery. All control calls return `camera.control.unsupported`.

## Verification

- `cargo fmt --all --check`
- `cargo clippy -p seeed-hal-adapter-avfoundation --all-targets --all-features -- -D warnings`
- `cargo test -p seeed-hal-adapter-avfoundation --all-features`
  - deterministic identity tests passed;
  - hardware test is feature-gated and ignored, requiring an authorized camera
    plus `SEEED_HAL_CAMERA_RESOURCE_ID`.
- `cargo test -p seeed-hal-testkit --test camera_conformance capture_only_camera_passes_public_adapter_conformance -- --exact`
- `cargo fmt --all --check && cargo clippy --workspace --all-targets --all-features -- -D warnings && cargo test --workspace --all-features`

## Limitations

- The hardware test was not run because this verification did not provide a
  selected, authorized physical camera.
- `objc2` 0.3.1 exposes no ergonomic binding path for safely representing
  AVFoundation compressed MJPEG output through this capture delegate; the
  adapter explicitly rejects MJPEG instead of converting or mislabeling it.
- The compatible bindings' CoreVideo dictionary-key types cannot be safely
  expressed as `NSDictionary` keys, so this slice requests device-native
  samples and verifies the requested NV12/YUYV format and dimensions at
  publication time. A device that cannot provide an exact match fails closed
  on capture with `camera.format.unsupported`; supported-format preflight at
  open remains future work.
- AVFoundation device discovery uses the binding's deprecated
  `devicesWithMediaType:` API because the pinned `objc2` release does not
  expose a usable discovery-session path. This is isolated and documented for
  replacement when the compatible binding line adds it.
