from __future__ import annotations

import asyncio
from array import array
from dataclasses import FrozenInstanceError
from pathlib import Path
import shutil
import struct
import subprocess
import sys
import threading
import time
from types import ModuleType

import pytest

from seeed_hal import (
    CanMode,
    CanSession,
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
from seeed_hal.can import _CanSessionProfile
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
        protocol_minor=0,
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
    pywintypes.error = FaithfulPyWinTypesError
    win32file.error = FaithfulPyWinTypesError
    win32file.GENERIC_READ = 1
    win32file.GENERIC_WRITE = 2
    win32file.OPEN_EXISTING = 3
    win32file.FILE_FLAG_OVERLAPPED = 4
    win32pipe.PIPE_READMODE_BYTE = 0
    win32pipe.PIPE_NOWAIT = 1
    monkeypatch.setitem(sys.modules, "win32file", win32file)
    monkeypatch.setitem(sys.modules, "win32pipe", win32pipe)
    monkeypatch.setitem(sys.modules, "pywintypes", pywintypes)


class FaithfulPyWinTypesError(Exception):
    """Match pywin32 311's direct-Exception error type and public fields."""

    def __init__(self, *args) -> None:
        self.winerror = args[0] if args else None
        self.funcname = args[1] if len(args) > 1 else None
        self.strerror = args[2] if len(args) > 2 else None
        super().__init__(*args)


@pytest.mark.asyncio
async def test_windows_native_no_data_read_retries_then_progresses(monkeypatch) -> None:
    from seeed_hal.transport_windows import WindowsFramedTransport

    handle = object()
    wire = bytearray(struct.pack(">I", 2) + b"ok")
    read_calls = 0
    close_calls = 0
    win32file = ModuleType("win32file")
    win32pipe = ModuleType("win32pipe")

    win32file.CreateFile = lambda *_args: handle
    win32pipe.SetNamedPipeHandleState = lambda *_args: None

    def read_file(value, count):
        nonlocal read_calls
        assert value is handle
        read_calls += 1
        if read_calls == 1:
            raise FaithfulPyWinTypesError(232, "ReadFile", "No data")
        chunk = bytes(wire[:count])
        del wire[:count]
        return 0, chunk

    def close_handle(value):
        nonlocal close_calls
        assert value is handle
        close_calls += 1

    win32file.ReadFile = read_file
    win32file.WriteFile = lambda _handle, data: (0, len(data))
    win32file.CloseHandle = close_handle
    install_win32_modules(monkeypatch, win32file, win32pipe)

    transport = await WindowsFramedTransport.connect(r"\\.\pipe\seeed-hal-no-data")
    assert await transport.receive() == b"ok"
    await transport.close()

    assert read_calls == 3
    assert close_calls == 1


@pytest.mark.asyncio
async def test_windows_native_no_data_read_deadline_terminates_actor(
    monkeypatch,
) -> None:
    import seeed_hal.transport_windows as transport_windows

    handle = object()
    read_times: list[float] = []
    close_calls = 0
    win32file = ModuleType("win32file")
    win32pipe = ModuleType("win32pipe")

    win32file.CreateFile = lambda *_args: handle
    win32pipe.SetNamedPipeHandleState = lambda *_args: None

    def read_file(value, _count):
        assert value is handle
        read_times.append(time.monotonic())
        raise FaithfulPyWinTypesError(232, "ReadFile", "No data")

    def close_handle(value):
        nonlocal close_calls
        assert value is handle
        close_calls += 1

    win32file.ReadFile = read_file
    win32file.WriteFile = lambda _handle, data: (0, len(data))
    win32file.CloseHandle = close_handle
    install_win32_modules(monkeypatch, win32file, win32pipe)
    monkeypatch.setattr(transport_windows, "_POLL_SECONDS", 0.01)

    transport = await transport_windows.WindowsFramedTransport.connect(
        r"\\.\pipe\seeed-hal-no-data-deadline"
    )
    with pytest.raises(asyncio.TimeoutError):
        await asyncio.wait_for(transport.receive(), 0.025)
    await asyncio.gather(transport.close(), transport.close())

    assert 1 <= len(read_times) <= 4
    assert all(
        later - earlier >= 0.008
        for earlier, later in zip(read_times, read_times[1:])
    )
    assert close_calls == 1
    assert not any(
        thread.name.startswith("seeed-hal-pipe-io") and thread.is_alive()
        for thread in threading.enumerate()
    )


@pytest.mark.asyncio
async def test_windows_native_write_backpressure_error_retries_then_progresses(
    monkeypatch,
) -> None:
    from seeed_hal.transport_windows import WindowsFramedTransport

    handle = object()
    write_calls = 0
    close_calls = 0
    win32file = ModuleType("win32file")
    win32pipe = ModuleType("win32pipe")

    win32file.CreateFile = lambda *_args: handle
    win32pipe.SetNamedPipeHandleState = lambda *_args: None
    win32file.ReadFile = lambda _handle, count: (0, b"\0" * count)

    def write_file(value, data):
        nonlocal write_calls
        assert value is handle
        write_calls += 1
        if write_calls == 1:
            raise FaithfulPyWinTypesError(232, "WriteFile", "No data")
        return 0, len(data)

    def close_handle(value):
        nonlocal close_calls
        assert value is handle
        close_calls += 1

    win32file.WriteFile = write_file
    win32file.CloseHandle = close_handle
    install_win32_modules(monkeypatch, win32file, win32pipe)

    transport = await WindowsFramedTransport.connect(
        r"\\.\pipe\seeed-hal-write-backpressure"
    )
    await transport.send(b"ok")
    await transport.close()

    assert write_calls == 2
    assert close_calls == 1


@pytest.mark.asyncio
async def test_windows_zero_byte_write_waits_then_progresses(monkeypatch) -> None:
    import seeed_hal.transport_windows as transport_windows

    handle = object()
    write_times: list[float] = []
    close_calls = 0
    win32file = ModuleType("win32file")
    win32pipe = ModuleType("win32pipe")

    win32file.CreateFile = lambda *_args: handle
    win32pipe.SetNamedPipeHandleState = lambda *_args: None
    win32file.ReadFile = lambda _handle, count: (0, b"\0" * count)

    def write_file(value, data):
        assert value is handle
        write_times.append(time.monotonic())
        if len(write_times) == 1:
            return 0, 0
        return 0, len(data)

    def close_handle(value):
        nonlocal close_calls
        assert value is handle
        close_calls += 1

    win32file.WriteFile = write_file
    win32file.CloseHandle = close_handle
    install_win32_modules(monkeypatch, win32file, win32pipe)
    monkeypatch.setattr(transport_windows, "_POLL_SECONDS", 0.01)

    transport = await transport_windows.WindowsFramedTransport.connect(
        r"\\.\pipe\seeed-hal-zero-write"
    )
    await transport.send(b"eventual")
    await transport.close()

    assert len(write_times) == 2
    assert write_times[1] - write_times[0] >= 0.008
    assert close_calls == 1


@pytest.mark.asyncio
async def test_windows_zero_byte_write_cancellation_and_repeated_close_terminate_actor(
    monkeypatch,
) -> None:
    import seeed_hal.transport_windows as transport_windows

    handle = object()
    write_started = threading.Event()
    write_calls = 0
    close_calls = 0
    win32file = ModuleType("win32file")
    win32pipe = ModuleType("win32pipe")

    win32file.CreateFile = lambda *_args: handle
    win32pipe.SetNamedPipeHandleState = lambda *_args: None
    win32file.ReadFile = lambda _handle, count: (0, b"\0" * count)

    def write_file(value, _data):
        nonlocal write_calls
        assert value is handle
        write_calls += 1
        write_started.set()
        return 0, 0

    def close_handle(value):
        nonlocal close_calls
        assert value is handle
        close_calls += 1

    win32file.WriteFile = write_file
    win32file.CloseHandle = close_handle
    install_win32_modules(monkeypatch, win32file, win32pipe)
    monkeypatch.setattr(transport_windows, "_POLL_SECONDS", 0.01)

    transport = await transport_windows.WindowsFramedTransport.connect(
        r"\\.\pipe\seeed-hal-zero-write-cancel"
    )
    sending = asyncio.create_task(transport.send(b"blocked"))
    assert await asyncio.to_thread(write_started.wait, 1)
    await asyncio.sleep(0.025)
    sending.cancel()
    with pytest.raises(asyncio.CancelledError):
        await sending
    await asyncio.gather(transport.close(), transport.close())

    assert write_calls <= 4
    assert close_calls == 1
    assert not any(
        thread.name.startswith("seeed-hal-pipe-io") and thread.is_alive()
        for thread in threading.enumerate()
    )


@pytest.mark.asyncio
@pytest.mark.parametrize("operation", ["read", "write"])
@pytest.mark.parametrize("code", [5, 231, 536])
async def test_windows_nonretryable_native_error_fails_closed_once(
    monkeypatch, operation: str, code: int
) -> None:
    from seeed_hal.transport_windows import WindowsFramedTransport

    handle = object()
    close_calls = 0
    win32file = ModuleType("win32file")
    win32pipe = ModuleType("win32pipe")

    win32file.CreateFile = lambda *_args: handle
    win32pipe.SetNamedPipeHandleState = lambda *_args: None

    def fail(*_args):
        raise FaithfulPyWinTypesError(code, operation.title(), "Native pipe failure")

    def close_handle(value):
        nonlocal close_calls
        assert value is handle
        close_calls += 1

    win32file.ReadFile = fail if operation == "read" else lambda _handle, count: (0, b"\0" * count)
    win32file.WriteFile = fail if operation == "write" else lambda _handle, data: (0, len(data))
    win32file.CloseHandle = close_handle
    install_win32_modules(monkeypatch, win32file, win32pipe)
    transport = await WindowsFramedTransport.connect(
        rf"\\.\pipe\seeed-hal-terminal-{operation}"
    )

    with pytest.raises(HalError) as caught:
        if operation == "read":
            await asyncio.wait_for(transport.receive(), 0.1)
        else:
            await asyncio.wait_for(transport.send(b"fail"), 0.1)
    await asyncio.gather(transport.close(), transport.close())

    assert caught.value.name == "runtime.broker.disconnected"
    assert caught.value.operation == f"runtime.protocol.{operation}"
    assert close_calls == 1
    assert not any(
        thread.name.startswith("seeed-hal-pipe-io") and thread.is_alive()
        for thread in threading.enumerate()
    )


@pytest.mark.asyncio
async def test_windows_blocked_read_cancellation_transfers_close_to_owned_worker(
    monkeypatch,
) -> None:
    from seeed_hal.transport_windows import WindowsFramedTransport

    handle = object()
    read_started = threading.Event()
    read_release = threading.Event()
    active = 0
    active_lock = threading.Lock()
    close_calls: list[int] = []
    win32file = ModuleType("win32file")
    win32pipe = ModuleType("win32pipe")

    win32file.CreateFile = lambda *_args: handle
    win32pipe.SetNamedPipeHandleState = lambda *_args: None

    def read_file(value, _count):
        nonlocal active
        assert value is handle
        with active_lock:
            active += 1
        read_started.set()
        assert read_release.wait(2)
        with active_lock:
            active -= 1
        return 0, b"\0\0\0\0"

    def close_handle(value):
        assert value is handle
        with active_lock:
            assert active == 0, "handle close raced an active read"
        close_calls.append(threading.get_ident())

    win32file.ReadFile = read_file
    win32file.WriteFile = lambda _handle, data: (0, len(data))
    win32file.CloseHandle = close_handle
    install_win32_modules(monkeypatch, win32file, win32pipe)
    transport = await WindowsFramedTransport.connect(r"\\.\pipe\seeed-hal-blocked-read")
    receiving = asyncio.create_task(transport.receive())
    assert await asyncio.to_thread(read_started.wait, 1)

    receiving.cancel()
    closing = asyncio.create_task(transport.close())
    await asyncio.sleep(0)
    closing.cancel()
    await asyncio.sleep(0)
    closing.cancel()
    try:
        assert not receiving.done()
        assert not closing.done()
        assert close_calls == []
    finally:
        read_release.set()
        results = await asyncio.gather(receiving, closing, return_exceptions=True)

    assert all(isinstance(result, asyncio.CancelledError) for result in results)
    assert len(close_calls) == 1
    assert not any(
        thread.name.startswith("seeed-hal-pipe-io") and thread.is_alive()
        for thread in threading.enumerate()
    )


@pytest.mark.asyncio
async def test_windows_blocked_write_cancellation_transfers_close_to_owned_worker(
    monkeypatch,
) -> None:
    from seeed_hal.transport_windows import WindowsFramedTransport

    handle = object()
    write_started = threading.Event()
    write_release = threading.Event()
    active = 0
    active_lock = threading.Lock()
    close_calls: list[int] = []
    win32file = ModuleType("win32file")
    win32pipe = ModuleType("win32pipe")

    win32file.CreateFile = lambda *_args: handle
    win32pipe.SetNamedPipeHandleState = lambda *_args: None
    win32file.ReadFile = lambda _handle, count: (0, b"\0" * count)

    def write_file(value, data):
        nonlocal active
        assert value is handle
        with active_lock:
            active += 1
        write_started.set()
        assert write_release.wait(2)
        with active_lock:
            active -= 1
        return 0, len(data)

    def close_handle(value):
        assert value is handle
        with active_lock:
            assert active == 0, "handle close raced an active write"
        close_calls.append(threading.get_ident())

    win32file.WriteFile = write_file
    win32file.CloseHandle = close_handle
    install_win32_modules(monkeypatch, win32file, win32pipe)
    transport = await WindowsFramedTransport.connect(r"\\.\pipe\seeed-hal-blocked-write")
    sending = asyncio.create_task(transport.send(b"blocked"))
    assert await asyncio.to_thread(write_started.wait, 1)

    sending.cancel()
    closing = asyncio.create_task(transport.close())
    await asyncio.sleep(0)
    closing.cancel()
    await asyncio.sleep(0)
    closing.cancel()
    try:
        assert not sending.done()
        assert not closing.done()
        assert close_calls == []
    finally:
        write_release.set()
        results = await asyncio.gather(sending, closing, return_exceptions=True)

    assert all(isinstance(result, asyncio.CancelledError) for result in results)
    assert len(close_calls) == 1
    assert not any(
        thread.name.startswith("seeed-hal-pipe-io") and thread.is_alive()
        for thread in threading.enumerate()
    )


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


@pytest.mark.asyncio
async def test_windows_repeated_cancel_while_worker_pending_cannot_abandon_handle(
    monkeypatch,
) -> None:
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
        WindowsFramedTransport.connect(r"\\.\pipe\seeed-hal-repeat-worker")
    )
    assert await asyncio.to_thread(state_started.wait, 1)
    connecting.cancel()
    await asyncio.sleep(0)
    connecting.cancel()
    connecting.cancel()
    await asyncio.sleep(0)
    completed_before_release = connecting.done()
    state_release.set()
    result = (await asyncio.gather(connecting, return_exceptions=True))[0]
    await asyncio.to_thread(closed.wait, 1)

    assert not completed_before_release
    assert isinstance(result, asyncio.CancelledError)
    assert [name for name, _thread in calls].count("close") == 1
    assert closed.is_set()
    assert all(thread != main_thread for _name, thread in calls)


