# Task 1 Implementation Report

Status: complete with Hosted Windows verification pending

---

# Windows shared-memory Task 1

## Outcome

Implemented protected named Windows sections in
`adapters/shared-memory` without changing the existing `Mapping` API:

- broker creation validates nonzero length, uses the protected
  `D:P(A;;GA;;;{current SID})(A;;GA;;;SY)(A;;GA;;;BA)` DACL, and closes the
  returned handle before reporting `ERROR_ALREADY_EXISTS` as
  `io::ErrorKind::AlreadyExists`;
- clients open the section read-only, query its actual length with
  `NtQuerySection(SectionBasicInformation)` before mapping, and reject zero,
  nonpositive, unrepresentable, or descriptor-shorter sections as
  `io::ErrorKind::InvalidData`;
- views are mapped at exactly the validated requested length. Mapping and
  handle cleanup covers every post-open/create error path.

Lock methods remain the pre-existing nonblocking `Unsupported` stubs as
required for Task 2 ownership/lifecycle work; no camera protocol, release,
or CI code changed.

## RED

Command:

```sh
cargo test -p robot-hal-adapter-shared-memory --target x86_64-pc-windows-msvc platform::windows_tests -- --nocapture
```

Result: expected failure before the implementation. The Windows-only policy
tests could not compile because `Mapping` lacked `dacl_sddl_for_test` and
`trustees_for_test`; the create/open backend was the deliberate
`Unsupported` stub. This establishes the requested policy API and behaviors
were absent.

## GREEN and verification

Commands:

```sh
cargo fmt --all --check
cargo clippy -p robot-hal-adapter-shared-memory --all-targets --all-features -- -D warnings
cargo test -p robot-hal-adapter-shared-memory
cargo check -p robot-hal-adapter-shared-memory --target x86_64-pc-windows-msvc
git diff --check
```

Results: all commands passed. The native adapter suite passed 18 tests. The
Windows target check passed and includes the production Windows source path.

Windows-only coverage added under `platform::windows_tests`:

- protected DACL SDDL prefix, exact three ACE count, and exact current-user /
  LocalSystem / Administrators trustee SID set;
- a repeated broker create maps `ERROR_ALREADY_EXISTS` to `AlreadyExists`;
- zero-length creation returns `InvalidInput`;
- a requested read-only length exceeding the actual section returns
  `InvalidData`.

## Hosted Windows limitation

This macOS machine has no `link.exe`; the requested Windows test command
reached linking but failed with “linker `link.exe` not found”. Therefore the
Windows runtime tests were not executed on a Hosted Windows runner. The
cross-target `cargo check` is compile-only evidence and must not be treated
as Hosted Windows API validation.

## Changed files

- `adapters/shared-memory/src/platform.rs`
- `adapters/shared-memory/Cargo.toml`

## Commit

`fc81a5a2877e69e22b690af34bbd92b9f3b533a7 feat(shared-memory): create protected Windows sections`

# Task 1 report: Rust workspace source bundle

## Outcome

`package-rust` now creates `robot-hal-crates-v0.5.0-rc.N.tar.gz` as a
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

---

# Task 1 review follow-up: Windows section query errors

## Outcome

Retained `FILE_MAP_READ | SECTION_QUERY` for the client section handle because
the Native Windows `NtQuerySection(SectionBasicInformation)` operation requires
`SECTION_QUERY`. `SECTION_QUERY` is used only for pre-map length validation:
the client view is always mapped with `MapViewOfFile(FILE_MAP_READ)` and never
requests `FILE_MAP_WRITE` or `FILE_MAP_ALL_ACCESS`.

`NtQuerySection` failures now remain query failures rather than being rewritten
as `InvalidData`: access denied is `PermissionDenied`, and other native-query,
ABI, or API failures are `Other`. Only valid query responses with zero,
nonpositive, unrepresentable, or descriptor-shorter lengths return
`InvalidData`. Error messages are generic and contain no NTSTATUS, mapping
name, path, SID, or token values.

## RED

Command:

```sh
cargo test -p robot-hal-adapter-shared-memory --target x86_64-pc-windows-msvc platform::windows_tests -- --nocapture
```

Result before the production fix: compilation failed because the new
`query_failure_is_not_disguised_as_an_invalid_section_length` test referenced
the absent `validate_section_length` behavior, and
`read_only_open_maps_a_read_only_view` referenced the absent view inspection
helper. This confirms both requested behaviors were not covered. A pre-existing
Windows-only `usize` conversion issue in the DACL test helper was corrected
while making that test target compile.

## GREEN and verification

Commands:

```sh
cargo fmt --all --check
cargo check -p robot-hal-adapter-shared-memory --target x86_64-pc-windows-msvc
cargo test -p robot-hal-adapter-shared-memory
cargo clippy -p robot-hal-adapter-shared-memory --all-targets --all-features -- -D warnings
git diff --check
```

Results: all commands passed. The native adapter suite passed 18 tests. The
Windows target check passed, including the new query-error and read-only-view
test code.

Windows-only tests added under `platform::windows_tests`:

- query failures preserve `PermissionDenied` rather than becoming `InvalidData`;
- a reader's mapped view reports `PAGE_READONLY` via `VirtualQuery`.

## Hosted Windows limitation

The focused Windows runtime command remains blocked on this macOS machine by
the missing `link.exe`; it compiles the test crate then fails at linking. Thus
the focused tests are compile-checked but have not run on Hosted Windows.
Normal mappings cannot construct zero or maximum section sizes, so the
zero/nonpositive/unrepresentable query-response branches still require Hosted
Windows fault injection to validate at runtime. The current tests must not be
represented as complete coverage of those native API failure and boundary
paths.
