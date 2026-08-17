use std::process::Command;

use sha2::{Digest, Sha256};

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
    assert_eq!(manifest["broker_version"], "0.2.0");
    assert_eq!(manifest["wire"]["maximum_minor"], 2);
    assert_eq!(manifest["target"]["triple"], env!("SEEED_HAL_TARGET"));
    assert_eq!(manifest["target"]["os"], std::env::consts::OS);
    assert_eq!(manifest["target"]["arch"], std::env::consts::ARCH);
    let mut expected_adapters = vec!["serialport"];
    let mut expected_features = Vec::new();
    #[cfg(feature = "pcan")]
    {
        expected_adapters.push("pcan");
        expected_features.push("pcan");
    }
    #[cfg(feature = "socketcan")]
    {
        expected_adapters.push("socketcan");
        expected_features.push("socketcan");
    }
    #[cfg(feature = "virtual-adapters")]
    {
        expected_adapters.extend([
            "virtual-can",
            "virtual-gpio",
            "virtual-serial",
            "virtual-usb",
        ]);
        expected_features.push("virtual-adapters");
    }
    expected_adapters.sort_unstable();
    assert_eq!(
        manifest["enabled"]["adapters"],
        serde_json::json!(expected_adapters)
    );
    assert_eq!(
        manifest["enabled"]["features"],
        serde_json::json!(expected_features)
    );
    #[cfg(feature = "pcan")]
    assert_eq!(
        manifest["required_vendor_runtime_libraries"],
        serde_json::json!(["PCAN-Basic"])
    );
    #[cfg(not(feature = "pcan"))]
    assert_eq!(
        manifest["required_vendor_runtime_libraries"],
        serde_json::json!([])
    );
    assert_eq!(manifest["msrv"], "1.85");
    assert_eq!(manifest["artifact_checksum"]["algorithm"], "sha256");
    let executable = std::fs::read(env!("CARGO_BIN_EXE_seeed-hal-broker")).unwrap();
    let expected_checksum = format!("{:x}", Sha256::digest(executable));
    assert_eq!(manifest["artifact_checksum"]["value"], expected_checksum);
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