@pytest.mark.asyncio
async def test_windows_repeated_cancel_after_worker_completion_closes_once(
    monkeypatch,
) -> None:
    from seeed_hal.transport_windows import WindowsFramedTransport

    main_thread = threading.get_ident()
    handle = object()
    worker_completed = asyncio.Event()
    allow_claim = asyncio.Event()
    calls: list[tuple[str, int]] = []
    win32file = ModuleType("win32file")
    win32pipe = ModuleType("win32pipe")
    real_shield = asyncio.shield
    shield_calls = 0

    async def pause_first_claim(awaitable):
        nonlocal shield_calls
        shield_calls += 1
        value = await real_shield(awaitable)
        if shield_calls == 1:
            worker_completed.set()
            await allow_claim.wait()
        return value

    def create_file(*_args):
        calls.append(("create", threading.get_ident()))
        return handle

    def set_state(*_args):
        calls.append(("state", threading.get_ident()))

    def close_handle(value):
        assert value is handle
        calls.append(("close", threading.get_ident()))

    win32file.CreateFile = create_file
    win32file.CloseHandle = close_handle
    win32pipe.SetNamedPipeHandleState = set_state
    install_win32_modules(monkeypatch, win32file, win32pipe)
    monkeypatch.setattr(asyncio, "shield", pause_first_claim)

    connecting = asyncio.create_task(
        WindowsFramedTransport.connect(r"\\.\pipe\seeed-hal-repeat-claim")
    )
    await asyncio.wait_for(worker_completed.wait(), 1)
    connecting.cancel()
    connecting.cancel()
    connecting.cancel()
    allow_claim.set()
    result = (await asyncio.gather(connecting, return_exceptions=True))[0]

    assert isinstance(result, asyncio.CancelledError)
    assert [name for name, _thread in calls].count("close") == 1
    assert all(thread != main_thread for _name, thread in calls)


