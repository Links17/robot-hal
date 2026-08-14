use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::capability::validate_identifier;
use crate::HalResult;

#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Deserialize, Serialize)]
pub struct LeaseId(String);

impl LeaseId {
    pub fn new() -> Self {
        Self(Uuid::new_v4().to_string())
    }

    pub fn parse(value: impl Into<String>) -> HalResult<Self> {
        let value = value.into();
        validate_identifier("lease.id", &value)?;
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl Default for LeaseId {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Deserialize, Serialize)]
pub struct OwnerId(String);

impl OwnerId {
    pub fn parse(value: impl Into<String>) -> HalResult<Self> {
        let value = value.into();
        validate_identifier("owner.id", &value)?;
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Deserialize, Serialize)]
pub struct SessionId(String);

impl SessionId {
    pub fn parse(value: impl Into<String>) -> HalResult<Self> {
        let value = value.into();
        validate_identifier("session.id", &value)?;
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Deserialize, Serialize)]
pub enum LeaseMode {
    Observe,
    Control,
}

#[derive(Clone, Debug, Eq, PartialEq, Deserialize, Serialize)]
pub struct LeaseToken(LeaseId, u64, LeaseMode);

impl LeaseToken {
    pub fn new(lease_id: LeaseId, generation: u64, mode: LeaseMode) -> Self {
        Self(lease_id, generation, mode)
    }

    pub fn new_for_test(generation: u64, mode: LeaseMode) -> Self {
        Self::new(LeaseId::new(), generation, mode)
    }

    pub fn lease_id(&self) -> &LeaseId {
        &self.0
    }

    pub fn generation(&self) -> u64 {
        self.1
    }

    pub fn mode(&self) -> LeaseMode {
        self.2
    }
}
