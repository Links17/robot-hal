# Task 6 report: AVFoundation direct frame sink

## Implementation

- Added `CameraFrameSink` and `CameraCaptureSession::capture_into`, retaining the existing
  `capture` implementation as the compatibility default for adapters that still materialize a
  `CameraFrame`.
- Changed the runtime camera worker to pass a broker-owned sink to sessions. The sink selects a
  shared-memory slot and invokes the adapter-provided copier directly against that slot.
- Added `SlotWriter::publish_with`; it validates the copier's returned byte count before marking
  the slot ready. The legacy byte-slice publisher delegates to it.
- Moved AVFoundation capture graph ownership into a dedicated native worker. Its delegate locks
  `CVPixelBuffer` read-only and copies the planes directly into the selected shared-memory slot;
  it does not construct a full-payload `Vec`, `Bytes`, or `CameraFrame`.
- Capture requests are one-shot worker commands. Each request has an ID, deadline, sink, and
  response channel. Capture timeout sends a cancellation command; the native worker removes the
  matching pending request before a later callback can use its sink.
- Native close detaches the delegate, stops the session, removes graph nodes, and drains the
  serial callback queue. A timed-out worker join retains the adapter claim and a background
  reaper releases it only after the worker exits.

## Tests

- Added a shared-memory test proving a producer copier executes exactly once while writing the
  selected ring slot.
- Added an AVFoundation claim-quarantine test proving a second open remains conflicting until a
  simulated native worker exits.
- Runtime camera tests continue to cover capture publication, close fencing, and lease
  generation behavior.

## Verification

Verification is recorded with the final implementation task after the complete workspace gates
finish.
