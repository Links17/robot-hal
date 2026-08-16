# Seeed HAL Architecture

**Status:** v0.1 core and Serial implemented; later hardware classes planned
**Date:** 2026-08-14  
**Scope:** Team-reusable Rust HAL; `lerobot-easy` is the first consumer, not the domain model.

## 1. Objective

Build a cross-platform, library-first Rust HAL for macOS, Windows, and Linux. The same implementation runs in-process for Rust consumers or behind a local broker for Python, Node, Electron, and multi-process consumers.

The HAL covers Serial, CAN/CAN FD, USB, GPIO, and Camera as standard hardware classes. It deliberately excludes device protocols and product business behavior.

## 2. Architecture

```text
Applications and device-protocol drivers
                    │
                    │ HAL interfaces or broker clients
                    ▼
┌─────────────────────────────────────────────────┐
│ Seeed HAL                                       │
│ identity · discovery · sessions · leases        │
│ async I/O · timeout · cancellation · hotplug    │
│ errors · events · diagnostics · metrics         │
└───────┬─────────────┬───────────┬────────┬──────┘
        │             │           │        │
     Serial          CAN         USB      GPIO   Camera
        │             │           │        │       │
┌───────▼─────────────▼───────────▼────────▼───────▼───┐
│ Platform and vendor transport adapters               │
│ serialport · SocketCAN · PCAN · nusb · gpiod · media │
└───────────────────────────┬───────────────────────────┘
                            ▼
                   OS drivers and hardware
```

The responsibility contract is normative: [HAL responsibility](../contracts/hal-responsibility.md).

## 3. Deployment forms

### 3.1 In-process library

Rust applications construct a runtime, register compiled adapters, and obtain typed hardware-class sessions. The library does not start a process-wide Tokio runtime, tracing subscriber, or signal handler.

### 3.2 Local broker

The broker constructs the same runtime and exposes it through versioned local IPC:

- Unix domain socket on macOS and Linux;
- Windows Named Pipe on Windows;
- length-delimited Protocol Buffers messages;
- connection handshake and capability negotiation;
- per-request correlation and event subscriptions.

The broker is a deployment adapter, not a separate implementation. Platform handles never cross IPC.

The v0.1 broker accepts frames up to exactly 1 MiB. Each connection has a 32-request admission
queue, at most 32 executing requests, and a 64-response queue; runtime event subscriptions retain
64 events. Request or execution admission overflow returns `runtime.queue.full`. Response overflow
returns `runtime.queue.response_full`, closes the connection, and is recorded in its cleanup outcome
because no further response can be safely admitted; event lag reports `runtime.event.lagged`. These
defaults are configurable downward or upward for embedding, but all queues remain bounded.

The codec enforces the hard 1 MiB frame cap before protobuf decode. After a handshake request is
admitted, the reader pauses until dispatch validates it and publishes the accepted frame limit;
already-pipelined frames therefore cannot race negotiation. That accepted limit applies to every
subsequent raw inbound frame and encoded outbound envelope, including reader-generated protocol and
queue errors. The writer checks protobuf field/envelope overhead against both the negotiated limit
and the hard cap before allocating its encode buffer. Session lifecycle events are owner-scoped: a
connection receives only events whose `OwnerId` matches that connection; adding a future global
event kind requires an explicit visibility decision in the broker.

Connection teardown revokes the owner before waiting for socket reader/writer tasks. The broker
permits only a bounded response drain and aborts a stalled task after the connection-task shutdown
deadline, so a peer that stops reading cannot retain hardware ownership.

The Rust broker client keeps a remote Serial handle reusable when a close request is rejected by
local bounded-queue admission, allowing the caller to retry `close(&mut self)`. Once a close
response succeeds, that handle is terminal and rejects every later operation locally; dropping an
unclosed handle still relies on owner cleanup when its client connection terminates.

Each launch creates a 256-bit startup token. The handshake compares it in constant time and rejects
incompatible protocol versions, unsupported required capabilities, and invalid frame/read/write
limits before exposing resources. Client and broker advertise inclusive minor ranges within one
major and select the highest shared minor; peers that omit both additive range fields retain the
legacy exact-minor behavior. Unix endpoints live in a caller-private `0700` directory and the socket
is `0600`. Windows uses a unique per-launch Named Pipe with remote clients rejected and a protected
DACL granting access only to the current user, LocalSystem, and built-in Administrators.

