import json
import subprocess
import sys
from pathlib import Path

import pytest

REPO_ROOT = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(REPO_ROOT))

from scripts.release.release_tool import ReleaseFailure, load_targets


TARGETS = REPO_ROOT / "release" / "targets.toml"
VALID_TARGETS = TARGETS.read_text(encoding="utf-8")
RELEASE_TOOL = REPO_ROOT / "scripts" / "release" / "release_tool.py"


def _load_text(tmp_path: Path, contents: str) -> None:
    path = tmp_path / "targets.toml"
    path.write_text(contents, encoding="utf-8")
    with pytest.raises(ReleaseFailure, match="release.targets.invalid"):
        load_targets(path)


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


def test_target_matrix_rejects_extra_target(tmp_path: Path) -> None:
    _load_text(
        tmp_path,
        VALID_TARGETS
        + """

[[target]]
name = "freebsd"
runner = "ubuntu-24.04"
triple = "x86_64-unknown-freebsd"
archive = "tar.gz"
features = ["serialport"]
required_adapters = ["serialport"]
""",
    )


def test_target_matrix_rejects_duplicate_target_name(tmp_path: Path) -> None:
    _load_text(
        tmp_path,
        VALID_TARGETS.replace('name = "linux"', 'name = "macos"', 1),
    )


@pytest.mark.parametrize(
    "contents",
    [
        VALID_TARGETS.replace(
            'features = ["serialport", "nusb", "socketcan", "linux-gpio", "v4l2"]',
            'features = ["serialport", "nusb", "socketcan", "pcan", "linux-gpio", "v4l2"]',
        ),
        VALID_TARGETS.replace(
            'triple = "x86_64-unknown-linux-gnu"',
            'triple = "aarch64-unknown-linux-gnu"',
        ),
    ],
)
def test_target_matrix_rejects_pcan_or_wrong_triple(
    tmp_path: Path,
    contents: str,
) -> None:
    _load_text(tmp_path, contents)


@pytest.mark.parametrize(
    "contents",
    [
        VALID_TARGETS.replace(
            'features = ["serialport", "nusb", "avfoundation"]',
            'features = "serialport"',
        ),
        VALID_TARGETS.replace('runner = "macos-14"', "runner = 14"),
        VALID_TARGETS.replace(
            'required_adapters = ["avfoundation", "nusb", "serialport"]',
            'required_adapters = ["avfoundation", 7, "serialport"]',
        ),
        VALID_TARGETS.replace(
            'archive = "tar.gz"',
            'archive = "tar.gz"\nunknown = true',
            1,
        ),
    ],
)
def test_target_matrix_rejects_invalid_types_or_unknown_fields(
    tmp_path: Path,
    contents: str,
) -> None:
    _load_text(tmp_path, contents)


def test_target_matrix_rejects_non_utf8_input(tmp_path: Path) -> None:
    path = tmp_path / "targets.toml"
    path.write_bytes(b"schema = 1\n\xff")

    with pytest.raises(ReleaseFailure, match="release.targets.invalid"):
        load_targets(path)


def test_print_target_emits_a_machine_readable_github_matrix() -> None:
    result = subprocess.run(
        [
            sys.executable,
            str(RELEASE_TOOL),
            "print-target",
            "--targets",
            str(TARGETS),
            "--format",
            "github-matrix",
        ],
        cwd=REPO_ROOT,
        check=False,
        capture_output=True,
        text=True,
        timeout=10,
    )

    assert result.returncode == 0
    assert result.stderr == ""
    assert json.loads(result.stdout) == {
        "include": [
            {
                "name": "macos",
                "runner": "macos-14",
                "features": "serialport,nusb,avfoundation",
            },
            {
                "name": "linux",
                "runner": "ubuntu-24.04",
                "features": "serialport,nusb,socketcan,linux-gpio,v4l2",
            },
            {
                "name": "windows",
                "runner": "windows-2025",
                "features": "serialport,nusb,windows-gpio,mediafoundation",
            },
        ]
    }
