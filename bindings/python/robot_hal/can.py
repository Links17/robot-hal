"""Immutable, protobuf-independent CAN/CAN FD models and session handle."""

from __future__ import annotations

from dataclasses import dataclass
from enum import Enum
from typing import TYPE_CHECKING, Iterable

from .errors import ErrorCategory, HalError, client_error

if TYPE_CHECKING:
    from .client import HalClient


MAX_U16 = (1 << 16) - 1
MAX_U32 = (1 << 32) - 1
MAX_U64 = (1 << 64) - 1
MAX_CAN_BATCH_FRAMES = 64
MAX_CAN_FILTERS = 64
MAX_CAN_ERROR_CLASSES = 10
_FD_LENGTHS = frozenset((*range(9), 12, 16, 20, 24, 32, 48, 64))


class CanIdFormat(Enum):
    STANDARD = "standard"
    EXTENDED = "extended"
    EITHER = "either"


@dataclass(frozen=True, slots=True)
class CanId:
    value: int
    format: CanIdFormat

    def __post_init__(self) -> None:
        if not _plain_int(self.value) or not isinstance(self.format, CanIdFormat):
            raise _frame_error("CAN identifier is invalid")
        maximum = 0x7FF if self.format is CanIdFormat.STANDARD else 0x1FFF_FFFF
        if self.format is CanIdFormat.EITHER or not 0 <= self.value <= maximum:
            width = "11" if self.format is CanIdFormat.STANDARD else "29"
            raise _frame_error(f"CAN identifier exceeds {width} bits")

    @classmethod
    def standard(cls, value: int) -> CanId:
        return cls(value, CanIdFormat.STANDARD)

    @classmethod
    def extended(cls, value: int) -> CanId:
        return cls(value, CanIdFormat.EXTENDED)


class CanErrorClass(Enum):
    TX_TIMEOUT = "tx_timeout"
    LOST_ARBITRATION = "lost_arbitration"
    CONTROLLER = "controller"
    PROTOCOL = "protocol"
    TRANSCEIVER = "transceiver"
    NO_ACKNOWLEDGEMENT = "no_acknowledgement"
    BUS_OFF = "bus_off"
    BUS_ERROR = "bus_error"
    RESTARTED = "restarted"
    OTHER = "other"


class CanFrame:
    """Base class for the four explicit immutable CAN frame variants."""

    __slots__ = ()

    def __new__(cls, *_args: object, **_kwargs: object):
        if cls is CanFrame:
            raise TypeError("CanFrame is an abstract frame-variant base")
        return super().__new__(cls)

    @staticmethod
    def classic_data(can_id: CanId, data: object = b"") -> ClassicDataFrame:
        return ClassicDataFrame(can_id, data)

    @staticmethod
    def classic_remote(can_id: CanId, dlc: int) -> ClassicRemoteFrame:
        return ClassicRemoteFrame(can_id, dlc)

    @staticmethod
    def fd_data(
        can_id: CanId,
        data: object = b"",
        *,
        bitrate_switch: bool = False,
        error_state_indicator: bool = False,
    ) -> FdDataFrame:
        return FdDataFrame(can_id, data, bitrate_switch, error_state_indicator)

    @staticmethod
    def error(
        classes: Iterable[CanErrorClass], data: object = b""
    ) -> ErrorFrame:
        return ErrorFrame(classes, data)


@dataclass(frozen=True, slots=True)
class ClassicDataFrame(CanFrame):
    id: CanId
    data: bytes = b""

    def __post_init__(self) -> None:
        _require_id(self.id)
        data = _copy_bytes(self.data)
        if len(data) > 8:
            raise _frame_error("Classical CAN data exceeds 8 bytes")
        object.__setattr__(self, "data", data)


@dataclass(frozen=True, slots=True)
class ClassicRemoteFrame(CanFrame):
    id: CanId
    dlc: int

    def __post_init__(self) -> None:
        _require_id(self.id)
        if not _plain_int(self.dlc) or not 0 <= self.dlc <= 8:
            raise _frame_error("Classical CAN remote DLC exceeds 8")


@dataclass(frozen=True, slots=True)
class FdDataFrame(CanFrame):
    id: CanId
    data: bytes = b""
    bitrate_switch: bool = False
    error_state_indicator: bool = False

    def __post_init__(self) -> None:
        _require_id(self.id)
        data = _copy_bytes(self.data)
        if len(data) not in _FD_LENGTHS:
            raise _frame_error(
                "CAN FD data length must be one of 0..=8, 12, 16, 20, 24, 32, 48, or 64"
            )
        if not isinstance(self.bitrate_switch, bool) or not isinstance(
            self.error_state_indicator, bool
        ):
            raise _frame_error("CAN FD flags must be bool")
        object.__setattr__(self, "data", data)


