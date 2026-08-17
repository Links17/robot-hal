# Camera v0.4 Task 4 report

## Scope delivered

- Added `CameraAdapter` registration and finite Camera cleanup timeout to `HalRuntimeBuilder`.
- Added a per-physical-resource exclusive Camera manager with control leases, monotonically
  fenced generations, and quarantine until a blocking worker exits.
- Added a dedicated OS thread per opened Camera session. The thread owns native open, capture,
  controls, close, and the `BrokerMapping`; it uses a fixed 64-command Tokio MPSC queue and
  `Handle::block_on` only from that thread.
- Added runtime Camera capture, mapping descriptor, frame lease, dropped-count, and standard
  control APIs. Mapping descriptors use the shared-memory crate's redacted token behavior.
- Added virtual-camera runtime integration coverage for exclusivity, stale leases after reopen,
  shared-ring publication, controls, owner revocation, hot-unplug cleanup, and reopening.
- Hardened Camera timeout and terminal handling: an opening worker quarantines its reserved
  generation until it exits; owner revocation registers completion reapers for every target
  before waiting; terminal native-unavailable errors are retained for later session requests.
- Fenced close against queued native work and made shared-memory close publish a synchronized
  terminal header state before unlinking, so already-open readers reject old frame leases.

## Verification

- `cargo test -p seeed-hal-runtime --test camera_runtime`
- `cargo test -p seeed-hal-camera`
- `cargo test -p seeed-hal-testkit`
- `cargo test -p seeed-hal-adapter-shared-memory`
- `cargo fmt --all --check`
- `cargo clippy -p seeed-hal-runtime --all-targets --all-features -- -D warnings`

## Platform limits

This task validates the runtime seam with the deterministic virtual adapter only. Native
AVFoundation, V4L2, and Media Foundation qualification remains external hardware work. A native
operation that blocks past the configured close timeout retains the Camera lease and mapping
quarantine until its worker finishes; the runtime does not reopen the physical resource early.
