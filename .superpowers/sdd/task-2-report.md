# Task 2 report: controlled Linux native prerequisites

## Outcome

Linux GPIO builds no longer depend on Ubuntu 24.04's `libgpiod-dev` 1.6.3.
`scripts/ci/install-linux-native-prerequisites.sh` downloads and verifies
libgpiod 2.2.1, installs it under a job-local prefix, and exposes that prefix
only through `PKG_CONFIG_PATH`. CI platform conformance and RC Linux platform
builds use this same script.

Source gates no longer install native prerequisites or run the Linux-native
ABI-dependent all-feature workspace checks. They retain formatting plus
`--no-default-features` Rust clippy and test coverage.

## RED

Command:

```sh
uv run --project bindings/python --python 3.11 --frozen pytest -q \
  tests/release/test_linux_prerequisites_contract.py \
  tests/release/test_workflow_contract.py
```

Result: **5 failed, 17 passed**. Failures proved the shared prerequisite
script did not exist, CI and RC still used distribution `libgpiod-dev`, and
the source gate still ran workspace all-feature clippy and tests.

## GREEN and verification

Commands:

```sh
uv run --project bindings/python --python 3.11 --frozen pytest -q \
  tests/release/test_linux_prerequisites_contract.py \
  tests/release/test_workflow_contract.py
shellcheck scripts/ci/install-linux-native-prerequisites.sh
bash -n scripts/ci/install-linux-native-prerequisites.sh
cargo +1.85 fmt --all --check
cargo +1.85 clippy --workspace --all-targets --no-default-features -- -D warnings
cargo +1.85 test --workspace --no-default-features
```

Results:

- Workflow and prerequisite contracts: **22 passed**.
- `shellcheck` and `bash -n`: passed.
- Rust formatting, no-default-features clippy, and no-default-features tests:
  passed.

The Linux Hosted CI-only script itself, production broker build, and virtual
broker build cannot run on this macOS worktree; their required execution paths
are wired in the Linux matrix jobs and will execute on GitHub-hosted Linux.

## Important review resolution: isolated runtime verification

`platform-verify` runs as an independent job from `platform-build`, so it does
not inherit the build job's job-local libgpiod prefix. Its Linux matrix entry
now invokes the same shared prerequisite script before it executes the
downloaded production broker.

The script verifies that the SHA-pinned libgpiod install produced
`$PREFIX/lib/libgpiod.so.3`, then exports only `$PREFIX/lib` through
`LD_LIBRARY_PATH` (including via `GITHUB_ENV` for subsequent workflow steps).
Thus the downloaded production broker resolves the verified libgpiod v2 ABI
explicitly, rather than accidentally using the runner's incompatible
distribution library. The prerequisite script continues to install only build
tools, libudev development files, pkg-config, and the pinned libgpiod source;
the release artifact does not carry arbitrary shared libraries.

### Review RED → GREEN

The added `test_linux_production_broker_jobs_pin_the_runtime_loader_to_libgpiod_v2`
initially failed because the script neither required `libgpiod.so.3` nor
exported `LD_LIBRARY_PATH`, and RC `platform-verify` did not invoke it.
After the minimal workflow and script changes, the focused contract suite
passed **23 tests**.

Additional local verification:

```sh
bash -n scripts/ci/install-linux-native-prerequisites.sh
uv run --project bindings/python --python 3.11 --frozen pytest -q tests/release
cargo +1.85 fmt --all --check
```

Results: shell parsing passed, the release suite passed **209 tests**, and
Rust formatting passed. `shellcheck` could not be run because it is not
installed on this macOS worktree (`command not found`); this is a local tool
availability limitation, not a passing result.

## Pinned upstream dependency

- libgpiod version: `2.2.1`
- archive: `libgpiod-2.2.1.tar.gz`
- SHA-256:
  `8f8f88f4ce764b02d03cc376f0a88cab028c63f94149e2cb5074301423f99098`
- source and release metadata:
  <https://www.kernel.org/pub/software/libs/libgpiod/>

The URL and SHA-256 are recorded directly in the installation script. The
script uses noninteractive APT with three retries and 30-second HTTP/HTTPS
timeouts, installs and preflights both `libudev-dev` and `pkg-config`, and
requires `pkg-config --exists 'libgpiod >= 2'` plus
`pkg-config --exists libudev`.

