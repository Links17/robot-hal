"""Immutable, protobuf-independent GPIO values and broker session."""

from __future__ import annotations

from dataclasses import dataclass
from enum import Enum
from typing import TYPE_CHECKING

from .errors import ErrorCategory, HalError, client_error

if TYPE_CHECKING:
    from .client import HalClient


MAX_GPIO_EVENTS = 1024


class GpioBias(Enum):
    DISABLED = "disabled"
    PULL_UP = "pull_up"
    PULL_DOWN = "pull_down"


class GpioDrive(Enum):
    PUSH_PULL = "push_pull"
    OPEN_DRAIN = "open_drain"
    OPEN_SOURCE = "open_source"


class GpioDirection(Enum):
    INPUT = "input"
    OUTPUT = "output"


class GpioEdge(Enum):
    RISING = "rising"
    FALLING = "falling"


@dataclass(frozen=True, slots=True)
class GpioLineConfig:
    direction: GpioDirection
    active_low: bool = False
    bias: GpioBias = GpioBias.DISABLED
    drive: GpioDrive | None = None
    initial_value: bool | None = None

    def __post_init__(self) -> None:
        if not isinstance(self.direction, GpioDirection) or not isinstance(self.active_low, bool) or not isinstance(self.bias, GpioBias):
            raise _invalid("GPIO configuration is invalid")
        output = self.direction is GpioDirection.OUTPUT
        if output != (self.drive is not None and self.initial_value is not None):
            raise _invalid("GPIO output requires drive and initial value")
        if self.drive is not None and not isinstance(self.drive, GpioDrive):
            raise _invalid("GPIO drive is invalid")
        if self.initial_value is not None and not isinstance(self.initial_value, bool):
            raise _invalid("GPIO initial value is invalid")


@dataclass(frozen=True, slots=True)
class GpioEdgeRequest:
    rising: bool
    falling: bool
    capacity: int

    def __post_init__(self) -> None:
        if not isinstance(self.rising, bool) or not isinstance(self.falling, bool) or not (self.rising or self.falling) or not isinstance(self.capacity, int) or isinstance(self.capacity, bool) or not 0 < self.capacity <= MAX_GPIO_EVENTS:
            raise _invalid("GPIO edge request is invalid")


@dataclass(frozen=True, slots=True)
class GpioEdgeEvent:
    edge: GpioEdge
    monotonic_ns: int
    sequence: int


class GpioSession:
    __slots__ = ("_client", "_session_id", "_lease_id", "_generation", "_mode", "_resource_id", "_line_count", "_closed")

    def __init__(self, client: HalClient, session_id: str, lease_id: str, generation: int, mode: int, resource_id: str, line_count: int) -> None:
        self._client, self._session_id, self._lease_id = client, session_id, lease_id
        self._generation, self._mode, self._resource_id, self._line_count, self._closed = generation, mode, resource_id, line_count, False

    async def read(self) -> tuple[bool, ...]:
        self._ensure_open("gpio.read")
        return await self._client._gpio_read(self)

    async def write(self, values: tuple[bool, ...]) -> None:
        self._ensure_open("gpio.write")
        await self._client._gpio_write(self, values)

    async def next_edge(self, request: GpioEdgeRequest, timeout_ms: int) -> GpioEdgeEvent | None:
        self._ensure_open("gpio.next_edge")
        return await self._client._gpio_next_edge(self, request, timeout_ms)

    async def close(self) -> None:
        if not self._closed:
            await self._client._gpio_close(self)
            self._closed = True

    def _ensure_open(self, operation: str) -> None:
        if self._closed:
            raise HalError("runtime.session.closed", ErrorCategory.CONFLICT, operation, False, "GPIO session is closed", resource_id=self._resource_id)


def _invalid(message: str) -> HalError:
    return client_error("gpio.configuration.invalid", ErrorCategory.INVALID_ARGUMENT, "gpio.configuration", False, message)
