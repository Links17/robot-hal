use robot_hal_adapter_linux_gpio::identity::{GpioChipMetadata, identity_from_metadata};
use robot_hal_gpio::IdentityQuality;

#[test]
fn chip_kernel_name_is_a_strong_gpio_identity() {
    let metadata = GpioChipMetadata {
        path: "/dev/gpiochip0".to_owned(),
        kernel_name: "gpiochip0".to_owned(),
        label: Some("pinctrl-bcm2711".to_owned()),
        line_count: 58,
    };

    let identity = identity_from_metadata(&metadata).expect("metadata is valid");

    assert_eq!(identity.id.as_str(), "gpio:chip:gpiochip0");
    assert_eq!(identity.quality, IdentityQuality::Strong);
}

#[test]
fn endpoint_path_is_not_used_when_chip_identity_exists() {
    let first = GpioChipMetadata {
        path: "/dev/gpiochip0".to_owned(),
        kernel_name: "gpiochip0".to_owned(),
        label: None,
        line_count: 8,
    };
    let moved = GpioChipMetadata {
        path: "/dev/gpiochip9".to_owned(),
        ..first.clone()
    };

    assert_eq!(
        identity_from_metadata(&first).unwrap().id,
        identity_from_metadata(&moved).unwrap().id
    );
}
