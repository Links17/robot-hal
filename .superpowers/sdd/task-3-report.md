# Task 3 Report: Wire-Minor Conformance Profiles

## Status

Implemented Task 3 only. Changes cover the conformance runner, its tests and README, plus the
test-only broker wiring and virtual CAN fixture needed to expose separate Classic-active and
FD-active resources for truthful mode qualification.

## Capability matrix source of truth

The matrix follows the current broker dispatch and crate constants rather than plan examples:

- minor 0: `serial.bytes/v1`
- minor 1 adds `can.classic/v1`, `can.fd/v1`, `can.configure/v1`,
  `can.error-frames/v1`, and `can.rx-timestamp/v1`
- minor 2 adds `usb.control/v1`, `usb.bulk/v1`, `usb.interrupt/v1`,
  `gpio.lines/v1`, and `gpio.edges/v1`
- minor 3 adds `camera.capture/v1`, `camera.frames.shm/v1`, and
  `camera.controls/v1`

The runner sends an exact minor offer by setting the legacy minor plus both range endpoints to the
same value, and requires the selected minor to equal that exact offer.

## TDD evidence

RED:

```text
uv run --project bindings/python --python 3.11 --frozen \
  pytest -q tests/conformance/test_runner_contract.py \
  tests/conformance/test_minor_matrix.py

12 failed, 11 passed
```

Failures were the intended missing CLI options, profile helpers, exact handshake parameters, and
later-minor probe selection.

An additional RED cycle captured the broker's real asymmetric dispatch names:

```text
1 failed, 7 passed
AttributeError: later_operation_error_for_minor
```

GREEN:

```text
24 passed in 0.19s
```

## Black-box profiles

Built the production broker entry point with virtual adapters:

```text
cargo build -p robot-hal-broker-app --features virtual-adapters
Finished `dev` profile
```

Actual profile results:

```text
broker conformance passed: protocol minor 0 profile
broker conformance passed: protocol minor 1 profile
broker conformance passed: protocol minor 2 profile
broker conformance passed: protocol minor 3 profile
```

Minor 0 probes CAN and requires `runtime.protocol.capability_unsupported`. Minor 1 probes USB and
minor 2 probes Camera; both require `runtime.protocol.unsupported_capability`. Minor 3 retains the
complete Serial/CAN/USB/GPIO/Camera lifecycle and control coverage. Camera frame bytes remain
outside protobuf.

## Verification

Final verification succeeded:

- `cargo fmt --all --check`
- Python `compileall` for conformance files
- runner contract and minor matrix tests: 24 passed
- minor 0, 1, 2, and 3 black-box profiles
- `git diff --check`
- IDE diagnostics: no linter errors

`ruff` was not available in the frozen Python environment, so no Ruff command was claimed as
passing.

## Self-review

- `--require-capability` is repeatable and, when supplied, replaces the profile defaults exactly;
  user requirements are not silently expanded.
- Every transport request, connection, process startup/readiness, shutdown, cleanup, and diagnostic
  drain remains bounded by existing deadlines.
- Lower profiles execute only operations available at or below their selected minor before one
  deliberate next-minor probe.
- No broker, protocol, plan, design, or Task 4 files were modified.

## Commit

Conventional Commit: `test(protocol): cover wire minor compatibility matrix`.

## Concerns

- The broker currently uses two stable names for minor gates: CAN returns
  `runtime.protocol.capability_unsupported`, while USB/GPIO and Camera return
  `runtime.protocol.unsupported_capability`. The runner intentionally asserts these exact current
  names rather than accepting arbitrary errors.
- Ruff is not installed in the frozen Python project environment; syntax compilation, pytest,
  Cargo formatting, IDE diagnostics, and whitespace checks were used instead.

## Important findings follow-up

Two review findings were fixed in a separate follow-up commit.

### TDD evidence

RED:

```text
5 failed, 23 passed
```

The failures proved that the handshake did not return negotiated capabilities, explicit profiles
did not select operations, and the later-minor rejection was not followed by a same-connection
request.

GREEN:

```text
29 passed
```

### Behavior and profile verification

- Explicit required capabilities now constrain execution to the intersection of the explicit set
  and handshake-advertised capabilities.
- USB control/bulk/interrupt and Camera capture/shared-memory/controls are independently selected.
- After a lower-minor structured rejection, the same `RawClient` enumerates Serial before close.
- Default minor 0, 1, 2, and 3 profiles all passed.
- An actual narrow minor 3 profile requiring only `usb.control/v1` passed without exercising
  Serial, CAN, GPIO, Camera, USB bulk, or USB interrupt.

