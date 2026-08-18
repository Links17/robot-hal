#!/usr/bin/env bash
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$repo_root"

./scripts/check-generated-protocol.sh
cargo test -p seeed-hal-testkit --test camera_conformance
cargo test -p seeed-hal-runtime --test camera_runtime
cargo test -p seeed-hal-adapter-shared-memory
uv run --project bindings/python --python 3.11 --frozen pytest -q \
  bindings/python/tests/test_camera_contract.py
cargo build -p seeed-hal-broker-app --features virtual-adapters
uv run --project bindings/python --python 3.11 --frozen python \
  tests/conformance/run-broker-conformance.py \
  --broker target/debug/seeed-hal-broker
cargo fmt --all --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace --all-features
cargo check -p seeed-hal-adapter-mediafoundation --target x86_64-pc-windows-gnu
cargo check -p seeed-hal-adapter-avfoundation
cargo test -p seeed-hal-adapter-avfoundation --all-features
