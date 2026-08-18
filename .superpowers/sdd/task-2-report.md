# Task 2 report: controlled Linux native prerequisites

## Outcome

Linux GPIO builds no longer depend on Ubuntu 24.04's `libgpiod-dev` 1.6.3.
`scripts/ci/install-linux-native-prerequisites.sh` downloads and verifies
libgpiod 2.2.1, installs it under a job-local prefix, and exposes that prefix
only through `PKG_CONFIG_PATH`. CI platform conformance and RC Linux platform
builds use this same script.

Source gates no longer install native prerequisites or run the Linux-native
ABI-dependent all-feature workspace checks. They retain formatting plus
`--no-default-features` Rust clippy and test coverage.

## RED

Command:

```sh
uv run --project bindings/python --python 3.11 --frozen pytest -q \
  tests/release/test_linux_prerequisites_contract.py \
  tests/release/test_workflow_contract.py
```

Result: **5 failed, 17 passed**. Failures proved the shared prerequisite
script did not exist, CI and RC still used distribution `libgpiod-dev`, and
the source gate still ran workspace all-feature clippy and tests.

## GREEN and verification

Commands:

```sh
uv run --project bindings/python --python 3.11 --frozen pytest -q \
  tests/release/test_linux_prerequisites_contract.py \
  tests/release/test_workflow_contract.py
shellcheck scripts/ci/install-linux-native-prerequisites.sh
bash -n scripts/ci/install-linux-native-prerequisites.sh
cargo +1.85 fmt --all --check
cargo +1.85 clippy --workspace --all-targets --no-default-features -- -D warnings
cargo +1.85 test --workspace --no-default-features
```

Results:

- Workflow and prerequisite contracts: **22 passed**.
- `shellcheck` and `bash -n`: passed.
- Rust formatting, no-default-features clippy, and no-default-features tests:
  passed.

The Linux Hosted CI-only script itself, production broker build, and virtual
broker build cannot run on this macOS worktree; their required execution paths
are wired in the Linux matrix jobs and will execute on GitHub-hosted Linux.

## Pinned upstream dependency

- libgpiod version: `2.2.1`
- archive: `libgpiod-2.2.1.tar.gz`
- SHA-256:
  `95689033324c16a13c32e947b9933553258544d6538466b04859a5d1ba950798`
- source and release metadata:
  <https://www.kernel.org/pub/software/libs/libgpiod/>

The URL and SHA-256 are recorded directly in the installation script. The
script uses noninteractive APT with three retries and 30-second HTTP/HTTPS
timeouts, installs and preflights both `libudev-dev` and `pkg-config`, and
requires `pkg-config --exists 'libgpiod >= 2'` plus
`pkg-config --exists libudev`.

## Changed files

- `scripts/ci/install-linux-native-prerequisites.sh`
- `tests/release/test_linux_prerequisites_contract.py`
- `tests/release/test_workflow_contract.py`
- `.github/workflows/ci.yml`
- `.github/workflows/release-rc.yml`
- `.superpowers/sdd/task-2-report.md`

## Commit

- `329a477 ci: provision Linux native dependencies`
