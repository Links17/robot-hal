use seeed_hal_gpio::{
    EdgeMask, GpioAdapter, GpioBias, GpioDrive, GpioEdge, GpioEdgeRequest, GpioLineConfig,
};
use seeed_hal_testkit::VirtualGpioAdapter;
use std::time::Duration;

#[tokio::test]
async fn virtual_gpio_enforces_direction_and_keeps_edge_order() {
    let adapter = VirtualGpioAdapter::line_bank("gpio:virtual:conformance", 2);
    let descriptor = adapter.enumerate().await.unwrap().remove(0);
    let mut output = adapter
        .open(
            &descriptor.selector(),
            &[0],
            GpioLineConfig::output(false, false, GpioDrive::PushPull).unwrap(),
        )
        .await
        .unwrap();
    output.write(&[true]).await.unwrap();
    assert_eq!(output.read().await.unwrap(), vec![true]);
    output.close().await.unwrap();
    let mut input = adapter
        .open(
            &descriptor.selector(),
            &[0],
            GpioLineConfig::input(false, GpioBias::Disabled).unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        input.write(&[true]).await.unwrap_err().name().as_str(),
        "gpio.direction.invalid"
    );
    adapter.inject_edge(0, GpioEdge::Rising, 10).unwrap();
    adapter.inject_edge(0, GpioEdge::Falling, 11).unwrap();
    let request = GpioEdgeRequest::new(EdgeMask::BOTH, 2).unwrap();
    assert_eq!(
        input
            .next_edge(request, Duration::ZERO)
            .await
            .unwrap()
            .unwrap()
            .edge(),
        GpioEdge::Rising
    );
    assert_eq!(
        input
            .next_edge(request, Duration::ZERO)
            .await
            .unwrap()
            .unwrap()
            .edge(),
        GpioEdge::Falling
    );
}

#[tokio::test]
async fn virtual_gpio_drops_oldest_edges_and_reports_structured_lag() {
    let adapter = VirtualGpioAdapter::line_bank_with_event_capacity("gpio:virtual:lag", 1, 2);
    let descriptor = adapter.enumerate().await.unwrap().remove(0);
    let mut input = adapter
        .open(
            &descriptor.selector(),
            &[0],
            GpioLineConfig::input(false, GpioBias::Disabled).unwrap(),
        )
        .await
        .unwrap();

    adapter.inject_edge(0, GpioEdge::Rising, 1).unwrap();
    adapter.inject_edge(0, GpioEdge::Falling, 2).unwrap();
    adapter.inject_edge(0, GpioEdge::Rising, 3).unwrap();

    let request = GpioEdgeRequest::new(EdgeMask::BOTH, 2).unwrap();
    let error = input.next_edge(request, Duration::ZERO).await.unwrap_err();
    assert_eq!(error.name().as_str(), "gpio.edge.lagged");
    assert_eq!(
        error
            .context()
            .iter()
            .find_map(|(key, value)| (key == "dropped_count").then_some(value)),
        Some("1")
    );
    assert_eq!(
        input
            .next_edge(request, Duration::ZERO)
            .await
            .unwrap()
            .unwrap()
            .monotonic_ns(),
        2
    );
}
