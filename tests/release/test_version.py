from __future__ import annotations

import importlib
import importlib.metadata
import subprocess
import sys
from pathlib import Path

import pytest

REPO_ROOT = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(REPO_ROOT))

from scripts.release.release_tool import ReleaseFailure, ReleaseVersion


RELEASE_TOOL = REPO_ROOT / "scripts" / "release" / "release_tool.py"


def test_rc_version_normalizes_cargo_and_python() -> None:
    version = ReleaseVersion.parse("v0.5.0-rc.7")
    assert version.cargo == "0.5.0-rc.7"
    assert version.python == "0.5.0rc7"


@pytest.mark.parametrize(
    "value",
    ["0.5.0-rc.1", "v0.5.0", "v0.5.0-rc.0", "v0.5.0-rc.x"],
)
def test_rc_version_rejects_noncanonical_tags(value: str) -> None:
    with pytest.raises(ReleaseFailure, match="release.version.invalid"):
        ReleaseVersion.parse(value)


def test_check_version_reports_a_stable_package_mismatch(tmp_path: Path) -> None:
    (tmp_path / "bindings" / "python").mkdir(parents=True)
    (tmp_path / "bindings" / "python" / "pyproject.toml").write_text(
        '[project]\nname = "seeed-hal"\nversion = "0.5.0rc1"\n',
        encoding="utf-8",
    )
    (tmp_path / "bindings" / "python" / "seeed_hal").mkdir()
    (tmp_path / "bindings" / "python" / "seeed_hal" / "__init__.py").write_text(
        "__version__ = '0.5.0rc1'\n",
        encoding="utf-8",
    )
    cargo = tmp_path / "cargo"
    cargo.write_text(
        "#!/bin/sh\n"
        "printf '%s\\n' "
        '\'{\"packages\":[{\"name\":\"seeed-hal-runtime\",\"version\":\"0.4.0\"}]}\'\n',
        encoding="utf-8",
    )
    cargo.chmod(0o755)

    result = subprocess.run(
        [
            sys.executable,
            str(RELEASE_TOOL),
            "check-version",
            "--tag",
            "v0.5.0-rc.1",
            "--repo-root",
            str(tmp_path),
            "--cargo",
            str(cargo),
        ],
        check=False,
        capture_output=True,
        text=True,
    )

    assert result.returncode == 1
    assert result.stderr == (
        "release.version.mismatch: seeed-hal-runtime is 0.4.0, "
        "expected 0.5.0-rc.1\n"
    )


def test_source_tree_version_fails_closed_without_distribution_metadata(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    def missing_distribution(name: str) -> str:
        raise importlib.metadata.PackageNotFoundError(name)

    monkeypatch.setattr(importlib.metadata, "version", missing_distribution)
    sys.modules.pop("seeed_hal", None)

    package = importlib.import_module("seeed_hal")

    assert package.__version__ == "0.5.0rc1"
    assert "__version__" in package.__all__
