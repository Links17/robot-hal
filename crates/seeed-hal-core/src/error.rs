use serde::{Deserialize, Serialize};

use crate::capability::validate_identifier;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Deserialize, Serialize)]
pub enum ErrorCategory {
    InvalidArgument,
    NotFound,
    Conflict,
    Unavailable,
    Internal,
}

#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Deserialize, Serialize)]
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

#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Deserialize, Serialize)]
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

#[derive(Clone, Debug, Eq, PartialEq, Deserialize)]
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

    pub fn invalid_argument(
        name: impl Into<String>,
        operation: impl Into<String>,
        debug_message: impl Into<String>,
    ) -> Self {
        Self::new(
            name,
            ErrorCategory::InvalidArgument,
            operation,
            false,
            debug_message,
        )
        .expect("static invalid argument error metadata must be valid")
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

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
pub struct ErrorDecisionFields<'a> {
    pub name: &'a ErrorName,
    pub category: ErrorCategory,
    pub operation: &'a OperationName,
    pub retryable: bool,
}

pub type HalResult<T> = Result<T, HalError>;
