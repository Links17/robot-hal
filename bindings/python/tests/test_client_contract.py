from __future__ import annotations

import asyncio
from contextlib import asynccontextmanager
import inspect
import os
from pathlib import Path
import struct
import sys
import threading
from types import ModuleType
from unittest.mock import patch

from google.protobuf.message import Message
import pytest

import seeed_hal
from seeed_hal import (
    ControlLines,
    ErrorCategory,
    HalClient,
    HalError,
    IdentityQuality,
    RuntimeEvent,
    SerialConfig,
    TransportKind,
)
from seeed_hal.proto import hal_pb2


TOKEN = bytearray([0x5A] * 32)
HARD_FRAME_BYTES = 1024 * 1024


async def read_frame(reader: asyncio.StreamReader, limit: int = HARD_FRAME_BYTES) -> bytes:
    length = struct.unpack(">I", await reader.readexactly(4))[0]
    if length > limit:
        raise AssertionError(f"test client sent oversized frame prefix: {length}")
    return await reader.readexactly(length)


async def send_frame(writer: asyncio.StreamWriter, payload: bytes) -> None:
    writer.write(struct.pack(">I", len(payload)) + payload)
    await writer.drain()


def envelope(request_id: int, field: str, value: Message) -> hal_pb2.Envelope:
    result = hal_pb2.Envelope(request_id=request_id)
    getattr(result, field).CopyFrom(value)
    return result


def enumerate_response(request_id: int, resource_id: str) -> hal_pb2.Envelope:
    return envelope(
        request_id,
        "enumerate_serial_response",
        hal_pb2.EnumerateSerialResponse(
            resources=[
                hal_pb2.ResourceDescriptor(
                    resource_id=resource_id,
                    endpoint=f"virtual://{resource_id}",
                    identity_quality=hal_pb2.IDENTITY_QUALITY_STRONG,
                    transport=hal_pb2.TRANSPORT_KIND_SERIAL,
                )
            ]
        ),
    )


@asynccontextmanager
async def fake_broker(
    handler,
    *,
    frame=HARD_FRAME_BYTES,
    read=64 * 1024,
    write=64 * 1024,
    selected_minor=0,
    minimum_minor=0,
    maximum_minor=0,
):
    if os.name == "nt":
        pytest.skip("Unix socket protocol fault injection is covered on Unix CI")
    import tempfile

    with tempfile.TemporaryDirectory(prefix="seeed-hal-fake-") as directory:
        endpoint = Path(directory) / "broker.sock"

        async def serve(reader_stream: asyncio.StreamReader, writer_stream: asyncio.StreamWriter):
            try:
                hello = hal_pb2.Envelope.FromString(await read_frame(reader_stream))
                assert hello.request_id == 1
                assert hello.WhichOneof("payload") == "handshake_request"
                assert hello.handshake_request.protocol_minor_minimum == 0
                assert hello.handshake_request.protocol_minor_maximum == 0
                await send_frame(
                    writer_stream,
                    envelope(
                        1,
                        "handshake_response",
                        hal_pb2.HandshakeResponse(
                            protocol_major=1,
                            protocol_minor=selected_minor,
                            capabilities=["serial.bytes/v1"],
                            max_frame_bytes=frame,
                            max_read_bytes=read,
                            max_write_bytes=write,
                            protocol_minor_minimum=minimum_minor,
                            protocol_minor_maximum=maximum_minor,
                        ),
                    ).SerializeToString(),
                )
                await handler(reader_stream, writer_stream)
            finally:
                writer_stream.close()
                await writer_stream.wait_closed()

        server = await asyncio.start_unix_server(serve, endpoint)
        try:
            yield str(endpoint)
        finally:
            server.close()
            await server.wait_closed()


def test_public_api_is_typed_and_does_not_export_protobuf_objects() -> None:
    assert HalClient.__module__ == "seeed_hal.client"
    assert SerialConfig.__module__ == "seeed_hal.serial"
    for name in seeed_hal.__all__:
        exported = getattr(seeed_hal, name)
        assert not (inspect.isclass(exported) and issubclass(exported, Message))
    assert "proto" not in seeed_hal.__all__


