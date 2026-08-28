from __future__ import annotations

import asyncio
import json
import os
from pathlib import Path
import subprocess
import tempfile
from types import SimpleNamespace

import pytest
import pytest_asyncio


PYTHON_ROOT = Path(__file__).resolve().parents[1]
REPO_ROOT = PYTHON_ROOT.parents[1]
TOKEN_SIZE = 32


@pytest.fixture(scope="session")
def virtual_broker_executable() -> Path:
    subprocess.run(
        [
            "cargo",
            "build",
            "--package",
            "robot-hal-broker",
            "--example",
            "virtual_broker",
        ],
        cwd=REPO_ROOT,
        check=True,
        timeout=120,
    )
    suffix = ".exe" if os.name == "nt" else ""
    return REPO_ROOT / "target" / "debug" / "examples" / f"virtual_broker{suffix}"


@pytest_asyncio.fixture
async def broker(virtual_broker_executable: Path):
    temp_parent = "/tmp" if os.name != "nt" else None
    with tempfile.TemporaryDirectory(prefix="shp-", dir=temp_parent) as raw_directory:
        directory = Path(raw_directory)
        if os.name != "nt":
            directory.chmod(0o700)
        token = bytearray(os.urandom(TOKEN_SIZE))
        token_path = directory / "token"
        token_path.write_bytes(token)
        if os.name != "nt":
            token_path.chmod(0o600)
        environment = os.environ.copy()
        environment["ROBOT_HAL_TEST_DIRECTORY"] = str(directory)
        environment["ROBOT_HAL_TEST_TOKEN_FILE"] = str(token_path)
        process = await asyncio.create_subprocess_exec(
            virtual_broker_executable,
            stdout=asyncio.subprocess.PIPE,
            stderr=asyncio.subprocess.PIPE,
            env=environment,
        )
        assert process.stdout is not None
        try:
            line = await asyncio.wait_for(process.stdout.readline(), timeout=10)
            if not line:
                assert process.stderr is not None
                stderr = (await process.stderr.read()).decode(errors="replace")
                raise AssertionError(f"virtual broker did not become ready: {stderr}")
            ready = json.loads(line)
            yield SimpleNamespace(
                endpoint=ready["endpoint"], token=token, process=process
            )
        finally:
            if process.returncode is None:
                try:
                    await asyncio.wait_for(process.wait(), timeout=1)
                except TimeoutError:
                    process.terminate()
                    try:
                        await asyncio.wait_for(process.wait(), timeout=3)
                    except TimeoutError:
                        process.kill()
                        await asyncio.wait_for(process.wait(), timeout=3)
            token[:] = b"\x00" * len(token)
            token_path.unlink(missing_ok=True)
