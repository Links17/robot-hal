from __future__ import annotations

import errno
import hashlib
import json
import subprocess
import sys
import tarfile
import threading
import zipfile
from pathlib import Path

import pytest


REPO_ROOT = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(REPO_ROOT))

from scripts.release import release_tool
from scripts.release.release_tool import ReleaseFailure, package_broker, validate_archive


RELEASE_TOOL = REPO_ROOT / "scripts" / "release" / "release_tool.py"


def _target(name: str) -> tuple[str, tuple[str, ...], tuple[str, ...]]:
    values = {
        "macos": (
            "aarch64-apple-darwin",
            ("serialport", "nusb", "avfoundation"),
            ("avfoundation", "nusb", "serialport"),
        ),
        "linux": (
            "x86_64-unknown-linux-gnu",
            ("serialport", "nusb", "socketcan", "linux-gpio", "v4l2"),
            ("linux-gpio", "nusb", "serialport", "socketcan", "v4l2"),
        ),
        "windows": (
            "x86_64-pc-windows-msvc",
            ("serialport", "nusb", "windows-gpio", "mediafoundation"),
            ("mediafoundation", "nusb", "serialport", "windows-gpio"),
        ),
    }
    return values[name]


def _write_fixture_manifest(target: str, binary: Path, path: Path) -> None:
    triple, features, adapters = _target(target)
    os_name, arch = {
        "macos": ("macos", "aarch64"),
        "linux": ("linux", "x86_64"),
        "windows": ("windows", "x86_64"),
    }[target]
    path.write_text(
        json.dumps(
            {
                "schema": {"major": 1},
                "broker_version": "0.5.0-rc.1",
                "wire": {"major": 1, "minimum_minor": 0, "maximum_minor": 3},
                "target": {"triple": triple, "os": os_name, "arch": arch},
                "enabled": {
                    "adapters": list(adapters),
                    "features": sorted(features),
                },
                "msrv": "1.85",
                "artifact_checksum": {
                    "algorithm": "sha256",
                    "value": hashlib.sha256(binary.read_bytes()).hexdigest(),
                },
                "required_vendor_runtime_libraries": [],
            }
        ),
        encoding="utf-8",
    )


def _fixture_repo(tmp_path: Path) -> tuple[Path, Path]:
    repo = tmp_path / "repo"
    repo.mkdir(parents=True)
    (repo / "LICENSE").write_text("Apache-2.0 fixture\n", encoding="utf-8")
    (repo / "README.md").write_text("# Fixture broker\n", encoding="utf-8")
    targets = repo / "targets.toml"
    targets.write_text((REPO_ROOT / "release" / "targets.toml").read_text(), encoding="utf-8")
    return repo, targets


def package_fixture_broker(tmp_path: Path, target: str) -> Path:
    repo, targets = _fixture_repo(tmp_path)
    binary = tmp_path / ("seeed-hal-broker.exe" if target == "windows" else "seeed-hal-broker")
    binary.write_bytes(b"broker fixture\n")
    manifest = tmp_path / "broker-manifest.json"
    _write_fixture_manifest(target, binary, manifest)
    output = tmp_path / "output"
    return package_broker(
        tag="v0.5.0-rc.1",
        target_name=target,
        targets_path=targets,
        binary_path=binary,
        output_dir=output,
        manifest_path=manifest,
        repo_root=repo,
    )


def archive_members(archive: Path) -> tuple[str, ...]:
    if archive.suffix == ".zip":
        with zipfile.ZipFile(archive) as contents:
            return tuple(member.filename for member in contents.infolist())
    with tarfile.open(archive, "r:gz") as contents:
        return tuple(member.name for member in contents.getmembers())


def test_broker_archive_has_exact_files(tmp_path: Path) -> None:
    archive = package_fixture_broker(tmp_path, target="linux")

    assert archive_members(archive) == (
        "seeed-hal-broker-v0.5.0-rc.1-x86_64-unknown-linux-gnu",
        "seeed-hal-broker-v0.5.0-rc.1-x86_64-unknown-linux-gnu/LICENSE",
        "seeed-hal-broker-v0.5.0-rc.1-x86_64-unknown-linux-gnu/README.md",
        "seeed-hal-broker-v0.5.0-rc.1-x86_64-unknown-linux-gnu/broker-manifest.json",
        "seeed-hal-broker-v0.5.0-rc.1-x86_64-unknown-linux-gnu/seeed-hal-broker",
    )


