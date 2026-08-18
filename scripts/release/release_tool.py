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
import tarfile
import tomllib
import zipfile
from dataclasses import dataclass
from pathlib import Path
from pathlib import PurePosixPath
from typing import Iterable, NoReturn, Sequence


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
    "schema",
    "broker_version",
    "wire",
    "target",
    "enabled",
    "msrv",
    "artifact_checksum",
    "required_vendor_runtime_libraries",
}
BROKER_MANIFEST_SCHEMA_MAJOR = 1
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
RELEASE_MANIFEST_SCHEMA = 1
RELEASE_WIRE = {"major": 1, "minimum_minor": 0, "maximum_minor": 3}
RELEASE_PYTHON_MIN = "3.11"
RELEASE_COMMIT = re.compile(r"^[0-9a-f]{40}$")
SHA256 = re.compile(r"^[0-9a-f]{64}$")
SENSITIVE_FIELD = re.compile(
    r"(?:token|secret|password|serial|payload|mapping|endpoint|address)",
    re.IGNORECASE,
)
SENSITIVE_VALUE = re.compile(
    r"(?:token|secret|password|payload|serial[_-](?:number|id)|"
    r"mapping[_-]name|endpoint|address)",
    re.IGNORECASE,
)
ARTIFACT_PATTERNS = (
    (
        "broker",
        re.compile(
            r"^seeed-hal-broker-v0\.5\.0-rc\.[1-9][0-9]*-"
            r"(?:aarch64-apple-darwin|x86_64-unknown-linux-gnu)\.tar\.gz$"
        ),
    ),
    (
        "broker",
        re.compile(
            r"^seeed-hal-broker-v0\.5\.0-rc\.[1-9][0-9]*-"
            r"x86_64-pc-windows-msvc\.zip$"
        ),
    ),
    (
        "python-wheel",
        re.compile(r"^seeed_hal-0\.5\.0rc[1-9][0-9]*-py3-none-any\.whl$"),
    ),
    (
        "python-source",
        re.compile(r"^seeed_hal-0\.5\.0rc[1-9][0-9]*\.tar\.gz$"),
    ),
)
EXPECTED_BROKER_COMPOSITION = {
    "macos": {
        "target": "aarch64-apple-darwin",
        "adapters": ["avfoundation", "nusb", "serialport"],
    },
    "linux": {
        "target": "x86_64-unknown-linux-gnu",
        "adapters": ["linux-gpio", "nusb", "serialport", "socketcan", "v4l2"],
    },
    "windows": {
        "target": "x86_64-pc-windows-msvc",
        "adapters": ["mediafoundation", "nusb", "serialport", "windows-gpio"],
    },
}


class ReleaseFailure(Exception):
    """A release validation failure with a stable machine-readable name."""

    def __init__(self, name: str, diagnostic: str) -> None:
        super().__init__(f"{name}: {diagnostic}")
        self.name = name
        self.diagnostic = diagnostic[:512]


@dataclass(frozen=True)
class ArtifactRecord:
    name: str
    kind: str
    target: str | None
    size: int
    sha256: str

    def to_dict(self) -> dict[str, object]:
        return {
            "name": self.name,
            "kind": self.kind,
            "target": self.target,
            "size": self.size,
            "sha256": self.sha256,
        }


@dataclass(frozen=True)
class QualificationStatus:
    id: str
    uri: str

    def to_dict(self) -> dict[str, str]:
        return {"id": self.id, "uri": self.uri}


@dataclass(frozen=True)
class ConformanceReport:
    software: QualificationStatus
    hardware: QualificationStatus

    def to_dict(self) -> dict[str, dict[str, str]]:
        return {
            "software": self.software.to_dict(),
            "hardware": self.hardware.to_dict(),
        }