### 3.3 Desktop integration

For Electron applications, Electron Main owns broker process lifecycle and update activation. Renderer code never connects directly to the broker. The application backend or Electron Main uses a language client according to the application's own architecture.

### 3.4 Python broker client

The Python binding exposes protobuf-independent async `HalClient` and `SerialSession` types. It
performs authentication and limit negotiation before starting one bounded writer task and one
reader task. Pending requests, cancellation and completion tombstones, writer admission, and event
delivery are bounded; request IDs are nonzero and correlated independently of response order.

Unix uses asyncio Unix sockets. On Windows, one tracked `asyncio.to_thread` call performs connection
setup, then a bounded per-transport actor thread exclusively owns `ReadFile`, `WriteFile`, and
`CloseHandle`. The pipe is placed in nonblocking byte mode so that actor can multiplex pending work;
cancelling an active operation terminally closes the transport and waits for handle ownership to
return before reporting cancellation. Both transports use the broker's big-endian 32-bit length
prefix, reject the hard 1 MiB limit before reading a frame body, and apply negotiated
frame/read/write limits. The binding wipes mutable token and encode buffers it owns;
Python/protobuf/asyncio or kernel-created immutable and transient copies are outside that
best-effort zeroization boundary.

## 4. Workspace modules

```text
seeed-hal/
├── crates/
│   ├── seeed-hal-core/
│   ├── seeed-hal-runtime/
│   ├── seeed-hal-protocol/
│   ├── seeed-hal-broker/
│   ├── seeed-hal-client/
│   ├── seeed-hal-serial/
│   ├── seeed-hal-can/
│   ├── seeed-hal-usb/
│   ├── seeed-hal-gpio/
│   ├── seeed-hal-camera/
│   └── seeed-hal-testkit/
├── adapters/
│   ├── serialport/
│   ├── socketcan/
│   ├── pcan/
│   ├── nusb/
│   ├── linux-gpio/
│   └── camera-platform/
├── bindings/
│   ├── python/
│   └── node/
└── apps/
    └── seeed-hal-broker/
```

Only modules needed by the current vertical slice are created. Empty future crates are prohibited.

## 5. Core model

### 5.1 Resource identity

`ResourceId` identifies a physical resource using the strongest available platform-neutral evidence. `Endpoint` describes its current access path. Endpoint values such as `/dev/ttyUSB0`, `COM7`, `can0`, and `PCAN_USBBUS1` are snapshots, not identity truth.

```rust
pub struct ResourceDescriptor {
    pub id: ResourceId,
    pub transport: TransportKind,
    pub endpoint: Endpoint,
    pub identity_quality: IdentityQuality,
    pub properties: ResourceProperties,
    pub capabilities: CapabilitySet,
}
```

Identity quality is `Strong`, `Medium`, or `Weak`. A caller can reject weak identities when persistent binding requires stronger guarantees.

`ResourceSelector.minimum_identity_quality` is an inclusive threshold with the stable ordering
`Weak < Medium < Strong`. The canonical resolver matches the persisted resource ID, transport,
minimum identity quality, and required hardware-class capability. It deliberately ignores the
transient endpoint, including for disambiguation. Zero matching descriptors return
`runtime.resource.not_found`; more than one returns `runtime.resource.ambiguous` and fails closed.

### 5.2 Capabilities

Capabilities describe only transport or standard hardware-class behavior. Identifiers are stable and versioned, such as:

- `serial.bytes/v1`;
- `can.classic/v1`;
- `can.fd/v1`;
- `usb.bulk/v1`;
- `gpio.edge-events/v1`;
- `camera.frames/v1`.

Standard capability configuration uses typed Rust and protobuf messages. Namespaced extensions cannot redefine a standard capability.

### 5.3 Sessions and leases

Opening a resource creates a session. In broker deployment the session is opaque to the client and the broker retains the OS handle.

Lease modes are:

- `Observe`: read-only access when the adapter can safely fan out observations;
- `Control`: normal read/write ownership; at most one controlling generation;
- `Maintenance`: fully exclusive reconfiguration.