@pytest.mark.parametrize("target", ["macos", "windows"])
def test_broker_archive_uses_target_matrix_format(
    tmp_path: Path,
    target: str,
) -> None:
    archive = package_fixture_broker(tmp_path, target)

    assert archive.suffix == (".zip" if target == "windows" else ".gz")
    expected_binary = "seeed-hal-broker.exe" if target == "windows" else "seeed-hal-broker"
    assert archive_members(archive)[-1] == (
        f"seeed-hal-broker-v0.5.0-rc.1-{_target(target)[0]}/{expected_binary}"
    )


@pytest.mark.parametrize("target", ["linux", "windows"])
def test_broker_archive_is_byte_deterministic(tmp_path: Path, target: str) -> None:
    first = package_fixture_broker(tmp_path / "first", target)
    second = package_fixture_broker(tmp_path / "second", target)

    assert first.read_bytes() == second.read_bytes()


def test_broker_package_rejects_manifest_target_mismatch(tmp_path: Path) -> None:
    repo, targets = _fixture_repo(tmp_path)
    binary = tmp_path / "seeed-hal-broker"
    binary.write_bytes(b"broker fixture\n")
    manifest = tmp_path / "broker-manifest.json"
    _write_fixture_manifest("macos", binary, manifest)

    with pytest.raises(ReleaseFailure) as failure:
        package_broker(
            tag="v0.5.0-rc.1",
            target_name="linux",
            targets_path=targets,
            binary_path=binary,
            output_dir=tmp_path / "output",
            manifest_path=manifest,
            repo_root=repo,
        )

    assert failure.value.name == "release.manifest.invalid"


def test_broker_package_diagnostic_does_not_echo_manifest_path(tmp_path: Path) -> None:
    repo, targets = _fixture_repo(tmp_path)
    binary = tmp_path / "seeed-hal-broker"
    binary.write_bytes(b"broker fixture\n")
    missing_manifest = tmp_path / "secret-endpoint-manifest.json"

    with pytest.raises(ReleaseFailure) as failure:
        package_broker(
            tag="v0.5.0-rc.1",
            target_name="macos",
            targets_path=targets,
            binary_path=binary,
            output_dir=tmp_path / "output",
            manifest_path=missing_manifest,
            repo_root=repo,
        )

    assert failure.value.name == "release.package.invalid"
    assert "secret" not in failure.value.diagnostic


