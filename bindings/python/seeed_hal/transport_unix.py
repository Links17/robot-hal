"""Bounded big-endian length-delimited asyncio Unix transport."""

from __future__ import annotations

import asyncio
import struct

from .errors import HalError, disconnected_error, frame_too_large


HARD_FRAME_BYTES = 1024 * 1024


class UnixFramedTransport:
    __slots__ = ("_reader", "_writer", "_frame_limit", "_closed")

    def __init__(
        self,
        reader: asyncio.StreamReader,
        writer: asyncio.StreamWriter,
        frame_limit: int,
    ) -> None:
        self._reader = reader
        self._writer = writer
        self._frame_limit = min(frame_limit, HARD_FRAME_BYTES)
        self._closed = False

    @classmethod
    async def connect(cls, endpoint: str, frame_limit: int = HARD_FRAME_BYTES) -> UnixFramedTransport:
        try:
            reader, writer = await asyncio.open_unix_connection(endpoint)
        except (OSError, ValueError) as error:
            raise disconnected_error("runtime.broker.connect", str(error)) from error
        return cls(reader, writer, frame_limit)

    def set_frame_limit(self, frame_limit: int) -> None:
        self._frame_limit = min(frame_limit, HARD_FRAME_BYTES)

    async def send(self, payload: bytes | bytearray | memoryview) -> None:
        size = len(payload)
        if size > self._frame_limit or size > HARD_FRAME_BYTES:
            raise frame_too_large("outbound frame exceeds the active frame limit")
        try:
            self._writer.write(struct.pack(">I", size))
            self._writer.write(payload)
            await self._writer.drain()
        except (ConnectionError, OSError, RuntimeError) as error:
            raise disconnected_error("runtime.protocol.write", str(error)) from error

    async def receive(self) -> bytes:
        try:
            prefix = await self._reader.readexactly(4)
            size = struct.unpack(">I", prefix)[0]
            if size > self._frame_limit or size > HARD_FRAME_BYTES:
                raise frame_too_large("inbound frame length prefix exceeds the active limit")
            return await self._reader.readexactly(size)
        except HalError:
            raise
        except (asyncio.IncompleteReadError, ConnectionError, OSError, RuntimeError) as error:
            raise disconnected_error("runtime.protocol.read", str(error)) from error

    async def close(self) -> None:
        if self._closed:
            return
        self._closed = True
        self._writer.close()
        try:
            await self._writer.wait_closed()
        except (ConnectionError, OSError, RuntimeError):
            pass
