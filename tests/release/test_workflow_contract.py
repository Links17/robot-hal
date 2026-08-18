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

    checkouts = [step for step in steps if step.get("uses", "").startswith("actions/checkout@")]
    assert checkouts
    assert all(
        step.get("with", {}).get("persist-credentials") is False
        for step in checkouts
    )

    uses = [step["uses"] for step in steps if "uses" in step]
    assert uses
    assert all(
        re.fullmatch(r"[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+@[0-9a-f]{40}", action)
        for action in uses
    )

    workflow_text = CI.read_text(encoding="utf-8")
    for action in uses:
        assert re.search(
            rf"(?m)^\s*(?:-\s+)?uses:\s+{re.escape(action)}\s+#\s+.+$",
            workflow_text,
        )
    assert "astral-sh/setup-uv@d4b2f3b6ecc6e67c4457f6d3e41ec42d3d0fcb86 # v5" in workflow_text
    assert "astral-sh/setup-uv@e58605a9b6da7c637471fab8847a5e5a6b8df081" not in workflow_text


def test_source_gate_declares_the_required_frozen_checks() -> None:
    workflow = load_workflow("ci.yml")
    jobs = workflow["jobs"]
    assert isinstance(jobs, dict)
    source_gate = jobs["source-gate"]
    commands = "\n".join(
        step["run"] for step in source_gate["steps"] if "run" in step
    )

    for command in (
        "rustup toolchain install 1.85 --profile minimal --component rustfmt --component clippy",
        "./scripts/check-generated-protocol.sh",
        "cargo +1.85 fmt --all --check",
        "cargo +1.85 clippy --workspace --all-targets --all-features -- -D warnings",
        "cargo +1.85 test --workspace --all-features",
        "uv run --project bindings/python --python 3.11 --frozen pytest -q",
        "pytest -q tests/release",
        "test_minor_matrix.py",
    ):
        assert command in commands


def test_linux_jobs_install_libgpiod_before_linux_gpio_builds() -> None:
    workflow = load_workflow("ci.yml")
    jobs = workflow["jobs"]
    assert isinstance(jobs, dict)
    source_gate = jobs["source-gate"]
    platform_job = jobs["platform-conformance"]
    source_steps = source_gate["steps"]
    platform_steps = platform_job["steps"]
    source_prepare = next(
        step for step in source_steps if step.get("name") == "Install Linux build prerequisites"
    )
    platform_prepare = next(
        step
        for step in platform_steps
        if step.get("name") == "Install Linux build prerequisites"
    )

    for step in (source_prepare, platform_prepare):
        assert step["if"] == "${{ runner.os == 'Linux' }}"
        assert step["run"].strip() == (
            "sudo apt-get update\n"
            "sudo apt-get install --yes libgpiod-dev pkg-config"
        )

    source_prepare_index = source_steps.index(source_prepare)
    source_clippy_index = next(
        index
        for index, step in enumerate(source_steps)
        if "cargo +1.85 clippy" in step.get("run", "")
    )
    platform_prepare_index = platform_steps.index(platform_prepare)
    platform_build_index = next(
        index
        for index, step in enumerate(platform_steps)
        if "cargo +1.85 build" in step.get("run", "")
    )
    assert source_prepare_index < source_clippy_index
    assert platform_prepare_index < platform_build_index


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
    assert "actions/upload-artifact@ea165f8d65b6e75b540449e92b4886f43607fa02" in json.dumps(
        platform_job
    )
