# Task 9 report: Linux SocketCAN adapter

## Status

Implemented the Linux SocketCAN platform adapter and an explicit non-Linux
unavailable stub.

## Files

- Added `adapters/socketcan/Cargo.toml`.
- Added `adapters/socketcan/src/{lib,identity,channel,link}.rs`.
- Added hardware-free identity metadata tests and ignored native `vcan` tests.
- Registered the adapter and pinned backend dependencies in the workspace
  `Cargo.toml`.

## Behavior implemented

- Discovery uses `socketcan::available_interfaces()` with its udev enumeration
  feature, then queries netlink details without shelling out to `ip`.
- Identity precedence is hardware serial (Strong), stable sysfs device path or
  topology (Medium), then interface endpoint (Weak). Every virtual interface is
  forced to an endpoint-derived Weak identity. Identity segments are
  percent-encoded. Descriptors and default properties never include serial or
  topology values.
- Linux discovery/open work is moved through `tokio::task::spawn_blocking`, so
  udev, netlink, and native socket opening do not run on Tokio executor workers.
  Runtime channel methods remain synchronous for the existing dedicated CAN
  actor and clamp each receive poll to 100 ms.
- Attach only queries state and validates every supplied expectation. Configure
  snapshots link state, MTU/mode, nominal/data timing, restart delay, listen-only,
  and loopback; applies the writable set in one netlink parameter update while
  down; restores the prior up/down state; and re-queries before exposing the
  channel.
- Configure close compare-checks the current link fingerprint before restoring
  the snapshot. External changes fail closed instead of being overwritten.
  Netlink permission, unsupported configuration, absent-resource, and busy
  errno values become stable structured resource-scoped errors.
- Classical and FD transmission uses the private `can-hal-socketcan` backend.
  A separate bounded raw SocketCAN receiver preserves remote frames and maps
  Linux error class bits if the kernel delivers an error frame. Remote sends
  use a separate raw sender so loopback reception does not require enabling
  receive-own-message behavior on the receive socket.
- Capabilities are conservative: Classic is always present, FD follows current
  link MTU, and Configure is omitted for virtual interfaces. Error-frame and RX
  timestamp capabilities are not advertised because the pinned backend does
  not provide the complete required delivery/timestamp contract. Backend types
  never cross the adapter boundary.
- Non-Linux builds do not select `can-hal-rs`, `can-hal-socketcan`, `socketcan`,
  `neli`, `libc`, or `libudev`; the adapter returns
  `runtime.adapter.unavailable`.

## Dependency audit

| Dependency | Version | License | Declared MSRV | Selected features |
|---|---:|---|---:|---|
| `can-hal-rs` | `0.4.2` exact | MIT OR Apache-2.0 | 1.81 | defaults off; `std` only |
| `can-hal-socketcan` | `0.4.2` exact | MIT OR Apache-2.0 | 1.81 | no crate features; defaults explicitly off |
| `socketcan` | `3.5.0` exact | MIT | 1.70 | direct defaults off; `netlink`, `enumerate` |
| `neli` | `0.6.5` exact | BSD-3-Clause | not declared | defaults empty; used for stable errno mapping |

`can-hal-socketcan 0.4.2` itself depends on `socketcan 3.5` with upstream
defaults, so Cargo feature unification also selects `socketcan`'s `dump`
library module. The CLI/application `utils`, Clap, Tokio, async-std, async-io,
and smol features remain disabled. `enumerate` brings in `libudev` only on the
Linux target.

All audited declared MSRVs are below the workspace Rust 1.85 MSRV. No unsafe
code was added to the adapter; native unsafe remains encapsulated by the
audited backend crates.

## Tests written but not run

- Strong/Medium/Weak identity precedence, percent encoding, endpoint
  separation, and mandatory Weak `vcan` identity.
- Ignored Linux-native Classical attach/send/receive/close coverage.
- Ignored FD/configuration qualification placeholder for a provisioned native
  environment.
- Ignored invocation of the shared CAN adapter conformance suite.

Per task instruction, no tests, builds/checks, Clippy, rustfmt, cross-builds,
or Cargo dependency resolution were run. `Cargo.lock` was not changed.

## Static review

- `git diff --check` completed with no output.
- Reviewed the adapter source and public test surface against the Task 1 CAN
  contract, Task 9 brief, and backend source for the exact pinned versions.
- No product/device protocol concepts, unbounded queues, shell configuration,
  native-handle exposure, global mutable state, or adapter-local unsafe code
  were introduced.

## Remaining qualification

Native compile/lint/test confirmation, non-Linux cross-build confirmation,
`vcan` lifecycle/configuration execution, real-controller timing and bus-state
behavior, and hardware timestamp/error-frame qualification remain deliberately
deferred to the owning integration gate.

## Fix round 1

- Made Configure transactional across netlink apply, post-apply query,
  active-state conversion, configuration verification, and channel socket open.
  Every failure path attempts a verified restore; a failed restore is returned as
  resource-scoped `can.configuration.rollback_failed` with the primary and
  rollback error names in structured context.
- Kept the link snapshot and applied fingerprint until restore and re-query both
  succeed. Failed close remains retryable by the caller, and drop performs a
  conflict-checked best-effort retry without overwriting externally changed link
  state.
