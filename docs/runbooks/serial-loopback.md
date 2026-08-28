# Physical Serial loopback acceptance

Use this optional acceptance test only with a Serial endpoint wired for hardware loopback. The
default workspace test gate never opens physical hardware.

Set the endpoint path in the environment variable consumed by the test:

```bash
export ROBOT_HAL_SERIAL_LOOPBACK=/dev/tty.usbserial-example
```

On PowerShell, use `$env:ROBOT_HAL_SERIAL_LOOPBACK = "COM7"`. Then run exactly:

```bash
cargo test -p robot-hal-adapter-serialport --features hardware-tests -- \
  --ignored --nocapture
```

The older `hardware-loopback` Cargo feature remains as a backward-compatible alias target. The
runbook and CI-facing command use `hardware-tests`.

## Evidence template

Do not record or commit a device serial number. Remove it from console excerpts and metadata before
attaching evidence.

```text
Date/time:
Tester:
Adapter chipset/model (no device serial):
OS and version:
Endpoint:
Baud rate:
Payload sizes tested:
Round-trip result and byte counts:
Unplug behavior/error:
Replug/re-enumeration behavior:
Session/lease cleanup result:
Environment variable removed after test: yes/no
Endpoint closed and loopback fixture disconnected: yes/no
```

After the run, close any remaining session, unset `ROBOT_HAL_SERIAL_LOOPBACK`, disconnect the fixture,
and verify another process can open the endpoint. Physical loopback is not evidence for the broker's
virtual black-box suite, and the virtual suite is not evidence for a chipset or OS driver.
