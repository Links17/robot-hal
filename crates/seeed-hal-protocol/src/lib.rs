#![forbid(unsafe_code)]

pub mod v1 {
    include!(concat!(env!("OUT_DIR"), "/seeed.hal.v1.rs"));
}

mod conversion;

pub use conversion::{invalid_message, parse_session_lease};

pub const PROTOCOL_MAJOR: u32 = 1;
pub const PROTOCOL_MINOR: u32 = 0;
pub const SERIAL_CAPABILITY: &str = "serial.bytes/v1";
pub const MAX_FRAME_BYTES: usize = 1024 * 1024;