@pytest.mark.asyncio
async def test_windows_repeated_cancel_while_close_pending_waits_for_cleanup(
    monkeypatch,
) -> None:
    from seeed_hal.transport_windows import WindowsFramedTransport

    main_thread = threading.get_ident()
    handle = object()
    state_started = threading.Event()
    state_release = threading.Event()
    close_started = threading.Event()
    close_release = threading.Event()
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
        close_started.set()
        assert close_release.wait(1)

    win32file.CreateFile = create_file
    win32file.CloseHandle = close_handle
    win32pipe.SetNamedPipeHandleState = set_state
    install_win32_modules(monkeypatch, win32file, win32pipe)

    connecting = asyncio.create_task(
        WindowsFramedTransport.connect(r"\\.\pipe\seeed-hal-repeat-close")
    )
    assert await asyncio.to_thread(state_started.wait, 1)
    connecting.cancel()
    state_release.set()
    assert await asyncio.to_thread(close_started.wait, 1)
    connecting.cancel()
    await asyncio.sleep(0)
    connecting.cancel()
    await asyncio.sleep(0)
    completed_before_close = connecting.done()
    close_release.set()
    result = (await asyncio.gather(connecting, return_exceptions=True))[0]

    assert not completed_before_close
    assert isinstance(result, asyncio.CancelledError)
    assert [name for name, _thread in calls].count("close") == 1
    assert all(thread != main_thread for _name, thread in calls)


