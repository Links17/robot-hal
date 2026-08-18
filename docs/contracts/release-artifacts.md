# RC release artifact contract

## Scope and identity

This contract applies only to Seeed HAL `v0.5.0-rc.N` prereleases, where `N`
is a non-zero decimal integer without leading zeroes. A release workflow
checks out the annotated or lightweight tag's resolved commit and rejects a
dirty checkout, a mismatched ref, a duplicate tag on the resolved commit, or
an existing GitHub Release for that tag. It never moves a tag, overwrites a
release, or retries by replacing assets.

The workflow is triggered only by matching tag pushes or a manual dispatch
that supplies an existing matching tag. Pull requests and branches cannot
publish releases. Dispatch `dry_run=true` stops after aggregate verification;
it does not request write permissions, attest, or create a release.

## Final release directory

The aggregate job creates one new private directory. Its contents must be
exactly the following six primary artifacts and three sidecars:

```text
seeed-hal-broker-v0.5.0-rc.N-aarch64-apple-darwin.tar.gz
seeed-hal-broker-v0.5.0-rc.N-x86_64-unknown-linux-gnu.tar.gz
seeed-hal-broker-v0.5.0-rc.N-x86_64-pc-windows-msvc.zip
seeed-hal-crates-v0.5.0-rc.N.tar.gz
seeed_hal-0.5.0rcN-py3-none-any.whl
seeed_hal-0.5.0rcN.tar.gz
release-manifest.json
SHA256SUMS
conformance-report.json
```

`release-manifest.json` is canonical JSON and records the exact artifact
names, sizes, SHA-256 digests, release tag/commit, broker composition, and a
canonical-byte binding for `conformance-report.json`. `SHA256SUMS` contains
only the six primary artifact digests in lexical artifact-name order. The
manifest binding is the second integrity check for conformance evidence; the
static verifier rejects a changed report even if it is semantically valid.

Candidate directories, platform virtual brokers, and virtual-conformance
outputs are intermediate evidence only. They are never Release assets.

Python candidate validation installs the candidate and the exact locked
pure-Python `protobuf` wheel into a fresh offline environment. The candidate
wheel is rejected when a safe, normalized wheel member—or the installed target
of its `.data/purelib/` or `.data/platlib/` member—would occupy top-level
`google`. The installed `protobuf` distribution must have the exact normalized
`RECORD` mapping from that wheel: non-`RECORD` rows require SHA-256 and size,
and additional, missing, duplicate, or hashless entries fail validation.

Private staging and candidate identity checks detect replacement before a
candidate is accepted, but do not claim a no-replace boundary against a
malicious process with the same operating-system UID. Python cannot provide
that boundary to pathname-consuming installers without an OS-level sandbox or
file-descriptor-aware installer interface. The current RC package-python
candidate build is supported on Unix hosts; Windows candidate qualification
requires hosted evidence before it may be claimed.

## Rust workspace source bundle

`seeed-hal-crates-v0.5.0-rc.N.tar.gz` is a deterministic, complete Rust
workspace source bundle. It is not a collection of independently installable
`.crate` archives and it makes no crates.io availability claim.

The archive has one top-level `seeed-hal-crates-v0.5.0-rc.N/` directory and
contains the tracked, regular repository files needed to retain the workspace
source closure, including the root `Cargo.toml`, `Cargo.lock`, and every
workspace member manifest and source file. Package construction freezes that
controlled file set; it rejects symlinks, unsafe or unexpected paths, missing
workspace members, a dirty checkout, changed frozen inputs, and failed or
timed-out validation.

The packager extracts the archive into a restricted temporary directory and
runs `cargo check --workspace --locked`. This validates path-and-version
internal dependencies as one workspace without contacting or publishing to a
registry. Public crate registry policy is outside this RC artifact contract.

## Evidence and release gate

Platform inputs are named with the immutable release tag and resolved
40-character commit. Every consumer obtains artifacts only from the current
workflow run through dependency edges, verifies its uploaded `SHA256SUMS`,
and rejects missing, additional, or digest-mismatched files.

Each hosted platform verifies its matching production broker archive and
executes a separately built virtual broker. The aggregate step accepts only
one evidence report for each of macOS, Linux, and Windows, with every platform
job and all wire minors `0..3` marked `Passed`. `Pending`, `Partial`,
`Blocked`, or `Failed` software evidence is a hard failure. The aggregate job
then runs `aggregate-release`, `verify-static`, and `verify-artifacts`; only a
complete static-valid release directory with `release_ready` may reach the
final job.

Hardware qualification remains separate from software release conformance.
Its factual status and externally accessible evidence URI are recorded in the
report; the RC prerelease does not imply that physical hardware has been
qualified.

## Attestation and publication

Only the final job has `contents: write`, `id-token: write`, and
`attestations: write`. All other jobs have read-only `contents`. The final job
re-validates the complete directory, generates GitHub build-provenance
attestations with the official `actions/attest` action, then creates exactly
one GitHub prerelease. It passes `--prerelease --latest=false` and attaches
the six primary artifacts plus all three sidecars.

Consumers can verify any downloaded asset with:

```sh
gh attestation verify PATH/TO/ARTIFACT -R OWNER/REPOSITORY
```

No registry publication, package upload, secret, registry token, or release
asset replacement is part of this contract. A failed or interrupted release is
investigated as a new immutable attempt; it is never repaired by overwriting
or deleting historical release state.

## Stable failure categories

Release tooling emits stable names including `release.version.invalid`,
`release.version.mismatch`, `release.artifact.unexpected`,
`release.manifest.invalid`, `release.conformance.incomplete`,
`release.conformance.invalid`, and `release.package.invalid`. Automation and
consumers must use these names and categories rather than parsing diagnostic
text.
