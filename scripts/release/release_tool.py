#!/usr/bin/env python3
"""Seeed HAL release conformance utilities."""

from __future__ import annotations

import argparse
import hashlib
import json
import os
import re
import subprocess
import sys
import tempfile
import tomllib
from dataclasses import dataclass
from pathlib import Path
from typing import NoReturn, Sequence


RC_TAG = re.compile(r"^v0\.5\.0-rc\.([1-9][0-9]*)$")
TARGET_FIELDS = {
    "name",
    "runner",
    "triple",
    "archive",
    "features",
    "required_adapters",
}
EXPECTED_TARGETS = (
    {
        "name": "macos",
        "runner": "macos-14",
        "triple": "aarch64-apple-darwin",
        "archive": "tar.gz",
        "features": ("serialport", "nusb", "avfoundation"),
        "required_adapters": ("avfoundation", "nusb", "serialport"),
    },
    {
        "name": "linux",
        "runner": "ubuntu-24.04",
        "triple": "x86_64-unknown-linux-gnu",
        "archive": "tar.gz",
        "features": ("serialport", "nusb", "socketcan", "linux-gpio", "v4l2"),
        "required_adapters": (
            "linux-gpio",
            "nusb",
            "serialport",
            "socketcan",
            "v4l2",
        ),
    },
    {
        "name": "windows",
        "runner": "windows-2025",
        "triple": "x86_64-pc-windows-msvc",
        "archive": "zip",
        "features": ("serialport", "nusb", "windows-gpio", "mediafoundation"),
        "required_adapters": (
            "mediafoundation",
            "nusb",
            "serialport",
            "windows-gpio",
        ),
    },
)
BROKER_MANIFEST_FIELDS = {
    "broker_version",
    "wire",
    "target",
    "enabled",
    "msrv",
    "artifact_checksum",
    "required_vendor_runtime_libraries",
}
BROKER_VERSION = "0.5.0-rc.1"
BROKER_WIRE = {
    "major": 1,
    "minimum_minor": 0,
    "maximum_minor": 3,
}
TARGET_PLATFORMS = {
    "macos": {"os": "macos", "arch": "aarch64"},
    "linux": {"os": "linux", "arch": "x86_64"},
    "windows": {"os": "windows", "arch": "x86_64"},
}
BROKER_MSRV = "1.85"


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
    except (OSError, UnicodeError, tomllib.TOMLDecodeError) as error:
        raise ReleaseFailure("release.targets.invalid", str(error)) from error
    if set(document) != {"schema", "target"} or document.get("schema") != 1:
        raise ReleaseFailure("release.targets.invalid", "expected schema 1")
    raw_targets = document.get("target")
    if not isinstance(raw_targets, list) or len(raw_targets) != len(EXPECTED_TARGETS):
        raise ReleaseFailure(
            "release.targets.invalid",
            "expected exactly macos, linux, and windows targets",
        )
    try:
        targets = tuple(_parse_target(target) for target in raw_targets)
    except (KeyError, TypeError, ValueError) as error:
        raise ReleaseFailure(
            "release.targets.invalid",
            f"missing or invalid field {error}",
        ) from error
    names = tuple(target.name for target in targets)
    if len(set(names)) != len(names):
        raise ReleaseFailure("release.targets.invalid", "duplicate target name")
    actual = tuple(
        {
            "name": target.name,
            "runner": target.runner,
            "triple": target.triple,
            "archive": target.archive,
            "features": target.features,
            "required_adapters": target.required_adapters,
        }
        for target in targets
    )
    if actual != EXPECTED_TARGETS:
        raise ReleaseFailure(
            "release.targets.invalid",
            "target matrix does not match the v0.5 release contract",
        )
    return targets


def _parse_target(target: object) -> ReleaseTarget:
    if not isinstance(target, dict) or set(target) != TARGET_FIELDS:
        raise ValueError("target fields")
    for field in ("name", "runner", "triple", "archive"):
        if not isinstance(target[field], str):
            raise TypeError(field)
    features = _string_tuple(target["features"], "features")
    required_adapters = _string_tuple(
        target["required_adapters"],
        "required_adapters",
    )
    return ReleaseTarget(
        name=target["name"],
        runner=target["runner"],
        triple=target["triple"],
        archive=target["archive"],
        features=features,
        required_adapters=required_adapters,
    )


def _string_tuple(value: object, field: str) -> tuple[str, ...]:
    if not isinstance(value, list) or not all(isinstance(item, str) for item in value):
        raise TypeError(field)
    return tuple(value)


