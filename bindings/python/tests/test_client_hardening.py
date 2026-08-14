from __future__ import annotations

import asyncio
from array import array
from dataclasses import FrozenInstanceError
from pathlib import Path
import struct
import subprocess
import sys
import threading
from types import ModuleType

import pytest

from seeed_hal import (
    ControlLines,
    ErrorCategory,
    HalClient,
    HalError,
    IdentityQuality,
    ResourceSelector,
    SerialConfig,
    SerialSession,
    TransportKind,
)
from seeed_hal.proto import hal_pb2
from seeed_hal.transport_unix import HARD_FRAME_BYTES, UnixFramedTransport


TOKEN = bytes([0x6B] * 32)
MAX_U32 = (1 << 32) - 1
MAX_U64 = (1 << 64) - 1
REPO_ROOT = Path(__file__).resolve().parents[3]


class ScriptedTransport:
    def __init__(self) -> None:
        self.sent: asyncio.Queue[bytes] = asyncio.Queue()
        self.inbound: asyncio.Queue[bytes | BaseException] = asyncio.Queue()
        self.closed = False
        self.frame_limit = HARD_FRAME_BYTES

    async def send(self, payload: bytes | bytearray | memoryview) -> None:
        await self.sent.put(bytes(payload))

    async def receive(self) -> bytes:
        value = await self.inbound.get()
        if isinstance(value, BaseException):
            raise value
        return value

    async def close(self) -> None:
        self.closed = True

    def set_frame_limit(self, frame_limit: int) -> None:
        self.frame_limit = frame_limit


def direct_client(
    transport: ScriptedTransport, *, pending_capacity: int = 32
) -> HalClient:
    return HalClient(
        transport,
        frame_limit=HARD_FRAME_BYTES,
        read_limit=64 * 1024,
        write_limit=64 * 1024,
        pending_capacity=pending_capacity,
        writer_capacity=32,
        event_capacity=64,
    )


async def next_request(transport: ScriptedTransport) -> hal_pb2.Envelope:
    return hal_pb2.Envelope.FromString(await asyncio.wait_for(transport.sent.get(), 1))


def enumerate_response(request_id: int, label: str) -> bytes:
    return hal_pb2.Envelope(
        request_id=request_id,
        enumerate_serial_response=hal_pb2.EnumerateSerialResponse(
            resources=[
                hal_pb2.ResourceDescriptor(
                    resource_id=f"serial:{label}",
                    endpoint=f"virtual://serial:{label}",
                    identity_quality=hal_pb2.IDENTITY_QUALITY_STRONG,
                    transport=hal_pb2.TRANSPORT_KIND_SERIAL,
                )
            ]
        ),
    ).SerializeToString()


async def assert_next_request_healthy(
    client: HalClient, transport: ScriptedTransport, label: str
) -> None:
    call = asyncio.create_task(client.enumerate_serial())
    request = await next_request(transport)
    transport.inbound.put_nowait(enumerate_response(request.request_id, label))
    resources = await asyncio.wait_for(call, 1)
    assert resources[0].resource_id == f"serial:{label}"


@pytest.mark.asyncio
async def test_response_before_cancellation_keeps_connection_healthy() -> None:
    transport = ScriptedTransport()
    client = direct_client(transport)
    call = asyncio.create_task(client.enumerate_serial())
    request = await next_request(transport)
    transport.inbound.put_nowait(enumerate_response(request.request_id, "first"))
    assert (await asyncio.wait_for(call, 1))[0].resource_id == "serial:first"
    call.cancel()
    await assert_next_request_healthy(client, transport, "after-response")
    await client.close()


@pytest.mark.asyncio
async def test_cancellation_before_response_discards_response_and_keeps_connection_healthy() -> None:
    transport = ScriptedTransport()
    client = direct_client(transport)
    call = asyncio.create_task(client.enumerate_serial())
    request = await next_request(transport)
    call.cancel()
    with pytest.raises(asyncio.CancelledError):
        await call
    transport.inbound.put_nowait(enumerate_response(request.request_id, "cancelled"))
    await asyncio.sleep(0)
    await assert_next_request_healthy(client, transport, "after-cancel")
    await client.close()


@pytest.mark.asyncio
async def test_simultaneous_response_and_cancellation_discards_without_terminating() -> None:
    transport = ScriptedTransport()
    client = direct_client(transport)
    call = asyncio.create_task(client.enumerate_serial())
    request = await next_request(transport)

    # Queue the reader first, then cancel the waiter before yielding. Directly
    # awaiting the Future cancels it while it is still present in _pending.
    transport.inbound.put_nowait(enumerate_response(request.request_id, "boundary"))
    call.cancel()
    with pytest.raises(asyncio.CancelledError):
        await call
    await asyncio.sleep(0)

    await assert_next_request_healthy(client, transport, "after-boundary")
    await client.close()


