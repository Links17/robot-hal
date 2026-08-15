use serde::de::{Error as DeError, Visitor};
use serde::{Deserialize, Deserializer, Serialize};
use std::collections::BTreeMap;
use std::fmt;

use crate::capability::validate_identifier;
use crate::ResourceId;

const ERROR_CONTEXT_MAX_ENTRIES: usize = 16;
const ERROR_CONTEXT_MAX_KEY_BYTES: usize = 64;
const ERROR_CONTEXT_MAX_VALUE_BYTES: usize = 1024;
const ERROR_CONTEXT_MAX_TOTAL_BYTES: usize = 8192;

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

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ErrorContext(BTreeMap<String, String>);

impl ErrorContext {
    pub fn new<I, K, V>(entries: I) -> HalResult<Self>
    where
        I: IntoIterator<Item = (K, V)>,
        K: Into<String>,
        V: Into<String>,
    {
        let mut context = BTreeMap::new();
        let mut total_bytes = 0usize;

        for (key, value) in entries {
            let key = key.into();
            let value = value.into();

            if context.len() >= ERROR_CONTEXT_MAX_ENTRIES {
                return Err(HalError::invalid_argument_error(
                    "error.context.too_many_entries",
                    "error.context.new",
                    "context must contain at most 16 entries",
                ));
            }
            validate_context_key(&key)?;
            if value.len() > ERROR_CONTEXT_MAX_VALUE_BYTES {
                return Err(HalError::invalid_argument_error(
                    "error.context.value.too_long",
                    "error.context.new",
                    "context values must be at most 1024 bytes",
                ));
            }
            if context.contains_key(&key) {
                return Err(HalError::invalid_argument_error(
                    "error.context.duplicate_key",
                    "error.context.new",
                    "context keys must be unique",
                ));
            }
            let entry_bytes = key.len() + value.len();
            if total_bytes + entry_bytes > ERROR_CONTEXT_MAX_TOTAL_BYTES {
                return Err(HalError::invalid_argument_error(
                    "error.context.too_large",
                    "error.context.new",
                    "context key/value bytes must total at most 8192",
                ));
            }

            total_bytes += entry_bytes;
            context.insert(key, value);
        }

        Ok(Self(context))
    }

    pub fn iter(&self) -> impl Iterator<Item = (&str, &str)> {
        self.0
            .iter()
            .map(|(key, value)| (key.as_str(), value.as_str()))
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

fn validate_context_key(key: &str) -> HalResult<()> {
    if key.is_empty() {
        return Err(HalError::invalid_argument_error(
            "error.context.key.empty",
            "error.context.new",
            "context keys must not be empty",
        ));
    }
    if key.len() > ERROR_CONTEXT_MAX_KEY_BYTES {
        return Err(HalError::invalid_argument_error(
            "error.context.key.too_long",
            "error.context.new",
            "context keys must be at most 64 bytes",
        ));
    }
    if !key.is_ascii() {
        return Err(HalError::invalid_argument_error(
            "error.context.key.non_ascii",
            "error.context.new",
            "context keys must be ASCII",
        ));
    }
    let mut characters = key.chars();
    if !characters
        .next()
        .is_some_and(|character| character.is_ascii_lowercase())
        || !characters.all(|character| {
            character.is_ascii_alphanumeric() || matches!(character, '_' | '-')
        })
    {
        return Err(HalError::invalid_argument_error(
            "error.context.key.invalid",
            "error.context.new",
            "context keys must match [a-z][a-zA-Z0-9_-]*",
        ));
    }
    Ok(())
}

#[derive(Clone, Eq, PartialEq)]
pub struct HalError {
    name: ErrorName,
    category: ErrorCategory,
    operation: OperationName,
    retryable: bool,
    debug_message: String,
    resource_id: Option<ResourceId>,
    platform_code: Option<String>,
    vendor_code: Option<String>,
    context: ErrorContext,
}

impl fmt::Debug for HalError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("HalError")
            .field("name", &self.name)
            .field("category", &self.category)
            .field("operation", &self.operation)
            .field("retryable", &self.retryable)
            .finish()
    }
}

impl HalError {
    pub fn new(
        name: impl Into<String>,
        category: ErrorCategory,
        operation: impl Into<String>,
        retryable: bool,
        debug_message: impl Into<String>,
    ) -> HalResult<Self> {
        Ok(Self {
            name: ErrorName::parse(name)?,
            category,
            operation: OperationName::parse(operation)?,
            retryable,
            debug_message: debug_message.into(),
            resource_id: None,
            platform_code: None,
            vendor_code: None,
            context: ErrorContext::default(),
        })
    }

    pub(crate) fn invalid_argument_error(
        name: impl Into<String>,
        operation: impl Into<String>,
        debug_message: impl Into<String>,
    ) -> Self {
        Self {
            name: ErrorName::parse(name)
                .expect("static invalid argument error metadata must be valid"),
            category: ErrorCategory::InvalidArgument,
            operation: OperationName::parse(operation)
                .expect("static invalid argument error metadata must be valid"),
            retryable: false,
            debug_message: debug_message.into(),
            resource_id: None,
            platform_code: None,
            vendor_code: None,
            context: ErrorContext::default(),
        }
    }

    pub fn name(&self) -> &ErrorName {
        &self.name
    }

    pub fn category(&self) -> ErrorCategory {
        self.category
    }

    pub fn operation(&self) -> &OperationName {
        &self.operation
    }

    pub fn retryable(&self) -> bool {
        self.retryable
    }

    pub fn debug_message(&self) -> &str {
        &self.debug_message
    }

    pub fn resource_id(&self) -> Option<&ResourceId> {
        self.resource_id.as_ref()
    }

    pub fn platform_code(&self) -> Option<&str> {
        self.platform_code.as_deref()
    }

    pub fn vendor_code(&self) -> Option<&str> {
        self.vendor_code.as_deref()
    }

    pub fn context(&self) -> &ErrorContext {
        &self.context
    }

    pub fn with_resource_id(mut self, resource_id: ResourceId) -> Self {
        self.resource_id = Some(resource_id);
        self
    }

    pub fn with_platform_code(mut self, code: impl Into<String>) -> HalResult<Self> {
        let code = code.into();
        validate_identifier("error.platform_code", &code)?;
        self.platform_code = Some(code);
        Ok(self)
    }

    pub fn with_vendor_code(mut self, code: impl Into<String>) -> HalResult<Self> {
        let code = code.into();
        validate_identifier("error.vendor_code", &code)?;
        self.vendor_code = Some(code);
        Ok(self)
    }

    pub fn with_context(mut self, context: ErrorContext) -> Self {
        self.context = context;
        self
    }

    pub fn decision_fields(&self) -> ErrorDecisionFields<'_> {
        ErrorDecisionFields {
            name: &self.name,
            category: self.category,
            operation: &self.operation,
            retryable: self.retryable,
        }
    }
}

impl std::fmt::Display for HalError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "{} during {}: {}",
            self.name.as_str(),
            self.operation.as_str(),
            self.debug_message
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
        Ok(Self {
            name: decision.name,
            category: decision.category,
            operation: decision.operation,
            retryable: decision.retryable,
            debug_message: String::new(),
            resource_id: None,
            platform_code: None,
            vendor_code: None,
            context: ErrorContext::default(),
        })
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
