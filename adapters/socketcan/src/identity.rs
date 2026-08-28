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
pub struct CanInterfaceMetadata {
    pub interface: String,
    pub serial: Option<String>,
    pub stable_path: Option<String>,
    pub topology: Option<String>,
    pub virtual_interface: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CanIdentity {
    pub id: ResourceId,
    pub quality: IdentityQuality,
}

pub fn identity_from_metadata(metadata: &CanInterfaceMetadata) -> HalResult<CanIdentity> {
    Endpoint::new(metadata.interface.clone())?;

    if metadata.virtual_interface {
        return endpoint_identity(&metadata.interface);
    }
    if let Some(serial) = non_empty(metadata.serial.as_deref()) {
        return identity(
            format!("can:serial:{}", encode_segment(serial)),
            IdentityQuality::Strong,
        );
    }
    if let Some(path) = non_empty(metadata.stable_path.as_deref()) {
        return identity(
            format!("can:path:{}", encode_segment(path)),
            IdentityQuality::Medium,
        );
    }
    if let Some(topology) = non_empty(metadata.topology.as_deref()) {
        return identity(
            format!("can:topology:{}", encode_segment(topology)),
            IdentityQuality::Medium,
        );
    }
    endpoint_identity(&metadata.interface)
}

fn endpoint_identity(interface: &str) -> HalResult<CanIdentity> {
    identity(
        format!("can:endpoint:{}", encode_segment(interface)),
        IdentityQuality::Weak,
    )
}

fn identity(id: String, quality: IdentityQuality) -> HalResult<CanIdentity> {
    Ok(CanIdentity {
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

#[cfg(target_os = "linux")]
pub(crate) fn metadata_from_sysfs(interface: &str) -> CanInterfaceMetadata {
    use std::fs;
    use std::path::PathBuf;

    let network_path = PathBuf::from("/sys/class/net").join(interface);
    let device_path = network_path.join("device");
    let serial = read_trimmed(device_path.join("serial"));
    let stable_path = fs::canonicalize(&device_path)
        .ok()
        .filter(|path| !path.starts_with("/sys/devices/virtual"))
        .and_then(|path| path.to_str().map(ToOwned::to_owned));
    let topology = read_trimmed(device_path.join("devpath"));
    let virtual_interface = fs::canonicalize(&network_path)
        .ok()
        .is_some_and(|path| path.starts_with("/sys/devices/virtual"));

    CanInterfaceMetadata {
        interface: interface.to_owned(),
        serial,
        stable_path,
        topology,
        virtual_interface,
    }
}

#[cfg(target_os = "linux")]
fn read_trimmed(path: std::path::PathBuf) -> Option<String> {
    std::fs::read_to_string(path)
        .ok()
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
}
