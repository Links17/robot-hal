#[tokio::test]
#[ignore = "requires ROBOT_HAL_USB_RESOURCE_ID and a selected native USB device"]
async fn selected_interface_claim_and_bounded_transfers() {
    #[cfg(not(feature = "hardware-tests"))]
    {
        panic!("enable hardware-tests and set ROBOT_HAL_USB_RESOURCE_ID");
    }

    #[cfg(feature = "hardware-tests")]
    {
        let _resource_id =
            std::env::var("ROBOT_HAL_USB_RESOURCE_ID").expect("set ROBOT_HAL_USB_RESOURCE_ID");
        panic!("hardware qualification is documented but requires a provisioned fixture");
    }
}