def test_hal_error_structured_details_are_immutable_copies() -> None:
    source = {"queueDepth": "64"}
    error = HalError(
        "runtime.queue.full",
        ErrorCategory.UNAVAILABLE,
        "serial.write",
        True,
        "full",
        resource_id="serial:virtual:0",
        platform_code="11",
        vendor_code="VENDOR_BUSY",
        context=source,
    )

    source["queueDepth"] = "changed"

    assert error.resource_id == "serial:virtual:0"
    assert error.platform_code == "11"
    assert error.vendor_code == "VENDOR_BUSY"
    assert dict(error.context) == {"queueDepth": "64"}
    with pytest.raises(TypeError):
        error.context["new"] = "value"


@pytest.mark.asyncio
async def test_python_client_accepts_overlap_selection_and_rejects_no_shared_minor() -> None:
    release = asyncio.Event()

    async def handler(_reader, _writer):
        await release.wait()

    async with fake_broker(handler, minimum_minor=0, maximum_minor=3) as endpoint:
        client = await HalClient.connect(endpoint, TOKEN)
        assert client.protocol_minor == 0
        await client.close()
        release.set()

    async with fake_broker(
        handler,
        selected_minor=1,
        minimum_minor=0,
        maximum_minor=1,
    ) as endpoint:
        with pytest.raises(HalError) as caught:
            await HalClient.connect(endpoint, TOKEN)
        assert caught.value.name == "runtime.protocol.invalid_handshake"


@pytest.mark.asyncio
async def test_invalid_python_arguments_use_stable_hal_errors() -> None:
    with pytest.raises(HalError) as invalid_limit:
        await HalClient.connect("not-used", TOKEN, max_frame_bytes=True)
    assert invalid_limit.value.name == "runtime.argument.invalid"

    release = asyncio.Event()

    async def handler(_reader, _writer):
        await release.wait()

    async with fake_broker(handler) as endpoint:
        client = await HalClient.connect(endpoint, TOKEN)
        with pytest.raises(HalError) as invalid_selector:
            await client.open_serial(object(), SerialConfig())
        assert invalid_selector.value.name == "runtime.argument.invalid"
        await client.close()
        release.set()


@pytest.mark.asyncio
async def test_python_client_round_trips_complete_serial_contract(broker) -> None:
    client = await HalClient.connect(broker.endpoint, broker.token)
    assert client.protocol_minor == 0
    events = client.subscribe()
    resources = await client.enumerate_serial()
    assert resources[0].identity_quality is IdentityQuality.STRONG
    assert resources[0].transport is TransportKind.SERIAL
    assert resources[0].capabilities == ("serial.bytes/v1",)

    serial = await client.open_serial(resources[0].selector(), SerialConfig())
    opened = await asyncio.wait_for(events.receive(), 1)
    assert opened.name == "session.opened"
    await serial.write(b"python")
    await serial.flush()
    await serial.set_control_lines(ControlLines(True, True))
    assert await serial.read(6) == b"python"

    with pytest.raises(HalError) as caught:
        await client.open_serial(resources[0].selector(), SerialConfig())
    assert caught.value.name == "runtime.lease.conflict"
    assert caught.value.category is ErrorCategory.CONFLICT
    assert caught.value.operation == "serial.open"
    assert not caught.value.retryable

    await serial.close()
    closed = await asyncio.wait_for(events.receive(), 1)
    assert closed.name == "session.closed"
    assert closed.sequence > opened.sequence
    await client.close()


@pytest.mark.asyncio
async def test_reversed_responses_remain_correlated() -> None:
    async def handler(reader, writer):
        first = hal_pb2.Envelope.FromString(await read_frame(reader))
        second = hal_pb2.Envelope.FromString(await read_frame(reader))
        await send_frame(writer, enumerate_response(second.request_id, "serial:second").SerializeToString())
        await send_frame(writer, enumerate_response(first.request_id, "serial:first").SerializeToString())

    async with fake_broker(handler) as endpoint:
        client = await HalClient.connect(endpoint, TOKEN)
        first, second = await asyncio.gather(client.enumerate_serial(), client.enumerate_serial())
        assert first[0].resource_id == "serial:first"
        assert second[0].resource_id == "serial:second"
        await client.close()


