# Task 3 report: canonical resource identity on resolver and lease errors

## Status

Implemented resolver and active-lease error enrichment.

## Implementation

- Resolver `runtime.resource.not_found` and `runtime.resource.ambiguous` errors now carry `selector.id().clone()`.
- `LeaseTable::reserve_control` conflict and generation-exhaustion errors now carry the supplied resource ID.
- Every `LeaseTable::validate` error path now carries the supplied resource ID.
- Added contract assertions for resolver not-found/ambiguous errors, stale-generation writes, and active lease conflicts.
- Closed-session replay handling was left unchanged because retained replay state has no canonical resource ID.

## Tests

NOT RUN / deferred as required. No test, lint, build, cargo check, rustfmt, or formatting-check command was run.

## Static self-check

- Reviewed every changed error path and corresponding assertions.
- Ran `git diff --check` only.
- `.codegraph/` and `.cursor/` were pre-existing untracked paths and were not touched or staged.

## Concerns

- Full verification is intentionally deferred to the unified verification task.

## Commit

`988423c fix(runtime): attach canonical resource error identity`
