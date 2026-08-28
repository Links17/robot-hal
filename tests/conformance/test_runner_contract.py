from __future__ import annotations

import asyncio
import argparse
import importlib.util
from pathlib import Path
import sys
from types import ModuleType

import pytest

from robot_hal.proto import hal_pb2


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
        == r"\\.\pipe\robot-hal-conformance-fixed"
    )


def test_broker_command_uses_only_production_startup_arguments(tmp_path: Path) -> None:
    runner = load_runner()
    broker = tmp_path / "robot-hal-broker"
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


def test_runner_defines_complete_camera_minor_three_capabilities() -> None:
    runner = load_runner()

    assert runner.CAMERA_CAPTURE_CAPABILITY == "camera.capture/v1"
    assert runner.CAMERA_FRAMES_SHM_CAPABILITY == "camera.frames.shm/v1"
    assert runner.CAMERA_CONTROLS_CAPABILITY == "camera.controls/v1"


def test_protocol_minor_cli_is_exact_and_capabilities_are_repeatable(
    monkeypatch,
) -> None:
    runner = load_runner()
    monkeypatch.setattr(
        sys,
        "argv",
        [
            str(RUNNER),
            "--broker",
            "broker",
            "--protocol-minor",
            "1",
            "--require-capability",
            "can.classic/v1",
            "--require-capability",
            "can.fd/v1",
        ],
    )

    args = runner.parse_args()

    assert args.protocol_minor == 1
    assert args.require_capability == ["can.classic/v1", "can.fd/v1"]


def test_protocol_minor_cli_defaults_to_latest_profile(monkeypatch) -> None:
    runner = load_runner()
    monkeypatch.setattr(sys, "argv", [str(RUNNER), "--broker", "broker"])

    args = runner.parse_args()

    assert args.protocol_minor == 3
    assert args.require_capability == []


def test_explicit_required_capabilities_replace_profile_defaults() -> None:
    runner = load_runner()

    assert runner.required_capabilities_for_run(2, ()) == runner.capabilities_for_minor(2)
    assert runner.required_capabilities_for_run(
        2, ("usb.control/v1", "gpio.lines/v1")
    ) == ("usb.control/v1", "gpio.lines/v1")


class HandshakeTransport:
    def __init__(self, protocol_minor: int, capabilities: tuple[str, ...]) -> None:
        self.sent = []
        self.frame_limit = None
        self.response = hal_pb2.Envelope(
            request_id=1,
            handshake_response=hal_pb2.HandshakeResponse(
                protocol_major=1,
                protocol_minor=protocol_minor,
                capabilities=capabilities,
                max_frame_bytes=1024 * 1024,
                max_read_bytes=64 * 1024,
                max_write_bytes=64 * 1024,
                protocol_minor_minimum=0,
                protocol_minor_maximum=3,
            ),
        ).SerializeToString()

    async def send(self, payload: bytes) -> None:
        self.sent.append(hal_pb2.Envelope.FromString(payload))

    async def receive(self) -> bytes:
        return self.response

    async def close(self) -> None:
        return None

    def set_frame_limit(self, frame_limit: int) -> None:
        self.frame_limit = frame_limit


@pytest.mark.asyncio
async def test_handshake_offers_and_requires_the_exact_selected_profile() -> None:
    runner = load_runner()
    required = ("can.classic/v1",)
    transport = HandshakeTransport(1, required)
    client = runner.RawClient(transport, timeout=0.2)

    negotiated = await client.handshake(
        bytes(32), minor=1, required_capabilities=required
    )

    request = transport.sent[0].handshake_request
    assert request.protocol_minor == 1
    assert request.protocol_minor_minimum == 1
    assert request.protocol_minor_maximum == 1
    assert list(request.required_capabilities) == ["can.classic/v1"]
    assert negotiated == frozenset(required)


@pytest.mark.asyncio
async def test_handshake_rejects_a_selection_other_than_the_exact_offer() -> None:
    runner = load_runner()
    transport = HandshakeTransport(2, runner.capabilities_for_minor(2))
    client = runner.RawClient(transport, timeout=0.2)

    with pytest.raises(AssertionError, match="exact offered protocol minor"):
        await client.handshake(
            bytes(32),
            minor=1,
            required_capabilities=runner.capabilities_for_minor(1),
        )


class RecordingClient:
    def __init__(self, responses) -> None:
        self.responses = iter(responses)
        self.requests = []

    async def request(self, payload_name: str, payload):
        self.requests.append(payload_name)
        return next(self.responses)


@pytest.mark.asyncio
async def test_later_minor_rejection_is_followed_by_same_connection_serial_probe() -> None:
    runner = load_runner()
    client = RecordingClient(
        (
            hal_pb2.Envelope(
                request_id=1,
                error=hal_pb2.Error(
                    name="runtime.protocol.capability_unsupported",
                    operation="runtime.protocol.dispatch",
                ),
            ),
            hal_pb2.Envelope(
                request_id=2,
                enumerate_can_response=hal_pb2.EnumerateCanResponse(),
            ),
        )
    )

    await runner._probe_later_operation(
        client, 0, frozenset((runner.CAN_CLASSIC_CAPABILITY,))
    )

    assert client.requests == [
        "enumerate_can_request",
        "enumerate_can_request",
    ]


