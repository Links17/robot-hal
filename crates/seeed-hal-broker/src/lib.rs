#![forbid(unsafe_code)]

mod connection;
pub mod listener;

use std::io;

pub use connection::{BrokerConfig, ConnectionOutcome};
use seeed_hal_runtime::HalRuntime;

#[derive(Clone, Eq, PartialEq)]
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
