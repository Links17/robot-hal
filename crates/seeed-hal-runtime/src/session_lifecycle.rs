use seeed_hal_core::{ErrorCategory, HalError, HalResult};

use crate::runtime_error;

/// Shared lifecycle phases for Serial and USB exclusive sessions.
///
/// Hardware-specific managers retain ownership of their workers and queues.
/// This kernel defines the smallest state transitions shared by those managers;
/// Camera, GPIO, and CAN retain their own terminal, reap, and native-close
/// bookkeeping because their concurrency semantics are not identical.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum SessionLifecycle {
    Opening,
    Active,
    Closing,
}

impl SessionLifecycle {
    pub(crate) const fn is_active(self) -> bool {
        matches!(self, Self::Active)
    }

    pub(crate) const fn is_closing(self) -> bool {
        matches!(self, Self::Closing)
    }

    pub(crate) fn commit_open(self, operation: &'static str) -> HalResult<Self> {
        if self != Self::Opening {
            return Err(invalid_transition(
                operation,
                self,
                Self::Active,
                "only an opening session can become active",
            ));
        }
        Ok(Self::Active)
    }

    pub(crate) fn begin_close(self, _operation: &'static str) -> HalResult<Self> {
        match self {
            Self::Opening | Self::Active => Ok(Self::Closing),
            Self::Closing => Ok(Self::Closing),
        }
    }

    pub(crate) fn admit_io(self, operation: &'static str) -> HalResult<()> {
        if self != Self::Active {
            return Err(runtime_error(
                "runtime.session.closed",
                ErrorCategory::Conflict,
                operation,
                false,
                "the session is not active and cannot admit new I/O",
            ));
        }
        Ok(())
    }
}

fn invalid_transition(
    operation: &'static str,
    actual: SessionLifecycle,
    expected: SessionLifecycle,
    message: &'static str,
) -> HalError {
    runtime_error(
        "runtime.session.lifecycle.invalid_transition",
        ErrorCategory::Conflict,
        operation,
        false,
        format!("session lifecycle is {actual:?}, expected {expected:?}: {message}"),
    )
}

#[cfg(test)]
mod tests {
    use super::SessionLifecycle;

    #[test]
    fn exclusive_session_follows_open_active_close_closed_order() {
        let state = SessionLifecycle::Opening;
        let state = state.commit_open("test.open").expect("opening commits");
        let state = state.begin_close("test.close").expect("active closes");
        assert!(state.is_closing());
    }

    #[test]
    fn opening_can_be_cancelled_into_closing_without_admitting_io() {
        let state = SessionLifecycle::Opening
            .begin_close("test.revoke")
            .expect("revocation can close an opening session");
        assert!(state.is_closing());
        let error = state
            .admit_io("test.read")
            .expect_err("closing rejects I/O");
        assert_eq!(error.name().as_str(), "runtime.session.closed");
    }

    #[test]
    fn active_sessions_cannot_commit_open_twice() {
        let error = SessionLifecycle::Active
            .commit_open("test.open")
            .expect_err("active sessions cannot commit open twice");
        assert_eq!(
            error.name().as_str(),
            "runtime.session.lifecycle.invalid_transition"
        );
    }
}