@dataclass(frozen=True)
class ReleaseManifest:
    schema: int
    tag: str
    version: str
    commit: str
    wire: dict[str, int]
    msrv: str
    python_min: str
    artifacts: tuple[ArtifactRecord, ...]
    broker_composition: dict[str, dict[str, object]]
    qualification: ConformanceReport

    def to_dict(self) -> dict[str, object]:
        return {
            "schema": self.schema,
            "tag": self.tag,
            "version": self.version,
            "commit": self.commit,
            "wire": self.wire,
            "msrv": self.msrv,
            "python_min": self.python_min,
            "artifacts": [artifact.to_dict() for artifact in self.artifacts],
            "broker_composition": self.broker_composition,
            "qualification": self.qualification.to_dict(),
        }


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


def _release_manifest_invalid(diagnostic: str) -> NoReturn:
    raise ReleaseFailure("release.manifest.invalid", diagnostic)


def _archive_invalid(diagnostic: str) -> NoReturn:
    raise ReleaseFailure("release.archive.invalid", diagnostic)


def _artifact_metadata(name: str) -> tuple[str, str | None]:
    if "/" in name or "\\" in name or Path(name).name != name:
        _release_manifest_invalid("artifact name must be a basename")
    for kind, pattern in ARTIFACT_PATTERNS:
        if pattern.fullmatch(name):
            if kind != "broker":
                return kind, None
            target = name.rsplit("-", 1)[1]
            if target.endswith(".tar.gz"):
                target = target.removesuffix(".tar.gz")
            else:
                target = target.removesuffix(".zip")
            return kind, target
    _release_manifest_invalid("artifact name is not permitted by the v0.5 contract")


def _reject_sensitive(value: object) -> None:
    if isinstance(value, dict):
        for key, nested in value.items():
            if not isinstance(key, str) or SENSITIVE_FIELD.search(key):
                _release_manifest_invalid("manifest contains a prohibited sensitive field")
            _reject_sensitive(nested)
    elif isinstance(value, list):
        for nested in value:
            _reject_sensitive(nested)
    elif isinstance(value, str) and SENSITIVE_VALUE.search(value):
        _release_manifest_invalid("manifest contains a prohibited sensitive value")


def _qualification(value: object, field: str) -> QualificationStatus:
    if not isinstance(value, dict) or set(value) != {"id", "uri"}:
        _release_manifest_invalid(f"{field} qualification must have id and uri")
    identifier = value["id"]
    uri = value["uri"]
    if not isinstance(identifier, str) or not identifier or not isinstance(uri, str):
        _release_manifest_invalid(f"{field} qualification values are invalid")
    return QualificationStatus(identifier, uri)


def _artifact_record(value: object) -> ArtifactRecord:
    if not isinstance(value, dict) or set(value) != {
        "name",
        "kind",
        "target",
        "size",
        "sha256",
    }:
        _release_manifest_invalid("artifact fields do not match the manifest contract")
    name = value["name"]
    kind = value["kind"]
    target = value["target"]
    size = value["size"]
    digest = value["sha256"]
    if not isinstance(name, str) or not isinstance(kind, str):
        _release_manifest_invalid("artifact name and kind must be strings")
    expected_kind, expected_target = _artifact_metadata(name)
    if kind != expected_kind or target != expected_target:
        _release_manifest_invalid("artifact kind or target does not match its name")
    if type(size) is not int or size < 0:
        _release_manifest_invalid("artifact size must be a non-negative integer")
    if not isinstance(digest, str) or SHA256.fullmatch(digest) is None:
        _release_manifest_invalid("artifact SHA-256 is invalid")
    return ArtifactRecord(name, kind, target, size, digest)


