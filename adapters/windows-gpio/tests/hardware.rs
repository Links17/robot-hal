#[cfg(windows)]
#[tokio::test]
#[ignore = "requires ROBOT_HAL_GPIO_RESOURCE_ID and a provisioned Windows GPIO controller"]
async fn selected_controller_reads_writes_and_reports_monotonic_edges() {
    #[cfg(not(feature = "hardware-tests"))]
    {
        panic!("enable hardware-tests and set ROBOT_HAL_GPIO_RESOURCE_ID");
    }

    #[cfg(feature = "hardware-tests")]
    {
        let _resource_id =
            std::env::var("ROBOT_HAL_GPIO_RESOURCE_ID").expect("set ROBOT_HAL_GPIO_RESOURCE_ID");
        panic!("hardware qualification is documented but requires a provisioned fixture");
    }
}
