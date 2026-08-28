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
pub struct UsbDeviceMetadata {
    pub vendor_id: u16,
    pub product_id: u16,
    pub serial_number: Option<String>,
    /// A stable physical-port topology when the device has no serial number.
    /// It is deliberately distinct from transient bus/address enumeration.
    pub topology: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct UsbIdentity {
    pub id: ResourceId,
    pub quality: IdentityQuality,
}

pub fn identity_from_metadata(metadata: &UsbDeviceMetadata) -> HalResult<UsbIdentity> {
    let (id, quality) = match metadata.serial_number.as_deref().map(str::trim) {
        Some(serial) if !serial.is_empty() => (
            format!(
                "usb:device:{:04x}:{:04x}:{}",
                metadata.vendor_id,
                metadata.product_id,
                encode(serial)
            ),
            IdentityQuality::Strong,
        ),
        _ => (
            format!("usb:topology:{}", encode(&metadata.topology)),
            IdentityQuality::Weak,
        ),
    };
    Ok(UsbIdentity {
        id: ResourceId::parse(id)?,
        quality,
    })
}

fn encode(value: &str) -> String {
    utf8_percent_encode(value, ENCODE_SET).to_string()
}
