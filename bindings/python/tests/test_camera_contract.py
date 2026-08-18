from __future__ import annotations

import asyncio
from dataclasses import FrozenInstanceError

import pytest

import seeed_hal
from seeed_hal import (
    CameraControlDescriptor,
    CameraFormat,
    CameraSession,
    ControlEnumValues,
    FrameLease,
    MappingDescriptor,
    PixelFormat,
    ControlRange,
)
from seeed_hal.errors import HalError
from seeed_hal.proto import hal_pb2
from test_client_contract import TOKEN, envelope, fake_broker, read_frame, send_frame


class _BytesMappingReader:
    def copy_bytes(self, descriptor: MappingDescriptor, lease: FrameLease) -> bytearray:
        assert descriptor.mapping_identity == b"i" * 32
        assert lease.slot_index == 0
        assert lease.sequence != 0
        assert lease.generation == 1
        return bytearray(b"frame")


@pytest.mark.asyncio
@pytest.mark.parametrize("acquire", ["next_frame", "next_frame_lease"])
async def test_frame_acquisition_without_a_local_mapping_reader_fails_before_lease_rpc(
    acquire: str,
) -> None:
    capabilities = ["camera.capture/v1", "camera.frames.shm/v1"]

    async def handler(reader, writer):
        request = hal_pb2.Envelope.FromString(await read_frame(reader))
        assert request.WhichOneof("payload") == "enumerate_camera_request"
        await send_frame(
            writer,
            envelope(
                request.request_id,
                "enumerate_camera_response",
                hal_pb2.EnumerateCameraResponse(
                    resources=[
                        hal_pb2.ResourceDescriptor(
                            resource_id="camera:virtual:no-reader",
                            endpoint="virtual://camera:no-reader",
                            identity_quality=hal_pb2.IDENTITY_QUALITY_STRONG,
                            transport=hal_pb2.TRANSPORT_KIND_CAMERA,
                            capabilities=capabilities,
                        )
                    ]
                ),
            ).SerializeToString(),
        )
        request = hal_pb2.Envelope.FromString(await read_frame(reader))
        assert request.WhichOneof("payload") == "open_camera_request"
        await send_frame(
            writer,
            envelope(
                request.request_id,
                "open_camera_response",
                hal_pb2.OpenCameraResponse(
                    session_id="camera-no-reader",
                    lease=hal_pb2.LeaseToken(
                        lease_id="camera-no-reader-lease",
                        generation=1,
                        mode=hal_pb2.LEASE_MODE_CONTROL,
                    ),
                ),
            ).SerializeToString(),
        )
        request = hal_pb2.Envelope.FromString(await read_frame(reader))
        assert request.WhichOneof("payload") == "camera_mapping_descriptor_request"
        await send_frame(
            writer,
            envelope(
                request.request_id,
                "camera_mapping_descriptor_response",
                hal_pb2.CameraMappingDescriptorResponse(
                    descriptor=hal_pb2.MappingDescriptor(
                        mapping_name="camera-no-reader-mapping",
                        mapping_identity=b"i" * 32,
                        capability_token=b"t" * 32,
                        total_length=4096,
                    )
                ),
            ).SerializeToString(),
        )
        with pytest.raises(asyncio.TimeoutError):
            await asyncio.wait_for(read_frame(reader), timeout=0.1)

    async with fake_broker(
        handler,
        selected_minor=3,
        minimum_minor=3,
        maximum_minor=3,
        capabilities=["serial.bytes/v1", *capabilities],
    ) as endpoint:
        from seeed_hal import HalClient

        client = await HalClient.connect(endpoint, TOKEN)
        resource = (await client.enumerate_camera())[0]
        session = await client.open_camera(
            resource.selector(), CameraFormat(PixelFormat.NV12, 640, 480)
        )
        await session.mapping_descriptor()
        with pytest.raises(HalError, match="shared_memory.unavailable"):
            await getattr(session, acquire)()
        await client.close()


