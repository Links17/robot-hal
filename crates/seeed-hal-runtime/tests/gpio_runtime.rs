use seeed_hal_core::OwnerId;
use seeed_hal_gpio::{GpioBias, GpioLineConfig};
use seeed_hal_runtime::HalRuntime;
use seeed_hal_testkit::VirtualGpioAdapter;

#[tokio::test]
async fn gpio_runtime_releases_owner_lines_and_fences_old_generation() {
    let adapter = VirtualGpioAdapter::line_bank("gpio:runtime:fencing", 2);
    let runtime = HalRuntime::builder().gpio_adapter(adapter).build();
    let descriptor = runtime.enumerate_gpio().await.unwrap().remove(0);
    let owner = OwnerId::parse("owner:gpio-runtime").unwrap();
    let mut first = runtime
        .open_gpio(
            owner.clone(),
            descriptor.selector(),
            vec![0],
            GpioLineConfig::input(false, GpioBias::Disabled).unwrap(),
        )
        .await
        .unwrap();
    let stale = first.lease_token().clone();
    let session = first.session_id();
    first.close().await.unwrap();
    let mut second = runtime
        .open_gpio(
            owner,
            descriptor.selector(),
            vec![0],
            GpioLineConfig::input(false, GpioBias::Disabled).unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        runtime
            .gpio_read(session, &stale)
            .await
            .unwrap_err()
            .name()
            .as_str(),
        "runtime.lease.stale_generation"
    );
    second.close().await.unwrap();
}
