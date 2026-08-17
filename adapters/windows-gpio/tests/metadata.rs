#[cfg(not(windows))]
#[tokio::test]
async fn unsupported_platform_fails_closed_without_gpio_discovery() {
    use seeed_hal_adapter_windows_gpio::WindowsGpioAdapter;
    use seeed_hal_gpio::GpioAdapter;

    let adapter = WindowsGpioAdapter::new();

    let error = adapter
        .enumerate()
        .await
        .expect_err("Windows GPIO must not probe hardware off Windows");

    assert_eq!(error.name().as_str(), "runtime.adapter.unavailable");
    assert_eq!(error.operation().as_str(), "gpio.enumerate");
}