def test_camera_public_values_redact_tokens_and_expose_no_callback_constructed_borrowed_frames() -> None:
    assert {
        "CameraControlDescriptor",
        "CameraFormat",
        "CameraSession",
        "ControlEnumValues",
        "FrameLease",
        "MappingDescriptor",
        "PixelFormat",
        "ControlRange",
    }.issubset(seeed_hal.__all__)

    descriptor = MappingDescriptor(
        mapping_name="seeed-camera-test",
        mapping_identity=b"i" * 32,
        capability_token=b"s" * 32,
        total_length=4096,
    )
    assert b"s" * 32 not in repr(descriptor).encode()
    assert b"i" * 32 not in repr(descriptor).encode()
    with pytest.raises(FrozenInstanceError):
        descriptor.total_length = 1  # type: ignore[misc]

    assert "BorrowedFrame" not in seeed_hal.__all__
    assert not hasattr(seeed_hal, "BorrowedFrame")
    assert not hasattr(CameraSession, "borrowed_frame")


def test_camera_control_discovery_values_are_public_immutable_and_validated() -> None:
    range_values = ControlRange(minimum=1, maximum=9, step=2)
    enum_values = ControlEnumValues(
        values=(seeed_hal.ControlValue(enum="manual"), seeed_hal.ControlValue(enum="auto"))
    )
    descriptor = CameraControlDescriptor(
        kind=seeed_hal.ControlKind.EXPOSURE,
        readable=True,
        writable=True,
        auto_supported=True,
        values=range_values,
        current_value_available=True,
        diagnostic="virtual camera",
    )

    assert descriptor.values == range_values
    assert enum_values.values[1].enum == "auto"
    with pytest.raises(FrozenInstanceError):
        descriptor.readable = False  # type: ignore[misc]
    with pytest.raises(HalError, match="camera control range is invalid"):
        ControlRange(minimum=2, maximum=1, step=1)
    with pytest.raises(HalError, match="camera control enum values are invalid"):
        ControlEnumValues(values=())
    with pytest.raises(HalError, match="camera control descriptor is invalid"):
        CameraControlDescriptor(
            kind=seeed_hal.ControlKind.GAIN,
            readable=False,
            writable=True,
            auto_supported=False,
            values=enum_values,
            current_value_available=True,
        )


