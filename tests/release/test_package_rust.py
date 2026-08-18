from __future__ import annotations

import subprocess
import sys
import tarfile
import tempfile
from pathlib import Path

import pytest

REPO_ROOT = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(REPO_ROOT))

from scripts.release.release_tool import (
    ReleaseFailure,
    _cargo_packageable_members,
    _packageable_workspace_members,
    _require_clean_repository,
    package_rust,
)


def _write_workspace(root: Path) -> None:
    (root / "Cargo.toml").write_text(
        """
[workspace]
resolver = "3"
members = ["core", "camera", "adapter"]
""".strip()
        + "\n",
        encoding="utf-8",
    )
    for name in ("core", "camera", "adapter"):
        package = root / name
        (package / "src").mkdir(parents=True)
        dependencies = {
            "core": "",
            "camera": '\n[dependencies]\ncore = { path = "../core", version = "0.5.0-rc.1" }\n',
            "adapter": '\n[dependencies]\ncamera = { path = "../camera", version = "0.5.0-rc.1" }\n',
        }[name]
        (package / "Cargo.toml").write_text(
            f"""
[package]
name = "{name}"
version = "0.5.0-rc.1"
edition = "2024"{dependencies}
""".strip()
            + "\n",
            encoding="utf-8",
        )
        (package / "src" / "lib.rs").write_text(
            (
                "pub fn core() {}\n"
                if name == "core"
                else (
                    "pub fn camera() { core::core(); }\n"
                    if name == "camera"
                    else "pub fn adapter() { camera::camera(); }\n"
                )
            ),
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
    _clean_git_repository(root)


def _bundle_members(bundle: Path) -> tuple[str, ...]:
    with tarfile.open(bundle, "r:gz") as archive:
        return tuple(member.name for member in archive.getmembers() if member.isfile())


def _check_workspace_bundle(bundle: Path) -> None:
    with tempfile.TemporaryDirectory() as directory:
        destination = Path(directory)
        with tarfile.open(bundle, "r:gz") as archive:
            archive.extractall(destination, filter="data")
            root = next(path for path in destination.iterdir() if path.is_dir())
        result = subprocess.run(
            ["cargo", "check", "--workspace", "--locked"],
            cwd=root,
            check=False,
            capture_output=True,
            text=True,
            timeout=30,
        )
    assert result.returncode == 0, result.stderr


def test_rust_bundle_preserves_path_version_workspace_closure(tmp_path: Path) -> None:
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
        "seeed-hal-crates-v0.5.0-rc.1/Cargo.lock",
        "seeed-hal-crates-v0.5.0-rc.1/Cargo.toml",
        "seeed-hal-crates-v0.5.0-rc.1/adapter/Cargo.toml",
        "seeed-hal-crates-v0.5.0-rc.1/adapter/src/lib.rs",
        "seeed-hal-crates-v0.5.0-rc.1/camera/Cargo.toml",
        "seeed-hal-crates-v0.5.0-rc.1/camera/src/lib.rs",
        "seeed-hal-crates-v0.5.0-rc.1/core/Cargo.toml",
        "seeed-hal-crates-v0.5.0-rc.1/core/src/lib.rs",
        "seeed-hal-crates-v0.5.0-rc.1/tracked",
    )
    _check_workspace_bundle(bundle)


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


def test_metadata_workspace_members_and_dependencies_define_package_order() -> None:
    metadata = {
        "workspace_members": ["gamma 0.5.0 (path+file:///gamma)", "alpha 0.5.0 (path+file:///alpha)", "beta 0.5.0 (path+file:///beta)", "private 0.5.0 (path+file:///private)"],
        "packages": [
            {"id": "alpha 0.5.0 (path+file:///alpha)", "name": "alpha", "version": "0.5.0", "publish": None},
            {"id": "beta 0.5.0 (path+file:///beta)", "name": "beta", "version": "0.5.0", "publish": None},
            {"id": "gamma 0.5.0 (path+file:///gamma)", "name": "gamma", "version": "0.5.0", "publish": None},
            {"id": "private 0.5.0 (path+file:///private)", "name": "private", "version": "0.5.0", "publish": []},
            {"id": "outside 0.5.0 (registry+https://example.invalid)", "name": "outside", "version": "0.5.0", "publish": None},
        ],
        "resolve": {
            "nodes": [
                {"id": "gamma 0.5.0 (path+file:///gamma)", "dependencies": ["beta 0.5.0 (path+file:///beta)"]},
                {"id": "alpha 0.5.0 (path+file:///alpha)", "dependencies": []},
                {"id": "beta 0.5.0 (path+file:///beta)", "dependencies": ["alpha 0.5.0 (path+file:///alpha)"]},
                {"id": "private 0.5.0 (path+file:///private)", "dependencies": []},
            ]
        },
    }

    assert [package["name"] for package in _packageable_workspace_members(metadata)] == [
        "alpha",
        "beta",
        "gamma",
    ]


def _git(root: Path, *arguments: str) -> None:
    subprocess.run(
        ["git", *arguments],
        cwd=root,
        check=True,
        capture_output=True,
        text=True,
        timeout=30,
    )


def _clean_git_repository(root: Path) -> None:
    _git(root, "init")
    _git(root, "config", "user.email", "release@example.invalid")
    _git(root, "config", "user.name", "Release Test")
    (root / "tracked").write_text("clean\n", encoding="utf-8")
    _git(root, "add", "-A")
    _git(root, "commit", "-m", "initial")


@pytest.mark.parametrize("state", ["staged", "unstaged", "untracked"])
def test_dirty_repository_is_rejected_for_every_status_kind(
    tmp_path: Path,
    state: str,
) -> None:
    _clean_git_repository(tmp_path)
    if state == "staged":
        (tmp_path / "tracked").write_text("staged\n", encoding="utf-8")
        _git(tmp_path, "add", "tracked")
    elif state == "unstaged":
        (tmp_path / "tracked").write_text("unstaged\n", encoding="utf-8")
    else:
        (tmp_path / "untracked").write_text("untracked\n", encoding="utf-8")

    with pytest.raises(ReleaseFailure, match="repository has uncommitted changes") as failure:
        _require_clean_repository(tmp_path)

    assert failure.value.name == "release.package.invalid"


def test_non_repository_is_rejected(tmp_path: Path) -> None:
    with pytest.raises(ReleaseFailure, match="unable to verify repository state") as failure:
        _require_clean_repository(tmp_path)

    assert failure.value.name == "release.package.invalid"


def test_resolve_metadata_uses_locked_graph(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    metadata = {
        "workspace_members": ["alpha"],
        "packages": [{"id": "alpha", "name": "alpha", "version": "0.5.0-rc.1"}],
        "resolve": {"nodes": [{"id": "alpha", "dependencies": []}]},
    }
    commands: list[list[str]] = []

    def record(command, **kwargs):
        commands.append(command)
        return subprocess.CompletedProcess(command, 0, __import__("json").dumps(metadata), "")

    monkeypatch.setattr("scripts.release.release_tool.subprocess.run", record)

    _cargo_packageable_members(tmp_path)

    assert commands[1] == ["cargo", "metadata", "--locked", "--format-version", "1"]
