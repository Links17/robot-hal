use serde::{Deserialize, Serialize};

use crate::{HalError, HalResult};

#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Deserialize, Serialize)]
pub struct CapabilityId(String);

impl CapabilityId {
    pub fn parse(value: impl Into<String>) -> HalResult<Self> {
        let value = value.into();
        validate_identifier("capability.id", &value)?;
        let (contract, version) = value.split_once('/').ok_or_else(|| {
            HalError::invalid_argument("capability.id.invalid", "capability.parse", "missing version")
        })?;
        let (namespace, name) = contract.split_once('.').ok_or_else(|| {
            HalError::invalid_argument("capability.id.invalid", "capability.parse", "missing namespace")
        })?;

        if namespace.is_empty()
            || name.is_empty()
            || contract.matches('.').count() != 1
            || !version.starts_with('v')
        {
            return Err(HalError::invalid_argument(
                "capability.id.invalid",
                "capability.parse",
                "expected <namespace>.<name>/v<positive integer>",
            ));
        }

        let number = &version[1..];
        if number.is_empty()
            || !number.chars().all(|character| character.is_ascii_digit())
            || number.parse::<u64>().ok().filter(|number| *number > 0).is_none()
        {
            return Err(HalError::invalid_argument(
                "capability.id.invalid",
                "capability.parse",
                "version must be a positive integer",
            ));
        }

        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Deserialize, Serialize)]
pub struct CapabilitySet(Vec<CapabilityId>);

impl CapabilitySet {
    pub fn new(capabilities: Vec<CapabilityId>) -> Self {
        Self(capabilities)
    }

    pub fn as_slice(&self) -> &[CapabilityId] {
        &self.0
    }
}

pub(crate) fn validate_identifier(field: &'static str, value: &str) -> HalResult<()> {
    if value.is_empty() {
        return Err(HalError::invalid_argument(
            format!("{field}.empty"),
            field,
            "identifier must not be empty",
        ));
    }

    if value.len() > 255 {
        return Err(HalError::invalid_argument(
            format!("{field}.too_long"),
            field,
            "identifier must be at most 255 bytes",
        ));
    }

    if !value.is_ascii() {
        return Err(HalError::invalid_argument(
            format!("{field}.non_ascii"),
            field,
            "identifier must be ASCII",
        ));
    }

    Ok(())
}
