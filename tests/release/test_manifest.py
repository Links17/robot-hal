from __future__ import annotations

import hashlib
import json
import subprocess
import sys
from dataclasses import replace
from pathlib import Path

import pytest


REPO_ROOT = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(REPO_ROOT))

from scripts.release.release_tool import (
    ArtifactRecord,
    ReleaseFailure,
    encode_manifest,
    generate_checksums,
    generate_manifest,
    validate_release_manifest,
    verify_static,
)


RELEASE_TOOL = REPO_ROOT / "scripts" / "release" / "release_tool.py"


def _broker_name(target: str) -> str:
    extension = "zip" if target == "x86_64-pc-windows-msvc" else "tar.gz"
    return f"seeed-hal-broker-v0.5.0-rc.1-{target}.{extension}"


def _write_artifacts(directory: Path) -> tuple[Path, ...]:
    names = (
        _broker_name("x86_64-pc-windows-msvc"),
        _broker_name("aarch64-apple-darwin"),
        _broker_name("x86_64-unknown-linux-gnu"),
        "seeed-hal-crates-v0.5.0-rc.1.tar.gz",
        "seeed_hal-0.5.0rc1-py3-none-any.whl",
        "seeed_hal-0.5.0rc1.tar.gz",
    )
    paths = tuple(directory / name for name in names)
    for index, path in enumerate(paths):
        path.write_bytes(f"artifact-{index}\n".encode())
    return paths


def _inputs(tmp_path: Path) -> dict[str, object]:
    artifacts = tmp_path / "artifacts"
    artifacts.mkdir(exist_ok=True)
    _write_artifacts(artifacts)
    return {
        "tag": "v0.5.0-rc.1",
        "commit": "a" * 40,
        "artifacts_dir": artifacts,
        "software_qualification": {
            "id": "software-conformance",
            "uri": "https://example.invalid/software-conformance",
        },
        "hardware_qualification": {
            "id": "hardware-qualification",
            "uri": "https://example.invalid/hardware-qualification",
        },
    }


def test_manifest_generation_is_byte_deterministic(tmp_path: Path) -> None:
    first = generate_manifest(_inputs(tmp_path))
    second = generate_manifest(_inputs(tmp_path))

    encoded = encode_manifest(first)
    assert encoded == encode_manifest(second)
    assert encoded.endswith(b"\n")
    assert b"timestamp" not in encoded
    assert [artifact["name"] for artifact in json.loads(encoded)["artifacts"]] == sorted(
        artifact.name for artifact in first.artifacts
    )


def test_checksums_are_byte_deterministic_and_basename_sorted(tmp_path: Path) -> None:
    manifest = generate_manifest(_inputs(tmp_path))
    first = generate_checksums(manifest)
    second = generate_checksums(manifest)

    assert first == second
    assert first == b"".join(
        f"{artifact.sha256}  {artifact.name}\n".encode()
        for artifact in manifest.artifacts
    )
    assert b"release-manifest.json" not in first


@pytest.mark.parametrize(
    "forbidden",
    ["startup_token", "mapping_name", "serial_number", "payload"],
)
def test_manifest_rejects_sensitive_field_names(forbidden: str, tmp_path: Path) -> None:
    data = generate_manifest(_inputs(tmp_path)).to_dict()
    data[forbidden] = "secret"

    with pytest.raises(ReleaseFailure) as failure:
        validate_release_manifest(data)

    assert failure.value.name == "release.manifest.invalid"
    assert "secret" not in failure.value.diagnostic


@pytest.mark.parametrize(
    "mutate",
    [
        lambda inputs: inputs.update(commit="short"),
        lambda inputs: inputs.update(tag="v0.5.0"),
        lambda inputs: (inputs["artifacts_dir"] / "unknown.bin").write_bytes(b"bad"),
        lambda inputs: (inputs["artifacts_dir"] / "nested").mkdir(),
        lambda inputs: (
            (inputs["artifacts_dir"] / "nested").mkdir(),
            (inputs["artifacts_dir"] / "nested" / "x").write_bytes(b"bad"),
        ),
    ],
)
def test_generation_fails_closed_for_invalid_release_inputs(
    tmp_path: Path,
    mutate,
) -> None:
    inputs = _inputs(tmp_path)
    mutate(inputs)

    with pytest.raises(ReleaseFailure) as failure:
        generate_manifest(inputs)

    assert failure.value.name == "release.manifest.invalid"


