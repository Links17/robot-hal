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
  and never claims that a foreign target executable was run. Native brokers run
  the existing bounded virtual conformance runner for wire minors 0–3.
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
- The current fixture run stubs the existing Python virtual runner; real
  macOS broker execution would run it for wire minors 0–3.
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

## Review follow-up — 1a30962..1b75633

Addressed the three Critical/Important review findings without adding Task 8
workflow or Task 9 release behavior:

- Production broker archives are now limited to archive/static checks and,
  only on their matching host, `--manifest`. `verify-artifacts` no longer
  runs virtual conformance from a production archive. The new
  `run-virtual-conformance --platform --broker --repo-root --command --ref`
  entrypoint accepts a separately supplied host-local virtual-adapters broker,
  runs minors 0–3 with bounded subprocesses, and emits controlled evidence
  records. It rejects cross-host invocation.
- Software `Passed` is valid only with exactly one `Passed` job for macOS,
  Linux, and Windows plus `Passed` virtual evidence for every platform/minor
  pair. `release_ready` and `verify-artifacts` fail a factual
  `Partial`/`Pending`/`Blocked`/`Failed` report as
  `release.conformance.incomplete`; static validation continues to preserve
  those factual non-passed reports.
- Aggregation now records all candidate lstat identities and hashes before any
  copy, rechecks them after copying and report writing, and rechecks them
  again after sidecar/static verification immediately before return.

Review RED:

```text
tests/release/test_conformance_report.py tests/release/test_verify_artifacts.py
# expected collection error: release_ready absent
```

Review GREEN:

```text
focused report/artifact/manifest tests: 52 passed
```

## Review follow-up — 1a30962..d8fa8be

Addressed the two Important findings without adding Task 8 workflow or Task 9
release behavior:

- The parser now stores the selected subcommand in `subcommand`, and
  `run-virtual-conformance` takes the non-conflicting required
  `--command-identity`. A missing virtual broker reaches the virtual handler,
  fails with `release.conformance.invalid`, and does not disclose its path.
  The valid dispatch regression confirms the supplied identity reaches the
  collector unchanged.
- `generate-manifest` now creates its sidecar using
  `initial_conformance_report`: schema 1, matching tag/commit/qualification,
  software `Pending` with empty `jobs` and `virtual`, and factual Pending
  hardware. It remains static-valid but is not release-ready.

Review RED:

```text
tests/release/test_conformance_report.py tests/release/test_manifest.py
# 3 failures: virtual CLI silently exited 0/failed dispatch; generated legacy report failed verify-static
```

Review GREEN:

```text
focused report and manifest tests: 45 passed
```

## Final review follow-up — 1a30962..0906e1d

Addressed the final Important report-substitution finding without adding Task
8 workflow or Task 9 release behavior:

- `release-manifest.json` now binds the canonical
  `conformance-report.json` independently of `SHA256SUMS` through an exact
  schema/name/byte-size/SHA-256 record. The checksum list remains restricted
  to the six primary artifacts, so the report binding introduces no
  self-reference.
- `verify-static` requires canonical report bytes, the bound size and digest,
  then validates report schema semantics and release identity. Replacing a
  Pending report with a fully formed three-platform `Passed` report, or merely
  reformatting the same report, now fails as `release.manifest.invalid`.
- Aggregation passes its validated report into manifest generation so the
  binding covers its actual sidecar. The standalone generate-manifest path
  binds the factual initial Pending report it writes.

Review RED:

```text
tests/release/test_verify_artifacts.py tests/release/test_manifest.py
# 2 failures: replacement passed verify-static; manifest lacked report size/hash
```

Review GREEN:

```text
focused report/artifact/manifest tests: 57 passed
tests/release: 175 passed
compileall and git diff --check: passed
```
