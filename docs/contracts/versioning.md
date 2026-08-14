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