- Forced both backend data transmission and the raw remote-frame sender into
  nonblocking mode before channel exposure. `EAGAIN`/`WouldBlock` and `ENOBUFS`
  now return retryable, resource-scoped `runtime.queue.full` instead of allowing
  an unbounded write or misclassifying backpressure as a timeout.
- Removed the fabricated 500 kbit/s fallback. Attach and Configure now fail with
  a structured, resource-scoped error when the kernel omits required nominal or
  FD data timing. Post-config verification checks exact requested bitrate,
  optional sample point, optional SJW, restart delay, mode, listen-only, and
  loopback state.
- Expanded compare/restore state to include raw nominal and data timing, all nine
  SocketCAN control modes, restart delay, MTU/up state, and termination. Restore
  always takes the current link down before applying writable snapshot state and
  preserves exact errno classification.
- Removed interface-name-prefix virtual detection. Virtual classification now
  comes only from the canonical sysfs path. Configure is advertised only with
  affirmative nonvirtual sysfs evidence and kernel nominal timing constants; FD
  requires consistent active FD state or kernel data-timing constants. The
  descriptor `mode` property now describes active state rather than capability.
- Replaced placeholder native tests with ignored, uniquely named netlink-created
  `vcan` fixtures covering sysfs-only virtual classification, Weak identity,
  conservative capabilities, missing-timing Attach errors, and deletion errors.
  Shared conformance now filters to the explicitly selected real interface from
  `SEEED_HAL_SOCKETCAN_CONFORMANCE_INTERFACE`. Added focused static unit coverage
  for Classic data-timing absence, sample point/SJW/restart verification, full
  control-mode/termination fingerprints, and evidence-gated capabilities.

Per instruction, no tests, builds, Clippy, rustfmt, cross-builds, or dependency
resolution were run in this fix round. Static inspection and `git diff --check`
were the only verification performed before staging.

## Fix round 2

- Fenced restoration with the interface index, current interface name, and a
  freshly resolved canonical physical resource identity. Attach and Configure
  also reject an endpoint that no longer resolves to the selected descriptor,
  so rename, ifindex reuse, and hotplug identity conflicts fail closed.
- Expanded nominal and data timing fingerprints to every raw kernel
  `can_bittiming` field written during restore: bitrate, sample point, time
  quantum, propagation segment, both phase segments, SJW, and prescaler.
- Made an absent snapshot control-mode attribute restore as an explicit
  all-nine-modes-disabled mask. Restore always submits the writable parameter
  set, and focused static tests cover raw timing, interface name/index, physical
  identity, control-mode, and termination fingerprint changes plus the explicit
  clearing mask.
- Distinguished failures before the first successful mutation from failures
  that require rollback. In particular, a permission denial on the first
  Configure operation is returned directly with its stable, resource-scoped
  classification instead of being obscured by a second unauthorized rollback.
- Kept the public active-configuration and core models unchanged. Ordinary
  `vcan` Attach remains honest when timing is unobservable: it returns a
  structured unsupported-configuration error, fabricates no timing, and
  discovery does not advertise Configure.
- Added an adapter-private raw-socket `vcan` path solely for ignored native
  qualification. It covers Classical and FD loopback without claiming active
  timing, authoritative software filtering, normalized bus status, structured
  behavior after deletion, retryable fixture cleanup, permission mapping, and
  a real-interface close-conflict retry that retains and restores its snapshot.

The scope deliberately stays inside the SocketCAN adapter and its tests. The
remaining risk is native/kernel behavior that cannot be established by static
inspection, especially controller-specific timing normalization, permissions,
hotplug races, and `vcan` FD behavior. Per the task ruling, no tests, builds,
Clippy, rustfmt, cross-builds, Cargo metadata, or dependency resolution were run
in this fix round; verification was limited to source/diff inspection and the
allowed whitespace/staged-diff checks.

## Fix round 3

- Narrowed control-mode restoration from the round-2 all-nine-bit mask to
  per-bit evidence for the three modes Configure can write: FD, listen-only,
  and loopback. A bit is restorable only when its snapshot value differs from
  the requested value, and that restore evidence becomes active only after the
  atomic `set_can_params` call succeeds. Unchanged false or unreported modes
  are omitted rather than sent as unsupported clear-mask bits that can produce
  `EOPNOTSUPP`; fingerprint conflict detection still observes all nine modes.
- Revalidated the current interface name through canonical sysfs identity on
  every bus-status query before returning normalized state or error counters.
  A stale descriptor therefore fails with the selected resource ID instead of
  exposing status from a renamed, reused, or hotplug-replaced endpoint.
- Expanded the ignored selected-real-interface close-retry qualification to
  compare all eight raw fields for both nominal and data timing, restart delay,
  all nine observable control-mode flags, and termination, in addition to link
  up/down state and MTU. The adapter-private `vcan` coverage now also checks
  that status rejects a mismatched canonical identity.
- Added focused static unit coverage for absent support evidence, per-bit
  changed-mode evidence, and omission of every control mode outside the
  evidenced restore mask.

This round remains confined to the SocketCAN adapter and its tests. Per task
instruction, no tests, builds, Clippy, rustfmt, cross-builds, Cargo metadata, or
dependency resolution were run. The TDD red/green execution observations are
therefore intentionally unavailable; verification is limited to source/diff
inspection and the allowed whitespace and staged-diff checks.
