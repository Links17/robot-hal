# Robot HAL release notes

## Proposed `v0.5.0-rc.1`

This is the first public-candidate naming pass from the former `seeed-hal`
repository. It includes the library-first Rust runtime, local broker, Python
client, virtual conformance adapters, and opt-in native Serial/CAN/USB/GPIO/
Camera adapters.

The release is alpha software. Hardware-free conformance is covered by CI;
native hardware qualification remains platform- and device-specific. The
crate and Python import names are now `robot_hal_*` / `robot_hal`.
