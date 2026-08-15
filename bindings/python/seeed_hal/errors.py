"""Stable structured errors returned by the HAL client and broker."""

from __future__ import annotations

from dataclasses import dataclass, field
from enum import Enum
from types import MappingProxyType
from typing import Mapping


class ErrorCategory(Enum):
    INVALID_ARGUMENT = "invalid_argument"
    NOT_FOUND = "not_found"
    CONFLICT = "conflict"
    UNAVAILABLE = "unavailable"
    INTERNAL = "internal"


@dataclass(eq=True, frozen=True, slots=True)
class HalError(Exception):
    name: str
    category: ErrorCategory
    operation: str
    retryable: bool
    debug_message: str
    resource_id: str | None = None
    platform_code: str | None = None
    vendor_code: str | None = None
    context: Mapping[str, str] = field(
        default_factory=lambda: MappingProxyType({}),
        hash=False,
    )

    def __post_init__(self) -> None:
        Exception.__init__(self, self.debug_message)
        object.__setattr__(self, "context", MappingProxyType(dict(self.context)))

    def __str__(self) -> str:
        return f"{self.name} during {self.operation}: {self.debug_message}"


@dataclass(frozen=True, slots=True)
class _ErrorData:
    name: str
    category: ErrorCategory
    operation: str
    retryable: bool
    debug_message: str
    resource_id: str | None
    platform_code: str | None
    vendor_code: str | None
    context_items: tuple[tuple[str, str], ...]


def _error_data(error: HalError) -> _ErrorData:
    return _ErrorData(
        error.name,
        error.category,
        error.operation,
        error.retryable,
        error.debug_message,
        error.resource_id,
        error.platform_code,
        error.vendor_code,
        tuple(sorted(error.context.items())),
    )


def _fresh_error(data: _ErrorData) -> HalError:
    return HalError(
        data.name,
        data.category,
        data.operation,
        data.retryable,
        data.debug_message,
        data.resource_id,
        data.platform_code,
        data.vendor_code,
        dict(data.context_items),
    )


def client_error(
    name: str,
    category: ErrorCategory,
    operation: str,
    retryable: bool,
    message: str,
) -> HalError:
    return HalError(name, category, operation, retryable, message)


def disconnected_error(operation: str, message: str) -> HalError:
    return client_error(
        "runtime.broker.disconnected",
        ErrorCategory.UNAVAILABLE,
        operation,
        True,
        message,
    )


def frame_too_large(message: str) -> HalError:
    return client_error(
        "runtime.protocol.frame_too_large",
        ErrorCategory.INVALID_ARGUMENT,
        "runtime.protocol.frame",
        False,
        message,
    )
