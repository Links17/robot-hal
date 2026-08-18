from __future__ import annotations

import sys
import errno
import subprocess
import zipfile
from pathlib import Path

import pytest

REPO_ROOT = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(REPO_ROOT))

from scripts.release import release_tool
from scripts.release.release_tool import (
    ReleaseFailure,
    ReleaseVersion,
    _verify_wheel_install,
    _wheel_metadata,
    python_artifact_names,
)


def test_python_artifact_names_use_pep440() -> None:
    assert python_artifact_names(ReleaseVersion.parse("v0.5.0-rc.3")) == (
        "seeed_hal-0.5.0rc3-py3-none-any.whl",
        "seeed_hal-0.5.0rc3.tar.gz",
    )


def test_python_package_reserves_both_artifacts_before_build(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    project = REPO_ROOT / "bindings" / "python"
    output = tmp_path / "artifacts"
    observed_build = False

    def should_not_build(*args, **kwargs) -> None:
        nonlocal observed_build
        observed_build = True
        raise AssertionError("build must not begin after reservation failure")

    monkeypatch.setattr(release_tool.subprocess, "run", should_not_build)
    output.mkdir()
    (output / "seeed_hal-0.5.0rc1.tar.gz").write_bytes(b"external sdist")

    with pytest.raises(ReleaseFailure) as failure:
        release_tool.package_python(
            tag="v0.5.0-rc.1",
            project=project,
            output_dir=output,
        )

    assert failure.value.name == "release.artifact.unexpected"
    assert not observed_build
    assert (output / "seeed_hal-0.5.0rc1.tar.gz").read_bytes() == b"external sdist"
    assert not (output / "seeed_hal-0.5.0rc1-py3-none-any.whl").exists()


def test_failed_second_reservation_cleans_only_our_wheel_reservation(
    tmp_path: Path,
) -> None:
    output = tmp_path / "artifacts"
    output.mkdir()
    sdist_reservation = output / ".reserve-package-seeed_hal-0.5.0rc1.tar.gz"
    sdist_reservation.write_bytes(b"external reservation")

    with pytest.raises(ReleaseFailure) as failure:
        release_tool.package_python(
            tag="v0.5.0-rc.1",
            project=REPO_ROOT / "bindings" / "python",
            output_dir=output,
        )

    assert failure.value.name == "release.artifact.unexpected"
    assert not (
        output / ".reserve-package-seeed_hal-0.5.0rc1-py3-none-any.whl"
    ).exists()
    assert sdist_reservation.read_bytes() == b"external reservation"


def test_python_package_rolls_back_wheel_when_sdist_link_fails(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    project = REPO_ROOT / "bindings" / "python"
    output = tmp_path / "artifacts"
    original_link = release_tool.os.link
    calls = 0

    def fail_second_link(source: Path, destination: Path) -> None:
        nonlocal calls
        calls += 1
        if calls == 2:
            destination.write_bytes(b"external sdist")
            raise FileExistsError(errno.EEXIST, "exists", str(destination))
        original_link(source, destination)

    monkeypatch.setattr(release_tool.os, "link", fail_second_link)

    with pytest.raises(ReleaseFailure) as failure:
        release_tool.package_python(
            tag="v0.5.0-rc.1",
            project=project,
            output_dir=output,
        )

    assert failure.value.name == "release.artifact.unexpected"
    assert not (output / "seeed_hal-0.5.0rc1-py3-none-any.whl").exists()
    assert (output / "seeed_hal-0.5.0rc1.tar.gz").read_bytes() == b"external sdist"


def test_python_package_publishes_the_complete_pair(tmp_path: Path) -> None:
    wheel, sdist = release_tool.package_python(
        tag="v0.5.0-rc.1",
        project=REPO_ROOT / "bindings" / "python",
        output_dir=tmp_path / "artifacts",
    )

    assert wheel.is_file()
    assert sdist.is_file()
    assert {path.name for path in wheel.parent.iterdir()} == {wheel.name, sdist.name}


def _wheel(
    tmp_path: Path,
    *,
    dist_info: str = "seeed_hal-0.5.0rc1.dist-info",
    wheel_metadata: str = "Wheel-Version: 1.0\nRoot-Is-Purelib: true\nTag: py3-none-any\n",
) -> Path:
    wheel = tmp_path / "seeed_hal-0.5.0rc1-py3-none-any.whl"
    with zipfile.ZipFile(wheel, "w") as archive:
        archive.writestr("seeed_hal/__init__.py", "")
        archive.writestr("seeed_hal/proto/__init__.py", "")
        archive.writestr("seeed_hal/proto/hal_pb2.py", "")
        archive.writestr(
            f"{dist_info}/METADATA",
            "Metadata-Version: 2.4\nName: seeed-hal\nVersion: 0.5.0rc1\n",
        )
        archive.writestr(f"{dist_info}/WHEEL", wheel_metadata)
    return wheel


@pytest.mark.parametrize(
    ("dist_info", "wheel_metadata"),
    [
        (
            "wrong-0.5.0rc1.dist-info",
            "Wheel-Version: 1.0\nRoot-Is-Purelib: true\nTag: py3-none-any\n",
        ),
        (
            "seeed_hal-0.5.0rc1.dist-info",
            "Wheel-Version: 1.0\nRoot-Is-Purelib: false\nTag: py3-none-any\n",
        ),
        (
            "seeed_hal-0.5.0rc1.dist-info",
            "Wheel-Version: 1.0\nRoot-Is-Purelib: true\nTag: py3-none-any\nTag: cp311-none-any\n",
        ),
    ],
)
def test_wheel_metadata_rejects_noncanonical_internal_identity(
    tmp_path: Path,
    dist_info: str,
    wheel_metadata: str,
) -> None:
    with pytest.raises(ReleaseFailure):
        _wheel_metadata(
            _wheel(tmp_path, dist_info=dist_info, wheel_metadata=wheel_metadata),
            ReleaseVersion.parse("v0.5.0-rc.1"),
        )


def test_isolated_wheel_venv_is_offline_and_uses_current_interpreter(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    commands: list[list[str]] = []
    environments: list[dict[str, str]] = []

    def record(command, **kwargs):
        commands.append(command)
        environments.append(kwargs["env"])
        return subprocess.CompletedProcess(command, 0, "", "")

    monkeypatch.setattr(release_tool.subprocess, "run", record)

    _verify_wheel_install(
        tmp_path / "package.whl",
        ReleaseVersion.parse("v0.5.0-rc.1"),
        tmp_path,
    )

    assert commands[0][:4] == ["uv", "venv", "--offline", "--no-project"]
    assert commands[0][4:6] == ["--python", sys.executable]
    assert all("HTTP_PROXY" not in environment for environment in environments)
    assert all("HTTPS_PROXY" not in environment for environment in environments)
