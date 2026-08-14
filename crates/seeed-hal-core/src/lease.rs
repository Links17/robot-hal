use serde::de::{Error as DeError, Visitor};
use serde::{Deserialize, Deserializer, Serialize};
use std::fmt;
use uuid::Uuid;

use crate::HalResult;
use crate::capability::validate_identifier;

#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
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

impl<'de> Deserialize<'de> for LeaseId {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct LeaseIdVisitor;

        impl Visitor<'_> for LeaseIdVisitor {
            type Value = LeaseId;

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("a validated lease id string")
            }

            fn visit_str<E>(self, value: &str) -> Result<Self::Value, E>
            where
                E: DeError,
            {
                LeaseId::parse(value.to_owned()).map_err(E::custom)
            }

            fn visit_string<E>(self, value: String) -> Result<Self::Value, E>
            where
                E: DeError,
            {
                LeaseId::parse(value).map_err(E::custom)
            }
        }

        deserializer.deserialize_str(LeaseIdVisitor)
    }
}

#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
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

impl<'de> Deserialize<'de> for OwnerId {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct OwnerIdVisitor;

        impl Visitor<'_> for OwnerIdVisitor {
            type Value = OwnerId;

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("a validated owner id string")
            }

            fn visit_str<E>(self, value: &str) -> Result<Self::Value, E>
            where
                E: DeError,
            {
                OwnerId::parse(value.to_owned()).map_err(E::custom)
            }

            fn visit_string<E>(self, value: String) -> Result<Self::Value, E>
            where
                E: DeError,
            {
                OwnerId::parse(value).map_err(E::custom)
            }
        }

        deserializer.deserialize_str(OwnerIdVisitor)
    }
}

#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
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

impl<'de> Deserialize<'de> for SessionId {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct SessionIdVisitor;

        impl Visitor<'_> for SessionIdVisitor {
            type Value = SessionId;

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("a validated session id string")
            }

            fn visit_str<E>(self, value: &str) -> Result<Self::Value, E>
            where
                E: DeError,
            {
                SessionId::parse(value.to_owned()).map_err(E::custom)
            }

            fn visit_string<E>(self, value: String) -> Result<Self::Value, E>
            where
                E: DeError,
            {
                SessionId::parse(value).map_err(E::custom)
            }
        }

        deserializer.deserialize_str(SessionIdVisitor)
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
