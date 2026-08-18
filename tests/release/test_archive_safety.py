from __future__ import annotations

import io
import gzip
import sys
import tarfile
import unicodedata
import warnings
import zipfile
from pathlib import Path

import pytest


REPO_ROOT = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(REPO_ROOT))

from scripts.release import release_tool
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


def _raw_tar_members(path: Path, members: list[tuple[str, bytes, bool]]) -> None:
    """Write directory headers without tarfile's trailing-slash normalization."""
    payload = bytearray()
    for name, contents, is_directory in members:
        header = bytearray(512)
        encoded = name.encode("utf-8")
        header[: len(encoded)] = encoded
        header[100:108] = b"0000755\x00"
        header[108:116] = b"0000000\x00"
        header[116:124] = b"0000000\x00"
        header[124:136] = f"{len(contents):011o}\0".encode("ascii")
        header[136:148] = b"00000000000\x00"
        header[148:156] = b"        "
        header[156:157] = b"5" if is_directory else b"0"
        header[257:263] = b"ustar\x00"
        header[263:265] = b"00"
        checksum = sum(header)
        header[148:156] = f"{checksum:06o}\0 ".encode("ascii")
        payload.extend(header)
        if not is_directory:
            payload.extend(contents)
            payload.extend(b"\0" * (-len(contents) % 512))
    payload.extend(b"\0" * 1024)
    with gzip.GzipFile(path, "wb", mtime=0) as archive:
        archive.write(payload)


def _raw_tar_with_claimed_size(path: Path, size: int) -> None:
    payload = bytearray(512)
    payload[: len(b"root/short.txt")] = b"root/short.txt"
    payload[100:108] = b"0000644\x00"
    payload[108:116] = b"0000000\x00"
    payload[116:124] = b"0000000\x00"
    payload[124:136] = f"{size:011o}\0".encode("ascii")
    payload[136:148] = b"00000000000\x00"
    payload[148:156] = b"        "
    payload[156:157] = b"0"
    payload[257:263] = b"ustar\x00"
    payload[263:265] = b"00"
    payload[148:156] = f"{sum(payload):06o}\0 ".encode("ascii")
    payload.extend(b"short")
    payload.extend(b"\0" * 1024)
    with gzip.GzipFile(path, "wb", mtime=0) as archive:
        archive.write(payload)


def test_raw_tar_size_is_consumed_in_bounded_chunks(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    archive_path = tmp_path / "claimed-size.tar.gz"
    _raw_tar_with_claimed_size(archive_path, 8 * 1024 * 1024 * 1024)
    original_open = gzip.open

    class BoundedReadArchive:
        def __init__(self, archive) -> None:
            self.archive = archive

        def __enter__(self):
            self.archive.__enter__()
            return self

        def __exit__(self, *args) -> None:
            self.archive.__exit__(*args)

        def read(self, size: int = -1) -> bytes:
            assert size <= 64 * 1024
            return self.archive.read(size)

    monkeypatch.setattr(
        release_tool.gzip,
        "open",
        lambda *args, **kwargs: BoundedReadArchive(original_open(*args, **kwargs)),
    )

    with pytest.raises(ReleaseFailure, match="release.archive.invalid"):
        validate_archive(
            archive_path,
            expected_root="root",
            expected_files={"short.txt"},
        )


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
@pytest.mark.parametrize("suffix", [".tar.gz", ".zip"])
def test_archive_rejects_casefold_and_unicode_nfc_collisions(
    tmp_path: Path,
    suffix: str,
) -> None:
    archive = tmp_path / f"collision{suffix}"
    members = [
        ("root/", b"", "directory"),
        ("root/README.txt", b"one", "file"),
        ("root/readme.TXT", b"two", "file"),
    ]
    if suffix == ".tar.gz":
        _tar(archive, members)
    else:
        _zip(
            archive,
            [(name, contents, False) for name, contents, _ in members],
        )
    with pytest.raises(ReleaseFailure, match="release.archive.invalid"):
        validate_archive(archive, expected_root="root", expected_files={"README.txt"})

    decomposed = "root/cafe\u0301.txt"
    composed_name = unicodedata.normalize("NFC", "cafe\u0301")
    composed = f"root/{composed_name}.txt"
    members = [
        ("root/", b"", "directory"),
        (decomposed, b"one", "file"),
        (composed, b"two", "file"),
    ]
    if suffix == ".tar.gz":
        _tar(archive, members)
    else:
        _zip(
            archive,
            [(name, contents, False) for name, contents, _ in members],
        )
    with pytest.raises(ReleaseFailure, match="release.archive.invalid"):
        validate_archive(
            archive,
            expected_root="root",
            expected_files={"café.txt", "cafe\u0301.txt"},
        )


@pytest.mark.parametrize("suffix", [".tar.gz", ".zip"])
def test_archive_compares_single_member_and_expected_file_as_nfc(
    tmp_path: Path,
    suffix: str,
) -> None:
    archive = tmp_path / f"nfc{suffix}"
    members = [
        ("root/", b"", "directory"),
        ("root/cafe\u0301.txt", b"ok", "file"),
    ]
    if suffix == ".tar.gz":
        _tar(archive, members)
    else:
        _zip(
            archive,
            [(name, contents, False) for name, contents, _ in members],
        )

    validate_archive(archive, expected_root="root", expected_files={"café.txt"})


@pytest.mark.parametrize("suffix", [".tar.gz", ".zip"])
def test_archive_rejects_unexpected_empty_directories(
    tmp_path: Path,
    suffix: str,
) -> None:
    archive = tmp_path / f"directory{suffix}"
    members = [
        ("root/", b"", "directory"),
        ("root/empty/", b"", "directory"),
        ("root/README.txt", b"ok", "file"),
    ]
    if suffix == ".tar.gz":
        _tar(archive, members)
    else:
        _zip(
            archive,
            [(name, contents, False) for name, contents, _ in members],
        )

    with pytest.raises(ReleaseFailure, match="release.archive.invalid"):
        validate_archive(archive, expected_root="root", expected_files={"README.txt"})


@pytest.mark.parametrize("suffix", [".tar.gz", ".zip"])
@pytest.mark.parametrize("directory", ["root//", "root/subdir//"])
def test_archive_rejects_directory_members_with_extra_trailing_slashes(
    tmp_path: Path,
    suffix: str,
    directory: str,
) -> None:
    archive = tmp_path / f"trailing-slash{suffix}"
    members = [
        ("root/", b"", "directory"),
        (directory, b"", "directory"),
        ("root/subdir/README.txt", b"ok", "file"),
    ]
    if suffix == ".tar.gz":
        _raw_tar_members(
            archive,
            [
                ("root/", b"", True),
                (directory, b"", True),
                ("root/subdir/README.txt", b"ok", False),
            ],
        )
    else:
        _zip(
            archive,
            [(name, contents, False) for name, contents, _ in members],
        )

    with pytest.raises(ReleaseFailure) as failure:
        validate_archive(
            archive,
            expected_root="root",
            expected_files={"subdir/README.txt"},
        )

    assert failure.value.name == "release.archive.invalid"


# End of archive safety cases.
