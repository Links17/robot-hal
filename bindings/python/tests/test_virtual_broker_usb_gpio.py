from __future__ import annotations

import pytest

from robot_hal import (
    GpioDirection,
    GpioDrive,
    GpioEdgeRequest,
    GpioLineConfig,
    HalClient,
    TransportKind,
    UsbTransfer,
    UsbTransferKind,
)


@pytest.mark.asyncio
async def test_virtual_broker_exercises_usb_and_gpio_sessions(broker) -> None:
    client = await HalClient.connect(broker.endpoint, broker.token)
    try:
        usb_resources = await client.enumerate_usb()
        assert [resource.resource_id for resource in usb_resources] == [
            "usb:virtual:python"
        ]
        assert usb_resources[0].transport is TransportKind.USB

        usb = await client.open_usb(usb_resources[0].selector(), interface_number=0)
        payload = b"python-virtual-usb"
        assert (
            await usb.transfer(
                UsbTransfer(UsbTransferKind.BULK_OUT, endpoint=1, data=payload),
                timeout_ms=100,
            )
            == b""
        )
        assert await usb.transfer(
            UsbTransfer(
                UsbTransferKind.BULK_IN,
                endpoint=0x81,
                max_bytes=len(payload),
            ),
            timeout_ms=100,
        ) == payload
        await usb.close()

        reused_usb = await client.open_usb(usb_resources[0].selector(), interface_number=0)
        await reused_usb.close()

        gpio_resources = await client.enumerate_gpio()
        assert [resource.resource_id for resource in gpio_resources] == [
            "gpio:virtual:python"
        ]
        assert gpio_resources[0].transport is TransportKind.GPIO

        config = GpioLineConfig(
            GpioDirection.OUTPUT,
            drive=GpioDrive.PUSH_PULL,
            initial_value=False,
        )
        gpio = await client.open_gpio(gpio_resources[0].selector(), lines=(0,), config=config)
        await gpio.write((True,))
        assert await gpio.read() == (True,)
        assert (
            await gpio.next_edge(
                GpioEdgeRequest(rising=True, falling=False, capacity=1), timeout_ms=100
            )
            is None
        )
        await gpio.close()

        reused_gpio = await client.open_gpio(
            gpio_resources[0].selector(), lines=(0,), config=config
        )
        await reused_gpio.close()
    finally:
        await client.close()
