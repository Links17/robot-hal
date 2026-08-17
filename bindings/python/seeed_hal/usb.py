"""Immutable, protobuf-independent USB values and broker session."""

from __future__ import annotations

from dataclasses import dataclass
from enum import Enum
from typing import TYPE_CHECKING

from .errors import ErrorCategory, HalError, client_error

if TYPE_CHECKING:
    from .client import HalClient


MAX_USB_TRANSFER_BYTES = 16 * 1024


class UsbTransferKind(Enum):
    CONTROL_OUT = "control_out"
    CONTROL_IN = "control_in"
    BULK_OUT = "bulk_out"
    BULK_IN = "bulk_in"
    INTERRUPT_OUT = "interrupt_out"
    INTERRUPT_IN = "interrupt_in"


@dataclass(frozen=True, slots=True)
class UsbTransfer:
    kind: UsbTransferKind
    endpoint: int | None = None
    data: bytes = b""
    max_bytes: int = 0
    request_type: int = 0
    request: int = 0
    value: int = 0
    index: int = 0

    def __post_init__(self) -> None:
        if not isinstance(self.kind, UsbTransferKind):
            raise _invalid("USB transfer kind is invalid")
        try:
            data = memoryview(self.data).tobytes()
        except (TypeError, ValueError, BufferError) as error:
            raise _invalid("USB transfer data must be bytes-like") from error
        object.__setattr__(self, "data", data)
        if len(data) > MAX_USB_TRANSFER_BYTES or not 0 <= self.max_bytes <= MAX_USB_TRANSFER_BYTES:
            raise _invalid("USB transfer exceeds the public byte bound")
        if self.kind in {UsbTransferKind.CONTROL_OUT, UsbTransferKind.CONTROL_IN}:
            if not all(isinstance(value, int) and not isinstance(value, bool) and 0 <= value <= maximum for value, maximum in ((self.request_type, 255), (self.request, 255), (self.value, 65535), (self.index, 65535))):
                raise _invalid("USB control transfer fields are invalid")
            if bool(self.request_type & 0x80) != (self.kind is UsbTransferKind.CONTROL_IN):
                raise _invalid("USB control direction does not match transfer")
        else:
            if not isinstance(self.endpoint, int) or isinstance(self.endpoint, bool) or not 0 < self.endpoint <= 255:
                raise _invalid("USB endpoint is invalid")
            input_transfer = self.kind in {UsbTransferKind.BULK_IN, UsbTransferKind.INTERRUPT_IN}
            if bool(self.endpoint & 0x80) != input_transfer:
                raise _invalid("USB endpoint direction does not match transfer")


class UsbSession:
    __slots__ = ("_client", "_session_id", "_lease_id", "_generation", "_mode", "_resource_id", "_closed")

    def __init__(self, client: HalClient, session_id: str, lease_id: str, generation: int, mode: int, resource_id: str) -> None:
        self._client, self._session_id, self._lease_id = client, session_id, lease_id
        self._generation, self._mode, self._resource_id, self._closed = generation, mode, resource_id, False

    async def transfer(self, transfer: UsbTransfer, timeout_ms: int) -> bytes:
        self._ensure_open("usb.transfer")
        return await self._client._usb_transfer(self, transfer, timeout_ms)

    async def close(self) -> None:
        if not self._closed:
            await self._client._usb_close(self)
            self._closed = True

    def _ensure_open(self, operation: str) -> None:
        if self._closed:
            raise HalError(
                "runtime.session.closed",
                ErrorCategory.CONFLICT,
                operation,
                False,
                "USB session is closed",
                resource_id=self._resource_id,
            )


def _invalid(message: str):
    return client_error("usb.transfer.invalid", ErrorCategory.INVALID_ARGUMENT, "usb.transfer", False, message)
