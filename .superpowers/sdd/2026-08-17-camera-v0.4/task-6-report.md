# Camera v0.4 Task 6 report

## Linux V4L2 adapter slice

- Added `seeed-hal-adapter-v4l2` to the root workspace. The crate uses the
  current `v4l` 0.14 safe wrapper, whose `Stream::with_buffers` implements
  kernel V4L2 mmap buffer allocation, queue/dequeue, streaming start/stop, and
  unmapping. The crate's dependency graph resolved with Rust 1.85-compatible
  package versions.
- On Linux, discovery scans actual `/dev/video*` endpoints, verifies V4L2
  capture plus streaming capability, and retains the `/dev/video*` node only as
  a transient endpoint. Stable identity prefers an ancestor sysfs `serial`
  value (`Strong`); when unavailable it uses the canonical sysfs device path
  (`Medium`) and never represents it as `Strong`.
- Open re-enumerates and resolves the selected identity, then applies the
  requested pixel format and dimensions through V4L2. It accepts only an exact
  NV12, YUYV, or MJPEG result with bounded `sizeimage` and valid stride. One
  adapter instance permits exactly one active session per resource.
- Capture owns a dedicated native worker thread. It holds the V4L2 device and
  mmap stream there, validates every dequeued payload and layout, copies the
  native bytes once into the owned `CameraFrame`, creates a receipt monotonic
  timestamp, and publishes strict adapter sequence numbers. `close` terminates
  the worker, drops the stream/device and releases the claim. Controls are not
  advertised and every control operation fails closed.
- Non-Linux calls return `runtime.adapter.unavailable` without native
  discovery. A feature-gated ignored hardware test requires
  `SEEED_HAL_CAMERA_RESOURCE_ID`.

## TDD evidence

- Red:
  `cargo test -p seeed-hal-adapter-v4l2 --all-features` initially failed because
  `V4l2Adapter::enumerate` was intentionally unimplemented and panicked instead
  of returning the stable non-Linux unavailable error.
- Green:
  after adding platform gating and the identity encoder, the same command
  passed the non-Linux unavailable and identity encoding tests.
- The Linux implementation was then added behind `cfg(target_os = "linux")`;
  its identity evidence helpers are unit-tested when compiled on Linux, and
  its physical capture test is `hardware-tests`-gated and ignored.

## Verification

- Passed:
  `cargo fmt --all --check`
- Passed:
  `cargo clippy -p seeed-hal-adapter-v4l2 --all-targets --all-features -- -D warnings`
- Passed:
  `cargo test -p seeed-hal-adapter-v4l2 --all-features`
- Attempted Linux target check:
  `cargo check -p seeed-hal-adapter-v4l2 --target x86_64-unknown-linux-gnu`
  could not build in this macOS environment because the `v4l2-sys-mit` build
  script requires the Linux kernel header `linux/videodev2.h`, which is absent.

## Limitations

- No physical Linux camera or Linux kernel sysroot was available here, so the
  ignored `hardware-tests` capture qualification was not run.
- This slice deliberately does not advertise controls. It does not map V4L2
  controls until their standard descriptors and operations can be implemented
  end-to-end without fabrication.
- The safe `v4l` wrapper owns its internal mmap unsafe boundary. The adapter
  adds no direct `unsafe`; the kernel streaming contract is the documented
  V4L2 mmap `REQBUFS`/`QUERYBUF`/`QBUF`/`DQBUF`/`STREAMON` sequence.

## Teardown remediation

- Root cause: the initial capture worker called `Stream::next()` with its
  default infinite poll wait. `CameraCaptureSession::capture` timed out only
  while awaiting the oneshot reply; its native worker therefore remained
  blocked and could not consume `Close`, leaving the join, mmap buffers, file
  descriptor, and adapter claim indefinitely occupied.
- Capture now supplies an absolute deadline to the native worker. The worker
  uses the safe `v4l` stream timeout (which is backed by the already
  non-blocking V4L2 descriptor and `poll`) in a 20 ms bounded wait loop. A
  poll timeout is reported as stable `runtime.transport.timeout`; streaming is
  stopped after that timeout so the next capture cleanly requeues all mmap
  buffers and may proceed.
- Close raises an atomic shutdown fence before attempting its bounded command
  send. The same 20 ms wait loop observes that fence, exits the only owner
  thread, and drops the stream/device on that thread. Joining remains in
  `spawn_blocking` and is itself limited to one second. No Tokio executor
  worker blocks and no thread is force-killed.
- Non-timeout V4L2 errors, including unplug/driver errors, remain mapped to
  `runtime.transport.unavailable`. Native buffer allocation, queue/dequeue,
  requeue, and unmapping remain serialized in the capture worker.

## Teardown TDD evidence

- Red:
  `cargo test -p seeed-hal-adapter-v4l2 --all-features` failed two new
  portable wait-helper tests before the implementation: one observed an
  unbounded native wait exceeding the capture deadline; the other observed a
  stalled capture returning timeout instead of yielding to shutdown.
- Green:
  after the bounded deadline loop and shutdown fence, the same test target
  passed all six tests, including a timed-out capture followed by a successful
  next capture.

## Teardown verification and limitation

- Passed:
  `cargo fmt --all --check`
- Passed:
  `cargo clippy -p seeed-hal-adapter-v4l2 --all-targets --all-features -- -D warnings`
- Passed on this non-Linux macOS host:
  `cargo test -p seeed-hal-adapter-v4l2 --all-features`
- Linux target checking remains unavailable here. The macOS toolchain reaches
  `v4l2-sys-mit` but cannot generate bindings because the Linux kernel header
  `linux/videodev2.h` is absent. No Linux kernel sysroot or physical V4L2
  camera was available to exercise the ignored hardware test.
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