@pytest.mark.asyncio
async def test_pending_backpressure_and_cancellation_are_bounded() -> None:
    first_seen = asyncio.Event()
    release = asyncio.Event()

    async def handler(reader, writer):
        first = hal_pb2.Envelope.FromString(await read_frame(reader))
        first_seen.set()
        await release.wait()
        second = hal_pb2.Envelope.FromString(await read_frame(reader))
        await send_frame(writer, enumerate_response(second.request_id, "serial:next").SerializeToString())
        await send_frame(writer, enumerate_response(first.request_id, "serial:cancelled").SerializeToString())

    async with fake_broker(handler) as endpoint:
        client = await HalClient.connect(endpoint, TOKEN, pending_capacity=1, writer_capacity=2)
        cancelled = asyncio.create_task(client.enumerate_serial())
        await asyncio.wait_for(first_seen.wait(), 1)
        with pytest.raises(HalError, match="runtime.queue.full"):
            await client.enumerate_serial()
        cancelled.cancel()
        with pytest.raises(asyncio.CancelledError):
            await cancelled
        next_call = asyncio.create_task(client.enumerate_serial())
        release.set()
        assert (await asyncio.wait_for(next_call, 1))[0].resource_id == "serial:next"
        await asyncio.sleep(0)
        await client.close()


@pytest.mark.asyncio
async def test_broker_error_response_preserves_rich_structured_details() -> None:
    async def handler(reader, writer):
        request = hal_pb2.Envelope.FromString(await read_frame(reader))
        await send_frame(
            writer,
            envelope(
                request.request_id,
                "error",
                hal_pb2.Error(
                    name="runtime.queue.full",
                    category=hal_pb2.ERROR_CATEGORY_UNAVAILABLE,
                    operation="serial.write",
                    retryable=True,
                    debug_message="full",
                    resource_id="serial:virtual:0",
                    platform_code="11",
                    vendor_code="VENDOR_BUSY",
                    context={"queueDepth": "64"},
                ),
            ).SerializeToString(),
        )

    async with fake_broker(handler) as endpoint:
        client = await HalClient.connect(endpoint, TOKEN)
        with pytest.raises(HalError) as caught:
            await client.enumerate_serial()
        error = caught.value
        assert error.resource_id == "serial:virtual:0"
        assert error.platform_code == "11"
        assert error.vendor_code == "VENDOR_BUSY"
        assert dict(error.context) == {"queueDepth": "64"}
        with pytest.raises(TypeError):
            error.context["new"] = "value"
        await client.close()


@pytest.mark.asyncio
async def test_legacy_broker_error_response_has_empty_structured_details() -> None:
    async def handler(reader, writer):
        request = hal_pb2.Envelope.FromString(await read_frame(reader))
        await send_frame(
            writer,
            envelope(
                request.request_id,
                "error",
                hal_pb2.Error(
                    name="runtime.queue.full",
                    category=hal_pb2.ERROR_CATEGORY_UNAVAILABLE,
                    operation="serial.write",
                    retryable=True,
                    debug_message="full",
                ),
            ).SerializeToString(),
        )

    async with fake_broker(handler) as endpoint:
        client = await HalClient.connect(endpoint, TOKEN)
        with pytest.raises(HalError) as caught:
            await client.enumerate_serial()
        error = caught.value
        assert error.resource_id is None
        assert error.platform_code is None
        assert error.vendor_code is None
        assert dict(error.context) == {}
        await client.close()


@pytest.mark.asyncio
@pytest.mark.parametrize(
    ("response", "expected"),
    [
        (enumerate_response(999, "serial:unknown").SerializeToString(), "runtime.protocol.unknown_response"),
        (envelope(1, "handshake_response", hal_pb2.HandshakeResponse()).SerializeToString(), "runtime.protocol.duplicate_response"),
        (b"\xff", "runtime.protocol.invalid_message"),
    ],
)
async def test_invalid_inbound_messages_fail_connection(response: bytes, expected: str) -> None:
    async def handler(_reader, writer):
        await send_frame(writer, response)
        await asyncio.sleep(0.05)

    async with fake_broker(handler) as endpoint:
        client = await HalClient.connect(endpoint, TOKEN)
        await asyncio.sleep(0)
        with pytest.raises(HalError) as caught:
            await client.enumerate_serial()
        assert caught.value.name == expected
        with pytest.raises(HalError) as repeated:
            await client.enumerate_serial()
        assert repeated.value.name == expected
        await client.close()


