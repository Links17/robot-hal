# Task 7 Report — Complete offline verification and conformance reports

## Status

Implemented and committed as an independent Task 7 release-conformance change.
No CI workflow or Task 8+ behavior was added.

## TDD

RED was observed before implementation:

```text
uv run --project bindings/python --python 3.11 --frozen pytest -q \
  tests/release/test_verify_artifacts.py tests/release/test_conformance_report.py
# collection errors: aggregate_release and validate_conformance_report absent
```

GREEN after the minimal implementation:

```text
uv run --project bindings/python --python 3.11 --frozen pytest -q \
  tests/release/test_verify_artifacts.py tests/release/test_conformance_report.py \
  tests/release/test_manifest.py
# 45 passed
```

## Delivered behavior

- `aggregate_release` creates only a new, `lstat`-checked mode-0700 directory,
  freezes candidate directory and file identities, copies through exclusive
  creates, emits the three sidecars, and requires `verify-static` before
  returning success. A failed directory is retained only for diagnostics.
- `write-conformance-report --inputs DIR --output FILE` enforces a controlled
  schema. Software job command identities are bounded and public refs are
  required. Hardware accepts only `Passed`, `Partial`, `Pending`, `Blocked`,
  or `Failed`; pending/blocked evidence remains null and cannot become passed
  implicitly. Sensitive fields and values are rejected.
- `verify-artifacts --tag --artifacts-dir --targets --repo-root` accepts only a
  complete directory, runs Task 4 `verify-static`, archive/static broker checks
  for all three targets, executes `--manifest` only for a native host broker,
  and never claims that a foreign target executable was run. A packaged
  `virtual-conformance` entrypoint, when supplied by a future broker archive,
  is run with bounded subprocesses for wire minors 0–3.
- POSIX and PowerShell wrappers and the qualification template were added.

## Commands

```text
uv run --project bindings/python --python 3.11 --frozen pytest -q tests/release
# 163 passed

python3 -m compileall -q scripts/release/release_tool.py \
  tests/release/test_verify_artifacts.py tests/release/test_conformance_report.py
git diff --check
# passed
```

The focused fixture constructs a new complete release directory with all six
artifacts and three sidecars, then passes `verify-static`. It uses fixture
broker archives only; no release-ready verdict is produced.

## Factual local and hosted limits

- No current packaged macOS broker was available under `target/`, so no actual
  macOS broker `--manifest` execution is claimed.
- Linux and Windows binaries were not executed on macOS. Their archive and
  static manifest checks remain local; hosted platform evidence is required
  before their executable conformance can be claimed.
- The checked archive format does not currently package a
  `virtual-conformance` executable. The verifier supports it if supplied but
  does not fabricate an execution claim.
- Real Rust bundle aggregation remains blocked by the existing unpublished
  crates.io dependency closure documented by Task 6. No missing bundle was
  faked and no release-ready result was claimed.
- Physical hardware remains Pending/Blocked in the template and requires the
  linked external runbooks/evidence.

## Self-review and concerns

- `verify-static` remains the all-or-nothing software artifact boundary.
- No `os.replace` or multi-file atomic-publication claim is used.
- The report is an evidence record, not a hardware qualification authority.
- The `--repo-root` CLI argument is retained for the prescribed interface but
  is intentionally not used to infer hosted or hardware evidence.
