#!/usr/bin/env python3
"""Seeed HAL release conformance utilities."""

from __future__ import annotations

import argparse
import contextlib
from email.parser import BytesParser
from email.policy import compat32
import gzip
import hashlib
import ipaddress
import json
import os
import re
import stat
import subprocess
import sys
import tempfile
import tarfile
import tomllib
import unicodedata
import urllib.parse
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
CONFORMANCE_REPORT_NAME = "conformance-report.json"
RELEASE_SIDECARS = frozenset(
    {"release-manifest.json", "SHA256SUMS", CONFORMANCE_REPORT_NAME}
)
ARCHIVE_READ_CHUNK_SIZE = 64 * 1024
PACKAGE_TIMESTAMP = (1980, 1, 1, 0, 0, 0)
# Release bundles may contain large broker binaries and Rust crate sources, but
# raw archive inspection must retain bounded CPU and I/O under hostile headers.
MAX_ARCHIVE_MEMBER_BYTES = 768 * 1024 * 1024
MAX_ARCHIVE_UNCOMPRESSED_BYTES = 1024 * 1024 * 1024
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
        "rust-crates",
        re.compile(r"^seeed-hal-crates-v0\.5\.0-rc\.[1-9][0-9]*\.tar\.gz$"),
    ),
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

    def sidecar_dict(self, tag: str, commit: str) -> dict[str, object]:
        return {
            "schema": 1,
            "tag": tag,
            "commit": commit,
            "qualification": self.to_dict(),
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
    conformance_report: dict[str, object]

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
            "conformance_report": self.conformance_report,
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


def _bounded_subprocess_summary(stderr: str, fallback: str) -> str:
    """Return one stable, non-sensitive subprocess diagnostic."""
    diagnostic = _subprocess_diagnostic(stderr)[:256]
    if (
        not diagnostic
        or SENSITIVE_VALUE.search(diagnostic)
        or re.search(r"(?:^|[\s(])/(?:[^\s:)]+)", diagnostic)
        or re.search(r"[A-Za-z]:[\\/]", diagnostic)
    ):
        return fallback
    return diagnostic


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


def _file_identity(path: Path) -> tuple[int, int, int, str]:
    try:
        metadata = path.lstat()
        if not stat.S_ISREG(metadata.st_mode):
            raise OSError("artifact is not a regular file")
        return (metadata.st_dev, metadata.st_ino, metadata.st_size, _artifact_sha256(path))
    except (OSError, ReleaseFailure) as error:
        raise ReleaseFailure(
            "release.artifact.unexpected",
            "artifact input changed during aggregation",
        ) from error


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
            for target in (
                "aarch64-apple-darwin",
                "x86_64-unknown-linux-gnu",
                "x86_64-pc-windows-msvc",
            ):
                if name.endswith(f"-{target}.tar.gz") or name.endswith(
                    f"-{target}.zip"
                ):
                    return kind, target
            _release_manifest_invalid("broker artifact target is invalid")
    _release_manifest_invalid("artifact name is not permitted by the v0.5 contract")


def _expected_artifacts(version: ReleaseVersion) -> dict[str, tuple[str, str | None]]:
    names = {
        f"seeed-hal-broker-v{version.cargo}-aarch64-apple-darwin.tar.gz": (
            "broker",
            "aarch64-apple-darwin",
        ),
        f"seeed-hal-broker-v{version.cargo}-x86_64-unknown-linux-gnu.tar.gz": (
            "broker",
            "x86_64-unknown-linux-gnu",
        ),
        f"seeed-hal-broker-v{version.cargo}-x86_64-pc-windows-msvc.zip": (
            "broker",
            "x86_64-pc-windows-msvc",
        ),
        f"seeed-hal-crates-v{version.cargo}.tar.gz": ("rust-crates", None),
        f"seeed_hal-{version.python}-py3-none-any.whl": ("python-wheel", None),
        f"seeed_hal-{version.python}.tar.gz": ("python-source", None),
    }
    return names


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


def _safe_public_https_uri(value: object, field: str) -> str:
    if not isinstance(value, str):
        _release_manifest_invalid(f"{field} qualification URI is invalid")
    try:
        parsed = urllib.parse.urlsplit(value)
    except ValueError as error:
        raise ReleaseFailure("release.manifest.invalid", f"{field} qualification URI is invalid") from error
    if (
        parsed.scheme != "https"
        or not parsed.hostname
        or parsed.username is not None
        or parsed.password is not None
        or parsed.query
        or parsed.fragment
        or parsed.path in {"", "/"}
    ):
        _release_manifest_invalid(f"{field} qualification URI is invalid")
    hostname = parsed.hostname.rstrip(".").lower()
    if hostname == "localhost" or hostname.endswith(".localhost"):
        _release_manifest_invalid(f"{field} qualification URI is invalid")
    try:
        address = ipaddress.ip_address(hostname)
    except ValueError:
        return value
    if address.is_private or address.is_loopback or address.is_link_local or address.is_unspecified:
        _release_manifest_invalid(f"{field} qualification URI is invalid")
    return value


def _conformance_report_invalid(diagnostic: str) -> NoReturn:
    raise ReleaseFailure("release.manifest.invalid", diagnostic)


def _bounded_command_identity(value: object) -> str:
    if (
        not isinstance(value, str)
        or not value
        or len(value) > 256
        or "\n" in value
        or "\r" in value
        or SENSITIVE_VALUE.search(value)
        or re.search(r"(?:^|[\s(])/(?:[^\s:)]+)", value)
        or re.search(r"[A-Za-z]:[\\/]", value)
    ):
        _conformance_report_invalid("software job command identity is invalid")
    return value


def validate_conformance_report(value: object) -> dict[str, object]:
    """Validate the controlled, evidence-only conformance report schema."""
    _reject_sensitive(value)
    if not isinstance(value, dict) or set(value) != {
        "schema",
        "tag",
        "commit",
        "qualification",
        "software",
        "hardware",
    }:
        _conformance_report_invalid("conformance report fields are invalid")
    if value["schema"] != 1:
        _conformance_report_invalid("unsupported conformance report schema")
    tag = value["tag"]
    commit = value["commit"]
    if not isinstance(tag, str) or not isinstance(commit, str):
        _conformance_report_invalid("conformance report identity is invalid")
    try:
        ReleaseVersion.parse(tag)
    except ReleaseFailure as error:
        raise ReleaseFailure(
            "release.manifest.invalid",
            "conformance report tag is invalid",
        ) from error
    if RELEASE_COMMIT.fullmatch(commit) is None:
        _conformance_report_invalid("conformance report commit is invalid")
    qualification = value["qualification"]
    if not isinstance(qualification, dict) or set(qualification) != {
        "software",
        "hardware",
    }:
        _conformance_report_invalid("conformance report qualification is invalid")
    normalized_qualification = {
        "software": _qualification(qualification["software"], "software").to_dict(),
        "hardware": _qualification(qualification["hardware"], "hardware").to_dict(),
    }
    software = value["software"]
    if not isinstance(software, dict) or set(software) != {"status", "jobs", "virtual"}:
        _conformance_report_invalid("software conformance fields are invalid")
    if software["status"] not in {"Passed", "Partial", "Pending", "Blocked", "Failed"}:
        _conformance_report_invalid("software conformance status is invalid")
    jobs = software["jobs"]
    if not isinstance(jobs, list) or len(jobs) > len(EXPECTED_TARGETS):
        _conformance_report_invalid("software conformance jobs are invalid")
    normalized_jobs: list[dict[str, str]] = []
    seen_platforms: set[str] = set()
    for job in jobs:
        if not isinstance(job, dict) or set(job) != {
            "platform",
            "result",
            "command",
            "ref",
        }:
            _conformance_report_invalid("software job fields are invalid")
        platform = job["platform"]
        result = job["result"]
        if platform not in {"macos", "linux", "windows"} or platform in seen_platforms:
            _conformance_report_invalid("software job platform is invalid")
        if result not in {"Passed", "Partial", "Pending", "Blocked", "Failed"}:
            _conformance_report_invalid("software job result is invalid")
        seen_platforms.add(platform)
        normalized_jobs.append(
            {
                "platform": platform,
                "result": result,
                "command": _bounded_command_identity(job["command"]),
                "ref": _safe_public_https_uri(job["ref"], "software"),
            }
        )
    virtual = software["virtual"]
    if not isinstance(virtual, list) or len(virtual) > len(EXPECTED_TARGETS) * 4:
        _conformance_report_invalid("virtual conformance evidence is invalid")
    normalized_virtual: list[dict[str, str | int]] = []
    seen_virtual: set[tuple[str, int]] = set()
    for evidence in virtual:
        if not isinstance(evidence, dict) or set(evidence) != {
            "platform",
            "protocol_minor",
            "result",
            "command",
            "ref",
        }:
            _conformance_report_invalid("virtual conformance evidence fields are invalid")
        platform = evidence["platform"]
        minor = evidence["protocol_minor"]
        result = evidence["result"]
        if (
            platform not in {"macos", "linux", "windows"}
            or type(minor) is not int
            or minor not in range(BROKER_WIRE["minimum_minor"], BROKER_WIRE["maximum_minor"] + 1)
            or (platform, minor) in seen_virtual
        ):
            _conformance_report_invalid("virtual conformance evidence identity is invalid")
        if result not in {"Passed", "Partial", "Pending", "Blocked", "Failed"}:
            _conformance_report_invalid("virtual conformance evidence result is invalid")
        seen_virtual.add((platform, minor))
        normalized_virtual.append(
            {
                "platform": platform,
                "protocol_minor": minor,
                "result": result,
                "command": _bounded_command_identity(evidence["command"]),
                "ref": _safe_public_https_uri(evidence["ref"], "software"),
            }
        )
    if software["status"] == "Passed":
        required_platforms = {"macos", "linux", "windows"}
        if (
            {job["platform"] for job in normalized_jobs} != required_platforms
            or any(job["result"] != "Passed" for job in normalized_jobs)
            or {
                (str(item["platform"]), int(item["protocol_minor"]))
                for item in normalized_virtual
            }
            != {
                (platform, minor)
                for platform in required_platforms
                for minor in range(
                    BROKER_WIRE["minimum_minor"],
                    BROKER_WIRE["maximum_minor"] + 1,
                )
            }
            or any(item["result"] != "Passed" for item in normalized_virtual)
        ):
            raise ReleaseFailure(
                "release.conformance.incomplete",
                "passed software conformance lacks complete hosted evidence",
            )
    hardware = value["hardware"]
    if not isinstance(hardware, dict) or not hardware:
        _conformance_report_invalid("hardware conformance is invalid")
    normalized_hardware: dict[str, dict[str, str | None]] = {}
    for name, entry in hardware.items():
        if (
            not isinstance(name, str)
            or not re.fullmatch(r"[a-z0-9]+(?:-[a-z0-9]+)*", name)
            or not isinstance(entry, dict)
            or set(entry) != {"status", "evidence"}
        ):
            _conformance_report_invalid("hardware conformance fields are invalid")
        status = entry["status"]
        evidence = entry["evidence"]
        if status not in {"Passed", "Partial", "Pending", "Blocked", "Failed"}:
            _conformance_report_invalid("hardware conformance status is invalid")
        if status in {"Passed", "Partial", "Failed"}:
            evidence = _safe_public_https_uri(evidence, "hardware")
        elif evidence is not None:
            _conformance_report_invalid("pending or blocked hardware evidence must be null")
        normalized_hardware[name] = {"status": status, "evidence": evidence}
    return {
        "schema": 1,
        "tag": tag,
        "commit": commit,
        "qualification": normalized_qualification,
        "software": {
            "status": software["status"],
            "jobs": normalized_jobs,
            "virtual": normalized_virtual,
        },
        "hardware": normalized_hardware,
    }


def release_ready(report: dict[str, object]) -> None:
    """Require complete successful hosted software evidence before final readiness."""
    if report["software"]["status"] != "Passed":
        raise ReleaseFailure(
            "release.conformance.incomplete",
            "software conformance is not passed",
        )


def initial_conformance_report(manifest: ReleaseManifest) -> dict[str, object]:
    """Create the factual, non-ready report emitted with a fresh manifest."""
    return {
        "schema": 1,
        "tag": manifest.tag,
        "commit": manifest.commit,
        "qualification": manifest.qualification.to_dict(),
        "software": {"status": "Pending", "jobs": [], "virtual": []},
        "hardware": {"release-hardware": {"status": "Pending", "evidence": None}},
    }


def encode_conformance_report(report: object) -> bytes:
    validated = validate_conformance_report(report)
    return (
        json.dumps(
            validated,
            sort_keys=True,
            separators=(",", ":"),
            ensure_ascii=False,
        ).encode("utf-8")
        + b"\n"
    )


def conformance_report_binding(report: object) -> dict[str, object]:
    encoded = encode_conformance_report(report)
    return {
        "schema": 1,
        "name": CONFORMANCE_REPORT_NAME,
        "byte_size": len(encoded),
        "sha256": hashlib.sha256(encoded).hexdigest(),
    }


def write_conformance_report(inputs: Path, output: Path) -> None:
    """Copy one frozen report input through controlled validation."""
    try:
        directory = inputs.lstat()
        report_path = inputs / CONFORMANCE_REPORT_NAME
        if (
            not stat.S_ISDIR(directory.st_mode)
            or inputs.is_symlink()
            or {path.name for path in inputs.iterdir()} != {CONFORMANCE_REPORT_NAME}
            or report_path.is_symlink()
            or not report_path.is_file()
        ):
            _conformance_report_invalid("conformance report input is invalid")
        before = _file_identity(report_path)
        report = validate_conformance_report(
            _read_json(report_path, "release.manifest.invalid")
        )
        if _file_identity(report_path) != before:
            _conformance_report_invalid("conformance report input changed")
        with output.open("xb") as destination:
            destination.write(encode_conformance_report(report))
    except OSError as error:
        raise ReleaseFailure(
            "release.manifest.invalid",
            "unable to write conformance report",
        ) from error


def _input_directory(
    directory: Path,
    expected_names: frozenset[str] | None = None,
) -> tuple[int, int, frozenset[str]]:
    try:
        metadata = directory.lstat()
        entries = frozenset(path.name for path in directory.iterdir())
    except OSError as error:
        raise ReleaseFailure(
            "release.artifact.unexpected",
            "artifact input directory is invalid",
        ) from error
    if not stat.S_ISDIR(metadata.st_mode) or directory.is_symlink():
        raise ReleaseFailure(
            "release.artifact.unexpected",
            "artifact input directory is invalid",
        )
    if expected_names is not None and entries != expected_names:
        raise ReleaseFailure(
            "release.artifact.unexpected",
            "artifact input directory entries are invalid",
        )
    return (metadata.st_dev, metadata.st_ino, entries)


def _require_input_directory(
    directory: Path,
    expected: tuple[int, int, frozenset[str]],
) -> None:
    actual = _input_directory(directory, expected[2])
    if actual != expected:
        raise ReleaseFailure(
            "release.artifact.unexpected",
            "artifact input directory changed during aggregation",
        )


def _require_frozen_inputs(
    directories: tuple[tuple[Path, tuple[int, int, frozenset[str]]], ...],
    artifacts: dict[str, Path],
    identities: dict[str, tuple[int, int, int, str]],
) -> None:
    for directory, identity in directories:
        _require_input_directory(directory, identity)
    for name, artifact in artifacts.items():
        if _file_identity(artifact) != identities[name]:
            raise ReleaseFailure(
                "release.artifact.unexpected",
                "artifact input changed during aggregation",
            )


def _copy_frozen_artifact(
    source: Path,
    destination: Path,
    identity: tuple[int, int, int, str],
) -> None:
    if _file_identity(source) != identity:
        raise ReleaseFailure(
            "release.artifact.unexpected",
            "artifact input changed during aggregation",
        )
    try:
        with source.open("rb") as input_file, destination.open("xb") as output_file:
            while chunk := input_file.read(ARCHIVE_READ_CHUNK_SIZE):
                output_file.write(chunk)
    except FileExistsError as error:
        raise ReleaseFailure(
            "release.artifact.unexpected",
            "release output entry already exists",
        ) from error
    except OSError as error:
        raise ReleaseFailure(
            "release.package.invalid",
            "unable to copy artifact input",
        ) from error
    if _file_identity(source) != identity or _file_identity(destination)[3:] != identity[3:]:
        raise ReleaseFailure(
            "release.artifact.unexpected",
            "artifact input changed during aggregation",
        )


def _create_release_directory(release_dir: Path) -> tuple[int, int]:
    try:
        release_dir.mkdir(parents=True, mode=0o700)
        metadata = release_dir.lstat()
    except FileExistsError as error:
        raise ReleaseFailure(
            "release.artifact.unexpected",
            "release directory must be new",
        ) from error
    except OSError as error:
        raise ReleaseFailure(
            "release.package.invalid",
            "unable to create release directory",
        ) from error
    if not stat.S_ISDIR(metadata.st_mode) or release_dir.is_symlink():
        raise ReleaseFailure(
            "release.artifact.unexpected",
            "release directory is invalid",
        )
    release_dir.chmod(0o700)
    return (metadata.st_dev, metadata.st_ino)


def aggregate_release(
    *,
    tag: str,
    commit: str,
    broker_dir: Path,
    rust_bundle: Path,
    python_candidate: Path,
    report_inputs: Path,
    release_dir: Path,
) -> Path:
    """Aggregate frozen Task 6 candidates into one newly-created release directory."""
    version = ReleaseVersion.parse(tag)
    expected = _expected_artifacts(version)
    broker_names = frozenset(
        name for name, (kind, _) in expected.items() if kind == "broker"
    )
    python_names = frozenset(
        name for name, (kind, _) in expected.items() if kind.startswith("python-")
    )
    rust_name = next(name for name, (kind, _) in expected.items() if kind == "rust-crates")
    broker_identity = _input_directory(broker_dir, broker_names)
    python_identity = _input_directory(python_candidate, python_names)
    report_identity = _input_directory(report_inputs, frozenset({CONFORMANCE_REPORT_NAME}))
    artifacts = {
        name: broker_dir / name for name in broker_names
    } | {
        name: python_candidate / name for name in python_names
    } | {rust_name: rust_bundle / rust_name}
    rust_directory_identity = _input_directory(rust_bundle, frozenset({rust_name}))
    identities = {name: _file_identity(path) for name, path in artifacts.items()}
    frozen_inputs = (
        (broker_dir, broker_identity),
        (python_candidate, python_identity),
        (rust_bundle, rust_directory_identity),
        (report_inputs, report_identity),
    )
    _create_release_directory(release_dir)
    try:
        for name in sorted(artifacts):
            _copy_frozen_artifact(artifacts[name], release_dir / name, identities[name])
        _require_frozen_inputs(frozen_inputs, artifacts, identities)
        report_path = release_dir / CONFORMANCE_REPORT_NAME
        write_conformance_report(report_inputs, report_path)
        _require_frozen_inputs(frozen_inputs, artifacts, identities)
        report = validate_conformance_report(
            _read_json(report_path, "release.manifest.invalid")
        )
        manifest = generate_manifest(
            {
                "tag": tag,
                "commit": commit,
                "artifacts_dir": release_dir,
                "software_qualification": report["qualification"]["software"],
                "hardware_qualification": report["qualification"]["hardware"],
                "conformance_report": report,
            }
        )
        (release_dir / "release-manifest.json").write_bytes(encode_manifest(manifest))
        (release_dir / "SHA256SUMS").write_bytes(generate_checksums(manifest))
        verify_static(release_dir)
        _require_frozen_inputs(frozen_inputs, artifacts, identities)
    except ReleaseFailure:
        raise
    except OSError as error:
        raise ReleaseFailure(
            "release.package.invalid",
            "unable to aggregate release directory",
        ) from error
    return release_dir


def _extract_broker_archive(
    archive: Path,
    target: ReleaseTarget,
    version: ReleaseVersion,
    destination: Path,
) -> Path:
    root = _package_root(version, target)
    binary_name = _package_binary_name(target)
    validate_archive(
        archive,
        expected_root=root,
        expected_files={"LICENSE", "README.md", "broker-manifest.json", binary_name},
    )
    try:
        destination.mkdir(mode=0o700)
        if archive.name.endswith(".tar.gz"):
            with tarfile.open(archive, "r:gz") as contents:
                contents.extractall(destination, filter="data")
        else:
            with zipfile.ZipFile(archive) as contents:
                contents.extractall(destination)
    except (OSError, tarfile.TarError, zipfile.BadZipFile) as error:
        raise ReleaseFailure("release.archive.invalid", "unable to extract broker archive") from error
    return destination / root


def _host_target(target: ReleaseTarget) -> bool:
    host = sys.platform
    return (
        (target.name == "macos" and host == "darwin" and os.uname().machine == "arm64")
        or (target.name == "linux" and host.startswith("linux") and os.uname().machine == "x86_64")
        or (target.name == "windows" and host == "win32")
    )


def _run_virtual_conformance(binary: Path, repo_root: Path) -> None:
    """Run every hardware-free virtual wire profile with bounded subprocesses."""
    runner = repo_root / "tests" / "conformance" / "run-broker-conformance.py"
    if not runner.is_file() or runner.is_symlink():
        raise ReleaseFailure(
            "release.package.invalid",
            "virtual conformance runner is unavailable",
        )
    try:
        for minor in range(BROKER_WIRE["minimum_minor"], BROKER_WIRE["maximum_minor"] + 1):
            result = subprocess.run(
                [
                    "uv",
                    "run",
                    "--project",
                    str(repo_root / "bindings" / "python"),
                    "--python",
                    "3.11",
                    "--frozen",
                    "python",
                    str(runner),
                    "--broker",
                    str(binary),
                    "--protocol-minor",
                    str(minor),
                ],
                cwd=repo_root,
                check=False,
                capture_output=True,
                text=True,
                timeout=120,
            )
            if result.returncode != 0:
                raise ReleaseFailure(
                    "release.package.invalid",
                    "local virtual conformance command failed",
                )
    except (OSError, subprocess.TimeoutExpired) as error:
        raise ReleaseFailure(
            "release.package.invalid",
            "local virtual conformance command failed",
        ) from error


def collect_virtual_conformance(
    *,
    platform: str,
    broker: Path,
    repo_root: Path,
    command: str,
    ref: str,
) -> list[dict[str, str | int]]:
    """Run a separately supplied virtual-adapters broker for one hosted platform."""
    if platform not in {"macos", "linux", "windows"} or not _host_target(
        _target_by_name(load_targets(repo_root / "release" / "targets.toml"), platform)
    ):
        raise ReleaseFailure(
            "release.conformance.host",
            "virtual conformance broker must run on its matching host",
        )
    _bounded_command_identity(command)
    _safe_public_https_uri(ref, "software")
    if not broker.is_file() or broker.is_symlink():
        raise ReleaseFailure(
            "release.conformance.invalid",
            "virtual conformance broker is invalid",
        )
    _run_virtual_conformance(broker, repo_root)
    return [
        {
            "platform": platform,
            "protocol_minor": minor,
            "result": "Passed",
            "command": command,
            "ref": ref,
        }
        for minor in range(BROKER_WIRE["minimum_minor"], BROKER_WIRE["maximum_minor"] + 1)
    ]


def verify_artifacts(
    release_dir: Path,
    tag: str,
    targets_path: Path,
    repo_root: Path,
) -> None:
    """Verify one complete release directory; executable checks run only on its host."""
    try:
        verify_static(release_dir)
    except ReleaseFailure as error:
        if error.name == "release.manifest.invalid":
            raise ReleaseFailure(
                "release.artifact.unexpected",
                "release directory is incomplete or inconsistent",
            ) from error
        raise
    manifest = validate_release_manifest(
        _read_json(release_dir / "release-manifest.json", "release.manifest.invalid")
    )
    if manifest.tag != tag:
        raise ReleaseFailure("release.artifact.unexpected", "release tag does not match")
    report = validate_conformance_report(
        _read_json(release_dir / CONFORMANCE_REPORT_NAME, "release.manifest.invalid")
    )
    release_ready(report)
    targets = load_targets(targets_path)
    with tempfile.TemporaryDirectory(prefix=".verify-artifacts-") as temporary:
        root = Path(temporary)
        for target in targets:
            archive = release_dir / _package_archive_name(ReleaseVersion.parse(tag), target)
            extracted = _extract_broker_archive(
                archive,
                target,
                ReleaseVersion.parse(tag),
                root / target.name,
            )
            binary = extracted / _package_binary_name(target)
            broker_manifest = extracted / "broker-manifest.json"
            verify_broker_manifest(
                _read_json(broker_manifest, "release.manifest.invalid"),
                target,
                binary,
                ReleaseVersion.parse(tag),
            )
            if not _host_target(target):
                continue
            try:
                result = subprocess.run(
                    [str(binary), "--manifest"],
                    cwd=extracted,
                    check=False,
                    capture_output=True,
                    text=True,
                    timeout=30,
                )
            except (OSError, subprocess.TimeoutExpired) as error:
                raise ReleaseFailure(
                    "release.package.invalid",
                    "local broker executable verification failed",
                ) from error
            if result.returncode != 0:
                raise ReleaseFailure(
                    "release.manifest.invalid",
                    "local broker manifest command failed",
                )
            verify_broker_manifest(
                json.loads(result.stdout),
                target,
                binary,
                ReleaseVersion.parse(tag),
            )


def _qualification(value: object, field: str) -> QualificationStatus:
    if not isinstance(value, dict) or set(value) != {"id", "uri"}:
        _release_manifest_invalid(f"{field} qualification must have id and uri")
    identifier = value["id"]
    expected_id = {
        "software": "software-conformance",
        "hardware": "hardware-qualification",
    }[field]
    if identifier != expected_id:
        _release_manifest_invalid(f"{field} qualification values are invalid")
    return QualificationStatus(identifier, _safe_public_https_uri(value["uri"], field))


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
        "conformance_report",
    }:
        _release_manifest_invalid("manifest fields do not match the manifest contract")
    if value["schema"] != RELEASE_MANIFEST_SCHEMA:
        _release_manifest_invalid("unsupported release manifest schema")
    tag = value["tag"]
    version = value["version"]
    commit = value["commit"]
    if not isinstance(tag, str) or not isinstance(version, str) or not isinstance(commit, str):
        _release_manifest_invalid("release identity is invalid")
    try:
        parsed = ReleaseVersion.parse(tag)
    except ReleaseFailure as error:
        raise ReleaseFailure("release.manifest.invalid", "release tag is invalid") from error
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
    if len({artifact.name for artifact in artifacts}) != len(artifacts):
        _release_manifest_invalid("artifact records must be unique")
    if tuple(item.name for item in artifacts) != tuple(
        sorted(item.name for item in artifacts)
    ):
        _release_manifest_invalid("artifacts must be basename sorted")
    expected_artifacts = _expected_artifacts(parsed)
    actual_artifacts = {
        item.name: (item.kind, item.target)
        for item in artifacts
    }
    if actual_artifacts != expected_artifacts:
        _release_manifest_invalid("artifacts do not exactly match the release tag")
    composition = value["broker_composition"]
    if composition != EXPECTED_BROKER_COMPOSITION:
        _release_manifest_invalid("broker composition does not match the release contract")
    qualification = value["qualification"]
    if not isinstance(qualification, dict) or set(qualification) != {"software", "hardware"}:
        _release_manifest_invalid("qualification fields are invalid")
    conformance_report = value["conformance_report"]
    if (
        not isinstance(conformance_report, dict)
        or set(conformance_report) != {"schema", "name", "byte_size", "sha256"}
        or conformance_report["schema"] != 1
        or conformance_report["name"] != CONFORMANCE_REPORT_NAME
        or type(conformance_report["byte_size"]) is not int
        or conformance_report["byte_size"] < 1
        or not isinstance(conformance_report["sha256"], str)
        or SHA256.fullmatch(conformance_report["sha256"]) is None
    ):
        _release_manifest_invalid("conformance report reference is invalid")
    report = ConformanceReport(
        _qualification(qualification["software"], "software"),
        _qualification(qualification["hardware"], "hardware"),
    )
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
        qualification=report,
        conformance_report=dict(conformance_report),
    )


