"""Asynchronous, bounded client for the local HAL broker.

The startup token is copied only into mutable client-owned buffers, which are
wiped after the handshake. CPython, protobuf, asyncio, and the operating system
may create transient immutable or kernel copies that Python cannot reliably
zeroize; callers retain ownership of the token object they pass to ``connect``.
Tokens are never stored on ``HalClient`` and no client repr includes them.
"""

from __future__ import annotations

import asyncio
from collections import OrderedDict
from dataclasses import dataclass
from enum import Enum
import os
import sys
from typing import Protocol

from google.protobuf.message import DecodeError, Message

from .errors import (
    _ErrorData,
    _error_data,
    _fresh_error,
    ErrorCategory,
    HalError,
    client_error,
    disconnected_error,
    frame_too_large,
)
from .proto import hal_pb2
from .serial import (
    ControlLines,
    DataBits,
    FlowControl,
    Parity,
    SerialConfig,
    SerialSession,
    StopBits,
)
from .can import (
    MAX_CAN_BATCH_FRAMES,
    MAX_CAN_ERROR_CLASSES,
    CanBatchSendError,
    CanBusState,
    CanBusStatus,
    CanErrorClass,
    CanFilterSet,
    CanFrame,
    CanId,
    CanIdFormat,
    CanMode,
    CanOpenConfig,
    CanSession,
    CanTimestamp,
    CanTimestampSource,
    ClassicDataFrame,
    ClassicRemoteFrame,
    ErrorFrame,
    FdDataFrame,
    ReceivedCanFrame,
    _CanSessionProfile,
)
from .transport_unix import HARD_FRAME_BYTES, UnixFramedTransport


PROTOCOL_MAJOR = 1
PROTOCOL_MINOR_MINIMUM = 0
PROTOCOL_MINOR_MAXIMUM = 1
PROTOCOL_MINOR = PROTOCOL_MINOR_MAXIMUM
SERIAL_CAPABILITY = "serial.bytes/v1"
CAN_CLASSIC_CAPABILITY = "can.classic/v1"
CAN_FD_CAPABILITY = "can.fd/v1"
CAN_CONFIGURE_CAPABILITY = "can.configure/v1"
CAN_ERROR_FRAMES_CAPABILITY = "can.error-frames/v1"
CAN_RX_TIMESTAMP_CAPABILITY = "can.rx-timestamp/v1"
DEFAULT_TRANSFER_BYTES = 64 * 1024
DEFAULT_CAPACITY = 32
DEFAULT_EVENT_CAPACITY = 64
MAX_U32 = (1 << 32) - 1
MAX_U64 = (1 << 64) - 1
ERROR_CODE_MAX_BYTES = 255
ERROR_CONTEXT_MAX_ENTRIES = 16
ERROR_CONTEXT_MAX_KEY_BYTES = 64
ERROR_CONTEXT_MAX_VALUE_BYTES = 1024
ERROR_CONTEXT_MAX_TOTAL_BYTES = 8192


class FramedTransport(Protocol):
    async def send(self, payload: bytes | bytearray | memoryview) -> None: ...
    async def receive(self) -> bytes: ...
    async def close(self) -> None: ...
    def set_frame_limit(self, frame_limit: int) -> None: ...


class IdentityQuality(Enum):
    WEAK = "weak"
    MEDIUM = "medium"
    STRONG = "strong"


class TransportKind(Enum):
    SERIAL = "serial"
    CAN = "can"


class LeaseMode(Enum):
    OBSERVE = "observe"
    CONTROL = "control"
    MAINTENANCE = "maintenance"


@dataclass(frozen=True, slots=True)
class ResourceSelector:
    resource_id: str
    minimum_identity_quality: IdentityQuality
    transport: TransportKind


@dataclass(frozen=True, slots=True)
class ResourceDescriptor:
    resource_id: str
    endpoint: str
    identity_quality: IdentityQuality
    transport: TransportKind
    properties: dict[str, str]
    capabilities: tuple[str, ...]

    def selector(self) -> ResourceSelector:
        return ResourceSelector(
            self.resource_id, self.identity_quality, self.transport
        )


@dataclass(frozen=True, slots=True)
class RuntimeEvent:
    sequence: int
    name: str
    resource_id: str
    session_id: str
    lease_generation: int


class EventSubscription:
    __slots__ = ("_client", "_queue", "_lagged", "_terminal", "__weakref__")

    def __init__(self, client: HalClient, capacity: int) -> None:
        self._client = client
        self._queue: asyncio.Queue[RuntimeEvent | _ErrorData] = asyncio.Queue(capacity)
        self._lagged = 0
        self._terminal: _ErrorData | None = None

    async def receive(self) -> RuntimeEvent:
        if self._lagged:
            skipped = self._lagged
            self._lagged = 0
            raise client_error(
                "runtime.event.lagged",
                ErrorCategory.UNAVAILABLE,
                "runtime.event.receive",
                True,
                f"event subscriber fell behind by {skipped} events",
            )
        if self._terminal is not None and self._queue.empty():
            raise _fresh_error(self._terminal)
        item = await self._queue.get()
        if isinstance(item, _ErrorData):
            raise _fresh_error(item)
        return item

    recv = receive

    def close(self) -> None:
        self._client._remove_subscription(self)

    def _publish(self, item: RuntimeEvent | _ErrorData) -> None:
        if self._terminal is not None:
            return
        if self._queue.full():
            self._queue.get_nowait()
            self._lagged = min(self._lagged + 1, sys.maxsize)
        self._queue.put_nowait(item)

    def _finish(self) -> None:
        self._terminal = _ErrorData(
            "runtime.event.closed",
            ErrorCategory.UNAVAILABLE,
            "runtime.event.receive",
            False,
            "the client event stream is closed",
            None,
            None,
            None,
            (),
        )
        if self._queue.empty():
            self._queue.put_nowait(self._terminal)


@dataclass(slots=True)
class _Pending:
    expected: str
    requested_read: int | None
    future: asyncio.Future[Message]
    input_count: int | None = None
    max_frames: int | None = None
    profile: _CanSessionProfile | None = None
    lease_mode: LeaseMode | None = None
    resource_id: str | None = None


