#[tokio::test]
#[ignore = "requires SEEED_HAL_USB_RESOURCE_ID and a selected native USB device"]
async fn selected_interface_claim_and_bounded_transfers() {
    #[cfg(not(feature = "hardware-tests"))]
    {
        panic!("enable hardware-tests and set SEEED_HAL_USB_RESOURCE_ID");
    }

    #[cfg(feature = "hardware-tests")]
    {
        let _resource_id =
            std::env::var("SEEED_HAL_USB_RESOURCE_ID").expect("set SEEED_HAL_USB_RESOURCE_ID");
        panic!("hardware qualification is documented but requires a provisioned fixture");
    }
}
