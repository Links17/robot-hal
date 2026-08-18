from __future__ import annotations

import sys
import errno
from pathlib import Path

import pytest

REPO_ROOT = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(REPO_ROOT))

from scripts.release import release_tool
from scripts.release.release_tool import ReleaseFailure, ReleaseVersion, python_artifact_names


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
