use percent_encoding::{AsciiSet, CONTROLS, utf8_percent_encode};
use robot_hal_core::{HalResult, IdentityQuality, ResourceId};

const ENCODE_SET: &AsciiSet = &CONTROLS
    .add(b' ')
    .add(b'%')
    .add(b'/')
    .add(b'?')
    .add(b'#')
    .add(b':');

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GpioChipMetadata {
    pub path: String,
    pub kernel_name: String,
    pub label: Option<String>,
    pub line_count: usize,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GpioIdentity {
    pub id: ResourceId,
    pub quality: IdentityQuality,
}

pub fn identity_from_metadata(metadata: &GpioChipMetadata) -> HalResult<GpioIdentity> {
    Ok(GpioIdentity {
        id: ResourceId::parse(format!("gpio:chip:{}", encode(&metadata.kernel_name)))?,
        quality: IdentityQuality::Strong,
    })
}

fn encode(value: &str) -> String {
    utf8_percent_encode(value, ENCODE_SET).to_string()
}
