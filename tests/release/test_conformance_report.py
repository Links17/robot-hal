from __future__ import annotations

import json
import subprocess
import sys
from pathlib import Path

import pytest


REPO_ROOT = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(REPO_ROOT))

from scripts.release.release_tool import (
    ReleaseFailure,
    release_ready,
    validate_conformance_report,
    write_conformance_report,
)

RELEASE_TOOL = REPO_ROOT / "scripts" / "release" / "release_tool.py"


def _report() -> dict[str, object]:
    return {
        "schema": 1,
        "tag": "v0.5.0-rc.1",
        "commit": "a" * 40,
        "qualification": {
            "software": {
                "id": "software-conformance",
                "uri": "https://example.invalid/software",
            },
            "hardware": {
                "id": "hardware-qualification",
                "uri": "https://example.invalid/hardware",
            },
        },
        "software": {
            "status": "Partial",
            "jobs": [
                {
                    "platform": "macos",
                    "result": "Passed",
                    "command": "verify-artifacts --tag v0.5.0-rc.1",
                    "ref": "https://example.invalid/jobs/macos",
                }
            ],
            "virtual": [],
        },
        "hardware": {
            "camera-avfoundation": {
                "status": "Pending",
                "evidence": None,
            },
            "camera-v4l2": {"status": "Blocked", "evidence": None},
        },
    }


def test_software_report_cannot_promote_pending_hardware() -> None:
    report = _report()
    hardware = report["hardware"]
    assert isinstance(hardware, dict)
    camera = hardware["camera-avfoundation"]
    assert isinstance(camera, dict)
    camera["status"] = "Passed"

    with pytest.raises(ReleaseFailure, match="release.manifest.invalid"):
        validate_conformance_report(report)


@pytest.mark.parametrize(
    "field",
    ["token", "serial_number", "endpoint", "payload"],
)
def test_report_rejects_sensitive_evidence_and_command_fields(field: str) -> None:
    report = _report()
    software = report["software"]
    assert isinstance(software, dict)
    jobs = software["jobs"]
    assert isinstance(jobs, list)
    job = jobs[0]
    assert isinstance(job, dict)
    job[field] = "secret"

    with pytest.raises(ReleaseFailure, match="release.manifest.invalid"):
        validate_conformance_report(report)


def test_report_requires_bounded_job_identity_and_public_ref() -> None:
    report = _report()
    software = report["software"]
    assert isinstance(software, dict)
    jobs = software["jobs"]
    assert isinstance(jobs, list)
    job = jobs[0]
    assert isinstance(job, dict)
    job["command"] = "x" * 257

    with pytest.raises(ReleaseFailure, match="release.manifest.invalid"):
        validate_conformance_report(report)


def test_write_report_reads_exact_input_and_never_promotes_hardware(
    tmp_path: Path,
) -> None:
    inputs = tmp_path / "report-inputs"
    inputs.mkdir()
    (inputs / "conformance-report.json").write_text(
        json.dumps(_report()),
        encoding="utf-8",
    )
    output = tmp_path / "conformance-report.json"

    write_conformance_report(inputs, output)

    assert json.loads(output.read_text(encoding="utf-8")) == _report()
    assert (inputs / "conformance-report.json").read_text(encoding="utf-8") == json.dumps(
        _report()
    )


def _passed_report() -> dict[str, object]:
    report = _report()
    software = report["software"]
    assert isinstance(software, dict)
    software["status"] = "Passed"
    software["jobs"] = [
        {
            "platform": platform,
            "result": "Passed",
            "command": "verify-artifacts --tag v0.5.0-rc.1",
            "ref": f"https://example.invalid/jobs/{platform}",
        }
        for platform in ("macos", "linux", "windows")
    ]
    software["virtual"] = [
        {
            "platform": platform,
            "protocol_minor": minor,
            "result": "Passed",
            "command": "run-broker-conformance",
            "ref": f"https://example.invalid/jobs/{platform}/minor-{minor}",
        }
        for platform in ("macos", "linux", "windows")
        for minor in range(4)
    ]
    return report