def validate_release_manifest(value: object) -> ReleaseManifest:
    _reject_sensitive(value)
    if not isinstance(value, dict) or set(value) != {
        "schema",
        "tag",
        "version",
        "commit",
        "wire",
        "msrv",
        "python_min",
        "artifacts",
        "broker_composition",
        "qualification",
    }:
        _release_manifest_invalid("manifest fields do not match the manifest contract")
    if value["schema"] != RELEASE_MANIFEST_SCHEMA:
        _release_manifest_invalid("unsupported release manifest schema")
    tag = value["tag"]
    version = value["version"]
    commit = value["commit"]
    if not isinstance(tag, str) or not isinstance(version, str) or not isinstance(commit, str):
        _release_manifest_invalid("release identity is invalid")
    parsed = ReleaseVersion.parse(tag)
    if version != parsed.cargo or RELEASE_COMMIT.fullmatch(commit) is None:
        _release_manifest_invalid("release identity does not match the v0.5 contract")
    if value["wire"] != RELEASE_WIRE or value["msrv"] != BROKER_MSRV:
        _release_manifest_invalid("wire or MSRV does not match the release contract")
    if value["python_min"] != RELEASE_PYTHON_MIN:
        _release_manifest_invalid("Python minimum does not match the release contract")
    raw_artifacts = value["artifacts"]
    if not isinstance(raw_artifacts, list):
        _release_manifest_invalid("artifacts must be a list")
    artifacts = tuple(_artifact_record(item) for item in raw_artifacts)
    if not artifacts or tuple(item.name for item in artifacts) != tuple(
        sorted(item.name for item in artifacts)
    ):
        _release_manifest_invalid("artifacts must be non-empty and basename sorted")
    if len({item.name for item in artifacts}) != len(artifacts):
        _release_manifest_invalid("artifact names must be unique")
    composition = value["broker_composition"]
    if composition != EXPECTED_BROKER_COMPOSITION:
        _release_manifest_invalid("broker composition does not match the release contract")
    qualification = value["qualification"]
    if not isinstance(qualification, dict) or set(qualification) != {"software", "hardware"}:
        _release_manifest_invalid("qualification fields are invalid")
    return ReleaseManifest(
        schema=RELEASE_MANIFEST_SCHEMA,
        tag=tag,
        version=version,
        commit=commit,
        wire=dict(RELEASE_WIRE),
        msrv=BROKER_MSRV,
        python_min=RELEASE_PYTHON_MIN,
        artifacts=artifacts,
        broker_composition=EXPECTED_BROKER_COMPOSITION,
        qualification=ConformanceReport(
            _qualification(qualification["software"], "software"),
            _qualification(qualification["hardware"], "hardware"),
        ),
    )


def generate_manifest(inputs: dict[str, object]) -> ReleaseManifest:
    expected = {
        "tag",
        "commit",
        "artifacts_dir",
        "software_qualification",
        "hardware_qualification",
    }
    if set(inputs) != expected:
        _release_manifest_invalid("generation inputs do not match the manifest contract")
    tag = inputs["tag"]
    commit = inputs["commit"]
    artifacts_dir = inputs["artifacts_dir"]
    if not isinstance(tag, str) or not isinstance(commit, str) or not isinstance(artifacts_dir, Path):
        _release_manifest_invalid("generation inputs are invalid")
    try:
        version = ReleaseVersion.parse(tag).cargo
    except ReleaseFailure as error:
        raise ReleaseFailure("release.manifest.invalid", "tag is invalid for manifest generation") from error
    if RELEASE_COMMIT.fullmatch(commit) is None:
        _release_manifest_invalid("commit must be a 40-character lowercase SHA")
    try:
        entries = tuple(sorted(artifacts_dir.iterdir(), key=lambda path: path.name))
    except OSError as error:
        raise ReleaseFailure("release.manifest.invalid", "unable to read artifacts directory") from error
    artifacts: list[dict[str, object]] = []
    for path in entries:
        if not path.is_file():
            _release_manifest_invalid("artifacts directory contains a non-file entry")
        kind, target = _artifact_metadata(path.name)
        try:
            size = path.stat().st_size
        except OSError as error:
            raise ReleaseFailure("release.manifest.invalid", "unable to read artifact metadata") from error
        artifacts.append(
            {
                "name": path.name,
                "kind": kind,
                "target": target,
                "size": size,
                "sha256": _artifact_sha256(path),
            }
        )
    return validate_release_manifest(
        {
            "schema": RELEASE_MANIFEST_SCHEMA,
            "tag": tag,
            "version": version,
            "commit": commit,
            "wire": RELEASE_WIRE,
            "msrv": BROKER_MSRV,
            "python_min": RELEASE_PYTHON_MIN,
            "artifacts": artifacts,
            "broker_composition": EXPECTED_BROKER_COMPOSITION,
            "qualification": {
                "software": inputs["software_qualification"],
                "hardware": inputs["hardware_qualification"],
            },
        }
    )


