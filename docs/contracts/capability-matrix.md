# Hardware Capability Matrix

This document is the review and qualification index for standard hardware-class
capabilities. It describes transport-level behavior only; it does not encode
device protocols, product roles, or application workflows.

## Contract columns

- **Capability**: stable namespaced capability identifier.
- **Wire minor**: minimum broker protocol minor required for the operation.
- **Prerequisite**: lifecycle or capability dependency that must be present.
- **Bound**: public payload or queue limit.
- **Backpressure**: documented overflow behavior.
- **Virtual evidence**: hardware-free conformance coverage.
- **Native status**: target-specific qualification status; compile checks do not
  imply runtime or physical qualification.

## Matrix

| Hardware class | Capability | Wire minor | Prerequisite | Bound | Backpressure | Virtual evidence | Native status |
| --- | --- | ---: | --- | --- | --- | --- | --- |
| Serial | `serial.bytes/v1` | 0 | Serial session | Adapter-defined bounded write queue; broker read/write limits negotiated | `runtime.queue.full` on admission overflow | Serial adapter and broker minor 0 conformance | Native qualification remains platform-specific |
| CAN | `can.classic/v1` | 1 | CAN session in Classic mode | Classic data payload at most 8 bytes; batch at most 64 frames | Bounded management/TX/RX queues; oldest RX frames may be dropped with lag | Virtual CAN classic, filtering, status, timestamps where advertised | SocketCAN / PCAN physical and native-host gates remain external |
| CAN | `can.fd/v1` | 1 | CAN session in FD mode | FD data payload at most 64 bytes; batch at most 64 frames | Same bounded CAN actor contract | Virtual CAN FD lifecycle and frame checks | Physical CAN FD qualification remains external |
| USB | `usb.control/v1` | 2 | Claimed USB interface | Transfer payload at most 16 KiB | At most 64 pending transfers; overflow returns `runtime.queue.full` | Virtual USB control transfer conformance | Native USB qualification remains external |
| USB | `usb.bulk/v1` | 2 | `usb.control/v1` and claimed interface | Transfer payload at most 16 KiB | Bounded per-interface worker queue | Virtual USB bulk conformance | Native USB qualification remains external |
| USB | `usb.interrupt/v1` | 2 | `usb.control/v1` and claimed interface | Transfer payload at most 16 KiB | Bounded per-interface worker queue | Virtual USB interrupt conformance | Native USB qualification remains external |
| GPIO | `gpio.lines/v1` | 2 | GPIO line-group session | Line values are typed and bounded by requested line set | Bounded line-group worker queue | Virtual GPIO line read/write conformance | Linux and Windows native qualification remain external |
| GPIO | `gpio.edges/v1` | 2 | `gpio.lines/v1` and edge request | Edge capacity from 1 through 1,024 | Oldest events drop; lag is reported structurally | Virtual GPIO edge and lag conformance | Timestamp and edge support are platform-specific |
| Camera | `camera.capture/v1` | 3 | Exclusive camera session and negotiated format | Dimensions at most 4096 × 2160; frame at most 24 MiB | Camera command queue is bounded; native close timeout quarantines claim | Virtual capture, exclusive open, stale-generation checks | AVFoundation Partial; V4L2 Blocked; Media Foundation Pending |
| Camera | `camera.frames.shm/v1` | 3 | `camera.capture/v1` and mapping descriptor | Four to eight ring slots; frame bytes stay out of protobuf | Latest-wins ring with pinned-slot protection and drop count | Virtual shared-memory descriptor, lease, pin/drop checks | Broker mapping and native frame qualification remain external |
| Camera | `camera.controls/v1` | 3 | Camera session and adapter-advertised controls | Typed control values and descriptors | Control requests use bounded camera command queue | Virtual control get/set/auto checks | Native control support must be qualified per adapter |

## Evidence interpretation

The following evidence classes are intentionally independent:

1. **Virtual conformance** proves the public Interface against a deterministic
   fixture.
2. **Broker black-box conformance** proves that the deployment Adapter preserves
   the library and wire contracts.
3. **Hosted platform conformance** proves that the selected production broker
   builds and executes on the target host.
4. **Native qualification** proves target-specific OS behavior and physical
   hardware observations using the applicable runbook.

A row must not be marked natively qualified from a cross-compile, a virtual
adapter, an ignored test, or a successful hardware-free release gate.

## Maintenance rule

When adding a hardware-class capability, update this matrix together with:

- the hardware-class Interface;
- protocol conversion and broker gate;
- Rust and Python clients;
- virtual adapter conformance;
- the protocol minor matrix when IPC is involved;
- the target-native runbook and qualification record.

## Qualification entry points

The matrix is a contract index, not a claim that every native row is qualified. Use the following
evidence paths:

- **Virtual evidence**: run the applicable `robot-hal-testkit` conformance test and the broker
  virtual-adapter conformance command for the advertised protocol minors.
- **Hosted evidence**: run the release target's production broker build, manifest verification,
  and virtual conformance on the target host.
- **Native evidence**: execute the target adapter runbook with physical hardware and record only
  the redacted resource identity, negotiated capabilities, observed bounds, and structured errors.
- **AVFoundation camera**: use
  [`camera-avfoundation-native.md`](../runbooks/camera-avfoundation-native.md), including the
  supervised hot-unplug entry point
  [`scripts/run-avfoundation-hot-unplug.sh`](../../scripts/run-avfoundation-hot-unplug.sh).

Cross-compilation, an ignored test that did not run, or a successful virtual adapter run must not
promote `Native status` to qualified.