## Changed files

- `scripts/ci/install-linux-native-prerequisites.sh`
- `tests/release/test_linux_prerequisites_contract.py`
- `tests/release/test_workflow_contract.py`
- `.github/workflows/ci.yml`
- `.github/workflows/release-rc.yml`
- `.superpowers/sdd/task-2-report.md`

## Commit

- `329a477 ci: provision Linux native dependencies`
- Pending: fail-closed runtime resolution review fix

## Important review resolution: exact libgpiod archive digest

The prior SHA-256 value was not the digest of
`libgpiod-2.2.1.tar.gz`, so every Linux job failed deterministically at the
archive integrity check before it could build libgpiod. The static contract
test was first changed to require the exact `2.2.1` archive URL construction
and its corrected SHA-256; it failed against the old script value as expected.

The minimal script correction now pins:

```text
LIBGPIOD_VERSION=2.2.1
LIBGPIOD_SHA256=8f8f88f4ce764b02d03cc376f0a88cab028c63f94149e2cb5074301423f99098
```

No dynamic or unverified digest resolution was introduced. A fresh download
from the pinned kernel.org URL was independently hashed locally with
`shasum -a 256` and produced the same digest.

### Verification

```sh
uv run --project bindings/python --python 3.11 --frozen pytest -q \
  tests/release/test_linux_prerequisites_contract.py \
  tests/release/test_workflow_contract.py
bash -n scripts/ci/install-linux-native-prerequisites.sh
```

Result: **23 passed**; shell parsing passed.

---

# Task 2 Implementation Report

Status: complete with Hosted Windows runtime verification pending

## Outcome

Implemented Windows mapping coordination and named-object lifecycle in
`adapters/shared-memory/src/platform.rs`:

- `Mapping` now owns both the section and a separately derived named mutex.
  The mutex name is a SHA-256-derived `Local\seeed-hal-lock-…` name; it is
  distinct from the mapping name and never included in errors.
- The same protected DACL (`current user`, `SY`, and `BA`, protected with
  `D:P`) is passed to both `CreateFileMappingW` and `CreateMutexW`. A
  pre-existing mutex after section creation fails closed as `AlreadyExists`;
  all acquired handles are closed on that path.
- `open_read_only` retains Task 1's minimum section request
  (`FILE_MAP_READ | SECTION_QUERY`) and maps with only `FILE_MAP_READ`. It
  additionally opens the mutex with only `SYNCHRONIZE | MUTEX_MODIFY_STATE`,
  which is sufficient to wait and release without all-access rights.
- Both `try_lock_shared` and `try_lock_exclusive` use the same Windows mutex:
  `WaitForSingleObject(lock, 0)` is non-blocking, maps timeout to
  `WouldBlock`, rejects `WAIT_ABANDONED` fail-closed as `Other`, and preserves
  OS errors for other failures. Successful calls must be paired with
  `ReleaseMutex` through `unlock`.
- A Windows mutex is exclusive; this preserves the `Mapping` mutual-exclusion
  and bounded-contention contract, not concurrent OS shared-read performance.
- `Mapping::unlink` is deliberately a name-validated no-op lifecycle hook.
  Win32 named sections and mutexes have no Unix-style unlink: after
  `BrokerMapping::close` writes terminal state, the names retire automatically
  when the final broker and reader section/mutex handles are dropped. It never
  reopens or alters an arbitrary named object.
- Drop unmaps the view and closes the section and mutex handles.

## RED

Added Windows-only policy and process tests before production implementation:

- protected DACL verification for both section and mutex;
- read-only holder contention returns `WouldBlock`, then unlock restores
  exclusive progress;
- a child process exits while holding the mutex; the parent requires the
  resulting `WAIT_ABANDONED` to fail closed rather than grant ownership;
- after all broker/reader handles drop, a fresh create under the same name
  succeeds;
- malformed mapping names are rejected and the private derived lock name is
  distinct from the section name.

Command:

```sh
cargo test -p seeed-hal-adapter-shared-memory --target x86_64-pc-windows-msvc platform::windows_tests -- --nocapture
```