### Follow-up commit

Conventional Commit: `fix(protocol): honor narrow conformance profiles`.

## Important findings second follow-up

The three remaining Important findings were addressed with executable lifecycle closures and
transport-specific disconnect cleanup.

### TDD evidence

RED:

```text
25 failed, 28 passed
```

The failures covered missing dependency closure, distinct CAN capability operations, active-profile
health request selection, and cleanup handles for CAN, USB, GPIO, and Camera.

GREEN:

```text
54 passed
```

### Black-box verification

- Default protocol minor 0, 1, 2, and 3 profiles passed.
- Explicit CAN Classic-only, USB Bulk (with automatically required USB Control), GPIO Lines-only,
  and Camera Capture-only profiles passed.
- Each narrow profile left its selected transport session open across abrupt disconnect and a
  second connection reopened and closed the same resource.
- Post-probe health checks now enumerate the active transport on the same connection.
- CAN Classic, FD, Configure, Error Frames, and RX Timestamp capabilities now have distinct
  executable virtual-fixture checks.

### Qualification boundary

Virtual CAN verifies deterministic loopback frames, error-frame capability handling/filtering, and
the presence of virtual receive timestamps. It does not qualify physical bus error injection,
hardware timestamp accuracy, or real adapter timing.

### Follow-up commit

Conventional Commit: `fix(protocol): complete narrow profile lifecycle`.

## Final Important finding follow-up

CAN FD qualification now follows the actual Attach contract rather than requiring Configure and
Classic.

### TDD evidence

RED:

```text
2 failed
```

The failures proved that FD-only closure incorrectly added Configure and Classic, and that the FD
open request used a Maintenance Configure request instead of a Control Attach(FD) request.

GREEN:

```text
56 passed
```

### Verification

- `can.fd/v1` has no lifecycle dependency on `can.configure/v1` or `can.classic/v1`.
- FD open uses Control lease plus `Attach(mode=FD)` and sends/receives an FD frame.
- Configure remains a distinct Classic Configure qualification.
- The virtual broker fixture starts with an FD-active CAN loopback so an actual FD-only Attach
  profile is executable without silently configuring the link.
- Default minor 0, 1, 2, and 3 profiles passed.
- An explicit minor 1 FD-only profile passed, including abrupt disconnect cleanup and same-resource
  reopen/close.

### Follow-up commit

Conventional Commit: `fix(protocol): qualify CAN FD via attach`.

## Dual-mode virtual CAN follow-up

The virtual broker now exposes separate Classic-active and FD-active CAN resource identities, so
each mode is qualified with an explicit matching Attach expectation.

### TDD evidence

RED:

```text
3 failed
```

The failures proved that Classic, Error Frames, and RX Timestamp checks used an empty Attach
expectation and therefore did not prove a Classic-active link.

GREEN:

```text
59 passed
```

### Verification

- Classic, Error Frames, and RX Timestamp explicitly use `Attach(mode=CLASSIC)` on the stable
  Classic virtual resource.
- FD-only explicitly uses `Attach(mode=FD)` on the stable FD virtual resource and has no
  Classic/Configure closure.
- Configure uses Maintenance Configure(Classic) on the Classic resource and restores its prior
  state when closed.
- Disconnect cleanup reopens and closes the same selected resource identity.
- Default minor 0, 1, 2, and 3 profiles passed.
- Explicit Classic-only, FD-only, Error Frames, and RX Timestamp profiles passed.

### Follow-up commit

Conventional Commit: `fix(protocol): qualify both virtual CAN modes`.

---

# Task 3 addendum: split three-platform CI conformance

## Outcome

`source-gate` retains only platform-independent generated-protocol, Rust,
Python, protocol-minor, and release checks. It does not install Linux native
prerequisites or compile platform adapter feature sets.

`platform-conformance` derives its macOS, Linux, and Windows matrix from
`release/targets.toml`. Every matrix entry builds and verifies its production
broker manifest, then separately builds a virtual-adapters broker and records
virtual protocol conformance for minors 0–3. The production and virtual
commands run under Python so Windows does not depend on Git Bash `shasum`,
`chmod`, or archive tooling.

## RED

```sh
uv run --project bindings/python --python 3.11 --frozen pytest -q \
  tests/release/test_workflow_contract.py
```

Result: **1 failed, 21 passed**. The failing contract proved that the two
platform build steps used Bash rather than Windows-compatible Python release
tooling.

## GREEN and local verification

