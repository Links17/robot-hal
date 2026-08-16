use std::collections::BTreeMap;

use serde::de::{Error as DeError, Visitor};
use serde::{Deserialize, Deserializer, Serialize};
use std::fmt;

use crate::capability::validate_identifier;
use crate::{CapabilityId, CapabilitySet, ErrorCategory, HalError, HalResult};

#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
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

impl<'de> Deserialize<'de> for ResourceId {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct ResourceIdVisitor;

        impl Visitor<'_> for ResourceIdVisitor {
            type Value = ResourceId;

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("a validated resource identifier string")
            }

            fn visit_str<E>(self, value: &str) -> Result<Self::Value, E>
            where
                E: DeError,
            {
                ResourceId::parse(value.to_owned()).map_err(E::custom)
            }

            fn visit_string<E>(self, value: String) -> Result<Self::Value, E>
            where
                E: DeError,
            {
                ResourceId::parse(value).map_err(E::custom)
            }
        }

        deserializer.deserialize_str(ResourceIdVisitor)
    }
}

#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
pub struct Endpoint(String);

impl Endpoint {
    pub fn new(value: impl Into<String>) -> HalResult<Self> {
        let value = value.into();
        if value.is_empty() {
            return Err(HalError::invalid_argument_error(
                "endpoint.empty",
                "endpoint.new",
                "endpoint must not be empty",
            ));
        }

        if value.len() > 4096 {
            return Err(HalError::invalid_argument_error(
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

impl<'de> Deserialize<'de> for Endpoint {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct EndpointVisitor;

        impl Visitor<'_> for EndpointVisitor {
            type Value = Endpoint;

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("a validated endpoint string")
            }

            fn visit_str<E>(self, value: &str) -> Result<Self::Value, E>
            where
                E: DeError,
            {
                Endpoint::new(value.to_owned()).map_err(E::custom)
            }

            fn visit_string<E>(self, value: String) -> Result<Self::Value, E>
            where
                E: DeError,
            {
                Endpoint::new(value).map_err(E::custom)
            }
        }

        deserializer.deserialize_str(EndpointVisitor)
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Deserialize, Serialize)]
pub enum IdentityQuality {
    Weak,
    Medium,
    Strong,
}

impl IdentityQuality {
    /// Returns whether this descriptor quality satisfies a caller's minimum.
    ///
    /// The stable ordering is `Weak < Medium < Strong`.
    pub const fn satisfies(self, minimum: Self) -> bool {
        self.rank() >= minimum.rank()
    }

    const fn rank(self) -> u8 {
        match self {
            Self::Weak => 0,
            Self::Medium => 1,
            Self::Strong => 2,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Deserialize, Serialize)]
pub enum TransportKind {
    Serial,
    Can,
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

    pub fn iter(&self) -> impl Iterator<Item = (&str, &str)> {
        self.0
            .iter()
            .map(|(key, value)| (key.as_str(), value.as_str()))
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Deserialize, Serialize)]
pub struct ResourceDescriptor(
    ResourceId,
    Endpoint,
    IdentityQuality,
    TransportKind,
    ResourceProperties,
    CapabilitySet,
);

impl ResourceDescriptor {
    pub fn new(
        id: ResourceId,
        endpoint: Endpoint,
        minimum_identity_quality: IdentityQuality,
        transport: TransportKind,
        properties: ResourceProperties,
        capabilities: CapabilitySet,
    ) -> Self {
        Self(
            id,
            endpoint,
            minimum_identity_quality,
            transport,
            properties,
            capabilities,
        )
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

    pub fn capabilities(&self) -> &CapabilitySet {
        &self.5
    }

    pub fn selector(&self) -> ResourceSelector {
        ResourceSelector::exact(self.0.clone(), self.2, self.3)
    }
}

/// Resolves exactly one descriptor using stable identity and capability rules.
///
/// Endpoints are deliberately not part of matching: they are transient access
/// paths and cannot disambiguate duplicate persisted identities.
pub fn resolve_resource<'a>(
    descriptors: &'a [ResourceDescriptor],
    selector: &ResourceSelector,
    required_capability: &CapabilityId,
    operation: &'static str,
) -> HalResult<&'a ResourceDescriptor> {
    let mut matches = descriptors.iter().filter(|descriptor| {
        descriptor.id() == selector.id()
            && descriptor.transport() == selector.transport()
            && descriptor
                .minimum_identity_quality()
                .satisfies(selector.minimum_identity_quality())
            && descriptor.capabilities().contains(required_capability)
    });
    let Some(selected) = matches.next() else {
        return Err(HalError::new(
            "runtime.resource.not_found",
            ErrorCategory::NotFound,
            operation,
            false,
            "resource selector did not match an enumerated descriptor",
        )?
        .with_resource_id(selector.id().clone()));
    };
    if matches.next().is_some() {
        return Err(HalError::new(
            "runtime.resource.ambiguous",
            ErrorCategory::Conflict,
            operation,
            false,
            "resource selector matched more than one enumerated descriptor",
        )?
        .with_resource_id(selector.id().clone()));
    }
    Ok(selected)
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