def test_validation_failure_leaves_no_final_archive(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    repo, targets = _fixture_repo(tmp_path)
    binary = tmp_path / "seeed-hal-broker"
    binary.write_bytes(b"broker fixture\n")
    manifest = tmp_path / "broker-manifest.json"
    _write_fixture_manifest("macos", binary, manifest)
    output = tmp_path / "output"

    def reject_archive(*args, **kwargs) -> None:
        raise ReleaseFailure("release.archive.invalid", "fixture validation failure")

    monkeypatch.setattr(release_tool, "validate_archive", reject_archive)

    with pytest.raises(ReleaseFailure) as failure:
        package_broker(
            tag="v0.5.0-rc.1",
            target_name="macos",
            targets_path=targets,
            binary_path=binary,
            output_dir=output,
            manifest_path=manifest,
            repo_root=repo,
        )

    assert failure.value.name == "release.archive.invalid"
    assert tuple(output.iterdir()) == ()


def test_existing_final_archive_is_never_overwritten(tmp_path: Path) -> None:
    repo, targets = _fixture_repo(tmp_path)
    binary = tmp_path / "seeed-hal-broker"
    binary.write_bytes(b"broker fixture\n")
    manifest = tmp_path / "broker-manifest.json"
    _write_fixture_manifest("macos", binary, manifest)
    output = tmp_path / "output"
    output.mkdir()
    archive = output / "seeed-hal-broker-v0.5.0-rc.1-aarch64-apple-darwin.tar.gz"
    archive.write_bytes(b"pre-existing archive")

    with pytest.raises(ReleaseFailure) as failure:
        package_broker(
            tag="v0.5.0-rc.1",
            target_name="macos",
            targets_path=targets,
            binary_path=binary,
            output_dir=output,
            manifest_path=manifest,
            repo_root=repo,
        )

    assert failure.value.name == "release.artifact.unexpected"
    assert archive.read_bytes() == b"pre-existing archive"
    assert {path.name for path in output.iterdir()} == {archive.name}


def test_packaging_uses_frozen_staged_input_copies(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    repo, targets = _fixture_repo(tmp_path)
    binary = tmp_path / "seeed-hal-broker"
    binary.write_bytes(b"broker fixture\n")
    manifest = tmp_path / "broker-manifest.json"
    _write_fixture_manifest("macos", binary, manifest)
    original_manifest = manifest.read_bytes()
    observed: list[Path] = []
    original_writer = release_tool._write_deterministic_tar

    def mutate_sources_after_staging(
        archive_path: Path,
        root: str,
        members: tuple[tuple[str, Path, int], ...],
    ) -> None:
        observed.extend(path for _, path, _ in members)
        binary.write_bytes(b"mutated binary\n")
        manifest.write_text("{}", encoding="utf-8")
        (repo / "LICENSE").write_text("mutated license\n", encoding="utf-8")
        (repo / "README.md").write_text("mutated readme\n", encoding="utf-8")
        original_writer(archive_path, root, members)

    monkeypatch.setattr(release_tool, "_write_deterministic_tar", mutate_sources_after_staging)

    archive = package_broker(
        tag="v0.5.0-rc.1",
        target_name="macos",
        targets_path=targets,
        binary_path=binary,
        output_dir=tmp_path / "output",
        manifest_path=manifest,
        repo_root=repo,
    )

    assert all(path.parent.name == "inputs" for path in observed)
    with tarfile.open(archive, "r:gz") as contents:
        assert contents.extractfile(
            "seeed-hal-broker-v0.5.0-rc.1-aarch64-apple-darwin/seeed-hal-broker"
        ).read() == b"broker fixture\n"
        assert contents.extractfile(
            "seeed-hal-broker-v0.5.0-rc.1-aarch64-apple-darwin/broker-manifest.json"
        ).read() == original_manifest
    assert not any(path.name.startswith(".package-broker-") for path in archive.parent.iterdir())


def test_concurrent_publish_reserves_final_archive_without_overwrite(
    tmp_path: Path,
) -> None:
    repo, targets = _fixture_repo(tmp_path)
    output = tmp_path / "output"
    results: list[Path | ReleaseFailure] = []

    def publish(label: str) -> None:
        source = tmp_path / label
        source.mkdir()
        binary = source / "seeed-hal-broker"
        binary.write_bytes(f"broker fixture {label}\n".encode())
        manifest = source / "broker-manifest.json"
        _write_fixture_manifest("macos", binary, manifest)
        try:
            results.append(
                package_broker(
                    tag="v0.5.0-rc.1",
                    target_name="macos",
                    targets_path=targets,
                    binary_path=binary,
                    output_dir=output,
                    manifest_path=manifest,
                    repo_root=repo,
                )
            )
        except ReleaseFailure as error:
            results.append(error)

    threads = [threading.Thread(target=publish, args=(label,)) for label in ("one", "two")]
    for thread in threads:
        thread.start()
    for thread in threads:
        thread.join(timeout=10)

    assert all(not thread.is_alive() for thread in threads)
    successes = [result for result in results if isinstance(result, Path)]
    failures = [result for result in results if isinstance(result, ReleaseFailure)]
    assert len(successes) == 1
    assert [failure.name for failure in failures] == ["release.artifact.unexpected"]
    archive = successes[0]
    validate_archive(
        archive,
        expected_root="seeed-hal-broker-v0.5.0-rc.1-aarch64-apple-darwin",
        expected_files={"LICENSE", "README.md", "broker-manifest.json", "seeed-hal-broker"},
    )
    assert not any(path.name.startswith(".package-broker-") for path in output.iterdir())
    assert not any(path.name.startswith(".reserve-broker-") for path in output.iterdir())


def test_existing_reservation_fails_closed_without_removal(tmp_path: Path) -> None:
    repo, targets = _fixture_repo(tmp_path)
    binary = tmp_path / "seeed-hal-broker"
    binary.write_bytes(b"broker fixture\n")
    manifest = tmp_path / "broker-manifest.json"
    _write_fixture_manifest("macos", binary, manifest)
    output = tmp_path / "output"
    output.mkdir()
    reservation = output / (
        ".reserve-broker-seeed-hal-broker-v0.5.0-rc.1-aarch64-apple-darwin.tar.gz"
    )
    reservation.write_bytes(b"unknown publisher reservation")

    with pytest.raises(ReleaseFailure) as failure:
        package_broker(
            tag="v0.5.0-rc.1",
            target_name="macos",
            targets_path=targets,
            binary_path=binary,
            output_dir=output,
            manifest_path=manifest,
            repo_root=repo,
        )

    assert failure.value.name == "release.artifact.unexpected"
    assert reservation.read_bytes() == b"unknown publisher reservation"


def test_external_final_after_reservation_is_not_overwritten(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    repo, targets = _fixture_repo(tmp_path)
    binary = tmp_path / "seeed-hal-broker"
    binary.write_bytes(b"broker fixture\n")
    manifest = tmp_path / "broker-manifest.json"
    _write_fixture_manifest("macos", binary, manifest)
    output = tmp_path / "output"
    external_bytes = b"external publisher archive"
    original_mkdtemp = release_tool.tempfile.mkdtemp

    def stage_then_publish_external_final(*args, **kwargs) -> str:
        staging = original_mkdtemp(*args, **kwargs)
        (output / "seeed-hal-broker-v0.5.0-rc.1-aarch64-apple-darwin.tar.gz").write_bytes(
            external_bytes
        )
        return staging

    monkeypatch.setattr(release_tool.tempfile, "mkdtemp", stage_then_publish_external_final)

    with pytest.raises(ReleaseFailure) as failure:
        package_broker(
            tag="v0.5.0-rc.1",
            target_name="macos",
            targets_path=targets,
            binary_path=binary,
            output_dir=output,
            manifest_path=manifest,
            repo_root=repo,
        )

    assert failure.value.name == "release.artifact.unexpected"
    archive = output / "seeed-hal-broker-v0.5.0-rc.1-aarch64-apple-darwin.tar.gz"
    assert archive.read_bytes() == external_bytes


def test_link_publish_rejects_external_final_without_overwrite(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    repo, targets = _fixture_repo(tmp_path)
    binary = tmp_path / "seeed-hal-broker"
    binary.write_bytes(b"broker fixture\n")
    manifest = tmp_path / "broker-manifest.json"
    _write_fixture_manifest("macos", binary, manifest)
    output = tmp_path / "output"
    external_bytes = b"external final archive"

    def inject_external_final(source: Path, destination: Path) -> None:
        destination.write_bytes(external_bytes)
        raise FileExistsError(errno.EEXIST, "destination exists", str(destination))

    monkeypatch.setattr(release_tool.os, "link", inject_external_final)

    with pytest.raises(ReleaseFailure) as failure:
        package_broker(
            tag="v0.5.0-rc.1",
            target_name="macos",
            targets_path=targets,
            binary_path=binary,
            output_dir=output,
            manifest_path=manifest,
            repo_root=repo,
        )

    assert failure.value.name == "release.artifact.unexpected"
    archive = output / "seeed-hal-broker-v0.5.0-rc.1-aarch64-apple-darwin.tar.gz"
    assert archive.read_bytes() == external_bytes


@pytest.mark.parametrize("error_number", [errno.EXDEV, errno.EPERM])
def test_link_publish_fails_closed_when_unsupported(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
    error_number: int,
) -> None:
    repo, targets = _fixture_repo(tmp_path)
    binary = tmp_path / "seeed-hal-broker"
    binary.write_bytes(b"broker fixture\n")
    manifest = tmp_path / "broker-manifest.json"
    _write_fixture_manifest("macos", binary, manifest)
    output = tmp_path / "output"

    def unsupported_link(source: Path, destination: Path) -> None:
        raise OSError(error_number, "link unavailable")

    monkeypatch.setattr(release_tool.os, "link", unsupported_link)

    with pytest.raises(ReleaseFailure) as failure:
        package_broker(
            tag="v0.5.0-rc.1",
            target_name="macos",
            targets_path=targets,
            binary_path=binary,
            output_dir=output,
            manifest_path=manifest,
            repo_root=repo,
        )

    assert failure.value.name == "release.package.invalid"
    assert not output.exists() or not any(output.iterdir())


def test_cli_rejects_missing_tag_with_stable_failure(tmp_path: Path) -> None:
    result = subprocess.run(
        [sys.executable, str(RELEASE_TOOL), "package-broker"],
        cwd=REPO_ROOT,
        check=False,
        capture_output=True,
        text=True,
        timeout=10,
    )

    assert result.returncode == 1
    assert result.stderr.startswith("release.tool.invalid:")
    assert "Traceback" not in result.stderr
