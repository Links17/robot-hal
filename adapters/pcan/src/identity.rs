use seeed_hal_core::{Endpoint, HalResult, IdentityQuality, ResourceId};

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PcanChannelMetadata {
    pub handle: u16,
    pub device_type: u8,
    pub controller_number: u8,
    pub device_name: Option<String>,
    pub device_id: Option<u32>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PcanIdentity {
    pub id: ResourceId,
    pub quality: IdentityQuality,
}

pub fn identity_from_metadata(metadata: &PcanChannelMetadata) -> HalResult<PcanIdentity> {
    Endpoint::new(format!("pcan://0x{:04X}", metadata.handle))?;

    if let Some(device_id) = metadata.device_id.filter(|value| *value != 0) {
        return identity(
            format!(
                "can:pcan:device:{:02X}:{device_id:08X}:{:02X}",
                metadata.device_type, metadata.controller_number
            ),
            IdentityQuality::Strong,
        );
    }
    // A model/hardware name describes a product family, not one physical
    // instance. Without a nonzero vendor device ID (or future serial evidence),
    // only the transient handle distinguishes channels, so identity stays Weak.
    identity(
        format!("can:pcan:handle:{:04X}", metadata.handle),
        IdentityQuality::Weak,
    )
}

fn identity(id: String, quality: IdentityQuality) -> HalResult<PcanIdentity> {
    Ok(PcanIdentity {
        id: ResourceId::parse(id)?,
        quality,
    })
}