def _read_toml(path: Path, error_name: str) -> dict[str, object]:
    try:
        return tomllib.loads(path.read_text(encoding="utf-8"))
    except (OSError, UnicodeError, tomllib.TOMLDecodeError) as error:
        raise ReleaseFailure(error_name, str(error)) from error


def _cargo_packages(repo_root: Path, cargo: str) -> tuple[dict[str, object], ...]:
    try:
        result = subprocess.run(
            [cargo, "metadata", "--no-deps", "--format-version", "1"],
            cwd=repo_root,
            check=False,
            capture_output=True,
            text=True,
            timeout=30,
        )
    except (OSError, subprocess.TimeoutExpired) as error:
        raise ReleaseFailure("release.cargo.failed", str(error)) from error
    if result.returncode != 0:
        diagnostic = result.stderr.strip() or f"cargo exited {result.returncode}"
        raise ReleaseFailure("release.cargo.failed", diagnostic)
    try:
        metadata = json.loads(result.stdout)
        return tuple(metadata["packages"])
    except (json.JSONDecodeError, KeyError, TypeError) as error:
        raise ReleaseFailure("release.cargo.invalid", str(error)) from error


def _python_public_version(repo_root: Path, project_version: str) -> str:
    try:
        with tempfile.TemporaryDirectory() as directory:
            fixture_root = Path(directory)
            dist_info = fixture_root / "seeed_hal-0.dist-info"
            dist_info.mkdir()
            (dist_info / "METADATA").write_text(
                "Metadata-Version: 2.4\n"
                "Name: seeed-hal\n"
                f"Version: {project_version}\n",
                encoding="utf-8",
            )
            package_root = repo_root / "bindings" / "python"
            import_paths = [
                str(package_root),
                str(fixture_root),
                *_project_site_paths(package_root),
            ]
            environment = {
                "PATH": os.environ.get("PATH", ""),
                "PYTHONNOUSERSITE": "1",
            }
            result = subprocess.run(
                [
                    sys.executable,
                    "-I",
                    "-c",
                    (
                        "import json, sys;"
                        "sys.path[:0] = json.loads(sys.argv[1]);"
                        "import seeed_hal;"
                        "print(seeed_hal.__version__)"
                    ),
                    json.dumps(import_paths),
                ],
                check=False,
                capture_output=True,
                text=True,
                timeout=30,
                env=environment,
            )
    except (OSError, subprocess.TimeoutExpired) as error:
        raise ReleaseFailure("release.python.invalid", str(error)) from error
    if result.returncode != 0:
        diagnostic = _subprocess_diagnostic(result.stderr)
        raise ReleaseFailure("release.python.invalid", diagnostic)
    lines = result.stdout.splitlines()
    if len(lines) != 1 or not lines[0]:
        raise ReleaseFailure(
            "release.python.invalid",
            "Python package did not expose one version value",
        )
    return lines[0]


def _project_site_paths(package_root: Path) -> list[str]:
    virtualenv = package_root / ".venv"
    candidates = [virtualenv / "Lib" / "site-packages"]
    candidates.extend(sorted((virtualenv / "lib").glob("python*/site-packages")))
    return [str(path) for path in candidates if path.is_dir()]


def _subprocess_diagnostic(stderr: str) -> str:
    lines = [line.strip() for line in stderr.splitlines() if line.strip()]
    if not lines:
        return "Python import failed"
    last = lines[-1]
    if ": " in last:
        return last.split(": ", 1)[1]
    return last


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

    actual_public = _python_public_version(repo_root, str(actual_project))
    if actual_public != expected.python:
        raise ReleaseFailure(
            "release.version.mismatch",
            f"seeed-hal __version__ is {actual_public}, expected {expected.python}",
        )


def _read_json(path: Path, error_name: str) -> object:
    try:
        return json.loads(path.read_text(encoding="utf-8"))
    except (OSError, UnicodeError, json.JSONDecodeError) as error:
        raise ReleaseFailure(error_name, str(error)) from error


def _manifest_invalid(diagnostic: str) -> NoReturn:
    raise ReleaseFailure("release.manifest.invalid", diagnostic)


def _require_object(value: object, field: str) -> dict[str, object]:
    if not isinstance(value, dict):
        _manifest_invalid(f"{field} must be an object")
    return value


def _require_exact_fields(
    value: dict[str, object],
    expected: set[str],
    field: str,
) -> None:
    if set(value) != expected:
        _manifest_invalid(f"{field} fields do not match the manifest contract")