Result before production implementation: the test crate failed to compile
because `lock_name` and mutex-DACL test accessors were absent. This is the
requested reviewable RED evidence that the new Windows lock/lifecycle behavior
did not yet exist.

## GREEN and verification

Commands:

```sh
cargo fmt --all
cargo fmt --all --check
cargo check -p seeed-hal-adapter-shared-memory --target x86_64-pc-windows-msvc
cargo test -p seeed-hal-adapter-shared-memory
cargo clippy -p seeed-hal-adapter-shared-memory --all-targets --all-features -- -D warnings
git diff --check
```

Results:

- Windows target `cargo check` passed, including the production mutex and
  Windows-only test code.
- Native shared-memory adapter suite passed: **18 passed, 0 failed**.
- Formatting, focused clippy with warnings denied, and diff whitespace checks
  passed.

## Hosted Windows limitation

The requested runtime command was attempted after implementation:

```sh
cargo test -p seeed-hal-adapter-shared-memory --target x86_64-pc-windows-msvc platform::windows_tests -- --nocapture
```

It compiled the crate but failed during final linking because this macOS host
does not have `link.exe`:

```text
error: linker `link.exe` not found
```

Therefore no Windows runtime test has executed on this machine. In
particular, actual `CreateMutexW`/`OpenMutexW`, protected mutex DACL,
`WAIT_TIMEOUT`, `WAIT_ABANDONED`, and final-handle namespace retirement need
Hosted Windows execution before they can be represented as runtime-qualified.
The target check is compile-only evidence, not a substitute.

## Changed files

- `adapters/shared-memory/src/platform.rs`
- `.superpowers/sdd/task-2-report.md`

## Self-review

- Confirmed no mapping or mutex name is interpolated into production error
  messages.
- Confirmed the reader has no writable section mapping permission and no
  mutex all-access permission.
- Confirmed `unlink` validates only the private mapping-name grammar and
  leaves named-object deletion to Win32's final-handle retirement semantics.
- No controller-owned plan file was changed or staged.

---

## Important correctness resolution: abandoned mutex ownership and retirement reopen

`WaitForSingleObject(lock, 0)` returns `WAIT_ABANDONED` only after assigning
the mutex to the waiting thread. The previous fail-closed error path returned
without releasing that ownership, permanently blocking all subsequent callers.

### RED

Windows-only tests were written before the production correction:

- `abandoned_mutex_is_released_while_still_failing_closed` first has a child
  abandon the mutex, requires the parent to receive the fail-closed error, and
  then launches a distinct child handle which must acquire and release the
  mutex. This proves the error path does not leave it permanently
  `WouldBlock`.
- `names_retire_after_all_handles_drop_and_can_be_created_fresh` now requires
  an old mapping name to return `NotFound` from `Mapping::open_read_only`
  after all broker and reader handles have dropped, before it permits a fresh
  same-name create.

Attempted RED command:

```sh
cargo test -p seeed-hal-adapter-shared-memory --target x86_64-pc-windows-msvc \
  platform::windows_tests -- --nocapture
```

The test crate reached link but could not execute on this macOS host because
`link.exe` is unavailable. Therefore the tests were written first, but their
runtime RED result must be established on Hosted Windows.

### GREEN

The `WAIT_ABANDONED` branch now calls `ReleaseMutex` before it returns the
stable fail-closed `Other` error. If that release fails, it instead returns
only `last_os_error`, which does not disclose either private object name.

The Windows target compile check passed:

```sh
cargo check -p seeed-hal-adapter-shared-memory --target x86_64-pc-windows-msvc
```

Hosted Windows still needs to run the two tests above: this machine cannot
execute the test binary, so target checking is compile-only evidence.

---

## Important review resolution: abandoned reader must not prevent terminal close

`BrokerMapping::close` previously used the ordinary exclusive lock. That
correctly fails closed after `WAIT_ABANDONED`, but it also prevented close from
publishing the terminal header and invoking the existing local teardown hook.

### RED

Windows-only tests were added before the implementation:

- `abandoned_mutex_shared_lock_is_released_while_still_failing_closed` proves
  the ordinary shared lock returns `Other` after an abandoned mutex and releases
  the ownership it was granted, allowing a later exclusive lock to proceed.
