from __future__ import annotations

from pathlib import Path


REPO_ROOT = Path(__file__).resolve().parents[2]
SCRIPT = REPO_ROOT / "scripts" / "ci" / "install-linux-native-prerequisites.sh"
CI = REPO_ROOT / ".github" / "workflows" / "ci.yml"
RELEASE_RC = REPO_ROOT / ".github" / "workflows" / "release-rc.yml"


def test_linux_prerequisite_script_pins_and_verifies_libgpiod_v2() -> None:
    script = SCRIPT.read_text(encoding="utf-8")

    assert "LIBGPIOD_VERSION=2.2.1" in script
    assert (
        "LIBGPIOD_SHA256=95689033324c16a13c32e947b9933553258544d6538466b04859a5d1ba950798"
        in script
    )
    assert "https://www.kernel.org/pub/software/libs/libgpiod/" in script
    assert "sha256sum --check" in script
    assert "libudev-dev" in script
    assert "pkg-config" in script
    assert "DEBIAN_FRONTEND=noninteractive" in script
    assert "Acquire::Retries=" in script
    assert "Acquire::http::Timeout=" in script
    assert "pkg-config --exists 'libgpiod >= 2'" in script
    assert "pkg-config --exists libudev" in script
    assert 'PKG_CONFIG_PATH="$PREFIX/lib/pkgconfig' in script
    assert '>> "$GITHUB_ENV"' in script
    assert "libgpiod-dev" not in script


def test_linux_build_jobs_use_the_shared_prerequisite_script() -> None:
    ci = CI.read_text(encoding="utf-8")
    release_rc = RELEASE_RC.read_text(encoding="utf-8")

    for workflow in (ci, release_rc):
        assert "./scripts/ci/install-linux-native-prerequisites.sh" in workflow

    assert "source-gate" in ci
    source_gate = ci.split("  platform-conformance:", maxsplit=1)[0]
    assert "./scripts/ci/install-linux-native-prerequisites.sh" not in source_gate
    assert "--all-features" not in source_gate


def test_linux_production_broker_jobs_pin_the_runtime_loader_to_libgpiod_v2() -> None:
    script = SCRIPT.read_text(encoding="utf-8")
    ci = CI.read_text(encoding="utf-8")
    release_rc = RELEASE_RC.read_text(encoding="utf-8")

    assert 'test -f "$PREFIX/lib/libgpiod.so.3"' in script
    assert 'LD_LIBRARY_PATH="$PREFIX/lib${LD_LIBRARY_PATH:+:$LD_LIBRARY_PATH}"' in script
    assert 'printf \'LD_LIBRARY_PATH=%s\\n\' "$LD_LIBRARY_PATH" >> "$GITHUB_ENV"' in script

    for workflow, job in (
        (ci, "platform-conformance"),
        (release_rc, "platform-verify"),
    ):
        job_text = workflow.split(f"  {job}:", maxsplit=1)[1]
        assert "./scripts/ci/install-linux-native-prerequisites.sh" in job_text
        assert 'if: ${{ runner.os == \'Linux\' }}' in job_text


def test_workflows_retain_linux_v2_preflight_and_bounded_apt_policy() -> None:
    script = SCRIPT.read_text(encoding="utf-8")

    assert "sudo apt-get" in script
    assert " update" in script
    assert " install" in script
    assert "libgpiod >= 2" in script
