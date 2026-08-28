# Structured Error Contract Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Preserve the complete v0.1 structured-error contract across in-process Rust, protobuf wire v1, the Rust broker client, and the Python broker client.

**Architecture:** Extend the existing `HalError` rather than adding an error framework. Add bounded Google-`ErrorInfo`-style diagnostic metadata to the core interface, append fields 6–9 to the existing protobuf `Error`, centralize Rust wire conversion in `robot-hal-protocol`, and validate the same contract in Python.

**Tech Stack:** Rust 2024/MSRV 1.85, `std::collections::BTreeMap`, serde, prost/protobuf, Tokio, Python 3.11, `MappingProxyType`, pytest.

**Spec:** `docs/superpowers/specs/2026-08-15-structured-error-contract-design.md`

## Global Constraints

- Do not add gRPC, `tonic-types`, `error-stack`, or another runtime dependency.
- Wire major version 1 changes are additive only; protobuf fields 1–5 retain their meanings and fields 6–9 are never reused.
- Stable decisions remain name, category, operation, and retryability; diagnostic details never become decision inputs.
- `ErrorContext` permits at most 16 entries, 64 ASCII key bytes, 1,024 UTF-8 value bytes, and 8,192 aggregate key/value bytes.
- Context keys match `[a-z][a-zA-Z0-9_-]*`; duplicate input keys fail.
- Do not put secrets, startup tokens, raw payloads, device serial numbers, or user paths in error context or default logs.
- Keep every queue bounded and preserve existing frame-limit behavior.
- Write focused contract tests alongside every behavior change, but do not run tests, lint, builds, or formatting checks during Tasks 1–6. Run all verification once, after implementation is complete, in Task 7.
- Do not change event, listener, CAN, USB, GPIO, Camera, or unrelated wire-policy modules.

## File Structure

- `crates/robot-hal-core/src/error.rs` owns validated diagnostic details and the complete Rust error interface.
- `crates/robot-hal-core/src/lib.rs` exports `ErrorContext`.
- `proto/seeed/hal/v1/hal.proto` owns the additive wire fields.
- `crates/robot-hal-protocol/src/conversion.rs` owns both Rust error conversion directions.
- `crates/robot-hal-protocol/src/lib.rs` exports `error_from_proto`.
- `crates/robot-hal-runtime/src/lease_table.rs` enriches active resource-scoped lease errors.
- `crates/robot-hal-core/src/identity.rs` enriches canonical resolver failures.
- `adapters/serialport/src/lib.rs` preserves native platform codes.
- `crates/robot-hal-client/src/connection.rs` delegates wire-error decoding to the protocol module.
- `bindings/python/robot_hal/errors.py` owns immutable Python diagnostic details.
- `bindings/python/robot_hal/client.py` validates wire details.
- Contract tests remain next to their owning interface; generated Python protobuf remains generated.

---

### Task 1: Add bounded diagnostic details to `HalError`

**Files:**
- Modify: `crates/robot-hal-core/src/error.rs`
- Modify: `crates/robot-hal-core/src/lib.rs`
- Modify: `crates/robot-hal-core/tests/core_contract.rs`

**Interfaces:**
- Consumes: existing `ResourceId`, `HalError::new`, decision-only serde contract.
- Produces: `ErrorContext`, four `HalError` enrichment methods, and four read-only detail accessors used by all later tasks.

- [ ] **Step 1: Write core interface tests without running them**

Add imports for `ErrorContext` and `BTreeMap`, then add focused tests equivalent to:

```rust
#[test]
fn structured_error_details_are_validated_and_preserved() {
    let context = ErrorContext::new([
        ("queueDepth".to_owned(), "64".to_owned()),
        ("limit_bytes".to_owned(), "1024".to_owned()),
    ])
    .unwrap();
    let error = robot_hal_core::HalError::new(
        "runtime.queue.full",
        robot_hal_core::ErrorCategory::Unavailable,
        "serial.write",
        true,
        "queue is full",
    )
    .unwrap()
    .with_resource_id(ResourceId::parse("serial:virtual:0").unwrap())
    .with_platform_code("11")
    .unwrap()
    .with_vendor_code("VENDOR_BUSY")
    .unwrap()
    .with_context(context);

    assert_eq!(error.resource_id().unwrap().as_str(), "serial:virtual:0");
    assert_eq!(error.platform_code(), Some("11"));
    assert_eq!(error.vendor_code(), Some("VENDOR_BUSY"));
    assert_eq!(error.context().iter().collect::<Vec<_>>(), vec![
        ("limit_bytes", "1024"),
        ("queueDepth", "64"),
    ]);
}

#[test]
fn legacy_error_constructor_has_empty_details() {
    let error = robot_hal_core::HalError::new(
        "runtime.session.closed",
        robot_hal_core::ErrorCategory::Conflict,
        "serial.read",
        false,
        "closed",
    )
    .unwrap();
    assert!(error.resource_id().is_none());
    assert!(error.platform_code().is_none());
    assert!(error.vendor_code().is_none());
    assert!(error.context().is_empty());
}
```

