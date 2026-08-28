use percent_encoding::{AsciiSet, CONTROLS, utf8_percent_encode};
use robot_hal_core::{Endpoint, HalResult, IdentityQuality, ResourceId};

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
pub struct UsbPortMetadata {
    pub vid: u16,
    pub pid: u16,
    pub serial_number: Option<String>,
    pub manufacturer: Option<String>,
    pub product: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SerialIdentity {
    pub id: ResourceId,
    pub quality: IdentityQuality,
}

pub fn identity_from_usb_metadata(
    endpoint: impl AsRef<str>,
    metadata: &UsbPortMetadata,
) -> HalResult<SerialIdentity> {
    if let Some(serial_number) = non_empty(metadata.serial_number.as_deref()) {
        return identity(
            format!(
                "serial:usb:{:04x}:{:04x}:{}",
                metadata.vid,
                metadata.pid,
                encode_segment(serial_number)
            ),
            IdentityQuality::Strong,
        );
    }

    identity_from_endpoint(endpoint)
}

pub fn identity_from_endpoint(endpoint: impl AsRef<str>) -> HalResult<SerialIdentity> {
    let endpoint = endpoint.as_ref();
    Endpoint::new(endpoint.to_owned())?;
    identity(
        format!("serial:endpoint:{}", encode_segment(endpoint)),
        IdentityQuality::Weak,
    )
}

fn identity(id: String, quality: IdentityQuality) -> HalResult<SerialIdentity> {
    Ok(SerialIdentity {
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