@pytest.mark.asyncio
async def test_windows_close_failure_does_not_replace_connect_cancellation(
    monkeypatch,
) -> None:
    from seeed_hal.transport_windows import WindowsFramedTransport

    handle = object()
    state_started = threading.Event()
    state_release = threading.Event()
    calls: list[str] = []
    win32file = ModuleType("win32file")
    win32pipe = ModuleType("win32pipe")

    def create_file(*_args):
        calls.append("create")
        return handle

    def set_state(*_args):
        calls.append("state")
        state_started.set()
        assert state_release.wait(1)

    def close_handle(value):
        assert value is handle
        calls.append("close")
        raise OSError("close failed")

    win32file.CreateFile = create_file
    win32file.CloseHandle = close_handle
    win32pipe.SetNamedPipeHandleState = set_state
    install_win32_modules(monkeypatch, win32file, win32pipe)

    connecting = asyncio.create_task(
        WindowsFramedTransport.connect(r"\\.\pipe\seeed-hal-close-error")
    )
    assert await asyncio.to_thread(state_started.wait, 1)
    connecting.cancel()
    state_release.set()
    result = (await asyncio.gather(connecting, return_exceptions=True))[0]

    assert isinstance(result, asyncio.CancelledError)
    assert calls.count("close") == 1


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
        SerialConfig(read_timeout_ms=0),
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
async def test_released_serial_write_view_is_a_structured_argument_error() -> None:
    transport = ScriptedTransport()
    client = direct_client(transport)
    serial = SerialSession(
        client,
        "session:valid",
        "lease:valid",
        1,
        hal_pb2.LEASE_MODE_CONTROL,
    )
    payload = memoryview(b"released")
    payload.release()

    with pytest.raises(HalError) as caught:
        await serial.write(payload)

    assert caught.value.name == "runtime.argument.invalid"
    assert transport.sent.empty()
    await client.close()


