from __future__ import annotations

import json
import subprocess
import sys
from pathlib import Path

import pytest

REPO_ROOT = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(REPO_ROOT))

from scripts.release.release_tool import ReleaseFailure, ReleaseVersion


RELEASE_TOOL = REPO_ROOT / "scripts" / "release" / "release_tool.py"


def _write_version_repo(
    root: Path,
    *,
    project_version: str = "0.5.0rc1",
    public_version: str = "0.5.0rc1",
    init_body: str | None = None,
    cargo_version: str = "0.5.0-rc.1",
) -> Path:
    package_root = root / "bindings" / "python"
    package = package_root / "seeed_hal"
    package.mkdir(parents=True)
    (package_root / "pyproject.toml").write_text(
        "[project]\n"
        'name = "seeed-hal"\n'
        f'version = "{project_version}"\n',
        encoding="utf-8",
    )
    (package / "__init__.py").write_text(
        init_body
        if init_body is not None
        else (
            "from importlib.metadata import version as _distribution_version\n"
            '__version__ = _distribution_version("seeed-hal")\n'
            f"__version__ = {public_version!r}\n"
        ),
        encoding="utf-8",
    )
    cargo = root / "cargo"
    cargo.write_text(
        "#!/bin/sh\n"
        f"printf '%s\\n' {json.dumps(json.dumps({'packages': [{'name': 'seeed-hal-runtime', 'version': cargo_version}]}))}\n",
        encoding="utf-8",
    )
    cargo.chmod(0o755)
    return cargo


def _run_check_version(root: Path, cargo: Path) -> subprocess.CompletedProcess[str]:
    return subprocess.run(
        [
            sys.executable,
            str(RELEASE_TOOL),
            "check-version",
            "--tag",
            "v0.5.0-rc.1",
            "--repo-root",
            str(root),
            "--cargo",
            str(cargo),
        ],
        check=False,
        capture_output=True,
        text=True,
        timeout=10,
    )


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
    cargo = _write_version_repo(
        tmp_path,
        cargo_version="0.4.0",
    )
    result = _run_check_version(tmp_path, cargo)

    assert result.returncode == 1
    assert result.stderr == (
        "release.version.mismatch: seeed-hal-runtime is 0.4.0, "
        "expected 0.5.0-rc.1\n"
    )


def test_check_version_rejects_public_python_version_drift(tmp_path: Path) -> None:
    cargo = _write_version_repo(tmp_path, public_version="0.5.0rc2")

    result = _run_check_version(tmp_path, cargo)

    assert result.returncode == 1
    assert result.stderr == (
        "release.version.mismatch: seeed-hal __version__ is 0.5.0rc2, "
        "expected 0.5.0rc1\n"
    )


def test_check_version_structures_python_import_failure(tmp_path: Path) -> None:
    cargo = _write_version_repo(
        tmp_path,
        init_body='raise OSError("metadata read failed")\n',
    )

    result = _run_check_version(tmp_path, cargo)

    assert result.returncode == 1
    assert result.stderr == "release.python.invalid: metadata read failed\n"
    assert "Traceback" not in result.stderr


def test_check_version_structures_missing_python_metadata_file(
    tmp_path: Path,
) -> None:
    cargo = _write_version_repo(tmp_path)
    (tmp_path / "bindings" / "python" / "pyproject.toml").unlink()

    result = _run_check_version(tmp_path, cargo)

    assert result.returncode == 1
    assert result.stderr.startswith("release.python.invalid: ")
    assert "Traceback" not in result.stderr