class HalClient:
    """One authenticated owner-scoped broker connection."""

    __slots__ = (
        "_transport",
        "_protocol_minor",
        "_capabilities",
        "_frame_limit",
        "_read_limit",
        "_write_limit",
        "_pending_capacity",
        "_tombstone_capacity",
        "_writer_queue",
        "_pending",
        "_cancelled",
        "_completed",
        "_next_request_id",
        "_terminal",
        "_subscriptions",
        "_event_capacity",
        "_writer_task",
        "_reader_task",
        "_close_lock",
    )

    def __init__(
        self,
        transport: FramedTransport,
        *,
        protocol_minor: int,
        capabilities: frozenset[str] | None = None,
        frame_limit: int,
        read_limit: int,
        write_limit: int,
        pending_capacity: int,
        writer_capacity: int,
        event_capacity: int,
    ) -> None:
        self._transport = transport
        self._protocol_minor = protocol_minor
        self._capabilities = (
            frozenset((SERIAL_CAPABILITY,))
            if capabilities is None
            else frozenset(capabilities)
        )
        self._frame_limit = frame_limit
        self._read_limit = read_limit
        self._write_limit = write_limit
        self._pending_capacity = max(1, pending_capacity)
        self._tombstone_capacity = self._pending_capacity
        self._writer_queue: asyncio.Queue[hal_pb2.Envelope] = asyncio.Queue(
            max(1, writer_capacity)
        )
        self._pending: dict[int, _Pending] = {}
        self._cancelled: OrderedDict[int, _Pending] = OrderedDict()
        self._completed: OrderedDict[int, None] = OrderedDict([(1, None)])
        self._next_request_id = 2
        self._terminal: _ErrorData | None = None
        self._subscriptions: list[EventSubscription] = []
        self._event_capacity = max(1, event_capacity)
        self._writer_task = asyncio.create_task(
            self._writer_loop(), name="seeed-hal-writer"
        )
        self._reader_task = asyncio.create_task(
            self._reader_loop(), name="seeed-hal-reader"
        )
        self._close_lock = asyncio.Lock()

    @classmethod
    async def connect(
        cls,
        endpoint: str | os.PathLike[str],
        startup_token: bytes | bytearray | memoryview,
        *,
        max_frame_bytes: int = HARD_FRAME_BYTES,
        max_read_bytes: int = DEFAULT_TRANSFER_BYTES,
        max_write_bytes: int = DEFAULT_TRANSFER_BYTES,
        pending_capacity: int = DEFAULT_CAPACITY,
        writer_capacity: int = DEFAULT_CAPACITY,
        event_capacity: int = DEFAULT_EVENT_CAPACITY,
    ) -> HalClient:
        _validate_limits(max_frame_bytes, max_read_bytes, max_write_bytes)
        _validate_capacities(pending_capacity, writer_capacity, event_capacity)
        endpoint_text = _validate_endpoint(endpoint)
        try:
            token_view = memoryview(startup_token)
            owned_token = bytearray(token_view)
        except (TypeError, ValueError, BufferError, OverflowError) as error:
            raise _argument_error(
                "runtime.broker.connect", "startup token must be bytes-like"
            ) from error
        if len(owned_token) != 32:
            _wipe(owned_token)
            raise _argument_error(
                "runtime.broker.connect", "startup token must contain exactly 32 bytes"
            )
        transport: FramedTransport
        try:
            if os.name == "nt":
                from .transport_windows import WindowsFramedTransport

                transport = await WindowsFramedTransport.connect(
                    endpoint_text, max_frame_bytes
                )
            else:
                transport = await UnixFramedTransport.connect(
                    endpoint_text, max_frame_bytes
                )
            protocol_minor, capabilities, frame, read, write = await _perform_handshake(
                transport,
                owned_token,
                max_frame_bytes,
                max_read_bytes,
                max_write_bytes,
            )
            transport.set_frame_limit(frame)
            return cls(
                transport,
                protocol_minor=protocol_minor,
                capabilities=capabilities,
                frame_limit=frame,
                read_limit=read,
                write_limit=write,
                pending_capacity=pending_capacity,
                writer_capacity=writer_capacity,
                event_capacity=event_capacity,
            )
        except BaseException:
            _wipe(owned_token)
            if "transport" in locals():
                await transport.close()
            raise
        finally:
            _wipe(owned_token)

    async def __aenter__(self) -> HalClient:
        return self

    @property
    def protocol_minor(self) -> int:
        return self._protocol_minor

    async def __aexit__(self, *_exc: object) -> None:
        await self.close()

    def subscribe(self) -> EventSubscription:
        if len(self._subscriptions) >= self._event_capacity:
            raise client_error(
                "runtime.queue.full",
                ErrorCategory.UNAVAILABLE,
                "runtime.event.subscribe",
                True,
                "client event subscription storage is full",
            )
        subscription = EventSubscription(self, self._event_capacity)
        if self._terminal is not None:
            subscription._finish()
        self._subscriptions.append(subscription)
        return subscription

    async def enumerate_serial(self) -> list[ResourceDescriptor]:
        response = await self._request(
            "enumerate_serial_request",
            hal_pb2.EnumerateSerialRequest(),
            "enumerate_serial_response",
        )
        assert isinstance(response, hal_pb2.EnumerateSerialResponse)
        try:
            return [
                _decode_descriptor(item, expected=TransportKind.SERIAL)
                for item in response.resources
            ]
        except HalError as error:
            self._terminate(error)
            raise

    async def open_serial(
        self, selector: ResourceSelector, config: SerialConfig
    ) -> SerialSession:
        if not isinstance(selector, ResourceSelector):
            raise _argument_error("serial.open", "selector must be ResourceSelector")
        if selector.transport is not TransportKind.SERIAL:
            raise _resource_argument_error(
                "serial.open",
                "serial resource selector transport must be Serial",
                selector.resource_id,
            )
        _validate_serial_config(config)
        selector_proto = hal_pb2.ResourceSelector(
            resource_id=_outbound_identifier(selector.resource_id, "resource.id"),
            minimum_identity_quality=_identity_to_proto(selector.minimum_identity_quality),
            transport=_transport_to_proto(selector.transport),
        )
        config_proto = hal_pb2.SerialConfig(
            baud_rate=config.baud_rate,
            data_bits=_DATA_BITS_TO_PROTO[config.data_bits],
            parity=_PARITY_TO_PROTO[config.parity],
            stop_bits=_STOP_BITS_TO_PROTO[config.stop_bits],
            flow_control=_FLOW_CONTROL_TO_PROTO[config.flow_control],
            read_timeout_ms=config.read_timeout_ms,
        )
        response = await self._request(
            "open_serial_request",
            hal_pb2.OpenSerialRequest(selector=selector_proto, config=config_proto),
            "open_serial_response",
        )
        assert isinstance(response, hal_pb2.OpenSerialResponse)
        try:
            session_id = _valid_identifier(response.session_id, "session.id")
            if not response.HasField("lease"):
                raise _invalid_message("broker returned invalid session metadata")
            lease_id = _valid_identifier(response.lease.lease_id, "lease.id")
            if response.lease.generation == 0 or response.lease.mode not in (
                hal_pb2.LEASE_MODE_OBSERVE,
                hal_pb2.LEASE_MODE_CONTROL,
            ):
                raise _invalid_message("broker returned invalid session metadata")
        except HalError as error:
            self._terminate(error)
            raise
        return SerialSession(
            self,
            session_id,
            lease_id,
            response.lease.generation,
            response.lease.mode,
        )

    async def enumerate_can(self) -> list[ResourceDescriptor]:
        self._require_can_capability("can.enumerate")
        response = await self._request(
            "enumerate_can_request",
            hal_pb2.EnumerateCanRequest(),
            "enumerate_can_response",
        )
        assert isinstance(response, hal_pb2.EnumerateCanResponse)
        return [
            _decode_descriptor(item, expected=TransportKind.CAN)
            for item in response.resources
        ]

    async def open_can(
        self,
        selector: ResourceSelector,
        mode: LeaseMode,
        config: CanOpenConfig,
        filters: CanFilterSet | None = None,
    ) -> CanSession:
        if not isinstance(selector, ResourceSelector):
            raise _argument_error("can.open", "selector must be ResourceSelector")
        if selector.transport is not TransportKind.CAN:
            raise _resource_argument_error(
                "can.open",
                "CAN resource selector transport must be CAN",
                selector.resource_id,
            )
        if not isinstance(mode, LeaseMode):
            raise _resource_argument_error(
                "can.open", "CAN lease mode is invalid", selector.resource_id
            )
        if not isinstance(config, CanOpenConfig):
            raise _resource_argument_error(
                "can.open", "config must be CanOpenConfig", selector.resource_id
            )
        if filters is None:
            filters = CanFilterSet()
        if not isinstance(filters, CanFilterSet):
            raise _resource_argument_error(
                "can.open", "filters must be CanFilterSet", selector.resource_id
            )
        self._require_can_capability("can.open", selector.resource_id)

        # Every open resolves against a fresh authoritative snapshot so stale
        # identities or capabilities cannot weaken fail-closed selection.
        resources = await self.enumerate_can()
        descriptor = _select_can_descriptor(resources, selector)
        profile_mode = _validate_can_open_capabilities(descriptor, config, filters)
        request = hal_pb2.OpenCanRequest(
            selector=_selector_to_proto(selector, "can.open"),
            mode=_LEASE_MODE_TO_PROTO[mode],
            config=_can_open_config_to_proto(config),
            filters=_can_filters_to_proto(filters),
        )
        try:
            response = await self._request(
                "open_can_request",
                request,
                "open_can_response",
                lease_mode=mode,
                resource_id=selector.resource_id,
            )
        except HalError as error:
            attached = _attach_resource(error, selector.resource_id)
            if attached is error:
                raise
            raise attached from error
        assert isinstance(response, hal_pb2.OpenCanResponse)
        session_id, lease_id, generation, lease_mode = _decode_open_can_response(
            response, mode
        )
        capabilities = frozenset(descriptor.capabilities)
        profile = _CanSessionProfile(
            profile_mode,
            CAN_CLASSIC_CAPABILITY in capabilities,
            CAN_FD_CAPABILITY in capabilities,
            CAN_ERROR_FRAMES_CAPABILITY in capabilities,
            CAN_RX_TIMESTAMP_CAPABILITY in capabilities,
            descriptor.resource_id,
            session_id,
        )
        return CanSession(
            self,
            session_id,
            lease_id,
            generation,
            lease_mode,
            profile,
        )

    async def close(self) -> None:
        async with self._close_lock:
            if self._terminal is None:
                self._terminate(
                    client_error(
                        "runtime.client.closed",
                        ErrorCategory.CONFLICT,
                        "runtime.client.close",
                        False,
                        "client is closed",
                    )
                )
            current = asyncio.current_task()
            tasks = [
                task
                for task in (self._writer_task, self._reader_task)
                if task is not current and not task.done()
            ]
            for task in tasks:
                task.cancel()
            if tasks:
                await asyncio.gather(*tasks, return_exceptions=True)
            try:
                await asyncio.wait_for(self._transport.close(), timeout=0.1)
            except TimeoutError:
                pass

    async def _serial_read(self, session: SerialSession, max_bytes: int) -> bytes:
        if (
            not _is_plain_int(max_bytes)
            or max_bytes <= 0
            or max_bytes > self._read_limit
            or max_bytes > MAX_U32
        ):
            raise _argument_error(
                "serial.read", "read size exceeds the negotiated maximum"
            )
        response = await self._request(
            "serial_read_request",
            hal_pb2.SerialReadRequest(
                session_id=session._session_id,
                lease=self._lease(session),
                max_bytes=max_bytes,
            ),
            "serial_read_response",
            requested_read=max_bytes,
        )
        assert isinstance(response, hal_pb2.SerialReadResponse)
        return bytes(response.data)

    async def _serial_write(
        self, session: SerialSession, data: bytes | bytearray | memoryview
    ) -> None:
        try:
            view = memoryview(data)
            size = view.nbytes
        except (TypeError, ValueError, BufferError, OverflowError) as error:
            raise _argument_error("serial.write", "write data must be bytes-like") from error
        if size > self._write_limit:
            raise _argument_error(
                "serial.write", "write size exceeds the negotiated maximum"
            )
        request_without_data = hal_pb2.SerialWriteRequest(
            session_id=session._session_id, lease=self._lease(session)
        )
        if _serial_write_envelope_size(request_without_data, size) > self._frame_limit:
            raise _argument_error(
                "serial.write", "write envelope exceeds the negotiated frame maximum"
            )
        try:
            normalized = view.tobytes()
        except (TypeError, ValueError, BufferError, OverflowError) as error:
            raise _argument_error(
                "serial.write", "write data must be bytes-like"
            ) from error
        request_without_data.data = normalized
        await self._request(
            "serial_write_request",
            request_without_data,
            "serial_write_response",
        )

    async def _serial_flush(self, session: SerialSession) -> None:
        await self._request(
            "serial_flush_request",
            hal_pb2.SerialFlushRequest(
                session_id=session._session_id, lease=self._lease(session)
            ),
            "serial_flush_response",
        )

    async def _serial_set_control_lines(
        self, session: SerialSession, lines: ControlLines
    ) -> None:
        if not isinstance(lines, ControlLines):
            raise _argument_error(
                "serial.set_control_lines", "control lines must be ControlLines"
            )
        if not isinstance(lines.data_terminal_ready, bool) or not isinstance(
            lines.request_to_send, bool
        ):
            raise _argument_error(
                "serial.set_control_lines", "control line values must be bool"
            )
        await self._request(
            "set_serial_control_lines_request",
            hal_pb2.SetSerialControlLinesRequest(
                session_id=session._session_id,
                lease=self._lease(session),
                data_terminal_ready=lines.data_terminal_ready,
                request_to_send=lines.request_to_send,
            ),
            "set_serial_control_lines_response",
        )

    async def _serial_close(self, session: SerialSession) -> None:
        await self._request(
            "close_session_request",
            hal_pb2.CloseSessionRequest(
                session_id=session._session_id, lease=self._lease(session)
            ),
            "close_session_response",
        )

    def _lease(self, session: SerialSession) -> hal_pb2.LeaseToken:
        return hal_pb2.LeaseToken(
            lease_id=session._lease_id,
            generation=session._generation,
            mode=session._mode,
        )

    def _can_lease(self, session: CanSession) -> hal_pb2.LeaseToken:
        return hal_pb2.LeaseToken(
            lease_id=session._lease_id,
            generation=session._generation,
            mode=session._lease_mode,
        )

    async def _can_send_batch(
        self, session: CanSession, frames: object
    ) -> None:
        try:
            copied = tuple(frames)  # type: ignore[arg-type]
        except TypeError as error:
            raise CanBatchSendError(
                _can_local_error(
                    session._profile,
                    "runtime.argument.invalid",
                    ErrorCategory.INVALID_ARGUMENT,
                    "can.send_batch",
                    False,
                    "CAN send frames must be iterable",
                )
            ) from error
        if not 1 <= len(copied) <= MAX_CAN_BATCH_FRAMES:
            raise CanBatchSendError(
                _can_local_error(
                    session._profile,
                    "runtime.argument.invalid",
                    ErrorCategory.INVALID_ARGUMENT,
                    "can.send_batch",
                    False,
                    "CAN send batch must contain 1..=64 frames",
                )
            )
        payload_bytes = 0
        protos: list[hal_pb2.CanFrame] = []
        for frame in copied:
            if not isinstance(
                frame, (ClassicDataFrame, ClassicRemoteFrame, FdDataFrame, ErrorFrame)
            ):
                raise CanBatchSendError(
                    _can_local_error(
                        session._profile,
                        "can.frame.invalid",
                        ErrorCategory.INVALID_ARGUMENT,
                        "can.frame",
                        False,
                        "CAN send batch contains an invalid frame",
                    )
                )
            if not _frame_allowed(frame, session._profile):
                raise CanBatchSendError(
                    _can_local_error(
                        session._profile,
                        "runtime.protocol.capability_unsupported",
                        ErrorCategory.CONFLICT,
                        "can.send_batch",
                        False,
                        "the negotiated broker protocol does not advertise the frame capability",
                    )
                )
            payload_bytes += _frame_data_length(frame)
            protos.append(_can_frame_to_proto(frame))
        if payload_bytes > self._write_limit:
            raise CanBatchSendError(
                _can_local_error(
                    session._profile,
                    "runtime.argument.invalid",
                    ErrorCategory.INVALID_ARGUMENT,
                    "can.send_batch",
                    False,
                    "CAN send payload exceeds the negotiated write maximum",
                )
            )
        try:
            response = await self._request(
                "can_send_request",
                hal_pb2.CanSendRequest(
                    session_id=session._session_id,
                    lease=self._can_lease(session),
                    frames=protos,
                ),
                "can_send_response",
                input_count=len(copied),
                profile=session._profile,
            )
        except HalError as error:
            raise CanBatchSendError(_attach_resource(error, session._profile.resource_id))
        assert isinstance(response, hal_pb2.CanSendResponse)
        if response.HasField("error"):
            raise CanBatchSendError(
                _attach_resource(
                    _decode_error(response.error), session._profile.resource_id
                ),
                response.committed_count,
            )

    async def _can_receive(
        self, session: CanSession, max_frames: int, timeout_ms: int
    ) -> tuple[ReceivedCanFrame, ...]:
        if not _is_plain_int(max_frames) or not 1 <= max_frames <= MAX_CAN_BATCH_FRAMES:
            raise _can_argument_error(
                session._profile,
                "can.receive",
                "CAN receive maximum must be 1..=64 frames",
            )
        if not _is_plain_int(timeout_ms) or not 0 <= timeout_ms <= MAX_U64:
            raise _can_argument_error(
                session._profile,
                "can.receive",
                "CAN receive timeout exceeds the wire range",
            )
        max_data = 64 if session._profile.mode is CanMode.FD else 8
        if max_frames * max_data > self._read_limit:
            raise _can_argument_error(
                session._profile,
                "can.receive",
                "CAN receive payload bound exceeds the negotiated read maximum",
            )
        if _maximum_receive_envelope_size(max_frames, session._profile) > self._frame_limit:
            raise _can_local_error(
                session._profile,
                "runtime.protocol.frame_too_large",
                ErrorCategory.INVALID_ARGUMENT,
                "can.receive",
                False,
                "CAN receive response bound exceeds the negotiated frame maximum",
            )
        try:
            response = await self._request(
                "can_receive_request",
                hal_pb2.CanReceiveRequest(
                    session_id=session._session_id,
                    lease=self._can_lease(session),
                    max_frames=max_frames,
                    timeout_ms=timeout_ms,
                ),
                "can_receive_response",
                requested_read=self._read_limit,
                max_frames=max_frames,
                profile=session._profile,
            )
        except HalError as error:
            attached = _attach_resource(error, session._profile.resource_id)
            if attached is error:
                raise
            raise attached from error
        assert isinstance(response, hal_pb2.CanReceiveResponse)
        return tuple(_received_can_frame_from_proto(item) for item in response.frames)

    async def _can_replace_filters(
        self, session: CanSession, filters: CanFilterSet
    ) -> None:
        if not isinstance(filters, CanFilterSet):
            raise _can_argument_error(
                session._profile,
                "can.replace_filters",
                "filters must be CanFilterSet",
            )
        if (
            any(item.classes.error for item in filters.filters)
            and not session._profile.error_frames
        ):
            raise _can_local_error(
                session._profile,
                "runtime.protocol.capability_unsupported",
                ErrorCategory.CONFLICT,
                "can.replace_filters",
                False,
                "the negotiated broker protocol does not advertise CAN error frames",
            )
        try:
            await self._request(
                "replace_can_filters_request",
                hal_pb2.ReplaceCanFiltersRequest(
                    session_id=session._session_id,
                    lease=self._can_lease(session),
                    filters=_can_filters_to_proto(filters),
                ),
                "replace_can_filters_response",
                profile=session._profile,
            )
        except HalError as error:
            attached = _attach_resource(error, session._profile.resource_id)
            if attached is error:
                raise
            raise attached from error

    async def _can_bus_status(self, session: CanSession) -> CanBusStatus:
        try:
            response = await self._request(
                "get_can_bus_status_request",
                hal_pb2.GetCanBusStatusRequest(
                    session_id=session._session_id, lease=self._can_lease(session)
                ),
                "get_can_bus_status_response",
                profile=session._profile,
            )
        except HalError as error:
            attached = _attach_resource(error, session._profile.resource_id)
            if attached is error:
                raise
            raise attached from error
        assert isinstance(response, hal_pb2.GetCanBusStatusResponse)
        return _can_bus_status_from_proto(response)

    async def _can_close(self, session: CanSession) -> None:
        try:
            await self._request(
                "close_session_request",
                hal_pb2.CloseSessionRequest(
                    session_id=session._session_id, lease=self._can_lease(session)
                ),
                "close_session_response",
                profile=session._profile,
            )
        except HalError as error:
            attached = _attach_resource(error, session._profile.resource_id)
            if attached is error:
                raise
            raise attached from error

    def _require_can_capability(
        self, operation: str, resource_id: str | None = None
    ) -> None:
        if self._protocol_minor >= 1 and (
            CAN_CLASSIC_CAPABILITY in self._capabilities
            or CAN_FD_CAPABILITY in self._capabilities
        ):
            return
        error = client_error(
            "runtime.protocol.capability_unsupported",
            ErrorCategory.CONFLICT,
            operation,
            False,
            "the negotiated broker protocol does not support CAN",
        )
        raise error if resource_id is None else _attach_resource(error, resource_id)

    async def _request(
        self,
        field: str,
        payload: Message,
        expected: str,
        *,
        requested_read: int | None = None,
        input_count: int | None = None,
        max_frames: int | None = None,
        profile: _CanSessionProfile | None = None,
        lease_mode: LeaseMode | None = None,
        resource_id: str | None = None,
    ) -> Message:
        if self._terminal is not None:
            raise _fresh_error(self._terminal)
        if len(self._pending) >= self._pending_capacity:
            raise client_error(
                "runtime.queue.full",
                ErrorCategory.UNAVAILABLE,
                "runtime.client.request",
                True,
                "client pending request storage is full",
            )
        request_id = self._take_request_id()
        request = hal_pb2.Envelope(request_id=request_id)
        getattr(request, field).CopyFrom(payload)
        if request.ByteSize() > self._frame_limit or request.ByteSize() > HARD_FRAME_BYTES:
            raise frame_too_large("outbound envelope exceeds the active frame limit")
        future: asyncio.Future[Message] = asyncio.get_running_loop().create_future()
        self._pending[request_id] = _Pending(
            expected,
            requested_read,
            future,
            input_count,
            max_frames,
            profile,
            lease_mode,
            resource_id,
        )
        try:
            self._writer_queue.put_nowait(request)
        except asyncio.QueueFull as error:
            self._pending.pop(request_id, None)
            raise client_error(
                "runtime.queue.full",
                ErrorCategory.UNAVAILABLE,
                "runtime.protocol.write",
                True,
                "client writer queue is full",
            ) from error
        try:
            return await asyncio.shield(future)
        except asyncio.CancelledError:
            pending = self._pending.pop(request_id, None)
            if pending is not None:
                if len(self._cancelled) >= self._tombstone_capacity:
                    self._terminate(
                        client_error(
                            "runtime.queue.cancelled_full",
                            ErrorCategory.UNAVAILABLE,
                            "runtime.client.cancel",
                            False,
                            "cancelled request tracking is full",
                        )
                    )
                else:
                    self._cancelled[request_id] = pending
            raise

    def _take_request_id(self) -> int:
        request_id = self._next_request_id
        if request_id == 0:
            raise client_error(
                "runtime.protocol.request_id_exhausted",
                ErrorCategory.INTERNAL,
                "runtime.client.request",
                False,
                "request ID space is exhausted",
            )
        self._next_request_id = 0 if request_id == MAX_U64 else request_id + 1
        return request_id

    async def _writer_loop(self) -> None:
        try:
            while True:
                request = await self._writer_queue.get()
                if (
                    request.ByteSize() > self._frame_limit
                    or request.ByteSize() > HARD_FRAME_BYTES
                ):
                    raise frame_too_large("writer rejected an oversized envelope")
                encoded = bytearray(request.SerializeToString())
                try:
                    await self._transport.send(encoded)
                finally:
                    _wipe(encoded)
        except asyncio.CancelledError:
            return
        except HalError as error:
            self._terminate(error)
        except Exception as error:
            self._terminate(disconnected_error("runtime.protocol.write", str(error)))

    async def _reader_loop(self) -> None:
        try:
            while self._terminal is None:
                frame = await self._transport.receive()
                request_id = _preflight_frame(frame, self)
                try:
                    response = hal_pb2.Envelope.FromString(frame)
                except DecodeError as error:
                    raise _invalid_message(str(error)) from error
                if response.request_id != request_id:
                    raise _invalid_message("inconsistent protobuf request ID")
                if request_id == 0:
                    self._handle_event(response)
                    continue
                pending = self._pending.pop(request_id, None)
                if pending is None:
                    cancelled = self._cancelled.pop(request_id, None)
                    if cancelled is not None:
                        self._remember_completed(request_id)
                        self._validate_correlated_response(response, cancelled)
                        continue
                    if request_id in self._completed:
                        raise client_error(
                            "runtime.protocol.duplicate_response",
                            ErrorCategory.CONFLICT,
                            "runtime.protocol.read",
                            False,
                            "broker sent a duplicate response",
                        )
                    raise client_error(
                        "runtime.protocol.unknown_response",
                        ErrorCategory.CONFLICT,
                        "runtime.protocol.read",
                        False,
                        "broker sent an unknown response ID",
                    )
                self._remember_completed(request_id)
                if pending.future.done():
                    continue
                field = response.WhichOneof("payload")
                if field == "error":
                    try:
                        error = _decode_error(response.error)
                        resource_id = (
                            pending.profile.resource_id
                            if pending.profile is not None
                            else pending.resource_id
                        )
                        pending.future.set_exception(
                            error
                            if resource_id is None
                            else _attach_resource(error, resource_id)
                        )
                    except HalError as error:
                        resource_id = (
                            pending.profile.resource_id
                            if pending.profile is not None
                            else pending.resource_id
                        )
                        if resource_id is not None:
                            error = _attach_resource(error, resource_id)
                        pending.future.set_exception(
                            _fresh_error(_error_data(error))
                        )
                        raise error
                elif field == pending.expected:
                    try:
                        _validate_response_payload(getattr(response, field), pending)
                    except HalError as error:
                        pending.future.set_exception(_fresh_error(_error_data(error)))
                        raise
                    pending.future.set_result(getattr(response, field))
                else:
                    error = client_error(
                        "runtime.protocol.unexpected_response",
                        ErrorCategory.INVALID_ARGUMENT,
                        "runtime.protocol.read",
                        False,
                        "response payload does not match its request",
                    )
                    resource_id = (
                        pending.profile.resource_id
                        if pending.profile is not None
                        else pending.resource_id
                    )
                    if resource_id is not None:
                        error = _attach_resource(error, resource_id)
                    pending.future.set_exception(_fresh_error(_error_data(error)))
                    raise error
        except asyncio.CancelledError:
            return
        except HalError as error:
            self._terminate(error)
        except Exception as error:
            self._terminate(disconnected_error("runtime.protocol.read", str(error)))

    def _validate_correlated_response(
        self, response: hal_pb2.Envelope, pending: _Pending
    ) -> None:
        field = response.WhichOneof("payload")
        if field == "error":
            _decode_error(response.error)
            return
        if field != pending.expected:
            error = client_error(
                "runtime.protocol.unexpected_response",
                ErrorCategory.INVALID_ARGUMENT,
                "runtime.protocol.read",
                False,
                "response payload does not match its request",
            )
            if pending.resource_id is not None:
                error = _attach_resource(error, pending.resource_id)
            raise error
        _validate_response_payload(getattr(response, field), pending)

    def _handle_event(self, response: hal_pb2.Envelope) -> None:
        field = response.WhichOneof("payload")
        if field == "runtime_event":
            event = response.runtime_event
            if event.sequence == 0 or event.kind not in (
                hal_pb2.RUNTIME_EVENT_KIND_SESSION_OPENED,
                hal_pb2.RUNTIME_EVENT_KIND_SESSION_CLOSED,
                hal_pb2.RUNTIME_EVENT_KIND_CAN_BUS_ACTIVE,
                hal_pb2.RUNTIME_EVENT_KIND_CAN_BUS_WARNING,
                hal_pb2.RUNTIME_EVENT_KIND_CAN_BUS_PASSIVE,
                hal_pb2.RUNTIME_EVENT_KIND_CAN_BUS_OFF,
                hal_pb2.RUNTIME_EVENT_KIND_CAN_BUS_STOPPED,
                hal_pb2.RUNTIME_EVENT_KIND_CAN_BUS_UNKNOWN,
            ):
                raise _invalid_message("runtime event metadata is invalid")
            public = RuntimeEvent(
                event.sequence,
                event.name,
                event.resource_id,
                event.session_id,
                event.lease_generation,
            )
            for subscription in tuple(self._subscriptions):
                subscription._publish(public)
            return
        if field == "error":
            error = _error_data(_decode_error(response.error))
            for subscription in tuple(self._subscriptions):
                subscription._publish(error)
            return
        raise _invalid_message("request ID zero is reserved for events")

    def _remember_completed(self, request_id: int) -> None:
        self._completed[request_id] = None
        while len(self._completed) > self._tombstone_capacity:
            self._completed.popitem(last=False)

    def _terminate(self, error: HalError) -> None:
        if self._terminal is not None:
            return
        self._terminal = _error_data(error)
        pending = tuple(self._pending.values())
        self._pending.clear()
        for item in pending:
            if not item.future.done():
                item.future.set_exception(_fresh_error(self._terminal))
        for subscription in tuple(self._subscriptions):
            subscription._finish()
        current = asyncio.current_task()
        for task in (self._writer_task, self._reader_task):
            if task is not current and not task.done():
                task.cancel()

    def _remove_subscription(self, subscription: EventSubscription) -> None:
        try:
            self._subscriptions.remove(subscription)
        except ValueError:
            pass
        subscription._finish()