@dataclass(frozen=True, slots=True)
class ErrorFrame(CanFrame):
    classes: tuple[CanErrorClass, ...]
    data: bytes = b""

    def __init__(self, classes: Iterable[CanErrorClass], data: object = b"") -> None:
        try:
            copied_classes = tuple(classes)
        except TypeError as error:
            raise _frame_error("CAN error frame classes must be iterable") from error
        copied_data = _copy_bytes(data)
        if not 1 <= len(copied_classes) <= MAX_CAN_ERROR_CLASSES or not all(
            isinstance(item, CanErrorClass) for item in copied_classes
        ):
            raise _frame_error("CAN error frame must contain 1..=10 classes")
        if len(copied_data) > 8:
            raise _frame_error("CAN error diagnostics exceed 8 bytes")
        object.__setattr__(self, "classes", copied_classes)
        object.__setattr__(self, "data", copied_data)


# Descriptive aliases keep variant names obvious without exposing wire types.
CanClassicDataFrame = ClassicDataFrame
CanClassicRemoteFrame = ClassicRemoteFrame
CanFdDataFrame = FdDataFrame
CanErrorFrame = ErrorFrame


class CanTimestampSource(Enum):
    HARDWARE = "hardware"
    KERNEL = "kernel"
    HOST_MONOTONIC = "host_monotonic"


@dataclass(frozen=True, slots=True)
class CanTimestamp:
    timestamp_ns: int
    source: CanTimestampSource
    clock_domain: str

    def __post_init__(self) -> None:
        if not _plain_int(self.timestamp_ns) or not 0 <= self.timestamp_ns <= MAX_U64:
            raise _frame_error("CAN timestamp is outside the u64 range")
        if not isinstance(self.source, CanTimestampSource):
            raise _frame_error("CAN timestamp source is invalid")
        if (
            not isinstance(self.clock_domain, str)
            or not self.clock_domain
            or len(self.clock_domain) > 255
            or not self.clock_domain.isascii()
        ):
            raise _frame_error(
                "CAN timestamp clock domain must be non-empty ASCII of at most 255 bytes"
            )


@dataclass(frozen=True, slots=True)
class ReceivedCanFrame:
    frame: CanFrame
    timestamp: CanTimestamp | None = None

    def __post_init__(self) -> None:
        if not isinstance(
            self.frame,
            (ClassicDataFrame, ClassicRemoteFrame, FdDataFrame, ErrorFrame),
        ) or (
            self.timestamp is not None and not isinstance(self.timestamp, CanTimestamp)
        ):
            raise _frame_error("received CAN frame metadata is invalid")


class CanMode(Enum):
    CLASSIC = "classic"
    FD = "fd"


@dataclass(frozen=True, slots=True)
class CanBitTiming:
    bitrate: int
    sample_point_permill: int | None = None
    sjw: int | None = None

    def __post_init__(self) -> None:
        if not _plain_int(self.bitrate) or not 1 <= self.bitrate <= MAX_U32:
            raise _configuration_error("CAN bitrate must be nonzero and within u32")
        if self.sample_point_permill is not None and (
            not _plain_int(self.sample_point_permill)
            or not 1 <= self.sample_point_permill <= 999
        ):
            raise _configuration_error("CAN sample point must be 1..=999 permill")
        if self.sjw is not None and (
            not _plain_int(self.sjw) or not 1 <= self.sjw <= MAX_U16
        ):
            raise _configuration_error("CAN SJW must be nonzero and within u16")


@dataclass(frozen=True, slots=True)
class CanLinkExpectation:
    mode: CanMode | None = None
    nominal_bitrate: int | None = None
    data_bitrate: int | None = None
    listen_only: bool | None = None
    loopback: bool | None = None

    def __post_init__(self) -> None:
        if self.mode is not None and not isinstance(self.mode, CanMode):
            raise _configuration_error("CAN expected mode is invalid")
        for value in (self.nominal_bitrate, self.data_bitrate):
            if value is not None and (
                not _plain_int(value) or not 1 <= value <= MAX_U32
            ):
                raise _configuration_error("CAN expected bitrates must be nonzero")
        if any(
            value is not None and not isinstance(value, bool)
            for value in (self.listen_only, self.loopback)
        ):
            raise _configuration_error("CAN expected flags must be bool")
        if self.mode is CanMode.CLASSIC and self.data_bitrate is not None:
            raise _configuration_error(
                "Classical CAN expectation cannot include data bitrate"
            )


