from __future__ import annotations

import json
import sys
from pathlib import Path

import pytest


REPO_ROOT = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(REPO_ROOT))

from scripts.release.release_tool import (
    ReleaseFailure,
    validate_conformance_report,
    write_conformance_report,
)


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
            "status": "Passed",
            "jobs": [
                {
                    "platform": "macos",
                    "result": "Passed",
                    "command": "verify-artifacts --tag v0.5.0-rc.1",
                    "ref": "https://example.invalid/jobs/macos",
                }
            ],
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
