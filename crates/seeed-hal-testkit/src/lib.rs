#![forbid(unsafe_code)]

mod virtual_serial;
mod virtual_can;

pub use virtual_can::{VirtualCanAdapter, run_can_adapter_conformance};
pub use virtual_serial::VirtualSerialAdapter;
