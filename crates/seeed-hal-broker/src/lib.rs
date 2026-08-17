#![forbid(unsafe_code)]

mod can_dispatch;
mod connection;
pub mod listener;

use std::io;
use subtle::ConstantTimeEq;
use zeroize::{Zeroize, ZeroizeOnDrop};

pub use connection::{BrokerConfig, ConnectionOutcome};
use seeed_hal_runtime::HalRuntime;

#[derive(Clone, Zeroize, ZeroizeOnDrop)]
pub struct StartupToken([u8; 32]);

impl StartupToken {
    pub fn generate() -> io::Result<Self> {
        let mut bytes = [0_u8; 32];
        getrandom::fill(&mut bytes).map_err(|error| {
            io::Error::other(format!("startup token generation failed: {error}"))
        })?;
        Ok(Self(bytes))
    }

    pub const fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    pub fn expose_bytes(&self) -> &[u8; 32] {
        &self.0
    }

    pub fn authenticates(&self, presented: &[u8]) -> bool {
        presented.len() == self.0.len() && bool::from(presented.ct_eq(self.0.as_slice()))
    }
}

#[derive(Clone)]
pub struct Broker {
    runtime: HalRuntime,
    startup_token: StartupToken,
    config: BrokerConfig,
}

impl Broker {
    pub fn new(runtime: HalRuntime) -> io::Result<Self> {
        Ok(Self::with_startup_token(runtime, StartupToken::generate()?))
    }

    pub fn with_startup_token(runtime: HalRuntime, startup_token: StartupToken) -> Self {
        Self {
            runtime,
            startup_token,
            config: BrokerConfig::default(),
        }
    }

    pub fn with_config(
        runtime: HalRuntime,
        startup_token: StartupToken,
        config: BrokerConfig,
    ) -> Self {
        Self {
            runtime,
            startup_token,
            config,
        }
    }

    pub fn startup_token(&self) -> &StartupToken {
        &self.startup_token
    }
}

#[cfg(test)]
mod tests {
    use zeroize::Zeroize;

    use super::StartupToken;

    #[test]
    fn startup_token_zeroize_clears_owned_bytes() {
        let mut token = StartupToken::from_bytes([0xa5; 32]);
        token.zeroize();
        assert!(token.expose_bytes().iter().all(|byte| *byte == 0));
    }

    #[test]
    fn startup_token_authentication_uses_the_explicit_secret_comparison() {
        let token = StartupToken::from_bytes([0xa5; 32]);

        assert!(token.authenticates(&[0xa5; 32]));
        assert!(!token.authenticates(&[0xa4; 32]));
        assert!(!token.authenticates(&[0xa5; 31]));
    }
}
