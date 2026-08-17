#![forbid(unsafe_code)]

mod virtual_can;
mod virtual_serial;

pub use virtual_can::{VirtualCanAdapter, run_can_adapter_conformance};
pub use virtual_serial::VirtualSerialAdapter;
