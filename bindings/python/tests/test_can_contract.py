from __future__ import annotations

import asyncio
from dataclasses import FrozenInstanceError

import pytest

from seeed_hal import (
    CanBatchSendError,
    CanBitTiming,
    CanBusState,
    CanBusStatus,
    CanConfigureConfig,
    CanErrorClass,
    CanFilter,
    CanFilterSet,
    CanFrame,
    CanFrameClasses,
    CanId,
    CanIdFormat,
    CanLinkExpectation,
    CanMode,
    CanOpenConfig,
    CanSession,
    CanTimestamp,
    CanTimestampSource,
    ErrorCategory,
    HalClient,
    HalError,
    IdentityQuality,
    LeaseMode,
    ResourceSelector,
    TransportKind,
)
from seeed_hal.can import _CanSessionProfile
from seeed_hal.proto import hal_pb2


MAX_U32 = (1 << 32) - 1
MAX_U64 = (1 << 64) - 1


class ScriptedTransport:
    def __init__(self) -> None:
        self.sent: asyncio.Queue[bytes] = asyncio.Queue()
        self.inbound: asyncio.Queue[bytes | BaseException] = asyncio.Queue()
        self.closed = False
        self.frame_limit = 1024 * 1024

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
    transport: ScriptedTransport,
    *,
    protocol_minor: int = 1,
    capabilities: frozenset[str] | None = None,
    read_limit: int = 64 * 1024,
    write_limit: int = 64 * 1024,
    pending_capacity: int = 32,
    writer_capacity: int = 32,
) -> HalClient:
    return HalClient(
        transport,
        protocol_minor=protocol_minor,
        capabilities=capabilities
        or frozenset(
            {
                "serial.bytes/v1",
                "can.classic/v1",
                "can.fd/v1",
                "can.configure/v1",
                "can.error-frames/v1",
                "can.rx-timestamp/v1",
            }
        ),
        frame_limit=1024 * 1024,
        read_limit=read_limit,
        write_limit=write_limit,
        pending_capacity=pending_capacity,
        writer_capacity=writer_capacity,
        event_capacity=64,
    )


async def next_request(transport: ScriptedTransport) -> hal_pb2.Envelope:
    return hal_pb2.Envelope.FromString(await asyncio.wait_for(transport.sent.get(), 1))


def response(request_id: int, field: str, value) -> bytes:
    envelope = hal_pb2.Envelope(request_id=request_id)
    getattr(envelope, field).CopyFrom(value)
    return envelope.SerializeToString()


def descriptor(*, capabilities: list[str] | None = None) -> hal_pb2.ResourceDescriptor:
    return hal_pb2.ResourceDescriptor(
        resource_id="can:virtual:test",
        endpoint="virtual://can/can:virtual:test",
        identity_quality=hal_pb2.IDENTITY_QUALITY_STRONG,
        transport=hal_pb2.TRANSPORT_KIND_CAN,
        capabilities=capabilities
        or [
            "can.classic/v1",
            "can.fd/v1",
            "can.configure/v1",
            "can.error-frames/v1",
            "can.rx-timestamp/v1",
        ],
    )


async def open_session(
    client: HalClient,
    transport: ScriptedTransport,
    *,
    mode: CanMode = CanMode.CLASSIC,
    capabilities: list[str] | None = None,
):
    opening = asyncio.create_task(
        client.open_can(
            ResourceSelector(
                "can:virtual:test", IdentityQuality.STRONG, TransportKind.CAN
            ),
            LeaseMode.CONTROL,
            CanOpenConfig(attach=CanLinkExpectation(mode=mode)),
            CanFilterSet(),
        )
    )
    enumerate_request = await next_request(transport)
    transport.inbound.put_nowait(
        response(
            enumerate_request.request_id,
            "enumerate_can_response",
            hal_pb2.EnumerateCanResponse(resources=[descriptor(capabilities=capabilities)]),
        )
    )
    open_request = await next_request(transport)
    transport.inbound.put_nowait(
        response(
            open_request.request_id,
            "open_can_response",
            hal_pb2.OpenCanResponse(
                session_id="session:can",
                lease=hal_pb2.LeaseToken(
                    lease_id="lease:can",
                    generation=1,
                    mode=hal_pb2.LEASE_MODE_CONTROL,
                ),
            ),
        )
    )
    return await asyncio.wait_for(opening, 1)


