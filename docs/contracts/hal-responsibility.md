# HAL Responsibility Contract

## Promise

Seeed HAL is a cross-platform, business-independent hardware access module. It gives applications consistent interfaces for standard hardware transports and hardware classes without interpreting application or device-protocol meaning.

## Included responsibility

- enumerate resources and describe transport-level metadata;
- separate physical identity from the current platform endpoint;
- open, configure, use, cancel, and close hardware sessions;
- enforce local ownership through sessions, leases, and fencing generations;
- expose Serial, CAN/CAN FD, USB, GPIO, and Camera interfaces;
- provide bounded asynchronous I/O and explicit backpressure behavior;
- report hotplug and transport-level health changes;
- normalize platform and vendor transport errors into stable structured errors;
- expose the same semantics through an in-process Rust library and a local broker;
- supply Rust, Python, and optional Node clients for the broker contract.

## Excluded responsibility

- product workflows, UI state, or user-facing localization;
- device protocols layered on raw transport data;
- robots, motors, joints, sensors as product concepts, or device roles;
- calibration, teleoperation, recording, training, inference, and datasets;
- domain safety behavior such as hold-position, damped stop, homing, or torque disable;
- application persistence schemas and product business error numbers.

## Hardware-class rule

A standard hardware class belongs in HAL when its interface is meaningful without knowing the consuming product. Camera frame capture, CAN frames, serial bytes, USB transfers, and GPIO edges satisfy this rule.

A device-specific protocol does not belong in HAL when its interface requires knowledge of a model, command set, mechanical behavior, or product workflow.

## Cleanup guarantee

On cancellation, lease expiry, client loss, or broker shutdown, HAL guarantees only transport-level cleanup:

- reject stale or unauthorized operations;
- stop accepting new writes;
- cancel or drain queued operations according to the session contract;
- release platform resources and close handles;
- publish the resulting session and resource events.

HAL does not guarantee a domain-safe physical state. Consuming device drivers must implement domain safety before releasing their HAL session.

## Ownership guarantee

An in-process library session owns its platform handle. In broker deployment, the broker owns all platform handles and clients operate through opaque session identifiers. A resource with an active exclusive lease cannot be opened by another HAL session.

HAL cannot prevent unrelated external processes from bypassing it and opening the same OS resource; platform adapters should request OS exclusivity where the platform supports it and report the limitation otherwise.
