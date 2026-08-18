from __future__ import annotations

import sys
import tarfile
import subprocess
from pathlib import Path


REPO_ROOT = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(REPO_ROOT))

from scripts.release.release_tool import package_rust


def _write_workspace(root: Path) -> None:
    (root / "Cargo.toml").write_text(
        """
[workspace]
resolver = "3"
members = ["alpha", "beta", "private"]
""".strip()
        + "\n",
        encoding="utf-8",
    )
    for name, private in (("alpha", False), ("beta", False), ("private", True)):
        package = root / name
        (package / "src").mkdir(parents=True)
        publish = "\npublish = false" if private else ""
        (package / "Cargo.toml").write_text(
            f"""
[package]
name = "{name}"
version = "0.5.0-rc.1"
edition = "2024"{publish}
""".strip()
            + "\n",
            encoding="utf-8",
        )
        (package / "src" / "lib.rs").write_text(
            f"pub fn {name}() {{}}\n",
            encoding="utf-8",
        )
    subprocess.run(
        ["cargo", "generate-lockfile"],
        cwd=root,
        check=True,
        capture_output=True,
        text=True,
        timeout=30,
    )


def _bundle_members(bundle: Path) -> tuple[str, ...]:
    with tarfile.open(bundle, "r:gz") as archive:
        return tuple(member.name for member in archive.getmembers() if member.isfile())


def test_rust_bundle_contains_every_publishable_package(tmp_path: Path) -> None:
    repo = tmp_path / "workspace"
    repo.mkdir()
    _write_workspace(repo)

    bundle = package_rust(
        tag="v0.5.0-rc.1",
        repo_root=repo,
        output_dir=tmp_path / "artifacts",
    )

    assert bundle.name == "seeed-hal-crates-v0.5.0-rc.1.tar.gz"
    assert _bundle_members(bundle) == (
        "seeed-hal-crates-v0.5.0-rc.1/alpha-0.5.0-rc.1.crate",
        "seeed-hal-crates-v0.5.0-rc.1/beta-0.5.0-rc.1.crate",
    )


def test_rust_bundle_refuses_existing_artifact(tmp_path: Path) -> None:
    repo = tmp_path / "workspace"
    repo.mkdir()
    _write_workspace(repo)
    output = tmp_path / "artifacts"
    output.mkdir()
    existing = output / "seeed-hal-crates-v0.5.0-rc.1.tar.gz"
    existing.write_bytes(b"do not overwrite")

    try:
        package_rust(
            tag="v0.5.0-rc.1",
            repo_root=repo,
            output_dir=output,
        )
    except Exception as error:
        assert getattr(error, "name", None) == "release.artifact.unexpected"
    else:
        raise AssertionError("existing artifact must be rejected")

    assert existing.read_bytes() == b"do not overwrite"
