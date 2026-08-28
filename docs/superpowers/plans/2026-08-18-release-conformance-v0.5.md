# Release Conformance v0.5 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development
> (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use
> checkbox (`- [ ]`) syntax for tracking.

**Goal:** Produce repeatable, independently verifiable `v0.5.0-rc.N` GitHub prerelease artifacts
for the three-platform broker, publishable Rust crates, and Python client.

**Architecture:** A Python 3.11 standard-library release tool owns version, matrix, packaging,
manifest, archive-safety, and offline verification rules. GitHub Actions calls that tool through
thin platform wrappers, separates source/platform/build/verify/release jobs, and grants release
permissions only after all downloaded artifacts pass clean verification.

**Tech Stack:** Rust 2024/MSRV 1.85, Python 3.11+ (`tomllib`, `tarfile`, `zipfile`, `hashlib`,
`json`), uv/hatchling, GitHub Actions hosted macOS/Linux/Windows runners, GitHub Artifact
Attestations, existing protobuf broker conformance runner.

**Spec:** `docs/superpowers/specs/2026-08-18-release-conformance-v0.5-design.md`

## Global Constraints

- Release/tag version is `0.5.0-rc.N`; Python metadata is the PEP 440 form `0.5.0rcN`.
- Broker, every publishable Rust package, Python project, artifact name, broker manifest, and release
  manifest must carry the same RC number.
- Wire contract remains major 1, inclusive minors `0..=3`; v0.5 adds no wire operation.
- Default production adapters are exact: macOS `serialport,nusb,avfoundation`; Linux
  `serialport,nusb,socketcan,linux-gpio,v4l2`; Windows
  `serialport,nusb,windows-gpio,mediafoundation`.
- PCAN is not in a default RC broker.
- No partial-platform prerelease is permitted.
- Public crates.io/PyPI publishing and Node/Electron bindings are out of scope.
- Every archive is verified without trusting its producer workspace; reject traversal, absolute
  paths, symlinks, duplicate members, unexpected files, and checksum mismatch.
- Manifests and logs exclude startup tokens, Camera mapping names/tokens, serial numbers, transient
  endpoints, and payload bytes.
- Physical qualification remains `Passed`, `Partial`, `Pending`, `Blocked`, or `Failed`; software
  CI never upgrades hardware status.
- PR/CI jobs use `contents: read`; only the final prerelease job receives `contents: write`,
  `id-token: write`, and `attestations: write`.
- All subprocesses and network/process conformance work have finite deadlines.

---

## Planned File Structure

- `release/targets.toml`: hosted runner, target triple, archive format, features, required adapters.
- `scripts/release/release_tool.py`: shared cross-platform release CLI and validated data model.
- `scripts/release/check-version.sh`: POSIX wrapper for the version subcommand.
- `scripts/release/check-version.ps1`: Windows wrapper for the same subcommand.
- `tests/release/`: version, matrix, manifest, archive, packaging, workflow, and end-to-end tests.
- `tests/release/fixtures/`: minimal deterministic valid/invalid artifact trees.
- `scripts/release/package-broker.sh` and `.ps1`: platform invocation wrappers.
- `scripts/release/package-rust.sh`: publishable crate packaging wrapper.
- `scripts/release/package-python.sh` and `.ps1`: wheel/sdist packaging wrappers.
- `scripts/release/generate-manifest.sh` and `.ps1`: manifest/checksum wrappers.
- `scripts/release/verify-artifacts.sh` and `.ps1`: offline verification wrappers.
- `.github/workflows/ci.yml`: source gate plus three-platform production/virtual conformance.
- `.github/workflows/release-rc.yml`: immutable RC build, clean verify, attest, prerelease.
- `docs/contracts/release-artifacts.md`: normative artifact and failure contract.
- `docs/releases/v0.5.0-rc-qualification.md`: software evidence and separate hardware status.

### Task 1: Define the unified RC version and release target matrix

**Files:**
- Create: `release/targets.toml`
- Create: `scripts/release/release_tool.py`
- Create: `scripts/release/check-version.sh`
- Create: `scripts/release/check-version.ps1`
- Create: `tests/release/test_version.py`
- Create: `tests/release/test_targets.py`
- Modify: `Cargo.toml`
- Modify: every workspace member `Cargo.toml`
- Modify: `bindings/python/pyproject.toml`
- Modify: `bindings/python/robot_hal/__init__.py`
- Modify: `Cargo.lock`

**Interfaces:**
- Produces `ReleaseVersion.parse(tag: str) -> ReleaseVersion`, with Cargo `0.5.0-rc.N` and Python
  `0.5.0rcN` representations.
- Produces `load_targets(path: Path) -> tuple[ReleaseTarget, ...]`.
- Produces CLI `release_tool.py check-version --tag v0.5.0-rc.N --repo-root PATH`.
- `ReleaseTarget` fields are `name`, `runner`, `triple`, `archive`, `features`,
  `required_adapters`.

- [ ] **Step 1: Write failing version and target tests**

```python
def test_rc_version_normalizes_cargo_and_python() -> None:
    version = ReleaseVersion.parse("v0.5.0-rc.7")
    assert version.cargo == "0.5.0-rc.7"
    assert version.python == "0.5.0rc7"


@pytest.mark.parametrize("value", ["0.5.0-rc.1", "v0.5.0", "v0.5.0-rc.0", "v0.5.0-rc.x"])
def test_rc_version_rejects_noncanonical_tags(value: str) -> None:
    with pytest.raises(ReleaseFailure, match="release.version.invalid"):
        ReleaseVersion.parse(value)


def test_target_matrix_has_exact_default_compositions() -> None:
    targets = {target.name: target for target in load_targets(TARGETS)}
    assert targets["macos"].required_adapters == (
        "avfoundation", "nusb", "serialport"
    )
    assert targets["linux"].required_adapters == (
        "linux-gpio", "nusb", "serialport", "socketcan", "v4l2"
    )
    assert targets["windows"].required_adapters == (
        "mediafoundation", "nusb", "serialport", "windows-gpio"
    )
```

- [ ] **Step 2: Run tests and verify RED**

Run:

```bash
uv run --project bindings/python --python 3.11 --frozen \
  pytest -q tests/release/test_version.py tests/release/test_targets.py
```

Expected: collection/import failure because `scripts.release.release_tool` and
`release/targets.toml` do not exist.

- [ ] **Step 3: Implement the validated version and matrix model**

Use only the Python standard library. Errors print one stable name followed by a bounded diagnostic:

```python
RC_TAG = re.compile(r"^v0\.5\.0-rc\.([1-9][0-9]*)$")


@dataclass(frozen=True)
class ReleaseVersion:
    rc: int

    @classmethod
    def parse(cls, tag: str) -> "ReleaseVersion":
        match = RC_TAG.fullmatch(tag)
        if match is None:
            raise ReleaseFailure("release.version.invalid", "expected v0.5.0-rc.N")
        return cls(rc=int(match.group(1)))

    @property
    def cargo(self) -> str:
        return f"0.5.0-rc.{self.rc}"

    @property
    def python(self) -> str:
        return f"0.5.0rc{self.rc}"
```

`release/targets.toml` must contain exactly:

```toml
schema = 1

[[target]]
name = "macos"
runner = "macos-14"
triple = "aarch64-apple-darwin"
archive = "tar.gz"
features = ["serialport", "nusb", "avfoundation"]
required_adapters = ["avfoundation", "nusb", "serialport"]

[[target]]
name = "linux"
runner = "ubuntu-24.04"
triple = "x86_64-unknown-linux-gnu"
archive = "tar.gz"
features = ["serialport", "nusb", "socketcan", "linux-gpio", "v4l2"]
required_adapters = ["linux-gpio", "nusb", "serialport", "socketcan", "v4l2"]

[[target]]
name = "windows"
runner = "windows-2025"
triple = "x86_64-pc-windows-msvc"
archive = "zip"
features = ["serialport", "nusb", "windows-gpio", "mediafoundation"]
required_adapters = ["mediafoundation", "nusb", "serialport", "windows-gpio"]
```

- [ ] **Step 4: Unify package versions**

Add `version = "0.5.0-rc.1"` under `[workspace.package]`; replace each Rust member's literal
`version` with `version.workspace = true`. Keep `apps/robot-hal-broker` as an executable artifact,
not a crates.io package, with `publish = false`. All library and adapter crates remain packageable.
Every path dependency between packageable crates receives `version = "=0.5.0-rc.1"` in addition to
`path`. Set Python project version to `0.5.0rc1` and expose:

```python
from importlib.metadata import version as _distribution_version

__version__ = _distribution_version("robot-hal")
```

Add `"__version__"` to `__all__`. Regenerate `Cargo.lock`.

- [ ] **Step 5: Implement repository-wide consistency checking**

`check-version` reads `cargo metadata --no-deps`, `pyproject.toml`, and the imported Python
distribution metadata fixture. It rejects a package mismatch as:

```text
release.version.mismatch: robot-hal-runtime is 0.4.0, expected 0.5.0-rc.1
```

Wrappers must only locate the repository and call:

```bash
python3 scripts/release/release_tool.py check-version \
  --tag "${1:?release tag required}" --repo-root "$repo_root"
```

- [ ] **Step 6: Run focused and workspace validation**

```bash
uv run --project bindings/python --python 3.11 --frozen \
  pytest -q tests/release/test_version.py tests/release/test_targets.py
python3 scripts/release/release_tool.py check-version \
  --tag v0.5.0-rc.1 --repo-root .
cargo metadata --no-deps --format-version 1 >/dev/null
cargo test -p robot-hal-broker-app --test manifest
uv run --project bindings/python --python 3.11 --frozen \
  pytest -q bindings/python/tests
```

Expected: all pass; manifest test must be updated from `0.2.0` to `0.5.0-rc.1`.

- [ ] **Step 7: Commit**

```bash
git add Cargo.toml Cargo.lock adapters crates apps bindings release scripts/release tests/release
git commit -m "build(release): unify v0.5 release candidate versions"
```

### Task 2: Validate production broker composition against the matrix

**Files:**
- Modify: `apps/robot-hal-broker/src/manifest.rs`
- Modify: `apps/robot-hal-broker/tests/manifest.rs`
- Modify: `apps/robot-hal-broker/Cargo.toml`
- Modify: `scripts/release/release_tool.py`
- Create: `tests/release/test_broker_manifest.py`

**Interfaces:**
- Produces CLI:
  `verify-broker-manifest --manifest FILE --target NAME --targets release/targets.toml --artifact FILE`.
- The verifier compares broker version, wire `1/0..=3`, target triple, MSRV `1.85`, exact features,
  exact adapters, vendor runtime list, and executable SHA-256.

- [ ] **Step 1: Write failing manifest-verifier tests**

Use a JSON fixture with one-field mutations:

```python
def test_manifest_rejects_missing_required_adapter(tmp_path: Path) -> None:
    manifest = valid_manifest("linux")
    manifest["enabled"]["adapters"].remove("v4l2")
    error = verify_broker_manifest(manifest, target("linux"), ARTIFACT)
    assert error.name == "release.manifest.invalid"


def test_manifest_rejects_extra_default_pcan(tmp_path: Path) -> None:
    manifest = valid_manifest("windows")
    manifest["enabled"]["adapters"].append("pcan")
    with pytest.raises(ReleaseFailure, match="release.manifest.invalid"):
        verify_broker_manifest(manifest, target("windows"), ARTIFACT)
```

- [ ] **Step 2: Verify RED**

```bash
uv run --project bindings/python --python 3.11 --frozen \
  pytest -q tests/release/test_broker_manifest.py
```

Expected: failure because the verifier does not exist.

- [ ] **Step 3: Make broker manifest output canonical**

Sort both `enabled.adapters` and `enabled.features`; include no machine-local path or timestamp.
Keep `required_vendor_runtime_libraries` empty for default targets. Update Rust manifest tests to
assert `0.5.0-rc.1`, exact sorted lists, and checksum.

- [ ] **Step 4: Implement strict manifest verification**

Reject unknown top-level fields only when the manifest schema major is unsupported; otherwise
require every current field and compare exact lists. Never infer a missing adapter from target OS.

- [ ] **Step 5: Validate each host-buildable composition**

On macOS:

```bash
cargo build -p robot-hal-broker-app --no-default-features \
  --features serialport,nusb,avfoundation
target/debug/robot-hal-broker --manifest > target/macos-manifest.json
python3 scripts/release/release_tool.py verify-broker-manifest \
  --manifest target/macos-manifest.json --target macos \
  --targets release/targets.toml --artifact target/debug/robot-hal-broker
```

In hosted Linux/Windows jobs, run the equivalent matrix-derived commands. Locally, run the Rust
manifest tests for non-host branches.

- [ ] **Step 6: Commit**

```bash
git add apps/robot-hal-broker scripts/release/release_tool.py tests/release
git commit -m "test(release): verify broker composition manifests"
```

### Task 3: Parameterize wire-minor conformance and fail-closed compatibility

**Files:**
- Modify: `tests/conformance/run-broker-conformance.py`
- Modify: `tests/conformance/test_runner_contract.py`
- Modify: `tests/conformance/README.md`
- Create: `tests/conformance/test_minor_matrix.py`

**Interfaces:**
- Produces CLI options `--protocol-minor {0,1,2,3}` and repeatable `--require-capability`.
- Produces `capabilities_for_minor(minor: int) -> tuple[str, ...]`.
- `exercise_contract` executes only operations introduced at or below the selected minor and then
  probes one later-minor operation for structured fail-closed rejection.

- [ ] **Step 1: Write failing compatibility tests**

```python
def test_capability_matrix_is_additive() -> None:
    assert set(capabilities_for_minor(0)) == {"serial.bytes/v1"}
    assert {"can.classic/v1", "can.fd/v1"} <= set(capabilities_for_minor(1))
    assert {"usb.control/v1", "gpio.lines/v1"} <= set(capabilities_for_minor(2))
    assert {"camera.capture/v1", "camera.frames.shm/v1"} <= set(
        capabilities_for_minor(3)
    )


@pytest.mark.parametrize(
    ("minor", "later_payload"),
    [(0, "enumerate_can_request"), (1, "enumerate_usb_request"), (2, "enumerate_camera_request")],
)
def test_lower_minor_selects_a_later_operation_probe(minor: int, later_payload: str) -> None:
    assert later_operation_for_minor(minor) == later_payload
```

- [ ] **Step 2: Verify RED**

```bash
uv run --project bindings/python --python 3.11 --frozen \
  pytest -q tests/conformance/test_runner_contract.py \
    tests/conformance/test_minor_matrix.py
```

Expected: missing functions/options.

- [ ] **Step 3: Refactor handshake and exercise profiles**

Remove fixed minor constants from behavior. `RawClient.handshake` accepts:

```python
async def handshake(
    self,
    token: bytes,
    *,
    minor: int,
    required_capabilities: tuple[str, ...],
) -> None:
```

For each minor, assert the selected minor exactly equals the offered exact minor. Probe a later
request and require `runtime.protocol.version_incompatible` or the existing stable minor-gate error;
the connection must remain well-defined according to the broker contract.

- [ ] **Step 4: Run all four black-box profiles**

```bash
cargo build -p robot-hal-broker-app --features virtual-adapters
for minor in 0 1 2 3; do
  uv run --project bindings/python --python 3.11 --frozen python \
    tests/conformance/run-broker-conformance.py \
    --broker target/debug/robot-hal-broker --protocol-minor "$minor"
done
```

Expected: all pass; minor 3 still covers complete Serial/CAN/USB/GPIO/Camera flows.

- [ ] **Step 5: Commit**

```bash
git add tests/conformance
git commit -m "test(protocol): cover wire minor compatibility matrix"
```

### Task 4: Implement deterministic release manifests and safe archive inspection

**Files:**
- Modify: `scripts/release/release_tool.py`
- Create: `tests/release/test_manifest.py`
- Create: `tests/release/test_archive_safety.py`
- Create: `tests/release/fixtures/valid/`
- Create: `tests/release/fixtures/qualification.json`

**Interfaces:**
- Produces `ArtifactRecord`, `ReleaseManifest`, `ConformanceReport`, and `QualificationStatus`.
- Produces CLI `generate-manifest --tag --commit --artifacts-dir --output-dir`.
- Produces CLI `verify-static --artifacts-dir --manifest --checksums`.
- JSON output uses UTF-8, `sort_keys=True`, compact separators, and a trailing newline.

- [ ] **Step 1: Write failing deterministic-manifest tests**

```python
def test_manifest_generation_is_byte_deterministic(tmp_path: Path) -> None:
    first = generate_manifest(inputs(tmp_path))
    second = generate_manifest(inputs(tmp_path))
    assert encode_manifest(first) == encode_manifest(second)


@pytest.mark.parametrize("forbidden", ["startup_token", "mapping_name", "serial_number", "payload"])
def test_manifest_rejects_sensitive_field_names(forbidden: str) -> None:
    data = valid_manifest_dict()
    data[forbidden] = "secret"
    with pytest.raises(ReleaseFailure, match="release.manifest.invalid"):
        validate_release_manifest(data)
```

- [ ] **Step 2: Write failing archive-safety tests**

Build tar/zip fixtures containing `../escape`, `/absolute`, a symlink, duplicate member, and an
unexpected executable. Each must raise `release.archive.invalid` before extraction.

- [ ] **Step 3: Verify RED**

```bash
uv run --project bindings/python --python 3.11 --frozen \
  pytest -q tests/release/test_manifest.py tests/release/test_archive_safety.py
```

- [ ] **Step 4: Implement deterministic models and checksum generation**

Artifact order is lexical by name. `SHA256SUMS` lines are:

```text
<64 lowercase hex><two spaces><basename>\n
```

Reject nested artifact names and any basename not matching a type-specific allowlist.

- [ ] **Step 5: Implement no-extract archive validation**

Inspect all members before extracting. For tar, reject `issym()`, `islnk()`, devices, and non-file
members except required directories. For zip, reject Unix symlink mode bits. Normalize with
`PurePosixPath` and require one relative component tree rooted at the expected package directory.

- [ ] **Step 6: Run focused tests**

```bash
uv run --project bindings/python --python 3.11 --frozen \
  pytest -q tests/release/test_manifest.py tests/release/test_archive_safety.py
```

- [ ] **Step 7: Commit**

```bash
git add scripts/release tests/release
git commit -m "feat(release): generate and validate artifact manifests"
```

### Task 5: Package and verify platform broker archives

**Files:**
- Modify: `scripts/release/release_tool.py`
- Create: `scripts/release/package-broker.sh`
- Create: `scripts/release/package-broker.ps1`
- Create: `tests/release/test_package_broker.py`

**Interfaces:**
- Produces CLI:
  `package-broker --tag --target --targets --binary --output-dir --manifest`.
- Archive contains only `robot-hal-broker[.exe]`, `broker-manifest.json`, `LICENSE`, and
  `README.md` below one versioned root directory.

- [ ] **Step 1: Write failing package-content tests**

```python
def test_broker_archive_has_exact_files(tmp_path: Path) -> None:
    archive = package_fixture_broker(tmp_path, target="linux")
    assert archive_members(archive) == (
        "robot-hal-broker-v0.5.0-rc.1-x86_64-unknown-linux-gnu/LICENSE",
        "robot-hal-broker-v0.5.0-rc.1-x86_64-unknown-linux-gnu/README.md",
        "robot-hal-broker-v0.5.0-rc.1-x86_64-unknown-linux-gnu/broker-manifest.json",
        "robot-hal-broker-v0.5.0-rc.1-x86_64-unknown-linux-gnu/robot-hal-broker",
    )
```

- [ ] **Step 2: Verify RED**

```bash
uv run --project bindings/python --python 3.11 --frozen \
  pytest -q tests/release/test_package_broker.py
```

- [ ] **Step 3: Implement deterministic broker packaging**

For tar, normalize uid/gid/user/group/mtime and file modes. For zip, normalize timestamp and external
attributes. Before packaging, run `verify-broker-manifest` against the binary and matrix.

- [ ] **Step 4: Package the local macOS broker and verify it**

```bash
cargo build --release -p robot-hal-broker-app --no-default-features \
  --features serialport,nusb,avfoundation
target/release/robot-hal-broker --manifest > target/release/broker-manifest.json
scripts/release/package-broker.sh v0.5.0-rc.1 macos \
  target/release/robot-hal-broker target/release/broker-manifest.json target/release-artifacts
python3 scripts/release/release_tool.py verify-static \
  --artifacts-dir target/release-artifacts
```

- [ ] **Step 5: Commit**

```bash
git add scripts/release tests/release
git commit -m "feat(release): package verified broker artifacts"
```

### Task 6: Package Rust crates and Python distributions

**Files:**
- Modify: `scripts/release/release_tool.py`
- Create: `scripts/release/package-rust.sh`
- Create: `scripts/release/package-python.sh`
- Create: `scripts/release/package-python.ps1`
- Create: `tests/release/test_package_rust.py`
- Create: `tests/release/test_package_python.py`
- Modify: `bindings/python/pyproject.toml`

**Interfaces:**
- Produces CLI `package-rust --tag --repo-root --output-dir`.
- Produces CLI `package-python --tag --project --output-dir`.
- Rust bundle includes exactly one `.crate` for every workspace package with `publish != false`,
  sorted by package name.
- Python output is one `py3-none-any` wheel and one sdist.

- [ ] **Step 1: Write failing publishable-package tests**

```python
def test_rust_bundle_contains_every_publishable_package(metadata: dict) -> None:
    expected = sorted(
        f'{package["name"]}-0.5.0-rc.1.crate'
        for package in metadata["packages"]
        if package["publish"] != []
    )
    assert rust_bundle_members(BUNDLE) == tuple(expected)


def test_python_artifact_names_use_pep440() -> None:
    assert wheel_name(ReleaseVersion.parse("v0.5.0-rc.3")) == (
        "robot_hal-0.5.0rc3-py3-none-any.whl"
    )
```

- [ ] **Step 2: Verify RED**

```bash
uv run --project bindings/python --python 3.11 --frozen \
  pytest -q tests/release/test_package_rust.py tests/release/test_package_python.py
```

- [ ] **Step 3: Implement Rust packaging**

Use `cargo metadata` to topologically sort packageable members. Run:

```bash
cargo package -p "$package" --locked --allow-dirty
```

Copy each resulting `.crate` to a staging directory, inspect members with the archive safety code,
and create a deterministic outer tarball. `--allow-dirty` is permitted only because the version
consistency test verifies the exact checkout and the workflow rejects any uncommitted files before
packaging; local dry-run must also require `git diff --quiet`.

- [ ] **Step 4: Implement Python packaging**

Add pinned `build==1.3.0` to the dev group using uv. Run:

```bash
uv run --project bindings/python --frozen python -m build \
  --outdir "$output_dir" bindings/python
```

Inspect wheel/sdist metadata and reject version, name, tag, package-file, or generated protobuf
mismatch.

- [ ] **Step 5: Verify packaged contents**

Unpack each `.crate` to a temporary directory and run `cargo check --locked` from its packaged
manifest with a bounded subprocess timeout. Install the wheel into a temporary venv and require:

```python
import importlib.metadata
import robot_hal

assert importlib.metadata.version("robot-hal") == "0.5.0rc1"
assert robot_hal.__version__ == "0.5.0rc1"
```

- [ ] **Step 6: Run focused tests and package dry-run**

```bash
uv run --project bindings/python --python 3.11 --frozen \
  pytest -q tests/release/test_package_rust.py tests/release/test_package_python.py
scripts/release/package-rust.sh v0.5.0-rc.1 target/release-artifacts
scripts/release/package-python.sh v0.5.0-rc.1 target/release-artifacts
```

- [ ] **Step 7: Commit**

```bash
git add Cargo.toml Cargo.lock bindings/python scripts/release tests/release
git commit -m "feat(release): package Rust and Python clients"
```

### Task 7: Add complete offline verification and conformance reports

**Files:**
- Modify: `scripts/release/release_tool.py`
- Create: `scripts/release/generate-manifest.sh`
- Create: `scripts/release/generate-manifest.ps1`
- Create: `scripts/release/verify-artifacts.sh`
- Create: `scripts/release/verify-artifacts.ps1`
- Create: `tests/release/test_verify_artifacts.py`
- Create: `tests/release/test_conformance_report.py`
- Create: `docs/releases/v0.5.0-rc-qualification.md`

**Interfaces:**
- Produces CLI `write-conformance-report --inputs DIR --output FILE`.
- Produces CLI `verify-artifacts --tag --artifacts-dir --targets --repo-root`.
- Verification returns success only when all three broker targets, Rust bundle, wheel, sdist,
  checksums, release manifest, and conformance report are present and mutually consistent.

- [ ] **Step 1: Write failing complete-set and qualification tests**

```python
def test_verify_rejects_partial_platform_set(tmp_path: Path) -> None:
    copy_valid_set(tmp_path, omit="windows")
    with pytest.raises(ReleaseFailure, match="release.artifact.unexpected"):
        verify_artifacts(tmp_path, VERSION, TARGETS)


def test_software_report_cannot_promote_pending_hardware() -> None:
    report = valid_report()
    report["hardware"]["camera-avfoundation"]["status"] = "Passed"
    report["hardware"]["camera-avfoundation"]["evidence"] = None
    with pytest.raises(ReleaseFailure, match="release.manifest.invalid"):
        validate_conformance_report(report)
```

- [ ] **Step 2: Verify RED**

```bash
uv run --project bindings/python --python 3.11 --frozen \
  pytest -q tests/release/test_verify_artifacts.py \
    tests/release/test_conformance_report.py
```

- [ ] **Step 3: Implement complete offline verification**

Verification uses a fresh temporary directory, validates every archive before extraction, compares
both checksum sources, runs the extracted broker `--manifest`, and executes minor 0–3 virtual
conformance against the extracted broker. On a host that cannot execute another target's binary,
static verification still runs locally and executable verification is required in that target's
hosted job; the release manifest records both result types.

- [ ] **Step 4: Define the qualification evidence template**

`docs/releases/v0.5.0-rc-qualification.md` has separate tables for source gate, hosted platform
conformance, artifact verification, attestation, and external hardware qualification. Initial
hardware entries link existing v0.1–v0.4 runbooks and retain their factual statuses.

- [ ] **Step 5: Run the release test suite**

```bash
uv run --project bindings/python --python 3.11 --frozen \
  pytest -q tests/release
```

- [ ] **Step 6: Commit**

```bash
git add scripts/release tests/release docs/releases/v0.5.0-rc-qualification.md
git commit -m "feat(release): verify complete RC artifact sets"
```

### Task 8: Add GitHub Actions source and platform conformance

**Files:**
- Create: `.github/workflows/ci.yml`
- Create: `tests/release/test_workflow_contract.py`
- Modify: `bindings/python/pyproject.toml`
- Modify: `bindings/python/uv.lock`
- Modify: `tests/conformance/README.md`
- Modify: `README.md`

**Interfaces:**
- PR/push workflow jobs: `source-gate`, `platform-conformance`.
- Platform matrix is generated from `release/targets.toml`; workflow does not contain a second
  adapter list.
- Workflow uploads conformance JSON only as test evidence, not as a release.

- [ ] **Step 1: Add the workflow-test parser and write failing least-privilege tests**

Run `uv add --project bindings/python --dev PyYAML` so the package manager selects and locks the
current compatible version. Parse workflow YAML with `yaml.safe_load`, after replacing the YAML
1.1-sensitive top-level `on:` key with a quoted `"on":` key in the in-memory test input:

```python
def load_workflow(name: str) -> dict:
    text = (WORKFLOWS / name).read_text(encoding="utf-8")
    normalized = re.sub(r"(?m)^on:", '"on":', text)
    return yaml.safe_load(normalized)


def test_ci_has_read_only_permissions() -> None:
    workflow = load_workflow("ci.yml")
    assert workflow["permissions"] == {"contents": "read"}


def test_ci_does_not_publish_or_request_oidc() -> None:
    text = CI.read_text()
    for forbidden in ("cargo publish", "twine upload", "id-token: write", "packages: write"):
        assert forbidden not in text
```

- [ ] **Step 2: Verify RED**

```bash
uv run --project bindings/python --python 3.11 --frozen \
  pytest -q tests/release/test_workflow_contract.py
```

- [ ] **Step 3: Implement `ci.yml`**

`source-gate` runs generated protocol, fmt, clippy, workspace tests, Python tests, release tests,
and minor matrix tests. `platform-conformance` runs on each hosted OS, installs platform build
prerequisites, derives feature arguments from `release_tool.py print-target`, builds production and
virtual brokers, verifies the production manifest, and runs minor 0–3 black-box conformance.

Every job uses:

```yaml
permissions:
  contents: read
timeout-minutes: 45
```

Pin official actions to reviewed major versions and set `persist-credentials: false` on checkout.

- [ ] **Step 4: Validate workflow contract and local source gate**

```bash
uv run --project bindings/python --python 3.11 --frozen \
  pytest -q tests/release/test_workflow_contract.py
./scripts/check-generated-protocol.sh
cargo fmt --all --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace --all-features
uv run --project bindings/python --python 3.11 --frozen pytest -q
```

- [ ] **Step 5: Commit**

```bash
git add .github/workflows/ci.yml bindings/python/pyproject.toml bindings/python/uv.lock \
  tests/release tests/conformance/README.md README.md
git commit -m "ci: add three-platform release conformance"
```

### Task 9: Add immutable RC build, verification, attestation, and prerelease workflow

**Files:**
- Create: `.github/workflows/release-rc.yml`
- Modify: `tests/release/test_workflow_contract.py`
- Modify: `scripts/release/release_tool.py`
- Create: `docs/contracts/release-artifacts.md`

**Interfaces:**
- Workflow inputs: `version` with exact `v0.5.0-rc.N`; optional `dry_run` boolean.
- Jobs: `validate`, `platform-build`, `client-build`, `platform-verify`, `aggregate`,
  `attest-and-release`.
- `dry_run=true` executes through aggregate verification but skips attest/release.

- [ ] **Step 1: Write failing release-workflow tests**

```python
def test_release_permissions_exist_only_on_final_job() -> None:
    workflow = load_workflow("release-rc.yml")
    assert workflow["jobs"]["attest-and-release"]["permissions"] == {
        "attestations": "write",
        "contents": "write",
        "id-token": "write",
    }
    for name, job in workflow["jobs"].items():
        if name != "attest-and-release":
            assert "contents: write" not in dump(job)


def test_release_cannot_publish_to_public_registries() -> None:
    text = RELEASE.read_text()
    for forbidden in ("cargo publish", "twine upload", "maturin publish", "PYPI", "CARGO_REGISTRY_TOKEN"):
        assert forbidden not in text
```

- [ ] **Step 2: Verify RED**

```bash
uv run --project bindings/python --python 3.11 --frozen \
  pytest -q tests/release/test_workflow_contract.py
```

- [ ] **Step 3: Implement validate and build jobs**

Validate canonical version, clean Git commit/tag relationship, absence of an existing release, and
source gates. Platform jobs build/package only their target broker. Client job packages Rust/Python.
Artifacts are uploaded under unique immutable names containing tag and commit SHA.

- [ ] **Step 4: Implement clean verification and aggregation**

Each platform verification job downloads only source-independent artifacts, verifies and executes
its broker, and emits signed-off conformance input JSON. Aggregate downloads all artifacts,
constructs `release-manifest.json`, `conformance-report.json`, and `SHA256SUMS`, then runs full static
verification.

- [ ] **Step 5: Implement attestation and prerelease**

Final job condition:

```yaml
if: ${{ !inputs.dry_run }}
permissions:
  contents: write
  id-token: write
  attestations: write
```

Use GitHub's official artifact attestation action with `subject-path` covering each final file.
Create a prerelease only after attestations succeed. Reject an existing tag/release; never pass a
force or overwrite option.

- [ ] **Step 6: Write the normative artifact contract**

Document exact names, contents, deterministic metadata, dual checksums, broker/release manifest
relationship, attestation verification with `gh attestation verify`, immutable retry semantics,
error names, prohibited secrets, and separate hardware qualification.

- [ ] **Step 7: Run workflow and release tests**

```bash
uv run --project bindings/python --python 3.11 --frozen \
  pytest -q tests/release
python3 scripts/release/release_tool.py check-version \
  --tag v0.5.0-rc.1 --repo-root .
```

- [ ] **Step 8: Commit**

```bash
git add .github/workflows/release-rc.yml scripts/release tests/release \
  docs/contracts/release-artifacts.md
git commit -m "ci(release): build and attest v0.5 prereleases"
```

### Task 10: Run end-to-end dry-run, review, and record v0.5 RC evidence

**Files:**
- Modify: `docs/releases/v0.5.0-rc-qualification.md`
- Modify: `docs/architecture/hal-architecture.md`
- Modify: `docs/contracts/versioning.md`
- Modify: `README.md`

**Interfaces:**
- Produces one local dry-run artifact set under `target/release-artifacts/`.
- Produces factual local evidence only; hosted macOS/Linux/Windows rows remain pending until their
  actual GitHub jobs run.

- [ ] **Step 1: Run all source and release tests**

```bash
./scripts/check-generated-protocol.sh
cargo fmt --all --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace --all-features
uv run --project bindings/python --python 3.11 --frozen pytest -q
```

- [ ] **Step 2: Run local RC dry-run**

```bash
rm -rf target/release-artifacts
scripts/release/package-broker.sh v0.5.0-rc.1 macos \
  target/release/robot-hal-broker target/release/broker-manifest.json \
  target/release-artifacts
scripts/release/package-rust.sh v0.5.0-rc.1 target/release-artifacts
scripts/release/package-python.sh v0.5.0-rc.1 target/release-artifacts
scripts/release/generate-manifest.sh v0.5.0-rc.1 HEAD target/release-artifacts
scripts/release/verify-artifacts.sh v0.5.0-rc.1 target/release-artifacts
```

Expected: local macOS executable verification passes; Linux/Windows hosted execution remains
explicitly pending, so this command is a dry-run and cannot create a prerelease.

- [ ] **Step 3: Perform a defect-first final review**

Review the full branch for:

- inconsistent package/tag/Python versions;
- duplicated target/adapter matrices;
- archive extraction before validation;
- unchecked subprocess timeouts;
- partial-platform release paths;
- excessive GitHub permissions;
- crates.io/PyPI publication paths;
- manifests containing secrets/endpoints/payloads;
- software evidence represented as hardware qualification.

Fix every Critical/Important finding and rerun its focused tests.

- [ ] **Step 4: Update factual documentation**

Architecture and README describe v0.5 as release/conformance infrastructure, not a new HAL feature.
Versioning links the artifact contract. Qualification records exact local commands, counts, host,
commit, and pending hosted/hardware gates. Do not state that GitHub-hosted jobs or attestations
passed before an actual workflow run.

- [ ] **Step 5: Run final verification**

```bash
git diff --check
git status --short
python3 scripts/release/release_tool.py check-version \
  --tag v0.5.0-rc.1 --repo-root .
uv run --project bindings/python --python 3.11 --frozen pytest -q tests/release
```

- [ ] **Step 6: Commit**

```bash
git add README.md docs
git commit -m "docs(release): record v0.5 RC qualification"
```

Do not create a Git tag, push, attest, or create a GitHub prerelease unless the user explicitly
requests those external mutations after reviewing the dry-run evidence.