def test_validation_rejects_duplicate_names_and_invalid_sizes(tmp_path: Path) -> None:
    data = generate_manifest(_inputs(tmp_path)).to_dict()
    artifacts = data["artifacts"]
    assert isinstance(artifacts, list)
    artifacts[1]["name"] = artifacts[0]["name"]

    with pytest.raises(ReleaseFailure, match="release.manifest.invalid"):
        validate_release_manifest(data)

    data = generate_manifest(_inputs(tmp_path)).to_dict()
    artifacts = data["artifacts"]
    assert isinstance(artifacts, list)
    artifacts[0]["size"] = -1
    with pytest.raises(ReleaseFailure, match="release.manifest.invalid"):
        validate_release_manifest(data)


def test_manifest_rejects_identical_duplicate_record_without_hash_leak(
    tmp_path: Path,
) -> None:
    data = generate_manifest(_inputs(tmp_path)).to_dict()
    artifacts = data["artifacts"]
    assert isinstance(artifacts, list)
    duplicate = dict(artifacts[0])
    duplicate["sha256"] = "f" * 64
    artifacts.insert(1, duplicate)

    with pytest.raises(ReleaseFailure) as failure:
        validate_release_manifest(data)

    assert failure.value.name == "release.manifest.invalid"
    assert "f" * 64 not in failure.value.diagnostic
    manifest = generate_manifest(_inputs(tmp_path))
    duplicate_manifest = replace(
        manifest,
        artifacts=(
            manifest.artifacts[0],
            ArtifactRecord(**manifest.artifacts[0].to_dict()),
            *manifest.artifacts[1:],
        ),
    )
    with pytest.raises(ReleaseFailure) as encoded:
        encode_manifest(duplicate_manifest)
    assert encoded.value.name == "release.manifest.invalid"


def _write_release_directory(release_dir: Path, inputs: dict[str, object]) -> None:
    manifest = generate_manifest(inputs)
    for artifact in manifest.artifacts:
        source = inputs["artifacts_dir"] / artifact.name
        (release_dir / artifact.name).write_bytes(source.read_bytes())
    (release_dir / "release-manifest.json").write_bytes(encode_manifest(manifest))
    (release_dir / "SHA256SUMS").write_bytes(generate_checksums(manifest))
    (release_dir / "conformance-report.json").write_text(
        json.dumps(manifest.qualification.sidecar_dict(manifest.tag, manifest.commit)),
        encoding="utf-8",
    )


def test_static_verifier_requires_exact_single_release_directory(tmp_path: Path) -> None:
    inputs = _inputs(tmp_path)
    release_dir = tmp_path / "release"
    release_dir.mkdir()
    _write_release_directory(release_dir, inputs)

    verify_static(release_dir)

    (release_dir / "unexpected.txt").write_text("unexpected", encoding="utf-8")
    with pytest.raises(ReleaseFailure, match="release.manifest.invalid"):
        verify_static(release_dir)


def test_static_verifier_rejects_split_and_symlinked_release_paths(tmp_path: Path) -> None:
    inputs = _inputs(tmp_path)
    release_dir = tmp_path / "release"
    release_dir.mkdir()
    _write_release_directory(release_dir, inputs)
    sibling = tmp_path / "sibling"
    sibling.mkdir()
    manifest = release_dir / "release-manifest.json"
    checksums = release_dir / "SHA256SUMS"

    with pytest.raises(ReleaseFailure, match="release.manifest.invalid"):
        verify_static(release_dir, sibling / "release-manifest.json", checksums)

    (sibling / "release-manifest.json").symlink_to(manifest)
    with pytest.raises(ReleaseFailure, match="release.manifest.invalid"):
        verify_static(release_dir, sibling / "release-manifest.json", checksums)


def test_verify_static_rejects_mismatched_files_and_self_reference(tmp_path: Path) -> None:
    inputs = _inputs(tmp_path)
    release_dir = tmp_path / "release"
    release_dir.mkdir()
    _write_release_directory(release_dir, inputs)
    verify_static(release_dir)

    first = next(
        path
        for path in release_dir.iterdir()
        if path.name.startswith("seeed-")
    )
    first.write_bytes(b"tampered")
    with pytest.raises(ReleaseFailure, match="release.manifest.invalid"):
        verify_static(release_dir)

    first.write_bytes(b"artifact-0\n")
    data = generate_manifest(inputs).to_dict()
    data["artifacts"][0]["sha256"] = "0" * 64
    changed = validate_release_manifest(data)
    (release_dir / "release-manifest.json").write_bytes(encode_manifest(changed))
    (release_dir / "SHA256SUMS").write_bytes(generate_checksums(changed))
    with pytest.raises(ReleaseFailure, match="release.manifest.invalid"):
        verify_static(release_dir)

    data = generate_manifest(inputs).to_dict()
    data["artifacts"][0]["size"] += 1
    changed = validate_release_manifest(data)
    (release_dir / "release-manifest.json").write_bytes(encode_manifest(changed))
    (release_dir / "SHA256SUMS").write_bytes(generate_checksums(changed))
    with pytest.raises(ReleaseFailure, match="release.manifest.invalid"):
        verify_static(release_dir)


