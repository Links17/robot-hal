from __future__ import annotations

import hashlib
import io
import json
import stat
import sys
import tarfile
import zipfile
from pathlib import Path

import pytest


REPO_ROOT = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(REPO_ROOT))

from scripts.release.release_tool import (
    ReleaseFailure,
    aggregate_release,
    verify_artifacts,
    verify_static,
)


TAG = "v0.5.0-rc.1"
COMMIT = "a" * 40
TARGETS = REPO_ROOT / "release" / "targets.toml"


def _broker_manifest(target: str, binary: bytes) -> bytes:
    values = {
        "macos": (
            "aarch64-apple-darwin",
            "macos",
            "aarch64",
            ["avfoundation", "nusb", "serialport"],
            ["avfoundation", "nusb", "serialport"],
        ),
        "linux": (
            "x86_64-unknown-linux-gnu",
            "linux",
            "x86_64",
            ["linux-gpio", "nusb", "serialport", "socketcan", "v4l2"],
            ["linux-gpio", "nusb", "serialport", "socketcan", "v4l2"],
        ),
        "windows": (
            "x86_64-pc-windows-msvc",
            "windows",
            "x86_64",
            ["mediafoundation", "nusb", "serialport", "windows-gpio"],
            ["mediafoundation", "nusb", "serialport", "windows-gpio"],
        ),
    }[target]
    triple, os_name, arch, adapters, features = values
    return json.dumps(
        {
            "schema": {"major": 1},
            "broker_version": "0.5.0-rc.1",
            "wire": {"major": 1, "minimum_minor": 0, "maximum_minor": 3},
            "target": {"triple": triple, "os": os_name, "arch": arch},
            "enabled": {"adapters": adapters, "features": features},
            "msrv": "1.85",
            "artifact_checksum": {
                "algorithm": "sha256",
                "value": hashlib.sha256(binary).hexdigest(),
            },
            "required_vendor_runtime_libraries": [],
        },
        sort_keys=True,
    ).encode()


def _broker_binary() -> bytes:
    return b"#!/bin/sh\nif [ \"$1\" = \"--manifest\" ]; then cat broker-manifest.json; exit 0; fi\nexit 1\n"


def _write_broker_candidate(directory: Path, target: str) -> Path:
    triple = {
        "macos": "aarch64-apple-darwin",
        "linux": "x86_64-unknown-linux-gnu",
        "windows": "x86_64-pc-windows-msvc",
    }[target]
    extension = "zip" if target == "windows" else "tar.gz"
    archive = directory / f"seeed-hal-broker-v0.5.0-rc.1-{triple}.{extension}"
    root = f"seeed-hal-broker-v0.5.0-rc.1-{triple}"
    binary_name = "seeed-hal-broker.exe" if target == "windows" else "seeed-hal-broker"
    binary = _broker_binary()
    manifest = _broker_manifest(target, binary)
    entries = {
        f"{root}/LICENSE": b"fixture license\n",
        f"{root}/README.md": b"fixture readme\n",
        f"{root}/broker-manifest.json": manifest,
        f"{root}/{binary_name}": binary,
    }
    if target == "windows":
        with zipfile.ZipFile(archive, "w") as contents:
            contents.writestr(f"{root}/", b"")
            for name, value in entries.items():
                contents.writestr(name, value)
    else:
        with tarfile.open(archive, "w:gz") as contents:
            root_info = tarfile.TarInfo(root)
            root_info.type = tarfile.DIRTYPE
            contents.addfile(root_info)
            for name, value in entries.items():
                info = tarfile.TarInfo(name)
                info.size = len(value)
                info.mode = 0o755 if name.endswith(binary_name) else 0o644
                contents.addfile(info, io.BytesIO(value))
    return archive


def _write_source_candidate(directory: Path) -> Path:
    directory.mkdir()
    artifact = directory / "seeed-hal-crates-v0.5.0-rc.1.tar.gz"
    artifact.write_bytes(b"rust bundle fixture\n")
    return directory


def _write_python_candidate(directory: Path) -> Path:
    directory.mkdir()
    (directory / "seeed_hal-0.5.0rc1-py3-none-any.whl").write_bytes(b"wheel\n")
    (directory / "seeed_hal-0.5.0rc1.tar.gz").write_bytes(b"sdist\n")
    return directory


