# Task 3 Report: Wire-Minor Conformance Profiles

## Status

Implemented Task 3 only. Changes are limited to the conformance runner, its tests, and conformance
README, plus this required report.

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
cargo build -p seeed-hal-broker-app --features virtual-adapters
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