async def _perform_handshake(
    transport: FramedTransport,
    token: bytearray,
    frame_limit: int,
    read_limit: int,
    write_limit: int,
) -> tuple[int, frozenset[str], int, int, int]:
    handshake = hal_pb2.HandshakeRequest(
        startup_token=bytes(token),
        protocol_major=PROTOCOL_MAJOR,
        protocol_minor=PROTOCOL_MINOR,
        required_capabilities=[SERIAL_CAPABILITY],
        max_frame_bytes=frame_limit,
        max_read_bytes=read_limit,
        max_write_bytes=write_limit,
        protocol_minor_minimum=PROTOCOL_MINOR_MINIMUM,
        protocol_minor_maximum=PROTOCOL_MINOR_MAXIMUM,
    )
    request = hal_pb2.Envelope(request_id=1, handshake_request=handshake)
    if request.ByteSize() > frame_limit or request.ByteSize() > HARD_FRAME_BYTES:
        handshake.ClearField("startup_token")
        request.ClearField("handshake_request")
        raise frame_too_large("handshake envelope exceeds the offered frame limit")
    encoded = bytearray(request.SerializeToString())
    handshake.ClearField("startup_token")
    request.ClearField("handshake_request")
    try:
        await transport.send(encoded)
    finally:
        _wipe(encoded)
    frame = await transport.receive()
    if len(frame) > frame_limit or len(frame) > HARD_FRAME_BYTES:
        raise frame_too_large("handshake response exceeds the offered frame limit")
    try:
        response = hal_pb2.Envelope.FromString(frame)
    except DecodeError as error:
        raise _invalid_message(str(error), "runtime.protocol.handshake") from error
    if response.request_id != 1:
        raise client_error(
            "runtime.protocol.unknown_response",
            ErrorCategory.CONFLICT,
            "runtime.protocol.handshake",
            False,
            "handshake response has an unknown request ID",
        )
    field = response.WhichOneof("payload")
    if field == "error":
        raise _decode_error(response.error)
    if field != "handshake_response":
        raise client_error(
            "runtime.protocol.unexpected_response",
            ErrorCategory.INVALID_ARGUMENT,
            "runtime.protocol.handshake",
            False,
            "broker returned a non-handshake response during negotiation",
        )
    accepted = response.handshake_response
    if accepted.protocol_minor_minimum == 0 and accepted.protocol_minor_maximum == 0:
        broker_minimum = accepted.protocol_minor
        broker_maximum = accepted.protocol_minor
    else:
        broker_minimum = accepted.protocol_minor_minimum
        broker_maximum = accepted.protocol_minor_maximum
    if (
        accepted.protocol_major != PROTOCOL_MAJOR
        or broker_minimum > broker_maximum
        or accepted.protocol_minor < broker_minimum
        or accepted.protocol_minor > broker_maximum
        or accepted.protocol_minor < PROTOCOL_MINOR_MINIMUM
        or accepted.protocol_minor > PROTOCOL_MINOR_MAXIMUM
        or accepted.max_frame_bytes == 0
        or accepted.max_frame_bytes > frame_limit
        or accepted.max_read_bytes == 0
        or accepted.max_read_bytes > read_limit
        or accepted.max_write_bytes == 0
        or accepted.max_write_bytes > write_limit
        or SERIAL_CAPABILITY not in accepted.capabilities
    ):
        raise client_error(
            "runtime.protocol.invalid_handshake",
            ErrorCategory.CONFLICT,
            "runtime.protocol.handshake",
            False,
            "broker returned invalid negotiated settings",
        )
    return (
        accepted.protocol_minor,
        frozenset(accepted.capabilities),
        accepted.max_frame_bytes,
        accepted.max_read_bytes,
        accepted.max_write_bytes,
    )