@pytest.mark.asyncio
@pytest.mark.parametrize(
    ("payload", "expected"),
    [
        (memoryview(bytearray(b"abcdef"))[::2], b"ace"),
        (memoryview(array("H", [0x0102, 0x0304])), struct.pack("=HH", 0x0102, 0x0304)),
    ],
)
async def test_serial_write_normalizes_noncontiguous_and_multibyte_views(
    payload: memoryview, expected: bytes
) -> None:
    transport = ScriptedTransport()
    client = direct_client(transport)
    serial = SerialSession(
        client,
        "session:valid",
        "lease:valid",
        1,
        hal_pb2.LEASE_MODE_CONTROL,
    )

    writing = asyncio.create_task(serial.write(payload))
    request = await next_request(transport)
    assert request.serial_write_request.data == expected
    transport.inbound.put_nowait(
        hal_pb2.Envelope(
            request_id=request.request_id,
            serial_write_response=hal_pb2.Empty(),
        ).SerializeToString()
    )
    await writing
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
    client._terminate(
        HalError(
            "runtime.queue.full",
            ErrorCategory.UNAVAILABLE,
            "serial.write",
            True,
            "full",
            resource_id="serial:virtual:0",
            platform_code="11",
            vendor_code="VENDOR_BUSY",
            context={"queueDepth": "64"},
        )
    )
    first_error, second_error = await asyncio.gather(
        first, second, return_exceptions=True
    )
    assert isinstance(first_error, HalError)
    assert isinstance(second_error, HalError)
    assert first_error == second_error
    assert first_error is not second_error
    assert dict(first_error.context) == {"queueDepth": "64"}
    assert first_error.context is not second_error.context
    with pytest.raises(FrozenInstanceError):
        first_error.name = "changed"
    with pytest.raises(TypeError):
        first_error.context["new"] = "value"

    with pytest.raises(HalError) as repeated:
        await client.enumerate_serial()
    assert repeated.value == first_error
    assert repeated.value is not first_error
    assert repeated.value.context is not first_error.context
    await client.close()


