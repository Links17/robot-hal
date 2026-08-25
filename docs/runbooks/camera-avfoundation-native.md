# macOS AVFoundation Camera qualification

The AVFoundation adapter provides standard camera discovery, exclusive capture, exact negotiated
NV12 or YUYV frames, lifecycle cleanup, and the broker shared-memory data plane. It does not claim
MJPEG support and does not advertise standardized controls. It must fail closed for either case.

This is an external hardware qualification gate. The default suite and a cross-platform build do
not qualify a physical macOS camera.

## Preconditions

- Run on macOS with a non-production camera selected in System Settings.
- If Camera access is `NotDetermined` for the test process, the adapter requests access once and waits for the system prompt. Grant it before continuing. Denied or restricted access fails closed.
- Use the resource ID returned by HAL discovery. Do not record a camera serial number, mapping name,
  capability token, raw frame, or image in source control or the qualification evidence.
- Ensure no other process is using the fixture. The test user must be permitted to access the
  camera.

## Run

For non-hot-unplug checks:

```bash
export SEEED_HAL_CAMERA_RESOURCE_ID='camera:avfoundation:<enumerated-id>'
cargo test -p seeed-hal-adapter-avfoundation --features hardware-tests -- \
  --ignored --nocapture
unset SEEED_HAL_CAMERA_RESOURCE_ID
```

For the supervised hot-unplug test (requires physical unplug/replug during the run):

```bash
# Auto-selects the active device from cameras named '1080P USB Camera'.
./scripts/run-avfoundation-hot-unplug.sh

# Or specify a specific resource ID explicitly:
SEEED_HAL_CAMERA_RESOURCE_ID='camera:avfoundation:<id>' \
  ./scripts/run-avfoundation-hot-unplug.sh
```

Frames are published only through `capture_into` and the shared-memory sink. `capture()` must
fail closed. Open the device's current exact NV12 or YUYV active format; the adapter does not
switch to a different advertised size.

Record the OS release, architecture, adapter crate version, the redacted HAL resource ID, identity
quality, advertised capabilities, selected format, command output location, and timestamp in
[`v0.4.0-camera-qualification.md`](../releases/v0.4.0-camera-qualification.md).

## Required observations

1. Discovery returns the selected physical identity and a transient endpoint separately.
2. Open the supported exact NV12 or YUYV format. A second open conflicts until the first closes.
3. Capture a frame; verify its negotiated format, nonzero sequence, bounded payload, and monotonic
   timestamp provenance. Request MJPEG and unsupported dimensions separately; both must fail closed.
4. The adapter does not advertise `camera.controls/v1`; descriptor, get, set, and auto operations
   must return the stable unsupported error rather than synthesize controls.
5. Through a broker client, request the mapping descriptor and verify that frame bytes are not
   carried in a protobuf response. Verify only the mapping protection outcome; never log its name
   or token.
6. Hold one frame lease, continue capture, and verify capture does not block: frames are
   latest-wins or increment `dropped_count`; the pinned slot is never overwritten.
7. Disconnect the fixture during capture when safe. Verify:
   - The session terminates with `camera.session.unplugged` (not merely `runtime.transport.timeout`).
   - **Platform observation (macOS UVC):** after physical unplug, macOS may retain a phantom device
     entry in `devicesWithMediaType:`. This is a known platform behavior and is not treated as a
     qualification failure; the adapter detects true disconnection via `AVCaptureDeviceWasDisconnectedNotification`
     and `AVCaptureSessionRuntimeErrorNotification` on a private `NSOperationQueue` (bypasses main
     `CFRunLoop`). Phantom entries are identified during session open by a 2-second frame-readiness
     probe: a device that enumerates but produces no frames within that window returns
     `camera.open` error and the caller retries with the next candidate.
   - After reconnect, the adapter re-enumerates, opens (with implicit frame-readiness probe),
     and the test captures a first frame within a 30-second window.
   - See `scripts/run-avfoundation-hot-unplug.sh` for supervised execution, including automatic
     active-device selection when multiple cameras share the same localizedName.
8. Close, reconnect, and verify a new lease generation fences the old lease.

Return the fixture to its normal privacy and physical state. A failed or unavailable check is a
recorded qualification result, not permission to silently fall back to another adapter or format.
