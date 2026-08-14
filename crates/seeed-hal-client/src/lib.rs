#![forbid(unsafe_code)]

mod connection;
mod serial;

pub use connection::{ClientEvent, ConnectionOptions, EventSubscription, HalClient};
pub use serial::RemoteSerialHandle;