@pytest.mark.asyncio
async def test_next_frame_invalidates_prior_copy_borrow_and_returns_owned_bytes() -> None:
    capabilities = ["camera.capture/v1", "camera.frames.shm/v1"]

    async def handler(reader, writer):
        request = hal_pb2.Envelope.FromString(await read_frame(reader))
        assert request.WhichOneof("payload") == "enumerate_camera_request"
        await send_frame(
            writer,
            envelope(
                request.request_id,
                "enumerate_camera_response",
                hal_pb2.EnumerateCameraResponse(
                    resources=[
                        hal_pb2.ResourceDescriptor(
                            resource_id="camera:virtual:borrow",
                            endpoint="virtual://camera:borrow",
                            identity_quality=hal_pb2.IDENTITY_QUALITY_STRONG,
                            transport=hal_pb2.TRANSPORT_KIND_CAMERA,
                            capabilities=capabilities,
                        )
                    ]
                ),
            ).SerializeToString(),
        )
        request = hal_pb2.Envelope.FromString(await read_frame(reader))
        assert request.WhichOneof("payload") == "open_camera_request"
        await send_frame(
            writer,
            envelope(
                request.request_id,
                "open_camera_response",
                hal_pb2.OpenCameraResponse(
                    session_id="camera-borrow",
                    lease=hal_pb2.LeaseToken(
                        lease_id="camera-borrow-lease",
                        generation=1,
                        mode=hal_pb2.LEASE_MODE_CONTROL,
                    ),
                ),
            ).SerializeToString(),
        )
        request = hal_pb2.Envelope.FromString(await read_frame(reader))
        assert request.WhichOneof("payload") == "camera_mapping_descriptor_request"
        await send_frame(
            writer,
            envelope(
                request.request_id,
                "camera_mapping_descriptor_response",
                hal_pb2.CameraMappingDescriptorResponse(
                    descriptor=hal_pb2.MappingDescriptor(
                        mapping_name="camera-borrow-mapping",
                        mapping_identity=b"i" * 32,
                        capability_token=b"t" * 32,
                        total_length=4096,
                    )
                ),
            ).SerializeToString(),
        )
        for sequence in (1, 2):
            request = hal_pb2.Envelope.FromString(await read_frame(reader))
            assert request.WhichOneof("payload") == "camera_next_frame_lease_request"
            await send_frame(
                writer,
                envelope(
                    request.request_id,
                    "camera_next_frame_lease_response",
                    hal_pb2.CameraNextFrameLeaseResponse(
                        lease=hal_pb2.FrameLease(
                            slot_index=0, sequence=sequence, generation=1
                        )
                    ),
                ).SerializeToString(),
            )

    async with fake_broker(
        handler,
        selected_minor=3,
        minimum_minor=3,
        maximum_minor=3,
        capabilities=["serial.bytes/v1", *capabilities],
    ) as endpoint:
        from seeed_hal import HalClient

        client = await HalClient.connect(endpoint, TOKEN)
        resource = (await client.enumerate_camera())[0]
        session = await client.open_camera(
            resource.selector(), CameraFormat(PixelFormat.NV12, 640, 480)
        )
        await session.mapping_descriptor()
        session._mapping_reader = _BytesMappingReader()
        first = await session.next_frame()
        assert first is not None
        assert first.copy_bytes() == b"frame"
        second = await session.next_frame()
        assert second is not None
        with pytest.raises(HalError, match="runtime.lease.stale_generation"):
            first.copy_bytes()
        await client.close()


@pytest.mark.asyncio
async def test_malformed_camera_frame_lease_terminates_the_python_client() -> None:
    capabilities = ["camera.capture/v1", "camera.frames.shm/v1"]
    release = asyncio.Event()

    async def handler(reader, writer):
        request = hal_pb2.Envelope.FromString(await read_frame(reader))
        assert request.WhichOneof("payload") == "enumerate_camera_request"
        await send_frame(
            writer,
            envelope(
                request.request_id,
                "enumerate_camera_response",
                hal_pb2.EnumerateCameraResponse(
                    resources=[
                        hal_pb2.ResourceDescriptor(
                            resource_id="camera:virtual:malformed",
                            endpoint="virtual://camera:malformed",
                            identity_quality=hal_pb2.IDENTITY_QUALITY_STRONG,
                            transport=hal_pb2.TRANSPORT_KIND_CAMERA,
                            capabilities=capabilities,
                        )
                    ]
                ),
            ).SerializeToString(),
        )
        request = hal_pb2.Envelope.FromString(await read_frame(reader))
        assert request.WhichOneof("payload") == "open_camera_request"
        await send_frame(
            writer,
            envelope(
                request.request_id,
                "open_camera_response",
                hal_pb2.OpenCameraResponse(
                    session_id="camera-malformed",
                    lease=hal_pb2.LeaseToken(
                        lease_id="camera-malformed-lease",
                        generation=1,
                        mode=hal_pb2.LEASE_MODE_CONTROL,
                    ),
                ),
            ).SerializeToString(),
        )
        request = hal_pb2.Envelope.FromString(await read_frame(reader))
        assert request.WhichOneof("payload") == "camera_next_frame_lease_request"
        await send_frame(
            writer,
            envelope(
                request.request_id,
                "camera_next_frame_lease_response",
                hal_pb2.CameraNextFrameLeaseResponse(
                    lease=hal_pb2.FrameLease(slot_index=0, sequence=0, generation=1)
                ),
            ).SerializeToString(),
        )
        await release.wait()

    async with fake_broker(
        handler,
        selected_minor=3,
        minimum_minor=3,
        maximum_minor=3,
        capabilities=["serial.bytes/v1", *capabilities],
    ) as endpoint:
        from seeed_hal import HalClient

        client = await HalClient.connect(endpoint, TOKEN)
        resource = (await client.enumerate_camera())[0]
        session = await client.open_camera(
            resource.selector(), CameraFormat(PixelFormat.NV12, 640, 480)
        )
        session._mapping_reader = _BytesMappingReader()
        with pytest.raises(HalError, match="runtime.protocol.invalid_message"):
            await session.next_frame_lease()
        assert client._terminal is not None
        assert client._terminal.name == "runtime.protocol.invalid_message"
        await client.close()
        release.set()


