# macOS AVFoundation Camera qualification

The AVFoundation adapter provides standard camera discovery, exclusive capture, exact negotiated
NV12 or YUYV frames, lifecycle cleanup, and the broker shared-memory data plane. It does not claim
MJPEG support and does not advertise standardized controls. It must fail closed for either case.

This is an external hardware qualification gate. The default suite and a cross-platform build do
not qualify a physical macOS camera.

## Preconditions

- Run on macOS with an authorized, non-production camera selected in System Settings.
- Use the resource ID returned by HAL discovery. Do not record a camera serial number, mapping name,
  capability token, raw frame, or image in source control or the qualification evidence.
- Ensure no other process is using the fixture. The test user must be permitted to access the
  camera.

## Run

```bash
export SEEED_HAL_CAMERA_RESOURCE_ID='camera:avfoundation:<enumerated-id>'
cargo test -p seeed-hal-adapter-avfoundation --features hardware-tests -- \
  --ignored --nocapture
unset SEEED_HAL_CAMERA_RESOURCE_ID
```

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
7. Disconnect the fixture during capture when safe, then verify the session becomes terminal and
   the resource can be re-enumerated/reopened after cleanup.
8. Close, reconnect, and verify a new lease generation fences the old lease.

Return the fixture to its normal privacy and physical state. A failed or unavailable check is a
recorded qualification result, not permission to silently fall back to another adapter or format.
