from __future__ import annotations

import asyncio
import importlib.util
from pathlib import Path
import sys
from types import ModuleType

import pytest

from seeed_hal.proto import hal_pb2


RUNNER = Path(__file__).with_name("run-broker-conformance.py")


def load_runner():
    spec = importlib.util.spec_from_file_location("broker_conformance", RUNNER)
    assert spec is not None and spec.loader is not None
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


def test_endpoint_name_is_local_and_platform_specific(tmp_path: Path) -> None:
    runner = load_runner()

    assert runner.endpoint_for_platform(tmp_path, "fixed", "posix") == str(
        tmp_path / "broker.sock"
    )
    assert (
        runner.endpoint_for_platform(tmp_path, "fixed", "nt")
        == r"\\.\pipe\seeed-hal-conformance-fixed"
    )


def test_broker_command_uses_only_production_startup_arguments(tmp_path: Path) -> None:
    runner = load_runner()
    broker = tmp_path / "seeed-hal-broker"
    token = tmp_path / "token"

    command = runner.broker_command(broker, "endpoint", token)

    assert command == [
        str(broker),
        "--endpoint",
        "endpoint",
        "--auth-token-file",
        str(token),
    ]
    assert "virtual" not in " ".join(command).lower()


def test_readiness_parser_decodes_windows_endpoint_escaping() -> None:
    runner = load_runner()

    assert (
        runner.parse_readiness_endpoint(
            br'{"status":"ready","endpoint":"\\\\.\\pipe\\seeed-hal-fixed"}'
        )
        == r"\\.\pipe\seeed-hal-fixed"
    )


class EventFloodTransport:
    def __init__(self) -> None:
        self.event = hal_pb2.Envelope(
            request_id=0,
            runtime_event=hal_pb2.RuntimeEvent(
                sequence=1,
                kind=hal_pb2.RUNTIME_EVENT_KIND_SESSION_OPENED,
                name="runtime.session.opened",
            ),
        ).SerializeToString()

    async def send(self, _payload: bytes) -> None:
        return None

    async def receive(self) -> bytes:
        await asyncio.sleep(0.02)
        return self.event

    async def close(self) -> None:
        return None

    def set_frame_limit(self, _frame_limit: int) -> None:
        return None


@pytest.mark.asyncio
async def test_request_event_flood_cannot_reset_the_original_deadline() -> None:
    runner = load_runner()
    client = runner.RawClient(EventFloodTransport(), timeout=0.05)
    started = asyncio.get_running_loop().time()

    with pytest.raises(asyncio.TimeoutError):
        await asyncio.wait_for(
            client.request(
                "enumerate_serial_request", hal_pb2.EnumerateSerialRequest()
            ),
            0.25,
        )

    assert asyncio.get_running_loop().time() - started < 0.15


@pytest.mark.asyncio
async def test_diagnostic_capture_retains_only_the_bounded_tail() -> None:
    runner = load_runner()
    reader = asyncio.StreamReader()
    reader.feed_data(b"first-middle-last")
    reader.feed_eof()

    assert await runner.capture_stream_tail(reader, 6) == b"e-last"


def test_windows_private_dacl_allows_only_broker_trustees(monkeypatch, tmp_path: Path) -> None:
    runner = load_runner()
    captured = {}

    class FakeAcl:
        def __init__(self) -> None:
            self.entries = []

        def AddAccessAllowedAce(self, revision, access, sid) -> None:
            self.entries.append((revision, access, sid))

    win32api = ModuleType("win32api")
    win32api.GetCurrentProcess = lambda: "process"
    win32con = ModuleType("win32con")
    win32con.TOKEN_QUERY = 0x8
    ntsecuritycon = ModuleType("ntsecuritycon")
    ntsecuritycon.FILE_ALL_ACCESS = 0x1F01FF
    win32security = ModuleType("win32security")
    win32security.ACL_REVISION = 2
    win32security.DACL_SECURITY_INFORMATION = 0x4
    win32security.OWNER_SECURITY_INFORMATION = 0x1
    win32security.PROTECTED_DACL_SECURITY_INFORMATION = 0x80000000
    win32security.SE_FILE_OBJECT = 1
    win32security.TokenUser = 1
    win32security.WinBuiltinAdministratorsSid = 26
    win32security.WinLocalSystemSid = 22
    win32security.ACL = FakeAcl
    win32security.OpenProcessToken = lambda process, access: (process, access)
    win32security.GetTokenInformation = lambda token, kind: ("current-user", 0)
    win32security.CreateWellKnownSid = lambda kind, _domain: {
        22: "system",
        26: "administrators",
    }[kind]

    def set_security(path, object_type, information, owner, group, dacl, sacl):
        captured.update(
            path=path,
            object_type=object_type,
            information=information,
            owner=owner,
            group=group,
            dacl=dacl,
            sacl=sacl,
        )

    win32security.SetNamedSecurityInfo = set_security
    monkeypatch.setitem(sys.modules, "win32api", win32api)
    monkeypatch.setitem(sys.modules, "win32con", win32con)
    monkeypatch.setitem(sys.modules, "ntsecuritycon", ntsecuritycon)
    monkeypatch.setitem(sys.modules, "win32security", win32security)

    runner.apply_windows_private_dacl(tmp_path)

    assert captured["path"] == str(tmp_path)
    assert captured["owner"] == "current-user"
    assert captured["information"] == 0x80000005
    assert captured["dacl"].entries == [
        (2, 0x1F01FF, "current-user"),
        (2, 0x1F01FF, "system"),
        (2, 0x1F01FF, "administrators"),
    ]


@pytest.mark.asyncio
async def test_windows_token_setup_protects_parent_and_token(
    monkeypatch, tmp_path: Path
) -> None:
    runner = load_runner()
    protected = []
    monkeypatch.setattr(
        runner, "apply_windows_private_dacl", lambda path: protected.append(path)
    )
    token_path = tmp_path / "startup-token"
    token = bytes(range(32))

    await runner.prepare_private_token(
        tmp_path, token_path, token, os_name="nt", timeout=0.2
    )

    assert protected == [tmp_path, token_path]
    assert token_path.read_bytes() == token


@pytest.mark.asyncio
async def test_initial_transport_connection_is_bounded(monkeypatch) -> None:
    runner = load_runner()

    async def never_connect(_endpoint):
        await asyncio.Future()

    monkeypatch.setattr(runner, "connect_transport", never_connect)
    started = asyncio.get_running_loop().time()

    with pytest.raises(asyncio.TimeoutError):
        await asyncio.wait_for(
            runner.exercise_contract("unused", bytes(32), timeout=0.04), 0.25
        )

    assert asyncio.get_running_loop().time() - started < 0.15


@pytest.mark.asyncio
async def test_final_cleanup_kills_child_and_finishes_diagnostic_capture() -> None:
    runner = load_runner()
    process = await asyncio.create_subprocess_exec(
        sys.executable,
        "-c",
        "import time; time.sleep(60)",
        stderr=asyncio.subprocess.PIPE,
    )
    assert process.stderr is not None
    diagnostics = asyncio.create_task(
        runner.capture_stream_tail(process.stderr, 1024)
    )
    started = asyncio.get_running_loop().time()

    try:
        tail = await runner.cleanup_process(process, diagnostics, timeout=0.5)
    finally:
        if process.returncode is None:
            process.kill()
            await process.wait()

    assert asyncio.get_running_loop().time() - started < 1.0
    assert process.returncode is not None
    assert tail == b""