@pytest.mark.asyncio
async def test_cancellation_response_boundary_stress_keeps_unrelated_requests_healthy() -> None:
    transport = ScriptedTransport()
    client = direct_client(transport, pending_capacity=4)
    for index in range(50):
        cancelled = asyncio.create_task(client.enumerate_serial())
        request = await next_request(transport)
        transport.inbound.put_nowait(
            enumerate_response(request.request_id, f"cancelled-{index}")
        )
        cancelled.cancel()
        with pytest.raises(asyncio.CancelledError):
            await cancelled
        await asyncio.sleep(0)
        await assert_next_request_healthy(client, transport, f"healthy-{index}")
    await client.close()


def install_win32_modules(monkeypatch, win32file: ModuleType, win32pipe: ModuleType) -> None:
    pywintypes = ModuleType("pywintypes")
    win32file.GENERIC_READ = 1
    win32file.GENERIC_WRITE = 2
    win32file.OPEN_EXISTING = 3
    win32pipe.PIPE_READMODE_BYTE = 0
    monkeypatch.setitem(sys.modules, "win32file", win32file)
    monkeypatch.setitem(sys.modules, "win32pipe", win32pipe)
    monkeypatch.setitem(sys.modules, "pywintypes", pywintypes)


@pytest.mark.asyncio
async def test_windows_setup_failure_closes_created_handle_once_off_loop(monkeypatch) -> None:
    from seeed_hal.transport_windows import WindowsFramedTransport

    main_thread = threading.get_ident()
    handle = object()
    calls: list[tuple[str, int]] = []
    win32file = ModuleType("win32file")
    win32pipe = ModuleType("win32pipe")

    def create_file(*_args):
        calls.append(("create", threading.get_ident()))
        return handle

    def set_state(*_args):
        calls.append(("state", threading.get_ident()))
        raise OSError("state setup failed")

    def close_handle(value):
        assert value is handle
        calls.append(("close", threading.get_ident()))

    win32file.CreateFile = create_file
    win32file.CloseHandle = close_handle
    win32pipe.SetNamedPipeHandleState = set_state
    install_win32_modules(monkeypatch, win32file, win32pipe)

    with pytest.raises(HalError) as caught:
        await WindowsFramedTransport.connect(r"\\.\pipe\seeed-hal-fail")
    assert caught.value.name == "runtime.broker.disconnected"
    assert [name for name, _thread in calls] == ["create", "state", "close"]
    assert all(thread != main_thread for _name, thread in calls)


@pytest.mark.asyncio
async def test_windows_cancelled_connect_closes_late_handle_once_off_loop(monkeypatch) -> None:
    from seeed_hal.transport_windows import WindowsFramedTransport

    main_thread = threading.get_ident()
    handle = object()
    state_started = threading.Event()
    state_release = threading.Event()
    closed = threading.Event()
    calls: list[tuple[str, int]] = []
    win32file = ModuleType("win32file")
    win32pipe = ModuleType("win32pipe")

    def create_file(*_args):
        calls.append(("create", threading.get_ident()))
        return handle

    def set_state(*_args):
        calls.append(("state", threading.get_ident()))
        state_started.set()
        assert state_release.wait(1)

    def close_handle(value):
        assert value is handle
        calls.append(("close", threading.get_ident()))
        closed.set()

    win32file.CreateFile = create_file
    win32file.CloseHandle = close_handle
    win32pipe.SetNamedPipeHandleState = set_state
    install_win32_modules(monkeypatch, win32file, win32pipe)

    connecting = asyncio.create_task(
        WindowsFramedTransport.connect(r"\\.\pipe\seeed-hal-cancel")
    )
    assert await asyncio.to_thread(state_started.wait, 1)
    connecting.cancel()
    state_release.set()
    with pytest.raises(asyncio.CancelledError):
        await connecting
    assert await asyncio.to_thread(closed.wait, 1)
    assert [name for name, _thread in calls].count("close") == 1
    assert all(thread != main_thread for _name, thread in calls)


class InvalidPath:
    def __fspath__(self):
        return 42


