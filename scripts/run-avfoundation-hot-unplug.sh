#!/usr/bin/env bash
# Supervised AVFoundation camera hot-unplug qualification on macOS.
#
# Usage:
#   ./scripts/run-avfoundation-hot-unplug.sh
#   SEEED_HAL_CAMERA_FIXTURE_NAME='My UVC Camera' ./scripts/run-avfoundation-hot-unplug.sh
#   SEEED_HAL_CAMERA_RESOURCE_ID='camera:avfoundation:<id>' ./scripts/run-avfoundation-hot-unplug.sh
#
# When multiple cameras share the same localizedName, the script probes each
# unique ID via AVCaptureSession to find the one that actually delivers frames
# and selects it automatically.  Set SEEED_HAL_CAMERA_RESOURCE_ID explicitly
# to skip auto-selection entirely.
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$repo_root"

if [[ "$(uname -s)" != "Darwin" ]]; then
  echo "error: AVFoundation hot-unplug qualification runs only on macOS" >&2
  exit 1
fi

fixture_name="${SEEED_HAL_CAMERA_FIXTURE_NAME:-1080P USB Camera}"
test_name="${SEEED_HAL_CAMERA_HOT_UNPLUG_TEST:-physical_camera_hot_unplug_becomes_terminal_then_reopens}"

if [[ -z "${SEEED_HAL_CAMERA_RESOURCE_ID:-}" ]]; then
  matches=()
  while IFS= read -r line; do
    [[ -z "$line" ]] && continue
    matches+=("$line")
  done <<EOF
$(swift -e '
import AVFoundation
let target = CommandLine.arguments[1]
for device in AVCaptureDevice.devices(for: .video) where device.localizedName == target {
    print(device.uniqueID)
}
' "$fixture_name" 2>/dev/null || true)
EOF

  if ((${#matches[@]} == 0)); then
    echo "error: no camera named '${fixture_name}' is connected" >&2
    echo "available video devices:" >&2
    swift -e '
import AVFoundation
for device in AVCaptureDevice.devices(for: .video) {
    print("- \(device.localizedName) (\(device.uniqueID)) connected=\(device.isConnected)")
}
' >&2 || true
    echo "hint: set SEEED_HAL_CAMERA_FIXTURE_NAME or SEEED_HAL_CAMERA_RESOURCE_ID" >&2
    exit 1
  fi

  if ((${#matches[@]} == 1)); then
    export SEEED_HAL_CAMERA_RESOURCE_ID="camera:avfoundation:${matches[0]}"
  else
    echo "notice: ${#matches[@]} cameras share the name '${fixture_name}' — probing for active device..."
    selected=""
    for uid in "${matches[@]}"; do
      echo "  probing ${uid} ..."
      result=$(swift -e '
import AVFoundation
import Foundation
final class FrameProbe: NSObject, AVCaptureVideoDataOutputSampleBufferDelegate {
    let semaphore = DispatchSemaphore(value: 0)
    func captureOutput(_ output: AVCaptureOutput,
                       didOutput sampleBuffer: CMSampleBuffer,
                       from connection: AVCaptureConnection) {
        semaphore.signal()
    }
}
let uid = CommandLine.arguments[1]
guard let device = AVCaptureDevice(uniqueID: uid) else {
    print("no-device")
    exit(0)
}
guard device.isConnected else {
    print("disconnected")
    exit(0)
}
let session = AVCaptureSession()
guard let input = try? AVCaptureDeviceInput(device: device) else {
    print("no-input")
    exit(0)
}
let output = AVCaptureVideoDataOutput()
let probe = FrameProbe()
output.setSampleBufferDelegate(probe, queue: DispatchQueue(label: "seeed-hal.avfoundation.probe"))
guard session.canAddInput(input), session.canAddOutput(output) else {
    print("cannot-add")
    exit(0)
}
session.addInput(input)
session.addOutput(output)
session.startRunning()
let running = session.isRunning &&
    probe.semaphore.wait(timeout: .now() + 2) == .success
session.stopRunning()
print(running ? "frame-ready" : "not-frame-ready")
' "$uid" 2>/dev/null || echo "probe-error")
      echo "  -> ${result}"
      if [[ "$result" == "frame-ready" ]]; then
        selected="$uid"
        break
      fi
    done

    if [[ -z "$selected" ]]; then
      echo "error: all matching cameras failed the running probe; check USB connections" >&2
      echo "candidates:" >&2
      printf '  camera:avfoundation:%s\n' "${matches[@]}" >&2
      echo "hint: set SEEED_HAL_CAMERA_RESOURCE_ID to the correct unique ID" >&2
      exit 1
    fi

    echo "auto-selected: ${selected}"
    export SEEED_HAL_CAMERA_RESOURCE_ID="camera:avfoundation:${selected}"
  fi
fi

echo "fixture resource id: ${SEEED_HAL_CAMERA_RESOURCE_ID}"
echo
echo "Operator steps:"
echo "  1. When you see UNPLUG, disconnect only the selected USB camera (not the whole hub)."
echo "  2. Wait for camera.session.unplugged and discovery to drop the device."
echo "  3. When you see RECONNECT, plug the camera back in."
echo "  4. The test re-enumerates, reopens, and captures one frame."
echo

cargo +1.85 test -p seeed-hal-adapter-avfoundation --features hardware-tests \
  "$test_name" -- --ignored --nocapture
