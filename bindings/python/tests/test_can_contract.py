from __future__ import annotations

import asyncio
from dataclasses import FrozenInstanceError

import pytest

from robot_hal import (
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
from robot_hal.can import _CanSessionProfile
from robot_hal.client import _decode_can_frame, _decode_descriptor
from robot_hal.proto import hal_pb2


MAX_U16 = (1 << 16) - 1
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


class BlockingWriterTransport(ScriptedTransport):
    def __init__(self) -> None:
        super().__init__()
        self.block = False
        self.release = asyncio.Event()

    async def send(self, payload: bytes | bytearray | memoryview) -> None:
        await super().send(payload)
        if self.block:
            await self.release.wait()


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


def test_classical_dlc_and_data_exact_boundaries_and_types() -> None:
    can_id = CanId.standard(0)
    for data in (b"", bytes(8), bytearray(b"x"), memoryview(b"x")):
        assert isinstance(CanFrame.classic_data(can_id, data).data, bytes)
    for data in (bytes(9), "x", object(), None):
        with pytest.raises(HalError):
            CanFrame.classic_data(can_id, data)

    for dlc in (0, 8):
        assert CanFrame.classic_remote(can_id, dlc).dlc == dlc
    for dlc in (-1, 9, True, 1.0, "1", None):
        with pytest.raises(HalError):
            CanFrame.classic_remote(can_id, dlc)


def test_all_frame_variants_and_exact_fd_lengths() -> None:
    can_id = CanId.extended(1)
    for length in (0, 1, 8, 12, 16, 20, 24, 32, 48, 64):
        CanFrame.fd_data(can_id, bytes(length))
    rejected = (
        9, 10, 11, 13, 14, 15, 17, 18, 19, 21, 22, 23,
        25, 26, 27, 28, 29, 30, 31, 33, 47, 49, 63, 65,
    )
    for length in rejected:
        with pytest.raises(HalError):
            CanFrame.fd_data(can_id, bytes(length))


def test_error_frame_diagnostics_and_classes_exact_boundaries_and_types() -> None:
    one_class = (CanErrorClass.BUS_OFF,)
    all_classes = tuple(CanErrorClass)
    assert len(all_classes) == 10
    for classes in (one_class, all_classes):
        for diagnostics in (b"", bytes(8), bytearray(b"x")):
            frame = CanFrame.error(classes, diagnostics)
            assert isinstance(frame.data, bytes)
            assert frame.classes == classes

    for classes in (
        (),
        all_classes + (CanErrorClass.OTHER,),
        (CanErrorClass.BUS_OFF, object()),
        None,
        1,
    ):
        with pytest.raises(HalError):
            CanFrame.error(classes, b"")
    for diagnostics in (bytes(9), "x", object(), None):
        with pytest.raises(HalError):
            CanFrame.error(one_class, diagnostics)


@pytest.mark.parametrize(
    ("bitrate_switch", "error_state_indicator"),
    [(True, False), (False, True), (True, True)],
)
def test_classical_peer_frames_reject_fd_only_flags(
    bitrate_switch: bool, error_state_indicator: bool
) -> None:
    for kind in (
        hal_pb2.CAN_FRAME_KIND_CLASSIC_DATA,
        hal_pb2.CAN_FRAME_KIND_CLASSIC_REMOTE,
    ):
        value = hal_pb2.CanFrame(
            id=hal_pb2.CanId(value=1, format=hal_pb2.CAN_ID_FORMAT_STANDARD),
            kind=kind,
            data=b"x" if kind == hal_pb2.CAN_FRAME_KIND_CLASSIC_DATA else b"",
            remote_dlc=0,
            bitrate_switch=bitrate_switch,
            error_state_indicator=error_state_indicator,
        )
        with pytest.raises(HalError):
            _decode_can_frame(value)


def test_timestamp_exact_boundaries_and_types() -> None:
    for timestamp_ns in (0, MAX_U64):
        for clock_domain in ("x", "x" * 255):
            timestamp = CanTimestamp(
                timestamp_ns, CanTimestampSource.HARDWARE, clock_domain
            )
            assert timestamp.timestamp_ns == timestamp_ns
    for timestamp_ns in (-1, MAX_U64 + 1, True, 1.0, "0", None):
        with pytest.raises(HalError):
            CanTimestamp(timestamp_ns, CanTimestampSource.KERNEL, "kernel")
    with pytest.raises(HalError):
        CanTimestamp(0, object(), "kernel")
    for clock_domain in ("", "x" * 256, "é", object(), None):
        with pytest.raises(HalError):
            CanTimestamp(0, CanTimestampSource.KERNEL, clock_domain)


def test_bit_timing_exact_boundaries_and_types() -> None:
    CanBitTiming(1, 1, 1)
    CanBitTiming(MAX_U32, 999, MAX_U16)
    CanBitTiming(1, None, None)

    for bitrate in (0, MAX_U32 + 1, -1, True, 1.0, "1", None):
        with pytest.raises(HalError):
            CanBitTiming(bitrate)
    for sample_point in (0, 1000, -1, True, 1.0, "1"):
        with pytest.raises(HalError):
            CanBitTiming(1, sample_point_permill=sample_point)
    for sjw in (0, MAX_U16 + 1, -1, True, 1.0, "1"):
        with pytest.raises(HalError):
            CanBitTiming(1, sjw=sjw)


def test_nominal_data_timing_restart_and_configuration_types() -> None:
    minimum_timing = CanBitTiming(1, 1, 1)
    maximum_timing = CanBitTiming(MAX_U32, 999, MAX_U16)
    for nominal in (minimum_timing, maximum_timing):
        CanConfigureConfig(CanMode.CLASSIC, nominal)
        for data in (minimum_timing, maximum_timing):
            CanConfigureConfig(CanMode.FD, nominal, data)
    for restart_ms in (None, 1, MAX_U32):
        CanConfigureConfig(
            CanMode.FD,
            minimum_timing,
            maximum_timing,
            listen_only=False,
            loopback=True,
            restart_ms=restart_ms,
        )

    with pytest.raises(HalError):
        CanConfigureConfig(CanMode.CLASSIC, minimum_timing, maximum_timing)
    with pytest.raises(HalError):
        CanConfigureConfig(CanMode.FD, minimum_timing)
    for arguments in (
        (object(), minimum_timing, None, False, False),
        (CanMode.CLASSIC, object(), None, False, False),
        (CanMode.FD, minimum_timing, object(), False, False),
        (CanMode.CLASSIC, minimum_timing, None, 0, False),
        (CanMode.CLASSIC, minimum_timing, None, False, 0),
    ):
        with pytest.raises(HalError):
            CanConfigureConfig(*arguments)
    for restart_ms in (0, MAX_U32 + 1, -1, True, 1.0, "1"):
        with pytest.raises(HalError):
            CanConfigureConfig(
                CanMode.CLASSIC, minimum_timing, restart_ms=restart_ms
            )


def test_link_expectation_exact_boundaries_and_types() -> None:
    CanLinkExpectation()
    for mode in (None, CanMode.CLASSIC, CanMode.FD):
        for nominal_bitrate in (None, 1, MAX_U32):
            CanLinkExpectation(mode=mode, nominal_bitrate=nominal_bitrate)
    for data_bitrate in (None, 1, MAX_U32):
        CanLinkExpectation(mode=CanMode.FD, data_bitrate=data_bitrate)
    for listen_only in (None, False, True):
        for loopback in (None, False, True):
            CanLinkExpectation(listen_only=listen_only, loopback=loopback)

    with pytest.raises(HalError):
        CanLinkExpectation(mode=object())
    with pytest.raises(HalError):
        CanLinkExpectation(mode=CanMode.CLASSIC, data_bitrate=1)
    for field in ("nominal_bitrate", "data_bitrate"):
        for value in (0, MAX_U32 + 1, -1, True, 1.0, "1"):
            with pytest.raises(HalError):
                CanLinkExpectation(**{field: value})
    for field in ("listen_only", "loopback"):
        for value in (0, 1, "false", object()):
            with pytest.raises(HalError):
                CanLinkExpectation(**{field: value})


def test_filter_exact_boundaries_and_types() -> None:
    data_only = CanFrameClasses.data_only()
    standard_lower = CanFilter(0, 0, CanIdFormat.STANDARD, data_only)
    for id_format, maximum in (
        (CanIdFormat.STANDARD, 0x7FF),
        (CanIdFormat.EXTENDED, 0x1FFF_FFFF),
        (CanIdFormat.EITHER, 0x1FFF_FFFF),
    ):
        CanFilter(0, 0, id_format, data_only)
        CanFilter(maximum, maximum, id_format, data_only)
        for field in ("id", "mask"):
            for value in (-1, maximum + 1, True, 1.0, "0", None):
                arguments = {"id": 0, "mask": 0}
                arguments[field] = value
                with pytest.raises(HalError):
                    CanFilter(
                        arguments["id"],
                        arguments["mask"],
                        id_format,
                        data_only,
                    )

    for field in ("data", "remote", "error"):
        with pytest.raises(HalError):
            CanFrameClasses(**{field: 1})
    with pytest.raises(HalError):
        CanFilter(0, 0, CanIdFormat.STANDARD, CanFrameClasses())
    with pytest.raises(HalError):
        CanFilter(0, 0, object(), data_only)
    with pytest.raises(HalError):
        CanFilter(0, 0, CanIdFormat.STANDARD, object())

    assert CanFilterSet().matches(
        CanFrame.classic_data(CanId.standard(1), b"x")
    )
    assert len(CanFilterSet([standard_lower] * 64).filters) == 64
    with pytest.raises(HalError):
        CanFilterSet([standard_lower] * 65)
    for filters in (None, 1, [standard_lower, object()]):
        with pytest.raises(HalError):
            CanFilterSet(filters)
    with pytest.raises(HalError):
        standard_lower.matches(object())


def test_bus_status_counter_exact_boundaries_and_types() -> None:
    for tx_counter in (None, 0, MAX_U32):
        for rx_counter in (None, 0, MAX_U32):
            CanBusStatus(CanBusState.ACTIVE, tx_counter, rx_counter)
    with pytest.raises(HalError):
        CanBusStatus(object())
    for field in ("tx_error_counter", "rx_error_counter"):
        for value in (-1, MAX_U32 + 1, True, 1.0, "0"):
            with pytest.raises(HalError):
                CanBusStatus(CanBusState.ACTIVE, **{field: value})


def test_descriptor_endpoint_and_capability_exact_boundaries() -> None:
    maximum_capability = f"n.{'x' * 250}/v1"
    decoded = _decode_descriptor(
        hal_pb2.ResourceDescriptor(
            resource_id="can:virtual:test",
            endpoint="é" * 2048,
            identity_quality=hal_pb2.IDENTITY_QUALITY_STRONG,
            transport=hal_pb2.TRANSPORT_KIND_CAN,
            capabilities=[maximum_capability],
        ),
        expected=TransportKind.CAN,
    )
    assert len(decoded.endpoint.encode("utf-8")) == 4096
    assert decoded.capabilities == (maximum_capability,)

    too_long_endpoint = descriptor(capabilities=["can.classic/v1"])
    too_long_endpoint.endpoint = "é" * 2049
    with pytest.raises(HalError):
        _decode_descriptor(too_long_endpoint, expected=TransportKind.CAN)


@pytest.mark.parametrize(
    "capability",
    [
        "",
        "can/v1",
        "can.classic",
        "can.classic/v0",
        "can.classic/v",
        "can.classic/v18446744073709551616",
        "can..classic/v1",
        "can.classic/v1/extra",
        "é.classic/v1",
        f"n.{'x' * 251}/v1",
    ],
)
def test_descriptor_rejects_invalid_capability_syntax(capability: str) -> None:
    value = descriptor(capabilities=["can.classic/v1"])
    del value.capabilities[:]
    value.capabilities.append(capability)
    with pytest.raises(HalError) as caught:
        _decode_descriptor(value, expected=TransportKind.CAN)
    assert caught.value.name == "runtime.protocol.invalid_message"


@pytest.mark.asyncio
async def test_malformed_descriptor_terminates_and_closes_transport() -> None:
    transport = ScriptedTransport()
    client = direct_client(transport)
    enumerating = asyncio.create_task(client.enumerate_can())
    request = await next_request(transport)
    invalid = descriptor(capabilities=["can.classic/v1"])
    invalid.endpoint = "é" * 2049
    transport.inbound.put_nowait(
        response(
            request.request_id,
            "enumerate_can_response",
            hal_pb2.EnumerateCanResponse(resources=[invalid]),
        )
    )
    with pytest.raises(HalError) as caught:
        await enumerating
    assert caught.value.name == "runtime.protocol.invalid_message"
    await asyncio.sleep(0)
    assert transport.closed


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
@pytest.mark.parametrize(
    ("negotiated", "config", "filters"),
    [
        (
            {"can.classic/v1"},
            CanOpenConfig(attach=CanLinkExpectation(mode=CanMode.FD)),
            CanFilterSet(),
        ),
        (
            {"can.classic/v1"},
            CanOpenConfig(
                configure=CanConfigureConfig(
                    CanMode.CLASSIC, CanBitTiming(500_000)
                )
            ),
            CanFilterSet(),
        ),
        (
            {"can.classic/v1"},
            CanOpenConfig(attach=CanLinkExpectation(mode=CanMode.CLASSIC)),
            CanFilterSet(
                [
                    CanFilter(
                        0,
                        0,
                        CanIdFormat.EITHER,
                        CanFrameClasses(error=True),
                    )
                ]
            ),
        ),
    ],
)
async def test_open_rejects_known_unnegotiated_capabilities_before_enumeration(
    negotiated: set[str],
    config: CanOpenConfig,
    filters: CanFilterSet,
) -> None:
    transport = ScriptedTransport()
    client = direct_client(
        transport,
        capabilities=frozenset({"serial.bytes/v1", *negotiated}),
    )
    opening = asyncio.create_task(
        client.open_can(
            ResourceSelector(
                "can:virtual:test", IdentityQuality.STRONG, TransportKind.CAN
            ),
            LeaseMode.CONTROL,
            config,
            filters,
        )
    )
    with pytest.raises(HalError) as caught:
        await opening
    assert caught.value.name == "runtime.protocol.capability_unsupported"
    assert caught.value.resource_id == "can:virtual:test"
    assert transport.sent.empty()
    await client.close()


@pytest.mark.asyncio
@pytest.mark.parametrize(
    ("negotiated", "advertised", "config", "filters"),
    [
        (
            {"can.classic/v1", "can.fd/v1"},
            {"can.classic/v1"},
            CanOpenConfig(attach=CanLinkExpectation(mode=CanMode.FD)),
            CanFilterSet(),
        ),
        (
            {"can.classic/v1", "can.configure/v1"},
            {"can.classic/v1"},
            CanOpenConfig(
                configure=CanConfigureConfig(
                    CanMode.CLASSIC, CanBitTiming(500_000)
                )
            ),
            CanFilterSet(),
        ),
        (
            {"can.classic/v1", "can.error-frames/v1"},
            {"can.classic/v1"},
            CanOpenConfig(attach=CanLinkExpectation(mode=CanMode.CLASSIC)),
            CanFilterSet(
                [
                    CanFilter(
                        0,
                        0,
                        CanIdFormat.EITHER,
                        CanFrameClasses(error=True),
                    )
                ]
            ),
        ),
    ],
)
async def test_open_intersects_negotiated_and_descriptor_capabilities(
    negotiated: set[str],
    advertised: set[str],
    config: CanOpenConfig,
    filters: CanFilterSet,
) -> None:
    transport = ScriptedTransport()
    client = direct_client(
        transport,
        capabilities=frozenset({"serial.bytes/v1", *negotiated}),
    )
    opening = asyncio.create_task(
        client.open_can(
            ResourceSelector(
                "can:virtual:test", IdentityQuality.STRONG, TransportKind.CAN
            ),
            LeaseMode.CONTROL,
            config,
            filters,
        )
    )
    request = await next_request(transport)
    transport.inbound.put_nowait(
        response(
            request.request_id,
            "enumerate_can_response",
            hal_pb2.EnumerateCanResponse(
                resources=[descriptor(capabilities=sorted(advertised))]
            ),
        )
    )
    with pytest.raises(HalError) as caught:
        await opening
    assert caught.value.name == "runtime.protocol.capability_unsupported"
    assert caught.value.resource_id == "can:virtual:test"
    assert transport.sent.empty()
    await client.close()


@pytest.mark.asyncio
async def test_session_operations_use_effective_capability_profile() -> None:
    transport = ScriptedTransport()
    client = direct_client(
        transport,
        capabilities=frozenset({"serial.bytes/v1", "can.classic/v1"}),
    )
    session = await open_session(client, transport)
    with pytest.raises(CanBatchSendError) as fd:
        await session.send(CanFrame.fd_data(CanId.standard(1), b""))
    assert fd.value.error.name == "runtime.protocol.capability_unsupported"
    with pytest.raises(CanBatchSendError) as error_frame:
        await session.send(CanFrame.error([CanErrorClass.BUS_OFF]))
    assert error_frame.value.error.name == "runtime.protocol.capability_unsupported"
    with pytest.raises(HalError) as filters:
        await session.replace_filters(
            CanFilterSet(
                [
                    CanFilter(
                        0,
                        0,
                        CanIdFormat.EITHER,
                        CanFrameClasses(error=True),
                    )
                ]
            )
        )
    assert filters.value.name == "runtime.protocol.capability_unsupported"
    assert transport.sent.empty()
    await client.close()


@pytest.mark.asyncio
@pytest.mark.parametrize("unadvertised", ["error_frame", "timestamp"])
async def test_receive_rejects_unnegotiated_operation_capabilities(
    unadvertised: str,
) -> None:
    transport = ScriptedTransport()
    client = direct_client(
        transport,
        capabilities=frozenset({"serial.bytes/v1", "can.classic/v1"}),
    )
    session = await open_session(client, transport)
    receiving = asyncio.create_task(session.receive(1, 10))
    request = await next_request(transport)
    if unadvertised == "error_frame":
        frame = hal_pb2.CanFrame(
            kind=hal_pb2.CAN_FRAME_KIND_ERROR,
            error_classes=[hal_pb2.CAN_ERROR_CLASS_BUS_OFF],
        )
        received = hal_pb2.ReceivedCanFrame(frame=frame)
    else:
        frame = hal_pb2.CanFrame(
            id=hal_pb2.CanId(value=1, format=hal_pb2.CAN_ID_FORMAT_STANDARD),
            kind=hal_pb2.CAN_FRAME_KIND_CLASSIC_DATA,
        )
        received = hal_pb2.ReceivedCanFrame(
            frame=frame,
            timestamp=hal_pb2.CanTimestamp(
                timestamp_ns=1,
                source=hal_pb2.CAN_TIMESTAMP_SOURCE_KERNEL,
                clock_domain="kernel",
            ),
        )
    transport.inbound.put_nowait(
        response(
            request.request_id,
            "can_receive_response",
            hal_pb2.CanReceiveResponse(frames=[received]),
        )
    )
    with pytest.raises(HalError) as caught:
        await receiving
    assert caught.value.name == "runtime.protocol.invalid_message"
    assert caught.value.resource_id == "can:virtual:test"
    await asyncio.sleep(0)
    assert transport.closed


@pytest.mark.asyncio
async def test_partial_batch_preserves_typed_error_and_prefix() -> None:
    transport = ScriptedTransport()
    client = direct_client(transport)
    session = await open_session(client, transport)
    sending = asyncio.create_task(
        session.send_batch(
            [
                CanFrame.classic_data(CanId.standard(1), b"a"),
                CanFrame.classic_data(CanId.standard(2), b"b"),
            ]
        )
    )
    request = await next_request(transport)
    transport.inbound.put_nowait(
        response(
            request.request_id,
            "can_send_response",
            hal_pb2.CanSendResponse(
                committed_count=1,
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
    assert caught.value.committed == 1
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
    await asyncio.sleep(0)
    assert transport.closed


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
@pytest.mark.parametrize("malformed", ["classic_brs", "remote_esi", "wrong_kind"])
async def test_cancelled_malformed_responses_terminate_and_close_transport(
    malformed: str,
) -> None:
    transport = ScriptedTransport()
    client = direct_client(transport)
    session = await open_session(client, transport)
    receive = asyncio.create_task(session.receive(1, 1))
    receive_request = await next_request(transport)
    receive.cancel()
    with pytest.raises(asyncio.CancelledError):
        await receive

    status = asyncio.create_task(session.bus_status())
    await next_request(transport)
    if malformed == "wrong_kind":
        payload = response(
            receive_request.request_id,
            "can_send_response",
            hal_pb2.CanSendResponse(committed_count=0),
        )
    else:
        remote = malformed == "remote_esi"
        payload = response(
            receive_request.request_id,
            "can_receive_response",
            hal_pb2.CanReceiveResponse(
                frames=[
                    hal_pb2.ReceivedCanFrame(
                        frame=hal_pb2.CanFrame(
                            id=hal_pb2.CanId(
                                value=1, format=hal_pb2.CAN_ID_FORMAT_STANDARD
                            ),
                            kind=(
                                hal_pb2.CAN_FRAME_KIND_CLASSIC_REMOTE
                                if remote
                                else hal_pb2.CAN_FRAME_KIND_CLASSIC_DATA
                            ),
                            data=b"" if remote else b"x",
                            bitrate_switch=not remote,
                            error_state_indicator=remote,
                        )
                    )
                ]
            ),
        )
    transport.inbound.put_nowait(payload)
    result = await asyncio.gather(status, return_exceptions=True)
    assert isinstance(result[0], HalError)
    assert result[0].name in {
        "runtime.protocol.invalid_message",
        "runtime.protocol.unexpected_response",
    }
    await asyncio.sleep(0)
    assert transport.closed


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
async def test_writer_queue_saturation_is_retryable_and_resource_scoped() -> None:
    transport = BlockingWriterTransport()
    client = direct_client(transport, writer_capacity=1)
    session = await open_session(client, transport)
    transport.block = True

    first = asyncio.create_task(session.bus_status())
    await next_request(transport)
    second = asyncio.create_task(session.bus_status())
    await asyncio.sleep(0)
    assert client._writer_queue.qsize() == 1
    with pytest.raises(HalError) as caught:
        await session.bus_status()
    assert caught.value.name == "runtime.queue.full"
    assert caught.value.retryable
    assert caught.value.resource_id == "can:virtual:test"

    await client.close()
    results = await asyncio.gather(first, second, return_exceptions=True)
    assert all(isinstance(result, HalError) for result in results)
    assert transport.closed


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
