import sys
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(REPO_ROOT))

from scripts.release.release_tool import load_targets


TARGETS = REPO_ROOT / "release" / "targets.toml"


def test_target_matrix_has_exact_default_compositions() -> None:
    targets = {target.name: target for target in load_targets(TARGETS)}
    assert targets["macos"].required_adapters == (
        "avfoundation",
        "nusb",
        "serialport",
    )
    assert targets["linux"].required_adapters == (
        "linux-gpio",
        "nusb",
        "serialport",
        "socketcan",
        "v4l2",
    )
    assert targets["windows"].required_adapters == (
        "mediafoundation",
        "nusb",
        "serialport",
        "windows-gpio",
    )