def test_models_are_immutable_and_defensively_copy_bytes() -> None:
    source = bytearray(b"12345678")
    frame = CanFrame.classic_data(CanId.standard(0x7FF), source)
    source[0] = ord("x")
    assert frame.data == b"12345678"
    with pytest.raises(FrozenInstanceError):
        frame.data = b"x"
    with pytest.raises(HalError) as caught:
        CanFrame.classic_data(CanId.standard(0), b"123456789")
    assert caught.value.name == "can.frame.invalid"


@pytest.mark.parametrize(
    ("factory", "good", "bad"),
    [
        (CanId.standard, 0x7FF, 0x800),
        (CanId.extended, 0x1FFF_FFFF, 0x2000_0000),
    ],
)
def test_identifier_exact_bounds(factory, good: int, bad: int) -> None:
    factory(good)
    with pytest.raises(HalError):
        factory(bad)


def test_all_frame_variants_and_exact_fd_lengths() -> None:
    can_id = CanId.extended(1)
    CanFrame.classic_remote(can_id, 8)
    for length in (0, 1, 8, 12, 16, 20, 24, 32, 48, 64):
        CanFrame.fd_data(can_id, bytes(length))
    rejected = (
        9, 10, 11, 13, 14, 15, 17, 18, 19, 21, 22, 23,
        25, 26, 27, 28, 29, 30, 31, 33, 47, 49, 63, 65,
    )
    for length in rejected:
        with pytest.raises(HalError):
            CanFrame.fd_data(can_id, bytes(length))
    CanFrame.error(tuple(CanErrorClass)[:10], b"12345678")
    with pytest.raises(HalError):
        CanFrame.error((), b"")
    with pytest.raises(HalError):
        CanFrame.error(tuple(CanErrorClass) + (CanErrorClass.OTHER,), b"")


def test_timestamp_configuration_and_filter_bounds() -> None:
    CanTimestamp(MAX_U64, CanTimestampSource.HARDWARE, "x" * 255)
    with pytest.raises(HalError):
        CanTimestamp(0, CanTimestampSource.KERNEL, "")
    nominal = CanBitTiming(MAX_U32, 999, MAX_U32 & 0xFFFF)
    CanConfigureConfig(CanMode.CLASSIC, nominal)
    with pytest.raises(HalError):
        CanConfigureConfig(CanMode.CLASSIC, nominal, nominal)
    with pytest.raises(HalError):
        CanConfigureConfig(CanMode.FD, nominal)
    filt = CanFilter(0x7FF, 0x7FF, CanIdFormat.STANDARD, CanFrameClasses.data_only())
    CanFilterSet([filt] * 64)
    with pytest.raises(HalError):
        CanFilterSet([filt] * 65)
    assert CanFilterSet().matches(CanFrame.classic_data(CanId.standard(1), b"x"))


@pytest.mark.asyncio
async def test_minor_and_capability_gates_reject_before_wire() -> None:
    transport = ScriptedTransport()
    client = direct_client(transport, protocol_minor=0)
    with pytest.raises(HalError) as caught:
        await client.enumerate_can()
    assert caught.value.name == "runtime.protocol.capability_unsupported"
    assert transport.sent.empty()
    await client.close()

    transport = ScriptedTransport()
    client = direct_client(transport, capabilities=frozenset({"serial.bytes/v1"}))
    with pytest.raises(HalError):
        await client.enumerate_can()
    assert transport.sent.empty()
    await client.close()


@pytest.mark.asyncio
async def test_partial_batch_preserves_typed_error_and_prefix() -> None:
    transport = ScriptedTransport()
    client = direct_client(transport)
    session = await open_session(client, transport)
    sending = asyncio.create_task(
        session.send_batch([CanFrame.classic_data(CanId.standard(1), b"a")])
    )
    request = await next_request(transport)
    transport.inbound.put_nowait(
        response(
            request.request_id,
            "can_send_response",
            hal_pb2.CanSendResponse(
                committed_count=0,
                error=hal_pb2.Error(
                    name="can.bus.off",
                    category=hal_pb2.ERROR_CATEGORY_UNAVAILABLE,
                    operation="can.send_batch",
                    retryable=True,
                    debug_message="offline",
                ),
            ),
        )
    )
    with pytest.raises(CanBatchSendError) as caught:
        await sending
    assert caught.value.committed == 0
    assert caught.value.error.category is ErrorCategory.UNAVAILABLE
    await client.close()