def generate_manifest(inputs: dict[str, object]) -> ReleaseManifest:
    required = {
        "tag",
        "commit",
        "artifacts_dir",
        "software_qualification",
        "hardware_qualification",
    }
    if not required.issubset(inputs) or set(inputs) - (required | {"conformance_report"}):
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
        if path.name in RELEASE_SIDECARS:
            continue
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
    qualification = {
        "software": inputs["software_qualification"],
        "hardware": inputs["hardware_qualification"],
    }
    default_report = {
        "schema": 1,
        "tag": tag,
        "commit": commit,
        "qualification": qualification,
        "software": {"status": "Pending", "jobs": [], "virtual": []},
        "hardware": {"release-hardware": {"status": "Pending", "evidence": None}},
    }
    report = inputs.get("conformance_report", default_report)
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
            "qualification": qualification,
            "conformance_report": conformance_report_binding(report),
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


def _release_directory_path(release_dir: Path, name: str) -> Path:
    path = release_dir / name
    if path.parent != release_dir or path.name != name or path.is_symlink():
        _release_manifest_invalid("release directory entry is invalid")
    return path


def verify_static(
    release_dir: Path,
    manifest_path: Path | None = None,
    checksums_path: Path | None = None,
) -> None:
    if manifest_path is not None or checksums_path is not None:
        _release_manifest_invalid("verify-static requires one release directory")
    if not release_dir.is_dir() or release_dir.is_symlink():
        _release_manifest_invalid("release directory is invalid")
    manifest_path = _release_directory_path(release_dir, "release-manifest.json")
    checksums_path = _release_directory_path(release_dir, "SHA256SUMS")
    report_path = _release_directory_path(release_dir, CONFORMANCE_REPORT_NAME)
    manifest = validate_release_manifest(
        _read_json(manifest_path, "release.manifest.invalid")
    )
    try:
        _validate_checksum_file(checksums_path.read_bytes(), manifest)
        entries = tuple(release_dir.iterdir())
        expected_names = frozenset(
            artifact.name for artifact in manifest.artifacts
        ) | RELEASE_SIDECARS
        if {path.name for path in entries} != expected_names:
            _release_manifest_invalid("release directory contents are invalid")
        if any(not path.is_file() or path.is_symlink() for path in entries):
            _release_manifest_invalid("release directory contains an unsafe entry")
        report = _read_json(report_path, "release.manifest.invalid")
    except OSError as error:
        raise ReleaseFailure("release.manifest.invalid", "unable to read static release inputs") from error
    for artifact in manifest.artifacts:
        path = _release_directory_path(release_dir, artifact.name)
        try:
            size = path.stat().st_size
        except OSError as error:
            raise ReleaseFailure("release.manifest.invalid", "unable to read artifact metadata") from error
        if size != artifact.size or _artifact_sha256(path) != artifact.sha256:
            _release_manifest_invalid("artifact size or SHA-256 does not match the manifest")
    try:
        report_bytes = report_path.read_bytes()
        expected_report = encode_conformance_report(_read_json(
            report_path,
            "release.manifest.invalid",
        ))
    except OSError as error:
        raise ReleaseFailure(
            "release.manifest.invalid",
            "unable to read conformance report",
        ) from error
    binding = manifest.conformance_report
    if (
        report_bytes != expected_report
        or len(report_bytes) != binding["byte_size"]
        or hashlib.sha256(report_bytes).hexdigest() != binding["sha256"]
    ):
        _release_manifest_invalid("conformance report binding does not match")
    try:
        validated_report = validate_conformance_report(report)
    except ReleaseFailure as error:
        raise ReleaseFailure(
            "release.manifest.invalid",
            "conformance report is invalid",
        ) from error
    if (
        validated_report["tag"] != manifest.tag
        or validated_report["commit"] != manifest.commit
        or validated_report["qualification"] != manifest.qualification.to_dict()
    ):
        _release_manifest_invalid("conformance report does not match the manifest")


