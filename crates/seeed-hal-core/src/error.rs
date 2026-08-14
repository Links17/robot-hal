use serde::de::{Error as DeError, Visitor};
use serde::{Deserialize, Deserializer, Serialize};
use std::fmt;

use crate::capability::validate_identifier;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Deserialize, Serialize)]
pub enum ErrorCategory {
    InvalidArgument,
    NotFound,
    Conflict,
    Unavailable,
    Internal,
}

#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
pub struct ErrorName(String);

impl ErrorName {
    pub fn parse(value: impl Into<String>) -> HalResult<Self> {
        let value = value.into();
        validate_identifier("error.name", &value)?;
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl<'de> Deserialize<'de> for ErrorName {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct ErrorNameVisitor;

        impl Visitor<'_> for ErrorNameVisitor {
            type Value = ErrorName;

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("a validated error name string")
            }

            fn visit_str<E>(self, value: &str) -> Result<Self::Value, E>
            where
                E: DeError,
            {
                ErrorName::parse(value.to_owned()).map_err(E::custom)
            }

            fn visit_string<E>(self, value: String) -> Result<Self::Value, E>
            where
                E: DeError,
            {
                ErrorName::parse(value).map_err(E::custom)
            }
        }

        deserializer.deserialize_str(ErrorNameVisitor)
    }
}

#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
pub struct OperationName(String);

impl OperationName {
    pub fn parse(value: impl Into<String>) -> HalResult<Self> {
        let value = value.into();
        validate_identifier("operation.name", &value)?;
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl<'de> Deserialize<'de> for OperationName {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct OperationNameVisitor;

        impl Visitor<'_> for OperationNameVisitor {
            type Value = OperationName;

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("a validated operation name string")
            }

            fn visit_str<E>(self, value: &str) -> Result<Self::Value, E>
            where
                E: DeError,
            {
                OperationName::parse(value.to_owned()).map_err(E::custom)
            }

            fn visit_string<E>(self, value: String) -> Result<Self::Value, E>
            where
                E: DeError,
            {
                OperationName::parse(value).map_err(E::custom)
            }
        }

        deserializer.deserialize_str(OperationNameVisitor)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HalError(ErrorName, ErrorCategory, OperationName, bool, String);

impl HalError {
    pub fn new(
        name: impl Into<String>,
        category: ErrorCategory,
        operation: impl Into<String>,
        retryable: bool,
        debug_message: impl Into<String>,
    ) -> HalResult<Self> {
        Ok(Self(
            ErrorName::parse(name)?,
            category,
            OperationName::parse(operation)?,
            retryable,
            debug_message.into(),
        ))
    }

    pub(crate) fn invalid_argument_error(
        name: impl Into<String>,
        operation: impl Into<String>,
        debug_message: impl Into<String>,
    ) -> Self {
        Self(
            ErrorName::parse(name).expect("static invalid argument error metadata must be valid"),
            ErrorCategory::InvalidArgument,
            OperationName::parse(operation)
                .expect("static invalid argument error metadata must be valid"),
            false,
            debug_message.into(),
        )
    }

    pub fn name(&self) -> &ErrorName {
        &self.0
    }

    pub fn category(&self) -> ErrorCategory {
        self.1
    }

    pub fn operation(&self) -> &OperationName {
        &self.2
    }

    pub fn retryable(&self) -> bool {
        self.3
    }

    pub fn debug_message(&self) -> &str {
        &self.4
    }

    pub fn decision_fields(&self) -> ErrorDecisionFields<'_> {
        ErrorDecisionFields {
            name: &self.0,
            category: self.1,
            operation: &self.2,
            retryable: self.3,
        }
    }
}

impl std::fmt::Display for HalError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "{} during {}: {}",
            self.0.as_str(),
            self.2.as_str(),
            self.4
        )
    }
}

impl std::error::Error for HalError {}

impl Serialize for HalError {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        self.decision_fields().serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for HalError {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        struct SerializedDecision {
            name: ErrorName,
            category: ErrorCategory,
            operation: OperationName,
            retryable: bool,
        }

        let decision = SerializedDecision::deserialize(deserializer)?;
        Ok(Self(
            decision.name,
            decision.category,
            decision.operation,
            decision.retryable,
            String::new(),
        ))
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
pub struct ErrorDecisionFields<'a> {
    pub name: &'a ErrorName,
    pub category: ErrorCategory,
    pub operation: &'a OperationName,
    pub retryable: bool,
}

pub type HalResult<T> = Result<T, HalError>;
