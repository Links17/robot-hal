# Task 2 Report

Status: done.

RED:
`cargo test -p seeed-hal-core --test core_contract`
Output: `error: package ID specification 'seeed-hal-core' did not match any packages`
Why expected: the workspace initially had no `seeed-hal-core` member.

GREEN:
`cargo test -p seeed-hal-core --test core_contract`
`cargo test`
Output: 5 contract tests passed; workspace test run passed; doc-tests passed.

Files changed:
`Cargo.toml`, `Cargo.lock`, `crates/seeed-hal-core/Cargo.toml`,
`crates/seeed-hal-core/src/{lib.rs,capability.rs,error.rs,identity.rs,lease.rs}`,
`crates/seeed-hal-core/tests/core_contract.rs`

Self-review:
core identifiers are validated; `ResourceDescriptor::selector()` preserves id, transport, and minimum identity quality; `HalError` serializes only decision fields.

Concerns:
No broader consumer integration was exercised; the core API is intentionally strict on identifier syntax.
