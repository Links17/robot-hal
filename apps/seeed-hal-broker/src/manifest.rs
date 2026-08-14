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
    required_vendor_runtime_libraries: [&'static str; 0],
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
    adapters: [&'static str; 1],
    features: [&'static str; 0],
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
                minimum_minor: seeed_hal_protocol::PROTOCOL_MINOR,
                maximum_minor: seeed_hal_protocol::PROTOCOL_MINOR,
            },
            target: Target {
                triple: env!("SEEED_HAL_TARGET"),
                os: std::env::consts::OS,
                arch: std::env::consts::ARCH,
            },
            enabled: Enabled {
                adapters: ["serialport"],
                features: [],
            },
            msrv: env!("CARGO_PKG_RUST_VERSION"),
            artifact_checksum: ArtifactChecksum {
                algorithm: "sha256",
                value: format!("{:x}", hasher.finalize()),
            },
            required_vendor_runtime_libraries: [],
        })
    }
}