Add separate tests for duplicate keys, invalid key syntax, the 17th entry, a 65-byte key, a 1,025-byte value, 8,193 aggregate bytes, empty/non-ASCII/256-byte codes, and unchanged decision-only serde output. Construct exact-limit inputs and assert they succeed before asserting one-byte-over inputs fail.

- [ ] **Step 2: Implement `ErrorContext` and named `HalError` fields**

In `error.rs`, add:

```rust
use std::collections::BTreeMap;
use crate::ResourceId;

const ERROR_CONTEXT_MAX_ENTRIES: usize = 16;
const ERROR_CONTEXT_MAX_KEY_BYTES: usize = 64;
const ERROR_CONTEXT_MAX_VALUE_BYTES: usize = 1024;
const ERROR_CONTEXT_MAX_TOTAL_BYTES: usize = 8192;

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ErrorContext(BTreeMap<String, String>);
```

Implement the generic constructor:

```rust
pub fn new<I, K, V>(entries: I) -> HalResult<Self>
where
    I: IntoIterator<Item = (K, V)>,
    K: Into<String>,
    V: Into<String>;
```

Validate before insertion, reject duplicates with `error.context.duplicate_key`, and use stable invalid-argument names under `error.context.*`. Implement `iter()` returning `(&str, &str)` and `is_empty()`.

Replace the tuple `HalError` implementation with named private fields while keeping `HalError::new` source-compatible. Initialize details to `None`/default in both constructors and decision-only deserialization. Add the enrichment methods and accessors specified by the design. Reuse `validate_identifier` for platform/vendor codes.

Export `ErrorContext` from `crates/robot-hal-core/src/lib.rs`.

- [ ] **Step 3: Refactor and statically self-review without executing tools**

Keep validation helpers private, remove repeated error construction, and inspect the changed source and tests for contract coverage. Do not run formatting, lint, build, or test commands.

- [ ] **Step 4: Commit the core contract**

```bash
git add crates/robot-hal-core/src/error.rs crates/robot-hal-core/src/lib.rs crates/robot-hal-core/tests/core_contract.rs
git commit -m "feat(core): add structured error details"
```

### Task 2: Extend protobuf wire v1 and centralize Rust conversion

**Files:**
- Modify: `proto/seeed/hal/v1/hal.proto`
- Modify: `crates/robot-hal-protocol/src/conversion.rs`
- Modify: `crates/robot-hal-protocol/src/lib.rs`
- Modify: `crates/robot-hal-protocol/tests/protocol_contract.rs`
- Regenerate: `bindings/python/robot_hal/proto/hal_pb2.py`

**Interfaces:**
- Consumes: `ErrorContext` and enriched `HalError` from Task 1.
- Produces: additive protobuf fields 6–9 and `pub fn error_from_proto(v1::Error) -> HalResult<HalError>`.

- [ ] **Step 1: Write protocol round-trip and compatibility tests without running them**

Add a rich-error round-trip test that constructs all details, converts with `v1::Error::from(&error)`, then calls `error_from_proto` and compares every accessor. Add a legacy test using only fields 1–5 and asserting empty details. Add malformed tests for invalid `resource_id`, invalid codes, invalid context keys, and every size/count bound.

Extend the existing protobuf field-lock test so `Error` asserts these exact tags:

```rust
let encoded = v1::Error {
    resource_id: "serial:virtual:0".to_owned(),
    platform_code: "11".to_owned(),
    vendor_code: "VENDOR_BUSY".to_owned(),
    context: [("queueDepth".to_owned(), "64".to_owned())].into(),
    ..valid_error()
}.encode_to_vec();
// Assert fields 6, 7, 8, and 9 are present without changing fields 1–5.
```

