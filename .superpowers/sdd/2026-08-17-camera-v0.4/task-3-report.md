# Task 3 report

## POSIX advisory lock recovery

- Replaced named POSIX semaphore coordination with non-blocking `flock` advisory locks on a broker-created private `0600` lock file paired with the mapping descriptor name.
- The broker writer continues to use `LOCK_EX | LOCK_NB`; contention immediately drops the frame. `FrameView` and `ReadOnlyMapping::copy` retain shared locks for their guarded read lifetimes.
- Removed `sem_open`, `sem_close`, `sem_unlink`, `sem_trywait`, and `sem_post`; no named semaphore object is created or left behind.
- Added a subprocess regression test: a child obtains an exclusive lock then calls `_exit`; its parent subsequently obtains the lock without blocking. BSD `flock(2)` releases advisory locks at process exit or final descriptor close.
- Existing fstat size, `0600`, identity/token, and header validation remain intact. Windows stays fail-closed and cross-compiles.

## Verification

- `cargo fmt --all --check`
- `cargo test -p seeed-hal-adapter-shared-memory`
- `cargo clippy -p seeed-hal-adapter-shared-memory --all-targets --all-features -- -D warnings`
- `cargo check -p seeed-hal-adapter-shared-memory --target x86_64-pc-windows-gnu`
