from __future__ import annotations

import json
import re
import sys
from pathlib import Path

import yaml


REPO_ROOT = Path(__file__).resolve().parents[2]
WORKFLOWS = REPO_ROOT / ".github" / "workflows"
CI = WORKFLOWS / "ci.yml"
RELEASE_TOOL = REPO_ROOT / "scripts" / "release" / "release_tool.py"


def load_workflow(name: str) -> dict[str, object]:
    text = (WORKFLOWS / name).read_text(encoding="utf-8")
    normalized = re.sub(r"(?m)^on:", '"on":', text)
    workflow = yaml.safe_load(normalized)
    assert isinstance(workflow, dict)
    return workflow


def test_ci_has_only_the_source_and_platform_conformance_jobs() -> None:
    workflow = load_workflow("ci.yml")

    assert workflow["name"] == "CI"
    assert workflow["on"] == {"push": None, "pull_request": None}
    assert set(workflow["jobs"]) == {"source-gate", "platform-conformance"}


def test_ci_has_read_only_permissions_and_bounded_jobs() -> None:
    workflow = load_workflow("ci.yml")

    assert workflow["permissions"] == {"contents": "read"}
    jobs = workflow["jobs"]
    assert isinstance(jobs, dict)
    for job in jobs.values():
        assert job["timeout-minutes"] == 45


def test_ci_does_not_publish_or_request_write_credentials() -> None:
    text = CI.read_text(encoding="utf-8")

    for forbidden in (
        "cargo publish",
        "twine upload",
        "id-token: write",
        "contents: write",
        "packages: write",
        "registry publish",
        "secrets.",
    ):
        assert forbidden not in text


def test_ci_uses_reviewed_actions_without_persisted_credentials() -> None:
    workflow = load_workflow("ci.yml")
    jobs = workflow["jobs"]
    assert isinstance(jobs, dict)
    steps = [step for job in jobs.values() for step in job["steps"]]

    checkouts = [step for step in steps if step.get("uses") == "actions/checkout@v6"]
    assert checkouts
    assert all(
        step.get("with", {}).get("persist-credentials") is False
        for step in checkouts
    )
    assert any(step.get("uses") == "actions/setup-python@v6" for step in steps)
    assert any(step.get("uses") == "astral-sh/setup-uv@v5" for step in steps)


def test_source_gate_declares_the_required_frozen_checks() -> None:
    workflow = load_workflow("ci.yml")
    jobs = workflow["jobs"]
    assert isinstance(jobs, dict)
    source_gate = jobs["source-gate"]
    commands = "\n".join(
        step["run"] for step in source_gate["steps"] if "run" in step
    )

    for command in (
        "rustup toolchain install 1.85",
        "./scripts/check-generated-protocol.sh",
        "cargo +1.85 fmt --all --check",
        "cargo +1.85 clippy --workspace --all-targets --all-features -- -D warnings",
        "cargo +1.85 test --workspace --all-features",
        "uv run --project bindings/python --python 3.11 --frozen pytest -q",
        "pytest -q tests/release",
        "test_minor_matrix.py",
    ):
        assert command in commands


def test_platform_matrix_is_derived_from_release_targets_without_adapter_copy() -> None:
    workflow = load_workflow("ci.yml")
    jobs = workflow["jobs"]
    assert isinstance(jobs, dict)
    matrix_job = jobs["source-gate"]
    platform_job = jobs["platform-conformance"]
    matrix_commands = "\n".join(
        step["run"] for step in matrix_job["steps"] if "run" in step
    )

    assert "print-target" in RELEASE_TOOL.read_text(encoding="utf-8")
    assert "release/targets.toml" in matrix_commands
    assert platform_job["strategy"]["matrix"]["include"] == (
        "${{ fromJSON(needs.source-gate.outputs.targets).include }}"
    )
    serialized = json.dumps(platform_job)
    for adapter in (
        "serialport",
        "nusb",
        "socketcan",
        "linux-gpio",
        "windows-gpio",
        "avfoundation",
        "v4l2",
        "mediafoundation",
    ):
        assert adapter not in serialized


def test_platform_job_separates_production_manifest_and_virtual_conformance() -> None:
    workflow = load_workflow("ci.yml")
    jobs = workflow["jobs"]
    assert isinstance(jobs, dict)
    platform_job = jobs["platform-conformance"]
    commands = "\n".join(
        step["run"] for step in platform_job["steps"] if "run" in step
    )

    assert "cargo +1.85 build -p seeed-hal-broker-app --no-default-features --features" in commands
    assert "verify-broker-manifest" in commands
    assert "cargo +1.85 build -p seeed-hal-broker-app --no-default-features --features virtual-adapters" in commands
    assert "run-virtual-conformance" in commands
    assert "actions/upload-artifact@v4" in json.dumps(platform_job)
