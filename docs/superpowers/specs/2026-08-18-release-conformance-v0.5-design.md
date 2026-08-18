# Release conformance v0.5 design

## Scope

v0.5 makes Seeed HAL software releases repeatable and independently verifiable across macOS,
Linux, and Windows. It adds no hardware class and does not freeze the v1.0 API. Its deliverable is
a layered GitHub Actions pipeline that runs source and platform conformance, builds release
candidate artifacts for the broker, Rust crates, and Python client, verifies those artifacts in
clean jobs, and creates an attested GitHub prerelease.

Physical hardware qualification remains separate evidence. Pending or blocked Serial, CAN, USB,
GPIO, or Camera hardware results do not silently become passed because a virtual adapter,
cross-target compiler, or hosted runner succeeded. An RC may be created with explicit pending
hardware status; the criteria for promoting an RC to final `v0.5.0` are a later release decision.

Node/Electron bindings, public crates.io or PyPI publication, a new hardware interface, device
protocols, and v1.0 interface freezing are outside v0.5.

## Version and release model

Every included package uses one release-candidate version:

```text
0.5.0-rc.N
```

The Git tag is `v0.5.0-rc.N`. The broker package, all publishable Rust crates, Python project
metadata, broker manifest, release manifest, and artifact names must encode the same version.
Python normalizes it to the PEP 440 form `0.5.0rcN` in wheel and sdist names. A mismatch fails
before artifact upload.

The RC workflow is manually dispatched or triggered by a matching RC tag. It rejects malformed
versions, an existing tag or release, and any request to replace or overwrite an artifact. It has
no crates.io or PyPI credentials and does not execute a public-registry publish command.

## Layered pipeline

### Source gate

The source gate runs the repository's canonical hardware-free checks:

- Rust 1.85 and edition 2024;
- generated protobuf consistency;
- `cargo fmt --all --check`;
- workspace clippy with warnings denied;
- full workspace tests with all features;
- Python 3.11 frozen tests;
- wire/tag-lock and release-script contract tests.

Failure stops the release before platform artifact construction.

### Platform conformance

GitHub-hosted macOS, Linux, and Windows runners build the production broker composition for their
platform and a separate test-only `virtual-adapters` broker. The production composition is:

| Platform | Required default adapters |
| --- | --- |
| macOS | `serialport`, `nusb`, `avfoundation` |
| Linux | `serialport`, `nusb`, `socketcan`, `linux-gpio`, `v4l2` |
| Windows | `serialport`, `nusb`, `windows-gpio`, `mediafoundation` |

PCAN remains an optional vendor-runtime variant and is not included in the default RC broker.

Each platform validates the production broker manifest: target triple, OS, architecture, MSRV,
wire major 1 with inclusive minors `0..=3`, enabled feature set, required adapter set, and required
vendor runtime declarations. Listing an adapter does not claim a physical device, privacy grant,
driver, or vendor runtime was available.

The virtual broker black-box suite covers Serial, CAN, USB, GPIO, and Camera through wire minor 3,
including owner cleanup, resource reuse, exclusive claims, stale generations, and bounded process
shutdown. The runner becomes parameterizable by negotiated minor and required capabilities. Its
compatibility matrix proves:

- minors 0 through 3 retain their previously defined capabilities;
- clients negotiated below a capability's introduction reject its entry points locally or at
  broker dispatch;
- Camera frame bytes never enter protobuf.

Every process start, request, cleanup, retry, and job has a finite deadline. A hang is a failure,
not a reason to wait indefinitely.

### Artifact build

After all platform conformance jobs pass, the pipeline builds:

- `seeed-hal-broker-v0.5.0-rc.N-<target>.tar.gz` on macOS/Linux;
- `seeed-hal-broker-v0.5.0-rc.N-<target>.zip` on Windows;
- `seeed-hal-rust-crates-v0.5.0-rc.N.tar.gz`;
- `seeed_hal-0.5.0rcN-py3-none-any.whl`;
- `seeed_hal-0.5.0rcN.tar.gz`;
- `SHA256SUMS`;
- `release-manifest.json`;
- `conformance-report.json`.

`release/targets.toml` is the single source of truth for target triples, runner OS, broker features,
required adapters, and archive format. Build scripts consume this file rather than duplicating
platform lists.

Rust packaging runs `cargo package` for every publishable workspace crate, checks package file
lists, and verifies the packaged dependency closure. It does not upload crates. Python packaging
uses the pinned build backend and emits one pure-Python wheel plus sdist.

### Clean artifact verification

Verification jobs download artifacts into a fresh workspace and do not trust the build job's
checkout or target directory. They:

1. reject absolute paths, traversal paths, duplicate names, unexpected files, and symlinks in
   archives;
2. verify every size and SHA-256 against both `SHA256SUMS` and `release-manifest.json`;
3. run each broker's `--manifest` and compare version, target, wire range, MSRV, features, adapters,
   and executable checksum with the release manifest;
4. start a broker on a private endpoint and run the virtual black-box conformance suite;
5. install the wheel in clean Python 3.11, 3.12, and 3.13 environments and verify import/version;
6. unpack each `.crate`, build it from packaged contents, and verify its declared dependencies.