def _safe_archive_path(name: str) -> PurePosixPath:
    normalized_input = name[:-1] if name.endswith("/") else name
    if (
        not normalized_input
        or "\\" in name
        or name.startswith("/")
        or re.match(r"^[A-Za-z]:", name)
        or "" in normalized_input.split("/")
    ):
        _archive_invalid("archive member path is not a safe POSIX relative path")
    path = PurePosixPath(normalized_input)
    if path.is_absolute() or not path.parts or any(part in {"", ".", ".."} for part in path.parts):
        _archive_invalid("archive member path is not normalized")
    return path


def _archive_member_path(name: str, is_directory: bool) -> PurePosixPath:
    if is_directory:
        # tarfile normalizes standard directory names to omit the trailing slash,
        # whereas ZIP retains it. Both forms are permitted, but doubled slashes
        # are never a valid directory representation.
        if name.endswith("//"):
            _archive_invalid("archive directory name has extra trailing slashes")
    elif name.endswith("/"):
        _archive_invalid("archive file name must not have a trailing slash")
    return _safe_archive_path(name)


def _validate_raw_tar_directory_names(archive_path: Path) -> None:
    """Reject slash forms tarfile normalizes before exposing TarInfo names."""
    try:
        with gzip.open(archive_path, "rb") as archive:
            total_payload_size = 0
            while header := archive.read(512):
                if len(header) != 512:
                    _archive_invalid("tar archive has a truncated header")
                if header == b"\0" * 512:
                    return
                raw_name = header[:100].split(b"\0", 1)[0]
                if header[156:157] == b"5":
                    try:
                        name = raw_name.decode("utf-8")
                    except UnicodeDecodeError as error:
                        raise ReleaseFailure(
                            "release.archive.invalid",
                            "tar archive member name is not UTF-8",
                        ) from error
                    if name.endswith("//"):
                        _archive_invalid(
                            "tar directory name has extra trailing slashes"
                        )
                size_field = header[124:136].rstrip(b"\0 ")
                try:
                    size = int(size_field or b"0", 8)
                except ValueError as error:
                    raise ReleaseFailure(
                        "release.archive.invalid",
                        "tar archive member size is invalid",
                    ) from error
                if size > MAX_ARCHIVE_MEMBER_BYTES:
                    _archive_invalid("tar archive member size exceeds safety limit")
                total_payload_size += size
                if total_payload_size > MAX_ARCHIVE_UNCOMPRESSED_BYTES:
                    _archive_invalid("tar archive payload exceeds safety limit")
                remaining = (size + 511) // 512 * 512
                while remaining:
                    chunk_size = min(remaining, ARCHIVE_READ_CHUNK_SIZE)
                    if len(archive.read(chunk_size)) != chunk_size:
                        _archive_invalid("tar archive has truncated member data")
                    remaining -= chunk_size
    except (EOFError, OSError) as error:
        raise ReleaseFailure("release.archive.invalid", "unable to inspect archive") from error


