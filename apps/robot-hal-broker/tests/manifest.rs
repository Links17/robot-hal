use std::process::Command;

use sha2::{Digest, Sha256};
use wait_timeout::ChildExt;

#[test]
fn manifest_is_deterministic_business_independent_and_hardware_free() {
    let token_path = std::env::temp_dir().join(format!(
        "robot-hal-manifest-token-that-must-not-be-read-{}",
        std::process::id()
    ));
    std::fs::write(&token_path, b"not-a-valid-token").unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_robot-hal-broker"))
        .args([
            "--manifest",
            "--endpoint",
            "/path/that/must/not/be/bound",
            "--auth-token-file",
            token_path.to_str().unwrap(),
        ])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let output = wait_with_output(output, std::time::Duration::from_secs(10));
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
    assert_eq!(manifest["schema"], serde_json::json!({"major": 1}));
    assert_eq!(manifest["broker_version"], env!("CARGO_PKG_VERSION"));
    assert_eq!(manifest["wire"]["major"], 1);
    assert_eq!(manifest["wire"]["minimum_minor"], 0);
    assert_eq!(manifest["broker_version"], "0.5.0-rc.1");
    assert_eq!(manifest["wire"]["maximum_minor"], 3);
    assert_eq!(manifest["target"]["triple"], env!("ROBOT_HAL_TARGET"));
    assert_eq!(manifest["target"]["os"], std::env::consts::OS);
    assert_eq!(manifest["target"]["arch"], std::env::consts::ARCH);
    let mut expected_adapters = vec!["serialport"];
    #[allow(unused_mut)]
    let mut expected_features: Vec<&str> = Vec::new();
    #[cfg(feature = "serialport")]
    expected_features.push("serialport");
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
    #[cfg(all(feature = "nusb", not(feature = "virtual-adapters")))]
    {
        expected_adapters.push("nusb");
        expected_features.push("nusb");
    }
    #[cfg(all(
        feature = "linux-gpio",
        target_os = "linux",
        not(feature = "virtual-adapters")
    ))]
    {
        expected_adapters.push("linux-gpio");
        expected_features.push("linux-gpio");
    }
    #[cfg(all(feature = "windows-gpio", windows, not(feature = "virtual-adapters")))]
    {
        expected_adapters.push("windows-gpio");
        expected_features.push("windows-gpio");
    }
    #[cfg(all(
        feature = "avfoundation",
        target_os = "macos",
        not(feature = "virtual-adapters")
    ))]
    {
        expected_adapters.push("avfoundation");
        expected_features.push("avfoundation");
    }
    #[cfg(all(
        feature = "v4l2",
        target_os = "linux",
        not(feature = "virtual-adapters")
    ))]
    {
        expected_adapters.push("v4l2");
        expected_features.push("v4l2");
    }
    #[cfg(all(
        feature = "mediafoundation",
        windows,
        not(feature = "virtual-adapters")
    ))]
    {
        expected_adapters.push("mediafoundation");
        expected_features.push("mediafoundation");
    }
    #[cfg(feature = "virtual-adapters")]
    {
        expected_adapters.extend([
            "virtual-can",
            "virtual-camera",
            "virtual-gpio",
            "virtual-serial",
            "virtual-usb",
        ]);
        expected_features.push("virtual-adapters");
    }
    expected_features.sort_unstable();
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
    let executable = std::fs::read(env!("CARGO_BIN_EXE_robot-hal-broker")).unwrap();
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

fn wait_with_output(
    mut child: std::process::Child,
    timeout: std::time::Duration,
) -> std::process::Output {
    if child.wait_timeout(timeout).unwrap().is_some() {
        return child.wait_with_output().unwrap();
    }
    child.kill().unwrap();
    let output = child.wait_with_output().unwrap();
    panic!(
        "broker manifest command timed out: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}
