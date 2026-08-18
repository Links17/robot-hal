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

## Important review resolution: isolated runtime verification

`platform-verify` runs as an independent job from `platform-build`, so it does
not inherit the build job's job-local libgpiod prefix. Its Linux matrix entry
now invokes the same shared prerequisite script before it executes the
downloaded production broker.

The script verifies that the SHA-pinned libgpiod install produced
`$PREFIX/lib/libgpiod.so.3`, then exports only `$PREFIX/lib` through
`LD_LIBRARY_PATH` (including via `GITHUB_ENV` for subsequent workflow steps).
Thus the downloaded production broker resolves the verified libgpiod v2 ABI
explicitly, rather than accidentally using the runner's incompatible
distribution library. The prerequisite script continues to install only build
tools, libudev development files, pkg-config, and the pinned libgpiod source;
the release artifact does not carry arbitrary shared libraries.

### Review RED → GREEN

The added `test_linux_production_broker_jobs_pin_the_runtime_loader_to_libgpiod_v2`
initially failed because the script neither required `libgpiod.so.3` nor
exported `LD_LIBRARY_PATH`, and RC `platform-verify` did not invoke it.
After the minimal workflow and script changes, the focused contract suite
passed **23 tests**.

Additional local verification:

```sh
bash -n scripts/ci/install-linux-native-prerequisites.sh
uv run --project bindings/python --python 3.11 --frozen pytest -q tests/release
cargo +1.85 fmt --all --check
```

Results: shell parsing passed, the release suite passed **209 tests**, and
Rust formatting passed. `shellcheck` could not be run because it is not
installed on this macOS worktree (`command not found`); this is a local tool
availability limitation, not a passing result.

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
- Pending: fail-closed runtime resolution review fix