def _preflight_frame(frame: bytes, client: HalClient) -> int:
    if len(frame) > client._frame_limit or len(frame) > HARD_FRAME_BYTES:
        raise frame_too_large("inbound frame exceeds the active frame limit")
    request_id = 0
    for field, wire, value in _fields(frame):
        if field == 1 and wire == 0:
            request_id = int(value)
    requested = client._pending.get(request_id)
    if requested is None:
        requested = client._cancelled.get(request_id)
    requested_read = None if requested is None else requested.requested_read
    for field, wire, value in _fields(frame):
        if field == 25 and wire == 2:
            for nested_field, nested_wire, nested_value in _fields(value):
                if nested_field == 1 and nested_wire == 2:
                    size = len(nested_value)
                    if size > client._read_limit or (
                        requested_read is not None and size > requested_read
                    ):
                        raise frame_too_large(
                            "serial read response exceeds the negotiated or requested byte limit"
                        )
        if (
            field == 57
            and wire == 2
            and requested is not None
            and requested.max_frames is not None
        ):
            count = 0
            payload_bytes = 0
            for nested_field, nested_wire, nested_value in _fields(value):
                if nested_field != 1 or nested_wire != 2:
                    continue
                count += 1
                if count > requested.max_frames:
                    raise frame_too_large(
                        "CAN receive response exceeds the requested frame limit"
                    )
                for received_field, received_wire, received_value in _fields(
                    nested_value
                ):
                    if (
                        received_field == 2
                        and received_wire == 2
                        and requested.profile is not None
                        and not requested.profile.timestamps
                    ):
                        raise _invalid_message(
                            "CAN receive response contains an unadvertised timestamp"
                        )
                    if received_field != 1 or received_wire != 2:
                        continue
                    for frame_field, frame_wire, frame_value in _fields(
                        received_value
                    ):
                        if frame_field == 3 and frame_wire == 2:
                            payload_bytes += len(frame_value)
            if payload_bytes > client._read_limit:
                raise frame_too_large(
                    "CAN receive response exceeds the negotiated read maximum"
                )
    return request_id