Every exposed lease includes an incrementing generation. Opening first creates a provisional
reservation; only successful runtime publication commits its fencing generation. A failed,
cancelled, or otherwise unexposed open compare-checks and rolls back that exact current reservation,
while any generation already exposed to a caller remains monotonic. Requests with a stale
generation fail before reaching the adapter. Queues are bounded and each hardware-class interface
documents overflow behavior.

HAL lease expiry performs transport cleanup only. Domain-safe physical behavior stays above HAL.

### 5.4 Errors

All errors carry stable structure:

```rust
pub struct HalError {
    name: ErrorName,
    category: ErrorCategory,
    operation: OperationName,
    retryable: bool,
    debug_message: String,
    resource_id: Option<ResourceId>,
    platform_code: Option<String>,
    vendor_code: Option<String>,
    context: ErrorContext,
}
```

`HalError::new(name, category, operation, retryable, debug_message)` preserves the legacy empty-detail
construction path. Read-only accessors expose every field, while the consuming
`with_resource_id`, `with_platform_code`, `with_vendor_code`, and `with_context` methods add
diagnostics. Platform and vendor codes reuse the non-empty ASCII identifier bound of 255 bytes.

`ErrorContext` is an ordered, validated string-to-string map with at most 16 entries. Keys are 1–64
ASCII bytes and match `[a-z][a-zA-Z0-9_-]*`; values may be empty and are at most 1,024 UTF-8 bytes.
The aggregate key-plus-value size is at most 8,192 bytes, and duplicate input keys fail rather than
overwrite an earlier value.

Callers make stable decisions only from `name`, `category`, `operation`, and `retryable`. The debug
message, resource identity, platform/vendor codes, and context are non-decision diagnostics and
must never be parsed to select behavior. Decision-only serde excludes all diagnostics; broker
protobuf conversion is the explicit cross-process detail transport.

### 5.5 Events

The runtime publishes ordered events for resource discovery, endpoint change, health change, session lifecycle, lease lifecycle, and transport failure. Each stream includes a sequence number. Reconnecting clients obtain a fresh snapshot and resume only where the broker explicitly retains the requested sequence.

## 6. Hardware-class interfaces

Each hardware class has its own typed interface. A generic byte-stream interface must not be stretched to represent CAN frames, USB transfers, GPIO edges, or camera frames.

The initial Serial interface covers:

- enumerate and filter descriptors;
- open from a `ResourceSelector` plus `SerialConfig`;
- bounded async read and write;
- flush and cancellation;
- line-control operations where supported;
- explicit close and idempotent cleanup.

CAN, USB, GPIO, and Camera add separate interfaces in later vertical slices while reusing core identity, session, lease, error, and event behavior.

## 7. Camera data plane

Camera belongs in HAL at the capture interface: enumeration, stable identity, format negotiation, controls, frames, timestamps, hotplug, and lifecycle. Preview, encoding policy, recording, image processing, inference, and application camera roles remain outside HAL.

Camera control uses normal broker IPC. Frame payloads use a bounded shared-memory ring or an equivalent explicitly negotiated zero/low-copy data plane. Protobuf carries descriptors and ownership metadata, not full-rate video frames.

## 8. Concurrency and cleanup

- The runtime owns one task or blocking worker per opened resource as required by its adapter.
- Blocking vendor calls never execute on Tokio executor workers.
- Operation queues are bounded and preserve documented ordering.
- Cancellation has a deadline; adapters that cannot cancel synchronously are isolated in a disposable worker.
- The Python Windows transport uses one bounded, per-transport actor thread as the sole steady-state
  owner of its Named Pipe handle. Its command capacity is four; full or closed admission fails
  without blocking the event loop. Native Windows execution remains an acceptance gate for
  pywin32 `PIPE_NOWAIT` behavior.
- The native Serial adapter serializes port access on one owned blocking actor with a one-command queue and no interrupt clone. Flush polls the platform output-queue count (`TIOCOUTQ` on Unix, `ClearCommError`/`COMSTAT.cbOutQue` on Windows) and succeeds only after it reaches zero; cancellation or the logical deadline stops polling and terminally releases the actor-owned handle. Read and write use finite nonzero native timeout slices of at most 100 ms while the configured Serial timeout remains the authoritative outer deadline.
- Adapter-level Serial close has a configurable deadline and defaults to two seconds. On timeout,
  the runtime drops the resource actor, releases its lease, completes close waiters with
  `runtime.session.close_timeout`, and does not continue using that session.
