"""Public Serial value types and broker-owned session handle."""

from __future__ import annotations

from dataclasses import dataclass
from enum import Enum
from typing import TYPE_CHECKING

from .errors import ErrorCategory, client_error

if TYPE_CHECKING:
    from .client import HalClient


class DataBits(Enum):
    FIVE = 5
    SIX = 6
    SEVEN = 7
    EIGHT = 8


class Parity(Enum):
    NONE = "none"
    ODD = "odd"
    EVEN = "even"


class StopBits(Enum):
    ONE = 1
    TWO = 2


class FlowControl(Enum):
    NONE = "none"
    SOFTWARE = "software"
    HARDWARE = "hardware"


@dataclass(frozen=True, slots=True)
class SerialConfig:
    baud_rate: int = 115_200
    data_bits: DataBits = DataBits.EIGHT
    parity: Parity = Parity.NONE
    stop_bits: StopBits = StopBits.ONE
    flow_control: FlowControl = FlowControl.NONE
    read_timeout_ms: int = 100


@dataclass(frozen=True, slots=True)
class ControlLines:
    data_terminal_ready: bool = False
    request_to_send: bool = False


class SerialSession:
    """Opaque broker session; the broker retains the platform handle."""

    __slots__ = ("_client", "_session_id", "_lease_id", "_generation", "_mode", "_closed")

    def __init__(
        self,
        client: HalClient,
        session_id: str,
        lease_id: str,
        generation: int,
        mode: int,
    ) -> None:
        self._client = client
        self._session_id = session_id
        self._lease_id = lease_id
        self._generation = generation
        self._mode = mode
        self._closed = False

    async def read(self, max_bytes: int) -> bytes:
        self._ensure_open("serial.read")
        return await self._client._serial_read(self, max_bytes)

    async def write(self, data: bytes | bytearray | memoryview) -> None:
        self._ensure_open("serial.write")
        await self._client._serial_write(self, data)

    async def flush(self) -> None:
        self._ensure_open("serial.flush")
        await self._client._serial_flush(self)

    async def set_control_lines(self, lines: ControlLines) -> None:
        self._ensure_open("serial.set_control_lines")
        await self._client._serial_set_control_lines(self, lines)

    async def close(self) -> None:
        if self._closed:
            return
        await self._client._serial_close(self)
        self._closed = True

    def _ensure_open(self, operation: str) -> None:
        if self._closed:
            raise client_error(
                "runtime.session.closed",
                ErrorCategory.CONFLICT,
                operation,
                False,
                "serial session is closed",
            )