- `teardown_lock_keeps_abandoned_ownership_until_unlocked` requires the new
  teardown-only API to retain abandoned ownership until its caller releases it;
  a separate child process observes `WouldBlock` meanwhile.
- `abandoned_reader_lock_allows_terminal_close_but_no_frame_recovery` creates a
  ring, has a reader child abandon its mutex ownership, then requires
  `BrokerMapping::close` to succeed and the already-open reader to return
  `None` for its lease. It therefore validates terminal closure rather than
  any recovery or reuse of possibly corrupted frame data.

The first RED check was:

```sh
cargo test -p seeed-hal-adapter-shared-memory --target x86_64-pc-windows-msvc \
  platform::windows_tests::teardown_lock_keeps_abandoned_ownership_until_unlocked --no-run
```

It failed as expected with `E0599`: `Mapping` had no
`try_lock_exclusive_for_teardown` method.

### GREEN and verification

`try_lock_exclusive_for_teardown` is an internal platform API. On Windows it
returns success for `WAIT_OBJECT_0` and `WAIT_ABANDONED`, retaining ownership
only until the caller's existing `unlock` performs `ReleaseMutex`; timeout
remains `WouldBlock` and all other waits preserve the native error. It is
called only by `BrokerMapping::close`, whose documented critical section writes
only `TERMINAL_STATE_CLOSED` before unlocking and invoking the existing
lifecycle hook. Ordinary shared/exclusive lock paths are unchanged and still
release abandoned ownership before returning the fail-closed `Other` error.

Commands:

```sh
cargo fmt --all --check
cargo test -p seeed-hal-adapter-shared-memory
cargo clippy -p seeed-hal-adapter-shared-memory --all-targets --all-features -- -D warnings
cargo check -p seeed-hal-adapter-shared-memory --target x86_64-pc-windows-msvc
git diff --check
```

Results: formatting, native adapter tests (**18 passed**), focused clippy, the
Windows target check, and diff whitespace verification passed. Source review
confirms the teardown-only API has one production caller: `BrokerMapping::close`;
all frame, pin, lease, reader, and writer paths retain ordinary lock calls.

### Hosted Windows limitation

The Windows test-binary build was attempted with:

```sh
cargo test -p seeed-hal-adapter-shared-memory --target x86_64-pc-windows-msvc \
  platform::windows_tests --no-run
```

It could not link on this macOS host because `link.exe` is unavailable. The
Windows target `cargo check` compiles the production and Windows-only test code,
but Hosted Windows must execute the abandoned shared/exclusive, teardown
ownership, and close-terminal-state tests to qualify their runtime behavior.

---

## Windows Hosted runtime gate in CI

The prior implementation was committed as `4efcc23` in unrelated `main` history.
That commit and its history were not cherry-picked or otherwise reused. This
branch-local change independently added the smallest CI contract and workflow
step needed to run the existing Windows shared-memory runtime tests on a Hosted
Windows matrix entry.

### RED

After adding the contract before changing CI, this command failed:

```sh
uv run --project bindings/python --python 3.11 --frozen pytest -q \
  tests/release/test_workflow_contract.py
```

Result: **1 failed, 24 passed**. The new contract failed with `StopIteration`
because `platform-conformance` had no `Run Windows shared-memory runtime tests`
step.

### GREEN and verification

`platform-conformance` now runs this Windows-only default-pwsh command before
`Build production broker`:

```text
cargo +1.85 test -p seeed-hal-adapter-shared-memory --all-features platform::windows_tests -- --nocapture
```

The workflow retains its top-level read-only permissions and existing
SHA-pinned actions. No Unix-specific shell is configured for this Windows step.

Commands:

```sh
uv run --project bindings/python --python 3.11 --frozen pytest -q \
  tests/release/test_workflow_contract.py
uv run --project bindings/python --python 3.11 --frozen pytest -q tests/release
cargo +1.85 fmt --all --check
git diff --check
```

Results:

- Focused workflow contracts: **25 passed**.
- Complete `tests/release`: **232 passed**.
- Rust formatting and diff whitespace checks: passed.

No existing release-suite failure occurred, so no unrelated test was modified.

### Branch commit

This branch-local implementation is committed as
`ci(shared-memory): gate Windows mapping runtime tests`.
