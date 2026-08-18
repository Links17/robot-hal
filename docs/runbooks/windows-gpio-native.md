# Windows GPIO qualification

The Windows GPIO adapter uses the safe Rust projection of `Windows.Devices.Gpio` from `windows`
0.58.0 (MIT OR Apache-2.0; Rust 1.70). It is compiled only on Windows and its native handles stay
inside the runtime-owned worker.

WinRT exposes a default controller and line read/write support. It does not provide a monotonic
timestamp with `ValueChanged`; therefore the adapter honestly advertises `gpio.lines/v1` only and
fails `gpio.next_edge` with a structured unsupported-capability result. It must not synthesize a
wall-clock timestamp.

## Run

```powershell
$env:SEEED_HAL_GPIO_RESOURCE_ID = "gpio:windows:default"
cargo test -p seeed-hal-adapter-windows-gpio --features hardware-tests --test hardware -- `
  --ignored --nocapture
```

Validate controller availability, exclusive pin open, input read, output write, unsupported
configuration mapping, and close/reopen behavior. Do not treat this as edge qualification: native
monotonic edge events remain unavailable until the platform exposes a suitable timestamp source.
Remove the environment variable and disconnect the fixture when finished.