@dataclass(frozen=True, slots=True)
class CanConfigureConfig:
    mode: CanMode
    nominal: CanBitTiming
    data: CanBitTiming | None = None
    listen_only: bool = False
    loopback: bool = False
    restart_ms: int | None = None

    def __post_init__(self) -> None:
        if not isinstance(self.mode, CanMode) or not isinstance(
            self.nominal, CanBitTiming
        ):
            raise _configuration_error("CAN configuration mode or timing is invalid")
        if self.data is not None and not isinstance(self.data, CanBitTiming):
            raise _configuration_error("CAN data timing is invalid")
        if not isinstance(self.listen_only, bool) or not isinstance(self.loopback, bool):
            raise _configuration_error("CAN configuration flags must be bool")
        if self.restart_ms is not None and (
            not _plain_int(self.restart_ms) or not 1 <= self.restart_ms <= MAX_U32
        ):
            raise _configuration_error(
                "CAN restart time must be nonzero when specified"
            )
        if self.mode is CanMode.CLASSIC and self.data is not None:
            raise _configuration_error(
                "Classical CAN configuration cannot include data timing"
            )
        if self.mode is CanMode.FD and self.data is None:
            raise _configuration_error("CAN FD configuration requires data timing")


@dataclass(frozen=True, slots=True)
class CanOpenConfig:
    attach: CanLinkExpectation | None = None
    configure: CanConfigureConfig | None = None

    def __post_init__(self) -> None:
        if (self.attach is None) == (self.configure is None):
            raise _configuration_error(
                "CAN open configuration must select exactly one of Attach or Configure"
            )
        if self.attach is not None and not isinstance(self.attach, CanLinkExpectation):
            raise _configuration_error("CAN Attach expectation is invalid")
        if self.configure is not None and not isinstance(
            self.configure, CanConfigureConfig
        ):
            raise _configuration_error("CAN Configure configuration is invalid")


@dataclass(frozen=True, slots=True)
class CanFrameClasses:
    data: bool = False
    remote: bool = False
    error: bool = False

    def __post_init__(self) -> None:
        if not all(isinstance(item, bool) for item in (self.data, self.remote, self.error)):
            raise _filter_error("CAN frame classes must be bool")

    @classmethod
    def data_only(cls) -> CanFrameClasses:
        return cls(data=True)


@dataclass(frozen=True, slots=True)
class CanFilter:
    id: int
    mask: int
    format: CanIdFormat
    classes: CanFrameClasses

    def __post_init__(self) -> None:
        if not isinstance(self.format, CanIdFormat) or not isinstance(
            self.classes, CanFrameClasses
        ):
            raise _filter_error("CAN filter format or classes are invalid")
        if not (self.classes.data or self.classes.remote or self.classes.error):
            raise _filter_error("CAN filter must enable a frame class")
        maximum = 0x7FF if self.format is CanIdFormat.STANDARD else 0x1FFF_FFFF
        if (
            not _plain_int(self.id)
            or not _plain_int(self.mask)
            or not 0 <= self.id <= maximum
            or not 0 <= self.mask <= maximum
        ):
            raise _filter_error("CAN filter ID or mask exceeds format width")

    def matches(self, frame: CanFrame) -> bool:
        if not isinstance(frame, CanFrame):
            raise _filter_error("CAN filter input must be a CAN frame")
        if isinstance(frame, ErrorFrame):
            return self.classes.error
        if isinstance(frame, ClassicRemoteFrame):
            enabled = self.classes.remote
        else:
            enabled = self.classes.data
        if not enabled:
            return False
        assert isinstance(frame, (ClassicDataFrame, ClassicRemoteFrame, FdDataFrame))
        format_matches = (
            self.format is CanIdFormat.EITHER or self.format is frame.id.format
        )
        return format_matches and (frame.id.value & self.mask) == (self.id & self.mask)


@dataclass(frozen=True, slots=True)
class CanFilterSet:
    filters: tuple[CanFilter, ...] = ()

    def __init__(self, filters: Iterable[CanFilter] = ()) -> None:
        try:
            copied = tuple(filters)
        except TypeError as error:
            raise _filter_error("CAN filters must be iterable") from error
        if len(copied) > MAX_CAN_FILTERS:
            raise _filter_error("CAN filter set exceeds 64 filters")
        if not all(isinstance(item, CanFilter) for item in copied):
            raise _filter_error("CAN filter set contains an invalid filter")
        object.__setattr__(self, "filters", copied)

    def matches(self, frame: CanFrame) -> bool:
        return not self.filters or any(item.matches(frame) for item in self.filters)


