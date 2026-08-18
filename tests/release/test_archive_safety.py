from __future__ import annotations

import io
import sys
import tarfile
import warnings
import zipfile
from pathlib import Path

import pytest


REPO_ROOT = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(REPO_ROOT))

from scripts.release.release_tool import ReleaseFailure, validate_archive


def _tar(path: Path, members: list[tuple[str, bytes, str]]) -> None:
    with tarfile.open(path, "w:gz") as archive:
        for name, contents, kind in members:
            info = tarfile.TarInfo(name)
            if kind == "directory":
                info.type = tarfile.DIRTYPE
                archive.addfile(info)
            elif kind == "symlink":
                info.type = tarfile.SYMTYPE
                info.linkname = "target"
                archive.addfile(info)
            else:
                info.size = len(contents)
                archive.addfile(info, io.BytesIO(contents))


def _zip(path: Path, members: list[tuple[str, bytes, bool]]) -> None:
    with zipfile.ZipFile(path, "w") as archive:
        for name, contents, symlink in members:
            info = zipfile.ZipInfo(name)
            if symlink:
                info.external_attr = 0o120777 << 16
            archive.writestr(info, contents)


def test_valid_tar_and_zip_are_inspected_without_extraction(tmp_path: Path) -> None:
    tar_path = tmp_path / "valid.tar.gz"
    zip_path = tmp_path / "valid.zip"
    _tar(
        tar_path,
        [
            ("seeed-hal-broker-v0.5.0-rc.1/", b"", "directory"),
            ("seeed-hal-broker-v0.5.0-rc.1/README.txt", b"ok", "file"),
        ],
    )
    _zip(
        zip_path,
        [
            ("seeed-hal-broker-v0.5.0-rc.1/", b"", False),
            ("seeed-hal-broker-v0.5.0-rc.1/README.txt", b"ok", False),
        ],
    )

    validate_archive(
        tar_path,
        expected_root="seeed-hal-broker-v0.5.0-rc.1",
        expected_files={"README.txt"},
    )
    validate_archive(
        zip_path,
        expected_root="seeed-hal-broker-v0.5.0-rc.1",
        expected_files={"README.txt"},
    )


@pytest.mark.parametrize(
    "name",
    [
        "../escape",
        "/absolute",
        "./dot",
        "root\\windows",
        "C:/drive",
        "root//empty",
        "root//",
    ],
)
def test_tar_rejects_unsafe_member_names_before_extraction(
    tmp_path: Path,
    name: str,
) -> None:
    archive = tmp_path / "unsafe.tar.gz"
    _tar(archive, [(name, b"bad", "file")])

    with pytest.raises(ReleaseFailure) as failure:
        validate_archive(archive, expected_root="root", expected_files={"README.txt"})

    assert failure.value.name == "release.archive.invalid"


@pytest.mark.parametrize(
    "name",
    [
        "../escape",
        "/absolute",
        "./dot",
        "root\\windows",
        "C:/drive",
        "root//empty",
        "root//",
    ],
)
def test_zip_rejects_unsafe_member_names_before_extraction(
    tmp_path: Path,
    name: str,
) -> None:
    archive = tmp_path / "unsafe.zip"
    _zip(archive, [(name, b"bad", False)])

    with pytest.raises(ReleaseFailure) as failure:
        validate_archive(archive, expected_root="root", expected_files={"README.txt"})

    assert failure.value.name == "release.archive.invalid"


@pytest.mark.parametrize("suffix", [".tar.gz", ".zip"])
def test_archive_rejects_symlink_duplicate_and_unexpected_content(
    tmp_path: Path,
    suffix: str,
) -> None:
    archive = tmp_path / f"unsafe{suffix}"
    if suffix == ".tar.gz":
        _tar(
            archive,
            [
                ("root/", b"", "directory"),
                ("root/README.txt", b"ok", "file"),
                ("root/link", b"", "symlink"),
            ],
        )
    else:
        _zip(
            archive,
            [
                ("root/", b"", False),
                ("root/README.txt", b"ok", False),
                ("root/link", b"target", True),
            ],
        )

    with pytest.raises(ReleaseFailure, match="release.archive.invalid"):
        validate_archive(archive, expected_root="root", expected_files={"README.txt"})

    if suffix == ".tar.gz":
        _tar(
            archive,
            [
                ("root/", b"", "directory"),
                ("root/README.txt", b"one", "file"),
                ("root/README.txt", b"two", "file"),
            ],
        )
    else:
        with warnings.catch_warnings():
            warnings.simplefilter("ignore", UserWarning)
            _zip(
                archive,
                [
                    ("root/", b"", False),
                    ("root/README.txt", b"one", False),
                    ("root/README.txt", b"two", False),
                ],
            )
    with pytest.raises(ReleaseFailure, match="release.archive.invalid"):
        validate_archive(archive, expected_root="root", expected_files={"README.txt"})

    if suffix == ".tar.gz":
        _tar(
            archive,
            [
                ("root/", b"", "directory"),
                ("root/README.txt", b"ok", "file"),
                ("root/unexpected.bin", b"bad", "file"),
            ],
        )
    else:
        _zip(
            archive,
            [
                ("root/", b"", False),
                ("root/README.txt", b"ok", False),
                ("root/unexpected.bin", b"bad", False),
            ],
        )
    with pytest.raises(ReleaseFailure, match="release.archive.invalid"):
        validate_archive(archive, expected_root="root", expected_files={"README.txt"})
