#![forbid(unsafe_code)]

mod camera;
mod can;
mod connection;
mod gpio;
mod serial;
mod usb;

pub use camera::RemoteCameraHandle;
pub use can::RemoteCanHandle;
pub use connection::{ClientEvent, ConnectionOptions, EventSubscription, HalClient};
pub use gpio::RemoteGpioHandle;
pub use serial::RemoteSerialHandle;
pub use usb::RemoteUsbHandle;
