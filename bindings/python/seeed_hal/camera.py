"""Immutable, copy-only Camera values and broker session."""

from __future__ import annotations

from dataclasses import dataclass
from enum import Enum
from typing import TYPE_CHECKING, Callable

from .errors import ErrorCategory, HalError, client_error

if TYPE_CHECKING:
    from .client import HalClient


MAX_CAMERA_DIMENSION = 16_384
MAX_CAMERA_SLOT_COUNT = 64
MAX_MAPPING_BYTES = 256 * 1024 * 1024


class PixelFormat(Enum):
    NV12 = "nv12"
    YUYV = "yuyv"
    MJPEG = "mjpeg"


class ControlKind(Enum):
    EXPOSURE = "exposure"
    GAIN = "gain"
    WHITE_BALANCE = "white_balance"
    FOCUS = "focus"


@dataclass(frozen=True, slots=True)
class CameraFormat:
    pixel_format: PixelFormat
    width: int
    height: int

    def __post_init__(self) -> None:
        if (
            not isinstance(self.pixel_format, PixelFormat)
            or not _plain_int(self.width)
            or not _plain_int(self.height)
            or not (0 < self.width <= MAX_CAMERA_DIMENSION)
            or not (0 < self.height <= MAX_CAMERA_DIMENSION)
        ):
            raise _invalid("camera format is invalid")


@dataclass(frozen=True, slots=True)
class ControlValue:
    integer: int | None = None
    enum: str | None = None

    def __post_init__(self) -> None:
        if (self.integer is None) == (self.enum is None):
            raise _invalid("camera control value must contain exactly one value")
        if self.integer is not None and not _plain_int(self.integer):
            raise _invalid("camera integer control value is invalid")
        if self.enum is not None and (
            not self.enum or not self.enum.isascii() or len(self.enum) > 255
        ):
            raise _invalid("camera enum control value is invalid")


@dataclass(frozen=True, slots=True, repr=False)
class MappingDescriptor:
    mapping_name: str
    mapping_identity: bytes
    capability_token: bytes
    total_length: int

    def __post_init__(self) -> None:
        if (
            not self.mapping_name
            or not self.mapping_name.isascii()
            or len(self.mapping_name) > 255
            or len(self.mapping_identity) != 32
            or len(self.capability_token) != 32
            or not _plain_int(self.total_length)
            or not (0 < self.total_length <= MAX_MAPPING_BYTES)
        ):
            raise _invalid("camera mapping descriptor is invalid")

    def __repr__(self) -> str:
        return (
            "MappingDescriptor("
            f"mapping_name={self.mapping_name!r}, mapping_identity=<redacted>, "
            "capability_token=<redacted>, "
            f"total_length={self.total_length})"
        )


@dataclass(frozen=True, slots=True)
class FrameLease:
    slot_index: int
    sequence: int
    generation: int

    def __post_init__(self) -> None:
        if (
            not _plain_int(self.slot_index)
            or not _plain_int(self.sequence)
            or not _plain_int(self.generation)
            or not 0 <= self.slot_index < MAX_CAMERA_SLOT_COUNT
            or self.generation == 0
        ):
            raise _invalid("camera frame lease is invalid")


class BorrowedFrame:
    """Copy-only read access; data must not outlive its session generation."""

    __slots__ = ("_session", "_lease", "_copy")

    def __init__(
        self,
        session: CameraSession,
        lease: FrameLease,
        copy: Callable[[], bytes],
    ) -> None:
        self._session, self._lease, self._copy = session, lease, copy

    def copy_bytes(self) -> bytes:
        if not self._session._is_generation_live(self._lease.generation):
            raise HalError(
                "runtime.lease.stale_generation",
                ErrorCategory.CONFLICT,
                "camera.frame.copy",
                False,
                "camera frame lease is no longer valid",
                resource_id=self._session._resource_id,
            )
        return bytes(self._copy())


class CameraSession:
    __slots__ = (
        "_client",
        "_session_id",
        "_lease_id",
        "_generation",
        "_mode",
        "_resource_id",
        "_closed",
    )

    def __init__(
        self,
        client: HalClient | None,
        session_id: str,
        lease_id: str,
        generation: int,
        resource_id: str,
        mode: int | None = None,
    ) -> None:
        self._client, self._session_id, self._lease_id = client, session_id, lease_id
        self._generation, self._mode = generation, mode
        self._resource_id, self._closed = resource_id, False

    async def capture(self, timeout_ms: int) -> None:
        self._ensure_open("camera.capture")
        assert self._client is not None
        await self._client._camera_capture(self, timeout_ms)

    async def mapping_descriptor(self) -> MappingDescriptor:
        self._ensure_open("camera.mapping_descriptor")
        assert self._client is not None
        return await self._client._camera_mapping_descriptor(self)

    async def next_frame_lease(self) -> FrameLease | None:
        self._ensure_open("camera.next_frame_lease")
        assert self._client is not None
        return await self._client._camera_next_frame_lease(self)

    async def dropped_count(self) -> int:
        self._ensure_open("camera.dropped_count")
        assert self._client is not None
        return await self._client._camera_dropped_count(self)

    async def get_control(self, kind: ControlKind) -> ControlValue:
        self._ensure_open("camera.control.get")
        assert self._client is not None
        return await self._client._camera_get_control(self, kind)

    async def set_control(self, kind: ControlKind, value: ControlValue) -> None:
        self._ensure_open("camera.control.set")
        assert self._client is not None
        await self._client._camera_set_control(self, kind, value)

    async def set_auto(self, kind: ControlKind, enabled: bool) -> None:
        self._ensure_open("camera.control.auto")
        assert self._client is not None
        await self._client._camera_set_auto(self, kind, enabled)

    async def close(self) -> None:
        if not self._closed:
            if self._client is not None:
                await self._client._camera_close(self)
            self._invalidate()

    def _invalidate(self) -> None:
        self._closed = True
        self._generation += 1

    def _is_generation_live(self, generation: int) -> bool:
        return not self._closed and generation == self._generation

    def _ensure_open(self, operation: str) -> None:
        if self._closed:
            raise HalError(
                "runtime.session.closed",
                ErrorCategory.CONFLICT,
                operation,
                False,
                "camera session is closed",
                resource_id=self._resource_id,
            )


def _plain_int(value: object) -> bool:
    return isinstance(value, int) and not isinstance(value, bool)


def _invalid(message: str) -> HalError:
    return client_error(
        "camera.configuration.invalid",
        ErrorCategory.INVALID_ARGUMENT,
        "camera.configuration",
        False,
        message,
    )
