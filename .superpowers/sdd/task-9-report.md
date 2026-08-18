# Task 9 Report — immutable RC build, verification, attestation, and prerelease

## Scope

Implemented Task 9 only on baseline `ab32d2b`. Task 10 was not implemented,
and no remote release, tag, push, or registry publication was attempted.

## TDD

Release workflow contract tests were written before the workflow existed:

```text
uv run --project bindings/python --python 3.11 --frozen \
  pytest -q tests/release/test_workflow_contract.py
# 9 failed, 8 passed: release-rc.yml did not exist
```

The aggregate command-line interface was also added test-first:

```text
tests/release/test_verify_artifacts.py::test_aggregate_cli_dispatches_all_frozen_input_directories
# failed: aggregate-release was not an accepted subcommand
```

After the minimal implementation and cross-platform checksum remediation:

```text
tests/release/test_workflow_contract.py plus aggregate CLI regression
# 18 passed
```

## Delivered behavior

- `.github/workflows/release-rc.yml` has the required `validate`,
  `platform-build`, `client-build`, `platform-verify`, `aggregate`, and
  `attest-and-release` jobs. It accepts tagged RC pushes and dispatch input;
  its first job rejects every tag except exact `v0.5.0-rc.N`, confirms the
  resolved tag/checkout/commit relation and clean tree, runs source gates,
  and refuses an existing GitHub Release.
- Build, verification, evidence, and final artifacts are named with the
  release tag plus the validated full commit. Consumers select only
  dependency-produced artifacts from their current workflow run and verify
  their `SHA256SUMS` before use.
- Each hosted platform validates and executes its matching production broker
  archive, then executes its separately built virtual conformance broker.
  Aggregate rejects anything other than exactly three platform reports with
  complete `Passed` software and virtual-minor evidence. It uses Task 7
  aggregation, static validation, complete artifact verification, and
  `release_ready` before the final job can run.
- Only the final job receives `contents: write`, `id-token: write`, and
  `attestations: write`. It revalidates the final directory, uses
  SHA-pinned `actions/attest` v4, then creates one prerelease with
  `--prerelease --latest=false`; it has no delete, edit, force, overwrite,
  registry, or secret-based publication path.
- Added `aggregate-release` as a narrowly tested CLI wrapper over the
  existing Task 7 frozen-input aggregate function.
- `docs/contracts/release-artifacts.md` defines exact artifact names,
  sidecars, deterministic bindings and checksums, evidence/attestation
  verification, immutable retry behavior, stable error categories,
  prohibited publication credentials, and hardware qualification separation.

## Validation

```text
uv run --project bindings/python --python 3.11 --frozen pytest -q tests/release
# 194 passed

python3 scripts/release/release_tool.py check-version \
  --tag v0.5.0-rc.1 --repo-root .
# passed

python3 -m compileall -q scripts/release/release_tool.py tests/release
git diff --check
# passed
```

## Hosted limitation and concern

GitHub Actions tag filters are glob patterns, not regular expressions.
`v0.5.0-rc.*` is therefore deliberately paired with the exact anchored
validation in the first, read-only `validate` job. A malformed matching glob
tag can start a workflow run, but cannot build, attest, or publish. Actual
macOS/Linux/Windows execution, GitHub attestation issuance, and prerelease
creation remain unexercised locally and were intentionally not triggered.

## Commit

Initial Task 9 implementation: `ddd7764 ci(release): build and attest v0.5 prereleases`.

## Review remediation

### RED / GREEN

- Archive-transfer permission regression first failed in two cases: an
  artifact-style mode-0644 tar archive left the extracted broker mode at
  `0755`, and an archive declaring a mode-0644 broker was accepted. The
  extractor now requires a validated regular tar member with owner execute,
  then applies mode `0700` only to the freshly extracted broker file. The
  source archive remains mode `0644`; no downloaded release tree is broadly
  chmodded. GREEN: 2 passed.
- Final-job tag immutability and dynamic target-matrix workflow contracts
  first failed because neither behavior existed. The final job now queries the
  remote plain tag and optional peeled annotated-tag ref before attestation,
  before release creation, and after creation; it requires one unambiguous
  result resolving to the validated commit. Target matrix output is now
  produced from `release/targets.toml` using `print-target --format json`.
- Per-platform evidence aggregation tests first failed because no aggregate
  interface existed. `aggregate-platform-reports` now requires exactly three
  reports, matching tag/commit/qualification, one Passed job per distinct
  release platform, and Passed virtual evidence for minors 0–3 on that same
  platform. Wrong tag, wrong commit, duplicate jobs, and missing minor records
  fail closed under `release.conformance.invalid` or
  `release.conformance.incomplete`.

### Review validation

```text
workflow contracts plus focused archive/evidence regressions
# 26 passed

uv run --project bindings/python --python 3.11 --frozen pytest -q tests/release
# 203 passed

python3 scripts/release/release_tool.py check-version \
  --tag v0.5.0-rc.1 --repo-root .
python3 -m compileall -q scripts/release/release_tool.py tests/release
git diff --check
# passed
```

Hosted Actions execution, remote tag observations, final attestation, and
prerelease publication remain intentionally unperformed locally.