def _write_report_inputs(directory: Path) -> Path:
    directory.mkdir()
    report = {
        "schema": 1,
        "tag": TAG,
        "commit": COMMIT,
        "qualification": {
            "software": {
                "id": "software-conformance",
                "uri": "https://example.invalid/software",
            },
            "hardware": {
                "id": "hardware-qualification",
                "uri": "https://example.invalid/hardware",
            },
        },
        "software": {
            "status": "Partial",
            "jobs": [
                {
                    "platform": "macos",
                    "result": "Passed",
                    "command": "verify-artifacts --tag v0.5.0-rc.1",
                    "ref": "https://example.invalid/jobs/macos",
                }
            ],
        },
        "hardware": {
            "camera-avfoundation": {"status": "Pending", "evidence": None},
            "camera-v4l2": {"status": "Blocked", "evidence": None},
        },
    }
    (directory / "conformance-report.json").write_text(json.dumps(report), encoding="utf-8")
    return directory


def _complete_release(tmp_path: Path) -> Path:
    inputs = tmp_path / "inputs"
    inputs.mkdir()
    brokers = inputs / "brokers"
    brokers.mkdir()
    for target in ("macos", "linux", "windows"):
        _write_broker_candidate(brokers, target)
    rust = _write_source_candidate(inputs / "rust")
    python = _write_python_candidate(inputs / "python")
    report = _write_report_inputs(inputs / "report")
    release = tmp_path / "release"
    aggregate_release(
        tag=TAG,
        commit=COMMIT,
        broker_dir=brokers,
        rust_bundle=rust,
        python_candidate=python,
        report_inputs=report,
        release_dir=release,
    )
    return release


def test_aggregate_requires_fresh_private_release_directory(tmp_path: Path) -> None:
    release = tmp_path / "release"
    release.mkdir()

    with pytest.raises(ReleaseFailure, match="release.artifact.unexpected"):
        aggregate_release(
            tag=TAG,
            commit=COMMIT,
            broker_dir=tmp_path / "missing",
            rust_bundle=tmp_path / "missing",
            python_candidate=tmp_path / "missing",
            report_inputs=tmp_path / "missing",
            release_dir=release,
        )


def test_aggregate_creates_complete_mode_0700_release_directory(tmp_path: Path) -> None:
    release = _complete_release(tmp_path)

    assert stat.S_IMODE(release.stat().st_mode) == 0o700
    verify_static(release)


def test_verify_rejects_partial_platform_set(tmp_path: Path) -> None:
    release = _complete_release(tmp_path)
    (
        release / "seeed-hal-broker-v0.5.0-rc.1-x86_64-pc-windows-msvc.zip"
    ).unlink()

    with pytest.raises(ReleaseFailure, match="release.artifact.unexpected"):
        verify_artifacts(release, TAG, TARGETS, REPO_ROOT)


def test_aggregate_rejects_external_candidate_mutation_before_publish(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    inputs = tmp_path / "inputs"
    inputs.mkdir()
    brokers = inputs / "brokers"
    brokers.mkdir()
    for target in ("macos", "linux", "windows"):
        _write_broker_candidate(brokers, target)
    rust = _write_source_candidate(inputs / "rust")
    python = _write_python_candidate(inputs / "python")
    report = _write_report_inputs(inputs / "report")

    original_copy = __import__("scripts.release.release_tool", fromlist=["_copy_frozen_artifact"])._copy_frozen_artifact

    def mutate_after_first_copy(*args, **kwargs):
        result = original_copy(*args, **kwargs)
        (python / "external").write_bytes(b"external write")
        return result

    monkeypatch.setattr(
        "scripts.release.release_tool._copy_frozen_artifact",
        mutate_after_first_copy,
    )

    with pytest.raises(ReleaseFailure, match="release.artifact.unexpected"):
        aggregate_release(
            tag=TAG,
            commit=COMMIT,
            broker_dir=brokers,
            rust_bundle=rust,
            python_candidate=python,
            report_inputs=report,
            release_dir=tmp_path / "release",
        )


def test_verify_static_release_directory_is_the_only_success_boundary(
    tmp_path: Path,
) -> None:
    release = _complete_release(tmp_path)

    verify_artifacts(release, TAG, TARGETS, REPO_ROOT)
