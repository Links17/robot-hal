# Task 10 Report — factual v0.5 RC qualification record

## Scope and identity

- **Recorded:** 2026-08-18
- **Task:** Task 10, end-to-end dry-run, review, and v0.5 RC evidence
- **Evidence-run commit:** `8db88edf867abc1dcbef53331bed24c072e74f76`
- **Documentation-record commit:** `a3dff80 docs(release): record v0.5 RC qualification`
- **Candidate tag:** `v0.5.0-rc.1`; no Git tag was created
- **Local host:** macOS arm64 (`Darwin arm64`, product version `26.5.2`)
- **Artifact root:** `target/release-artifacts/` (local, untracked release output)

This is an audit record of evidence actually available in the worktree,
terminal history, and local artifact directory. It does not claim a complete
release directory, a GitHub workflow result, a GitHub attestation, a GitHub
Release, crates.io publication, PyPI publication, or physical-hardware
qualification.

## Actual local source evidence

The following source gate ran from the Task 10 worktree and exited `0`:

```sh
./scripts/check-generated-protocol.sh
cargo fmt --all --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace --all-features
uv run --project bindings/python --python 3.11 --frozen pytest -q
```

The available terminal output records `449 passed in 81.18s` for the final
Python command. It does not preserve a separate full transcript or aggregate
test count for the Rust commands; their exit status is `0` in the same command
record. Terminal evidence: Cursor terminal `46004`, command
`运行发布与工作区验证套件`; the terminal record itself is not a repository
artifact.

The final release regression record executed:

```sh
uv run --project bindings/python --python 3.11 --frozen pytest -q tests/release
python3 scripts/release/release_tool.py check-version \
  --tag v0.5.0-rc.1 --repo-root .
python3 -m compileall -q scripts/release/release_tool.py tests/release
git diff --check
```

It exited `0`; the preserved output reports `203 passed in 68.20s`. Terminal
evidence: Cursor terminal `379862`, completed at `2026-08-18T15:21:08Z`. The
post-documentation record reran the release test suite and reported `203
passed in 67.49s`; it also exited `0` after `check-version` and `git diff
--check`. That terminal output is not a repository artifact.

## Actual local release candidates

The production macOS candidate was built and packaged with:

```sh
cargo build --release -p seeed-hal-broker-app --no-default-features \
  --features serialport,nusb,avfoundation
target/release/seeed-hal-broker --manifest > target/release/broker-manifest.json
scripts/release/package-broker.sh v0.5.0-rc.1 macos \
  target/release/seeed-hal-broker target/release/broker-manifest.json \
  target/release-artifacts/macos-production
```

This command path succeeded. The actual archive remains at
`target/release-artifacts/macos-production/seeed-hal-broker-v0.5.0-rc.1-aarch64-apple-darwin.tar.gz`;
the recorded SHA-256 is:

```text
141c60942cfe00414494a85ed0e48f9dea3b1fecb54dc70db94b17731e08c32c
```

The separately built virtual-adapter broker produced actual local
hardware-free evidence for protocol minors 0–3:

```sh
cargo build --release -p seeed-hal-broker-app --no-default-features \
  --features virtual-adapters
python3 scripts/release/release_tool.py run-virtual-conformance \
  --platform macos --broker target/release/seeed-hal-broker --repo-root . \
  --command-identity "local macOS virtual conformance" \
  --ref "https://github.com/seeed-studio/seeed-hal/commit/8db88edf867abc1dcbef53331bed24c072e74f76"
```

The persisted result is
`target/release-artifacts/macos-virtual-conformance.json`. It contains four
macOS records, one for each minor `0`, `1`, `2`, and `3`, all with result
`Passed`. This is virtual fixture evidence only and does not qualify a physical
CAN, serial, USB, GPIO, or camera device.

The private Python candidate was built and validated with:

```sh
scripts/release/package-python.sh v0.5.0-rc.1 \
  target/release-artifacts/python-candidate
```

The actual candidate files and recorded SHA-256 values are:

```text
5ede5f9e42189f764648ada35a1745fe42196e98a4f3637a4cbd73adc9a23d74  seeed_hal-0.5.0rc1-py3-none-any.whl
979f6d1d9c003de36883f4f57daa26b9c66b803403a8c88e48fbbed059a15849  seeed_hal-0.5.0rc1.tar.gz
```

The package command validates candidate metadata and an isolated offline wheel
installation. It is a local candidate, not a PyPI upload.

## Actual Rust bundle block

The real Rust package operation was attempted:

```sh
scripts/release/package-rust.sh v0.5.0-rc.1 \
  target/release-artifacts/rust-candidate
```

It exited nonzero with the structured diagnostic:

```text
release.cargo.failed: cargo package failed
```

The focused reproduction identified the first blocking package as
`seeed-hal-adapter-avfoundation`. Cargo reported:

```text
error: failed to prepare local package for uploading

Caused by:
  no matching package named `seeed-hal-camera` found
  location searched: crates.io index
  required by package `seeed-hal-adapter-avfoundation v0.5.0-rc.1`
```

No Rust source bundle was produced. The Rust dependency-publication closure is
out of scope for Task 10 and was not changed.

## What was not run or qualified

- The complete aggregate, generated release manifest, checksum sidecar,
  `verify-static`, and `verify-artifacts` did not run against a complete actual
  release directory: the Rust bundle is absent and only macOS broker output is
  local. Fixture tests cover their contracts but are not release evidence.
- GitHub-hosted macOS/Linux/Windows jobs remain pending. The `release-rc`
  workflow was not dispatched, and `act` was not used because it cannot
  reproduce the required native runners or GitHub artifact/attestation
  semantics.
- No tag, push, GitHub Release, attestation, crates.io publication, or PyPI
  publication occurred.
- All physical hardware qualification remains pending. The qualification
  record now also links the pending [SocketCAN vcan](../../docs/runbooks/socketcan-vcan.md)
  and [PCAN loopback](../../docs/runbooks/pcan-loopback.md) runbooks.

## Review resolution

The factual qualification review of `8db88ed..a3dff80` found two Critical
documentation-audit gaps:

1. the qualification document stated local outcomes without a repository audit
   report that located their available evidence; and
2. the physical-hardware table omitted the existing CAN qualification
   runbooks.

There was no implementation behavior to test, so TDD was not applicable.
This report is the GREEN documentation resolution for the first finding; the
qualification table now includes pending SocketCAN vcan and PCAN loopback rows
for the second. Link checks and `git diff --check` are recorded with this
remediation commit. The Task 9 release workflow semantics were not modified.
