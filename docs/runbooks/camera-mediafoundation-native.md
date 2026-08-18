# Windows Media Foundation Camera qualification

The Media Foundation adapter provides standard discovery, exclusive capture, exact NV12, YUYV, or
MJPEG negotiation, lifecycle cleanup, and the broker shared-memory data plane. It currently does
not advertise standardized controls; controls must fail closed.

This is an external hardware qualification gate. A non-Windows test or `x86_64-pc-windows-gnu`
cross-check validates Rust type paths only; it does not exercise Media Foundation, privacy consent,
or a camera driver.

## Preconditions

- Run on Windows with a disposable camera attached and Camera privacy permissions granted to the
  test process.
- Use the HAL-discovered resource ID. Do not commit a symbolic link, camera serial number, mapping
  name, capability token, or raw frame data.
- Ensure another application does not hold the fixture and that the fixture can be unplugged safely.

## Run

```powershell
$env:SEEED_HAL_CAMERA_RESOURCE_ID = 'camera:mediafoundation:<enumerated-id>'
cargo test -p seeed-hal-adapter-mediafoundation --features hardware-tests -- `
  --ignored --nocapture
Remove-Item Env:SEEED_HAL_CAMERA_RESOURCE_ID
```

Record the Windows release/build, architecture, adapter crate version, redacted HAL resource ID,
identity quality, capabilities, tested formats, command output location, and timestamp in
[`v0.4.0-camera-qualification.md`](../releases/v0.4.0-camera-qualification.md).

## Required observations

1. Discovery derives identity from the Media Foundation capture symbolic-link evidence without
   depending on enumeration order.
2. For each available common format among NV12, YUYV, and MJPEG, exact open and capture report the
   requested format/dimensions; unsupported and over-limit requests fail closed.
3. A second session conflicts while the first is active. Close/reopen supplies a greater lease
   generation and rejects the previous lease.
4. Capture returns a bounded payload with nonzero sequence and monotonic timestamp. A held pin never
   blocks capture: verify latest-wins/drop accounting and that the pinned slot stays intact.
5. `camera.controls/v1` is absent and every control operation produces a stable unsupported error.
6. The broker passes only shared-memory descriptors over protobuf. Verify the protected mapping opens
   only for the authorized process; do not print descriptor credentials.
7. Safely unplug the fixture during capture, confirm the session is terminal, then reconnect and
   reopen it. Verify close timeout quarantines the claim until the Media Foundation worker exits.

Return the fixture to its normal state. Record privacy denial, driver limits, unavailable formats,
and hot-unplug behavior exactly as observed; none authorize fallback or fabricated control support.
