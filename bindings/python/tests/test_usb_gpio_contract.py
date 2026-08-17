from __future__ import annotations

import asyncio
from dataclasses import FrozenInstanceError

import pytest

import seeed_hal
from seeed_hal import (
    ErrorCategory,
    GpioBias,
    GpioDirection,
    GpioDrive,
    GpioEdge,
    GpioEdgeRequest,
    GpioLineConfig,
    HalClient,
    HalError,
    IdentityQuality,
    ResourceSelector,
    TransportKind,
    UsbTransfer,
    UsbTransferKind,
)
from seeed_hal.proto import hal_pb2

from test_client_contract import TOKEN, envelope, fake_broker, read_frame, send_frame
from test_client_hardening import ScriptedTransport


def _descriptor(resource_id: str, transport: int, capabilities: list[str]) -> hal_pb2.ResourceDescriptor:
    return hal_pb2.ResourceDescriptor(
        resource_id=resource_id,
        endpoint=f"virtual://{resource_id}",
        identity_quality=hal_pb2.IDENTITY_QUALITY_STRONG,
        transport=transport,
        capabilities=capabilities,
    )


def test_usb_gpio_models_are_public_immutable_and_validate() -> None:
    assert {
        "UsbSession",
        "UsbTransfer",
        "UsbTransferKind",
        "GpioBias",
        "GpioDirection",
        "GpioDrive",
        "GpioEdge",
        "GpioEdgeEvent",
        "GpioEdgeRequest",
        "GpioLineConfig",
        "GpioSession",
    }.issubset(seeed_hal.__all__)

    transfer = UsbTransfer(
        UsbTransferKind.BULK_OUT, endpoint=1, data=bytearray(b"out")
    )
    assert transfer.data == b"out"
    with pytest.raises(FrozenInstanceError):
        transfer.data = b"changed"  # type: ignore[misc]
    with pytest.raises(HalError, match="usb.transfer.invalid"):
        UsbTransfer(UsbTransferKind.BULK_IN, endpoint=1)

    config = GpioLineConfig(
        GpioDirection.OUTPUT,
        bias=GpioBias.PULL_UP,
        drive=GpioDrive.PUSH_PULL,
        initial_value=False,
    )
    with pytest.raises(FrozenInstanceError):
        config.active_low = True  # type: ignore[misc]
    with pytest.raises(HalError, match="gpio.configuration.invalid"):
        GpioEdgeRequest(False, False, 1)


@pytest.mark.asyncio
async def test_usb_gpio_reject_minor_zero_and_one_locally() -> None:
    transport = ScriptedTransport()
    for minor in (0, 1):
        client = HalClient(
            transport,
            protocol_minor=minor,
            capabilities=frozenset(
                {
                    "serial.bytes/v1",
                    "usb.control/v1",
                    "gpio.lines/v1",
                }
            ),
            frame_limit=1024 * 1024,
            read_limit=64 * 1024,
            write_limit=64 * 1024,
            pending_capacity=32,
            writer_capacity=32,
            event_capacity=64,
        )
        selector = ResourceSelector(
            "usb:virtual:python", IdentityQuality.STRONG, TransportKind.USB
        )
        with pytest.raises(HalError) as usb_error:
            await client.open_usb(selector, 0)
        assert usb_error.value.name == "runtime.protocol.capability_unsupported"
        with pytest.raises(HalError) as gpio_error:
            await client.enumerate_gpio()
        assert gpio_error.value.name == "runtime.protocol.capability_unsupported"
        await client.close()