@pytest.mark.asyncio
async def test_camera_fake_broker_wires_capture_mapping_lease_controls_and_close() -> None:
    capabilities = ["camera.capture/v1", "camera.frames.shm/v1", "camera.controls/v1"]

    async def handler(reader, writer):
        request = hal_pb2.Envelope.FromString(await read_frame(reader))
        assert request.WhichOneof("payload") == "enumerate_camera_request"
        await send_frame(
            writer,
            envelope(
                request.request_id,
                "enumerate_camera_response",
                hal_pb2.EnumerateCameraResponse(
                    resources=[
                        hal_pb2.ResourceDescriptor(
                            resource_id="camera:virtual:python",
                            endpoint="virtual://camera:python",
                            identity_quality=hal_pb2.IDENTITY_QUALITY_STRONG,
                            transport=hal_pb2.TRANSPORT_KIND_CAMERA,
                            capabilities=capabilities,
                        )
                    ]
                ),
            ).SerializeToString(),
        )
        request = hal_pb2.Envelope.FromString(await read_frame(reader))
        assert request.WhichOneof("payload") == "open_camera_request"
        assert request.open_camera_request.request.format.pixel_format == hal_pb2.CAMERA_PIXEL_FORMAT_NV12
        await send_frame(
            writer,
            envelope(
                request.request_id,
                "open_camera_response",
                hal_pb2.OpenCameraResponse(
                    session_id="camera-session",
                    lease=hal_pb2.LeaseToken(
                        lease_id="camera-lease",
                        generation=1,
                        mode=hal_pb2.LEASE_MODE_CONTROL,
                    ),
                ),
            ).SerializeToString(),
        )
        request = hal_pb2.Envelope.FromString(await read_frame(reader))
        assert request.WhichOneof("payload") == "capture_camera_request"
        await send_frame(
            writer,
            envelope(
                request.request_id,
                "capture_camera_response",
                hal_pb2.CaptureCameraResponse(),
            ).SerializeToString(),
        )
        request = hal_pb2.Envelope.FromString(await read_frame(reader))
        assert request.WhichOneof("payload") == "camera_mapping_descriptor_request"
        await send_frame(
            writer,
            envelope(
                request.request_id,
                "camera_mapping_descriptor_response",
                hal_pb2.CameraMappingDescriptorResponse(
                    descriptor=hal_pb2.MappingDescriptor(
                        mapping_name="camera-test-mapping",
                        mapping_identity=b"i" * 32,
                        capability_token=b"t" * 32,
                        total_length=4096,
                    )
                ),
            ).SerializeToString(),
        )
        request = hal_pb2.Envelope.FromString(await read_frame(reader))
        assert request.WhichOneof("payload") == "camera_next_frame_lease_request"
        await send_frame(
            writer,
            envelope(
                request.request_id,
                "camera_next_frame_lease_response",
                hal_pb2.CameraNextFrameLeaseResponse(
                    lease=hal_pb2.FrameLease(slot_index=0, sequence=1, generation=1)
                ),
            ).SerializeToString(),
        )
        request = hal_pb2.Envelope.FromString(await read_frame(reader))
        assert request.WhichOneof("payload") == "camera_next_frame_lease_request"
        await send_frame(
            writer,
            envelope(
                request.request_id,
                "camera_next_frame_lease_response",
                hal_pb2.CameraNextFrameLeaseResponse(
                    lease=hal_pb2.FrameLease(slot_index=0, sequence=2, generation=1)
                ),
            ).SerializeToString(),
        )
        request = hal_pb2.Envelope.FromString(await read_frame(reader))
        assert request.WhichOneof("payload") == "camera_controls_request"
        assert request.camera_controls_request.session_id == "camera-session"
        assert request.camera_controls_request.lease.generation == 1
        await send_frame(
            writer,
            envelope(
                request.request_id,
                "camera_controls_response",
                hal_pb2.CameraControlsResponse(
                    controls=[
                        hal_pb2.CameraControlDescriptor(
                            kind=hal_pb2.CAMERA_CONTROL_KIND_EXPOSURE,
                            readable=True,
                            writable=True,
                            auto_supported=True,
                            values=hal_pb2.CameraControlValues(
                                range=hal_pb2.CameraControlRange(
                                    minimum=1, maximum=99, step=1
                                )
                            ),
                            current_value_available=True,
                            diagnostic="virtual camera",
                        ),
                        hal_pb2.CameraControlDescriptor(
                            kind=hal_pb2.CAMERA_CONTROL_KIND_WHITE_BALANCE,
                            readable=True,
                            writable=True,
                            auto_supported=False,
                            values=hal_pb2.CameraControlValues(
                                enumerated=hal_pb2.CameraControlEnumValues(
                                    values=[
                                        hal_pb2.CameraControlValue(enum_value="daylight"),
                                        hal_pb2.CameraControlValue(enum_value="tungsten"),
                                    ]
                                )
                            ),
                            current_value_available=False,
                        ),
                    ]
                ),
            ).SerializeToString(),
        )
        request = hal_pb2.Envelope.FromString(await read_frame(reader))
        assert request.WhichOneof("payload") == "camera_get_control_request"
        await send_frame(
            writer,
            envelope(
                request.request_id,
                "camera_get_control_response",
                hal_pb2.CameraGetControlResponse(
                    value=hal_pb2.CameraControlValue(integer_value=42)
                ),
            ).SerializeToString(),
        )
        request = hal_pb2.Envelope.FromString(await read_frame(reader))
        assert request.WhichOneof("payload") == "camera_set_control_request"
        assert request.camera_set_control_request.value.integer_value == 43
        await send_frame(
            writer,
            envelope(
                request.request_id, "camera_set_control_response", hal_pb2.Empty()
            ).SerializeToString(),
        )
        request = hal_pb2.Envelope.FromString(await read_frame(reader))
        assert request.WhichOneof("payload") == "camera_set_auto_request"
        await send_frame(
            writer,
            envelope(
                request.request_id, "camera_set_auto_response", hal_pb2.Empty()
            ).SerializeToString(),
        )
        request = hal_pb2.Envelope.FromString(await read_frame(reader))
        assert request.WhichOneof("payload") == "close_camera_request"
        await send_frame(
            writer,
            envelope(
                request.request_id, "close_camera_response", hal_pb2.Empty()
            ).SerializeToString(),
        )

    async with fake_broker(
        handler,
        selected_minor=3,
        minimum_minor=3,
        maximum_minor=3,
        capabilities=["serial.bytes/v1", *capabilities],
    ) as endpoint:
        from seeed_hal import HalClient

        client = await HalClient.connect(endpoint, TOKEN)
        resources = await client.enumerate_camera()
        session = await client.open_camera(
            resources[0].selector(), CameraFormat(PixelFormat.NV12, 640, 480)
        )
        await session.capture(1)
        descriptor = await session.mapping_descriptor()
        assert b"t" * 32 not in repr(descriptor).encode()
        session._mapping_reader = _BytesMappingReader()
        lease = await session.next_frame_lease()
        assert lease == FrameLease(0, 1, 1)
        frame = await session.next_frame()
        assert frame is not None
        assert frame.copy_bytes() == b"frame"
        controls = await session.controls()
        assert controls == [
            CameraControlDescriptor(
                kind=seeed_hal.ControlKind.EXPOSURE,
                readable=True,
                writable=True,
                auto_supported=True,
                values=ControlRange(minimum=1, maximum=99, step=1),
                current_value_available=True,
                diagnostic="virtual camera",
            ),
            CameraControlDescriptor(
                kind=seeed_hal.ControlKind.WHITE_BALANCE,
                readable=True,
                writable=True,
                auto_supported=False,
                values=ControlEnumValues(
                    values=(
                        seeed_hal.ControlValue(enum="daylight"),
                        seeed_hal.ControlValue(enum="tungsten"),
                    )
                ),
                current_value_available=False,
            ),
        ]
        assert isinstance(controls[0].values, ControlRange)
        assert isinstance(controls[1].values, ControlEnumValues)
        assert (await session.get_control(seeed_hal.ControlKind.EXPOSURE)).integer == 42
        await session.set_control(seeed_hal.ControlKind.EXPOSURE, seeed_hal.ControlValue(integer=43))
        await session.set_auto(seeed_hal.ControlKind.EXPOSURE, True)
        await session.close()
        await client.close()


