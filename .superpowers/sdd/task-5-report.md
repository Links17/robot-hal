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
cargo build --release -p seeed-hal-broker-app --no-default-features \
  --features serialport,nusb,avfoundation
target/release/seeed-hal-broker --manifest > target/release/broker-manifest.json
scripts/release/package-broker.sh v0.5.0-rc.1 macos \
  target/release/seeed-hal-broker target/release/broker-manifest.json \
  target/release-artifacts
python3 scripts/release/release_tool.py verify-broker-manifest \
  --tag v0.5.0-rc.1 \
  --manifest target/release/broker-manifest.json \
  --target macos \
  --targets release/targets.toml \
  --artifact target/release/seeed-hal-broker
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
