use seeed_hal_gpio::*;

#[test]
fn edge_requests_enforce_bounded_delivery() {
    assert!(GpioEdgeRequest::new(EdgeMask::BOTH, MAX_GPIO_EVENTS).is_ok());
    assert!(GpioEdgeRequest::new(EdgeMask::BOTH, MAX_GPIO_EVENTS + 1).is_err());
    assert!(GpioEdgeRequest::new(EdgeMask::empty(), 1).is_err());
    assert!(GpioEdgeRequest::new(EdgeMask::RISING, 0).is_err());
}

#[test]
fn line_configuration_and_capabilities_are_hardware_class_stable() {
    assert!(GpioLineConfig::input(false, GpioBias::PullUp).is_ok());
    assert!(GpioLineConfig::output(true, false, GpioDrive::PushPull).is_ok());
    assert!(GpioLineConfig::output(true, false, GpioDrive::OpenDrain).is_ok());
    assert_eq!(gpio_lines_capability().as_str(), GPIO_LINES_CAPABILITY);
    assert_eq!(gpio_edges_capability().as_str(), GPIO_EDGES_CAPABILITY);
    assert_eq!(DEFAULT_GPIO_EVENT_CAPACITY, 256);
}
