from __future__ import annotations

import json
import re
import sys
from pathlib import Path

import yaml


REPO_ROOT = Path(__file__).resolve().parents[2]
WORKFLOWS = REPO_ROOT / ".github" / "workflows"
CI = WORKFLOWS / "ci.yml"
RELEASE = WORKFLOWS / "release-rc.yml"
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


def _release_workflow() -> tuple[dict[str, object], dict[str, object], str]:
    workflow = load_workflow("release-rc.yml")
    jobs = workflow["jobs"]
    assert isinstance(jobs, dict)
    return workflow, jobs, RELEASE.read_text(encoding="utf-8")


def _job_commands(job: dict[str, object]) -> str:
    steps = job["steps"]
    assert isinstance(steps, list)
    return "\n".join(
        step["run"] for step in steps if isinstance(step, dict) and "run" in step
    )


def test_rc_release_is_manual_or_strict_rc_tag_only() -> None:
    workflow, _, text = _release_workflow()

    assert workflow["on"] == {
        "push": {"tags": ["v0.5.0-rc.*"]},
        "workflow_dispatch": {
            "inputs": {
                "version": {
                    "description": "Existing v0.5.0-rc.N tag to release",
                    "required": True,
                    "type": "string",
                },
                "dry_run": {
                    "description": "Verify only; do not attest or create a release",
                    "default": False,
                    "required": False,
                    "type": "boolean",
                },
            }
        },
    }
    assert "pull_request:" not in text
    assert "branches:" not in text
    assert "github.ref_type == 'tag'" in text
    assert "github.ref_name =~" not in text
    assert '[[ "$tag" =~ ^v0\\.5\\.0-rc\\.([1-9][0-9]*)$ ]]' in text


def test_rc_release_declares_only_required_jobs_and_read_only_default() -> None:
    workflow, jobs, _ = _release_workflow()

    assert workflow["permissions"] == {"contents": "read"}
    assert set(jobs) == {
        "validate",
        "platform-build",
        "client-build",
        "platform-verify",
        "aggregate",
        "attest-and-release",
    }
    for name, job in jobs.items():
        assert job["timeout-minutes"] == 45, name


def test_rc_release_grants_write_permissions_only_to_final_job() -> None:
    _, jobs, text = _release_workflow()

    assert jobs["attest-and-release"]["permissions"] == {
        "attestations": "write",
        "contents": "write",
        "id-token": "write",
    }
    for name, job in jobs.items():
        if name != "attest-and-release":
            assert "permissions" not in job, name
    assert "packages: write" not in text


def test_rc_release_uses_full_sha_pinned_actions_and_nonpersistent_checkout() -> None:
    _, jobs, text = _release_workflow()
    steps = [
        step
        for job in jobs.values()
        for step in job["steps"]
        if isinstance(step, dict)
    ]
    checkouts = [
        step for step in steps if step.get("uses", "").startswith("actions/checkout@")
    ]
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
    for action in uses:
        assert re.search(
            rf"(?m)^\s*(?:-\s+)?uses:\s+{re.escape(action)}\s+#\s+.+$",
            text,
        )


def test_rc_release_validate_job_fails_closed_on_identity_and_existing_release() -> None:
    _, jobs, _ = _release_workflow()
    commands = _job_commands(jobs["validate"])

    for required in (
        "git status --porcelain",
        "git rev-parse",
        "git rev-list -n 1",
        "git tag --points-at",
        "check-version",
        "gh release view",
        "git fetch --tags",
    ):
        assert required in commands
    assert "gh release delete" not in commands
    assert "git tag -f" not in commands
    assert "git push --force" not in commands
    assert "git fetch --force" not in commands


def test_rc_release_rechecks_one_remote_peeled_tag_before_and_after_publish() -> None:
    _, jobs, _ = _release_workflow()
    final_commands = _job_commands(jobs["attest-and-release"])

    assert "require_remote_peeled_tag()" in final_commands
    assert final_commands.count("require_remote_peeled_tag()") == 2
    assert final_commands.count("require_remote_peeled_tag\n") == 3
    assert "git ls-remote --tags origin" in final_commands
    assert "refs/tags/$TAG^{}" in final_commands
    assert 'test "$remote_commit" = "${{ needs.validate.outputs.commit }}"' in final_commands
    assert "awk" not in final_commands