def test_passed_software_requires_each_platform_and_virtual_minor() -> None:
    report = _passed_report()
    software = report["software"]
    assert isinstance(software, dict)
    virtual = software["virtual"]
    assert isinstance(virtual, list)
    virtual.pop()

    with pytest.raises(ReleaseFailure, match="release.conformance.incomplete"):
        validate_conformance_report(report)


def test_release_ready_rejects_factual_partial_report() -> None:
    report = validate_conformance_report(_report())

    with pytest.raises(ReleaseFailure, match="release.conformance.incomplete"):
        release_ready(report)


def test_virtual_conformance_cli_dispatches_and_hides_broker_path(tmp_path: Path) -> None:
    broker = tmp_path / "secret-broker"
    result = subprocess.run(
        [
            sys.executable,
            str(RELEASE_TOOL),
            "run-virtual-conformance",
            "--platform",
            "macos",
            "--broker",
            str(broker),
            "--repo-root",
            str(REPO_ROOT),
            "--command-identity",
            "hosted virtual conformance",
            "--ref",
            "https://example.invalid/jobs/macos",
        ],
        check=False,
        capture_output=True,
        text=True,
        timeout=10,
    )

    assert result.returncode == 1
    assert result.stderr.startswith("release.conformance.invalid:")
    assert str(broker) not in result.stderr


def test_virtual_conformance_cli_rejects_invalid_platform_before_host_check(
    tmp_path: Path,
) -> None:
    broker = tmp_path / "secret-broker"
    result = subprocess.run(
        [
            sys.executable,
            str(RELEASE_TOOL),
            "run-virtual-conformance",
            "--platform",
            "unsupported",
            "--broker",
            str(broker),
            "--repo-root",
            str(REPO_ROOT),
            "--command-identity",
            "hosted virtual conformance",
            "--ref",
            "https://example.invalid/jobs/unsupported",
        ],
        check=False,
        capture_output=True,
        text=True,
        timeout=10,
    )

    assert result.returncode == 1
    assert result.stderr.startswith("release.conformance.invalid:")
    assert str(broker) not in result.stderr


def test_virtual_conformance_cli_rejects_invalid_broker_before_host_check(
    tmp_path: Path,
) -> None:
    broker = tmp_path / "secret-broker"
    result = subprocess.run(
        [
            sys.executable,
            str(RELEASE_TOOL),
            "run-virtual-conformance",
            "--platform",
            "macos",
            "--broker",
            str(broker),
            "--repo-root",
            str(REPO_ROOT),
            "--command-identity",
            "hosted virtual conformance",
            "--ref",
            "https://example.invalid/jobs/macos",
        ],
        check=False,
        capture_output=True,
        text=True,
        timeout=10,
    )

    assert result.returncode == 1
    assert result.stderr.startswith("release.conformance.invalid:")
    assert str(broker) not in result.stderr


def test_virtual_conformance_cli_preserves_command_identity(
    monkeypatch: pytest.MonkeyPatch,
    tmp_path: Path,
) -> None:
    broker = tmp_path / "broker"
    broker.write_bytes(b"fixture")
    observed: dict[str, str] = {}

    def record(*, platform, broker, repo_root, command, ref):
        observed["command"] = command
        return []

    monkeypatch.setattr("scripts.release.release_tool.collect_virtual_conformance", record)

    assert __import__("scripts.release.release_tool", fromlist=["main"]).main(
        [
            "run-virtual-conformance",
            "--platform",
            "macos",
            "--broker",
            str(broker),
            "--repo-root",
            str(REPO_ROOT),
            "--command-identity",
            "hosted virtual conformance",
            "--ref",
            "https://example.invalid/jobs/macos",
        ]
    ) == 0
    assert observed["command"] == "hosted virtual conformance"
