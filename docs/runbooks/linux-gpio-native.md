# Linux GPIO qualification

The Linux GPIO adapter uses `libgpiod` v2 through `libgpiod` 1.0.0. The crate is Apache-2.0 OR
BSD-3-Clause, declares Rust 1.78, and is used under the workspace MSRV of Rust 1.85. Native
requests and their libgpiod handles remain inside the runtime-owned terminal worker.

Discovery enumerates `/dev/gpiochip*`; the kernel chip name is persisted as resource identity and
the device path is only the current endpoint. Input/output direction, active-low, bias, and drive
are configured only when libgpiod accepts them. Edge requests select libgpiod's monotonic event
clock, preserving timestamps without deriving wall-clock time.

## Preconditions

- Provision an isolated GPIO fixture with known safe external pull resistors and an edge source.
- Give the test user access to the selected `/dev/gpiochip*` node.
- Record only the HAL resource ID and line offsets; do not commit board-specific wiring or payloads.

## Run

```bash
export ROBOT_HAL_GPIO_RESOURCE_ID=gpio:chip:gpiochipN
cargo test -p robot-hal-adapter-linux-gpio --features hardware-tests --test hardware -- \
  --ignored --nocapture
```

Verify input read, output write, rising/falling ordering, monotonic timestamps, exclusive request,
edge timeout, disconnect/re-enumeration, and lease cleanup. Unset the environment variable and
return the fixture to its normal wiring afterward. These ignored tests are external qualification
gates and do not run in the default suite.