@pytest.mark.asyncio
async def test_rich_receive_conversion_and_bus_status() -> None:
    transport = ScriptedTransport()
    client = direct_client(transport)
    session = await open_session(client, transport, mode=CanMode.FD)
    receiving = asyncio.create_task(session.receive(3, MAX_U64))
    request = await next_request(transport)
    fd_frame = hal_pb2.CanFrame(
        id=hal_pb2.CanId(value=4, format=hal_pb2.CAN_ID_FORMAT_EXTENDED),
        kind=hal_pb2.CAN_FRAME_KIND_FD_DATA,
        data=b"fd",
        bitrate_switch=True,
    )
    timestamp = hal_pb2.CanTimestamp(
        timestamp_ns=9,
        source=hal_pb2.CAN_TIMESTAMP_SOURCE_KERNEL,
        clock_domain="kernel",
    )
    transport.inbound.put_nowait(
        response(
            request.request_id,
            "can_receive_response",
            hal_pb2.CanReceiveResponse(
                frames=[
                    hal_pb2.ReceivedCanFrame(
                        frame=hal_pb2.CanFrame(
                            id=hal_pb2.CanId(
                                value=1, format=hal_pb2.CAN_ID_FORMAT_STANDARD
                            ),
                            kind=hal_pb2.CAN_FRAME_KIND_CLASSIC_DATA,
                            data=b"classic",
                        )
                    ),
                    hal_pb2.ReceivedCanFrame(frame=fd_frame, timestamp=timestamp),
                    hal_pb2.ReceivedCanFrame(
                        frame=hal_pb2.CanFrame(
                            kind=hal_pb2.CAN_FRAME_KIND_ERROR,
                            error_classes=[hal_pb2.CAN_ERROR_CLASS_BUS_OFF],
                            data=b"error",
                        )
                    ),
                ]
            ),
        )
    )
    received = await receiving
    assert received[0].frame.data == b"classic"
    assert received[1].frame.data == b"fd"
    assert received[1].timestamp is not None
    assert received[2].frame.classes == (CanErrorClass.BUS_OFF,)

    status_call = asyncio.create_task(session.bus_status())
    status_request = await next_request(transport)
    transport.inbound.put_nowait(
        response(
            status_request.request_id,
            "get_can_bus_status_response",
            hal_pb2.GetCanBusStatusResponse(
                status=hal_pb2.CanBusStatus(
                    state=hal_pb2.CAN_BUS_STATE_WARNING,
                    tx_error_counter=3,
                )
            ),
        )
    )
    status = await status_call
    assert status.state is CanBusState.WARNING
    assert status.tx_error_counter == 3
    await client.close()


@pytest.mark.asyncio
async def test_malformed_peer_terminates_and_fans_out_fresh_errors() -> None:
    transport = ScriptedTransport()
    client = direct_client(transport)
    session = await open_session(client, transport)
    sending = asyncio.create_task(session.send(CanFrame.classic_data(CanId.standard(1), b"x")))
    status = asyncio.create_task(session.bus_status())
    requests = [await next_request(transport), await next_request(transport)]
    first = next(
        item for item in requests if item.WhichOneof("payload") == "can_send_request"
    )
    transport.inbound.put_nowait(
        response(
            first.request_id,
            "can_send_response",
            hal_pb2.CanSendResponse(
                committed_count=1,
                error=hal_pb2.Error(
                    name="can.bus.off",
                    category=hal_pb2.ERROR_CATEGORY_UNAVAILABLE,
                    operation="can.send_batch",
                    retryable=True,
                ),
            ),
        )
    )
    send_error, status_error = await asyncio.gather(
        sending, status, return_exceptions=True
    )
    assert isinstance(send_error, CanBatchSendError)
    assert isinstance(status_error, HalError)
    assert send_error.error.name == "runtime.protocol.invalid_message"
    assert status_error.name == "runtime.protocol.invalid_message"
    assert send_error.error is not status_error
    await client.close()


@pytest.mark.asyncio
async def test_cancelled_receive_tombstone_keeps_following_request_correlated() -> None:
    transport = ScriptedTransport()
    client = direct_client(transport)
    session = await open_session(client, transport)
    receive = asyncio.create_task(session.receive(1, 1))
    request = await next_request(transport)
    receive.cancel()
    with pytest.raises(asyncio.CancelledError):
        await receive
    transport.inbound.put_nowait(
        response(
            request.request_id,
            "can_receive_response",
            hal_pb2.CanReceiveResponse(),
        )
    )
    status = asyncio.create_task(session.bus_status())
    status_request = await next_request(transport)
    transport.inbound.put_nowait(
        response(
            status_request.request_id,
            "get_can_bus_status_response",
            hal_pb2.GetCanBusStatusResponse(
                status=hal_pb2.CanBusStatus(state=hal_pb2.CAN_BUS_STATE_ACTIVE)
            ),
        )
    )
    assert (await status).state is CanBusState.ACTIVE
    await client.close()


