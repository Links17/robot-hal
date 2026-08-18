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

`d4e2f03` — `feat(release): generate and validate artifact manifests`

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