- [ ] **Step 2: Add protobuf fields and generate bindings**

Append to `message Error` without changing existing tags:

```protobuf
string resource_id = 6;
string platform_code = 7;
string vendor_code = 8;
map<string, string> context = 9;
```

Run the repository generator as an implementation step:

```bash
./scripts/generate-protocol.sh
```

This is permitted because it materializes the checked-in binding; it is not a test, lint, build, or formatting check. Review the generated Python diff and confirm no hand-written file outside the expected binding changed. The generated-code verification check runs in Task 7.

- [ ] **Step 3: Implement both Rust conversion directions**

Extend `From<&HalError> for v1::Error` to populate all details. Add:

```rust
pub fn error_from_proto(value: v1::Error) -> HalResult<HalError>;
```

Map the category first, build the legacy error with `HalError::new`, then conditionally validate non-empty optional strings and construct `ErrorContext::new(value.context)`. Map every peer-supplied detail validation failure to `invalid_message` inside `error_from_proto`, retaining only a safe field-level diagnostic. Export the function from `lib.rs`. Do not silently drop or truncate invalid details.

- [ ] **Step 4: Statically self-review and commit the additive wire contract**

Inspect the schema tags, both conversion directions, generated binding diff, and tests. Do not run formatting, lint, build, or test commands.

```bash
git add proto/seeed/hal/v1/hal.proto crates/robot-hal-protocol/src/conversion.rs crates/robot-hal-protocol/src/lib.rs crates/robot-hal-protocol/tests/protocol_contract.rs bindings/python/robot_hal/proto/hal_pb2.py
git commit -m "feat(protocol): preserve structured error details"
```

### Task 3: Attach canonical resource identity to resolver and active lease errors

**Files:**
- Modify: `crates/robot-hal-core/src/identity.rs`
- Modify: `crates/robot-hal-core/tests/core_contract.rs`
- Modify: `crates/robot-hal-runtime/src/lease_table.rs`
- Modify: `crates/robot-hal-runtime/tests/serial_runtime.rs`

**Interfaces:**
- Consumes: `HalError::with_resource_id` from Task 1.
- Produces: resource-scoped resolver and active-lease errors that carry the canonical `ResourceId`.

- [ ] **Step 1: Write enrichment assertions without running them**

Extend resolver tests so both not-found and ambiguous failures assert:

```rust
assert_eq!(error.resource_id(), Some(selector.id()));
```

Extend `stale_generation_never_reaches_the_adapter` so the stale error asserts the selected descriptor ID. Add a conflict-open assertion that `runtime.lease.conflict` also carries the descriptor ID.

- [ ] **Step 2: Enrich errors at creation sites**

In `resolve_resource`, attach `selector.id().clone()` to not-found and ambiguous errors. In `LeaseTable::reserve_control` and every error path of `LeaseTable::validate`, attach the supplied `resource_id.clone()` after constructing the stable error.

Do not add a resource ID to closed-session replay errors because `Registry::ClosedSession` does not currently retain one; expanding retained replay state is outside this task.

- [ ] **Step 3: Statically self-review and commit resource enrichment**

Inspect every changed error path and its assertions. Do not run formatting, lint, build, or test commands.

```bash
git add crates/robot-hal-core/src/identity.rs crates/robot-hal-core/tests/core_contract.rs crates/robot-hal-runtime/src/lease_table.rs crates/robot-hal-runtime/tests/serial_runtime.rs
git commit -m "fix(runtime): attach canonical resource error identity"
```

### Task 4: Preserve native Serial platform codes

**Files:**
- Modify: `adapters/serialport/src/lib.rs`
- Modify: `adapters/serialport/src/session.rs`

**Interfaces:**
- Consumes: `HalError::with_platform_code` from Task 1.
- Produces: native `std::io::Error::raw_os_error()` as a decimal `platform_code` without debug-message parsing.

- [ ] **Step 1: Change existing diagnostics tests to require structured codes without running them**

Update `io_error_diagnostics_preserve_raw_os_error_code`:

```rust
assert_eq!(error.platform_code(), Some("13"));
```

Update `actual_open_missing_endpoint_carries_io_raw_os_error_from_open_path` to assert `platform_code()` equals the independently obtained decimal OS code. Keep the existing debug assertion only as a diagnostic regression, not as the source of the structured assertion.