@pytest.mark.asyncio
async def test_receive_lag_is_structured_and_connection_remains_usable() -> None:
    transport = ScriptedTransport()
    client = direct_client(transport)
    session = await open_session(client, transport)
    receiving = asyncio.create_task(session.receive(1, 10))
    request = await next_request(transport)
    transport.inbound.put_nowait(
        response(
            request.request_id,
            "error",
            hal_pb2.Error(
                name="can.receive.lagged",
                category=hal_pb2.ERROR_CATEGORY_UNAVAILABLE,
                operation="can.receive",
                retryable=True,
                debug_message="subscriber lagged",
                context={"dropped_count": "3"},
            ),
        )
    )
    with pytest.raises(HalError) as caught:
        await receiving
    assert caught.value.name == "can.receive.lagged"
    assert caught.value.context["dropped_count"] == "3"

    status = asyncio.create_task(session.bus_status())
    status_request = await next_request(transport)
    transport.inbound.put_nowait(
        response(
            status_request.request_id,
            "get_can_bus_status_response",
            hal_pb2.GetCanBusStatusResponse(
                status=hal_pb2.CanBusStatus(state=hal_pb2.CAN_BUS_STATE_ACTIVE)
            ),
        )
    )
    assert (await status).state is CanBusState.ACTIVE
    await client.close()


@pytest.mark.asyncio
async def test_pending_queue_saturation_rejects_without_losing_session() -> None:
    transport = ScriptedTransport()
    client = direct_client(transport, pending_capacity=1)
    session = await open_session(client, transport)
    status = asyncio.create_task(session.bus_status())
    status_request = await next_request(transport)
    with pytest.raises(HalError) as caught:
        await session.receive(1, 1)
    assert caught.value.name == "runtime.queue.full"
    assert transport.sent.empty()
    transport.inbound.put_nowait(
        response(
            status_request.request_id,
            "get_can_bus_status_response",
            hal_pb2.GetCanBusStatusResponse(
                status=hal_pb2.CanBusStatus(state=hal_pb2.CAN_BUS_STATE_ACTIVE)
            ),
        )
    )
    assert (await status).state is CanBusState.ACTIVE
    await client.close()


@pytest.mark.asyncio
async def test_close_is_retryable_until_acknowledged() -> None:
    class RetryCloseClient:
        def __init__(self) -> None:
            self.calls = 0

        async def _can_close(self, _session) -> None:
            self.calls += 1
            if self.calls == 1:
                raise HalError(
                    "runtime.queue.full",
                    ErrorCategory.UNAVAILABLE,
                    "runtime.protocol.write",
                    True,
                    "full",
                )

    client = RetryCloseClient()
    profile = _CanSessionProfile(
        CanMode.CLASSIC,
        True,
        False,
        False,
        False,
        "can:virtual:test",
        "session:can",
    )
    session = CanSession(
        client,  # type: ignore[arg-type]
        "session:can",
        "lease:can",
        1,
        hal_pb2.LEASE_MODE_CONTROL,
        profile,
    )
    with pytest.raises(HalError):
        await session.close()
    assert not session._closed
    await session.close()
    assert session._closed


@pytest.mark.asyncio
async def test_real_virtual_broker_can_ipc(broker) -> None:
    client = await HalClient.connect(broker.endpoint, broker.token)
    resources = await client.enumerate_can()
    assert resources[0].transport is TransportKind.CAN
    session = await client.open_can(
        resources[0].selector(),
        LeaseMode.CONTROL,
        CanOpenConfig(attach=CanLinkExpectation(mode=CanMode.CLASSIC)),
        CanFilterSet(),
    )
    frame = CanFrame.classic_data(CanId.standard(0x123), b"python")
    await session.send(frame)
    received = await session.receive(1, 100)
    assert received[0].frame == frame
    await session.replace_filters(
        CanFilterSet(
            [
                CanFilter(
                    0x123,
                    0x7FF,
                    CanIdFormat.STANDARD,
                    CanFrameClasses.data_only(),
                )
            ]
        )
    )
    assert await session.bus_status() == CanBusStatus(
        CanBusState.ACTIVE, tx_error_counter=0, rx_error_counter=0
    )
    await session.close()
    await client.close()
