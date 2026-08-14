use std::process::Command;

#[test]
fn manifest_is_deterministic_business_independent_and_hardware_free() {
    let token_path = std::env::temp_dir().join(format!(
        "seeed-hal-manifest-token-that-must-not-be-read-{}",
        std::process::id()
    ));
    std::fs::write(&token_path, b"not-a-valid-token").unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_seeed-hal-broker"))
        .args([
            "--manifest",
            "--endpoint",
            "/path/that/must/not/be/bound",
            "--auth-token-file",
            token_path.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        token_path.exists(),
        "manifest mode must not remove the token file"
    );

    let stdout = String::from_utf8(output.stdout).unwrap();
    let manifest: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    assert_eq!(manifest["broker_version"], env!("CARGO_PKG_VERSION"));
    assert_eq!(manifest["wire"]["major"], 1);
    assert_eq!(manifest["wire"]["minimum_minor"], 0);
    assert_eq!(manifest["wire"]["maximum_minor"], 0);
    assert_eq!(manifest["target"], env!("SEEED_HAL_TARGET"));
    assert_eq!(
        manifest["enabled_adapters"],
        serde_json::json!(["serialport"])
    );

    let lower = stdout.to_ascii_lowercase();
    for forbidden in [
        "robot",
        "leader",
        "follower",
        "joint",
        "episode",
        "calibration",
        "teleoperation",
        "training",
        "inference",
        "dataset",
        "feetech",
        "damiao",
        "robstride",
        "b601",
        "motor",
    ] {
        assert!(
            !lower.contains(forbidden),
            "forbidden term in manifest: {forbidden}"
        );
    }

    std::fs::remove_file(token_path).unwrap();
}