@pytest.mark.asyncio
@pytest.mark.parametrize("endpoint", [None, b"bytes", "", "bad\x00path", InvalidPath()])
async def test_invalid_endpoints_are_structured_arguments(endpoint) -> None:
    with pytest.raises(HalError) as caught:
        await HalClient.connect(endpoint, TOKEN)
    assert caught.value.name == "runtime.argument.invalid"


@pytest.mark.asyncio
@pytest.mark.parametrize("field", ["pending_capacity", "writer_capacity", "event_capacity"])
@pytest.mark.parametrize("value", [True, "1", 0, -1, MAX_U32 + 1])
async def test_invalid_capacities_are_rejected_before_transport(field: str, value) -> None:
    with pytest.raises(HalError) as caught:
        await HalClient.connect("not-used", TOKEN, **{field: value})
    assert caught.value.name == "runtime.argument.invalid"


@pytest.mark.asyncio
@pytest.mark.parametrize("field", ["max_frame_bytes", "max_read_bytes", "max_write_bytes"])
@pytest.mark.parametrize("value", [True, "1", 0, -1, MAX_U64 + 1])
async def test_invalid_limits_are_rejected_before_transport(field: str, value) -> None:
    with pytest.raises(HalError) as caught:
        await HalClient.connect("not-used", TOKEN, **{field: value})
    assert caught.value.name == "runtime.argument.invalid"


@pytest.mark.asyncio
@pytest.mark.parametrize(
    "selector",
    [
        ResourceSelector("", IdentityQuality.STRONG, TransportKind.SERIAL),
        ResourceSelector("s" * 256, IdentityQuality.STRONG, TransportKind.SERIAL),
        ResourceSelector("serial:é", IdentityQuality.STRONG, TransportKind.SERIAL),
        ResourceSelector(42, IdentityQuality.STRONG, TransportKind.SERIAL),
        ResourceSelector("serial:ok", "strong", TransportKind.SERIAL),
        ResourceSelector("serial:ok", IdentityQuality.STRONG, "serial"),
    ],
)
async def test_invalid_resource_selectors_are_structured_arguments(selector) -> None:
    transport = ScriptedTransport()
    client = direct_client(transport)
    with pytest.raises(HalError) as caught:
        await client.open_serial(selector, SerialConfig())
    assert caught.value.name == "runtime.argument.invalid"
    assert transport.sent.empty()
    await client.close()


@pytest.mark.asyncio
@pytest.mark.parametrize(
    "config",
    [
        SerialConfig(baud_rate=True),
        SerialConfig(baud_rate="115200"),
        SerialConfig(baud_rate=0),
        SerialConfig(baud_rate=MAX_U32 + 1),
        SerialConfig(data_bits="eight"),
        SerialConfig(parity="none"),
        SerialConfig(stop_bits="one"),
        SerialConfig(flow_control="none"),
        SerialConfig(read_timeout_ms=True),
        SerialConfig(read_timeout_ms="100"),
        SerialConfig(read_timeout_ms=-1),
        SerialConfig(read_timeout_ms=MAX_U64 + 1),
    ],
)
async def test_invalid_serial_configs_are_structured_arguments(config) -> None:
    transport = ScriptedTransport()
    client = direct_client(transport)
    selector = ResourceSelector(
        "serial:valid", IdentityQuality.STRONG, TransportKind.SERIAL
    )
    with pytest.raises(HalError) as caught:
        await client.open_serial(selector, config)
    assert caught.value.name == "runtime.argument.invalid"
    assert transport.sent.empty()
    await client.close()


@pytest.mark.asyncio
async def test_invalid_control_lines_and_read_sizes_are_structured_arguments() -> None:
    transport = ScriptedTransport()
    client = direct_client(transport)
    serial = SerialSession(
        client,
        "session:valid",
        "lease:valid",
        1,
        hal_pb2.LEASE_MODE_CONTROL,
    )
    for lines in (object(), ControlLines(1, False), ControlLines(False, "no")):
        with pytest.raises(HalError) as caught:
            await serial.set_control_lines(lines)
        assert caught.value.name == "runtime.argument.invalid"
    for size in (True, "1", 0, -1, MAX_U32 + 1):
        with pytest.raises(HalError) as caught:
            await serial.read(size)
        assert caught.value.name == "runtime.argument.invalid"
    assert transport.sent.empty()
    await client.close()