def encode_manifest(manifest: ReleaseManifest) -> bytes:
    validate_release_manifest(manifest.to_dict())
    return (
        json.dumps(
            manifest.to_dict(),
            sort_keys=True,
            separators=(",", ":"),
            ensure_ascii=False,
        ).encode("utf-8")
        + b"\n"
    )


def generate_checksums(manifest: ReleaseManifest) -> bytes:
    validate_release_manifest(manifest.to_dict())
    return b"".join(
        f"{artifact.sha256}  {artifact.name}\n".encode("ascii")
        for artifact in manifest.artifacts
    )


def _validate_checksum_file(checksums: bytes, manifest: ReleaseManifest) -> None:
    if checksums != generate_checksums(manifest):
        _release_manifest_invalid("SHA256SUMS does not exactly cover manifest artifacts")


def verify_static(artifacts_dir: Path, manifest_path: Path, checksums_path: Path) -> None:
    manifest = validate_release_manifest(
        _read_json(manifest_path, "release.manifest.invalid")
    )
    try:
        _validate_checksum_file(checksums_path.read_bytes(), manifest)
        actual = tuple(sorted(path.name for path in artifacts_dir.iterdir() if path.is_file()))
    except OSError as error:
        raise ReleaseFailure("release.manifest.invalid", "unable to read static release inputs") from error
    expected = tuple(artifact.name for artifact in manifest.artifacts)
    if actual != expected:
        _release_manifest_invalid("artifacts directory does not exactly match the manifest")
    for artifact in manifest.artifacts:
        path = artifacts_dir / artifact.name
        try:
            size = path.stat().st_size
        except OSError as error:
            raise ReleaseFailure("release.manifest.invalid", "unable to read artifact metadata") from error
        if size != artifact.size or _artifact_sha256(path) != artifact.sha256:
            _release_manifest_invalid("artifact size or SHA-256 does not match the manifest")


def _safe_archive_path(name: str) -> PurePosixPath:
    if not name or "\\" in name or name.startswith("/") or re.match(r"^[A-Za-z]:", name):
        _archive_invalid("archive member path is not a safe POSIX relative path")
    path = PurePosixPath(name)
    if path.is_absolute() or not path.parts or any(part in {"", ".", ".."} for part in path.parts):
        _archive_invalid("archive member path is not normalized")
    return path


def _validate_members(
    members: Iterable[tuple[str, bool, bool]],
    expected_root: str,
    expected_files: set[str],
) -> None:
    root_path = _safe_archive_path(expected_root)
    if len(root_path.parts) != 1:
        _archive_invalid("expected archive root must be one safe component")
    seen: set[str] = set()
    actual_files: set[str] = set()
    root_seen = False
    for name, is_directory, is_regular in members:
        path = _safe_archive_path(name.rstrip("/") if name.endswith("/") else name)
        normalized = path.as_posix()
        if normalized in seen:
            _archive_invalid("archive contains a duplicate member")
        seen.add(normalized)
        if path.parts[0] != expected_root:
            _archive_invalid("archive member is outside the expected root")
        if is_directory:
            if len(path.parts) == 1:
                root_seen = True
            continue
        if not is_regular or len(path.parts) < 2:
            _archive_invalid("archive contains an invalid member type")
        relative = PurePosixPath(*path.parts[1:]).as_posix()
        actual_files.add(relative)
    if not root_seen or actual_files != expected_files:
        _archive_invalid("archive content does not exactly match the expected files")