def _validate_members(
    members: Iterable[tuple[str, bool, bool]],
    expected_root: str,
    expected_files: set[str],
) -> None:
    root_path = _safe_archive_path(expected_root)
    if len(root_path.parts) != 1:
        _archive_invalid("expected archive root must be one safe component")
    canonical_expected = _canonical_archive_paths(expected_files, "expected files")
    seen: set[str] = set()
    actual_files: set[str] = set()
    directories: set[str] = set()
    root_seen = False
    for name, is_directory, is_regular in members:
        path = _archive_member_path(name, is_directory)
        normalized = unicodedata.normalize("NFC", path.as_posix())
        collision_key = _archive_collision_key(normalized)
        if collision_key in seen:
            _archive_invalid("archive contains a duplicate member")
        seen.add(collision_key)
        if path.parts[0] != expected_root:
            _archive_invalid("archive member is outside the expected root")
        if is_directory:
            directories.add(normalized)
            if len(path.parts) == 1:
                root_seen = True
            continue
        if not is_regular or len(path.parts) < 2:
            _archive_invalid("archive contains an invalid member type")
        relative = unicodedata.normalize(
            "NFC",
            PurePosixPath(*path.parts[1:]).as_posix(),
        )
        actual_files.add(relative)
    allowed_directories = {expected_root}
    for relative in canonical_expected:
        parts = PurePosixPath(relative).parts
        for index in range(1, len(parts)):
            allowed_directories.add(
                PurePosixPath(expected_root, *parts[:index]).as_posix()
            )
    if (
        not root_seen
        or actual_files != canonical_expected
        or directories != allowed_directories
    ):
        _archive_invalid("archive content does not exactly match the expected files")


def _archive_collision_key(path: str) -> str:
    return unicodedata.normalize("NFC", path).casefold()


def _canonical_archive_paths(paths: set[str], field: str) -> set[str]:
    canonical: set[str] = set()
    collisions: set[str] = set()
    for item in paths:
        path = _safe_archive_path(item)
        if len(path.parts) < 1:
            _archive_invalid(f"{field} contains an invalid path")
        normalized = unicodedata.normalize("NFC", path.as_posix())
        key = _archive_collision_key(normalized)
        if key in collisions:
            _archive_invalid(f"{field} contains a case or Unicode collision")
        collisions.add(key)
        canonical.add(normalized)
    return canonical


