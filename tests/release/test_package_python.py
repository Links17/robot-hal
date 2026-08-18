from __future__ import annotations

import sys
from pathlib import Path


REPO_ROOT = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(REPO_ROOT))

from scripts.release.release_tool import ReleaseVersion, python_artifact_names


def test_python_artifact_names_use_pep440() -> None:
    assert python_artifact_names(ReleaseVersion.parse("v0.5.0-rc.3")) == (
        "seeed_hal-0.5.0rc3-py3-none-any.whl",
        "seeed_hal-0.5.0rc3.tar.gz",
    )
