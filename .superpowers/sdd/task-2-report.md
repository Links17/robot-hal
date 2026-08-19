# Task 2 Report: Hosted Windows Shared-Memory Runtime Gate

## Status

Added CI evidence coverage only. No production shared-memory implementation changed.

`platform-conformance` now runs the Windows-only shared-memory runtime suite before either the
production or virtual Broker build:

```text
cargo +1.85 test -p seeed-hal-adapter-shared-memory --all-features platform::windows_tests -- --nocapture
```

The step is conditioned on `runner.os == 'Windows'`, so Linux and macOS matrix jobs do not run
Windows-specific tests. It uses the workflow's default portable shell and does not add actions,
permissions, publishing, or credentials.

## TDD evidence

RED:

```text
uv run --project bindings/python --python 3.11 --frozen \
  pytest -q tests/release/test_workflow_contract.py

1 failed, 19 passed
StopIteration: missing "Run Windows shared-memory runtime tests"
```

GREEN:

```text
uv run --project bindings/python --python 3.11 --frozen \
  pytest -q tests/release/test_workflow_contract.py

20 passed
```

The contract test requires the Windows condition, the exact command, and ordering before both
Broker build/conformance steps.

## Remaining hosted evidence

This change makes the Task 2 runtime suite a required Hosted Windows CI gate. Actual evidence for
Windows DACL behavior, `CreateMutex`/`OpenMutex`, `WAIT_ABANDONED` ownership and release, close
terminal state, and final-handle retirement still requires the commit to be pushed and the
Windows matrix job to complete.
