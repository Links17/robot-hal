from __future__ import annotations

from dataclasses import FrozenInstanceError

import pytest

import seeed_hal
from seeed_hal import (
    BorrowedFrame,
    CameraFormat,
    CameraSession,
    FrameLease,
    MappingDescriptor,
    PixelFormat,
)
from seeed_hal.errors import HalError
from seeed_hal.proto import hal_pb2
from test_client_contract import TOKEN, envelope, fake_broker, read_frame, send_frame


def test_camera_public_values_redact_tokens_and_invalidate_borrowed_frames() -> None:
    assert {
        "BorrowedFrame",
        "CameraFormat",
        "CameraSession",
        "FrameLease",
        "MappingDescriptor",
        "PixelFormat",
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

    session = CameraSession(None, "camera-session", "camera-lease", 1, "camera:virtual:test")
    frame = BorrowedFrame(session, FrameLease(0, 1, 1), lambda: b"frame")
    assert frame.copy_bytes() == b"frame"
    session._invalidate()
    with pytest.raises(HalError, match="runtime.lease.stale_generation"):
        frame.copy_bytes()


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
        assert await session.next_frame_lease() == FrameLease(0, 1, 1)
        assert (await session.get_control(seeed_hal.ControlKind.EXPOSURE)).integer == 42
        await session.set_control(seeed_hal.ControlKind.EXPOSURE, seeed_hal.ControlValue(integer=43))
        await session.set_auto(seeed_hal.ControlKind.EXPOSURE, True)
        await session.close()
        await client.close()