@pytest.mark.asyncio
async def test_disconnect_and_close_fan_out_stable_errors() -> None:
    calls_seen = asyncio.Event()

    async def disconnecting(reader, _writer):
        await read_frame(reader)
        await read_frame(reader)
        calls_seen.set()

    async with fake_broker(disconnecting) as endpoint:
        client = await HalClient.connect(endpoint, TOKEN)
        calls = [asyncio.create_task(client.enumerate_serial()) for _ in range(2)]
        await asyncio.wait_for(calls_seen.wait(), 1)
        results = await asyncio.gather(*calls, return_exceptions=True)
        assert {error.name for error in results} == {"runtime.broker.disconnected"}
        await client.close()

    release = asyncio.Event()

    async def waiting(reader, _writer):
        await read_frame(reader)
        await read_frame(reader)
        await release.wait()

    async with fake_broker(waiting) as endpoint:
        client = await HalClient.connect(endpoint, TOKEN)
        calls = [asyncio.create_task(client.enumerate_serial()) for _ in range(2)]
        await asyncio.sleep(0.02)
        await client.close()
        results = await asyncio.gather(*calls, return_exceptions=True)
        assert {error.name for error in results} == {"runtime.client.closed"}
        release.set()


@pytest.mark.asyncio
async def test_client_close_wakes_a_waiting_event_subscriber() -> None:
    release = asyncio.Event()

    async def handler(_reader, _writer):
        await release.wait()

    async with fake_broker(handler) as endpoint:
        client = await HalClient.connect(endpoint, TOKEN)
        events = client.subscribe()
        waiting = asyncio.create_task(events.receive())
        await asyncio.sleep(0)
        await client.close()
        with pytest.raises(HalError) as caught:
            await asyncio.wait_for(waiting, 1)
        assert caught.value.name == "runtime.event.closed"
        release.set()


@pytest.mark.asyncio
async def test_negotiated_limits_reject_operations_and_oversized_prefix() -> None:
    async def handler(reader, writer):
        enumerate_request = hal_pb2.Envelope.FromString(await read_frame(reader))
        await send_frame(writer, enumerate_response(enumerate_request.request_id, "serial:limits").SerializeToString())
        open_request = hal_pb2.Envelope.FromString(await read_frame(reader))
        await send_frame(
            writer,
            envelope(
                open_request.request_id,
                "open_serial_response",
                hal_pb2.OpenSerialResponse(
                    session_id="session-limits",
                    lease=hal_pb2.LeaseToken(lease_id="lease-limits", generation=1, mode=hal_pb2.LEASE_MODE_CONTROL),
                ),
            ).SerializeToString(),
        )
        await asyncio.sleep(0.1)

    async with fake_broker(handler, frame=256, read=8, write=8) as endpoint:
        client = await HalClient.connect(endpoint, TOKEN, max_frame_bytes=256, max_read_bytes=8, max_write_bytes=8)
        serial = await client.open_serial((await client.enumerate_serial())[0].selector(), SerialConfig())
        with pytest.raises(HalError, match="runtime.argument.invalid"):
            await serial.read(9)
        with pytest.raises(HalError, match="runtime.argument.invalid"):
            await serial.write(b"123456789")
        await client.close()

    async def oversized_prefix(_reader, writer):
        writer.write(struct.pack(">I", 257))
        await writer.drain()
        await asyncio.sleep(0.1)

    async with fake_broker(oversized_prefix, frame=256) as endpoint:
        client = await HalClient.connect(endpoint, TOKEN, max_frame_bytes=256)
        with pytest.raises(HalError) as caught:
            await client.enumerate_serial()
        assert caught.value.name == "runtime.protocol.frame_too_large"
        await client.close()


@pytest.mark.asyncio
async def test_read_response_is_preflighted_against_requested_size() -> None:
    async def handler(reader, writer):
        enumerate_request = hal_pb2.Envelope.FromString(await read_frame(reader))
        await send_frame(writer, enumerate_response(enumerate_request.request_id, "serial:read-limit").SerializeToString())
        open_request = hal_pb2.Envelope.FromString(await read_frame(reader))
        await send_frame(
            writer,
            envelope(
                open_request.request_id,
                "open_serial_response",
                hal_pb2.OpenSerialResponse(
                    session_id="session-read-limit",
                    lease=hal_pb2.LeaseToken(lease_id="lease-read-limit", generation=1, mode=hal_pb2.LEASE_MODE_CONTROL),
                ),
            ).SerializeToString(),
        )
        read_request = hal_pb2.Envelope.FromString(await read_frame(reader))
        await send_frame(
            writer,
            envelope(read_request.request_id, "serial_read_response", hal_pb2.SerialReadResponse(data=b"123456789")).SerializeToString(),
        )

    async with fake_broker(handler, frame=512, read=16, write=16) as endpoint:
        client = await HalClient.connect(endpoint, TOKEN, max_frame_bytes=512, max_read_bytes=16, max_write_bytes=16)
        serial = await client.open_serial((await client.enumerate_serial())[0].selector(), SerialConfig())
        with pytest.raises(HalError) as caught:
            await serial.read(8)
        assert caught.value.name == "runtime.protocol.frame_too_large"
        with pytest.raises(HalError) as terminal:
            await client.enumerate_serial()
        assert terminal.value.name == caught.value.name
        await client.close()


