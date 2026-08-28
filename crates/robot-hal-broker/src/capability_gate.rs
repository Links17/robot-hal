use robot_hal_core::{
    CapabilityId, CapabilitySet, ErrorCategory, ErrorContext, HalError, HalResult, ResourceId,
};

/// Validates a resource capability at the broker seam.
///
/// Dispatchers retain their hardware-specific payload and session logic; this
/// helper centralizes the stable error contract for capability admission.
pub(crate) fn require(
    capabilities: &CapabilitySet,
    capability: &CapabilityId,
    name: &'static str,
    resource: &ResourceId,
) -> HalResult<()> {
    if capabilities.contains(capability) {
        return Ok(());
    }

    Err(HalError::new(
        "runtime.protocol.capability_unsupported",
        ErrorCategory::Conflict,
        "runtime.protocol.dispatch",
        false,
        "selected resource does not advertise the required capability",
    )?
    .with_resource_id(resource.clone())
    .with_context(ErrorContext::new([("capability", name)])?))
}
