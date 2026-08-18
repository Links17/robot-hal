# Versioning and Compatibility Contract

## Versioned surfaces

Seeed HAL versions three related surfaces:

1. Rust crate interfaces;
2. broker executable releases;
3. local IPC wire contract.

All use SemVer 2.0.0. Their versions may advance independently, but every broker release declares the wire-contract major and supported minor range it provides.

## Rust crates

- Breaking public interface changes require a crate major release after `1.0.0`.
- Before `1.0.0`, a breaking change requires a minor release and release-note migration instructions.
- Platform adapter crates may release independently from core crates when compatibility ranges remain satisfied.

## Wire contract

- Every connection begins with version negotiation before another request is accepted.
- A client sends its supported major and inclusive minor range.
- The broker selects the highest mutually supported minor within the same major.
- The legacy `protocol_minor` field remains populated with the client's maximum supported minor.
  When both additive request range fields are zero, the request is interpreted as an exact offer of
  that legacy minor. A range-aware request must set the legacy field to its inclusive maximum.
- The response reports the selected minor and the broker's inclusive supported range. When both
  additive response range fields are zero, the response is interpreted as the selected legacy exact
  minor.
- A different major or no shared minor means `runtime.protocol.version_incompatible` and immediate
  connection closure.
- Changes within a major are additive: new messages, new optional fields, new enum values, and new capabilities.
- Field numbers are never reused, including removed fields.
- Required semantic changes, ordering changes, or changed defaults require a new major.
- Clients and brokers ignore unknown fields when doing so is safe and preserve request correlation.

### Structured errors in wire v1

The wire-v1 `Error` message retains its original fields without changing their meaning: `name = 1`,
`category = 2`, `operation = 3`, `retryable = 4`, and `debug_message = 5`. Structured diagnostics
are the following additive fields:

```protobuf
string resource_id = 6;
string platform_code = 7;
string vendor_code = 8;
map<string, string> context = 9;
```

Legacy errors with fields 6–9 absent remain valid. Empty optional strings decode as absent values,
and missing or empty context decodes as an empty `ErrorContext`. Non-empty details are validated by
the same core invariants as in-process errors; a malformed detail makes the containing error payload
an invalid protocol message and fails the connection closed.

Older peers may ignore the unknown fields 6–9, and newer peers tolerate their absence. No field
number is reused. Only `name`, `category`, `operation`, and `retryable` are stable decision fields;
`debug_message` and fields 6–9 are diagnostics and must not drive application decisions.

## Capability contracts

Capability identifiers carry their own contract version, for example `serial.bytes/v1` or `camera.frames/v1`. Adding a capability does not require a wire major change. Changing the meaning or invariant of an existing capability requires a new capability version.

## Broker manifest

Every broker artifact includes a machine-readable manifest containing:

- broker SemVer;
- wire major and supported minor range;
- target OS and architecture;
- enabled adapters and features;
- MSRV used for the build;
- artifact checksum;
- required vendor runtime libraries, when any.

For v0.2 the broker is `0.2.0` and supports wire major 1, inclusive minors `0..=1`. Its manifest
lists compiled adapters only; it does not claim that optional runtime libraries or physical devices
were available at startup. Startup diagnostics record an unavailable optional adapter using stable
structured error fields. `--require-adapter pcan` turns an unavailable or uncompiled PCAN adapter
into a startup failure before the endpoint is published.

The USB/GPIO vertical slice supports wire major 1, inclusive minors `0..=2`. Minor 2 adds only
optional USB Control/Bulk/Interrupt and GPIO line/edge operations and their hardware-class
capabilities. A peer negotiated below minor 2 rejects those operations locally or at broker
dispatch; the pre-existing Serial and CAN meanings remain unchanged. The manifest may list
independently compiled `nusb`, `linux-gpio`, or `windows-gpio` adapters, but that does not claim
their vendor runtime, controller, or physical hardware was available. `virtual-adapters` is
test-only and is not a production-device claim.

The Camera vertical slice supports wire major 1, inclusive minors `0..=3`. Minor 3 adds optional
Camera discovery, exclusive sessions, capture, mapping descriptors, frame leases, drop counts, and
standardized controls. Frame bytes never appear in the protobuf control plane: the broker conveys
only a validated shared-memory descriptor and access credential. A peer negotiated below minor 3
rejects every Camera entry point locally or at broker dispatch; it must not downgrade capture to a
payload-bearing protobuf response. The manifest may list an AVFoundation, V4L2, or Media Foundation
adapter without claiming the target runtime, privacy authorization, driver, or a physical camera was
available. `virtual-adapters` remains test-only evidence.

## Release artifact contract

The v0.5 RC release artifact names, target matrix, manifest schema, checksums,
conformance-report binding, immutable aggregation, and qualification rules are
defined by the [release artifact contract](release-artifacts.md). A release
manifest records the broker and Python release versions derived from its RC tag,
the wire range, and artifact checksums; it does not itself prove hosted
execution, attestation, or physical-hardware qualification. Those evidence
classes remain distinct in the [v0.5 RC qualification record](../releases/v0.5.0-rc-qualification.md).
