# Naming migration

The repository and public package names are changing from `seeed-hal` to
`robot-hal` before the first public release.

- Rust crates: `robot-hal-*`
- Python distribution/import: `robot-hal` / `robot_hal`
- Broker binary: `robot-hal-broker`
- Environment variables: `ROBOT_HAL_*`

There is no compatibility alias in this pre-release tree. Consumers should
update imports, crate dependency names, executable names and test fixtures in
one change. The `dora-lerobot` integration tracks the same naming baseline.
