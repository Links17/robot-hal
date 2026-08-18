# Task 4 Report: Deterministic Release Manifests and Safe Archive Inspection

## Status

DONE

## Changes

- Added standard-library-only release manifest models: `ArtifactRecord`,
  `ReleaseManifest`, `ConformanceReport`, and `QualificationStatus`.
- Added deterministic manifest encoding and `SHA256SUMS` generation. The manifest
  is compact UTF-8 JSON with sorted keys and a trailing newline; artifacts and
  checksum lines are lexically sorted by basename.
- Added exact v0.5 artifact basename allowlists, strict identity/composition
  validation, sensitive-field/value rejection, and static manifest/checksum/artifact
  validation. `SHA256SUMS` covers release artifacts only; it deliberately excludes
  `release-manifest.json` and itself to avoid self-reference.
- Added `generate-manifest` and `verify-static` CLI commands for subsequent release
  tasks.
- Added no-extract tar/zip validation that rejects unsafe paths, non-root members,
  duplicate members, symlinks/hardlinks/devices/non-regular types, and unexpected
  content.
- Added deterministic manifest, checksum, static verifier, and archive-safety test
  coverage plus qualification and valid-fixture directory placeholders.

## RED Evidence

Before implementation:

```bash
uv run --project bindings/python --python 3.11 --frozen \
  pytest -q tests/release/test_manifest.py tests/release/test_archive_safety.py
```

Result: exit code 2 during collection, as expected. The test modules could not
import the missing `encode_manifest` and `validate_archive` interfaces from
`scripts.release.release_tool`.

## GREEN Evidence and Commands

```bash
uv run --project bindings/python --python 3.11 --frozen \
  pytest -q tests/release/test_manifest.py tests/release/test_archive_safety.py
```

Result: `31 passed`.

```bash
uv run --project bindings/python --python 3.11 --frozen pytest -q tests/release
python3 -m compileall -q scripts/release tests/release
git diff --check
```

Result: release suite `77 passed`; Python compilation and diff check exited 0.
IDE diagnostics reported no errors for changed files.

## Commit

- `d4e2f03` — `feat(release): generate and validate artifact manifests`
- `cfe5f18` — `fix(release): reject empty archive path components`

## Self-review

- All Task 4 business behavior is centralized in `scripts/release/release_tool.py`
  and uses only Python standard-library modules.
- Manifest persistence rejects prohibited sensitive field names recursively and does
  not echo manifest values in diagnostics; all new failure paths use stable
  `release.manifest.invalid` or `release.archive.invalid` names.
- Static verification requires an exact artifact set, sizes, hashes, and canonical
  checksums rather than accepting extra or omitted files.
- Archive inspection only reads member metadata; it never calls extraction APIs.
  Member paths explicitly reject empty components, `.`, `..`, backslashes,
  absolute paths, and Windows drive-like paths.
- Scope is limited to Task 4 release tooling, focused tests, required fixtures, and
  this report; no Task 5+ behavior, plans, or design documents were changed.

## Concerns

- Archive content expectations are supplied by future packaging tasks through the
  `validate_archive(..., expected_root=..., expected_files=...)` interface. This
  task intentionally establishes strict validation without guessing a future
  package file inventory.

---

## P0/P1 Review Follow-up

### Status

DONE

### RED Evidence

After adding tests for the reviewed gaps, the required focused command failed as
expected:

```bash
uv run --project bindings/python --python 3.11 --frozen \
  pytest -q tests/release/test_manifest.py tests/release/test_archive_safety.py
```

Result: `16 failed, 35 passed`. Failures demonstrated that the release model
accepted a partial artifact set, omitted the crates bundle, lacked a controlled
conformance report sidecar, allowed unsafe qualification URIs, emitted argparse
usage for malformed CLI input, and did not reject archive case/Unicode
collisions or empty directories.

### Changes

- The manifest now derives and requires the exact v0.5 RC artifact set from its
  tag: three broker archives, one Rust crates bundle, wheel, and sdist.
- `conformance-report.json` is a controlled manifest sidecar with an explicit
  schema and identity/qualification linkage. It is required by static
  verification but excluded from artifact checksum coverage, along with
  `release-manifest.json` and `SHA256SUMS`, to avoid self-reference.
- Qualification IDs and HTTPS report URIs are schema-validated. Userinfo,
  query/fragment, local hosts, and private/loopback/link-local IP endpoints are
  rejected without echoing values.
- Archive validation now detects case-fold and NFC collisions, canonicalizes
  expected paths, and permits only the root plus parent directories implied by
  expected files.
- CLI parsing maps argument errors to `release.tool.invalid` without argparse
  usage text or supplied values.

### GREEN Evidence

```bash
uv run --project bindings/python --python 3.11 --frozen \
  pytest -q tests/release/test_manifest.py tests/release/test_archive_safety.py
uv run --project bindings/python --python 3.11 --frozen pytest -q tests/release
python3 -m compileall -q scripts/release tests/release
git diff --check
```

Result: focused suite `51 passed`; complete release suite `97 passed`;
compileall and diff check passed. IDE diagnostics reported no errors.

### Commit

`7fa8b54` — `fix(release): harden manifest conformance validation`

### Concerns

- The follow-up supplies only the release model and static verification required
  for later packaging and qualification tasks. It does not perform artifact
  packaging or cross-platform qualification execution.

---

## Important Review Follow-up

### Status

DONE

### RED Evidence

```bash
uv run --project bindings/python --python 3.11 --frozen \
  pytest -q tests/release/test_manifest.py tests/release/test_archive_safety.py
```

Result: `4 failed, 52 passed`. The failures demonstrated acceptance of identical
duplicate records through the artifact map, the old split-path static verifier
API, and inconsistent NFD-member/NFC-expected archive comparison.

### Changes

- Manifest validation rejects duplicate artifact names before comparing the
  exact artifact map. Diagnostics remain value-free.
- `verify-static` is now a complete single-directory gate:
  `verify-static --release-dir <dir>`. The directory must contain exactly the
  six tag-derived primary artifacts plus `release-manifest.json`, `SHA256SUMS`,
  and `conformance-report.json`; every entry must be a regular non-symlink
  file in that same directory.
- Archive comparisons use NFC canonical path strings while collision keys use
  NFC plus case-folding. A single NFD member therefore matches one NFC expected
  name, but multiple colliding spellings are rejected.

### GREEN Evidence

```bash
uv run --project bindings/python --python 3.11 --frozen \
  pytest -q tests/release/test_manifest.py tests/release/test_archive_safety.py
uv run --project bindings/python --python 3.11 --frozen pytest -q tests/release
python3 -m compileall -q scripts/release tests/release
git diff --check
```

Result: focused suite `56 passed`; full release suite `102 passed`; compileall
and diff check passed.

### Commit

Pending.

### Concerns

- This follow-up establishes static directory layout and archive-validation
  semantics only. It does not add package production or release workflows.
