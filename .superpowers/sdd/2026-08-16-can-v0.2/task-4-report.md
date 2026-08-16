# Task 4 report: shared CAN actor and runtime manager

## Status

Implemented the Task 4 runtime layer and focused tests. Per the task brief, build, test, lint, formatting, and protocol verification were deliberately deferred.

## Files

- Created `crates/seeed-hal-runtime/src/can_actor.rs`.
- Created `crates/seeed-hal-runtime/src/can_manager.rs`.
- Created `crates/seeed-hal-runtime/tests/can_runtime.rs`.
- Modified `crates/seeed-hal-runtime/src/lib.rs`.
- Modified `crates/seeed-hal-runtime/src/events.rs`.
- Modified `crates/seeed-hal-runtime/Cargo.toml`.

## Behavior implemented

- Runtime builder accumulates CAN adapters and exposes clamped RX/TX capacities plus a finite CAN cleanup deadline. Defaults are RX 256, TX 64, and two seconds; documented runtime hard limits are 4096 frames.
- CAN discovery queries every adapter on named non-Tokio standard threads, combines descriptors deterministically by resource ID, adapter registration order, and endpoint, and resolves selectors with the canonical core resolver. Duplicate physical IDs remain ambiguous.
- One named standard-thread actor owns each resolved physical CAN channel. All adapter/channel open, receive, send, status, and close calls execute on named standard threads rather than Tokio workers.
- The actor uses a bounded 64-command management queue and separately accounts bounded TX frames with atomic whole-batch admission. Actor execution preserves FIFO and returns exact backend-committed prefixes.
- Session-local software filters, independent bounded drop-oldest RX rings, one-shot structured lag reports, one pending receive per session, finite receive polling, and cancellation pruning are implemented.
- Observe/Control sharing and exclusive Maintenance use the Task 3 provisional reserve/commit/cancel/release/validate lease table. Actor open/configuration and compatible-session admission happen before lease commit, and failed/cancelled provisional opens do not consume a generation.
- `HalRuntime` and `CanHandle` expose enumerate/open/send/send-batch/receive/filter/status/close APIs plus broker-facing `into_parts`. Retryable local close admission leaves the handle open; successful close is terminal.
- Owner revocation always runs Serial cleanup and CAN cleanup, preserving the first structured error while continuing remaining cleanup. Provisional CAN opens are cancelled and given a finite actor-removal wait.
- Lifecycle events remain owner-associated. Additive CAN bus-health event kinds contain only lifecycle identity/fencing fields; ordinary frames and diagnostic payloads never enter the event stream.
- Last-session close waits for actor completion before normal resource reuse. A close timeout releases the logical lease but keeps the still-running physical actor unavailable until its worker actually exits.

## Focused tests written (not run)

- Multi-observer independent fan-out.
- One Control lease coexisting with Observe and rejecting another Control.
- Maintenance exclusion, configured-channel close, restoration, and reopening.
- Session-local filter replacement.
- RX drop-oldest plus one-shot `can.receive.lagged` and retained order.
- Atomic TX batch admission with committed zero.
- Exact partial backend send count.
- FIFO batch execution.
- Finite receive timeout.
- Cancelled receive not consuming a later frame.
- Stale generation fencing after reuse.
- Owner revoke of all CAN sessions.
- Actor panic/disconnect propagation without hanging.
- Finite structured close timeout.
- Normal physical resource reuse.
- Multiple-adapter coexistence and duplicate identity ambiguity.
- Backend thread-affinity assertion proving calls use named CAN workers, not Tokio workers.

## Commands and output

- Read the exact task brief, CAN design sections, Tasks 1–3 implementation APIs, runtime registry/events/serial actor, virtual CAN adapter, and existing contract tests using `sed`/`rg`.
- Inspected branch/worktree state with `git status --short --branch`: branch `feat/v0.2-can`, initially clean.
- Inspected the complete changed sources and diff statically with `nl`, `sed`, `rg`, and `git diff`.
- Ran `git diff --check`: exit 0, no output.
- CodeGraph MCP tools were not exposed in this delegated agent session, so the review used direct reads of the specific files named by the brief rather than structural CodeGraph queries.

## Deferred commands

The following were explicitly not run:

```text
cargo fmt --all --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace --all-features
uv run pytest bindings/python/tests
```

No build, test, lint, format, protocol check, or dependency-generation command was run. `Cargo.lock` was not generated or modified.

## Static self-review

- Concurrency: manager mutexes are held only across synchronous state transitions; no backend call or async wait occurs while holding manager state. Actor management and TX capacity are independently bounded. TX reservation is released on command admission failure, completion, and queued shutdown rejection.
- Shutdown: receive polling is finite; normal last-session close waits for worker completion; close timeout does not falsely expose a still-running physical actor; runtime/channel drop cannot strand a blocking receive poll indefinitely under the channel timeout contract.
- Leak paths: provisional opens use an RAII guard, cancellation watch, cleanup completion watch, actor-session removal, and lease cancellation. Failed actor spawn/open/admission and commit races remove provisional state.
- Fencing: every session-ID operation validates the exact active token and required permission before actor admission. Closed replay caches retain exact tokens and canonical resource IDs; stale tokens against a newer active session are rejected.
- Fan-out/backpressure: each subscriber owns its ring/filter/error/pending receive state. A full subscriber drops only its oldest frame and never blocks another session.
- Routing: adapter indices are confined to transient discovery records and never stored as resource identity.
- Events: only normalized kind plus existing resource/session/owner/generation fields are emitted.
- Compile surfaces were reviewed manually, but compilation remains intentionally unverified until the deferred gate runs.

## Concerns

- The task's explicit deferred-verification constraint means syntax, type checking, formatting, Clippy, and runtime behavior are not yet evidenced by executed tooling.
- Synchronous `CanChannel::send` has no API deadline parameter. Exact physical-prefix reporting requires awaiting the backend result; therefore finite send completion depends on adapters honoring their nonblocking/bounded send contract. Receive, discovery, management operations, and cleanup have explicit runtime deadlines.
