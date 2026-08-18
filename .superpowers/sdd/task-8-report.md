# Task 8 report

## RED / GREEN

- Added workflow contract coverage before changing the workflow. It failed against the former
  single `verify` job: 6 failed, 1 passed.
- Added the `print-target --format github-matrix` contract before implementing that command. It
  failed with `release.tool.invalid`.
- Implemented the least-privilege source/platform workflow and read-only target-matrix command.
  Workflow and target contracts then passed: 18 passed.

## Workflow facts

- `.github/workflows/ci.yml` has only `source-gate` and `platform-conformance` jobs, top-level
  `permissions: {contents: read}`, and a 45-minute timeout on every job.
- `source-gate` installs Rust 1.85 and runs generated-protocol, format, clippy, full workspace,
  frozen Python, release, and minor-matrix checks.
- The platform matrix is generated from `release/targets.toml` through
  `scripts/release/release_tool.py print-target --format github-matrix`; the workflow has no
  copied adapter feature list.
- Every platform builds a production broker with `--no-default-features` and its matrix feature
  list, obtains and validates its manifest, then separately builds a virtual-only broker for
  minor 0–3 conformance. JSON uploads are Actions test artifacts only.
- Checkout uses current reviewed `actions/checkout@v6` with `persist-credentials: false`;
  Python uses `actions/setup-python@v6`; `astral-sh/setup-uv@v5` and
  `actions/upload-artifact@v4` remain reviewed major versions.

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

## Hosted boundary, self-review, and concerns

The local machine did not run GitHub-hosted macOS, Linux, or Windows jobs. Native
production-manifest and virtual-conformance evidence remains hosted-pending until this workflow
runs on GitHub Actions.

Self-review confirmed the workflow cannot publish and does not request OIDC, write permissions,
package/registry access, or secrets. No Task 9 release or attestation behavior was added.