def _require_string_list(value: object, field: str) -> tuple[str, ...]:
    if not isinstance(value, list) or not all(isinstance(item, str) for item in value):
        _manifest_invalid(f"{field} must be a string list")
    return tuple(value)


def _artifact_sha256(path: Path) -> str:
    digest = hashlib.sha256()
    try:
        with path.open("rb") as artifact:
            while chunk := artifact.read(64 * 1024):
                digest.update(chunk)
    except OSError as error:
        raise ReleaseFailure("release.manifest.invalid", str(error)) from error
    return digest.hexdigest()


def verify_broker_manifest(
    manifest: object,
    target: ReleaseTarget,
    artifact: Path,
) -> None:
    document = _require_object(manifest, "manifest")
    missing_fields = BROKER_MANIFEST_FIELDS - set(document)
    if missing_fields:
        _manifest_invalid("manifest is missing required fields")

    if document["broker_version"] != BROKER_VERSION:
        _manifest_invalid("broker version does not match")

    wire = _require_object(document["wire"], "wire")
    _require_exact_fields(wire, set(BROKER_WIRE), "wire")
    if any(type(wire[field]) is not int for field in BROKER_WIRE):
        _manifest_invalid("wire values must be integers")
    if wire != BROKER_WIRE:
        _manifest_invalid("wire range does not match")

    expected_platform = TARGET_PLATFORMS.get(target.name)
    if expected_platform is None:
        _manifest_invalid("target platform is unsupported")
    manifest_target = _require_object(document["target"], "target")
    _require_exact_fields(manifest_target, {"triple", "os", "arch"}, "target")
    expected_target = {"triple": target.triple, **expected_platform}
    if manifest_target != expected_target:
        _manifest_invalid("target identity does not match")

    enabled = _require_object(document["enabled"], "enabled")
    _require_exact_fields(enabled, {"adapters", "features"}, "enabled")
    adapters = _require_string_list(enabled["adapters"], "enabled.adapters")
    features = _require_string_list(enabled["features"], "enabled.features")
    if adapters != target.required_adapters:
        _manifest_invalid("enabled adapters do not match")
    if features != tuple(sorted(target.features)):
        _manifest_invalid("enabled features do not match")

    if document["msrv"] != BROKER_MSRV:
        _manifest_invalid("MSRV does not match")
    vendor_runtime = _require_string_list(
        document["required_vendor_runtime_libraries"],
        "required_vendor_runtime_libraries",
    )
    if vendor_runtime:
        _manifest_invalid("vendor runtime libraries do not match")

    checksum = _require_object(document["artifact_checksum"], "artifact_checksum")
    _require_exact_fields(checksum, {"algorithm", "value"}, "artifact_checksum")
    if checksum["algorithm"] != "sha256" or not isinstance(checksum["value"], str):
        _manifest_invalid("artifact checksum is invalid")
    if checksum["value"] != _artifact_sha256(artifact):
        _manifest_invalid("artifact checksum does not match")


def _target_by_name(
    targets: tuple[ReleaseTarget, ...],
    name: str,
) -> ReleaseTarget:
    for target in targets:
        if target.name == name:
            return target
    raise ReleaseFailure("release.target.invalid", f"unknown target {name}")


def _parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser()
    subcommands = parser.add_subparsers(dest="command", required=True)
    check = subcommands.add_parser("check-version")
    check.add_argument("--tag", required=True)
    check.add_argument("--repo-root", required=True, type=Path)
    check.add_argument("--cargo", default="cargo")
    verify = subcommands.add_parser("verify-broker-manifest")
    verify.add_argument("--manifest", required=True, type=Path)
    verify.add_argument("--target", required=True)
    verify.add_argument("--targets", required=True, type=Path)
    verify.add_argument("--artifact", required=True, type=Path)
    return parser


def _fail(error: ReleaseFailure) -> NoReturn:
    print(f"{error.name}: {error.diagnostic}", file=sys.stderr)
    raise SystemExit(1)


def main(argv: Sequence[str] | None = None) -> int:
    try:
        arguments = _parser().parse_args(argv)
        if arguments.command == "check-version":
            check_version(
                arguments.repo_root.resolve(),
                arguments.tag,
                arguments.cargo,
            )
        elif arguments.command == "verify-broker-manifest":
            targets = load_targets(arguments.targets)
            verify_broker_manifest(
                _read_json(arguments.manifest, "release.manifest.invalid"),
                _target_by_name(targets, arguments.target),
                arguments.artifact,
            )
    except ReleaseFailure as error:
        _fail(error)
    except OSError as error:
        _fail(ReleaseFailure("release.tool.failed", str(error)))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
