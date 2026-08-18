# Task 6 Report — Package Rust crates and Python distributions

## Status

Complete within the Task 6 scope. No Task 7+ behavior, release plan, or design
documents were modified.

Commit sequence:

1. `c79636c feat(release): package Rust and Python clients`
2. Pending report and packaged-crate closure validation commit.

## Implementation

- Added `package-rust --tag --repo-root --output-dir` and
  `package-python --tag --project --output-dir` to `release_tool.py`.
- Added thin POSIX wrappers for Rust and Python plus a PowerShell wrapper for
  Python; policy, validation, subprocess handling, and output publication
  remain in the Python tool.
- Rust package selection is derived from `cargo metadata --no-deps` and limits
  output to workspace members whose `publish` is not `false`.  The bundle uses
  exact, name-sorted `.crate` basenames, deterministic outer tar.gz encoding,
  archive validation, staging, and hard-link publication without overwriting an
  existing artifact.
- Each local `.crate` is archive-inspected, extracted with `tarfile`'s data
  filter, and checked using `cargo check --locked`; all subprocesses have
  bounded timeouts and exposed diagnostics are stable/sanitized.
- Added locked `build==1.3.0` to the Python development group and lockfile.
  Python packaging validates exact PEP 440 artifact names, wheel metadata/tag
  (`py3-none-any`), sdist metadata/content, and a temporary offline wheel
  installation asserting both distribution and `seeed_hal.__version__`.

## TDD

RED command:

```text
uv run --project bindings/python --python 3.11 --frozen pytest -q tests/release/test_package_rust.py tests/release/test_package_python.py
```

Observed expected RED: test collection failed because
`package_rust` and `python_artifact_names` did not yet exist.

GREEN command:

```text
uv run --project bindings/python --python 3.11 --frozen pytest -q tests/release/test_package_rust.py tests/release/test_package_python.py
```

Observed GREEN: `3 passed`.

## Verification

Completed:

```text
uv run --project bindings/python --python 3.11 --frozen pytest -q tests/release
# 130 passed

uv run --project bindings/python --python 3.11 --frozen pytest -q bindings/python/tests
# 187 passed

cargo fmt --all --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace --all-features
# completed successfully; hardware tests remained appropriately ignored

scripts/release/package-python.sh v0.5.0-rc.1 target/task6-python-artifacts
# emitted seeed_hal-0.5.0rc1-py3-none-any.whl and seeed_hal-0.5.0rc1.tar.gz
# wheel was installed offline in a temporary venv and version assertions passed

python3 -m compileall -q scripts/release/release_tool.py \
  tests/release/test_package_rust.py tests/release/test_package_python.py
git diff --check
```

## Actual Rust package boundary

The public v0.5 crates are not yet published to crates.io.  Running the
release workspace's real package command reached Cargo's normal upload
preparation check and failed because `seeed-hal-camera` was not available in
the crates.io index:

```text
cargo package --package seeed-hal-adapter-avfoundation --locked --allow-dirty --no-verify
# failed to prepare local package for uploading:
# no matching package named `seeed-hal-camera` found; location searched: crates.io index
```

This is expected for the current release state.  The implementation neither
publishes nor changes workspace dependencies to registry sources.  It retains
the safe, repeatable local `cargo package` workflow and validates generated
crate archive structure plus extracted `cargo check --locked` whenever Cargo
can produce the local package.  Full actual-workspace Rust bundle generation
remains blocked until the dependency publication closure exists; it is not
claimed as verified.

Task 7's clean Python 3.11–3.13 matrix was not run or claimed.

## Self-review and concerns

- Verified no artifact may overwrite an existing final filename.
- Verified sensitive subprocess diagnostics are not returned as paths/tokens.
- Kept artifact publication local and credential-independent.
- Concern: `cargo package` necessarily asks Cargo to assess the crates.io
  dependency closure; that is a Cargo behavior, not a registry credential
  dependency in this tool.  The current unpublished dependency closure blocks
  the real workspace Rust bundle exactly as documented above.

## Review follow-up

Addressed the four Important findings from the Task 6 review:

- Packageable Rust members now come exclusively from Cargo metadata
  `workspace_members`; the no-deps metadata result supplies member/package
  identity and a second Cargo resolve graph supplies deterministic,
  dependency-first package/check order.  Final bundle entries remain basename
  sorted.
- The Rust gate now requires an empty
  `git status --porcelain=v1 --untracked-files=all`; staged, unstaged,
  untracked, failed-status, and non-repository states all fail closed before
  Cargo receives `--allow-dirty`.
- Python wheel/sdist publication reserves both final names before building.  A
  failed reservation, build, validation, or second link removes only artifacts
  hard-linked by this invocation and leaves external final files untouched.
- The exact generated files
  `seeed_hal/proto/__init__.py` and `seeed_hal/proto/hal_pb2.py` are required
  in both distributions.  The isolated install imports the generated module,
  asserts its descriptor name, and serializes `Empty`.

Review RED:

```text
pytest initially failed importing the absent metadata-selection helper.
```

Review GREEN:

```text
uv run --project bindings/python --python 3.11 --frozen pytest -q \
  tests/release/test_package_rust.py tests/release/test_package_python.py
# 11 passed

uv run --project bindings/python --python 3.11 --frozen pytest -q tests/release
# 138 passed

uv run --project bindings/python --python 3.11 --frozen pytest -q bindings/python/tests
# 187 passed
```

The in-progress pre-commit Rust package attempt correctly failed closed on the
strict dirty-tree gate.  After commit `fc58a5c`, a clean-tree
`scripts/release/package-rust.sh v0.5.0-rc.1 target/task6-review-rust-clean`
passed that gate and again reached the existing Cargo package boundary:
`release.cargo.failed: cargo package failed`.  This is the already documented
unpublished crates.io dependency closure; no dependency source was changed and
no publication was attempted.