def validate_archive(
    archive_path: Path,
    *,
    expected_root: str,
    expected_files: set[str],
) -> None:
    try:
        if archive_path.name.endswith(".tar.gz"):
            _validate_raw_tar_directory_names(archive_path)
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
    raise ReleaseFailure("release.target.invalid", "unknown target")


def print_target_matrix(targets_path: Path) -> str:
    """Encode the release targets as a GitHub Actions matrix."""
    return json.dumps(
        {
            "include": [
                {
                    "name": target.name,
                    "runner": target.runner,
                    "features": ",".join(target.features),
                }
                for target in load_targets(targets_path)
            ]
        },
        sort_keys=True,
        separators=(",", ":"),
    )


def _package_invalid(diagnostic: str) -> NoReturn:
    raise ReleaseFailure("release.package.invalid", diagnostic)


def _package_diagnostic(name: str) -> str:
    diagnostics = {
        "release.archive.invalid": "archive validation failed",
        "release.artifact.unexpected": "final broker archive already exists",
        "release.manifest.invalid": "broker manifest is invalid",
        "release.package.invalid": "unable to package broker",
        "release.target.invalid": "unknown target",
        "release.targets.invalid": "target matrix is invalid",
        "release.version.invalid": "expected v0.5.0-rc.N",
    }
    return diagnostics.get(name, "unable to package broker")


def _package_file(path: Path, name: str) -> Path:
    if path.name != name or not path.is_file() or path.is_symlink():
        _package_invalid("package input is invalid")
    return path


def _package_binary_name(target: ReleaseTarget) -> str:
    return "seeed-hal-broker.exe" if target.name == "windows" else "seeed-hal-broker"


def _package_root(version: ReleaseVersion, target: ReleaseTarget) -> str:
    return f"seeed-hal-broker-v{version.cargo}-{target.triple}"


def _package_archive_name(version: ReleaseVersion, target: ReleaseTarget) -> str:
    suffix = "zip" if target.archive == "zip" else "tar.gz"
    return f"{_package_root(version, target)}.{suffix}"


def _package_reservation_path(output_dir: Path, archive_name: str) -> Path:
    return output_dir / f".reserve-broker-{archive_name}"


def _reserve_package_archive(path: Path) -> None:
    try:
        descriptor = os.open(path, os.O_CREAT | os.O_EXCL | os.O_WRONLY, 0o600)
    except FileExistsError as error:
        raise ReleaseFailure(
            "release.artifact.unexpected",
            "final broker archive is already reserved",
        ) from error
    except OSError as error:
        raise ReleaseFailure(
            "release.package.invalid",
            "unable to reserve final broker archive",
        ) from error
    else:
        os.close(descriptor)


def _package_output_entries(output_dir: Path) -> frozenset[str]:
    try:
        return frozenset(path.name for path in output_dir.iterdir())
    except OSError as error:
        raise ReleaseFailure(
            "release.package.invalid",
            "unable to inspect package output directory",
        ) from error


def _require_package_output_entries(
    output_dir: Path,
    expected: frozenset[str],
) -> None:
    if _package_output_entries(output_dir) != expected:
        raise ReleaseFailure(
            "release.artifact.unexpected",
            "package output directory changed during publication",
        )


def _candidate_directory_identity(output_dir: Path) -> tuple[int, int]:
    try:
        directory = output_dir.lstat()
    except OSError as error:
        raise ReleaseFailure(
            "release.package.invalid",
            "unable to inspect Python candidate directory",
        ) from error
    if not stat.S_ISDIR(directory.st_mode) or output_dir.is_symlink():
        raise ReleaseFailure(
            "release.artifact.unexpected",
            "Python candidate directory changed during build",
        )
    return (directory.st_dev, directory.st_ino)


def _require_python_candidate_entries(
    output_dir: Path,
    identity: tuple[int, int],
    expected: frozenset[str],
) -> None:
    if _candidate_directory_identity(output_dir) != identity:
        raise ReleaseFailure(
            "release.artifact.unexpected",
            "Python candidate directory changed during build",
        )
    _require_package_output_entries(output_dir, expected)


def _python_candidate_artifact_identity(path: Path) -> tuple[int, int, int, str]:
    try:
        metadata = path.lstat()
        if not stat.S_ISREG(metadata.st_mode):
            raise OSError("candidate artifact is not a regular file")
        digest = _artifact_sha256(path)
    except (OSError, ReleaseFailure) as error:
        raise ReleaseFailure(
            "release.artifact.unexpected",
            "Python candidate artifact changed during validation",
        ) from error
    return (metadata.st_dev, metadata.st_ino, metadata.st_size, digest)


def _require_python_candidate_artifact_identity(
    path: Path,
    expected: tuple[int, int, int, str],
) -> None:
    if _python_candidate_artifact_identity(path) != expected:
        raise ReleaseFailure(
            "release.artifact.unexpected",
            "Python candidate artifact changed during validation",
        )


def _create_package_output_directory(output_dir: Path) -> None:
    try:
        output_dir.mkdir(parents=True)
    except FileExistsError as error:
        raise ReleaseFailure(
            "release.artifact.unexpected",
            "package output directory already exists",
        ) from error
    except OSError as error:
        raise ReleaseFailure(
            "release.package.invalid",
            "unable to create package output directory",
        ) from error
    if output_dir.is_symlink() or not output_dir.is_dir():
        _package_invalid("package output directory is invalid")


def _package_members(
    root: str,
    binary_name: str,
    binary: Path,
    manifest: Path,
    repo_root: Path,
) -> tuple[tuple[str, Path, int], ...]:
    files = (
        ("LICENSE", _package_file(repo_root / "LICENSE", "LICENSE"), 0o644),
        ("README.md", _package_file(repo_root / "README.md", "README.md"), 0o644),
        (
            "broker-manifest.json",
            _package_file(manifest, "broker-manifest.json"),
            0o644,
        ),
        (binary_name, _package_file(binary, binary_name), 0o755),
    )
    return tuple((f"{root}/{name}", path, mode) for name, path, mode in files)


def _freeze_package_inputs(
    staging_dir: Path,
    binary_name: str,
    binary: Path,
    manifest: Path,
    repo_root: Path,
) -> tuple[Path, Path]:
    inputs_dir = staging_dir / "inputs"
    inputs_dir.mkdir(mode=0o700)
    copies = (
        (binary, inputs_dir / binary_name),
        (manifest, inputs_dir / "broker-manifest.json"),
        (repo_root / "LICENSE", inputs_dir / "LICENSE"),
        (repo_root / "README.md", inputs_dir / "README.md"),
    )
    for source, destination in copies:
        _package_file(source, source.name)
        with source.open("rb") as input_file, destination.open("xb") as output_file:
            while chunk := input_file.read(ARCHIVE_READ_CHUNK_SIZE):
                output_file.write(chunk)
        destination.chmod(0o600)
    return (inputs_dir / binary_name, inputs_dir / "broker-manifest.json")


def _write_deterministic_tar(
    archive_path: Path,
    root: str,
    members: tuple[tuple[str, Path, int], ...],
) -> None:
    with archive_path.open("wb") as destination:
        with gzip.GzipFile(
            fileobj=destination,
            mode="wb",
            filename="",
            mtime=0,
        ) as compressed:
            with tarfile.open(fileobj=compressed, mode="w") as archive:
                directory = tarfile.TarInfo(root)
                directory.type = tarfile.DIRTYPE
                directory.mode = 0o755
                directory.uid = 0
                directory.gid = 0
                directory.uname = ""
                directory.gname = ""
                directory.mtime = 0
                archive.addfile(directory)
                for name, path, mode in members:
                    contents = path.read_bytes()
                    info = tarfile.TarInfo(name)
                    info.size = len(contents)
                    info.mode = mode
                    info.uid = 0
                    info.gid = 0
                    info.uname = ""
                    info.gname = ""
                    info.mtime = 0
                    archive.addfile(info, fileobj=_BytesReader(contents))


class _BytesReader:
    def __init__(self, contents: bytes) -> None:
        self.contents = contents
        self.offset = 0

    def read(self, size: int = -1) -> bytes:
        if size < 0:
            size = len(self.contents) - self.offset
        result = self.contents[self.offset : self.offset + size]
        self.offset += len(result)
        return result


def _write_deterministic_zip(
    archive_path: Path,
    root: str,
    members: tuple[tuple[str, Path, int], ...],
) -> None:
    with zipfile.ZipFile(
        archive_path,
        mode="w",
        compression=zipfile.ZIP_DEFLATED,
        compresslevel=9,
        strict_timestamps=True,
    ) as archive:
        directory = zipfile.ZipInfo(f"{root}/", date_time=PACKAGE_TIMESTAMP)
        directory.create_system = 3
        directory.external_attr = (0o40755 << 16) | 0x10
        archive.writestr(directory, b"")
        for name, path, mode in members:
            info = zipfile.ZipInfo(name, date_time=PACKAGE_TIMESTAMP)
            info.create_system = 3
            info.external_attr = (0o100000 | mode) << 16
            info.compress_type = zipfile.ZIP_DEFLATED
            archive.writestr(info, path.read_bytes(), compress_type=zipfile.ZIP_DEFLATED)


