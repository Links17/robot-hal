use seeed_hal_client::RemoteCameraHandle;

#[test]
fn remote_camera_handle_is_a_public_client_surface() {
    assert!(std::any::type_name::<RemoteCameraHandle>().contains("RemoteCameraHandle"));
}
