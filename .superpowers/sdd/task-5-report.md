# Task 5 Report: Package and Verify Platform Broker Archives

## Status

DONE

## Changes

- Added `package-broker --tag --target --targets --binary --output-dir --manifest`
  to `scripts/release/release_tool.py`.
- The command verifies the explicit RC tag, strict target matrix, and Task 2
  broker manifest against the supplied binary before packaging.
- The target matrix selects archive type: macOS/Linux use deterministic
  `tar.gz`; Windows uses deterministic `zip`. It never selects a format from
  the host operating system.
- Broker archives contain exactly one versioned root directory and the four
  required files: broker executable (with `.exe` only for Windows),
  `broker-manifest.json`, `LICENSE`, and `README.md`.
- tar normalizes uid/gid/user/group/mtime/mode; zip normalizes member timestamp
  and external mode attributes. Every completed archive immediately passes the
  existing no-extract `validate_archive` gate with the exact required content.
- Added thin POSIX and PowerShell wrappers that locate the repository and
  delegate all validation and packaging to the Python command.
- Added the missing root Apache-2.0 `LICENSE`, matching the existing workspace
  license declaration, so the required release archive member is real.

## RED Evidence

Before implementation:

```bash
uv run --project bindings/python --python 3.11 --frozen \
  pytest -q tests/release/test_package_broker.py
```

Result: exit code 2 during collection, as expected:

```text
ImportError: cannot import name 'package_broker'
```

The new tests therefore exercised a missing production interface, rather than
an already-implemented behavior.

## GREEN Evidence

Focused Task 5 tests:

```bash
uv run --project bindings/python --python 3.11 --frozen \
  pytest -q tests/release/test_package_broker.py
```

Result: `8 passed`.

Full release suite and static checks:

```bash
uv run --project bindings/python --python 3.11 --frozen pytest -q tests/release
python3 -m compileall -q scripts/release tests/release
git diff --check
```

Result: `117 passed`; Python compilation and whitespace validation exited 0.
IDE diagnostics reported no errors for edited Python files.

## Actual macOS arm64 Package Verification

The following command ran successfully on this macOS arm64 host:

```bash
cargo build --release -p robot-hal-broker-app --no-default-features \
  --features serialport,nusb,avfoundation
target/release/robot-hal-broker --manifest > target/release/broker-manifest.json
scripts/release/package-broker.sh v0.5.0-rc.1 macos \
  target/release/robot-hal-broker target/release/broker-manifest.json \
  target/release-artifacts
python3 scripts/release/release_tool.py verify-broker-manifest \
  --tag v0.5.0-rc.1 \
  --manifest target/release/broker-manifest.json \
  --target macos \
  --targets release/targets.toml \
  --artifact target/release/robot-hal-broker
```

The release build, manifest generation, matrix/manifest verification, and
deterministic macOS archive production completed with exit code 0. Task 4
`verify-static` intentionally was not run: it requires the complete six-artifact
release directory, whereas Task 5 generated only the broker archive and did
not fabricate Task 6+ artifacts.

## Platform Boundary

Only macOS arm64 binary build and runtime manifest generation were performed.
Linux and Windows archive-format behavior is covered by deterministic fixture
tests, but their broker binaries were not built or run on this machine.

## Commit

`feat(release): package verified broker artifacts`

## Self-review

- All release decisions use the existing tag parser, exact target matrix,
  Task 2 manifest verifier, and Task 4 no-extract archive validator.
- Input and output paths reject symlinks and non-files where applicable. The
  archive validator rejects unsafe paths, links, unexpected members, and extra
  directories after creation.
- Packaging failures preserve stable `ReleaseFailure` names and use bounded,
  value-free diagnostics; command input paths, tokens, and endpoints are not
  echoed.
- Wrappers do not duplicate Python validation logic.
- Scope is limited to Task 5 files and its required license input; no plan,
  design, or Task 6+ implementation changed.

## Concerns

- Linux and Windows native binary build/runtime verification remains for their
  hosted release jobs; it is not claimed here.
- Full `verify-static` is intentionally deferred until later tasks produce the
  exact complete release artifact set.

---

## Important Review Follow-up: Staged Atomic Publication

### Status

DONE

### RED Evidence

After adding the review regression cases:

```bash
uv run --project bindings/python --python 3.11 --frozen \
  pytest -q tests/release/test_package_broker.py
```

Result: `3 failed, 8 passed`, as expected. The failures showed that an archive
validation failure left the final artifact behind, an existing final archive was
silently overwritten, and archive writers received live source paths instead
of private frozen input copies.

### Changes

- `package_broker` now creates a unique hidden `.package-broker-*` workspace
  inside the requested output directory with owner-only permissions.
- It copies the verified binary, manifest, license, and README into a private
  `inputs/` directory, then packages exclusively from those frozen files.
- Archive creation and exact no-extract validation run only against a staging
  archive. The final path is published only after validation using same-directory
  `os.replace`.
- A pre-existing final archive fails closed as
  `release.artifact.unexpected`; it is never overwritten.