@pytest.mark.asyncio
async def test_malformed_camera_controls_response_terminates_the_python_client() -> None:
    capabilities = ["camera.capture/v1", "camera.controls/v1"]
    release = asyncio.Event()

    async def handler(reader, writer):
        request = hal_pb2.Envelope.FromString(await read_frame(reader))
        assert request.WhichOneof("payload") == "enumerate_camera_request"
        await send_frame(
            writer,
            envelope(
                request.request_id,
                "enumerate_camera_response",
                hal_pb2.EnumerateCameraResponse(
                    resources=[
                        hal_pb2.ResourceDescriptor(
                            resource_id="camera:virtual:bad-controls",
                            endpoint="virtual://camera:bad-controls",
                            identity_quality=hal_pb2.IDENTITY_QUALITY_STRONG,
                            transport=hal_pb2.TRANSPORT_KIND_CAMERA,
                            capabilities=capabilities,
                        )
                    ]
                ),
            ).SerializeToString(),
        )
        request = hal_pb2.Envelope.FromString(await read_frame(reader))
        assert request.WhichOneof("payload") == "open_camera_request"
        await send_frame(
            writer,
            envelope(
                request.request_id,
                "open_camera_response",
                hal_pb2.OpenCameraResponse(
                    session_id="camera-bad-controls",
                    lease=hal_pb2.LeaseToken(
                        lease_id="camera-bad-controls-lease",
                        generation=1,
                        mode=hal_pb2.LEASE_MODE_CONTROL,
                    ),
                ),
            ).SerializeToString(),
        )
        request = hal_pb2.Envelope.FromString(await read_frame(reader))
        assert request.WhichOneof("payload") == "camera_controls_request"
        await send_frame(
            writer,
            envelope(
                request.request_id,
                "camera_controls_response",
                hal_pb2.CameraControlsResponse(
                    controls=[
                        hal_pb2.CameraControlDescriptor(
                            kind=999,
                            values=hal_pb2.CameraControlValues(
                                range=hal_pb2.CameraControlRange(
                                    minimum=1, maximum=2, step=1
                                )
                            ),
                        )
                    ]
                ),
            ).SerializeToString(),
        )
        await release.wait()

    async with fake_broker(
        handler,
        selected_minor=3,
        minimum_minor=3,
        maximum_minor=3,
        capabilities=["serial.bytes/v1", *capabilities],
    ) as endpoint:
        from seeed_hal import HalClient

        client = await HalClient.connect(endpoint, TOKEN)
        resource = (await client.enumerate_camera())[0]
        session = await client.open_camera(
            resource.selector(), CameraFormat(PixelFormat.NV12, 640, 480)
        )
        with pytest.raises(HalError, match="runtime.protocol.invalid_message"):
            await session.controls()
        assert client._terminal is not None
        assert client._terminal.name == "runtime.protocol.invalid_message"
        await client.close()
        release.set()