def test_rc_release_builds_unique_verified_platform_and_client_artifacts() -> None:
    _, jobs, text = _release_workflow()
    platform_commands = _job_commands(jobs["platform-build"])
    client_commands = _job_commands(jobs["client-build"])

    for commands in (platform_commands, client_commands):
        assert "github.run_id" not in commands
    assert "${{ env.TAG }}" in text
    assert "${{ needs.validate.outputs.commit }}" in text
    for required in (
        "package-broker",
        "verify-broker-manifest",
        "virtual-adapters",
        "package-rust",
        "package-python",
    ):
        assert required in platform_commands + client_commands
    assert "release/" in text


def test_rc_release_derives_platform_matrices_from_release_targets() -> None:
    _, jobs, _ = _release_workflow()

    validate_commands = _job_commands(jobs["validate"])
    assert "print-target --targets release/targets.toml --format json" in validate_commands
    for name in ("platform-build", "platform-verify"):
        job = jobs[name]
        needs = job["needs"]
        assert "validate" in (needs if isinstance(needs, list) else [needs])
        assert job["strategy"]["matrix"]["include"] == (
            "${{ fromJSON(needs.validate.outputs.targets).include }}"
        )
    assert "macos-14" not in json.dumps(jobs["platform-build"])
    assert "windows-2025" not in json.dumps(jobs["platform-build"])


def test_rc_release_downloads_only_immutable_same_run_inputs_and_verifies_them() -> None:
    _, jobs, _ = _release_workflow()
    verify_commands = _job_commands(jobs["platform-verify"])
    aggregate_commands = _job_commands(jobs["aggregate"])
    serialized = json.dumps({"verify": jobs["platform-verify"], "aggregate": jobs["aggregate"]})

    assert "actions/download-artifact@" in serialized
    assert "github.run_id" not in json.dumps(
        [
            step
            for job in (jobs["platform-verify"], jobs["aggregate"])
            for step in job["steps"]
            if step.get("uses", "").startswith("actions/download-artifact@")
        ]
    )
    for commands in (verify_commands, aggregate_commands):
        assert "shasum -a 256 --check" in commands
    for required in (
        "verify-static",
        "verify-artifacts",
        "release_ready",
        "aggregate-platform-reports",
    ):
        assert required in aggregate_commands


def test_rc_release_gates_attestation_and_prerelease_after_aggregate() -> None:
    _, jobs, text = _release_workflow()
    final_job = jobs["attest-and-release"]
    final_commands = _job_commands(final_job)

    assert final_job["needs"] == ["validate", "aggregate"]
    assert final_job["if"] == "${{ !inputs.dry_run }}"
    assert "actions/attest@" in text
    assert "subject-path:" in text
    assert "gh release create" in final_commands
    assert "--prerelease" in final_commands
    assert "--latest=false" in final_commands
    assert "gh release view" in final_commands
    assert "gh release upload" not in final_commands
    assert "gh release edit" not in final_commands
    assert "release-manifest.json" in final_commands
    assert "SHA256SUMS" in final_commands
    assert "conformance-report.json" in final_commands
    assert "--prerelease --latest=false" in final_commands
    assert "--verify-tag" in final_commands


def test_rc_release_attests_and_publishes_only_exact_final_assets() -> None:
    _, _, text = _release_workflow()

    for required in (
        "seeed-hal-broker-v${TAG}-aarch64-apple-darwin.tar.gz",
        "seeed-hal-broker-v${TAG}-x86_64-unknown-linux-gnu.tar.gz",
        "seeed-hal-broker-v${TAG}-x86_64-pc-windows-msvc.zip",
        "seeed-hal-crates-v${TAG}.tar.gz",
        "seeed_hal-${PYTHON_VERSION}-py3-none-any.whl",
        "seeed_hal-${PYTHON_VERSION}.tar.gz",
    ):
        assert required in text
    for forbidden in (
        "virtual-broker",
        "candidate/",
        "cargo publish",
        "twine upload",
        "maturin publish",
        "PYPI",
        "CARGO_REGISTRY_TOKEN",
        "secrets.",
    ):
        assert forbidden not in text
