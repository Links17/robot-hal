# AGENTS.md

Repository-level instructions for Robot HAL. Keep changes small, factual, testable, and reusable outside any one product.

## Priority

1. Follow the latest user request.
2. Treat current code, tests, and documented contracts as the source of truth.
3. Preserve the responsibility seam in `docs/contracts/hal-responsibility.md`.
4. Prefer an existing module and interface before adding a new abstraction.

## Non-negotiable responsibility seam

Robot HAL abstracts hardware access, transport behavior, and resource lifecycle. It must not contain product or device-protocol business logic.

Allowed concepts include:

- resource identity and endpoint resolution;
- discovery and hotplug;
- sessions, leases, cancellation, timeouts, queues, and backpressure;
- Serial, CAN/CAN FD, USB, GPIO, and Camera hardware-class interfaces;
- platform adapters and vendor transport adapters;
- transport-level health, errors, diagnostics, tracing, and metrics;
- local broker and language clients.

Forbidden concepts include:

- robot, leader, follower, joint, episode, calibration, teleoperation, training, inference, or dataset;
- Feetech, Damiao, RobStride, B601, or another device protocol in the core HAL;
- motor control modes or product safety sequences;
- application roles, workflow states, UI messages, or product business codes.

If a proposed change requires a forbidden concept, place it in the consuming application or a separate device-driver asset.

## Architecture rules

- The project is **library-first**. `robot-hal-broker` is an adapter that exposes the same library semantics over local IPC; it is not a second HAL implementation.
- Core types and behavior must be platform-neutral.
- Platform-specific code lives in an adapter crate.
- One adapter is hypothetical; require either two real adapters or one real adapter plus the conformance adapter before stabilizing a seam.
- Persist physical resource identity, never a transient endpoint as identity truth.
- Broker sessions own OS handles. Clients never receive raw native handles.
- Resource access fails closed on ambiguous identity, stale lease generation, unsupported capability, or incompatible protocol version.
- Camera control uses broker IPC, but future frame transport must use shared memory or an equivalent bounded data plane rather than protobuf request/response payloads.

## Interface and compatibility

- Public interfaces include types, invariants, ordering, errors, configuration, and performance behavior.
- Additive protobuf changes only within a protocol major version. Never reuse field numbers.
- Unknown protobuf fields and capabilities must be tolerated when safe.
- Stable error decisions depend on structured error names and categories, never message parsing.
- Capability identifiers are transport/hardware-class concepts such as `can.fd/v1` or `camera.frames/v1`; business capabilities are prohibited.
- Every lease carries a monotonically increasing generation used as a fencing token.

## Rust engineering

- Workspace edition: Rust 2024.
- MSRV: Rust 1.85 until changed by an approved contract update.
- Production async runtime: Tokio.
- `unsafe` is forbidden in core, runtime, protocol, broker, and client crates.
- Adapter-local `unsafe` requires a preceding `// SAFETY:` invariant and a focused test or upstream conformance citation.
- No blocking hardware call may run on a Tokio executor worker. Use native async I/O or an explicitly owned blocking worker.
- Bound every queue. Document overflow behavior at the interface.
- Avoid global mutable state and implicit singleton runtimes.
- Library crates must not initialize tracing subscribers, Tokio runtimes, or process-wide signal handlers.

## Testing

- Complete the focused implementation and its test coverage before running tests. Do not require
  per-case red-green cycles; run the relevant tests and canonical verification gates together after
  the change is fully written.
- The interface is the primary test surface.
- Every adapter must pass the shared conformance suite.
- Tests requiring physical hardware must be marked and excluded from the default test command.
- The default verification command is:

```bash
cargo fmt --all --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace --all-features
```

- Python binding verification additionally runs:

```bash
uv run pytest bindings/python/tests
```

## Documentation

- Architecture changes update `docs/architecture/` in the same change.
- Responsibility or compatibility changes update `docs/contracts/` in the same change.
- Implementation plans live in `docs/superpowers/plans/`.
- Do not describe a planned capability as implemented.

## Git

- Use Conventional Commits: `<type>(<scope>): <subject>`.
- Keep commits independently reviewable.
- Do not mix adapter cleanup, protocol changes, and unrelated documentation work.
- SemVer applies to crates, broker releases, and the IPC contract as described in `docs/contracts/versioning.md`.