def package_broker(
    *,
    tag: str,
    target_name: str,
    targets_path: Path,
    binary_path: Path,
    output_dir: Path,
    manifest_path: Path,
    repo_root: Path,
) -> Path:
    staging_dir: Path | None = None
    reservation_path: Path | None = None
    published_archive: Path | None = None
    owns_reservation = False
    try:
        version = ReleaseVersion.parse(tag)
        targets = load_targets(targets_path)
        target = _target_by_name(targets, target_name)
        binary_name = _package_binary_name(target)
        binary = _package_file(binary_path, binary_name)
        manifest = _package_file(manifest_path, "broker-manifest.json")
        verify_broker_manifest(
            _read_json(manifest, "release.manifest.invalid"),
            target,
            binary,
            version,
        )
        resolved_root = repo_root.resolve()
        if not resolved_root.is_dir() or repo_root.is_symlink():
            _package_invalid("repository root is invalid")
        root = _package_root(version, target)
        _create_package_output_directory(output_dir)
        _require_package_output_entries(output_dir, frozenset())
        archive_path = output_dir / _package_archive_name(version, target)
        reservation_path = _package_reservation_path(output_dir, archive_path.name)
        _reserve_package_archive(reservation_path)
        owns_reservation = True
        staging_dir = Path(
            tempfile.mkdtemp(prefix=".package-broker-", dir=output_dir)
        )
        staging_dir.chmod(0o700)
        _require_package_output_entries(
            output_dir,
            frozenset({reservation_path.name, staging_dir.name}),
        )
        frozen_binary, frozen_manifest = _freeze_package_inputs(
            staging_dir,
            binary_name,
            binary,
            manifest,
            resolved_root,
        )
        verify_broker_manifest(
            _read_json(frozen_manifest, "release.manifest.invalid"),
            target,
            frozen_binary,
            version,
        )
        members = _package_members(
            root,
            binary_name,
            frozen_binary,
            frozen_manifest,
            staging_dir / "inputs",
        )
        staged_archive = staging_dir / archive_path.name
        if target.archive == "tar.gz":
            _write_deterministic_tar(staged_archive, root, members)
        elif target.archive == "zip":
            _write_deterministic_zip(staged_archive, root, members)
        else:
            _package_invalid("target archive format is unsupported")
        validate_archive(
            staged_archive,
            expected_root=root,
            expected_files={name.rsplit("/", 1)[1] for name, _, _ in members},
        )
        _require_package_output_entries(
            output_dir,
            frozenset({reservation_path.name, staging_dir.name}),
        )
        try:
            os.link(staged_archive, archive_path)
        except FileExistsError as error:
            raise ReleaseFailure(
                "release.artifact.unexpected",
                "final broker archive already exists",
            ) from error
        except OSError as error:
            raise ReleaseFailure(
                "release.package.invalid",
                "unable to publish final broker archive",
            ) from error
        published_archive = archive_path
        with contextlib.suppress(OSError):
            staged_archive.unlink()
    except ReleaseFailure as error:
        raise ReleaseFailure(error.name, _package_diagnostic(error.name)) from error
    except OSError as error:
        raise ReleaseFailure("release.package.invalid", "unable to package broker") from error
    finally:
        if staging_dir is not None:
            for path in sorted(staging_dir.rglob("*"), reverse=True):
                with contextlib.suppress(OSError):
                    if path.is_dir():
                        path.rmdir()
                    else:
                        path.unlink()
            with contextlib.suppress(OSError):
                staging_dir.rmdir()
        if owns_reservation and reservation_path is not None:
            with contextlib.suppress(OSError):
                reservation_path.unlink()
    if published_archive is None:
        _package_invalid("broker archive publication did not complete")
    return published_archive


def _package_output_directory(output_dir: Path) -> None:
    try:
        output_dir.mkdir(parents=True, exist_ok=True)
    except OSError as error:
        raise ReleaseFailure(
            "release.package.invalid",
            "unable to create package output directory",
        ) from error
    if output_dir.is_symlink() or not output_dir.is_dir():
        _package_invalid("package output directory is invalid")


def _published_artifact_path(output_dir: Path, name: str) -> Path:
    path = output_dir / name
    if path.exists() or path.is_symlink():
        raise ReleaseFailure(
            "release.artifact.unexpected",
            "final package artifact already exists",
        )
    return path


def _publish_staged_artifact(staged: Path, destination: Path) -> Path:
    try:
        os.link(staged, destination)
    except FileExistsError as error:
        raise ReleaseFailure(
            "release.artifact.unexpected",
            "final package artifact already exists",
        ) from error
    except OSError as error:
        raise ReleaseFailure(
            "release.package.invalid",
            "unable to publish final package artifact",
        ) from error
    return destination


PYTHON_GENERATED_FILES = frozenset(
    {
        "seeed_hal/proto/__init__.py",
        "seeed_hal/proto/hal_pb2.py",
    }
)


def _cargo_packageable_members(
    repo_root: Path,
    cargo: str = "cargo",
) -> tuple[dict[str, object], ...]:
    try:
        result = subprocess.run(
            [cargo, "metadata", "--no-deps", "--format-version", "1"],
            cwd=repo_root,
            check=False,
            capture_output=True,
            text=True,
            timeout=30,
        )
        metadata = json.loads(result.stdout) if result.returncode == 0 else None
        graph = subprocess.run(
            [cargo, "metadata", "--locked", "--format-version", "1"],
            cwd=repo_root,
            check=False,
            capture_output=True,
            text=True,
            timeout=30,
        )
        if graph.returncode != 0:
            metadata = None
        elif isinstance(metadata, dict):
            metadata["resolve"] = json.loads(graph.stdout).get("resolve")
    except (OSError, subprocess.TimeoutExpired, json.JSONDecodeError) as error:
        raise ReleaseFailure("release.cargo.failed", "cargo metadata failed") from error
    if not isinstance(metadata, dict):
        raise ReleaseFailure("release.cargo.failed", "cargo metadata failed")
    return _packageable_workspace_members(metadata)


def _packageable_workspace_members(metadata: dict[str, object]) -> tuple[dict[str, object], ...]:
    raw_packages = metadata.get("packages")
    raw_members = metadata.get("workspace_members")
    resolve = metadata.get("resolve")
    if not isinstance(raw_packages, list) or not isinstance(raw_members, list):
        _package_invalid("cargo metadata workspace members are invalid")
    packages_by_id = {
        package.get("id"): package
        for package in raw_packages
        if isinstance(package, dict) and isinstance(package.get("id"), str)
    }
    if not all(isinstance(member, str) and member in packages_by_id for member in raw_members):
        _package_invalid("cargo metadata workspace members are invalid")
    packageable = tuple(
        packages_by_id[member]
        for member in raw_members
        if packages_by_id[member].get("publish") != []
    )
    if not packageable:
        _package_invalid("workspace has no packageable crates")
    if any(
        not isinstance(package.get("name"), str) or not isinstance(package.get("id"), str)
        or not isinstance(package.get("version"), str)
        for package in packageable
    ):
        _package_invalid("cargo metadata package is invalid")
    if not isinstance(resolve, dict) or not isinstance(resolve.get("nodes"), list):
        _package_invalid("cargo metadata resolve graph is invalid")
    selected_ids = {str(package["id"]) for package in packageable}
    dependencies: dict[str, set[str]] = {identifier: set() for identifier in selected_ids}
    for node in resolve["nodes"]:
        if not isinstance(node, dict) or not isinstance(node.get("id"), str):
            _package_invalid("cargo metadata resolve graph is invalid")
        identifier = node["id"]
        if identifier not in selected_ids:
            continue
        raw_dependencies = node.get("dependencies")
        if not isinstance(raw_dependencies, list) or not all(
            isinstance(dependency, str) for dependency in raw_dependencies
        ):
            _package_invalid("cargo metadata resolve graph is invalid")
        dependencies[identifier] = {
            dependency for dependency in raw_dependencies if dependency in selected_ids
        }
    ordered: list[str] = []
    while dependencies:
        ready = sorted(
            (identifier for identifier, needs in dependencies.items() if not needs),
            key=lambda identifier: str(packages_by_id[identifier]["name"]),
        )
        if not ready:
            _package_invalid("cargo metadata resolve graph contains a cycle")
        for identifier in ready:
            ordered.append(identifier)
            dependencies.pop(identifier)
        for needs in dependencies.values():
            needs.difference_update(ready)
    return tuple(packages_by_id[identifier] for identifier in ordered)


def _require_clean_repository(repo_root: Path) -> None:
    try:
        result = subprocess.run(
            ["git", "status", "--porcelain=v1", "--untracked-files=all"],
            cwd=repo_root,
            check=False,
            capture_output=True,
            text=True,
            timeout=30,
        )
    except (OSError, subprocess.TimeoutExpired) as error:
        raise ReleaseFailure("release.package.invalid", "unable to verify repository state") from error
    if result.returncode == 0 and not result.stdout:
        return
    if result.returncode == 0:
        _package_invalid("repository has uncommitted changes")
    _package_invalid("unable to verify repository state")