@pytest.mark.asyncio
async def test_usb_gpio_fake_broker_wires_requests_responses_and_close() -> None:
    async def handler(reader, writer):
        request = hal_pb2.Envelope.FromString(await read_frame(reader))
        assert request.WhichOneof("payload") == "enumerate_usb_request"
        await send_frame(
            writer,
            envelope(
                request.request_id,
                "enumerate_usb_response",
                hal_pb2.EnumerateUsbResponse(
                    resources=[
                        _descriptor(
                            "usb:virtual:python",
                            hal_pb2.TRANSPORT_KIND_USB,
                            ["usb.control/v1", "usb.bulk/v1", "usb.interrupt/v1"],
                        )
                    ]
                ),
            ).SerializeToString(),
        )
        request = hal_pb2.Envelope.FromString(await read_frame(reader))
        assert request.WhichOneof("payload") == "open_usb_request"
        await send_frame(
            writer,
            envelope(
                request.request_id,
                "open_usb_response",
                hal_pb2.OpenUsbResponse(
                    session_id="usb-session",
                    lease=hal_pb2.LeaseToken(
                        lease_id="usb-lease",
                        generation=1,
                        mode=hal_pb2.LEASE_MODE_CONTROL,
                    ),
                ),
            ).SerializeToString(),
        )
        request = hal_pb2.Envelope.FromString(await read_frame(reader))
        assert request.usb_transfer_request.kind == hal_pb2.USB_TRANSFER_KIND_BULK_OUT
        await send_frame(
            writer,
            envelope(
                request.request_id,
                "usb_transfer_response",
                hal_pb2.UsbTransferResponse(),
            ).SerializeToString(),
        )
        request = hal_pb2.Envelope.FromString(await read_frame(reader))
        assert request.WhichOneof("payload") == "close_usb_request"
        await send_frame(
            writer,
            envelope(request.request_id, "close_usb_response", hal_pb2.Empty()).SerializeToString(),
        )
        request = hal_pb2.Envelope.FromString(await read_frame(reader))
        assert request.WhichOneof("payload") == "enumerate_gpio_request"
        await send_frame(
            writer,
            envelope(
                request.request_id,
                "enumerate_gpio_response",
                hal_pb2.EnumerateGpioResponse(
                    resources=[
                        _descriptor(
                            "gpio:virtual:python",
                            hal_pb2.TRANSPORT_KIND_GPIO,
                            ["gpio.lines/v1", "gpio.edges/v1"],
                        )
                    ]
                ),
            ).SerializeToString(),
        )
        request = hal_pb2.Envelope.FromString(await read_frame(reader))
        assert request.WhichOneof("payload") == "open_gpio_request"
        await send_frame(
            writer,
            envelope(
                request.request_id,
                "open_gpio_response",
                hal_pb2.OpenGpioResponse(
                    session_id="gpio-session",
                    lease=hal_pb2.LeaseToken(
                        lease_id="gpio-lease",
                        generation=1,
                        mode=hal_pb2.LEASE_MODE_CONTROL,
                    ),
                ),
            ).SerializeToString(),
        )
        request = hal_pb2.Envelope.FromString(await read_frame(reader))
        assert request.WhichOneof("payload") == "gpio_write_request"
        await send_frame(
            writer,
            envelope(request.request_id, "gpio_write_response", hal_pb2.Empty()).SerializeToString(),
        )
        request = hal_pb2.Envelope.FromString(await read_frame(reader))
        assert request.WhichOneof("payload") == "gpio_read_request"
        await send_frame(
            writer,
            envelope(
                request.request_id,
                "gpio_read_response",
                hal_pb2.GpioReadResponse(values=[True]),
            ).SerializeToString(),
        )
        request = hal_pb2.Envelope.FromString(await read_frame(reader))
        assert request.WhichOneof("payload") == "gpio_next_edge_request"
        await send_frame(
            writer,
            envelope(
                request.request_id,
                "gpio_next_edge_response",
                hal_pb2.GpioNextEdgeResponse(
                    event=hal_pb2.GpioEdgeEvent(
                        edge=hal_pb2.GPIO_EDGE_RISING, monotonic_ns=1, sequence=1
                    )
                ),
            ).SerializeToString(),
        )
        request = hal_pb2.Envelope.FromString(await read_frame(reader))
        assert request.WhichOneof("payload") == "close_gpio_request"
        await send_frame(
            writer,
            envelope(request.request_id, "close_gpio_response", hal_pb2.Empty()).SerializeToString(),
        )

    async with fake_broker(
        handler,
        selected_minor=2,
        minimum_minor=2,
        maximum_minor=2,
        capabilities=[
            "serial.bytes/v1",
            "usb.control/v1",
            "usb.bulk/v1",
            "usb.interrupt/v1",
            "gpio.lines/v1",
            "gpio.edges/v1",
        ],
    ) as endpoint:
        client = await HalClient.connect(endpoint, TOKEN)
        usb = await client.open_usb(
            (await client.enumerate_usb())[0].selector(), interface_number=0
        )
        assert await usb.transfer(
            UsbTransfer(UsbTransferKind.BULK_OUT, endpoint=1, data=b"out"), 1
        ) == b""
        await usb.close()
        await usb.close()
        with pytest.raises(HalError, match="runtime.session.closed"):
            await usb.transfer(
                UsbTransfer(UsbTransferKind.BULK_OUT, endpoint=1, data=b"out"), 1
            )

        gpio = await client.open_gpio(
            (await client.enumerate_gpio())[0].selector(),
            lines=(0,),
            config=GpioLineConfig(
                GpioDirection.OUTPUT,
                drive=GpioDrive.PUSH_PULL,
                initial_value=False,
            ),
        )
        await gpio.write((True,))
        assert await gpio.read() == (True,)
        edge = await gpio.next_edge(GpioEdgeRequest(True, False, 1), 1)
        assert edge is not None and edge.edge is GpioEdge.RISING
        await gpio.close()
        await gpio.close()
        await client.close()


@pytest.mark.asyncio
async def test_malformed_usb_expected_response_closes_connection() -> None:
    async def handler(reader, writer):
        request = hal_pb2.Envelope.FromString(await read_frame(reader))
        await send_frame(
            writer,
            envelope(
                request.request_id,
                "open_usb_response",
                hal_pb2.OpenUsbResponse(),
            ).SerializeToString(),
        )
        await asyncio.sleep(0.01)

    async with fake_broker(
        handler,
        selected_minor=2,
        minimum_minor=2,
        maximum_minor=2,
        capabilities=["serial.bytes/v1", "usb.control/v1"],
    ) as endpoint:
        client = await HalClient.connect(endpoint, TOKEN)
        selector = ResourceSelector(
            "usb:virtual:python", IdentityQuality.STRONG, TransportKind.USB
        )
        with pytest.raises(HalError) as caught:
            await client.open_usb(selector, 0)
        assert caught.value.name == "runtime.protocol.invalid_message"
        with pytest.raises(HalError) as repeated:
            await client.enumerate_usb()
        assert repeated.value.name == "runtime.protocol.invalid_message"
        await client.close()
