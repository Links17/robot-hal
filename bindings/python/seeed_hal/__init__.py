"""Public, protobuf-independent Seeed HAL Python API."""

from .client import (
    EventSubscription,
    HalClient,
    IdentityQuality,
    ResourceDescriptor,
    ResourceSelector,
    RuntimeEvent,
    TransportKind,
)
from .errors import ErrorCategory, HalError
from .serial import (
    ControlLines,
    DataBits,
    FlowControl,
    Parity,
    SerialConfig,
    SerialSession,
    StopBits,
)

__all__ = [
    "ControlLines",
    "DataBits",
    "ErrorCategory",
    "EventSubscription",
    "FlowControl",
    "HalClient",
    "HalError",
    "IdentityQuality",
    "Parity",
    "ResourceDescriptor",
    "ResourceSelector",
    "RuntimeEvent",
    "SerialConfig",
    "SerialSession",
    "StopBits",
    "TransportKind",
]
