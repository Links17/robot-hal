#[cfg(target_os = "linux")]
#[tokio::test]
#[ignore = "requires SEEED_HAL_GPIO_RESOURCE_ID, provisioned GPIO lines, and an external edge fixture"]
async fn selected_chip_reads_writes_and_reports_monotonic_edges() {
    #[cfg(not(feature = "hardware-tests"))]
    {
        panic!("enable hardware-tests and set SEEED_HAL_GPIO_RESOURCE_ID");
    }

    #[cfg(feature = "hardware-tests")]
    {
        let _resource_id =
            std::env::var("SEEED_HAL_GPIO_RESOURCE_ID").expect("set SEEED_HAL_GPIO_RESOURCE_ID");
        panic!("hardware qualification is documented but requires a provisioned fixture");
    }
}
