"""pywin32 Named Pipe transport with all blocking calls delegated to threads."""

from __future__ import annotations

import asyncio
import struct
from typing import Any

from .errors import HalError, disconnected_error, frame_too_large
from .transport_unix import HARD_FRAME_BYTES


def _load_pywin32() -> tuple[Any, Any]:
    try:
        import win32file
        import win32pipe
    except ImportError as error:
        raise disconnected_error(
            "runtime.broker.connect", "pywin32 is required for Windows Named Pipes"
        ) from error
    return win32file, win32pipe


class WindowsFramedTransport:
    __slots__ = ("_handle", "_win32file", "_frame_limit", "_closed")

    def __init__(self, handle: Any, win32file: Any, frame_limit: int) -> None:
        self._handle = handle
        self._win32file = win32file
        self._frame_limit = min(frame_limit, HARD_FRAME_BYTES)
        self._closed = False

    @classmethod
    async def connect(
        cls, endpoint: str, frame_limit: int = HARD_FRAME_BYTES
    ) -> WindowsFramedTransport:
        if not endpoint.lower().startswith("\\\\.\\pipe\\"):
            raise disconnected_error(
                "runtime.broker.connect", "only local Named Pipe endpoints are accepted"
            )
        win32file, win32pipe = _load_pywin32()

        def blocking_connect() -> Any:
            handle = win32file.CreateFile(
                endpoint,
                win32file.GENERIC_READ | win32file.GENERIC_WRITE,
                0,
                None,
                win32file.OPEN_EXISTING,
                0,
                None,
            )
            win32pipe.SetNamedPipeHandleState(
                handle, win32pipe.PIPE_READMODE_BYTE, None, None
            )
            return handle

        try:
            handle = await asyncio.to_thread(blocking_connect)
        except OSError as error:
            raise disconnected_error("runtime.broker.connect", str(error)) from error
        return cls(handle, win32file, frame_limit)

    def set_frame_limit(self, frame_limit: int) -> None:
        self._frame_limit = min(frame_limit, HARD_FRAME_BYTES)

    def _read_exact(self, size: int) -> bytes:
        result = bytearray()
        while len(result) < size:
            _status, chunk = self._win32file.ReadFile(
                self._handle, size - len(result)
            )
            if not chunk:
                raise ConnectionError("Named Pipe closed during read")
            result.extend(chunk)
        return bytes(result)

    async def receive(self) -> bytes:
        try:
            prefix = await asyncio.to_thread(self._read_exact, 4)
            size = struct.unpack(">I", prefix)[0]
            if size > self._frame_limit or size > HARD_FRAME_BYTES:
                raise frame_too_large("inbound frame length prefix exceeds the active limit")
            return await asyncio.to_thread(self._read_exact, size)
        except HalError:
            raise
        except (ConnectionError, OSError) as error:
            raise disconnected_error("runtime.protocol.read", str(error)) from error

    def _write_all(self, payload: bytes) -> None:
        _status, written = self._win32file.WriteFile(self._handle, payload)
        if written != len(payload):
            raise ConnectionError("Named Pipe write was incomplete")

    async def send(self, payload: bytes | bytearray | memoryview) -> None:
        size = len(payload)
        if size > self._frame_limit or size > HARD_FRAME_BYTES:
            raise frame_too_large("outbound frame exceeds the active frame limit")
        framed = struct.pack(">I", size) + bytes(payload)
        try:
            await asyncio.to_thread(self._write_all, framed)
        except (ConnectionError, OSError) as error:
            raise disconnected_error("runtime.protocol.write", str(error)) from error

    async def close(self) -> None:
        if self._closed:
            return
        self._closed = True
        try:
            await asyncio.to_thread(self._win32file.CloseHandle, self._handle)
        except OSError:
            pass