- [ ] **Step 2: Enrich `map_io_error` without parsing messages**

Capture `raw_os_error()` before consuming the I/O error. Build the existing `HalError`, and only when the integer exists call:

```rust
error.with_platform_code(raw_os_error.to_string())
    .expect("a decimal OS error code is a valid platform code")
```

Do not populate `vendor_code`; `serialport::ErrorKind` is not a documented vendor code.

- [ ] **Step 3: Statically self-review and commit platform enrichment**

Inspect both native I/O mapping paths and their tests. Do not run formatting, lint, build, or test commands. The physical loopback test remains ignored.

```bash
git add adapters/serialport/src/lib.rs adapters/serialport/src/session.rs
git commit -m "fix(serial): preserve native platform error codes"
```

### Task 5: Make the Rust client use the shared wire conversion

**Files:**
- Modify: `crates/robot-hal-client/src/connection.rs`
- Modify: `crates/robot-hal-client/tests/client_contract.rs`

**Interfaces:**
- Consumes: `robot_hal_protocol::error_from_proto` from Task 2.
- Produces: rich and legacy broker errors through every Rust-client response path.

- [ ] **Step 1: Write Rust-client tests without running them**

Add a fake-server request error containing all four details and assert the returned `HalError` preserves them. Add a legacy fields-1–5 error test. Add malformed detail tests for normal responses and `request_id == 0` unsolicited errors; assert they terminate the connection with `runtime.protocol.invalid_message` and fan out to pending requests.

Add one real broker round-trip using `Broker` with `VirtualSerialAdapter`: connect `HalClient`, construct a valid `ResourceSelector::exact` for the absent ID `serial:virtual:missing`, call `open_serial`, and assert the returned `runtime.resource.not_found` error carries that exact resource ID. This proves core resolver → adapter → runtime → broker → protocol → Rust client preservation without test-only production hooks.

- [ ] **Step 2: Delegate decoding to `robot-hal-protocol`**

Import `error_from_proto`, replace all local `decode_error` calls, and delete the duplicate category/error constructor in `connection.rs`. Preserve existing termination behavior: `error_from_proto` already normalizes malformed peer details to `runtime.protocol.invalid_message`, which closes the connection.

- [ ] **Step 3: Statically self-review and commit the Rust client conversion**

Inspect all normal and unsolicited error paths and their tests. Do not run formatting, lint, build, or test commands.

```bash
git add crates/robot-hal-client/src/connection.rs crates/robot-hal-client/tests/client_contract.rs
git commit -m "fix(client): retain structured broker errors"
```

### Task 6: Add immutable structured errors to the Python client

**Files:**
- Modify: `bindings/python/robot_hal/errors.py`
- Modify: `bindings/python/robot_hal/client.py`
- Modify: `bindings/python/tests/test_client_contract.py`
- Modify: `bindings/python/tests/test_client_hardening.py`

**Interfaces:**
- Consumes: generated protobuf fields from Task 2.
- Produces: backward-compatible Python `HalError` details and identical Rust/Python wire validation bounds.

- [ ] **Step 1: Write Python model and decode tests without running them**

Add a direct construction test:

```python
source = {"queueDepth": "64"}
error = HalError(
    "runtime.queue.full",
    ErrorCategory.UNAVAILABLE,
    "serial.write",
    True,
    "full",
    resource_id="serial:virtual:0",
    platform_code="11",
    vendor_code="VENDOR_BUSY",
    context=source,
)
source["queueDepth"] = "changed"
assert dict(error.context) == {"queueDepth": "64"}
with pytest.raises(TypeError):
    error.context["new"] = "value"
```

Add broker-response tests for rich fields, legacy absent fields, duplicate-copy fan-out, invalid IDs/codes/keys, the 17th entry, 65-byte key, 1,025-byte value, and 8,193 aggregate bytes. Assert malformed details terminate with `runtime.protocol.invalid_message`.

- [ ] **Step 2: Implement immutable Python details**

In `errors.py`, import `field`, `Mapping`, and `MappingProxyType`. Add optional fields after `debug_message` and normalize in `__post_init__`:

```python
context: Mapping[str, str] = field(
    default_factory=lambda: MappingProxyType({}),
    hash=False,
)

def __post_init__(self) -> None:
    Exception.__init__(self, self.debug_message)
    object.__setattr__(self, "context", MappingProxyType(dict(self.context)))
```