- `finally` removes staging inputs and staged archives for both success and
  failure, leaving pre-existing output entries intact. Failure diagnostics remain
  structured and do not expose staging paths or input values.

### GREEN Evidence

```bash
uv run --project bindings/python --python 3.11 --frozen \
  pytest -q tests/release/test_package_broker.py
uv run --project bindings/python --python 3.11 --frozen pytest -q tests/release
python3 -m compileall -q scripts/release tests/release
git diff --check
```

Result: Task 5 focused suite `11 passed`; complete release suite `120 passed`;
Python compilation and whitespace validation exited 0. IDE diagnostics reported
no errors for edited files.

The macOS arm64 release invocation was also re-run successfully with
`--no-default-features --features serialport,nusb,avfoundation`, runtime
manifest generation, `package-broker.sh`, and `verify-broker-manifest`.

### Commit

`fix(release): stage broker archive publication`

### Concerns

- The final existence check before `os.replace` avoids normal accidental
  overwrite. It cannot provide a cross-process no-clobber primitive with the
  standard portable `os.replace` API; release jobs should continue to own
  isolated artifact directories.
- Linux and Windows native binary build/runtime validation remains unperformed
  on this macOS arm64 host.

---

## Important Review Follow-up: No-clobber Publish Reservation

### Status

DONE

### RED Evidence

The new deterministic two-publisher race test synchronized both callers at the
old pre-`os.replace` window:

```bash
uv run --project bindings/python --python 3.11 --frozen \
  pytest -q tests/release/test_package_broker.py
```

Result: `1 failed, 11 passed`. Both calls reported success, proving that the
existing `exists()` check followed by `os.replace()` could overwrite a
concurrently published archive.

### Changes

- Publication now creates a stable hidden same-directory reservation:
  `.reserve-broker-<archive-basename>`, using portable `os.open` with
  `O_CREAT | O_EXCL`.
- A final archive or any pre-existing reservation fails closed as
  `release.artifact.unexpected`. Existing reservations are not deleted or
  reused, so crash leftovers require isolated-job cleanup or manual review.
- Only the reservation owner stages inputs, creates and validates the archive,
  then atomically places the staged archive at the final path.
- Success and ordinary failure remove only the caller's staging directory and
  reservation. The reservation is hidden, does not match a release artifact
  basename, and is removed on successful completion so later exact release-dir
  validation remains clean.

### GREEN Evidence

```bash
uv run --project bindings/python --python 3.11 --frozen \
  pytest -q tests/release/test_package_broker.py
uv run --project bindings/python --python 3.11 --frozen pytest -q tests/release
python3 -m compileall -q scripts/release tests/release
git diff --check
```

Result: Task 5 focused suite `13 passed`; complete release suite `122 passed`;
Python compilation and whitespace validation exited 0. The concurrency case
asserts exactly one success, one `release.artifact.unexpected` failure, a
validator-accepted final archive, and no staging/reservation leftovers. A
separate regression confirms that a pre-existing reservation fails closed and
is not removed.

The macOS arm64 release build, runtime manifest generation,
`package-broker.sh`, and manifest verification were re-run successfully.

### Commit

`fix(release): reserve broker artifact publication`

### Concerns

- A crash may leave a reservation by design; subsequent publishers fail closed
  instead of deleting a possibly active or unknown reservation. Isolated release
  artifact directories or explicit operator cleanup are therefore required.
- Linux and Windows native binary build/runtime verification remains outside
  this macOS arm64 execution.

---

## Important Review Follow-up: Isolated Output Directory Contract

### Status

DONE

### RED Evidence

A deterministic external-writer regression injected a final archive after the
package reservation was acquired but before publication:

```bash
uv run --project bindings/python --python 3.11 --frozen \
  pytest -q tests/release/test_package_broker.py
```

Result: `1 failed, 13 passed`. The old path called `os.replace` and silently
overwrote the injected external bytes.

### Changes

- `package-broker` now creates and exclusively owns a requested output directory
  that must not already exist. It is therefore a per-invocation isolated artifact
  directory, suitable for the existing per-platform release jobs and later
  aggregation into a separate complete release directory.
- It snapshots the only two permitted in-progress entries (its hidden
  reservation and private staging directory) after staging is created and
  immediately before `os.replace`.
- Any external final archive, stale artifact, reservation, or other output
  entry changes the snapshot and returns
  `release.artifact.unexpected`; publication is not attempted. The detected
  external file is retained byte-for-byte.
- This is an honest safety precondition rather than a claim of a universal
  cross-platform atomic no-clobber primitive: the caller supplies a fresh output
  path and must not permit arbitrary writers during publication.

### GREEN Evidence

```bash
uv run --project bindings/python --python 3.11 --frozen \
  pytest -q tests/release/test_package_broker.py
uv run --project bindings/python --python 3.11 --frozen pytest -q tests/release
python3 -m compileall -q scripts/release tests/release
git diff --check
```

