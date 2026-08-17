use std::io::{self, Read};

use serde::Serialize;
use sha2::{Digest, Sha256};

#[derive(Serialize)]
pub struct BrokerManifest {
    broker_version: &'static str,
    wire: WireRange,
    target: Target,
    enabled: Enabled,
    msrv: &'static str,
    artifact_checksum: ArtifactChecksum,
    required_vendor_runtime_libraries: Vec<&'static str>,
}

#[derive(Serialize)]
struct WireRange {
    major: u32,
    minimum_minor: u32,
    maximum_minor: u32,
}

#[derive(Serialize)]
struct Target {
    triple: &'static str,
    os: &'static str,
    arch: &'static str,
}

#[derive(Serialize)]
struct Enabled {
    adapters: Vec<&'static str>,
    features: Vec<&'static str>,
}

#[derive(Serialize)]
struct ArtifactChecksum {
    algorithm: &'static str,
    value: String,
}

impl BrokerManifest {
    pub fn current() -> io::Result<Self> {
        let executable = std::env::current_exe()?;
        let mut file = std::fs::File::open(executable)?;
        let mut hasher = Sha256::new();
        let mut buffer = [0_u8; 64 * 1024];
        loop {
            let read = file.read(&mut buffer)?;
            if read == 0 {
                break;
            }
            hasher.update(&buffer[..read]);
        }
        Ok(Self {
            broker_version: env!("CARGO_PKG_VERSION"),
            wire: WireRange {
                major: seeed_hal_protocol::PROTOCOL_MAJOR,
                minimum_minor: seeed_hal_protocol::PROTOCOL_MINOR_MINIMUM,
                maximum_minor: seeed_hal_protocol::PROTOCOL_MINOR_MAXIMUM,
            },
            target: Target {
                triple: env!("SEEED_HAL_TARGET"),
                os: std::env::consts::OS,
                arch: std::env::consts::ARCH,
            },
            enabled: Enabled {
                adapters: enabled_adapters(),
                features: enabled_features(),
            },
            msrv: env!("CARGO_PKG_RUST_VERSION"),
            artifact_checksum: ArtifactChecksum {
                algorithm: "sha256",
                value: format!("{:x}", hasher.finalize()),
            },
            required_vendor_runtime_libraries: required_vendor_runtime_libraries(),
        })
    }
}

fn enabled_adapters() -> Vec<&'static str> {
    let mut adapters = vec!["serialport"];
    #[cfg(feature = "pcan")]
    adapters.push("pcan");
    #[cfg(feature = "socketcan")]
    adapters.push("socketcan");
    #[cfg(all(feature = "nusb", not(feature = "virtual-adapters")))]
    adapters.push("nusb");
    #[cfg(all(
        feature = "linux-gpio",
        target_os = "linux",
        not(feature = "virtual-adapters")
    ))]
    adapters.push("linux-gpio");
    #[cfg(all(feature = "windows-gpio", windows, not(feature = "virtual-adapters")))]
    adapters.push("windows-gpio");
    #[cfg(feature = "virtual-adapters")]
    adapters.extend([
        "virtual-can",
        "virtual-gpio",
        "virtual-serial",
        "virtual-usb",
    ]);
    adapters.sort_unstable();
    adapters
}

#[allow(clippy::vec_init_then_push)] // Feature membership is compile-time conditional.
fn enabled_features() -> Vec<&'static str> {
    #[allow(unused_mut)]
    let mut features = Vec::new();
    #[cfg(feature = "pcan")]
    features.push("pcan");
    #[cfg(feature = "socketcan")]
    features.push("socketcan");
    #[cfg(all(feature = "nusb", not(feature = "virtual-adapters")))]
    features.push("nusb");
    #[cfg(all(
        feature = "linux-gpio",
        target_os = "linux",
        not(feature = "virtual-adapters")
    ))]
    features.push("linux-gpio");
    #[cfg(all(feature = "windows-gpio", windows, not(feature = "virtual-adapters")))]
    features.push("windows-gpio");
    #[cfg(feature = "virtual-adapters")]
    features.push("virtual-adapters");
    features
}

fn required_vendor_runtime_libraries() -> Vec<&'static str> {
    #[cfg(feature = "pcan")]
    return vec!["PCAN-Basic"];
    #[cfg(not(feature = "pcan"))]
    Vec::new()
}