```sh
uv run --project bindings/python --python 3.11 --frozen pytest -q \
  tests/release/test_workflow_contract.py
uv run --project bindings/python --python 3.11 --frozen pytest -q tests/release
cargo +1.85 fmt --all --check
cargo +1.85 clippy --workspace --all-targets --no-default-features -- -D warnings
cargo +1.85 test --workspace --no-default-features
./scripts/check-generated-protocol.sh
```

- Workflow contract tests: **22 passed**.
- Release tests: **212 passed**.
- Rust formatting and no-default-features clippy: passed.
- The final complete no-default-features Rust test run passed. An earlier run
  transiently failed `gpio_cancelled_queued_read_does_not_start_native_io`
  with `runtime.session.close_timeout`; its isolated rerun and final full run
  passed. Task 3 does not modify GPIO runtime code.
- Generated protocol check: passed.

## Hosted evidence prerequisite

No workflow was pushed, dispatched, or remotely triggered for Task 3. Hosted
macOS, Linux, and Windows evidence remains pending. After implementation review
and push, the controller must obtain one real GitHub Actions run showing all
three platform matrix entries green, each with production manifest verification
and virtual protocol-minor coverage 0–3. Only then may the qualification record
include a run URL, commit, and Passed hosted evidence.

## Important review follow-up: source-gate package isolation

The prior source-gate Rust commands used `--workspace --no-default-features`.
That is not platform isolation: Cargo still selects every workspace member,
including native adapter packages such as Linux GPIO and V4L2, and can therefore
compile their native ABI surfaces.

### TDD evidence

RED:

```text
1 failed, 22 passed
AssertionError: '--workspace' is contained here:
cargo +1.85 clippy --workspace --all-targets --no-default-features -- -D warnings
```

GREEN:

```text
23 passed in 0.19s
```

The workflow contract now rejects workspace-wide source-gate Rust commands and
requires both Clippy and tests to enumerate the platform-neutral public
libraries explicitly: client, broker, CAN, camera, core, GPIO, protocol,
runtime, serial, testkit, and USB. It also rejects all native adapter package
names and the broker application from source-gate commands.

Native adapter and production broker-app builds remain exclusively in the
`platform-conformance` matrix, which derives the macOS, Linux, and Windows
targets from `release/targets.toml`.

### Verification

- Workflow contract: **23 passed**.
- Full release suite: **213 passed**.
- Explicit platform-neutral Rust package set: `cargo +1.85 clippy --all-targets
  ... -- -D warnings` and `cargo +1.85 test ...` passed.
- `cargo +1.85 fmt --all --check` passed.
- `./scripts/check-generated-protocol.sh` passed.
- No hosted workflow was pushed, dispatched, or triggered.

## Review follow-up: precise source-gate dependency boundary

The source gate is not defined by the absence of every OS API. Its purpose is
to isolate build requirements that depend on external Linux native packages or
compile hardware device adapters, while retaining the production runtime
closure. This matches the architecture: the runtime owns the bounded
shared-memory camera data plane, and the broker uses Windows local-IPC security
when compiled for Windows.

`robot-hal-adapter-shared-memory` is therefore allowed through
`robot-hal-runtime`; its Windows implementation uses Windows memory/security
APIs, but its manifest has no Linux-target dependency and no `pkg-config`,
`libgpiod`, or `libudev` prerequisite. `robot-hal-windows-security` is likewise
allowed through the broker's `cfg(windows)` dependency, because it provides
Windows named-pipe/file security rather than a hardware adapter and has no
Linux native prerequisite.

The excluded closure remains the hardware adapter and production application
set: AVFoundation, Linux GPIO/libgpiod, Media Foundation, nusb, PCAN,
serialport, SocketCAN, V4L2, Windows GPIO, and `robot-hal-broker-app`. Those
build only in `platform-conformance`; Linux package provisioning stays there
with the libgpiod/libudev `pkg-config` preflight.

### TDD evidence

RED:

```text
1 failed, 23 passed
StopIteration: no step named
"Run Linux-prerequisite-free Rust clippy ..."
```

The new workflow contract required source-gate names to describe the actual
boundary and rejected the inaccurate `platform-neutral` label. It also checked
that the permitted shared-memory and Windows-security manifests do not declare
external Linux native prerequisites.

GREEN:

```text
24 passed in 0.18s
```

The renamed workflow steps now state that they run only the runtime closure
requiring no external Linux native prerequisites, and a nearby comment records
why host-specific shared-memory and Windows-security implementations remain in
that closure. No hosted workflow was pushed, dispatched, or triggered.