@pytest.mark.asyncio
async def test_request_id_exhaustion_uses_last_nonzero_id() -> None:
    seen = []

    async def handler(reader, writer):
        request = hal_pb2.Envelope.FromString(await read_frame(reader))
        seen.append(request.request_id)
        await send_frame(writer, enumerate_response(request.request_id, "serial:last-id").SerializeToString())
        await asyncio.sleep(0.05)

    async with fake_broker(handler) as endpoint:
        client = await HalClient.connect(endpoint, TOKEN)
        client._next_request_id = (1 << 64) - 1
        assert (await client.enumerate_serial())[0].resource_id == "serial:last-id"
        with pytest.raises(HalError) as caught:
            await client.enumerate_serial()
        assert caught.value.name == "runtime.protocol.request_id_exhausted"
        assert seen == [(1 << 64) - 1]
        await client.close()


@pytest.mark.asyncio
async def test_windows_transport_owns_steady_state_pywin32_calls_on_one_thread(
    monkeypatch,
) -> None:
    from seeed_hal.transport_windows import WindowsFramedTransport

    win32file = ModuleType("win32file")
    win32pipe = ModuleType("win32pipe")
    pywintypes = ModuleType("pywintypes")
    pywintypes.error = type("error", (Exception,), {})
    calls = []
    main_thread = threading.get_ident()
    wire = bytearray(struct.pack(">I", 2) + b"ok")

    def create_file(*args):
        calls.append(("connect", threading.get_ident(), args))
        return object()

    def read_file(_handle, count):
        calls.append(("read", threading.get_ident(), count))
        chunk = bytes(wire[:count])
        del wire[:count]
        return 0, chunk

    def write_file(_handle, data):
        calls.append(("write", threading.get_ident(), bytes(data)))
        return 0, len(data)

    def close_handle(_handle):
        calls.append(("close", threading.get_ident()))

    win32file.CreateFile = create_file
    win32file.ReadFile = read_file
    win32file.WriteFile = write_file
    win32file.CloseHandle = close_handle
    win32file.GENERIC_READ = 1
    win32file.GENERIC_WRITE = 2
    win32file.OPEN_EXISTING = 3
    win32pipe.SetNamedPipeHandleState = lambda *_args: calls.append(
        ("state", threading.get_ident())
    )
    win32pipe.PIPE_READMODE_BYTE = 0
    win32pipe.PIPE_NOWAIT = 1
    monkeypatch.setitem(sys.modules, "win32file", win32file)
    monkeypatch.setitem(sys.modules, "win32pipe", win32pipe)
    monkeypatch.setitem(sys.modules, "pywintypes", pywintypes)

    real_to_thread = asyncio.to_thread
    delegated = []

    async def observing_to_thread(function, *args):
        delegated.append(function)
        return await real_to_thread(function, *args)

    with patch("asyncio.to_thread", observing_to_thread):
        transport = await WindowsFramedTransport.connect(r"\\.\pipe\seeed-hal-test")
        assert await transport.receive() == b"ok"
        await transport.send(b"go")
        await transport.close()

    assert [entry[0] for entry in calls] == ["connect", "state", "read", "read", "write", "close"]
    assert len(delegated) == 1
    actor_threads = {entry[1] for entry in calls if entry[0] in {"read", "write", "close"}}
    assert len(actor_threads) == 1
    assert actor_threads != {main_thread}


def test_non_windows_import_does_not_require_pywin32() -> None:
    assert "win32file" not in sys.modules or os.name == "nt"
    from seeed_hal.transport_windows import WindowsFramedTransport

    assert WindowsFramedTransport.__name__ == "WindowsFramedTransport"
