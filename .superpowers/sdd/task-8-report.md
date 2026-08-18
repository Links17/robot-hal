# Task 8 report

## RED / GREEN

- Added workflow contract coverage before changing the workflow. It failed against the former
  single `verify` job: 6 failed, 1 passed.
- Added the `print-target --format github-matrix` contract before implementing that command. It
  failed with `release.tool.invalid`.
- Implemented the least-privilege source/platform workflow and read-only target-matrix command.
  Workflow and target contracts then passed: 18 passed.
- Task 8 review required explicit Rust formatter/linter installation and immutable action
  references. The added contracts failed against the reviewed workflow (2 failed, 5 passed),
  then passed after remediation (7 passed).
- Final review required Linux system prerequisites before `linux-gpio` compilation and the
  peeled setup-uv v5 commit. The added contracts failed against the reviewed workflow (2 failed,
  6 passed), then passed after remediation (8 passed).

## Workflow facts

- `.github/workflows/ci.yml` has only `source-gate` and `platform-conformance` jobs, top-level
  `permissions: {contents: read}`, and a 45-minute timeout on every job.
- Both jobs install Rust 1.85 with explicit `rustfmt` and `clippy` components. `source-gate`
  then runs generated-protocol, format, clippy, full workspace, frozen Python, release, and
  minor-matrix checks.
- A reusable Linux-only prerequisite step runs before any Cargo all-features or platform broker
  build and installs `libgpiod-dev` plus `pkg-config`. This provides the libgpiod v2 system
  library required by the Linux GPIO adapter's `libgpiod = "=1.0.0"` dependency; macOS and
  Windows do not run the step.
- The platform matrix is generated from `release/targets.toml` through
  `scripts/release/release_tool.py print-target --format github-matrix`; the workflow has no
  copied adapter feature list.
- Every platform builds a production broker with `--no-default-features` and its matrix feature
  list, obtains and validates its manifest, then separately builds a virtual-only broker for
  minor 0–3 conformance. JSON uploads are Actions test artifacts only.
- Every third-party action is pinned to a full immutable commit SHA with a readable reviewed
  version comment: checkout v6 (`d23441a48e516b6c34aea4fa41551a30e30af803`),
  setup-python v6 (`ece7cb06caefa5fff74198d8649806c4678c61a1`), setup-uv v5
  (`d4b2f3b6ecc6e67c4457f6d3e41ec42d3d0fcb86`, peeled tag commit), and upload-artifact v4
  (`ea165f8d65b6e75b540449e92b4886f43607fa02`). These SHAs were resolved through each
  action's GitHub tag reference. Checkout retains `persist-credentials: false`.

## Local verification

- `uv run --project bindings/python --python 3.11 --frozen pytest -q
  tests/release/test_workflow_contract.py tests/release/test_targets.py` — 18 passed.
- `./scripts/check-generated-protocol.sh` — passed.
- `cargo fmt --all --check` — passed.
- `cargo clippy --workspace --all-targets --all-features -- -D warnings` — passed.
- `cargo test --workspace --all-features` — passed.
- `uv run --project bindings/python --python 3.11 --frozen pytest -q tests/release` —
  183 passed.
- `uv run --project bindings/python --python 3.11 --frozen pytest -q` — 429 passed.
- `python3 -m compileall -q scripts/release/release_tool.py
  tests/release/test_workflow_contract.py tests/release/test_targets.py` and `git diff --check`
  — passed.
- Review remediation: `tests/release/test_workflow_contract.py` — 7 passed; `tests/release`
  — 183 passed; `compileall` and `git diff --check` — passed.
- Final-review remediation: `tests/release/test_workflow_contract.py` — 8 passed;
  `tests/release` — 184 passed; `compileall` and `git diff --check` — passed.

## Hosted boundary, self-review, and concerns

The local machine did not run GitHub-hosted macOS, Linux, or Windows jobs. Native
production-manifest and virtual-conformance evidence remains hosted-pending until this workflow
runs on GitHub Actions.

Self-review confirmed the workflow cannot publish and does not request OIDC, write permissions,
package/registry access, or secrets. No Task 9 release or attestation behavior was added.
