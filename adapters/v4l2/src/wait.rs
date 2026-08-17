use std::time::{Duration, Instant};

#[derive(Debug, PartialEq, Eq)]
#[cfg_attr(not(any(target_os = "linux", test)), allow(dead_code))]
pub(crate) enum WaitResult<T> {
    Ready(T),
    TimedOut,
    Shutdown,
}

#[cfg_attr(not(any(target_os = "linux", test)), allow(dead_code))]
pub(crate) fn bounded_wait<T, E>(
    deadline: Instant,
    maximum_wait: Duration,
    mut shutdown_requested: impl FnMut() -> bool,
    mut wait_once: impl FnMut(Duration) -> Result<T, E>,
    is_timeout: impl Fn(&E) -> bool,
) -> Result<WaitResult<T>, E> {
    loop {
        if shutdown_requested() {
            return Ok(WaitResult::Shutdown);
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Ok(WaitResult::TimedOut);
        }
        match wait_once(remaining.min(maximum_wait)) {
            Ok(value) => return Ok(WaitResult::Ready(value)),
            Err(error) if is_timeout(&error) => continue,
            Err(error) => return Err(error),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{WaitResult, bounded_wait};
    use std::{
        cell::Cell,
        time::{Duration, Instant},
    };

    #[derive(Debug, PartialEq, Eq)]
    enum WaitError {
        TimedOut,
    }

    #[test]
    fn capture_deadline_bounds_each_native_wait() {
        let deadline = Instant::now() + Duration::from_millis(7);
        let result = bounded_wait(
            deadline,
            Duration::from_millis(2),
            || false,
            |wait_for| -> Result<(), WaitError> {
                assert!(
                    wait_for <= Duration::from_millis(7),
                    "native wait must never exceed the capture deadline"
                );
                Err(WaitError::TimedOut)
            },
            |error| matches!(error, WaitError::TimedOut),
        );

        assert_eq!(result, Ok(WaitResult::<()>::TimedOut));
    }

    #[test]
    fn shutdown_preempts_a_stalled_capture_without_waiting_for_its_deadline() {
        let waits = Cell::new(0);
        let result = bounded_wait(
            Instant::now() + Duration::from_secs(30),
            Duration::from_millis(2),
            || waits.get() == 1,
            |_| -> Result<(), WaitError> {
                waits.set(waits.get() + 1);
                Err(WaitError::TimedOut)
            },
            |error| matches!(error, WaitError::TimedOut),
        );

        assert_eq!(waits.get(), 1);
        assert_eq!(result, Ok(WaitResult::<()>::Shutdown));
    }

    #[test]
    fn a_timed_out_capture_does_not_poison_the_next_capture() {
        let first = bounded_wait(
            Instant::now(),
            Duration::from_millis(2),
            || false,
            |_| -> Result<(), WaitError> { Err(WaitError::TimedOut) },
            |error| matches!(error, WaitError::TimedOut),
        );
        assert_eq!(first, Ok(WaitResult::<()>::TimedOut));

        let second = bounded_wait(
            Instant::now() + Duration::from_secs(1),
            Duration::from_millis(2),
            || false,
            |_| -> Result<u8, WaitError> { Ok(42) },
            |error| matches!(error, WaitError::TimedOut),
        );
        assert_eq!(second, Ok(WaitResult::Ready(42)));
    }
}
