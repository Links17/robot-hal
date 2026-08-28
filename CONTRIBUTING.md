# Contributing

Robot HAL is transport/runtime infrastructure. Keep it independent of robots,
device protocols, Dora graph topology and product policy.

Run the full hardware-free gate before opening a pull request:

```bash
cargo fmt --all --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace --all-features
cd bindings/python && uv run --frozen pytest -q
```

Native adapter tests are opt-in and require the hardware described by their
runbooks. Never commit credentials, device paths, customer data or generated
artifacts. Changes to protocol/wire contracts require a versioning note and
updated conformance tests.

By submitting a contribution, you agree that it is provided under the Apache
License, Version 2.0, subject to any separate written agreement with the
copyright holder.