def _fields(data: bytes | memoryview):
    view = memoryview(data)
    index = 0
    while index < len(view):
        key, index = _varint(view, index)
        field = key >> 3
        wire = key & 7
        if field == 0:
            raise _invalid_message("protobuf field number zero is invalid")
        if wire == 0:
            value, index = _varint(view, index)
            yield field, wire, value
        elif wire == 1:
            index = _advance(view, index, 8, "truncated fixed64 protobuf field")
            yield field, wire, None
        elif wire == 2:
            size, index = _varint(view, index)
            end = _advance(view, index, size, "truncated length-delimited protobuf field")
            yield field, wire, view[index:end]
            index = end
        elif wire == 3:
            index = _skip_group(view, index, field, 1)
        elif wire == 4:
            raise _invalid_message("unexpected protobuf end-group field")
        elif wire == 5:
            index = _advance(view, index, 4, "truncated fixed32 protobuf field")
            yield field, wire, None
        else:
            raise _invalid_message("unsupported protobuf wire type")


def _skip_group(view: memoryview, index: int, expected: int, depth: int) -> int:
    if depth > 64:
        raise _invalid_message("protobuf group nesting is too deep")
    while index < len(view):
        key, index = _varint(view, index)
        field = key >> 3
        wire = key & 7
        if field == 0:
            raise _invalid_message("protobuf field number zero is invalid")
        if wire == 0:
            _, index = _varint(view, index)
        elif wire == 1:
            index = _advance(view, index, 8, "truncated fixed64 protobuf field")
        elif wire == 2:
            size, index = _varint(view, index)
            index = _advance(view, index, size, "truncated length-delimited protobuf field")
        elif wire == 3:
            index = _skip_group(view, index, field, depth + 1)
        elif wire == 4:
            if field != expected:
                raise _invalid_message(
                    "protobuf end-group field does not match start-group"
                )
            return index
        elif wire == 5:
            index = _advance(view, index, 4, "truncated fixed32 protobuf field")
        else:
            raise _invalid_message("unsupported protobuf wire type")
    raise _invalid_message("unterminated protobuf group")


def _varint(view: memoryview, index: int) -> tuple[int, int]:
    value = 0
    for offset in range(10):
        if index >= len(view):
            raise _invalid_message("truncated protobuf varint")
        byte = view[index]
        index += 1
        if offset == 9 and byte > 1:
            raise _invalid_message("protobuf varint overflows u64")
        value |= (byte & 0x7F) << (offset * 7)
        if byte & 0x80 == 0:
            return value, index
    raise _invalid_message("truncated protobuf varint")


def _advance(view: memoryview, index: int, size: int, message: str) -> int:
    end = index + size
    if end < index or end > len(view):
        raise _invalid_message(message)
    return end


def _decode_error(error: hal_pb2.Error) -> HalError:
    categories = {
        hal_pb2.ERROR_CATEGORY_INVALID_ARGUMENT: ErrorCategory.INVALID_ARGUMENT,
        hal_pb2.ERROR_CATEGORY_NOT_FOUND: ErrorCategory.NOT_FOUND,
        hal_pb2.ERROR_CATEGORY_CONFLICT: ErrorCategory.CONFLICT,
        hal_pb2.ERROR_CATEGORY_UNAVAILABLE: ErrorCategory.UNAVAILABLE,
        hal_pb2.ERROR_CATEGORY_INTERNAL: ErrorCategory.INTERNAL,
    }
    category = categories.get(error.category)
    try:
        name = _valid_identifier(error.name, "error.name")
        operation = _valid_identifier(error.operation, "operation.name")
    except HalError as invalid:
        raise _invalid_message("broker error metadata is invalid") from invalid
    if category is None:
        raise _invalid_message("broker error has an unknown category")
    resource_id, platform_code, vendor_code, context = _decode_error_details(error)
    return HalError(
        name,
        category,
        operation,
        error.retryable,
        error.debug_message,
        resource_id,
        platform_code,
        vendor_code,
        context,
    )


def _decode_error_details(
    error: hal_pb2.Error,
) -> tuple[str | None, str | None, str | None, dict[str, str]]:
    try:
        resource_id = (
            _valid_identifier(error.resource_id, "error.resource_id")
            if error.resource_id
            else None
        )
        platform_code = _optional_error_code(error.platform_code, "platform_code")
        vendor_code = _optional_error_code(error.vendor_code, "vendor_code")
        context = _valid_error_context(error.context)
    except HalError as invalid:
        raise _invalid_message("broker error details are invalid") from invalid
    return resource_id, platform_code, vendor_code, context


def _optional_error_code(value: str, field: str) -> str | None:
    if not value:
        return None
    if len(value) > ERROR_CODE_MAX_BYTES or not value.isascii():
        raise _invalid_message(f"error {field} is invalid")
    return value


def _valid_error_context(entries) -> dict[str, str]:
    if len(entries) > ERROR_CONTEXT_MAX_ENTRIES:
        raise _invalid_message("error context has too many entries")
    context: dict[str, str] = {}
    total_bytes = 0
    for key, value in entries.items():
        if not isinstance(key, str) or not isinstance(value, str):
            raise _invalid_message("error context entry is invalid")
        try:
            key_bytes = key.encode("utf-8")
            value_bytes = value.encode("utf-8")
        except UnicodeEncodeError as invalid:
            raise _invalid_message("error context entry is invalid") from invalid
        if (
            not key
            or len(key_bytes) > ERROR_CONTEXT_MAX_KEY_BYTES
            or not key.isascii()
            or not "a" <= key[0] <= "z"
            or not all(
                character.isascii()
                and (character.isalnum() or character in "_-")
                for character in key[1:]
            )
        ):
            raise _invalid_message("error context key is invalid")
        if len(value_bytes) > ERROR_CONTEXT_MAX_VALUE_BYTES:
            raise _invalid_message("error context value is invalid")
        total_bytes += len(key_bytes) + len(value_bytes)
        if total_bytes > ERROR_CONTEXT_MAX_TOTAL_BYTES:
            raise _invalid_message("error context is too large")
        context[key] = value
    return context


