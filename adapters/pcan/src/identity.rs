use percent_encoding::{AsciiSet, CONTROLS, utf8_percent_encode};
use seeed_hal_core::{Endpoint, HalResult, IdentityQuality, ResourceId};

const IDENTITY_ENCODE_SET: &AsciiSet = &CONTROLS
    .add(b' ')
    .add(b'%')
    .add(b'/')
    .add(b'?')
    .add(b'#')
    .add(b'[')
    .add(b']')
    .add(b'@')
    .add(b'!')
    .add(b'$')
    .add(b'&')
    .add(b'\'')
    .add(b'(')
    .add(b')')
    .add(b'*')
    .add(b'+')
    .add(b',')
    .add(b';')
    .add(b'=')
    .add(b':');

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
    if let Some(device_name) = non_empty(metadata.device_name.as_deref()) {
        return identity(
            format!(
                "can:pcan:hardware:{}:{:02X}",
                encode_segment(device_name),
                metadata.controller_number
            ),
            IdentityQuality::Medium,
        );
    }
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

fn non_empty(value: Option<&str>) -> Option<&str> {
    value.map(str::trim).filter(|value| !value.is_empty())
}

fn encode_segment(value: &str) -> String {
    utf8_percent_encode(value, IDENTITY_ENCODE_SET).to_string()
}
