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
