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