def test_cli_generates_manifest_and_checksums_without_self_reference(tmp_path: Path) -> None:
    inputs = _inputs(tmp_path)
    output = tmp_path / "output"
    result = subprocess.run(
        [
            sys.executable,
            str(RELEASE_TOOL),
            "generate-manifest",
            "--tag",
            "v0.5.0-rc.1",
            "--commit",
            "a" * 40,
            "--artifacts-dir",
            str(inputs["artifacts_dir"]),
            "--output-dir",
            str(output),
            "--software-qualification",
            "https://example.invalid/software-conformance",
            "--hardware-qualification",
            "https://example.invalid/hardware-qualification",
        ],
        check=False,
        capture_output=True,
        text=True,
        timeout=10,
    )

    assert result.returncode == 0, result.stderr
    manifest = json.loads((output / "release-manifest.json").read_text())
    assert "release-manifest.json" not in {item["name"] for item in manifest["artifacts"]}
    assert (output / "SHA256SUMS").read_bytes() == generate_checksums(
        validate_release_manifest(manifest)
    )


@pytest.mark.parametrize(
    "missing",
    [
        _broker_name("x86_64-pc-windows-msvc"),
        _broker_name("aarch64-apple-darwin"),
        _broker_name("x86_64-unknown-linux-gnu"),
        "seeed-hal-crates-v0.5.0-rc.1.tar.gz",
        "seeed_hal-0.5.0rc1-py3-none-any.whl",
        "seeed_hal-0.5.0rc1.tar.gz",
    ],
)
def test_manifest_requires_complete_exact_rc_artifact_set(
    tmp_path: Path,
    missing: str,
) -> None:
    inputs = _inputs(tmp_path)
    (inputs["artifacts_dir"] / missing).unlink()

    with pytest.raises(ReleaseFailure, match="release.manifest.invalid"):
        generate_manifest(inputs)


def test_manifest_rejects_artifact_version_that_does_not_match_tag(tmp_path: Path) -> None:
    inputs = _inputs(tmp_path)
    artifact = inputs["artifacts_dir"] / "seeed_hal-0.5.0rc1.tar.gz"
    artifact.rename(inputs["artifacts_dir"] / "seeed_hal-0.5.0rc2.tar.gz")

    with pytest.raises(ReleaseFailure, match="release.manifest.invalid"):
        generate_manifest(inputs)


def test_static_verifier_requires_controlled_conformance_report(tmp_path: Path) -> None:
    inputs = _inputs(tmp_path)
    manifest = generate_manifest(inputs)
    release_dir = tmp_path / "release"
    release_dir.mkdir()
    _write_release_directory(release_dir, inputs)
    (release_dir / "conformance-report.json").unlink()

    with pytest.raises(ReleaseFailure, match="release.manifest.invalid"):
        verify_static(release_dir)


@pytest.mark.parametrize(
    "uri",
    [
        "https://user:credential@example.invalid/report",
        "https://example.invalid/report?token=secret",
        "https://example.invalid/report#secret",
        "http://127.0.0.1/report",
        "https://[::1]/report",
        "https://localhost/report",
        "file:///private/report",
    ],
)
def test_manifest_rejects_unsafe_qualification_uri(tmp_path: Path, uri: str) -> None:
    inputs = _inputs(tmp_path)
    inputs["software_qualification"] = {
        "id": "software-conformance",
        "uri": uri,
    }

    with pytest.raises(ReleaseFailure) as failure:
        generate_manifest(inputs)

    assert failure.value.name == "release.manifest.invalid"
    assert "credential" not in failure.value.diagnostic
    assert "secret" not in failure.value.diagnostic


def test_cli_parse_errors_are_structured_and_do_not_echo_values() -> None:
    result = subprocess.run(
        [sys.executable, str(RELEASE_TOOL), "generate-manifest", "--commit", "secret-value"],
        check=False,
        capture_output=True,
        text=True,
        timeout=10,
    )

    assert result.returncode == 1
    assert result.stderr.startswith("release.tool.invalid:")
    assert "usage:" not in result.stderr
    assert "secret-value" not in result.stderr