Result: Task 5 focused suite `14 passed`; complete release suite `123 passed`;
Python compilation and whitespace validation exited 0. The external-writer case
asserts stable failure and unchanged external bytes.

The macOS arm64 release build, runtime manifest generation,
`package-broker.sh`, and manifest verification were re-run successfully.

### Commit

`fix(release): require isolated broker output`

### Concerns

- The portable standard library has no operation that atomically couples
  “destination absent” with replacement on both POSIX and Windows. This task
  therefore deliberately constrains publication to a fresh, exclusively owned
  per-invocation directory and verifies it before replacement; callers
  that allow arbitrary external writers after that final verification violate
  the contract.
- Linux and Windows native binary build/runtime validation remains outside
  this macOS arm64 execution.

---

## Important Review Follow-up: Atomic No-clobber Link Publication

### Status

DONE

### RED Evidence

The uncommitted link-publication regressions ran before the production change:

```bash
uv run --project bindings/python --python 3.11 --frozen \
  pytest -q tests/release/test_package_broker.py
```

Result: `3 failed, 14 passed`. The failures confirmed the prior implementation
never invoked `os.link`, so it could not prevent an external writer from being
silently replaced by `os.replace`.

### Changes

- Final publication now atomically creates `archive_path` as a hard link to
  the validated same-directory staging archive with `os.link`.
- A destination created by another publisher causes `FileExistsError`, which
  maps to `release.artifact.unexpected`; the external archive is retained
  byte-for-byte.
- `EXDEV`, `EPERM`, and any other link failure fail closed as
  `release.package.invalid`; no copy or replacement fallback exists.
- After a successful link publish the staging link is removed, preserving the
  published archive while normal staging and reservation cleanup continues.
- The existing output-directory contract, reservation, frozen inputs, and
  archive validation remain defense-in-depth only. The hard-link create is the
  final no-clobber primitive; snapshot checks are not claimed to ensure
  no-clobber on their own.

### GREEN Evidence

```bash
uv run --project bindings/python --python 3.11 --frozen \
  pytest -q tests/release/test_package_broker.py
uv run --project bindings/python --python 3.11 --frozen pytest -q tests/release
python3 -m compileall -q scripts/release tests/release
git diff --check
```

Result: focused Task 5 suite `17 passed`; complete release suite `126 passed`;
Python compilation and whitespace validation exited 0. IDE diagnostics reported
no errors for edited Python files.

### Actual macOS arm64 Package Verification

The local macOS arm64 broker was built with
`--no-default-features --features serialport,nusb,avfoundation`. Runtime
manifest generation, `package-broker.sh`, `verify-broker-manifest`, and
`validate_archive` against the produced archive all completed successfully.
Linux and Windows binaries were not built or run on this host.

### Commit

`fix(release): prevent broker archive clobber`

### Self-review

- A concurrent final-name creation reaches the atomic `link(2)` create and
  cannot overwrite its bytes.
- Link publication has no `os.replace` or copy fallback.
- Staging inputs, validation, reservation, and cleanup are retained.

### Concerns

- Hard-link publication requires source and destination to share a filesystem,
  which is ensured by staging inside the output directory. Filesystems that
  reject hard links fail closed.
- Crash-left reservations remain fail-closed by design.

---

## Final Review Follow-up: Published Archive Cleanup

### Status

DONE

### RED Evidence

The focused suite ran after adding a regression that makes `Path.unlink` fail
only for the staged archive after `os.link` has created the final name:

```bash
uv run --project bindings/python --python 3.11 --frozen \
  pytest -q tests/release/test_package_broker.py
```

Result: `1 failed, 17 passed`. The prior implementation surfaced
`release.package.invalid` even though the published archive already existed.

### Changes

- `package_broker` explicitly records the final archive only after the atomic
  hard-link publication succeeds.
- Removal of the staged archive is now best-effort after publication.
- Per-entry staging cleanup is also best-effort, so a residual staged file
  cannot turn a successful publication into a failed result.
- Validation, link publication, reservation ownership, and all pre-publication
  failures remain fail-closed. Final archives still use the no-clobber link
  create and are returned only after it succeeds.

### GREEN Evidence

```bash
uv run --project bindings/python --python 3.11 --frozen \
  pytest -q tests/release/test_package_broker.py
uv run --project bindings/python --python 3.11 --frozen pytest -q tests/release
python3 -m compileall -q scripts/release tests/release
git diff --check
```

Result: focused Task 5 suite `18 passed`; complete release suite `127 passed`;
Python compilation and whitespace validation exited 0. The regression validates
the returned final archive after the staged unlink failure.

### Commit

`fix(release): preserve published broker result`

### Self-review

- A post-link cleanup failure no longer changes the successful public result.
- Link failures and all pre-link validation failures still return stable,
  fail-closed release errors.
- Reservation cleanup and no-clobber behavior are unchanged.

### Concerns

- A failed post-publication staging cleanup can leave a private staging
  directory for operator cleanup; the final archive remains valid and its
  publication result remains authoritative.
