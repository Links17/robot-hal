"""Owned pywin32 Named Pipe transport.

Connection setup uses one tracked ``asyncio.to_thread`` call. After setup, a
single bounded actor thread exclusively owns every read, write, and close on
the pipe handle. The pipe is placed in nonblocking byte mode so the actor can
multiplex one pending read with writes and observe terminal cancellation.
"""

from __future__ import annotations

import asyncio
from collections import deque
from concurrent.futures import Future
from dataclasses import dataclass, field
import queue
import struct
import threading
import time
from typing import Any, Literal

from .errors import HalError, disconnected_error, frame_too_large
from .transport_unix import HARD_FRAME_BYTES


_COMMAND_CAPACITY = 4
_POLL_SECONDS = 0.001
_WOULD_BLOCK_CODES = frozenset((232,))


def _load_pywin32() -> tuple[Any, Any, type[BaseException]]:
    try:
        import pywintypes
        import win32file
        import win32pipe
    except ImportError as error:
        raise disconnected_error(
            "runtime.broker.connect", "pywin32 is required for Windows Named Pipes"
        ) from error
    return win32file, win32pipe, pywintypes.error


async def _cleanup_cancelled_connect(
    worker: asyncio.Task[Any], win32file: Any
) -> None:
    try:
        handle = await worker
    except BaseException:
        return
    try:
        await asyncio.to_thread(win32file.CloseHandle, handle)
    except BaseException:
        return


async def _await_cleanup_despite_cancellation(cleanup: asyncio.Task[None]) -> None:
    while True:
        try:
            await asyncio.shield(cleanup)
            return
        except asyncio.CancelledError:
            if cleanup.done():
                return


@dataclass(slots=True)
class _Command:
    kind: Literal["read", "write"]
    future: Future[bytes | None]
    payload: bytes = b""
    frame_limit: int = HARD_FRAME_BYTES
    offset: int = 0
    buffer: bytearray = field(default_factory=bytearray)
    expected: int = 4
    reading_body: bool = False


class _PipeActor:
    def __init__(
        self,
        handle: Any,
        win32file: Any,
        native_error: type[BaseException],
    ) -> None:
        self._handle = handle
        self._win32file = win32file
        self._native_errors = (OSError, native_error)
        self._commands: queue.Queue[_Command] = queue.Queue(_COMMAND_CAPACITY)
        self._slots = threading.BoundedSemaphore(_COMMAND_CAPACITY)
        self._shutdown = threading.Event()
        self._terminated: Future[None] = Future()
        self._thread = threading.Thread(
            target=self._run,
            name=f"robot-hal-pipe-io-{id(self):x}",
            daemon=True,
        )
        self._thread.start()

    def submit_read(self, frame_limit: int) -> Future[bytes | None]:
        return self._submit(_Command("read", Future(), frame_limit=frame_limit))

    def submit_write(self, payload: bytes) -> Future[bytes | None]:
        return self._submit(_Command("write", Future(), payload=payload))

    def _submit(self, command: _Command) -> Future[bytes | None]:
        if self._shutdown.is_set() or not self._slots.acquire(blocking=False):
            raise ConnectionError("Named Pipe actor queue is closed or full")
        try:
            self._commands.put_nowait(command)
        except BaseException:
            self._slots.release()
            raise
        return command.future

    def request_close(self) -> None:
        self._shutdown.set()

    async def wait_closed(self) -> None:
        await asyncio.wrap_future(self._terminated)
        self._thread.join(timeout=0)

    def _run(self) -> None:
        reads: deque[_Command] = deque()
        writes: deque[_Command] = deque()
        terminal_error = ConnectionError("Named Pipe transport is closed")
        try:
            while not self._shutdown.is_set():
                self._drain_commands(reads, writes)
                progressed = False
                if writes:
                    progressed = self._advance_write(writes) or progressed
                if reads:
                    progressed = self._advance_read(reads) or progressed
                if not progressed:
                    time.sleep(_POLL_SECONDS)
        except BaseException as error:
            terminal_error = error
            self._shutdown.set()
        finally:
            self._fail_all(reads, terminal_error)
            self._fail_all(writes, terminal_error)
            while True:
                try:
                    command = self._commands.get_nowait()
                except queue.Empty:
                    break
                self._finish(command, error=terminal_error)
            try:
                self._win32file.CloseHandle(self._handle)
            except BaseException:
                pass
            if not self._terminated.done():
                self._terminated.set_result(None)

    def _drain_commands(
        self, reads: deque[_Command], writes: deque[_Command]
    ) -> None:
        while True:
            try:
                command = self._commands.get_nowait()
            except queue.Empty:
                return
            (reads if command.kind == "read" else writes).append(command)

    def _advance_write(self, writes: deque[_Command]) -> bool:
        command = writes[0]
        try:
            _status, written = self._win32file.WriteFile(
                self._handle, command.payload[command.offset :]
            )
        except self._native_errors as error:
            if _would_block(error):
                return False
            writes.popleft()
            self._finish(command, error=error)
            raise
        if not isinstance(written, int) or written < 0:
            error = ConnectionError("Named Pipe write made no progress")
            writes.popleft()
            self._finish(command, error=error)
            raise error
        if written == 0:
            return False
        command.offset += written
        if command.offset >= len(command.payload):
            writes.popleft()
            self._finish(command, value=None)
        return True

    def _advance_read(self, reads: deque[_Command]) -> bool:
        command = reads[0]
        remaining = command.expected - len(command.buffer)
        try:
            _status, chunk = self._win32file.ReadFile(self._handle, remaining)
        except self._native_errors as error:
            if _would_block(error):
                return False
            reads.popleft()
            self._finish(command, error=error)
            raise
        if not chunk:
            error = ConnectionError("Named Pipe closed during read")
            reads.popleft()
            self._finish(command, error=error)
            raise error
        command.buffer.extend(chunk)
        if len(command.buffer) < command.expected:
            return True
        if not command.reading_body:
            size = struct.unpack(">I", command.buffer)[0]
            if size > command.frame_limit or size > HARD_FRAME_BYTES:
                error = frame_too_large(
                    "inbound frame length prefix exceeds the active limit"
                )
                reads.popleft()
                self._finish(command, error=error)
                raise error
            command.buffer.clear()
            command.expected = size
            command.reading_body = True
            if size != 0:
                return True
        value = bytes(command.buffer)
        reads.popleft()
        self._finish(command, value=value)
        return True

    def _finish(
        self,
        command: _Command,
        *,
        value: bytes | None = None,
        error: BaseException | None = None,
    ) -> None:
        if not command.future.done():
            if error is None:
                command.future.set_result(value)
            else:
                command.future.set_exception(error)
        self._slots.release()

    def _fail_all(self, commands: deque[_Command], error: BaseException) -> None:
        while commands:
            self._finish(commands.popleft(), error=error)


