# Seeed HAL Architecture

**Status:** Approved design  
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

### 3.3 Desktop integration

For Electron applications, Electron Main owns broker process lifecycle and update activation. Renderer code never connects directly to the broker. The application backend or Electron Main uses a language client according to the application's own architecture.

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

Every lease includes an incrementing generation. Requests with a stale generation fail before reaching the adapter. Queues are bounded and each hardware-class interface documents overflow behavior.

HAL lease expiry performs transport cleanup only. Domain-safe physical behavior stays above HAL.

### 5.4 Errors

All errors carry stable structure:

```rust
pub struct HalError {
    pub name: ErrorName,
    pub category: ErrorCategory,
    pub operation: OperationName,
    pub retryable: bool,
    pub resource_id: Option<ResourceId>,
    pub platform_code: Option<String>,
    pub vendor_code: Option<String>,
    pub debug_message: String,
    pub context: ErrorContext,
}
```

Callers make decisions from `name`, `category`, and `retryable`; they never parse `debug_message`.

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

## 10. Observability

The library emits structured tracing spans and metrics but never installs a subscriber. Required dimensions include adapter, transport kind, operation, normalized result, duration, queue depth, and retry count. Resource serial numbers and raw payloads are excluded from logs by default.

## 11. Delivery sequence

1. **v0.1:** core, runtime, broker, Rust/Python clients, virtual adapter, Serial.
2. **v0.2:** CAN/CAN FD, SocketCAN, and PCAN.
3. **v0.3:** USB and GPIO, with GPIO-through-USB adapters supported on desktop platforms.
4. **v0.4:** Camera control plus shared-memory frame data plane.
5. **v1.0:** stabilized interfaces, compatibility guarantees, and cross-platform conformance qualification.

Each version is a working vertical slice. Later modules may refine core only through backward-compatible additions or an explicit contract revision.

## 12. Decisions

- The asset is independent of its first consumer.
- The HAL is business-independent.
- The implementation is library-first.
- The broker exposes, but does not duplicate, library semantics.
- Hardware-class interfaces are typed and separate.
- Physical identity and endpoint are distinct.
- Camera is in target scope but not v0.1.
- v0.1 begins with Serial and a virtual adapter.
