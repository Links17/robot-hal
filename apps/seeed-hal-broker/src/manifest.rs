use serde::Serialize;

#[derive(Serialize)]
pub struct BrokerManifest {
    broker_version: &'static str,
    wire: WireRange,
    target: &'static str,
    enabled_adapters: [&'static str; 1],
}

#[derive(Serialize)]
struct WireRange {
    major: u32,
    minimum_minor: u32,
    maximum_minor: u32,
}

impl BrokerManifest {
    pub const fn current() -> Self {
        Self {
            broker_version: env!("CARGO_PKG_VERSION"),
            wire: WireRange {
                major: seeed_hal_protocol::PROTOCOL_MAJOR,
                minimum_minor: seeed_hal_protocol::PROTOCOL_MINOR,
                maximum_minor: seeed_hal_protocol::PROTOCOL_MINOR,
            },
            target: env!("SEEED_HAL_TARGET"),
            enabled_adapters: ["serialport"],
        }
    }
}
