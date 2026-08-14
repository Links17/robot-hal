"""Stable structured errors returned by the HAL client and broker."""

from __future__ import annotations

from dataclasses import dataclass
from enum import Enum


class ErrorCategory(Enum):
    INVALID_ARGUMENT = "invalid_argument"
    NOT_FOUND = "not_found"
    CONFLICT = "conflict"
    UNAVAILABLE = "unavailable"
    INTERNAL = "internal"


@dataclass(eq=True, slots=True)
class HalError(Exception):
    name: str
    category: ErrorCategory
    operation: str
    retryable: bool
    debug_message: str

    def __post_init__(self) -> None:
        Exception.__init__(self, self.debug_message)

    def __str__(self) -> str:
        return f"{self.name} during {self.operation}: {self.debug_message}"


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
