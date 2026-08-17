# Task 3 report: shared-memory frame-ring security remediation

## Delivered

- Reworked the adapter boundary only; runtime, broker, client, native camera, and plan files are
  unchanged.
- `BrokerMapping::acquire(&mut self) -> FrameView<'_>` now produces the only zero-copy view.
  `FrameView` holds the exclusive mutable broker borrow, so Rust rejects another acquire, writer,
  lease release, or broker operation until the view drops. The broker pin remains valid for that
  entire borrow.
- Independently reopened `ReadOnlyMapping` has no zero-copy API. `copy(&mut self, lease)` returns
  `CopiedFrame` with owned bytes. Python bindings must expose only this copying boundary (and must
  validate the broker-provided lease before copying); no foreign zero-copy buffer may escape.
- Removed every `&AtomicU64` cast and all atomic object references over mapping bytes. POSIX uses a
  named system semaphore for whole-operation exclusion. All mutable shared bytes are accessed only
  while its lock is held; the producer uses `sem_trywait`, dropping immediately on contention.
  This avoids Rust atomic-object creation and races on metadata or payload.
- Leases carry mapping identity, slot, sequence, and generation. Release verifies all fields and
  requires `Pinned` state. One `BrokerMapping` owns one pin; this is intentionally single-session
  / single-owner scope until the broker control plane implements ownership.

## Validation and platform behavior

- The fixed, versioned layout remains bounded (4–8 slots), latest-wins avoids pinned slots, and
  writers never wait for a reader. Header stores only the SHA-256 token hash.
- POSIX reopen executes `fstat` before mapping and rejects an object smaller than descriptor
  length; the header must still exactly agree with descriptor length. POSIX creation requests and
  verifies `0600` plus effective-user ownership. macOS rejects `fchmod` on POSIX SHM (`EINVAL`);
  its `shm_open(..., 0600)` creation mode and post-create ownership/mode verification are the
  supported equivalent.
- POSIX names use 72 random bits because Darwin's 30-character usable SHM-name limit prevents a
  256-bit name. The name is not an authorization secret: identity and token are each 256-bit, and
  `O_EXCL` rejects a collision.
- Windows create/open explicitly return structured `shared_memory.unavailable` until a qualified
  implementation performs post-create DACL and section/view-length verification. The Windows
  target compiles, but cannot claim protected operational support.
- Header validation computes identity and token-hash constant-time equality unconditionally and
  combines both results without short-circuiting. Tokens are redacted from debug output, lack
  `Display`, and zeroize when dropped; descriptor cloning creates another capability copy whose
  lifetime is explicitly bounded by its own drop.

## Unsafe audit

- `platform.rs`: POSIX `shm_open`, permissions/stat calls, mapping, named semaphore operations,
  unlink, unmap, close all have immediate `SAFETY` ownership/range contracts. The independent
  reopen test exercises separate mapping and semaphore handles.
- `ring.rs`: raw fixed-layout read/write helpers and payload slice construction have immediate
  layout, lock, pin, and bounded-range contracts. A `FrameView` payload is created only under the
  retained semaphore lock and broker pin; copied clients copy while locked.
- No raw atomic intrinsics, `AtomicU64` casts, or safe references to mapped atomic objects remain.

## RED / GREEN and verification

- RED: tests were first changed to require `BrokerMapping::acquire` and `ReadOnlyMapping::copy`;
  compilation failed because neither API existed.
- GREEN: `cargo test -p seeed-hal-adapter-shared-memory` — 8 unit tests passed on macOS, including
  independently reopened copy access and broker-pinned zero-copy acquisition.
- `cargo clippy -p seeed-hal-adapter-shared-memory --all-targets --all-features -- -D warnings`
  — passed.
- `cargo check -p seeed-hal-adapter-shared-memory --target x86_64-pc-windows-gnu` — passed.

## Remaining qualification gates

- A process crash while holding a POSIX semaphore can leave it unavailable; robust recovery needs
  a separately designed, broker-owned recovery protocol before production multi-process use.
- Windows is deliberately unavailable rather than providing unverified DACL/length behavior.
- Broker control-plane auto-release, session scoping across IPC, and Python binding integration
  remain later tasks and must preserve the copy-only foreign-language boundary.