No artifact reaches a GitHub Release unless every default platform and client artifact verifies.
Partial-platform prereleases are prohibited.

## Manifests and evidence

The broker's existing `--manifest` remains the identity of one executable. The release-level
`release-manifest.json` indexes the complete RC and contains:

- schema version;
- release version, Git tag, and commit;
- wire major/minor range, MSRV, and Python minimum;
- each artifact's name, kind, target, byte size, and SHA-256;
- expected broker features, adapters, and vendor runtimes;
- references to software conformance and hardware qualification records.

`conformance-report.json` records each software job, command identity, platform, bounded result,
and retained GitHub run reference. Hardware qualification is represented only as `Passed`,
`Partial`, `Pending`, `Blocked`, or `Failed` with a link to its external evidence record. The
pipeline never rewrites `Pending` or `Blocked` to `Passed`.

Manifest serialization is deterministic: fields and artifact arrays use a documented stable order,
timestamps are omitted from checksum-bearing content, and repeated generation from the same inputs
produces identical bytes.

Logs, reports, and manifests exclude startup tokens, Camera mapping names or capability tokens,
device serial numbers, transient native endpoints, and payload bytes.

## Integrity and permissions

The release job generates `SHA256SUMS` and GitHub Artifact Attestations for every published file.
Attestations use GitHub's OIDC identity and bind each artifact digest to the repository, workflow,
commit, and release run. The release notes link to verification instructions.

Workflow permissions are minimal:

- pull-request and source-conformance jobs use read-only repository contents;
- build and verification jobs cannot write releases;
- only the final release job receives `contents: write` and `id-token: write`;
- no job receives package-registry publication permission.

If checksum, manifest, signature, version, target, feature, adapter, or conformance evidence differs,
the workflow fails closed and creates no prerelease.

## Repository structure

v0.5 adds these responsibility-focused files:

- `.github/workflows/ci.yml`: source gates and three-platform software conformance;
- `.github/workflows/release-rc.yml`: validated RC orchestration, attestation, and prerelease;
- `release/targets.toml`: platform and adapter matrix;
- `scripts/release/check-version.*`: unified package/tag version validation;
- `scripts/release/package-broker.*`: target broker archive creation;
- `scripts/release/package-rust.*`: deterministic `.crate` collection;
- `scripts/release/package-python.*`: wheel and sdist construction;
- `scripts/release/generate-manifest.*`: release manifest and checksums;
- `scripts/release/verify-artifacts.*`: offline clean artifact verification;
- `tests/release/`: release-script, fixture, archive-security, manifest, and workflow-permission tests;
- `docs/contracts/release-artifacts.md`: normative artifact contract;
- `docs/releases/v0.5.0-rc-qualification.md`: RC software and external qualification evidence.

Scripts remain platform-neutral where practical. Platform-specific shell or PowerShell wrappers may
only adapt invocation and filesystem conventions; validation rules live in one shared implementation.

## Error and retry semantics

Release tooling emits machine-readable failure categories in addition to human diagnostics:

- `release.version.invalid`;
- `release.version.mismatch`;
- `release.target.unsupported`;
- `release.artifact.unexpected`;
- `release.archive.invalid`;
- `release.checksum.mismatch`;
- `release.manifest.invalid`;
- `release.conformance.failed`;
- `release.attestation.failed`;
- `release.already_exists`.

No failure is repaired by silently dropping an adapter, lowering a protocol minor, omitting a
package, renaming an unexpected file, or publishing only the platforms that passed. Retry creates a
new workflow run for the same immutable commit before release creation, or a new `rc.N` after an RC
exists.

## Test strategy

Implementation follows TDD:

1. version tests reject malformed RCs and every package/tag mismatch;
2. matrix tests enforce one target entry per platform and exact required adapter composition;
3. archive fixture tests reject traversal, absolute paths, symlinks, duplicate names, and checksum
   tampering;
4. deterministic manifest tests compare byte-identical repeated output and reject sensitive fields;
5. broker manifest tests lock version, target, wire range, features, adapters, and checksum;
6. black-box tests parameterize negotiated minors and capability fail-closed behavior;
7. wheel tests install/import under Python 3.11 through 3.13;
8. packaged Rust crate tests build from unpacked package contents;
9. workflow contract tests assert trigger constraints and least privilege, and reject crates.io or
   PyPI secrets/publish commands;
10. end-to-end dry-run builds and verifies the full artifact set without creating a GitHub Release.

GitHub-hosted runner results are the evidence for their respective operating systems. A local macOS
run or cross-target compiler is not recorded as Linux/Windows hosted conformance.

## Acceptance criteria

v0.5 is complete when:

- all included package versions and an RC tag are exactly consistent;
- source gates and all three hosted platform conformance jobs pass;
- default production broker manifests match `release/targets.toml`;
- broker, Rust crate, wheel, and sdist artifacts build deterministically and verify from a clean job;
- all release artifacts have matching SHA-256 entries and GitHub Artifact Attestations;
- the prerelease workflow has least privilege and cannot publish to public package registries;
- compatibility minors 0 through 3 and capability fail-closed behavior have executable evidence;
- release records distinguish software conformance from physical hardware qualification;
- no credential, mapping secret, device serial, endpoint, or payload enters logs or manifests.
