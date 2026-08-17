#![forbid(unsafe_code)]

pub mod v1 {
    include!(concat!(env!("OUT_DIR"), "/seeed.hal.v1.rs"));
}

mod can_conversion;
mod conversion;

pub use can_conversion::{
    can_receive_parameters, can_receive_request_from_proto, can_receive_response_from_proto,
    can_send_request_from_proto, can_send_response_from_proto, can_send_response_to_proto,
    enumerate_can_response_from_proto, get_can_bus_status_request_from_proto,
    get_can_bus_status_response_from_proto, open_can_request_from_proto,
    open_can_response_from_proto, received_can_frames_from_proto,
    replace_can_filters_request_from_proto, send_can_frames_from_proto,
};
pub use conversion::{
    enumerate_serial_response_from_proto, error_from_proto, gpio_close_request_from_proto,
    gpio_config_from_proto, gpio_edge_event_from_proto, gpio_edge_request_from_proto,
    gpio_next_edge_request_from_proto, gpio_next_edge_response_from_proto,
    gpio_next_edge_response_to_proto, gpio_read_request_from_proto, gpio_read_response_from_proto,
    gpio_read_response_to_proto, gpio_selector_from_proto, gpio_write_request_from_proto,
    invalid_message, open_gpio_request_from_proto, open_gpio_response_from_proto,
    open_gpio_response_to_proto, open_serial_request_from_proto, open_serial_response_from_proto,
    open_usb_request_from_proto, open_usb_response_from_proto, open_usb_response_to_proto,
    parse_serial_session_lease, parse_session_lease, serial_selector_from_proto,
    usb_close_request_from_proto, usb_selector_from_proto, usb_transfer_from_proto,
    usb_transfer_request_from_proto, usb_transfer_response_from_proto,
    usb_transfer_response_to_proto,
};

pub const PROTOCOL_MAJOR: u32 = 1;
pub const PROTOCOL_MINOR_MINIMUM: u32 = 0;
pub const PROTOCOL_MINOR_MAXIMUM: u32 = 2;
/// Legacy exact-minor field value sent to peers that predate range fields.
pub const PROTOCOL_MINOR: u32 = PROTOCOL_MINOR_MAXIMUM;
pub const SERIAL_CAPABILITY: &str = "serial.bytes/v1";
pub const MAX_FRAME_BYTES: usize = 1024 * 1024;

pub fn handshake_minor_range(request: &v1::HandshakeRequest) -> HalResult<(u32, u32)> {
    if request.protocol_minor_minimum == 0 && request.protocol_minor_maximum == 0 {
        return Ok((request.protocol_minor, request.protocol_minor));
    }
    if request.protocol_minor_minimum > request.protocol_minor_maximum
        || request.protocol_minor != request.protocol_minor_maximum
    {
        return Err(invalid_message(
            "protocol minor range is invalid or conflicts with legacy protocol_minor",
        ));
    }
    Ok((
        request.protocol_minor_minimum,
        request.protocol_minor_maximum,
    ))
}

pub fn handshake_response_minor_range(response: &v1::HandshakeResponse) -> HalResult<(u32, u32)> {
    if response.protocol_minor_minimum == 0 && response.protocol_minor_maximum == 0 {
        return Ok((response.protocol_minor, response.protocol_minor));
    }
    if response.protocol_minor_minimum > response.protocol_minor_maximum
        || response.protocol_minor < response.protocol_minor_minimum
        || response.protocol_minor > response.protocol_minor_maximum
    {
        return Err(invalid_message(
            "broker protocol minor range does not contain the selected minor",
        ));
    }
    Ok((
        response.protocol_minor_minimum,
        response.protocol_minor_maximum,
    ))
}

pub fn negotiate_protocol_minor(
    client_major: u32,
    client_minimum: u32,
    client_maximum: u32,
    broker_major: u32,
    broker_minimum: u32,
    broker_maximum: u32,
) -> HalResult<u32> {
    if client_minimum > client_maximum || broker_minimum > broker_maximum {
        return Err(invalid_message(
            "protocol minor range minimum exceeds maximum",
        ));
    }
    let shared_minimum = client_minimum.max(broker_minimum);
    let shared_maximum = client_maximum.min(broker_maximum);
    if client_major != broker_major || shared_minimum > shared_maximum {
        return Err(HalError::new(
            "runtime.protocol.version_incompatible",
            ErrorCategory::Conflict,
            "runtime.protocol.handshake",
            false,
            "client and broker protocol version ranges do not overlap",
        )?);
    }
    Ok(shared_maximum)
}

use seeed_hal_core::{ErrorCategory, HalError, HalResult};
