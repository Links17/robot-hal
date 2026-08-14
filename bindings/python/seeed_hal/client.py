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
from .transport_unix import HARD_FRAME_BYTES, UnixFramedTransport


PROTOCOL_MAJOR = 1
PROTOCOL_MINOR = 0
SERIAL_CAPABILITY = "serial.bytes/v1"
DEFAULT_TRANSFER_BYTES = 64 * 1024
DEFAULT_CAPACITY = 32
DEFAULT_EVENT_CAPACITY = 64
MAX_U32 = (1 << 32) - 1
MAX_U64 = (1 << 64) - 1


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
        )
        if self._queue.empty():
            self._queue.put_nowait(self._terminal)


@dataclass(slots=True)
class _Pending:
    expected: str
    requested_read: int | None
    future: asyncio.Future[Message]


class HalClient:
    """One authenticated owner-scoped broker connection."""

    __slots__ = (
        "_transport",
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
        frame_limit: int,
        read_limit: int,
        write_limit: int,
        pending_capacity: int,
        writer_capacity: int,
        event_capacity: int,
    ) -> None:
        self._transport = transport
        self._frame_limit = frame_limit
        self._read_limit = read_limit
        self._write_limit = write_limit
        self._pending_capacity = max(1, pending_capacity)
        self._tombstone_capacity = self._pending_capacity
        self._writer_queue: asyncio.Queue[hal_pb2.Envelope] = asyncio.Queue(
            max(1, writer_capacity)
        )
        self._pending: dict[int, _Pending] = {}
        self._cancelled: OrderedDict[int, tuple[str, int | None]] = OrderedDict()
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
        except (TypeError, ValueError, BufferError) as error:
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
            frame, read, write = await _perform_handshake(
                transport,
                owned_token,
                max_frame_bytes,
                max_read_bytes,
                max_write_bytes,
            )
            transport.set_frame_limit(frame)
            return cls(
                transport,
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
            return [_decode_descriptor(item) for item in response.resources]
        except HalError as error:
            self._terminate(error)
            raise

    async def open_serial(
        self, selector: ResourceSelector, config: SerialConfig
    ) -> SerialSession:
        if not isinstance(selector, ResourceSelector):
            raise _argument_error("serial.open", "selector must be ResourceSelector")
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
        except TypeError as error:
            raise _argument_error("serial.write", "write data must be bytes-like") from error
        size = view.nbytes
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
        request_without_data.data = view.tobytes()
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

    async def _request(
        self,
        field: str,
        payload: Message,
        expected: str,
        *,
        requested_read: int | None = None,
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
        self._pending[request_id] = _Pending(expected, requested_read, future)
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
                    self._cancelled[request_id] = (
                        pending.expected,
                        pending.requested_read,
                    )
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
                    if self._cancelled.pop(request_id, None) is not None:
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
                        pending.future.set_exception(_decode_error(response.error))
                    except HalError as error:
                        pending.future.set_exception(
                            _fresh_error(_error_data(error))
                        )
                        raise
                elif field == pending.expected:
                    pending.future.set_result(getattr(response, field))
                else:
                    error = client_error(
                        "runtime.protocol.unexpected_response",
                        ErrorCategory.INVALID_ARGUMENT,
                        "runtime.protocol.read",
                        False,
                        "response payload does not match its request",
                    )
                    pending.future.set_exception(_fresh_error(_error_data(error)))
                    raise error
        except asyncio.CancelledError:
            return
        except HalError as error:
            self._terminate(error)
        except Exception as error:
            self._terminate(disconnected_error("runtime.protocol.read", str(error)))

    def _handle_event(self, response: hal_pb2.Envelope) -> None:
        field = response.WhichOneof("payload")
        if field == "runtime_event":
            event = response.runtime_event
            if event.sequence == 0 or event.kind not in (
                hal_pb2.RUNTIME_EVENT_KIND_SESSION_OPENED,
                hal_pb2.RUNTIME_EVENT_KIND_SESSION_CLOSED,
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
) -> tuple[int, int, int]:
    handshake = hal_pb2.HandshakeRequest(
        startup_token=bytes(token),
        protocol_major=PROTOCOL_MAJOR,
        protocol_minor=PROTOCOL_MINOR,
        required_capabilities=[SERIAL_CAPABILITY],
        max_frame_bytes=frame_limit,
        max_read_bytes=read_limit,
        max_write_bytes=write_limit,
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
    if (
        accepted.protocol_major != PROTOCOL_MAJOR
        or accepted.protocol_minor != PROTOCOL_MINOR
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
        accepted.max_frame_bytes,
        accepted.max_read_bytes,
        accepted.max_write_bytes,
    )


def _preflight_frame(frame: bytes, client: HalClient) -> int:
    request_id = 0
    for field, wire, value in _fields(frame):
        if field == 1 and wire == 0:
            request_id = int(value)
    requested = client._pending.get(request_id)
    if requested is None:
        cancelled = client._cancelled.get(request_id)
        requested_read = None if cancelled is None else cancelled[1]
    else:
        requested_read = requested.requested_read
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
    return HalError(name, category, operation, error.retryable, error.debug_message)


def _decode_descriptor(value: hal_pb2.ResourceDescriptor) -> ResourceDescriptor:
    resource_id = _valid_identifier(value.resource_id, "resource.id")
    if not value.endpoint or len(value.endpoint) > 4096:
        raise _invalid_message("broker resource endpoint is invalid")
    qualities = {
        hal_pb2.IDENTITY_QUALITY_WEAK: IdentityQuality.WEAK,
        hal_pb2.IDENTITY_QUALITY_MEDIUM: IdentityQuality.MEDIUM,
        hal_pb2.IDENTITY_QUALITY_STRONG: IdentityQuality.STRONG,
    }
    quality = qualities.get(value.identity_quality)
    if quality is None or value.transport != hal_pb2.TRANSPORT_KIND_SERIAL:
        raise _invalid_message("broker resource descriptor enum is invalid")
    return ResourceDescriptor(
        resource_id,
        value.endpoint,
        quality,
        TransportKind.SERIAL,
        dict(value.properties),
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


def _outbound_identifier(value: object, field: str) -> str:
    if (
        not isinstance(value, str)
        or not value
        or len(value) > 255
        or not value.isascii()
    ):
        raise _argument_error("serial.open", f"{field} is invalid")
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
        or config.read_timeout_ms < 0
        or not isinstance(config.data_bits, DataBits)
        or not isinstance(config.parity, Parity)
        or not isinstance(config.stop_bits, StopBits)
        or not isinstance(config.flow_control, FlowControl)
    ):
        raise _argument_error("serial.open", "serial configuration is invalid")


def _identity_to_proto(value: IdentityQuality) -> int:
    try:
        return {
            IdentityQuality.WEAK: hal_pb2.IDENTITY_QUALITY_WEAK,
            IdentityQuality.MEDIUM: hal_pb2.IDENTITY_QUALITY_MEDIUM,
            IdentityQuality.STRONG: hal_pb2.IDENTITY_QUALITY_STRONG,
        }[value]
    except (KeyError, TypeError) as error:
        raise _argument_error("serial.open", "identity quality is invalid") from error


def _transport_to_proto(value: TransportKind) -> int:
    if value is not TransportKind.SERIAL:
        raise _argument_error("serial.open", "transport kind is invalid")
    return hal_pb2.TRANSPORT_KIND_SERIAL


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