class CanBusState(Enum):
    ACTIVE = "active"
    WARNING = "warning"
    PASSIVE = "passive"
    BUS_OFF = "bus_off"
    STOPPED = "stopped"
    UNKNOWN = "unknown"


@dataclass(frozen=True, slots=True)
class CanBusStatus:
    state: CanBusState
    tx_error_counter: int | None = None
    rx_error_counter: int | None = None

    def __post_init__(self) -> None:
        if not isinstance(self.state, CanBusState):
            raise _frame_error("CAN bus state is invalid")
        if any(
            value is not None and (not _plain_int(value) or not 0 <= value <= MAX_U32)
            for value in (self.tx_error_counter, self.rx_error_counter)
        ):
            raise _frame_error("CAN bus error counter is outside the u32 range")


@dataclass(eq=True, frozen=True, repr=False, slots=True)
class CanBatchSendError(Exception):
    error: HalError
    committed: int = 0

    def __post_init__(self) -> None:
        if not isinstance(self.error, HalError):
            raise TypeError("error must be HalError")
        if not _plain_int(self.committed) or self.committed < 0:
            raise TypeError("committed must be a non-negative integer")
        Exception.__init__(self, str(self))

    def __str__(self) -> str:
        return f"{self.error} (committed {self.committed})"

    def __repr__(self) -> str:
        return (
            "CanBatchSendError("
            f"name={self.error.name!r}, category={self.error.category!r}, "
            f"operation={self.error.operation!r}, retryable={self.error.retryable!r}, "
            f"committed={self.committed!r})"
        )


@dataclass(frozen=True, slots=True)
class _CanSessionProfile:
    mode: CanMode
    classic_frames: bool
    fd_frames: bool
    error_frames: bool
    timestamps: bool
    resource_id: str
    session_id: str


class CanSession:
    """Opaque broker CAN session; the broker retains the native channel."""

    __slots__ = (
        "_client",
        "_session_id",
        "_lease_id",
        "_generation",
        "_lease_mode",
        "_profile",
        "_closed",
    )

    def __init__(
        self,
        client: HalClient,
        session_id: str,
        lease_id: str,
        generation: int,
        lease_mode: int,
        profile: _CanSessionProfile,
    ) -> None:
        self._client = client
        self._session_id = session_id
        self._lease_id = lease_id
        self._generation = generation
        self._lease_mode = lease_mode
        self._profile = profile
        self._closed = False

    async def send(self, frame: CanFrame) -> None:
        await self.send_batch((frame,))

    async def send_batch(self, frames: Iterable[CanFrame]) -> None:
        self._ensure_open("can.send_batch")
        await self._client._can_send_batch(self, frames)

    async def receive(self, max_frames: int, timeout_ms: int) -> tuple[ReceivedCanFrame, ...]:
        self._ensure_open("can.receive")
        return await self._client._can_receive(self, max_frames, timeout_ms)

    async def replace_filters(self, filters: CanFilterSet) -> None:
        self._ensure_open("can.replace_filters")
        await self._client._can_replace_filters(self, filters)

    async def bus_status(self) -> CanBusStatus:
        self._ensure_open("can.status")
        return await self._client._can_bus_status(self)

    async def close(self) -> None:
        if self._closed:
            return
        await self._client._can_close(self)
        self._closed = True

    def _ensure_open(self, operation: str) -> None:
        if self._closed:
            raise HalError(
                "runtime.session.closed",
                ErrorCategory.CONFLICT,
                operation,
                False,
                "the remote CAN handle is closed",
                resource_id=self._profile.resource_id,
            )


def _plain_int(value: object) -> bool:
    return isinstance(value, int) and not isinstance(value, bool)


def _copy_bytes(value: object) -> bytes:
    try:
        return memoryview(value).tobytes()  # type: ignore[arg-type]
    except (TypeError, ValueError, BufferError, OverflowError) as error:
        raise _frame_error("CAN frame data must be bytes-like") from error


def _require_id(value: object) -> None:
    if not isinstance(value, CanId):
        raise _frame_error("CAN frame identifier is invalid")


def _frame_error(message: str) -> HalError:
    return client_error(
        "can.frame.invalid", ErrorCategory.INVALID_ARGUMENT, "can.frame", False, message
    )


def _configuration_error(message: str) -> HalError:
    return client_error(
        "can.configuration.invalid",
        ErrorCategory.INVALID_ARGUMENT,
        "can.configuration",
        False,
        message,
    )


def _filter_error(message: str) -> HalError:
    return client_error(
        "can.filter.invalid", ErrorCategory.INVALID_ARGUMENT, "can.filter", False, message
    )
