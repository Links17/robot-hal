from __future__ import annotations

import io
import hashlib
import sys
import stat
import subprocess
import zipfile
from pathlib import Path

import pytest

REPO_ROOT = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(REPO_ROOT))

from scripts.release import release_tool
from scripts.release.release_tool import (
    _locked_protobuf_wheel,
    ReleaseFailure,
    ReleaseVersion,
    _verify_wheel_install,
    _wheel_metadata,
    _prepare_locked_protobuf_wheel,
    python_artifact_names,
)


def test_python_artifact_names_use_pep440() -> None:
    assert python_artifact_names(ReleaseVersion.parse("v0.5.0-rc.3")) == (
        "seeed_hal-0.5.0rc3-py3-none-any.whl",
        "seeed_hal-0.5.0rc3.tar.gz",
    )


def test_python_package_requires_a_new_candidate_directory(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    project = REPO_ROOT / "bindings" / "python"
    output = tmp_path / "python-candidate"
    observed_build = False

    def should_not_build(*args, **kwargs) -> None:
        nonlocal observed_build
        observed_build = True
        raise AssertionError("build must not begin for an existing candidate directory")

    monkeypatch.setattr(release_tool.subprocess, "run", should_not_build)
    output.mkdir()
    sentinel = output / "external-input"
    sentinel.write_bytes(b"must remain untouched")

    with pytest.raises(ReleaseFailure) as failure:
        release_tool.package_python(
            tag="v0.5.0-rc.1",
            project=project,
            output_dir=output,
        )

    assert failure.value.name == "release.artifact.unexpected"
    assert not observed_build
    assert {path.name for path in output.iterdir()} == {sentinel.name}
    assert sentinel.read_bytes() == b"must remain untouched"


def test_python_package_does_not_create_reservations_for_candidate_outputs(
    tmp_path: Path,
) -> None:
    output = tmp_path / "python-candidate"
    wheel, sdist = release_tool.package_python(
        tag="v0.5.0-rc.1",
        project=REPO_ROOT / "bindings" / "python",
        output_dir=output,
    )

    assert wheel.parent == output
    assert sdist.parent == output
    assert not tuple(output.glob(".reserve-package-*"))


def test_failed_python_candidate_is_not_a_complete_release_directory(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    output = tmp_path / "python-candidate"

    def failed_build(command, **kwargs):
        return subprocess.CompletedProcess(command, 1, "", "build failed")

    monkeypatch.setattr(release_tool.subprocess, "run", failed_build)

    with pytest.raises(ReleaseFailure) as failure:
        release_tool.package_python(
            tag="v0.5.0-rc.1",
            project=REPO_ROOT / "bindings" / "python",
            output_dir=output,
        )

    assert failure.value.name == "release.package.invalid"
    assert output.is_dir()
    assert not tuple(output.glob(".reserve-package-*"))
    with pytest.raises(ReleaseFailure) as static_failure:
        release_tool.verify_static(output)
    assert static_failure.value.name == "release.manifest.invalid"


def test_python_candidate_rejects_external_write_during_validation(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    output = tmp_path / "python-candidate"
    original_verify = release_tool._verify_wheel_install

    def write_external_file(
        wheel: Path,
        version: ReleaseVersion,
        staging_dir: Path,
        protobuf_wheel: Path,
        protobuf_hash: str,
    ) -> None:
        original_verify(wheel, version, staging_dir, protobuf_wheel, protobuf_hash)
        (output / "external-write").write_bytes(b"unexpected")

    monkeypatch.setattr(release_tool, "_verify_wheel_install", write_external_file)

    with pytest.raises(ReleaseFailure) as failure:
        release_tool.package_python(
            tag="v0.5.0-rc.1",
            project=REPO_ROOT / "bindings" / "python",
            output_dir=output,
        )

    assert failure.value.name == "release.artifact.unexpected"
    assert (output / "external-write").read_bytes() == b"unexpected"
    with pytest.raises(ReleaseFailure):
        release_tool.verify_static(output)


def test_python_candidate_rejects_external_directory_replacement(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    output = tmp_path / "python-candidate"
    replacement = tmp_path / "external-replacement"
    original_verify = release_tool._verify_wheel_install

    def replace_candidate_directory(
        wheel: Path,
        version: ReleaseVersion,
        staging_dir: Path,
        protobuf_wheel: Path,
        protobuf_hash: str,
    ) -> None:
        original_verify(wheel, version, staging_dir, protobuf_wheel, protobuf_hash)
        output.rename(replacement)
        output.symlink_to(replacement, target_is_directory=True)

    monkeypatch.setattr(
        release_tool,
        "_verify_wheel_install",
        replace_candidate_directory,
    )

    with pytest.raises(ReleaseFailure) as failure:
        release_tool.package_python(
            tag="v0.5.0-rc.1",
            project=REPO_ROOT / "bindings" / "python",
            output_dir=output,
        )

    assert failure.value.name == "release.artifact.unexpected"
    assert output.is_symlink()
    assert {path.name for path in replacement.iterdir()} == {
        "seeed_hal-0.5.0rc1-py3-none-any.whl",
        "seeed_hal-0.5.0rc1.tar.gz",
    }


@pytest.mark.parametrize(
    ("artifact_name", "replacement_kind"),
    [
        ("seeed_hal-0.5.0rc1-py3-none-any.whl", "different-bytes"),
        ("seeed_hal-0.5.0rc1.tar.gz", "different-bytes"),
        ("seeed_hal-0.5.0rc1-py3-none-any.whl", "symlink"),
        ("seeed_hal-0.5.0rc1.tar.gz", "symlink"),
    ],
)
def test_python_candidate_rejects_artifact_replacement_after_validation(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
    artifact_name: str,
    replacement_kind: str,
) -> None:
    output = tmp_path / "python-candidate"
    replacement = tmp_path / "external-replacement"
    original_verify = release_tool._verify_wheel_install

    def replace_artifact(
        wheel: Path,
        version: ReleaseVersion,
        staging_dir: Path,
        protobuf_wheel: Path,
        protobuf_hash: str,
    ) -> None:
        original_verify(wheel, version, staging_dir, protobuf_wheel, protobuf_hash)
        artifact = output / artifact_name
        artifact.unlink()
        if replacement_kind == "symlink":
            replacement.write_bytes(b"external replacement")
            artifact.symlink_to(replacement)
        else:
            artifact.write_bytes(b"external replacement")

    monkeypatch.setattr(release_tool, "_verify_wheel_install", replace_artifact)

    with pytest.raises(ReleaseFailure) as failure:
        release_tool.package_python(
            tag="v0.5.0-rc.1",
            project=REPO_ROOT / "bindings" / "python",
            output_dir=output,
        )

    assert failure.value.name == "release.artifact.unexpected"
    assert not (output / "external-write").exists()


def test_package_python_cli_and_wrappers_describe_candidate_directory() -> None:
    arguments = release_tool._parser().parse_args(
        [
            "package-python",
            "--tag",
            "v0.5.0-rc.1",
            "--project",
            "bindings/python",
            "--candidate-dir",
            "target/python-candidate",
        ]
    )

    assert arguments.candidate_dir == Path("target/python-candidate")
    for wrapper in ("package-python.sh", "package-python.ps1"):
        contents = (REPO_ROOT / "scripts" / "release" / wrapper).read_text(
            encoding="utf-8"
        )
        assert "candidate" in contents.lower()


def test_python_package_builds_and_validates_complete_candidate_pair(tmp_path: Path) -> None:
    wheel, sdist = release_tool.package_python(
        tag="v0.5.0-rc.1",
        project=REPO_ROOT / "bindings" / "python",
        output_dir=tmp_path / "python-candidate",
    )

    assert wheel.is_file()
    assert sdist.is_file()
    assert stat.S_IMODE(wheel.parent.stat().st_mode) == 0o700
    assert {path.name for path in wheel.parent.iterdir()} == {wheel.name, sdist.name}


def _wheel(
    tmp_path: Path,
    *,
    dist_info: str = "seeed_hal-0.5.0rc1.dist-info",
    wheel_metadata: str = "Wheel-Version: 1.0\nRoot-Is-Purelib: true\nTag: py3-none-any\n",
    extra_members: dict[str, bytes] | None = None,
) -> Path:
    wheel = tmp_path / "seeed_hal-0.5.0rc1-py3-none-any.whl"
    with zipfile.ZipFile(wheel, "w") as archive:
        archive.writestr("seeed_hal/__init__.py", "")
        archive.writestr("seeed_hal/proto/__init__.py", "")
        archive.writestr("seeed_hal/proto/hal_pb2.py", "")
        archive.writestr(
            f"{dist_info}/METADATA",
            "Metadata-Version: 2.4\nName: seeed-hal\nVersion: 0.5.0rc1\n",
        )
        archive.writestr(f"{dist_info}/WHEEL", wheel_metadata)
        for name, contents in (extra_members or {}).items():
            archive.writestr(name, contents)
    return wheel


@pytest.mark.parametrize(
    ("dist_info", "wheel_metadata"),
    [
        (
            "wrong-0.5.0rc1.dist-info",
            "Wheel-Version: 1.0\nRoot-Is-Purelib: true\nTag: py3-none-any\n",
        ),
        (
            "seeed_hal-0.5.0rc1.dist-info",
            "Wheel-Version: 1.0\nRoot-Is-Purelib: false\nTag: py3-none-any\n",
        ),
        (
            "seeed_hal-0.5.0rc1.dist-info",
            "Wheel-Version: 1.0\nRoot-Is-Purelib: true\nTag: py3-none-any\nTag: cp311-none-any\n",
        ),
    ],
)
def test_wheel_metadata_rejects_noncanonical_internal_identity(
    tmp_path: Path,
    dist_info: str,
    wheel_metadata: str,
) -> None:
    with pytest.raises(ReleaseFailure):
        _wheel_metadata(
            _wheel(tmp_path, dist_info=dist_info, wheel_metadata=wheel_metadata),
            ReleaseVersion.parse("v0.5.0-rc.1"),
        )


def test_wheel_metadata_rejects_candidate_protobuf_shadowing(tmp_path: Path) -> None:
    with pytest.raises(ReleaseFailure) as failure:
        _wheel_metadata(
            _wheel(
                tmp_path,
                extra_members={"google/protobuf/__init__.py": b"__version__ = '6.32.1'"},
            ),
            ReleaseVersion.parse("v0.5.0-rc.1"),
        )

    assert failure.value.name == "release.package.invalid"


def test_wheel_metadata_rejects_backslash_protobuf_shadowing(tmp_path: Path) -> None:
    with pytest.raises(ReleaseFailure) as failure:
        _wheel_metadata(
            _wheel(
                tmp_path,
                extra_members={
                    r"google\protobuf\__init__.py": b"__version__ = '6.32.1'"
                },
            ),
            ReleaseVersion.parse("v0.5.0-rc.1"),
        )

    assert failure.value.name == "release.package.invalid"


def test_locked_protobuf_requires_direct_pure_python_wheel(tmp_path: Path) -> None:
    lock = tmp_path / "uv.lock"
    lock.write_text(
        """
version = 1
[[package]]
name = "protobuf"
version = "6.32.1"
wheels = [
    { url = "https://example.invalid/protobuf-cp39-abi3.whl", hash = "sha256:abc" },
]
""".strip(),
        encoding="utf-8",
    )

    with pytest.raises(ReleaseFailure) as failure:
        _locked_protobuf_wheel(tmp_path)

    assert failure.value.name == "release.package.invalid"


@pytest.mark.parametrize(
    "url",
    [
        "http://example.invalid/protobuf-6.32.1-py3-none-any.whl",
        "https://user:password@example.invalid/protobuf-6.32.1-py3-none-any.whl",
    ],
)
def test_locked_protobuf_rejects_insecure_or_credentialed_url(
    tmp_path: Path,
    url: str,
) -> None:
    (tmp_path / "uv.lock").write_text(
        f"""
version = 1
[[package]]
name = "protobuf"
version = "6.32.1"
wheels = [
    {{ url = "{url}", hash = "sha256:0000000000000000000000000000000000000000000000000000000000000000" }},
]
""".strip(),
        encoding="utf-8",
    )

    with pytest.raises(ReleaseFailure) as failure:
        _locked_protobuf_wheel(tmp_path)

    assert failure.value.name == "release.package.invalid"


def test_locked_protobuf_rejects_download_with_wrong_hash(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    lock = tmp_path / "uv.lock"
    lock.write_text(
        """
version = 1
[[package]]
name = "protobuf"
version = "6.32.1"
wheels = [
    { url = "https://example.invalid/protobuf-6.32.1-py3-none-any.whl", hash = "sha256:0000000000000000000000000000000000000000000000000000000000000000" },
]
""".strip(),
        encoding="utf-8",
    )
    class Response(io.BytesIO):
        def geturl(self) -> str:
            return "https://example.invalid/protobuf-6.32.1-py3-none-any.whl"

        def __enter__(self) -> Response:
            return self

        def __exit__(self, *args: object) -> None:
            self.close()

    class Opener:
        def open(self, url: str, timeout: int) -> Response:
            return Response(b"tampered wheel")

    monkeypatch.setattr(
        release_tool.urllib.request,
        "build_opener",
        lambda *handlers: Opener(),
    )

    with pytest.raises(ReleaseFailure) as failure:
        _prepare_locked_protobuf_wheel(tmp_path, tmp_path / "staging")

    assert failure.value.name == "release.package.invalid"
    assert not (tmp_path / "staging" / "protobuf-6.32.1-py3-none-any.whl").exists()


def test_locked_protobuf_rejects_redirect_to_non_https_url(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    wheel_bytes = b"locked protobuf wheel"
    digest = hashlib.sha256(wheel_bytes).hexdigest()
    (tmp_path / "uv.lock").write_text(
        f"""
version = 1
[[package]]
name = "protobuf"
version = "6.32.1"
wheels = [
    {{ url = "https://example.invalid/protobuf-6.32.1-py3-none-any.whl", hash = "sha256:{digest}" }},
]
""".strip(),
        encoding="utf-8",
    )

    class RedirectedResponse(io.BytesIO):
        def geturl(self) -> str:
            return "http://example.invalid/protobuf-6.32.1-py3-none-any.whl"

        def __enter__(self) -> RedirectedResponse:
            return self

        def __exit__(self, *args: object) -> None:
            self.close()

    class Opener:
        def open(self, url: str, timeout: int) -> RedirectedResponse:
            return RedirectedResponse(wheel_bytes)

    monkeypatch.setattr(
        release_tool.urllib.request,
        "build_opener",
        lambda *handlers: Opener(),
    )

    with pytest.raises(ReleaseFailure) as failure:
        _prepare_locked_protobuf_wheel(tmp_path, tmp_path / "staging")

    assert failure.value.name == "release.package.invalid"


def test_locked_protobuf_rejects_replacement_before_install(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    protobuf_wheel = tmp_path / "protobuf-6.32.1-py3-none-any.whl"
    protobuf_wheel.write_bytes(b"locked protobuf wheel")
    protobuf_hash = hashlib.sha256(protobuf_wheel.read_bytes()).hexdigest()
    commands: list[list[str]] = []

    def replace_after_venv(command, **kwargs):
        commands.append(command)
        if command[:2] == ["uv", "venv"]:
            protobuf_wheel.write_bytes(b"replaced protobuf wheel")
        return subprocess.CompletedProcess(command, 0, "", "")

    monkeypatch.setattr(release_tool.subprocess, "run", replace_after_venv)

    with pytest.raises(ReleaseFailure) as failure:
        _verify_wheel_install(
            tmp_path / "package.whl",
            ReleaseVersion.parse("v0.5.0-rc.1"),
            tmp_path / "staging",
            protobuf_wheel,
            protobuf_hash,
        )

    assert failure.value.name == "release.package.invalid"
    assert not any(command[:3] == ["uv", "pip", "install"] for command in commands)


def test_isolated_wheel_venv_is_offline_and_uses_current_interpreter(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    commands: list[list[str]] = []
    environments: list[dict[str, str]] = []
    protobuf_wheel = tmp_path / "protobuf-6.32.1-py3-none-any.whl"
    protobuf_wheel.write_bytes(b"locked protobuf wheel")
    protobuf_hash = hashlib.sha256(protobuf_wheel.read_bytes()).hexdigest()

    def record(command, **kwargs):
        commands.append(command)
        environments.append(kwargs["env"])
        return subprocess.CompletedProcess(command, 0, "", "")

    monkeypatch.setattr(release_tool.subprocess, "run", record)

    _verify_wheel_install(
        tmp_path / "package.whl",
        ReleaseVersion.parse("v0.5.0-rc.1"),
        tmp_path,
        protobuf_wheel,
        protobuf_hash,
    )

    assert commands[0][:4] == ["uv", "venv", "--offline", "--no-project"]
    assert commands[0][4:6] == ["--python", sys.executable]
    assert commands[1] == [
        "uv",
        "pip",
        "install",
        "--python",
        str(tmp_path / "venv" / ("Scripts/python.exe" if sys.platform == "win32" else "bin/python")),
        "--offline",
        "--no-deps",
        str(tmp_path / "package.whl"),
        str(protobuf_wheel),
    ]
    assert commands[2][:3] == [
        str(tmp_path / "venv" / ("Scripts/python.exe" if sys.platform == "win32" else "bin/python")),
        "-I",
        "-c",
    ]
    assert "import seeed_hal;" in commands[2][3]
    assert "from seeed_hal.proto import hal_pb2;" in commands[2][3]
    assert "hal_pb2.Empty().SerializeToString() == b''" in commands[2][3]
    assert "google.protobuf.__version__ == '6.32.1'" in commands[2][3]
    assert "google.protobuf.__file__" in commands[2][3]
    assert "RECORD" in commands[2][3]
    assert protobuf_hash in commands[2][3]
    assert all("HTTP_PROXY" not in environment for environment in environments)
    assert all("HTTPS_PROXY" not in environment for environment in environments)