@pytest.mark.asyncio
async def test_explicit_profile_executes_only_selected_capability_operations(
    monkeypatch,
) -> None:
    runner = load_runner()
    called = []

    async def record(name):
        called.append(name)

    monkeypatch.setattr(
        runner,
        "_exercise_serial",
        lambda _client, **_kwargs: record("serial"),
    )
    monkeypatch.setattr(
        runner,
        "_exercise_can",
        lambda _client, _capabilities, **_kwargs: record("can"),
    )
    monkeypatch.setattr(
        runner,
        "_exercise_usb",
        lambda _client, _capabilities, **_kwargs: record("usb"),
    )
    monkeypatch.setattr(
        runner,
        "_exercise_gpio",
        lambda _client, _capabilities, **_kwargs: record("gpio"),
    )
    monkeypatch.setattr(
        runner,
        "_exercise_camera",
        lambda _client, _capabilities, **_kwargs: record("camera"),
    )

    await runner._exercise_profile(
        object(),
        3,
        frozenset(
            (
                runner.USB_CONTROL_CAPABILITY,
            )
        ),
    )

    assert called == ["usb"]


@pytest.mark.asyncio
async def test_can_profile_exercises_each_selected_mode_and_optional_check(
    monkeypatch,
) -> None:
    runner = load_runner()
    modes = []

    async def record(_client, mode, capabilities, *, leave_open=False):
        modes.append((mode, capabilities, leave_open))
        return None

    monkeypatch.setattr(runner, "_exercise_can_mode", record)

    await runner._exercise_can(
        object(),
        frozenset(
            (
                runner.CAN_CLASSIC_CAPABILITY,
                runner.CAN_FD_CAPABILITY,
                runner.CAN_CONFIGURE_CAPABILITY,
                runner.CAN_ERROR_FRAMES_CAPABILITY,
                runner.CAN_RX_TIMESTAMP_CAPABILITY,
            )
        ),
    )

    assert [mode for mode, _capabilities, _leave_open in modes] == [
        "classic",
        "fd",
        "configure",
        "error-frames",
        "rx-timestamp",
    ]


def test_can_fd_open_uses_fd_attach_without_configure() -> None:
    runner = load_runner()
    descriptor = hal_pb2.ResourceDescriptor(
        resource_id="virtual-can",
        identity_quality=hal_pb2.IDENTITY_QUALITY_STRONG,
        transport=hal_pb2.TRANSPORT_KIND_CAN,
    )

    request = runner._can_open_request(descriptor, "fd")

    assert request.mode == hal_pb2.LEASE_MODE_CONTROL
    assert request.config.WhichOneof("config") == "attach"
    assert request.config.attach.mode == hal_pb2.CAN_MODE_FD
    assert not request.config.HasField("configure")


@pytest.mark.parametrize("mode", ["classic", "error-frames", "rx-timestamp"])
def test_classic_can_checks_use_explicit_classic_attach(mode: str) -> None:
    runner = load_runner()
    descriptor = hal_pb2.ResourceDescriptor(
        resource_id="virtual-can-classic",
        identity_quality=hal_pb2.IDENTITY_QUALITY_STRONG,
        transport=hal_pb2.TRANSPORT_KIND_CAN,
    )

    request = runner._can_open_request(descriptor, mode)

    assert request.mode == hal_pb2.LEASE_MODE_CONTROL
    assert request.config.WhichOneof("config") == "attach"
    assert request.config.attach.mode == hal_pb2.CAN_MODE_CLASSIC
    assert not request.config.HasField("configure")


@pytest.mark.asyncio
@pytest.mark.parametrize("transport", ["can", "usb", "gpio", "camera"])
async def test_non_serial_profile_returns_cleanup_handle_for_selected_transport(
    monkeypatch,
    transport: str,
) -> None:
    runner = load_runner()
    handle = object()
    capabilities = {
        "can": frozenset((runner.CAN_CLASSIC_CAPABILITY,)),
        "usb": frozenset((runner.USB_CONTROL_CAPABILITY,)),
        "gpio": frozenset((runner.GPIO_LINES_CAPABILITY,)),
        "camera": frozenset((runner.CAMERA_CAPTURE_CAPABILITY,)),
    }[transport]

    async def exercise(*_args, **kwargs):
        assert kwargs["leave_open"]
        return handle

    monkeypatch.setattr(runner, f"_exercise_{transport}", exercise)

    assert await runner._exercise_profile(object(), 3, capabilities) is handle


def test_camera_runner_exercises_control_read_write_and_auto() -> None:
    runner = load_runner()
    source = RUNNER.read_text(encoding="utf-8")

    for payload in (
        "camera_get_control_request",
        "camera_set_control_request",
        "camera_set_auto_request",
    ):
        assert payload in source


def test_main_reports_stable_validation_error_for_unavailable_capability(
    monkeypatch, capsys
) -> None:
    runner = load_runner()
    monkeypatch.setattr(
        runner,
        "parse_args",
        lambda: argparse.Namespace(
            broker=Path("unused"),
            timeout=1.0,
            protocol_minor=0,
            require_capability=[runner.USB_CONTROL_CAPABILITY],
        ),
    )

    assert runner.main() == 1
    assert capsys.readouterr().err == (
        "broker conformance failed: capability usb.control/v1 "
        "is unavailable at protocol minor 0\n"
    )


def test_readiness_parser_decodes_windows_endpoint_escaping() -> None:
    runner = load_runner()

    assert (
        runner.parse_readiness_endpoint(
            br'{"status":"ready","endpoint":"\\\\.\\pipe\\robot-hal-fixed"}'
        )
        == r"\\.\pipe\robot-hal-fixed"
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