Extend `_ErrorData` to store `resource_id`, both codes, and `context_items: tuple[tuple[str, str], ...]`. `_error_data` snapshots sorted items; `_fresh_error` constructs a fresh mapping so terminal fan-out retains fresh immutable errors.

- [ ] **Step 3: Validate wire details in one Python helper**

In `client.py`, add constants matching the Rust bounds and a helper that:

- treats empty optional strings as `None`;
- validates non-empty resource IDs with existing identifier validation;
- validates codes as non-empty ASCII up to 255 bytes;
- validates context entry count, key pattern, UTF-8 byte lengths, and aggregate bytes;
- returns an immutable-copy-ready plain dictionary;
- raises `_invalid_message` for malformed peer data.

Extend `_decode_error` to pass all details to `HalError`. Do not parse `debug_message`.

- [ ] **Step 4: Statically self-review and commit the Python client contract**

Inspect immutability, fresh fan-out, validation parity, and their tests. Do not run formatting, lint, build, or test commands.

```bash
git add bindings/python/robot_hal/errors.py bindings/python/robot_hal/client.py bindings/python/tests/test_client_contract.py bindings/python/tests/test_client_hardening.py
git commit -m "fix(python): retain immutable structured errors"
```

### Task 7: Align documentation and run the single release-grade verification gate

**Files:**
- Modify: `docs/architecture/hal-architecture.md`
- Modify: `docs/contracts/versioning.md`
- Modify: `docs/releases/v0.1.0-acceptance.md`

**Interfaces:**
- Consumes: verified implementation from Tasks 1–6.
- Produces: factual architecture, compatibility rules, and dated acceptance evidence.

- [ ] **Step 1: Update architecture and compatibility documentation**

Make the `HalError` example match the implemented named fields and document `ErrorContext` bounds. In the versioning contract, record `Error` fields 6–9, legacy empty-field decoding, unknown-field tolerance, and the rule that details are non-decision diagnostics.

- [ ] **Step 2: Run formatting and generated-code checks**

Run:

```bash
cargo fmt --all --check
./scripts/check-generated-protocol.sh
```

- [ ] **Step 3: Run warnings-denied lint**

Run:

```bash
cargo clippy --workspace --all-targets --all-features -- -D warnings
```

- [ ] **Step 4: Run the complete Rust suite**

Run:

```bash
cargo test --workspace --all-features
```

Record the exact passed/failed/ignored counts from fresh output.

- [ ] **Step 5: Run the frozen Python 3.11 suite**

Run:

```bash
uv run --project bindings/python --python 3.11 --frozen pytest -q
```

Record the exact count from fresh output.

- [ ] **Step 6: Run broker black-box conformance**

Run:

```bash
cargo build -p robot-hal-broker-app --features virtual-adapter
uv run --project bindings/python --frozen python tests/conformance/run-broker-conformance.py --broker target/debug/robot-hal-broker
```

Record the exact checks passed. Do not claim native Linux, native Windows, remote CI, or physical-loopback qualification without fresh evidence.

- [ ] **Step 7: Update acceptance evidence with only fresh results**

Append a dated structured-error section listing the exact commands, host, counts, and remaining external gates. Do not rewrite earlier historical evidence.

- [ ] **Step 8: Review the final diff and contract coverage**

Run:

```bash
git diff --check
git status --short
```

Confirm every design-spec testing bullet maps to a passing test and that `.codegraph/` and `.cursor/` remain untracked and unstaged.

- [ ] **Step 9: Commit documentation and evidence**

```bash
git add docs/architecture/hal-architecture.md docs/contracts/versioning.md docs/releases/v0.1.0-acceptance.md
git commit -m "docs(v0.1): record structured error contract"
```

## Plan Self-Review

- Spec coverage: all Rust core, wire, runtime, native adapter, Rust client, Python client, observability, compatibility, and verification requirements have an owning task.
- Scope: event, listener, future hardware classes, and unrelated protocol refactors remain excluded.
- Type consistency: `ErrorContext`, enrichment method names, protobuf fields 6–9, and `error_from_proto` match the approved spec in every task.
- Deferred verification: every production behavior has focused contract tests, but Tasks 1–6 execute no test, lint, build, or formatting command. Task 7 is the sole verification gate after all implementation is complete; protocol generation in Task 2 is an implementation step, not verification.