def _crate_root_and_files(crate: Path) -> tuple[str, set[str]]:
    try:
        with tarfile.open(crate, "r:gz") as archive:
            members = archive.getmembers()
    except (OSError, tarfile.TarError) as error:
        raise ReleaseFailure("release.archive.invalid", "unable to inspect crate archive") from error
    if not members:
        _archive_invalid("crate archive is empty")
    paths = tuple(
        _archive_member_path(member.name, member.isdir()) for member in members
    )
    roots = {path.parts[0] for path in paths}
    if len(roots) != 1:
        _archive_invalid("crate archive root is invalid")
    root = roots.pop()
    files: set[str] = set()
    for member, path in zip(members, paths, strict=True):
        if path.parts[0] != root or member.issym() or member.islnk():
            _archive_invalid("crate archive member is invalid")
        if member.isfile():
            files.add(PurePosixPath(*path.parts[1:]).as_posix())
    if "Cargo.toml" not in files or not any(path.startswith("src/") for path in files):
        _archive_invalid("crate archive does not contain Rust package sources")
    return root, files


def _build_local_crate(
    package: dict[str, object],
    repo_root: Path,
    staging_dir: Path,
) -> Path:
    name = str(package["name"])
    version = str(package["version"])
    target_dir = staging_dir / "target"
    environment = {**os.environ, "CARGO_TARGET_DIR": str(target_dir)}
    try:
        result = subprocess.run(
            [
                "cargo",
                "package",
                "--package",
                name,
                "--locked",
                "--allow-dirty",
                "--no-verify",
            ],
            cwd=repo_root,
            check=False,
            capture_output=True,
            text=True,
            timeout=120,
            env=environment,
        )
    except (OSError, subprocess.TimeoutExpired) as error:
        raise ReleaseFailure("release.cargo.failed", "cargo package failed") from error
    if result.returncode != 0:
        raise ReleaseFailure(
            "release.cargo.failed",
            _bounded_subprocess_summary(result.stderr, "cargo package failed"),
        )
    crate = target_dir / "package" / f"{name}-{version}.crate"
    if not crate.is_file() or crate.is_symlink():
        _package_invalid("cargo package did not produce the expected crate")
    _crate_root_and_files(crate)
    return crate


def _check_packaged_crate(crate: Path, staging_dir: Path) -> None:
    root, _ = _crate_root_and_files(crate)
    extract_root = staging_dir / "checked" / root
    try:
        extract_root.parent.mkdir(parents=True, exist_ok=True)
        with tarfile.open(crate, "r:gz") as archive:
            archive.extractall(extract_root.parent, filter="data")
        result = subprocess.run(
            ["cargo", "check", "--locked"],
            cwd=extract_root,
            check=False,
            capture_output=True,
            text=True,
            timeout=120,
            env={**os.environ, "CARGO_TARGET_DIR": str(staging_dir / "check-target")},
        )
    except (OSError, tarfile.TarError, subprocess.TimeoutExpired) as error:
        raise ReleaseFailure("release.cargo.failed", "packaged crate validation failed") from error
    if result.returncode != 0:
        raise ReleaseFailure(
            "release.cargo.failed",
            _bounded_subprocess_summary(result.stderr, "packaged crate validation failed"),
        )


def package_rust(*, tag: str, repo_root: Path, output_dir: Path) -> Path:
    """Create a deterministic local source bundle without registry publication."""
    staging_dir: Path | None = None
    try:
        version = ReleaseVersion.parse(tag)
        resolved_root = repo_root.resolve()
        if repo_root.is_symlink() or not resolved_root.is_dir():
            _package_invalid("repository root is invalid")
        _require_clean_repository(resolved_root)
        packages = _cargo_packageable_members(resolved_root)
        if any(package["version"] != version.cargo for package in packages):
            _package_invalid("cargo package version does not match release tag")
        _package_output_directory(output_dir)
        archive_name = f"seeed-hal-crates-v{version.cargo}.tar.gz"
        destination = _published_artifact_path(output_dir, archive_name)
        staging_dir = Path(tempfile.mkdtemp(prefix=".package-rust-", dir=output_dir))
        staged_crates = tuple(
            _build_local_crate(package, resolved_root, staging_dir) for package in packages
        )
        for crate in staged_crates:
            _check_packaged_crate(crate, staging_dir)
        root = f"seeed-hal-crates-v{version.cargo}"
        members = tuple(
            (f"{root}/{crate.name}", crate, 0o644)
            for crate in sorted(staged_crates, key=lambda item: item.name)
        )
        staged_archive = staging_dir / archive_name
        _write_deterministic_tar(staged_archive, root, members)
        validate_archive(
            staged_archive,
            expected_root=root,
            expected_files={crate.name for crate in staged_crates},
        )
        return _publish_staged_artifact(staged_archive, destination)
    except ReleaseFailure:
        raise
    except OSError as error:
        raise ReleaseFailure("release.package.invalid", "unable to package Rust crates") from error
    finally:
        if staging_dir is not None:
            with contextlib.suppress(OSError):
                for path in sorted(staging_dir.rglob("*"), reverse=True):
                    if path.is_dir():
                        path.rmdir()
                    else:
                        path.unlink()
                staging_dir.rmdir()


def python_artifact_names(version: ReleaseVersion) -> tuple[str, str]:
    return (
        f"seeed_hal-{version.python}-py3-none-any.whl",
        f"seeed_hal-{version.python}.tar.gz",
    )


def _wheel_metadata(wheel: Path, version: ReleaseVersion) -> None:
    try:
        with zipfile.ZipFile(wheel) as archive:
            names = set(archive.namelist())
            dist_info = f"seeed_hal-{version.python}.dist-info"
            metadata_name = f"{dist_info}/METADATA"
            wheel_name = f"{dist_info}/WHEEL"
            if metadata_name not in names or wheel_name not in names:
                _package_invalid("Python wheel metadata is invalid")
            metadata = archive.read(metadata_name).decode("utf-8")
            wheel_data = archive.read(wheel_name)
            wheel_headers = BytesParser(policy=compat32).parsebytes(wheel_data)
    except (OSError, UnicodeError, zipfile.BadZipFile) as error:
        raise ReleaseFailure("release.package.invalid", "Python wheel metadata is invalid") from error
    if (
        "Name: seeed-hal\n" not in metadata
        or f"Version: {version.python}\n" not in metadata
        or wheel_headers.get_all("Root-Is-Purelib") != ["true"]
        or wheel_headers.get_all("Tag") != ["py3-none-any"]
        or "seeed_hal/__init__.py" not in names
        or not PYTHON_GENERATED_FILES.issubset(names)
    ):
        _package_invalid("Python wheel content is invalid")


def _sdist_metadata(sdist: Path, version: ReleaseVersion) -> None:
    try:
        with tarfile.open(sdist, "r:gz") as archive:
            members = archive.getmembers()
    except (OSError, tarfile.TarError) as error:
        raise ReleaseFailure(
            "release.package.invalid",
            "Python source distribution is invalid",
        ) from error
    if not members:
        _package_invalid("Python source distribution content is invalid")
    paths = tuple(
        _archive_member_path(member.name, member.isdir()) for member in members
    )
    roots = {path.parts[0] for path in paths}
    if len(roots) != 1:
        _package_invalid("Python source distribution content is invalid")
    root = roots.pop()
    files = set()
    for member, path in zip(members, paths, strict=True):
        if member.issym() or member.islnk() or path.parts[0] != root:
            _package_invalid("Python source distribution content is invalid")
        if member.isfile():
            files.add(PurePosixPath(*path.parts[1:]).as_posix())
    try:
        with tarfile.open(sdist, "r:gz") as archive:
            package_info = archive.extractfile(f"{root}/PKG-INFO")
            metadata = package_info.read().decode("utf-8") if package_info else ""
    except (OSError, tarfile.TarError, UnicodeError) as error:
        raise ReleaseFailure(
            "release.package.invalid",
            "Python source distribution metadata is invalid",
        ) from error
    if (
        root != f"seeed_hal-{version.python}"
        or "pyproject.toml" not in files
        or "seeed_hal/__init__.py" not in files
        or not PYTHON_GENERATED_FILES.issubset(files)
        or "Name: seeed-hal\n" not in metadata
        or f"Version: {version.python}\n" not in metadata
    ):
        _package_invalid("Python source distribution content is invalid")


def _verify_wheel_install(wheel: Path, version: ReleaseVersion, staging_dir: Path) -> None:
    virtualenv = staging_dir / "venv"
    python = virtualenv / ("Scripts/python.exe" if os.name == "nt" else "bin/python")
    environment = {
        key: value
        for key, value in os.environ.items()
        if key.upper() not in {"HTTP_PROXY", "HTTPS_PROXY", "ALL_PROXY", "NO_PROXY"}
    }
    try:
        for command in (
            [
                "uv",
                "venv",
                "--offline",
                "--no-project",
                "--python",
                sys.executable,
                str(virtualenv),
            ],
            [
                "uv",
                "pip",
                "install",
                "--python",
                str(python),
                "--offline",
                str(wheel),
            ],
            [
                str(python),
                "-I",
                "-c",
                (
                    "import importlib.metadata;"
                    "import seeed_hal;"
                    f"expected={version.python!r};"
                    "assert importlib.metadata.version('seeed-hal') == expected;"
                    "assert seeed_hal.__version__ == expected;"
                    "from seeed_hal.proto import hal_pb2;"
                    "assert hal_pb2.DESCRIPTOR.name == 'hal.proto';"
                    "assert hal_pb2.Empty().SerializeToString() == b''"
                ),
            ],
        ):
            result = subprocess.run(
                command,
                check=False,
                capture_output=True,
                text=True,
                timeout=120,
                env=environment,
            )
            if result.returncode != 0:
                _package_invalid("Python wheel installation validation failed")
    except (OSError, subprocess.TimeoutExpired) as error:
        raise ReleaseFailure(
            "release.package.invalid",
            "Python wheel installation validation failed",
        ) from error


