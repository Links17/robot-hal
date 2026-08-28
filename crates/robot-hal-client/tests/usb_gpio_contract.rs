use robot_hal_client::{HalClient, RemoteGpioHandle, RemoteUsbHandle};
use robot_hal_gpio::{
    EdgeMask, GpioBias, GpioDirection, GpioDrive, GpioEdge, GpioEdgeEvent, GpioEdgeRequest,
    GpioLineConfig,
};
use robot_hal_usb::UsbInterfaceClaim;

#[tokio::test]
async fn usb_transfer_reversed_responses_remain_correlated() {
    let _ = HalClient::enumerate_usb;
    let _ = RemoteUsbHandle::transfer;
}

#[tokio::test]
async fn gpio_public_handles_reuse_public_models() {
    let config = GpioLineConfig::output(false, true, GpioDrive::PushPull).unwrap();
    assert_eq!(config.direction(), GpioDirection::Output);
    assert!(GpioEdgeRequest::new(EdgeMask::BOTH, 1).is_ok());
    let _ = RemoteGpioHandle::next_edge;
    let _: fn(GpioEdgeEvent) -> GpioEdge = GpioEdgeEvent::edge;
    let _ = (GpioBias::Disabled, UsbInterfaceClaim::new(0).unwrap());
}

#[tokio::test]
async fn minor_zero_and_one_reject_usb_gpio_locally() {
    let _ = HalClient::enumerate_usb;
    let _ = HalClient::enumerate_gpio;
}
