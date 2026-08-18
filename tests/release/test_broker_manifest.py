from __future__ import annotations

import hashlib
import json
import subprocess
import sys
from pathlib import Path

import pytest


REPO_ROOT = Path(__file__).parents[2]
sys.path.insert(0, str(REPO_ROOT))

from scripts.release.release_tool import (
    ReleaseFailure,
    load_targets,
    verify_broker_manifest,
)


RELEASE_TOOL_PATH = REPO_ROOT / "scripts" / "release" / "release_tool.py"
TARGETS_PATH = REPO_ROOT / "release" / "targets.toml"


TARGET_PLATFORM = {
    "macos": ("macos", "aarch64"),
    "linux": ("linux", "x86_64"),
    "windows": ("windows", "x86_64"),
}


@pytest.fixture
def artifact(tmp_path: Path) -> Path:
    path = tmp_path / "seeed-hal-broker"
    path.write_bytes(b"broker artifact fixture\n")
    return path


def target(name: str):
    return next(item for item in load_targets(TARGETS_PATH) if item.name == name)


def valid_manifest(name: str, artifact: Path) -> dict[str, object]:
    release_target = target(name)
    os_name, arch = TARGET_PLATFORM[name]
    return {
        "broker_version": "0.5.0-rc.1",
        "wire": {
            "major": 1,
            "minimum_minor": 0,
            "maximum_minor": 3,
        },
        "target": {
            "triple": release_target.triple,
            "os": os_name,
            "arch": arch,
        },
        "enabled": {
            "adapters": list(release_target.required_adapters),
            "features": sorted(release_target.features),
        },
        "msrv": "1.85",
        "artifact_checksum": {
            "algorithm": "sha256",
            "value": hashlib.sha256(artifact.read_bytes()).hexdigest(),
        },
        "required_vendor_runtime_libraries": [],
    }


@pytest.mark.parametrize("name", ["macos", "linux", "windows"])
def test_manifest_accepts_exact_target_composition(name: str, artifact: Path) -> None:
    verify_broker_manifest(valid_manifest(name, artifact), target(name), artifact)


def test_manifest_accepts_additive_top_level_field(artifact: Path) -> None:
    manifest = valid_manifest("macos", artifact)
    manifest["future_field"] = {"safe": True}

    verify_broker_manifest(manifest, target("macos"), artifact)


@pytest.mark.parametrize(
    ("name", "mutate"),
    [
        ("linux", lambda manifest: manifest["enabled"]["adapters"].remove("v4l2")),
        ("windows", lambda manifest: manifest["enabled"]["adapters"].append("pcan")),
        ("macos", lambda manifest: manifest.update(broker_version="0.5.0-rc.2")),
        ("linux", lambda manifest: manifest["wire"].update(maximum_minor=2)),
        ("windows", lambda manifest: manifest["target"].update(triple="x86_64-pc-windows-gnu")),
        ("macos", lambda manifest: manifest["target"].update(os="linux")),
        ("linux", lambda manifest: manifest["target"].update(arch="aarch64")),
        ("windows", lambda manifest: manifest.update(msrv="1.86")),
        ("macos", lambda manifest: manifest["enabled"]["features"].append("pcan")),
        (
            "linux",
            lambda manifest: manifest.update(
                required_vendor_runtime_libraries=["libpcanbasic.so"]
            ),
        ),
        ("windows", lambda manifest: manifest["artifact_checksum"].update(algorithm="md5")),
        ("macos", lambda manifest: manifest["artifact_checksum"].update(value="0" * 64)),
    ],
)
def test_manifest_rejects_one_field_mutations(
    name: str,
    mutate,
    artifact: Path,
) -> None:
    manifest = valid_manifest(name, artifact)
    mutate(manifest)

    with pytest.raises(ReleaseFailure) as failure:
        verify_broker_manifest(manifest, target(name), artifact)

    assert failure.value.name == "release.manifest.invalid"


@pytest.mark.parametrize(
    "mutate",
    [
        lambda manifest: manifest.pop("enabled"),
        lambda manifest: manifest.update(wire=[]),
        lambda manifest: manifest["enabled"].update(features=["serialport", 1]),
        lambda manifest: manifest["artifact_checksum"].pop("value"),
    ],
)
def test_manifest_rejects_missing_or_mistyped_fields(mutate, artifact: Path) -> None:
    manifest = valid_manifest("macos", artifact)
    mutate(manifest)

    with pytest.raises(ReleaseFailure) as failure:
        verify_broker_manifest(manifest, target("macos"), artifact)

    assert failure.value.name == "release.manifest.invalid"


def test_manifest_rejects_unreadable_artifact(artifact: Path) -> None:
    manifest = valid_manifest("macos", artifact)
    artifact.unlink()

    with pytest.raises(ReleaseFailure) as failure:
        verify_broker_manifest(manifest, target("macos"), artifact)

    assert failure.value.name == "release.manifest.invalid"


@pytest.mark.parametrize(
    ("contents", "expected_name"),
    [
        ("{", "release.manifest.invalid"),
        ("[]", "release.manifest.invalid"),
    ],
)
def test_cli_maps_invalid_json_without_traceback(
    contents: str,
    expected_name: str,
    artifact: Path,
    tmp_path: Path,
) -> None:
    manifest_path = tmp_path / "manifest.json"
    manifest_path.write_text(contents, encoding="utf-8")

    result = subprocess.run(
        [
            sys.executable,
            str(RELEASE_TOOL_PATH),
            "verify-broker-manifest",
            "--manifest",
            str(manifest_path),
            "--target",
            "macos",
            "--targets",
            str(TARGETS_PATH),
            "--artifact",
            str(artifact),
        ],
        cwd=REPO_ROOT,
        check=False,
        capture_output=True,
        text=True,
        timeout=10,
    )

    assert result.returncode == 1
    assert result.stderr.startswith(f"{expected_name}:")
    assert "Traceback" not in result.stderr


def test_cli_rejects_unknown_target_without_traceback(
    artifact: Path,
    tmp_path: Path,
) -> None:
    manifest_path = tmp_path / "manifest.json"
    manifest_path.write_text(
        json.dumps(valid_manifest("macos", artifact)),
        encoding="utf-8",
    )

    result = subprocess.run(
        [
            sys.executable,
            str(RELEASE_TOOL_PATH),
            "verify-broker-manifest",
            "--manifest",
            str(manifest_path),
            "--target",
            "freebsd",
            "--targets",
            str(TARGETS_PATH),
            "--artifact",
            str(artifact),
        ],
        cwd=REPO_ROOT,
        check=False,
        capture_output=True,
        text=True,
        timeout=10,
    )

    assert result.returncode == 1
    assert result.stderr.startswith("release.target.invalid:")
    assert "Traceback" not in result.stderr