@pytest.mark.asyncio
@pytest.mark.parametrize(
    ("session_id", "lease_id"),
    [
        ("session:é", "lease:valid"),
        ("s" * 256, "lease:valid"),
        ("session:valid", "lease:é"),
        ("session:valid", "l" * 256),
    ],
)
async def test_invalid_broker_session_credentials_terminate_and_fan_out(
    session_id: str, lease_id: str
) -> None:
    transport = ScriptedTransport()
    client = direct_client(transport)
    selector = ResourceSelector(
        "serial:valid", IdentityQuality.STRONG, TransportKind.SERIAL
    )
    opening = asyncio.create_task(client.open_serial(selector, SerialConfig()))
    enumerating = asyncio.create_task(client.enumerate_serial())
    requests = [await next_request(transport), await next_request(transport)]
    open_request = next(
        request
        for request in requests
        if request.WhichOneof("payload") == "open_serial_request"
    )
    transport.inbound.put_nowait(
        hal_pb2.Envelope(
            request_id=open_request.request_id,
            open_serial_response=hal_pb2.OpenSerialResponse(
                session_id=session_id,
                lease=hal_pb2.LeaseToken(
                    lease_id=lease_id,
                    generation=1,
                    mode=hal_pb2.LEASE_MODE_CONTROL,
                ),
            ),
        ).SerializeToString()
    )
    results = await asyncio.wait_for(
        asyncio.gather(opening, enumerating, return_exceptions=True), 1
    )
    assert all(isinstance(error, HalError) for error in results)
    assert {error.name for error in results} == {"runtime.protocol.invalid_message"}
    assert results[0] is not results[1]
    await client.close()


@pytest.mark.asyncio
async def test_terminal_fanout_and_repeated_calls_receive_fresh_immutable_errors() -> None:
    transport = ScriptedTransport()
    client = direct_client(transport)
    first = asyncio.create_task(client.enumerate_serial())
    second = asyncio.create_task(client.enumerate_serial())
    await next_request(transport)
    await next_request(transport)
    transport.inbound.put_nowait(OSError("connection lost"))
    first_error, second_error = await asyncio.gather(
        first, second, return_exceptions=True
    )
    assert isinstance(first_error, HalError)
    assert isinstance(second_error, HalError)
    assert first_error == second_error
    assert first_error is not second_error
    with pytest.raises(FrozenInstanceError):
        first_error.name = "changed"

    with pytest.raises(HalError) as repeated:
        await client.enumerate_serial()
    assert repeated.value == first_error
    assert repeated.value is not first_error
    await client.close()


@pytest.mark.asyncio
async def test_event_terminal_errors_are_fresh_per_subscriber_and_receive() -> None:
    transport = ScriptedTransport()
    client = direct_client(transport)
    first = client.subscribe()
    second = client.subscribe()
    first_wait = asyncio.create_task(first.receive())
    second_wait = asyncio.create_task(second.receive())
    await asyncio.sleep(0)
    await client.close()
    first_error, second_error = await asyncio.gather(
        first_wait, second_wait, return_exceptions=True
    )
    assert isinstance(first_error, HalError)
    assert isinstance(second_error, HalError)
    assert first_error == second_error
    assert first_error is not second_error
    with pytest.raises(HalError) as repeated:
        await first.receive()
    assert repeated.value == first_error
    assert repeated.value is not first_error


class RecordingWriter:
    def __init__(self) -> None:
        self.writes: list[bytes] = []

    def write(self, payload) -> None:
        self.writes.append(bytes(payload))

    async def drain(self) -> None:
        return None

    def close(self) -> None:
        return None

    async def wait_closed(self) -> None:
        return None


@pytest.mark.asyncio
async def test_unix_transport_uses_memoryview_nbytes_for_frame_prefix() -> None:
    writer = RecordingWriter()
    transport = UnixFramedTransport(asyncio.StreamReader(), writer, HARD_FRAME_BYTES)
    payload = memoryview(array("I", [1, 2]))
    await transport.send(payload)
    assert writer.writes[0] == struct.pack(">I", payload.nbytes)
    assert len(writer.writes[1]) == payload.nbytes


def test_generation_check_rejects_untracked_stale_outputs() -> None:
    checker = REPO_ROOT / "scripts" / "check-generated-protocol.sh"
    assert checker.is_file(), "repository generation drift checker is required"
    stale = REPO_ROOT / "bindings/python/seeed_hal/proto/stale_pb2.py"
    stale.write_text("# stale generated output\n", encoding="utf-8")
    try:
        result = subprocess.run(
            [checker], cwd=REPO_ROOT, text=True, capture_output=True, timeout=30
        )
    finally:
        stale.unlink(missing_ok=True)
    assert result.returncode != 0
    assert "stale_pb2.py" in result.stdout + result.stderr