def package_python(*, tag: str, project: Path, output_dir: Path) -> tuple[Path, Path]:
    """Build and validate a private wheel/sdist candidate pair.

    The caller must provide a path that does not exist.  This function creates
    and owns that candidate directory; it never publishes into a shared
    release directory.  Task 7 aggregates validated candidates separately.
    """
    try:
        version = ReleaseVersion.parse(tag)
        pyproject = _read_toml(project / "pyproject.toml", "release.package.invalid")
        metadata = pyproject.get("project")
        if not isinstance(metadata, dict) or metadata.get("version") != version.python:
            _package_invalid("Python project version does not match release tag")
        wheel_name, sdist_name = python_artifact_names(version)
        _create_package_output_directory(output_dir)
        output_dir.chmod(0o700)
        candidate_identity = _candidate_directory_identity(output_dir)
        result = subprocess.run(
            [
                "uv",
                "run",
                "--project",
                str(project),
                "--frozen",
                "python",
                "-m",
                "build",
                "--outdir",
                str(output_dir),
                str(project),
            ],
            cwd=project.parent.parent,
            check=False,
            capture_output=True,
            text=True,
            timeout=120,
        )
        if result.returncode != 0:
            _package_invalid(
                _bounded_subprocess_summary(result.stderr, "Python build failed")
            )
        wheel = output_dir / wheel_name
        sdist = output_dir / sdist_name
        expected = frozenset({wheel_name, sdist_name})
        _require_python_candidate_entries(output_dir, candidate_identity, expected)
        wheel_identity = _python_candidate_artifact_identity(wheel)
        sdist_identity = _python_candidate_artifact_identity(sdist)
        _wheel_metadata(wheel, version)
        _sdist_metadata(sdist, version)
        with tempfile.TemporaryDirectory(prefix=".package-python-validate-") as directory:
            _verify_wheel_install(wheel, version, Path(directory))
        _require_python_candidate_entries(output_dir, candidate_identity, expected)
        _require_python_candidate_artifact_identity(wheel, wheel_identity)
        _require_python_candidate_artifact_identity(sdist, sdist_identity)
        return (wheel, sdist)
    except (OSError, subprocess.TimeoutExpired) as error:
        raise ReleaseFailure("release.package.invalid", "Python build failed") from error


class _ReleaseArgumentParser(argparse.ArgumentParser):
    def error(self, message: str) -> NoReturn:
        raise ReleaseFailure("release.tool.invalid", "invalid command arguments")


def _parser() -> argparse.ArgumentParser:
    parser = _ReleaseArgumentParser(add_help=False)
    subcommands = parser.add_subparsers(dest="subcommand", required=True)
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
    package = subcommands.add_parser("package-broker")
    package.add_argument("--tag", required=True)
    package.add_argument("--target", required=True)
    package.add_argument("--targets", required=True, type=Path)
    package.add_argument("--binary", required=True, type=Path)
    package.add_argument("--output-dir", required=True, type=Path)
    package.add_argument("--manifest", required=True, type=Path)
    rust = subcommands.add_parser("package-rust")
    rust.add_argument("--tag", required=True)
    rust.add_argument("--repo-root", required=True, type=Path)
    rust.add_argument("--output-dir", required=True, type=Path)
    python = subcommands.add_parser(
        "package-python",
        help="build and validate a private Python candidate output pair",
    )
    python.add_argument("--tag", required=True)
    python.add_argument("--project", required=True, type=Path)
    python.add_argument("--candidate-dir", required=True, type=Path)
    generate = subcommands.add_parser("generate-manifest")
    generate.add_argument("--tag", required=True)
    generate.add_argument("--commit", required=True)
    generate.add_argument("--artifacts-dir", required=True, type=Path)
    generate.add_argument("--output-dir", required=True, type=Path)
    generate.add_argument("--software-qualification", required=True)
    generate.add_argument("--hardware-qualification", required=True)
    report = subcommands.add_parser("write-conformance-report")
    report.add_argument("--inputs", required=True, type=Path)
    report.add_argument("--output", required=True, type=Path)
    virtual = subcommands.add_parser("run-virtual-conformance")
    virtual.add_argument("--platform", required=True)
    virtual.add_argument("--broker", required=True, type=Path)
    virtual.add_argument("--repo-root", required=True, type=Path)
    virtual.add_argument("--command-identity", required=True)
    virtual.add_argument("--ref", required=True)
    target = subcommands.add_parser("print-target")
    target.add_argument("--targets", required=True, type=Path)
    target.add_argument("--format", required=True, choices=("github-matrix",))
    static = subcommands.add_parser("verify-static")
    static.add_argument("--release-dir", required=True, type=Path)
    artifacts = subcommands.add_parser("verify-artifacts")
    artifacts.add_argument("--tag", required=True)
    artifacts.add_argument("--artifacts-dir", required=True, type=Path)
    artifacts.add_argument("--targets", required=True, type=Path)
    artifacts.add_argument("--repo-root", required=True, type=Path)
    aggregate = subcommands.add_parser("aggregate-release")
    aggregate.add_argument("--tag", required=True)
    aggregate.add_argument("--commit", required=True)
    aggregate.add_argument("--broker-dir", required=True, type=Path)
    aggregate.add_argument("--rust-bundle", required=True, type=Path)
    aggregate.add_argument("--python-artifacts", required=True, type=Path)
    aggregate.add_argument("--report-inputs", required=True, type=Path)
    aggregate.add_argument("--release-dir", required=True, type=Path)
    return parser


def _fail(error: ReleaseFailure) -> NoReturn:
    print(f"{error.name}: {error.diagnostic}", file=sys.stderr)
    raise SystemExit(1)


def main(argv: Sequence[str] | None = None) -> int:
    try:
        arguments = _parser().parse_args(argv)
        if arguments.subcommand == "check-version":
            check_version(
                arguments.repo_root.resolve(),
                arguments.tag,
                arguments.cargo,
            )
        elif arguments.subcommand == "verify-broker-manifest":
            targets = load_targets(arguments.targets)
            verify_broker_manifest(
                _read_json(arguments.manifest, "release.manifest.invalid"),
                _target_by_name(targets, arguments.target),
                arguments.artifact,
                ReleaseVersion.parse(arguments.tag),
            )
        elif arguments.subcommand == "package-broker":
            package_broker(
                tag=arguments.tag,
                target_name=arguments.target,
                targets_path=arguments.targets,
                binary_path=arguments.binary,
                output_dir=arguments.output_dir,
                manifest_path=arguments.manifest,
                repo_root=Path(__file__).resolve().parents[2],
            )
        elif arguments.subcommand == "package-rust":
            package_rust(
                tag=arguments.tag,
                repo_root=arguments.repo_root,
                output_dir=arguments.output_dir,
            )
        elif arguments.subcommand == "package-python":
            package_python(
                tag=arguments.tag,
                project=arguments.project,
                output_dir=arguments.candidate_dir,
            )
        elif arguments.subcommand == "generate-manifest":
            manifest = generate_manifest(
                {
                    "tag": arguments.tag,
                    "commit": arguments.commit,
                    "artifacts_dir": arguments.artifacts_dir,
                    "software_qualification": {
                        "id": "software-conformance",
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
                (arguments.output_dir / CONFORMANCE_REPORT_NAME).write_bytes(
                    encode_conformance_report(initial_conformance_report(manifest))
                )
            except OSError as error:
                raise ReleaseFailure(
                    "release.manifest.invalid",
                    "unable to write release manifest output",
                ) from error
        elif arguments.subcommand == "write-conformance-report":
            write_conformance_report(arguments.inputs, arguments.output)
        elif arguments.subcommand == "run-virtual-conformance":
            print(
                json.dumps(
                    collect_virtual_conformance(
                        platform=arguments.platform,
                        broker=arguments.broker,
                        repo_root=arguments.repo_root,
                        command=arguments.command_identity,
                        ref=arguments.ref,
                    ),
                    sort_keys=True,
                    separators=(",", ":"),
                )
            )
        elif arguments.subcommand == "print-target":
            print(print_target_matrix(arguments.targets))
        elif arguments.subcommand == "verify-static":
            verify_static(arguments.release_dir)
        elif arguments.subcommand == "verify-artifacts":
            verify_artifacts(
                arguments.artifacts_dir,
                arguments.tag,
                arguments.targets,
                arguments.repo_root,
            )
        elif arguments.subcommand == "aggregate-release":
            aggregate_release(
                tag=arguments.tag,
                commit=arguments.commit,
                broker_dir=arguments.broker_dir,
                rust_bundle=arguments.rust_bundle,
                python_candidate=arguments.python_artifacts,
                report_inputs=arguments.report_inputs,
                release_dir=arguments.release_dir,
            )
    except ReleaseFailure as error:
        _fail(error)
    except OSError as error:
        _fail(ReleaseFailure("release.tool.failed", str(error)))
    except (TypeError, ValueError):
        _fail(ReleaseFailure("release.tool.invalid", "invalid command arguments"))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
