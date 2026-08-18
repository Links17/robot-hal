# Task 1 report: Rust workspace source bundle

## Outcome

`package-rust` now creates `seeed-hal-crates-v0.5.0-rc.N.tar.gz` as a
deterministic complete workspace source bundle. It is not a `.crate`
collection and does not imply registry publication readiness.

## RED

Command:

```sh
uv run --project bindings/python pytest ../../tests/release/test_package_rust.py -q
```

Result: failed as expected in
`test_rust_bundle_preserves_path_version_workspace_closure`. The old
`cargo package` flow returned `release.cargo.failed: cargo package failed`
when packaging the fixture's `adapter -> camera -> core` internal
`path + version` dependency chain. This proves the former independently
packaged-crate model cannot provide registry-independent closure.

## GREEN and verification

Commands:

```sh
uv run --project bindings/python pytest ../../tests/release/test_package_rust.py -q
uv run --project bindings/python pytest ../../tests/release -q
```

Results:

- `test_package_rust.py`: 8 passed.
- Complete `tests/release`: 203 passed.

The focused test unpacks the generated bundle and runs
`cargo check --workspace --locked`; it verifies the root manifest, lockfile,
all three workspace members, and the path-and-version dependency chain.

## Changed files

- `scripts/release/release_tool.py`
- `tests/release/test_package_rust.py`
- `docs/contracts/release-artifacts.md`
- `docs/superpowers/specs/2026-08-18-release-conformance-v0.5-design.md`
- `.superpowers/sdd/task-1-report.md`

## Commits

- `f82fb46 fix(release): bundle Rust workspace sources`
- `16169d4 fix(release): preserve source bundle directories`

## Concerns

- No registry publish command or registry policy was added. Public-crate
  registry policy remains deferred to the separately planned v1.0 work.

## Review follow-up: final clean repository check and reproducibility

### RED

Added two regression tests to `tests/release/test_package_rust.py`:

- `test_rust_bundle_rechecks_repository_cleanliness_before_publish` patches
  bundle validation to create an untracked file after the initial clean check.
  It records every invocation of the existing `_require_clean_repository`
  helper and requires the resulting failure to retain the structured
  `release.package.invalid` name.
- `test_rust_bundle_is_byte_identical_across_independent_output_directories`
  builds the same source workspace into two distinct artifact directories and
  compares the complete `.tar.gz` byte streams.

Command:

```sh
uv run --project bindings/python pytest ../../tests/release/test_package_rust.py -q
```

Result before the implementation change: **1 failed, 9 passed**. The clean
repository regression failed with `DID NOT RAISE ReleaseFailure`, proving that
the previous code performed only its initial clean check and could publish
after a later untracked-file change. The independent-output byte assertion
already passed, documenting the existing deterministic archive behavior.

### GREEN

Added the minimal final `_require_clean_repository(resolved_root)` call after
source freezing and workspace-bundle validation, immediately before publishing
the staged artifact. This reuses the existing structured failure path and
rejects staged, unstaged, and untracked changes introduced during packaging.

Commands:

```sh
uv run --project bindings/python pytest ../../tests/release/test_package_rust.py -q
uv run --project bindings/python pytest ../../tests/release -q
```

Results:

- `test_package_rust.py`: **10 passed**.
- Complete `tests/release`: **205 passed**.

### Review follow-up changed files

- `scripts/release/release_tool.py`
- `tests/release/test_package_rust.py`
- `.superpowers/sdd/task-1-report.md`