def _would_block(error: BaseException) -> bool:
    code = getattr(error, "winerror", None)
    if code is None and error.args and isinstance(error.args[0], int):
        code = error.args[0]
    return code in _WOULD_BLOCK_CODES


class WindowsFramedTransport:
    __slots__ = ("_actor", "_frame_limit", "_closed", "_transport_errors")

    def __init__(
        self,
        handle: Any,
        win32file: Any,
        native_error: type[BaseException],
        frame_limit: int,
    ) -> None:
        self._actor = _PipeActor(handle, win32file, native_error)
        self._frame_limit = min(frame_limit, HARD_FRAME_BYTES)
        self._closed = False
        self._transport_errors = (ConnectionError, OSError, native_error)

    @classmethod
    async def connect(
        cls, endpoint: str, frame_limit: int = HARD_FRAME_BYTES
    ) -> WindowsFramedTransport:
        if not endpoint.lower().startswith("\\\\.\\pipe\\"):
            raise disconnected_error(
                "runtime.broker.connect", "only local Named Pipe endpoints are accepted"
            )
        win32file, win32pipe, native_error = _load_pywin32()
        native_errors = (OSError, native_error)

        def blocking_connect() -> Any:
            handle = None
            try:
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
                    handle,
                    win32pipe.PIPE_READMODE_BYTE | win32pipe.PIPE_NOWAIT,
                    None,
                    None,
                )
                return handle
            except BaseException:
                if handle is not None:
                    win32file.CloseHandle(handle)
                raise

        worker = asyncio.create_task(asyncio.to_thread(blocking_connect))
        try:
            handle = await asyncio.shield(worker)
        except asyncio.CancelledError as cancelled:
            cleanup = asyncio.create_task(
                _cleanup_cancelled_connect(worker, win32file)
            )
            await _await_cleanup_despite_cancellation(cleanup)
            raise cancelled
        except native_errors as error:
            raise disconnected_error("runtime.broker.connect", str(error)) from error
        return cls(handle, win32file, native_error, frame_limit)

    def set_frame_limit(self, frame_limit: int) -> None:
        self._frame_limit = min(frame_limit, HARD_FRAME_BYTES)

    async def receive(self) -> bytes:
        try:
            future = self._actor.submit_read(self._frame_limit)
            result = await asyncio.wrap_future(future)
            assert isinstance(result, bytes)
            return result
        except asyncio.CancelledError as cancelled:
            await self._close_after_operation_cancellation()
            raise cancelled
        except HalError:
            await self._finish_close()
            raise
        except self._transport_errors as error:
            await self._finish_close()
            raise disconnected_error("runtime.protocol.read", str(error)) from error

    async def send(self, payload: bytes | bytearray | memoryview) -> None:
        view = memoryview(payload)
        if not view.c_contiguous:
            view = memoryview(view.tobytes())
        elif view.format != "B" or view.ndim != 1:
            view = view.cast("B")
        size = view.nbytes
        if size > self._frame_limit or size > HARD_FRAME_BYTES:
            raise frame_too_large("outbound frame exceeds the active frame limit")
        framed = struct.pack(">I", size) + view.tobytes()
        try:
            future = self._actor.submit_write(framed)
            await asyncio.wrap_future(future)
        except asyncio.CancelledError as cancelled:
            await self._close_after_operation_cancellation()
            raise cancelled
        except self._transport_errors as error:
            await self._finish_close()
            raise disconnected_error("runtime.protocol.write", str(error)) from error

    async def close(self) -> None:
        cleanup = asyncio.create_task(self._finish_close())
        try:
            await asyncio.shield(cleanup)
        except asyncio.CancelledError as cancelled:
            await _await_cleanup_despite_cancellation(cleanup)
            raise cancelled

    async def _close_after_operation_cancellation(self) -> None:
        cleanup = asyncio.create_task(self._finish_close())
        await _await_cleanup_despite_cancellation(cleanup)

    async def _finish_close(self) -> None:
        self._closed = True
        self._actor.request_close()
        await self._actor.wait_closed()