- Authenticated close replay is idempotent for the 256 most recently closed sessions in a runtime.
  The replay key is the exact `SessionId` and `LeaseToken`; closing a 257th newer session evicts the
  oldest entry, whose next replay returns `runtime.session.not_found`.
- Client disconnect revokes its leases, rejects new operations, cancels queued work, and closes transport resources.
- An unrelated external process can still bypass HAL; adapters use OS exclusivity where available and report limitations otherwise.

## 9. IPC security

- Local endpoints are accessible only to the current user by default.
- Broker startup creates a random per-launch authentication token supplied to trusted clients through the host application's private process environment or inherited handle.
- Authentication occurs during handshake before resource metadata is exposed.
- Tokens are never logged.
- Renderer processes do not receive broker credentials.

When the executable receives a startup token by file path, it performs the blocking trust checks on
a dedicated blocking worker and removes the file only after reading exactly 32 bytes. On Unix, the
parent must be a real `0700` directory; the token must be a non-symlink regular file that is
owner-readable, inaccessible to group/other, owned by the broker's effective user, and have exactly
one hard link. The executable retains a no-follow parent directory descriptor, opens and revalidates
the token relative to that descriptor, and deletes it with descriptor-relative `unlinkat` only after
the device/inode/owner identities still match. On Windows, the parent and token must be non-reparse
directory/file objects owned by the current process user. Their protected DACLs may grant effective
access only to that user, LocalSystem, or built-in administrators. The
executable retains the parent handle and file handles that deny write/delete sharing, requires one
hard link, and revalidates handle security plus volume/file identity before marking the validated
file handle for deletion. It does not arm delete-on-close until every trust check succeeds. A failed
length, type, ownership, ACL, link-count, or identity check leaves the path in place.

The zeroization guarantee is limited to specific mutable startup-token buffers explicitly owned by
this code: the executable and client token arrays, decoded broker/client protobuf token vectors,
broker input frame, and client encode scratch buffer. This code zeroizes those buffers on drop or
before releasing them. Any buffer outside that list is outside this guarantee.

## 10. Observability

The library emits structured tracing spans and metrics but never installs a subscriber. Required dimensions include adapter, transport kind, operation, normalized result, duration, queue depth, and retry count. Resource serial numbers and raw payloads are excluded from logs by default. The broker executable records both connection and owner-cleanup outcomes using only the error kind, stable name, category, operation, and retryable flag; diagnostic strings and credentials are excluded.

## 11. Delivery sequence

1. **v0.1:** core, runtime, broker, Rust/Python clients, virtual adapter, Serial.
2. **v0.2:** CAN/CAN FD, SocketCAN, and PCAN.
3. **v0.3:** USB and GPIO, with GPIO-through-USB adapters supported on desktop platforms.
4. **v0.4:** Camera control plus shared-memory frame data plane.
5. **v1.0:** stabilized interfaces, compatibility guarantees, and cross-platform conformance qualification.

Each version is a working vertical slice. Later modules may refine core only through backward-compatible additions or an explicit contract revision.

Implementation status is intentionally separate from cross-platform release qualification:

- Implemented in v0.1: core identity/capability/lease/error/event types, runtime ownership and
  fencing, the versioned local broker, Rust and Python clients, the virtual Serial conformance
  adapter, and the native `serialport` adapter.
- Qualified locally: hardware-free macOS execution and the warnings-denied Windows Rust cross-target
  check recorded in [v0.1.0 acceptance](../releases/v0.1.0-acceptance.md). The Linux cross-target
  check remains blocked by the missing libudev pkg-config target sysroot/wrapper.
- Pending external acceptance: native Linux and Windows CI execution and physical Serial loopback.
- Planned: CAN/CAN FD, USB, GPIO, Camera, Node bindings, shared-memory frame transport, device
  protocols, and consuming-application migration.

These modules retain the responsibility boundary defined in
[HAL responsibility](../contracts/hal-responsibility.md); implementation status does not move
device protocols or product behavior into the HAL.

## 12. Decisions

- The asset is independent of its first consumer.
- The HAL is business-independent.
- The implementation is library-first.
- The broker exposes, but does not duplicate, library semantics.
- Hardware-class interfaces are typed and separate.
- Physical identity and endpoint are distinct.
- Camera is in target scope but not v0.1.
- v0.1 begins with Serial and a virtual adapter.
