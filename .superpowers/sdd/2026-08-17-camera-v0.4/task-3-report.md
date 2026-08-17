# Task 3 report: named shared-memory frame ring

## Delivered

- Added the workspace crate `seeed-hal-adapter-shared-memory`, the sole new crate containing
  mapping and atomic unsafe code. Runtime, broker, protocol, client, Camera core, and native
  camera adapters were not changed.
- The public safe API creates broker-owned named mappings, opens read-only mappings through a
  descriptor carrying a distinct capability token, validates a fixed versioned header, publishes
  bounded frame slots, acquires the latest committed broker lease, and exposes a lifetime-bounded
  read view.
- The mapping stores only SHA-256 capability-token material, never the token. Descriptor debug
  output redacts the capability; neither descriptor nor token implements `Display`.
- The broker owns the single active pin. `next_frame_lease` releases the preceding pin before
  selecting and pinning the newest slot; read-only mappings cannot mutate shared slot state.
  Writers choose a free slot or oldest unpinned slot, otherwise drop without blocking and advance
  the monotonic session drop counter.

## Layout and validation

- Header magic/version, total length, slot count, 64-byte-aligned stride, negotiated format and
  dimensions, capacity, 256-bit session identity, and token hash are validated fail-closed.
- Slot metadata includes state, sequence, generation, timestamp, dropped count, payload length,
  plane count/layout, and payload. All layout arithmetic, dimensions, plane ranges, overlap,
  payload bounds, and total-map limits are checked before safe views are exposed.
- Writer payload/metadata writes precede release sequence/state publication; reader observes
  acquire state/sequence, validates metadata and repeats sequence/generation checks before
  returning a view.

## Unsafe boundary and SAFETY evidence

- `platform.rs`: POSIX `shm_open`, `ftruncate`, `mmap`, `munmap`, `close`, and `shm_unlink` are
  each adjacent to `SAFETY` invariants. macOS independent reopen testing exercises create,
  writable broker map, separate read-only client map, and teardown.
- `platform.rs`: Windows `CreateFileMappingW`, `MapViewOfFile`, `OpenFileMappingW`,
  `UnmapViewOfFile`, and `CloseHandle` are adjacent to lifetime/ownership `SAFETY` invariants.
  Creation derives a protected DACL granting current user, LocalSystem, and Administrators.
  The Windows target compiles; its DACL behavior still requires OS qualification.
- `ring.rs`: raw fixed-layout reads/writes, atomic field casts, payload slice creation, and
  endian helpers are adjacent to layout, alignment, publication, and pinning invariants. Unit
  tests cover malformed header, capacity/overflow, bad token, escaped plane, generation mismatch,
  torn state, latest-wins replacement, pin/drop behavior, and independent reopen.

## Verification

- RED: `cargo test -p seeed-hal-adapter-shared-memory` initially failed because `layout`,
  `platform`, and `ring` modules did not exist.
- GREEN on macOS: `cargo test -p seeed-hal-adapter-shared-memory` — 7 unit tests passed.
- `cargo clippy -p seeed-hal-adapter-shared-memory --all-targets --all-features -- -D warnings`
  — passed.
- `cargo fmt --all --check` and `git diff --check` — passed.
- `cargo check -p seeed-hal-adapter-shared-memory --target x86_64-pc-windows-gnu` — passed after
  installing that Rust target.

## Platform limits and follow-up gates

- macOS uses POSIX shared memory with `O_CREAT | O_EXCL | O_RDWR` and `0600`; macOS constrains
  POSIX shm names, so the name uses 64 random bits while the required session identity and
  independently generated capability token remain 256 bits. The mapping name is not the security
  credential.
- Linux uses the same POSIX path but was not executed in this macOS environment.
- Windows creation and protected DACL path compile but lack Windows runtime/DACL inspection
  validation. This remains a release-qualification gate.
- No broker protobuf, runtime/client integration, or native camera adapter was added. The later
  control-plane task must transport descriptor/token and broker lease operations; clients remain
  strictly read-only.