def _decode_descriptor(
    value: hal_pb2.ResourceDescriptor,
    *,
    expected: TransportKind | None = None,
) -> ResourceDescriptor:
    resource_id = _valid_identifier(value.resource_id, "resource.id")
    if not value.endpoint or len(value.endpoint) > 4096:
        raise _invalid_message("broker resource endpoint is invalid")
    qualities = {
        hal_pb2.IDENTITY_QUALITY_WEAK: IdentityQuality.WEAK,
        hal_pb2.IDENTITY_QUALITY_MEDIUM: IdentityQuality.MEDIUM,
        hal_pb2.IDENTITY_QUALITY_STRONG: IdentityQuality.STRONG,
    }
    quality = qualities.get(value.identity_quality)
    transports = {
        hal_pb2.TRANSPORT_KIND_SERIAL: TransportKind.SERIAL,
        hal_pb2.TRANSPORT_KIND_CAN: TransportKind.CAN,
    }
    transport = transports.get(value.transport)
    if quality is None or transport is None or (
        expected is not None and transport is not expected
    ):
        raise _invalid_message("broker resource descriptor enum is invalid")
    if value.capabilities:
        capabilities = tuple(value.capabilities)
    elif transport is TransportKind.SERIAL:
        capabilities = (SERIAL_CAPABILITY,)
    else:
        raise _invalid_message("CAN resource descriptor capabilities are empty")
    return ResourceDescriptor(
        resource_id,
        value.endpoint,
        quality,
        transport,
        dict(value.properties),
        capabilities,
    )


def _valid_identifier(value: str, field: str) -> str:
    if (
        not isinstance(value, str)
        or not value
        or len(value) > 255
        or not value.isascii()
    ):
        raise _invalid_message(f"{field} is invalid")
    return value


def _outbound_identifier(
    value: object, field: str, operation: str = "serial.open"
) -> str:
    if (
        not isinstance(value, str)
        or not value
        or len(value) > 255
        or not value.isascii()
    ):
        raise _argument_error(operation, f"{field} is invalid")
    return value


def _validate_endpoint(endpoint: object) -> str:
    try:
        value = os.fspath(endpoint)
    except Exception as error:
        raise _argument_error(
            "runtime.broker.connect", "broker endpoint is invalid"
        ) from error
    if not isinstance(value, str) or not value or "\x00" in value:
        raise _argument_error("runtime.broker.connect", "broker endpoint is invalid")
    try:
        size = len(value.encode("utf-8"))
    except UnicodeEncodeError as error:
        raise _argument_error(
            "runtime.broker.connect", "broker endpoint is invalid"
        ) from error
    if size > 4096:
        raise _argument_error("runtime.broker.connect", "broker endpoint is invalid")
    return value


def _validate_limits(frame: int, read: int, write: int) -> None:
    if (
        not all(_is_plain_int(item) for item in (frame, read, write))
        or frame <= 0
        or frame > HARD_FRAME_BYTES
        or read <= 0
        or read > MAX_U32
        or write <= 0
        or write > MAX_U32
    ):
        raise _argument_error(
            "runtime.broker.connect", "connection byte limits are invalid"
        )


def _validate_capacities(pending: int, writer: int, event: int) -> None:
    if not all(
        _is_plain_int(item) and 0 < item <= MAX_U32
        for item in (pending, writer, event)
    ):
        raise _argument_error(
            "runtime.broker.connect", "connection capacities are invalid"
        )


def _validate_serial_config(config: SerialConfig) -> None:
    if not isinstance(config, SerialConfig):
        raise _argument_error("serial.open", "config must be SerialConfig")
    if (
        not _is_plain_int(config.baud_rate)
        or config.baud_rate <= 0
        or config.baud_rate > MAX_U32
        or not _is_plain_int(config.read_timeout_ms)
        or config.read_timeout_ms > MAX_U64
        or config.read_timeout_ms <= 0
        or not isinstance(config.data_bits, DataBits)
        or not isinstance(config.parity, Parity)
        or not isinstance(config.stop_bits, StopBits)
        or not isinstance(config.flow_control, FlowControl)
    ):
        raise _argument_error("serial.open", "serial configuration is invalid")


def _identity_to_proto(
    value: IdentityQuality, operation: str = "serial.open"
) -> int:
    try:
        return {
            IdentityQuality.WEAK: hal_pb2.IDENTITY_QUALITY_WEAK,
            IdentityQuality.MEDIUM: hal_pb2.IDENTITY_QUALITY_MEDIUM,
            IdentityQuality.STRONG: hal_pb2.IDENTITY_QUALITY_STRONG,
        }[value]
    except (KeyError, TypeError) as error:
        raise _argument_error(operation, "identity quality is invalid") from error


def _transport_to_proto(value: TransportKind) -> int:
    if value is TransportKind.SERIAL:
        return hal_pb2.TRANSPORT_KIND_SERIAL
    if value is TransportKind.CAN:
        return hal_pb2.TRANSPORT_KIND_CAN
    raise _argument_error("serial.open", "transport kind is invalid")


def _selector_to_proto(selector: ResourceSelector, operation: str) -> hal_pb2.ResourceSelector:
    return hal_pb2.ResourceSelector(
        resource_id=_outbound_identifier(selector.resource_id, "resource.id", operation),
        minimum_identity_quality=_identity_to_proto(
            selector.minimum_identity_quality, operation
        ),
        transport=_transport_to_proto_for_operation(selector.transport, operation),
    )


def _transport_to_proto_for_operation(value: TransportKind, operation: str) -> int:
    if value is TransportKind.SERIAL:
        return hal_pb2.TRANSPORT_KIND_SERIAL
    if value is TransportKind.CAN:
        return hal_pb2.TRANSPORT_KIND_CAN
    raise _argument_error(operation, "transport kind is invalid")


_DATA_BITS_TO_PROTO = {
    DataBits.FIVE: hal_pb2.DATA_BITS_FIVE,
    DataBits.SIX: hal_pb2.DATA_BITS_SIX,
    DataBits.SEVEN: hal_pb2.DATA_BITS_SEVEN,
    DataBits.EIGHT: hal_pb2.DATA_BITS_EIGHT,
}
_PARITY_TO_PROTO = {
    Parity.NONE: hal_pb2.PARITY_NONE,
    Parity.ODD: hal_pb2.PARITY_ODD,
    Parity.EVEN: hal_pb2.PARITY_EVEN,
}
_STOP_BITS_TO_PROTO = {
    StopBits.ONE: hal_pb2.STOP_BITS_ONE,
    StopBits.TWO: hal_pb2.STOP_BITS_TWO,
}
_FLOW_CONTROL_TO_PROTO = {
    FlowControl.NONE: hal_pb2.FLOW_CONTROL_NONE,
    FlowControl.SOFTWARE: hal_pb2.FLOW_CONTROL_SOFTWARE,
    FlowControl.HARDWARE: hal_pb2.FLOW_CONTROL_HARDWARE,
}

_CAN_ID_FORMAT_TO_PROTO = {
    CanIdFormat.STANDARD: hal_pb2.CAN_ID_FORMAT_STANDARD,
    CanIdFormat.EXTENDED: hal_pb2.CAN_ID_FORMAT_EXTENDED,
}
_CAN_ERROR_CLASS_TO_PROTO = {
    CanErrorClass.TX_TIMEOUT: hal_pb2.CAN_ERROR_CLASS_TX_TIMEOUT,
    CanErrorClass.LOST_ARBITRATION: hal_pb2.CAN_ERROR_CLASS_LOST_ARBITRATION,
    CanErrorClass.CONTROLLER: hal_pb2.CAN_ERROR_CLASS_CONTROLLER,
    CanErrorClass.PROTOCOL: hal_pb2.CAN_ERROR_CLASS_PROTOCOL,
    CanErrorClass.TRANSCEIVER: hal_pb2.CAN_ERROR_CLASS_TRANSCEIVER,
    CanErrorClass.NO_ACKNOWLEDGEMENT: hal_pb2.CAN_ERROR_CLASS_NO_ACKNOWLEDGEMENT,
    CanErrorClass.BUS_OFF: hal_pb2.CAN_ERROR_CLASS_BUS_OFF,
    CanErrorClass.BUS_ERROR: hal_pb2.CAN_ERROR_CLASS_BUS_ERROR,
    CanErrorClass.RESTARTED: hal_pb2.CAN_ERROR_CLASS_RESTARTED,
    CanErrorClass.OTHER: hal_pb2.CAN_ERROR_CLASS_OTHER,
}
_CAN_TIMESTAMP_SOURCE_TO_PROTO = {
    CanTimestampSource.HARDWARE: hal_pb2.CAN_TIMESTAMP_SOURCE_HARDWARE,
    CanTimestampSource.KERNEL: hal_pb2.CAN_TIMESTAMP_SOURCE_KERNEL,
    CanTimestampSource.HOST_MONOTONIC: hal_pb2.CAN_TIMESTAMP_SOURCE_HOST_MONOTONIC,
}
_CAN_MODE_TO_PROTO = {
    CanMode.CLASSIC: hal_pb2.CAN_MODE_CLASSIC,
    CanMode.FD: hal_pb2.CAN_MODE_FD,
}
_LEASE_MODE_TO_PROTO = {
    LeaseMode.OBSERVE: hal_pb2.LEASE_MODE_OBSERVE,
    LeaseMode.CONTROL: hal_pb2.LEASE_MODE_CONTROL,
    LeaseMode.MAINTENANCE: hal_pb2.LEASE_MODE_MAINTENANCE,
}
_CAN_BUS_STATE_FROM_PROTO = {
    hal_pb2.CAN_BUS_STATE_ACTIVE: CanBusState.ACTIVE,
    hal_pb2.CAN_BUS_STATE_WARNING: CanBusState.WARNING,
    hal_pb2.CAN_BUS_STATE_PASSIVE: CanBusState.PASSIVE,
    hal_pb2.CAN_BUS_STATE_BUS_OFF: CanBusState.BUS_OFF,
    hal_pb2.CAN_BUS_STATE_STOPPED: CanBusState.STOPPED,
    hal_pb2.CAN_BUS_STATE_UNKNOWN: CanBusState.UNKNOWN,
}


