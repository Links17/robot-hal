#![forbid(unsafe_code)]

mod virtual_can;
mod virtual_gpio;
mod virtual_serial;
mod virtual_usb;

pub use virtual_can::{VirtualCanAdapter, run_can_adapter_conformance};
pub use virtual_gpio::VirtualGpioAdapter;
pub use virtual_serial::VirtualSerialAdapter;
pub use virtual_usb::VirtualUsbAdapter;