@pytest.mark.asyncio
async def test_malformed_can_status_uses_shared_terminal_fanout() -> None:
    transport = ScriptedTransport()
    client = direct_client(transport)
    session = CanSession(
        client,
        "session:can",
        "lease:can",
        1,
        hal_pb2.LEASE_MODE_CONTROL,
        _CanSessionProfile(
            CanMode.CLASSIC,
            True,
            False,
            False,
            False,
            "can:virtual:hardening",
            "session:can",
        ),
    )
    status = asyncio.create_task(session.bus_status())
    serial = asyncio.create_task(client.enumerate_serial())
    requests = [await next_request(transport), await next_request(transport)]
    status_request = next(
        request
        for request in requests
        if request.WhichOneof("payload") == "get_can_bus_status_request"
    )
    transport.inbound.put_nowait(
        hal_pb2.Envelope(
            request_id=status_request.request_id,
            get_can_bus_status_response=hal_pb2.GetCanBusStatusResponse(
                status=hal_pb2.CanBusStatus(state=999)
            ),
        ).SerializeToString()
    )

    first, second = await asyncio.gather(status, serial, return_exceptions=True)
    assert isinstance(first, HalError)
    assert isinstance(second, HalError)
    assert first.name == "runtime.protocol.invalid_message"
    assert second.name == "runtime.protocol.invalid_message"
    assert first is not second
    await client.close()


@pytest.mark.asyncio
@pytest.mark.parametrize(
    "peer_error",
    [
        hal_pb2.Error(resource_id="é"),
        hal_pb2.Error(resource_id="r" * 256),
        hal_pb2.Error(platform_code="é"),
        hal_pb2.Error(platform_code="x" * 256),
        hal_pb2.Error(vendor_code="é"),
        hal_pb2.Error(vendor_code="x" * 256),
        hal_pb2.Error(context={"QueueDepth": "64"}),
        hal_pb2.Error(context={"k" * 65: "x"}),
        hal_pb2.Error(context={"key": "x" * 1025}),
        hal_pb2.Error(
            context={f"key{index}": "x" for index in range(17)}
        ),
        hal_pb2.Error(
            context={
                f"k{index}": "x" * (1023 if index == 0 else 1022)
                for index in range(8)
            }
        ),
    ],
)
async def test_malformed_broker_error_details_terminate_with_invalid_message(
    peer_error: hal_pb2.Error,
) -> None:
    transport = ScriptedTransport()
    client = direct_client(transport)
    first = asyncio.create_task(client.enumerate_serial())
    second = asyncio.create_task(client.enumerate_serial())
    first_request = await next_request(transport)
    await next_request(transport)
    peer_error.name = "runtime.queue.full"
    peer_error.category = hal_pb2.ERROR_CATEGORY_UNAVAILABLE
    peer_error.operation = "serial.write"
    peer_error.retryable = True
    peer_error.debug_message = "full"
    transport.inbound.put_nowait(
        hal_pb2.Envelope(
            request_id=first_request.request_id,
            error=peer_error,
        ).SerializeToString()
    )

    first_error, second_error = await asyncio.gather(
        first, second, return_exceptions=True
    )
    assert isinstance(first_error, HalError)
    assert isinstance(second_error, HalError)
    assert first_error.name == "runtime.protocol.invalid_message"
    assert second_error.name == "runtime.protocol.invalid_message"
    assert first_error is not second_error
    with pytest.raises(HalError) as repeated:
        await client.enumerate_serial()
    assert repeated.value.name == "runtime.protocol.invalid_message"
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
            ["bash", checker],
            cwd=REPO_ROOT,
            text=True,
            capture_output=True,
            timeout=30,
        )
    finally:
        stale.unlink(missing_ok=True)
    assert result.returncode != 0
    assert "stale_pb2.py" in result.stdout + result.stderr