def _can_id_to_proto(value: CanId) -> hal_pb2.CanId:
    return hal_pb2.CanId(value=value.value, format=_CAN_ID_FORMAT_TO_PROTO[value.format])


def _can_frame_to_proto(frame: CanFrame) -> hal_pb2.CanFrame:
    if isinstance(frame, ClassicDataFrame):
        return hal_pb2.CanFrame(
            id=_can_id_to_proto(frame.id),
            kind=hal_pb2.CAN_FRAME_KIND_CLASSIC_DATA,
            data=frame.data,
        )
    if isinstance(frame, ClassicRemoteFrame):
        return hal_pb2.CanFrame(
            id=_can_id_to_proto(frame.id),
            kind=hal_pb2.CAN_FRAME_KIND_CLASSIC_REMOTE,
            remote_dlc=frame.dlc,
        )
    if isinstance(frame, FdDataFrame):
        return hal_pb2.CanFrame(
            id=_can_id_to_proto(frame.id),
            kind=hal_pb2.CAN_FRAME_KIND_FD_DATA,
            data=frame.data,
            bitrate_switch=frame.bitrate_switch,
            error_state_indicator=frame.error_state_indicator,
        )
    assert isinstance(frame, ErrorFrame)
    return hal_pb2.CanFrame(
        kind=hal_pb2.CAN_FRAME_KIND_ERROR,
        data=frame.data,
        error_classes=[_CAN_ERROR_CLASS_TO_PROTO[item] for item in frame.classes],
    )


def _can_filters_to_proto(filters: CanFilterSet) -> hal_pb2.CanFilterSet:
    return hal_pb2.CanFilterSet(
        filters=[
            hal_pb2.CanFilter(
                id=item.id,
                mask=item.mask,
                format={
                    CanIdFormat.STANDARD: hal_pb2.CAN_ID_FORMAT_STANDARD,
                    CanIdFormat.EXTENDED: hal_pb2.CAN_ID_FORMAT_EXTENDED,
                    CanIdFormat.EITHER: hal_pb2.CAN_ID_FORMAT_EITHER,
                }[item.format],
                classes=hal_pb2.CanFrameClasses(
                    data=item.classes.data,
                    remote=item.classes.remote,
                    error=item.classes.error,
                ),
            )
            for item in filters.filters
        ]
    )


def _can_open_config_to_proto(config: CanOpenConfig) -> hal_pb2.CanOpenConfig:
    if config.attach is not None:
        attach = config.attach
        result = hal_pb2.CanLinkExpectation()
        if attach.mode is not None:
            result.mode = _CAN_MODE_TO_PROTO[attach.mode]
        if attach.nominal_bitrate is not None:
            result.nominal_bitrate = attach.nominal_bitrate
        if attach.data_bitrate is not None:
            result.data_bitrate = attach.data_bitrate
        if attach.listen_only is not None:
            result.listen_only = attach.listen_only
        if attach.loopback is not None:
            result.loopback = attach.loopback
        return hal_pb2.CanOpenConfig(attach=result)
    assert config.configure is not None
    value = config.configure
    nominal = hal_pb2.CanBitTiming(bitrate=value.nominal.bitrate)
    if value.nominal.sample_point_permill is not None:
        nominal.sample_point_permill = value.nominal.sample_point_permill
    if value.nominal.sjw is not None:
        nominal.sjw = value.nominal.sjw
    result = hal_pb2.CanConfigureConfig(
        mode=_CAN_MODE_TO_PROTO[value.mode],
        nominal=nominal,
        listen_only=value.listen_only,
        loopback=value.loopback,
    )
    if value.data is not None:
        data = hal_pb2.CanBitTiming(bitrate=value.data.bitrate)
        if value.data.sample_point_permill is not None:
            data.sample_point_permill = value.data.sample_point_permill
        if value.data.sjw is not None:
            data.sjw = value.data.sjw
        result.data.CopyFrom(data)
    if value.restart_ms is not None:
        result.restart_ms = value.restart_ms
    return hal_pb2.CanOpenConfig(configure=result)


def _decode_can_id(value: hal_pb2.CanId) -> CanId:
    formats = {
        hal_pb2.CAN_ID_FORMAT_STANDARD: CanIdFormat.STANDARD,
        hal_pb2.CAN_ID_FORMAT_EXTENDED: CanIdFormat.EXTENDED,
    }
    try:
        return CanId(value.value, formats[value.format])
    except (KeyError, HalError) as error:
        raise _invalid_message("CAN identifier is invalid") from error


def _decode_can_frame(value: hal_pb2.CanFrame) -> CanFrame:
    try:
        kind = value.kind
        if kind == hal_pb2.CAN_FRAME_KIND_CLASSIC_DATA:
            if not value.HasField("id") or value.remote_dlc or value.error_classes:
                raise ValueError
            return ClassicDataFrame(_decode_can_id(value.id), bytes(value.data))
        if kind == hal_pb2.CAN_FRAME_KIND_CLASSIC_REMOTE:
            if not value.HasField("id") or value.data or value.error_classes:
                raise ValueError
            return ClassicRemoteFrame(_decode_can_id(value.id), value.remote_dlc)
        if kind == hal_pb2.CAN_FRAME_KIND_FD_DATA:
            if not value.HasField("id") or value.remote_dlc or value.error_classes:
                raise ValueError
            return FdDataFrame(
                _decode_can_id(value.id),
                bytes(value.data),
                value.bitrate_switch,
                value.error_state_indicator,
            )
        if kind == hal_pb2.CAN_FRAME_KIND_ERROR:
            if (
                value.HasField("id")
                or value.remote_dlc
                or value.bitrate_switch
                or value.error_state_indicator
            ):
                raise ValueError
            classes = tuple(
                {
                    hal_pb2.CAN_ERROR_CLASS_TX_TIMEOUT: CanErrorClass.TX_TIMEOUT,
                    hal_pb2.CAN_ERROR_CLASS_LOST_ARBITRATION: CanErrorClass.LOST_ARBITRATION,
                    hal_pb2.CAN_ERROR_CLASS_CONTROLLER: CanErrorClass.CONTROLLER,
                    hal_pb2.CAN_ERROR_CLASS_PROTOCOL: CanErrorClass.PROTOCOL,
                    hal_pb2.CAN_ERROR_CLASS_TRANSCEIVER: CanErrorClass.TRANSCEIVER,
                    hal_pb2.CAN_ERROR_CLASS_NO_ACKNOWLEDGEMENT: CanErrorClass.NO_ACKNOWLEDGEMENT,
                    hal_pb2.CAN_ERROR_CLASS_BUS_OFF: CanErrorClass.BUS_OFF,
                    hal_pb2.CAN_ERROR_CLASS_BUS_ERROR: CanErrorClass.BUS_ERROR,
                    hal_pb2.CAN_ERROR_CLASS_RESTARTED: CanErrorClass.RESTARTED,
                    hal_pb2.CAN_ERROR_CLASS_OTHER: CanErrorClass.OTHER,
                }[item]
                for item in value.error_classes
            )
            return ErrorFrame(classes, bytes(value.data))
    except (KeyError, TypeError, ValueError, HalError) as error:
        raise _invalid_message("CAN frame metadata is invalid") from error
    raise _invalid_message("CAN frame kind is invalid")


def _decode_timestamp(value: hal_pb2.CanTimestamp) -> CanTimestamp:
    source = {
        hal_pb2.CAN_TIMESTAMP_SOURCE_HARDWARE: CanTimestampSource.HARDWARE,
        hal_pb2.CAN_TIMESTAMP_SOURCE_KERNEL: CanTimestampSource.KERNEL,
        hal_pb2.CAN_TIMESTAMP_SOURCE_HOST_MONOTONIC: CanTimestampSource.HOST_MONOTONIC,
    }.get(value.source)
    try:
        if source is None:
            raise ValueError
        return CanTimestamp(value.timestamp_ns, source, value.clock_domain)
    except (ValueError, HalError) as error:
        raise _invalid_message("CAN timestamp metadata is invalid") from error


def _received_can_frame_from_proto(value: hal_pb2.ReceivedCanFrame) -> ReceivedCanFrame:
    try:
        if not value.HasField("frame"):
            raise ValueError
        timestamp = _decode_timestamp(value.timestamp) if value.HasField("timestamp") else None
        return ReceivedCanFrame(_decode_can_frame(value.frame), timestamp)
    except HalError:
        raise
    except (TypeError, ValueError) as error:
        raise _invalid_message("received CAN frame metadata is invalid") from error


def _can_bus_status_from_proto(value: hal_pb2.GetCanBusStatusResponse) -> CanBusStatus:
    if not value.HasField("status"):
        raise _invalid_message("CAN bus status response is missing status")
    status = value.status
    state = _CAN_BUS_STATE_FROM_PROTO.get(status.state)
    if state is None:
        raise _invalid_message("CAN bus status state is invalid")
    try:
        return CanBusStatus(
            state,
            status.tx_error_counter if status.HasField("tx_error_counter") else None,
            status.rx_error_counter if status.HasField("rx_error_counter") else None,
        )
    except HalError as error:
        raise _invalid_message("CAN bus status counters are invalid") from error


def _decode_open_can_response(
    response: hal_pb2.OpenCanResponse, expected_mode: LeaseMode
) -> tuple[str, str, int, int]:
    try:
        session_id = _valid_identifier(response.session_id, "session.id")
        if not response.HasField("lease"):
            raise ValueError
        lease_id = _valid_identifier(response.lease.lease_id, "lease.id")
        if response.lease.generation == 0 or response.lease.mode != _LEASE_MODE_TO_PROTO[
            expected_mode
        ]:
            raise ValueError
        return session_id, lease_id, response.lease.generation, response.lease.mode
    except (KeyError, ValueError, HalError) as error:
        raise _invalid_message("broker returned invalid CAN session metadata") from error


