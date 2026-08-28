# Native USB qualification

The `nusb` adapter is a production adapter for macOS, Linux, and Windows. It exposes only USB
device discovery, interface claims, and bounded Control, Bulk, and Interrupt transfers. It does
not decode a device protocol.

`nusb` 0.2.7 is Apache-2.0 OR MIT and declares Rust 1.85. Its blocking transfer calls run only
inside the runtime-owned terminal worker. A transfer timeout is passed to `nusb`; endpoint transfer
timeout cancels and drains its single submitted transfer before returning. The runtime keeps the
native interface handle inside that worker and quarantines the session when close does not finish
by its finite deadline.

## Preconditions

- Select a disposable fixture and record only its HAL resource ID, not its serial number or raw
  transfer payload.
- Linux: grant the test user narrowly scoped `/dev/bus/usb` access with a udev rule.
- Windows: bind the selected interface to WinUSB (WCID or a documented provisioning process).
- macOS: ensure no kernel driver owns the selected interface.

## Run

```bash
export ROBOT_HAL_USB_RESOURCE_ID=usb:device:vvvv:pppp:fixture
cargo test -p robot-hal-adapter-nusb --features hardware-tests --test hardware -- \
  --ignored --nocapture
```

Confirm interface exclusivity, each transfer type needed by the fixture, timeout/cancellation,
disconnect, and that release permits a new claim. Remove the environment variable and disconnect
the fixture afterward. This ignored test is an external qualification gate; it is not evidence
from the default hardware-free suite.
