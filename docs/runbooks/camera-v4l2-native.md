# Linux V4L2 Camera qualification

The V4L2 adapter provides standard camera discovery, exclusive capture, exact NV12, YUYV, or MJPEG
negotiation, lifecycle cleanup, and the broker shared-memory data plane. It currently does not
advertise standardized controls; control requests must fail closed.

This is an external hardware qualification gate. A macOS cross-check without Linux kernel headers,
or the default ignored-test suite, is not Linux camera evidence.

## Preconditions

- Run on Linux with a disposable V4L2 capture fixture connected.
- Grant the test user narrowly scoped read/write access to the selected `/dev/video*` node, normally
  through a udev rule. Do not weaken permissions globally.
- Use the HAL resource ID returned by discovery; do not record raw `/dev/video*` paths, serial
  values, mapping names, capability tokens, or frame data in the evidence.
- Ensure the fixture can be disconnected safely for the hot-unplug check.

## Run

```bash
export SEEED_HAL_CAMERA_RESOURCE_ID='camera:v4l2:<enumerated-id>'
cargo test -p seeed-hal-adapter-v4l2 --features hardware-tests -- \
  --ignored --nocapture
unset SEEED_HAL_CAMERA_RESOURCE_ID
```

Record the OS/kernel release, architecture, adapter crate version, redacted HAL resource ID,
identity quality, capabilities, tested format matrix, command output location, and timestamp in
[`v0.4.0-camera-qualification.md`](../releases/v0.4.0-camera-qualification.md).

## Required observations

1. Discovery persists physical identity independently of the current `/dev/video*` endpoint.
2. For every camera-advertised common format among NV12, YUYV, and MJPEG, exact open/capture returns
   matching format and dimensions. Unsupported formats and over-limit dimensions fail closed.
3. A concurrent open conflicts until close has completed; a post-close open receives a new fenced
   lease generation.
4. Capture has a nonzero sequence, bounded payload and monotonic timestamp. While a frame remains
   pinned, ongoing capture remains non-blocking and reports latest-wins replacement or increasing
   `dropped_count` without overwriting the pin.
5. The adapter does not advertise `camera.controls/v1`; all controls operations return stable
   unsupported errors rather than exposing driver-specific semantics.
6. Broker shared-memory descriptors carry no payload bytes over protobuf and validate local mapping
   access protections. Do not retain or record mapping names/tokens.
7. Unplug during capture if the fixture permits it. Verify terminal cleanup, then re-enumeration and
   reopen once the device returns. Capture cancellation/close must not leave the native claim
   reusable before its worker exits.

Restore the fixture and permissions after testing. Record unavailable formats and driver limitations
as results; do not substitute another format or software conversion.
