#!/usr/bin/env python3
"""Seeed HAL release conformance utilities."""

from __future__ import annotations

import argparse
import json
import re
import subprocess
import sys
import tempfile
import tomllib
from dataclasses import dataclass
from importlib.metadata import Distribution
from pathlib import Path
from typing import NoReturn, Sequence


RC_TAG = re.compile(r"^v0\.5\.0-rc\.([1-9][0-9]*)$")


class ReleaseFailure(Exception):
    """A release validation failure with a stable machine-readable name."""

    def __init__(self, name: str, diagnostic: str) -> None:
        super().__init__(f"{name}: {diagnostic}")
        self.name = name
        self.diagnostic = diagnostic[:512]


@dataclass(frozen=True)
class ReleaseVersion:
    rc: int

    @classmethod
    def parse(cls, tag: str) -> "ReleaseVersion":
        match = RC_TAG.fullmatch(tag)
        if match is None:
            raise ReleaseFailure(
                "release.version.invalid",
                "expected v0.5.0-rc.N",
            )
        return cls(rc=int(match.group(1)))

    @property
    def cargo(self) -> str:
        return f"0.5.0-rc.{self.rc}"

    @property
    def python(self) -> str:
        return f"0.5.0rc{self.rc}"


@dataclass(frozen=True)
class ReleaseTarget:
    name: str
    runner: str
    triple: str
    archive: str
    features: tuple[str, ...]
    required_adapters: tuple[str, ...]


def load_targets(path: Path) -> tuple[ReleaseTarget, ...]:
    try:
        document = tomllib.loads(path.read_text(encoding="utf-8"))
    except (OSError, tomllib.TOMLDecodeError) as error:
        raise ReleaseFailure("release.targets.invalid", str(error)) from error
    if document.get("schema") != 1:
        raise ReleaseFailure("release.targets.invalid", "expected schema 1")
    try:
        return tuple(
            ReleaseTarget(
                name=target["name"],
                runner=target["runner"],
                triple=target["triple"],
                archive=target["archive"],
                features=tuple(target["features"]),
                required_adapters=tuple(target["required_adapters"]),
            )
            for target in document["target"]
        )
    except (KeyError, TypeError) as error:
        raise ReleaseFailure(
            "release.targets.invalid",
            f"missing or invalid field {error}",
        ) from error


def _read_toml(path: Path, error_name: str) -> dict[str, object]:
    try:
        return tomllib.loads(path.read_text(encoding="utf-8"))
    except (OSError, tomllib.TOMLDecodeError) as error:
        raise ReleaseFailure(error_name, str(error)) from error


def _cargo_packages(repo_root: Path, cargo: str) -> tuple[dict[str, object], ...]:
    try:
        result = subprocess.run(
            [cargo, "metadata", "--no-deps", "--format-version", "1"],
            cwd=repo_root,
            check=False,
            capture_output=True,
            text=True,
        )
    except OSError as error:
        raise ReleaseFailure("release.cargo.failed", str(error)) from error
    if result.returncode != 0:
        diagnostic = result.stderr.strip() or f"cargo exited {result.returncode}"
        raise ReleaseFailure("release.cargo.failed", diagnostic)
    try:
        metadata = json.loads(result.stdout)
        return tuple(metadata["packages"])
    except (json.JSONDecodeError, KeyError, TypeError) as error:
        raise ReleaseFailure("release.cargo.invalid", str(error)) from error


def _python_distribution_fixture_version(project_version: str) -> str:
    with tempfile.TemporaryDirectory() as directory:
        dist_info = Path(directory) / "seeed_hal-0.dist-info"
        dist_info.mkdir()
        (dist_info / "METADATA").write_text(
            "Metadata-Version: 2.4\n"
            "Name: seeed-hal\n"
            f"Version: {project_version}\n",
            encoding="utf-8",
        )
        distributions = tuple(Distribution.discover(path=[directory]))
        if len(distributions) != 1:
            raise ReleaseFailure(
                "release.python.invalid",
                "expected one distribution metadata fixture",
            )
        return distributions[0].version


def check_version(repo_root: Path, tag: str, cargo: str = "cargo") -> None:
    expected = ReleaseVersion.parse(tag)
    for package in _cargo_packages(repo_root, cargo):
        name = str(package.get("name"))
        actual = str(package.get("version"))
        if actual != expected.cargo:
            raise ReleaseFailure(
                "release.version.mismatch",
                f"{name} is {actual}, expected {expected.cargo}",
            )

    pyproject = _read_toml(
        repo_root / "bindings" / "python" / "pyproject.toml",
        "release.python.invalid",
    )
    project = pyproject.get("project")
    actual_project = project.get("version") if isinstance(project, dict) else None
    if actual_project != expected.python:
        raise ReleaseFailure(
            "release.version.mismatch",
            f"seeed-hal is {actual_project}, expected {expected.python}",
        )

    actual_distribution = _python_distribution_fixture_version(str(actual_project))
    if actual_distribution != expected.python:
        raise ReleaseFailure(
            "release.version.mismatch",
            f"seeed-hal distribution is {actual_distribution}, expected {expected.python}",
        )


def _parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser()
    subcommands = parser.add_subparsers(dest="command", required=True)
    check = subcommands.add_parser("check-version")
    check.add_argument("--tag", required=True)
    check.add_argument("--repo-root", required=True, type=Path)
    check.add_argument("--cargo", default="cargo")
    return parser


def _fail(error: ReleaseFailure) -> NoReturn:
    print(f"{error.name}: {error.diagnostic}", file=sys.stderr)
    raise SystemExit(1)


def main(argv: Sequence[str] | None = None) -> int:
    arguments = _parser().parse_args(argv)
    try:
        if arguments.command == "check-version":
            check_version(
                arguments.repo_root.resolve(),
                arguments.tag,
                arguments.cargo,
            )
    except ReleaseFailure as error:
        _fail(error)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