def _frame_data_length(frame: CanFrame) -> int:
    return len(frame.data) if hasattr(frame, "data") else 0


def _frame_allowed(frame: CanFrame, profile: _CanSessionProfile) -> bool:
    if isinstance(frame, (ClassicDataFrame, ClassicRemoteFrame)):
        return profile.classic_frames
    if isinstance(frame, FdDataFrame):
        return profile.fd_frames and profile.mode is CanMode.FD
    return profile.error_frames


def _validate_response_payload(value: Message, pending: _Pending) -> None:
    try:
        if pending.expected == "enumerate_can_response":
            assert isinstance(value, hal_pb2.EnumerateCanResponse)
            for descriptor in value.resources:
                _decode_descriptor(descriptor, expected=TransportKind.CAN)
            return
        if pending.expected == "open_can_response":
            assert isinstance(value, hal_pb2.OpenCanResponse)
            _decode_open_can_response(value, pending.lease_mode or LeaseMode.OBSERVE)
            return
        if pending.expected == "can_send_response":
            assert isinstance(value, hal_pb2.CanSendResponse)
            committed = value.committed_count
            if pending.input_count is None or committed > pending.input_count:
                raise _invalid_message("CAN committed count exceeds input count")
            if value.HasField("error"):
                if committed >= pending.input_count:
                    raise _invalid_message("CAN committed count is not a strict error prefix")
                _decode_error(value.error)
            elif committed != pending.input_count:
                raise _invalid_message("CAN committed count must equal input count")
            return
        if pending.expected == "can_receive_response":
            assert isinstance(value, hal_pb2.CanReceiveResponse)
            if pending.max_frames is None or len(value.frames) > pending.max_frames:
                raise _invalid_message("CAN receive response exceeds requested maximum")
            if pending.profile is None:
                raise _invalid_message("CAN receive response profile is missing")
            total = 0
            for item in value.frames:
                received = _received_can_frame_from_proto(item)
                total += _frame_data_length(received.frame)
                if not _frame_allowed(received.frame, pending.profile):
                    raise _invalid_message("CAN receive frame is outside the active profile")
                if received.timestamp is not None and not pending.profile.timestamps:
                    raise _invalid_message(
                        "CAN receive response contains an unadvertised timestamp"
                    )
            if pending.requested_read is not None and total > pending.requested_read:
                raise _invalid_message("CAN receive response exceeds negotiated read maximum")
            return
        if pending.expected == "get_can_bus_status_response":
            assert isinstance(value, hal_pb2.GetCanBusStatusResponse)
            _can_bus_status_from_proto(value)
            return
    except HalError as error:
        resource_id = (
            pending.profile.resource_id
            if pending.profile is not None
            else pending.resource_id
        )
        if resource_id is not None:
            raise _attach_resource(error, resource_id) from error
        raise
    except AssertionError as error:
        raise _invalid_message("correlated response payload type is invalid") from error


def _maximum_receive_envelope_size(max_frames: int, profile: _CanSessionProfile) -> int:
    if profile.mode is CanMode.FD:
        frame = hal_pb2.CanFrame(
            id=hal_pb2.CanId(value=0x1FFF_FFFF, format=hal_pb2.CAN_ID_FORMAT_EXTENDED),
            kind=hal_pb2.CAN_FRAME_KIND_FD_DATA,
            data=b"\0" * 64,
            bitrate_switch=True,
            error_state_indicator=True,
        )
    elif profile.error_frames:
        frame = hal_pb2.CanFrame(
            kind=hal_pb2.CAN_FRAME_KIND_ERROR,
            data=b"\0" * 8,
            error_classes=list(range(1, MAX_CAN_ERROR_CLASSES + 1)),
        )
    else:
        frame = hal_pb2.CanFrame(
            id=hal_pb2.CanId(value=0x1FFF_FFFF, format=hal_pb2.CAN_ID_FORMAT_EXTENDED),
            kind=hal_pb2.CAN_FRAME_KIND_CLASSIC_DATA,
            data=b"\0" * 8,
        )
    timestamp = (
        hal_pb2.CanTimestamp(
            timestamp_ns=MAX_U64,
            source=hal_pb2.CAN_TIMESTAMP_SOURCE_HARDWARE,
            clock_domain="x" * 255,
        )
        if profile.timestamps
        else None
    )
    received = hal_pb2.ReceivedCanFrame(frame=frame)
    if timestamp is not None:
        received.timestamp.CopyFrom(timestamp)
    response = hal_pb2.CanReceiveResponse(frames=[received] * max_frames)
    return hal_pb2.Envelope(
        request_id=MAX_U64, can_receive_response=response
    ).ByteSize()


def _select_can_descriptor(
    resources: list[ResourceDescriptor], selector: ResourceSelector
) -> ResourceDescriptor:
    quality_rank = {
        IdentityQuality.WEAK: 1,
        IdentityQuality.MEDIUM: 2,
        IdentityQuality.STRONG: 3,
    }
    try:
        minimum = quality_rank[selector.minimum_identity_quality]
    except (KeyError, TypeError) as error:
        raise _resource_argument_error(
            "can.open", "identity quality is invalid", selector.resource_id
        ) from error
    matches = [
        item
        for item in resources
        if item.resource_id == selector.resource_id
        and item.transport is TransportKind.CAN
        and quality_rank[item.identity_quality] >= minimum
    ]
    if not matches:
        raise HalError(
            "runtime.resource.not_found",
            ErrorCategory.NOT_FOUND,
            "can.open",
            False,
            "CAN resource selector did not match an enumerated descriptor",
            resource_id=selector.resource_id,
        )
    if len(matches) != 1:
        raise HalError(
            "runtime.resource.ambiguous",
            ErrorCategory.CONFLICT,
            "can.open",
            False,
            "CAN resource selector matched more than one enumerated descriptor",
            resource_id=selector.resource_id,
        )
    return matches[0]


def _validate_can_open_capabilities(
    descriptor: ResourceDescriptor,
    config: CanOpenConfig,
    filters: CanFilterSet,
) -> CanMode:
    capabilities = frozenset(descriptor.capabilities)
    if config.attach is not None:
        requested_mode = config.attach.mode
        if requested_mode is None:
            profile_mode = (
                CanMode.FD if CAN_FD_CAPABILITY in capabilities else CanMode.CLASSIC
            )
            mode_supported = bool(
                capabilities & {CAN_CLASSIC_CAPABILITY, CAN_FD_CAPABILITY}
            )
        else:
            profile_mode = requested_mode
            mode_supported = (
                CAN_CLASSIC_CAPABILITY in capabilities
                if requested_mode is CanMode.CLASSIC
                else CAN_FD_CAPABILITY in capabilities
            )
    else:
        assert config.configure is not None
        profile_mode = config.configure.mode
        mode_supported = CAN_CONFIGURE_CAPABILITY in capabilities and (
            CAN_CLASSIC_CAPABILITY in capabilities
            if profile_mode is CanMode.CLASSIC
            else CAN_FD_CAPABILITY in capabilities
        )
    filters_supported = (
        not any(item.classes.error for item in filters.filters)
        or CAN_ERROR_FRAMES_CAPABILITY in capabilities
    )
    if mode_supported and filters_supported:
        return profile_mode
    raise HalError(
        "runtime.protocol.capability_unsupported",
        ErrorCategory.CONFLICT,
        "can.open",
        False,
        "the selected CAN resource does not support the requested configuration or filters",
        resource_id=descriptor.resource_id,
    )


def _attach_resource(error: HalError, resource_id: str) -> HalError:
    if error.resource_id is not None:
        return error
    return HalError(
        error.name,
        error.category,
        error.operation,
        error.retryable,
        error.debug_message,
        resource_id,
        error.platform_code,
        error.vendor_code,
        error.context,
    )


def _can_local_error(
    profile: _CanSessionProfile,
    name: str,
    category: ErrorCategory,
    operation: str,
    retryable: bool,
    message: str,
) -> HalError:
    return HalError(
        name,
        category,
        operation,
        retryable,
        message,
        resource_id=profile.resource_id,
    )


def _can_argument_error(
    profile: _CanSessionProfile, operation: str, message: str
) -> HalError:
    return _can_local_error(
        profile,
        "runtime.argument.invalid",
        ErrorCategory.INVALID_ARGUMENT,
        operation,
        False,
        message,
    )


def _resource_argument_error(operation: str, message: str, resource_id: object) -> HalError:
    error = _argument_error(operation, message)
    if isinstance(resource_id, str) and resource_id:
        return _attach_resource(error, resource_id)
    return error


def _serial_write_envelope_size(
    request_without_data: hal_pb2.SerialWriteRequest, data_size: int
) -> int:
    data_field = 0 if data_size == 0 else 1 + _varint_size(data_size) + data_size
    request_size = request_without_data.ByteSize() + data_field
    return 11 + 2 + _varint_size(request_size) + request_size


def _varint_size(value: int) -> int:
    size = 1
    while value >= 0x80:
        value >>= 7
        size += 1
    return size


def _argument_error(operation: str, message: str) -> HalError:
    return client_error(
        "runtime.argument.invalid",
        ErrorCategory.INVALID_ARGUMENT,
        operation,
        False,
        message,
    )


def _is_plain_int(value: object) -> bool:
    return isinstance(value, int) and not isinstance(value, bool)


def _invalid_message(
    message: str, operation: str = "runtime.protocol.decode"
) -> HalError:
    return client_error(
        "runtime.protocol.invalid_message",
        ErrorCategory.INVALID_ARGUMENT,
        operation,
        False,
        message,
    )


def _wipe(buffer: bytearray) -> None:
    buffer[:] = b"\x00" * len(buffer)