def _generation_check_repo(tmp_path: Path) -> Path:
    fixture = tmp_path / "repo"
    files = [
        ".gitignore",
        "scripts/check-generated-protocol.sh",
        "scripts/generate-protocol.sh",
        "bindings/python/pyproject.toml",
        "bindings/python/uv.lock",
        "bindings/python/seeed_hal/proto/__init__.py",
        "bindings/python/seeed_hal/proto/hal_pb2.py",
        "proto/seeed/hal/v1/hal.proto",
    ]
    for relative in files:
        source = REPO_ROOT / relative
        destination = fixture / relative
        destination.parent.mkdir(parents=True, exist_ok=True)
        shutil.copy2(source, destination)
    subprocess.run(["git", "init", "-q"], cwd=fixture, check=True)
    subprocess.run(
        ["git", "config", "user.email", "tests@example.invalid"],
        cwd=fixture,
        check=True,
    )
    subprocess.run(
        ["git", "config", "user.name", "Tests"], cwd=fixture, check=True
    )
    subprocess.run(["git", "add", "."], cwd=fixture, check=True)
    subprocess.run(
        ["git", "commit", "-qm", "fixture"], cwd=fixture, check=True
    )
    return fixture


@pytest.mark.parametrize(
    "drift",
    ["clean", "modified", "deleted", "stale", "schema"],
)
def test_generation_check_is_non_mutating_for_clean_and_drifted_trees(
    tmp_path: Path, drift: str
) -> None:
    fixture = _generation_check_repo(tmp_path)
    output = fixture / "bindings/python/seeed_hal/proto"
    generated = output / "hal_pb2.py"
    if drift == "modified":
        generated.write_text("# locally modified\n", encoding="utf-8")
    elif drift == "deleted":
        generated.unlink()
    elif drift == "stale":
        (output / "stale_pb2.py").write_text("# stale\n", encoding="utf-8")
    elif drift == "schema":
        schema = fixture / "proto/seeed/hal/v1/hal.proto"
        schema.write_text(
            schema.read_text(encoding="utf-8") + "\nmessage DriftProbe {}\n",
            encoding="utf-8",
        )

    original_bytes = generated.read_bytes() if generated.exists() else None
    original_status = subprocess.run(
        ["git", "status", "--porcelain", "--untracked-files=all"],
        cwd=fixture,
        check=True,
        text=True,
        capture_output=True,
    ).stdout
    result = subprocess.run(
        ["bash", fixture / "scripts/check-generated-protocol.sh"],
        cwd=fixture,
        text=True,
        capture_output=True,
        timeout=30,
    )
    final_bytes = generated.read_bytes() if generated.exists() else None
    final_status = subprocess.run(
        ["git", "status", "--porcelain", "--untracked-files=all"],
        cwd=fixture,
        check=True,
        text=True,
        capture_output=True,
    ).stdout

    if drift == "clean":
        assert result.returncode == 0, result.stdout + result.stderr
    else:
        assert result.returncode != 0
    assert final_bytes == original_bytes
    assert final_status == original_status
