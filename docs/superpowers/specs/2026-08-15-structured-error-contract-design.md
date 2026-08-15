# Structured Error Contract Design

**Status:** Approved for implementation planning

**Date:** 2026-08-15

**Scope:** Complete the v0.1 structured-error contract across in-process Rust, broker wire v1, the Rust broker client, and the Python broker client.

## 1. Objective

Make every Seeed HAL deployment form preserve one structured error model without introducing gRPC or a second error framework. The change must remain additive within wire major version 1 and must not alter the stable error decisions already used by callers.

This design is informed by the Google RPC richer error model:

- `ErrorInfo.metadata` motivates a bounded string-to-string diagnostic context;
- `ResourceInfo.resource_name` motivates carrying the canonical `ResourceId`;
- developer-facing diagnostics remain separate from stable machine decisions.

The project does not adopt `google.rpc.Status`, `tonic-types`, `error-stack`, or RFC 9457 as a dependency. Those models target gRPC or HTTP and would add transport concepts the local HAL does not use.

## 2. Existing contract and gap

The architecture defines an error with:

- stable name;
- category;
- operation;
- retryability;
- optional resource identity;
- optional platform code;
- optional vendor code;
- developer-facing debug message;
- structured context.

The current Rust, protobuf, Rust-client, and Python implementations preserve only the first four fields and the debug message. Broker deployment therefore loses details available to an in-process caller.

## 3. Chosen approach

Keep `HalError` as the single public Rust error and extend it with validated diagnostic details. Keep the existing protobuf `Error` message and append new fields. Teach both language clients to validate and retain those fields.

No new runtime dependency is required. `BTreeMap` provides deterministic context ordering in Rust. Protobuf map support and Python's standard mapping types cover the language interfaces.

### Rejected alternatives

#### Replace the wire error with `google.rpc.Status`

Rejected because changing existing protobuf field meanings would break wire v1. `Any` details would also enlarge the interface and require a type registry without providing leverage for the current local IPC transport.

#### Add `tonic-types` or gRPC

Rejected because the broker uses Unix Domain Sockets and Windows Named Pipes rather than HTTP/2. The dependency would not own transport, session fencing, or broker cleanup behavior.

#### Adopt `error-stack` as the public error

Rejected because `error-stack` is an in-process diagnostic report, not a stable cross-language wire contract. It may remain an internal option in a future implementation, but it must never become the public HAL decision surface.

## 4. Rust core interface

`HalError::new(name, category, operation, retryable, debug_message)` remains available with its current behavior so existing callers do not need migration.

`HalError` gains these diagnostic fields:

```rust
resource_id: Option<ResourceId>
platform_code: Option<String>
vendor_code: Option<String>
context: ErrorContext
```

It gains read-only accessors for all four fields and consuming enrichment methods:

```rust
pub fn with_resource_id(self, resource_id: ResourceId) -> Self;
pub fn with_platform_code(self, code: impl Into<String>) -> HalResult<Self>;
pub fn with_vendor_code(self, code: impl Into<String>) -> HalResult<Self>;
pub fn with_context(self, context: ErrorContext) -> Self;
```

The fallible code methods reuse the existing identifier invariant: non-empty ASCII with a maximum length of 255 bytes. An absent code is represented only by `None`, never by an empty string.

`ErrorContext` is a validated module backed by `BTreeMap<String, String>`:

```rust
pub struct ErrorContext(BTreeMap<String, String>);

impl ErrorContext {
    pub fn new(entries: impl IntoIterator<Item = (String, String)>) -> HalResult<Self>;
    pub fn iter(&self) -> impl Iterator<Item = (&str, &str)>;
    pub fn is_empty(&self) -> bool;
}
```

Its interface follows the useful subset of Google `ErrorInfo.metadata`:

- at most 16 entries;
- keys are 1–64 ASCII bytes and match `[a-z][a-zA-Z0-9_-]*`;
- values are valid UTF-8 and at most 1,024 bytes each;
- the sum of key and value byte lengths is at most 8,192 bytes;
- duplicate input keys fail instead of overwriting an earlier value;
- empty values are allowed because presence can itself be diagnostic.

These bounds keep the diagnostic module well below the hard 1 MiB frame cap while leaving room for the envelope and stable fields.

`ErrorDecisionFields` remains exactly:

- name;
- category;
- operation;
- retryable.

`HalError` serde remains decision-only. It must not serialize the debug message or diagnostic details. Broker protobuf conversion is the explicit interface for transporting diagnostic details.

## 5. Population rules

Modules enrich an error only with facts they already possess:

- attach `resource_id` whenever the failing operation has resolved a canonical `ResourceId`;
- attach `platform_code` when an OS or platform library provides a raw, symbolic, numeric, or hexadecimal code;
- attach `vendor_code` only when a future vendor adapter provides a documented vendor code;
- use context for bounded diagnostic facts whose keys are defined at the error creation site;
- never derive stable decisions from context;
- never parse a debug message to construct details;
- never put startup tokens, payload bytes, device serial numbers, user paths, or other secrets into context.

The v0.1 native Serial adapter must preserve `raw_os_error()` as a decimal `platform_code` when present. Runtime lease and session failures must attach the canonical resource identity whenever that identity is available at the failure site. No fabricated vendor code is added in v0.1.

## 6. Wire v1 evolution

The existing protobuf `Error` field numbers 1–5 remain unchanged. Add:

```protobuf
string resource_id = 6;
string platform_code = 7;
string vendor_code = 8;
map<string, string> context = 9;
```

Wire rules:

- empty optional strings decode as `None` for legacy compatibility;
- a missing or empty context decodes as `ErrorContext::default()`;
- non-empty details are validated with the core invariants;
- an invalid detail makes the containing error payload an invalid protocol message and fails the connection closed;
- old peers safely ignore fields 6–9;
- new peers safely accept old errors with fields 6–9 absent;
- no existing field number is reused and no existing field changes meaning.

`seeed-hal-protocol` owns both Rust conversion directions. The Rust client must call the protocol conversion rather than maintain a second mapping for `v1::Error`.

The generated Python protobuf file remains generated by the existing script and is never hand-edited.

## 7. Python interface

Python `HalError` retains its existing positional fields and adds optional keyword-compatible fields with defaults:

```python
resource_id: str | None = None
platform_code: str | None = None
vendor_code: str | None = None
context: Mapping[str, str] = empty immutable mapping
```

Construction makes a defensive immutable copy of context so callers cannot mutate an error after delivery. Each terminal fan-out still receives a fresh `HalError`, including a fresh immutable context view.

The Python client validates the same entry count, key syntax, per-value limit, aggregate limit, identifier rules, and legacy empty-field behavior as Rust. Invalid broker details terminate the connection as `runtime.protocol.invalid_message`.

## 8. Broker and client behavior

Broker conversion includes diagnostic details in every encoded `Error`. Existing response-queue and frame-limit rules still apply. If a complete error envelope exceeds the negotiated frame limit, the existing bounded connection failure behavior remains authoritative; the implementation must not silently truncate details.

Rust and Python clients retain every valid field. Application decisions continue to use only name, category, operation, and retryability. Debug messages and details are for diagnostics.

Unsolicited error events (`request_id == 0`) use the same conversion and validation as request errors.

## 9. Observability and security

Library code still does not install tracing subscribers. The broker executable continues logging only:

- outcome kind;
- error name;
- category;
- operation;
- retryability.

It must not log debug messages, resource identity, platform/vendor codes, context, startup tokens, or raw payloads by default.

## 10. Testing strategy

### Core contract

- legacy `HalError::new` yields empty details;
- enrichment accessors preserve validated details;
- invalid or duplicate context entries fail;
- entry, key, value, and aggregate limits are exact and deterministic;
- decision-only serde output remains unchanged;
- malformed diagnostic codes fail without panicking.

### Protocol contract

- rich Rust error round-trips through protobuf without field loss;
- legacy protobuf error decodes to empty details;
- invalid resource ID, code, key, value, entry count, and aggregate size fail structurally;
- generated payload tag locks remain unchanged;
- generated Python protobuf output is reproducible.

### Runtime and adapter contract

- a stale lease or another resource-scoped runtime failure includes the canonical resource ID;
- a native I/O error with `raw_os_error()` includes its decimal platform code;
- no v0.1 error fabricates a vendor code.

### Rust client contract

- a real broker request returns all rich details through `HalClient`;
- legacy server errors remain accepted;
- malformed rich errors fail the connection closed;
- unsolicited errors follow the same validation.

### Python client contract

- rich broker errors expose immutable details;
- terminal fan-out produces fresh immutable errors;
- legacy errors remain accepted;
- malformed details terminate the connection;
- exact bounds match the Rust contract.

### Full verification

Run:

```bash
cargo fmt --all --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace --all-features
uv run --project bindings/python --python 3.11 --frozen pytest -q
./scripts/check-generated-protocol.sh
```

The physical Serial loopback remains ignored by default.

## 11. Documentation and compatibility

Update the architecture document so its `HalError` example exactly matches the implemented interface. Update the versioning contract to record fields 6–9 as an additive wire-v1 evolution and state the legacy empty-field behavior. Update v0.1 acceptance evidence only with commands actually rerun during this implementation.

No responsibility seam changes. No CAN, USB, GPIO, Camera, event-model, listener, or unrelated wire-policy refactor belongs in this change.
