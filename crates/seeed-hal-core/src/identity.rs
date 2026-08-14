use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::capability::validate_identifier;
use crate::{HalError, HalResult};

#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Deserialize, Serialize)]
pub struct ResourceId(String);

impl ResourceId {
    pub fn parse(value: impl Into<String>) -> HalResult<Self> {
        let value = value.into();
        validate_identifier("resource.id", &value)?;
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Deserialize, Serialize)]
pub struct Endpoint(String);

impl Endpoint {
    pub fn new(value: impl Into<String>) -> HalResult<Self> {
        let value = value.into();
        if value.is_empty() {
            return Err(HalError::invalid_argument(
                "endpoint.empty",
                "endpoint.new",
                "endpoint must not be empty",
            ));
        }

        if value.len() > 4096 {
            return Err(HalError::invalid_argument(
                "endpoint.too_long",
                "endpoint.new",
                "endpoint must be at most 4096 bytes",
            ));
        }

        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Deserialize, Serialize)]
pub enum IdentityQuality {
    Weak,
    Medium,
    Strong,
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Deserialize, Serialize)]
pub enum TransportKind {
    Serial,
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Deserialize, Serialize)]
pub struct ResourceProperties(BTreeMap<String, String>);

impl ResourceProperties {
    pub fn new(properties: BTreeMap<String, String>) -> Self {
        Self(properties)
    }

    pub fn get(&self, key: &str) -> Option<&str> {
        self.0.get(key).map(String::as_str)
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Deserialize, Serialize)]
pub struct ResourceDescriptor(
    ResourceId,
    Endpoint,
    IdentityQuality,
    TransportKind,
    ResourceProperties,
);

impl ResourceDescriptor {
    pub fn new(
        id: ResourceId,
        endpoint: Endpoint,
        minimum_identity_quality: IdentityQuality,
        transport: TransportKind,
        properties: ResourceProperties,
    ) -> Self {
        Self(id, endpoint, minimum_identity_quality, transport, properties)
    }

    pub fn id(&self) -> &ResourceId {
        &self.0
    }

    pub fn endpoint(&self) -> &Endpoint {
        &self.1
    }

    pub fn minimum_identity_quality(&self) -> IdentityQuality {
        self.2
    }

    pub fn transport(&self) -> TransportKind {
        self.3
    }

    pub fn properties(&self) -> &ResourceProperties {
        &self.4
    }

    pub fn selector(&self) -> ResourceSelector {
        ResourceSelector::exact(self.0.clone(), self.2, self.3)
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Deserialize, Serialize)]
pub struct ResourceSelector(ResourceId, IdentityQuality, TransportKind);

impl ResourceSelector {
    pub fn exact(
        id: ResourceId,
        minimum_identity_quality: IdentityQuality,
        transport: TransportKind,
    ) -> Self {
        Self(id, minimum_identity_quality, transport)
    }

    pub fn id(&self) -> &ResourceId {
        &self.0
    }

    pub fn minimum_identity_quality(&self) -> IdentityQuality {
        self.1
    }

    pub fn transport(&self) -> TransportKind {
        self.2
    }
}