def validate_archive(
    archive_path: Path,
    *,
    expected_root: str,
    expected_files: set[str],
) -> None:
    try:
        if archive_path.name.endswith(".tar.gz"):
            with tarfile.open(archive_path, "r:gz") as archive:
                members = []
                for member in archive.getmembers():
                    if not (member.isdir() or member.isreg()) or member.issym() or member.islnk():
                        _archive_invalid("tar archive has an unsupported member type")
                    members.append((member.name, member.isdir(), member.isreg()))
        elif archive_path.name.endswith(".zip"):
            with zipfile.ZipFile(archive_path) as archive:
                members = []
                for member in archive.infolist():
                    mode = member.external_attr >> 16
                    if mode & 0o170000 == 0o120000:
                        _archive_invalid("zip archive contains a symbolic link")
                    is_directory = member.is_dir()
                    file_type = mode & 0o170000
                    is_regular = not is_directory and file_type in {0, 0o100000}
                    if not is_directory and not is_regular:
                        _archive_invalid("zip archive has an unsupported member type")
                    members.append((member.filename, is_directory, is_regular))
        else:
            _archive_invalid("archive type is unsupported")
    except (OSError, tarfile.TarError, zipfile.BadZipFile) as error:
        raise ReleaseFailure("release.archive.invalid", "unable to inspect archive") from error
    _validate_members(members, expected_root, expected_files)


def verify_broker_manifest(
    manifest: object,
    target: ReleaseTarget,
    artifact: Path,
    expected_version: ReleaseVersion,
) -> None:
    document = _require_object(manifest, "manifest")

    schema = _require_object(document.get("schema"), "schema")
    _require_exact_fields(schema, {"major"}, "schema")
    major = schema["major"]
    if type(major) is not int:
        _manifest_invalid("schema.major must be an integer")
    if major != BROKER_MANIFEST_SCHEMA_MAJOR:
        _manifest_invalid(f"unsupported manifest schema major {major}")

    missing_fields = BROKER_MANIFEST_FIELDS - set(document)
    if missing_fields:
        _manifest_invalid("manifest is missing required fields")

    if document["broker_version"] != expected_version.cargo:
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
    verify.add_argument("--tag", required=True)
    verify.add_argument("--manifest", required=True, type=Path)
    verify.add_argument("--target", required=True)
    verify.add_argument("--targets", required=True, type=Path)
    verify.add_argument("--artifact", required=True, type=Path)
    generate = subcommands.add_parser("generate-manifest")
    generate.add_argument("--tag", required=True)
    generate.add_argument("--commit", required=True)
    generate.add_argument("--artifacts-dir", required=True, type=Path)
    generate.add_argument("--output-dir", required=True, type=Path)
    generate.add_argument("--software-qualification", required=True)
    generate.add_argument("--hardware-qualification", required=True)
    static = subcommands.add_parser("verify-static")
    static.add_argument("--artifacts-dir", required=True, type=Path)
    static.add_argument("--manifest", required=True, type=Path)
    static.add_argument("--checksums", required=True, type=Path)
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
                ReleaseVersion.parse(arguments.tag),
            )
        elif arguments.command == "generate-manifest":
            manifest = generate_manifest(
                {
                    "tag": arguments.tag,
                    "commit": arguments.commit,
                    "artifacts_dir": arguments.artifacts_dir,
                    "software_qualification": {
                        "id": "software-qualification",
                        "uri": arguments.software_qualification,
                    },
                    "hardware_qualification": {
                        "id": "hardware-qualification",
                        "uri": arguments.hardware_qualification,
                    },
                }
            )
            try:
                arguments.output_dir.mkdir(parents=True, exist_ok=True)
                (arguments.output_dir / "release-manifest.json").write_bytes(
                    encode_manifest(manifest)
                )
                (arguments.output_dir / "SHA256SUMS").write_bytes(
                    generate_checksums(manifest)
                )
            except OSError as error:
                raise ReleaseFailure(
                    "release.manifest.invalid",
                    "unable to write release manifest output",
                ) from error
        elif arguments.command == "verify-static":
            verify_static(
                arguments.artifacts_dir,
                arguments.manifest,
                arguments.checksums,
            )
    except ReleaseFailure as error:
        _fail(error)
    except OSError as error:
        _fail(ReleaseFailure("release.tool.failed", str(error)))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
