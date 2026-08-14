#![forbid(unsafe_code)]

mod capability;
mod error;
mod identity;
mod lease;

pub use capability::{CapabilityId, CapabilitySet};
pub use error::{ErrorCategory, ErrorName, HalError, HalResult, OperationName};
pub use identity::{
    Endpoint, IdentityQuality, ResourceDescriptor, ResourceId, ResourceProperties,
    ResourceSelector, TransportKind, resolve_resource,
};
pub use lease::{LeaseId, LeaseMode, LeaseToken, OwnerId, SessionId};
