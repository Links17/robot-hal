# Windows Shared-Memory Camera Data Plane Design

**Status:** Approved design; implementation pending

## Goal

Provide the Windows implementation of the existing `camera.frames.shm/v1` shared-memory data plane so the virtual Camera profile can complete protocol minor 3 conformance on macOS, Linux, and Windows without routing frame payloads through broker protobuf IPC.

## Scope and boundary

This work is confined to the platform adapter that backs the already-defined shared-memory mapping, its Windows tests, release-tool diagnostics, and Hosted Windows conformance evidence.

It does not add camera product behavior, encode video, add WebRTC/H.264, change the camera wire contract, or transfer frame bytes through the broker control plane. The broker continues to own handles and the frame mapping lifecycle; clients receive opaque mapping descriptors and open the mapping read-only.

## Security model

Each broker-created mapping has an unpredictable name supplied by the existing mapping identity/token machinery and is bound by existing descriptor and header validation to the negotiated layout and lease generation.

The Windows adapter creates a named section with a protected explicit DACL granting access only to:

- the current broker user SID;
- LocalSystem;
- built-in Administrators.

The broker creates the section read/write. A client opens it read-only. Mapping creation rejects an already-existing name rather than attaching to an externally created section. No fallback to anonymous memory, an unprotected DACL, or broker IPC is permitted.

All security descriptor, Win32 handle, section, view, and locking FFI stays in `adapters/shared-memory` under `cfg(windows)`. Every adapter-local unsafe operation has a preceding `SAFETY:` invariant and a focused Windows test or upstream API contract citation. Core, runtime, protocol, broker, and client crates remain free of unsafe code.

The platform adapter cannot make a pathname or handle safe against a hostile process with the same OS identity and equivalent process privileges; successful creation/open and post-open descriptor/header validation remain fail-closed. The public contract must not claim stronger same-user isolation than Windows object security provides.

## Windows Mapping contract

`platform::Mapping` retains the current internal interface:

- `create(name, length)` creates a new writable mapping;
- `open_read_only(name, length)` opens an existing mapping read-only;
- `as_ptr()` exposes only the internally owned mapped range;
- `try_lock_shared`, `try_lock_exclusive`, and `unlock` preserve non-blocking ring ownership semantics;
- `unlink(name)` retires the named mapping only after broker lifecycle code confirms readers have released it;
- `Drop` unmaps the view and closes every owned Windows handle.

Before calling Win32 APIs, all mapping lengths and page-aligned view ranges must be non-zero and representable in the `CreateFileMappingW` high/low size fields. Existing `RingConfig`, descriptor, token, header magic/layout version, resource identity, lease generation, slot count, stride, and plane-bound validations remain authoritative. A section whose opened/viewable length is shorter than the descriptor requirement is rejected before any header read.

The implementation uses `CreateFileMappingW` with the explicit security attributes and checks `ERROR_ALREADY_EXISTS` immediately after successful creation. It uses `OpenFileMappingW(FILE_MAP_READ, ...)` for readers and maps exactly the descriptor-validated length. All system errors become the existing stable shared-memory structured errors at callers; sensitive mapping names, descriptors, SIDs, paths, and startup tokens are never emitted in user-facing diagnostics.

## Locking and lifecycle

Windows must implement the same non-blocking shared/exclusive coordination expected by the ring:

- broker writes and close/unlink paths take exclusive ownership;
- readers take shared ownership;
- acquisition contention reports an unavailable/busy condition instead of blocking a Tokio worker;
- releasing a mapping always releases the matching lock and native handles.

The lock object is created under the same explicit DACL and uses a distinct unpredictable name derived from the mapping identity. Creation collision, type mismatch, access denial, abandoned lock, or failed release is treated as a fail-closed platform error. A mapping close first prevents new reader opens, then waits according to the existing bounded reader-close contract, unmaps and closes the local resources, and finally retires named objects.

## Diagnostics

`run-virtual-conformance` continues to fail with a stable release error on non-zero runner exit, but includes a bounded, sanitized summary of the child failure. It may identify a stable structured error name and operation such as `shared_memory.unavailable` / `shared_memory.create`; it must redact absolute paths, mapping names, tokens, and unbounded output. This makes Hosted failures actionable without exposing control-plane secrets.

## Test and qualification strategy

TDD precedes each Windows implementation slice. Windows-only adapter tests must cover:

1. create, writable publish, read-only reopen, and frame copy through the existing ring;
2. rejection of a pre-existing named mapping and lock collision;
3. protected-DACL access behavior using an unauthorized process/token fixture where Hosted Windows supports it;
4. undersized section, malformed header, descriptor/token/generation mismatch, and unsupported object/open failures;
5. non-blocking shared/exclusive lock conflict, unlock, close, and reopen behavior;
6. bounded cleanup with a reader held open and no stale mapping available after retire;
7. Windows-only compile/runtime checks for every unsafe Windows API invariant.

The GitHub Hosted Windows platform job then builds the production broker, builds the virtual-adapter broker, and passes protocol minors 0 through 3. Minor 3 must exercise virtual camera capture and `camera.frames.shm/v1`; it cannot skip the capability or report it as passed without mapping creation and read-only client verification.

Only a successful Hosted run with source gate plus macOS, Linux, and Windows platform evidence may update the RC qualification document from Pending/Partial for software conformance. Physical hardware qualification remains separately Pending.

## Alternatives rejected

- Disabling `camera.frames.shm/v1` only on Windows: creates a platform-split minor-3 capability contract and leaves the already advertised camera data plane incomplete.
- Sending frames through protobuf broker requests/responses: violates the bounded shared-memory data-plane architecture.
- Using default DACLs or an anonymous mapping fallback: makes access policy implicit and cannot meet broker ownership requirements.
- Declaring Hosted Windows passed based on production build alone: does not qualify minor-3 virtual camera behavior.
