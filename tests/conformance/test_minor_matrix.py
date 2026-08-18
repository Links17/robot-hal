from __future__ import annotations

import importlib.util
from pathlib import Path

import pytest


RUNNER = Path(__file__).with_name("run-broker-conformance.py")


def load_runner():
    spec = importlib.util.spec_from_file_location("broker_conformance_minor_matrix", RUNNER)
    assert spec is not None and spec.loader is not None
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


def test_capability_matrix_matches_protocol_introduction_minors() -> None:
    runner = load_runner()

    assert runner.capabilities_for_minor(0) == (runner.SERIAL_CAPABILITY,)
    assert runner.capabilities_for_minor(1) == (
        runner.SERIAL_CAPABILITY,
        runner.CAN_CLASSIC_CAPABILITY,
        runner.CAN_FD_CAPABILITY,
        runner.CAN_CONFIGURE_CAPABILITY,
        runner.CAN_ERROR_FRAMES_CAPABILITY,
        runner.CAN_RX_TIMESTAMP_CAPABILITY,
    )
    assert runner.capabilities_for_minor(2) == (
        runner.SERIAL_CAPABILITY,
        runner.CAN_CLASSIC_CAPABILITY,
        runner.CAN_FD_CAPABILITY,
        runner.CAN_CONFIGURE_CAPABILITY,
        runner.CAN_ERROR_FRAMES_CAPABILITY,
        runner.CAN_RX_TIMESTAMP_CAPABILITY,
        runner.USB_CONTROL_CAPABILITY,
        runner.USB_BULK_CAPABILITY,
        runner.USB_INTERRUPT_CAPABILITY,
        runner.GPIO_LINES_CAPABILITY,
        runner.GPIO_EDGES_CAPABILITY,
    )
    assert runner.capabilities_for_minor(3) == (
        runner.SERIAL_CAPABILITY,
        runner.CAN_CLASSIC_CAPABILITY,
        runner.CAN_FD_CAPABILITY,
        runner.CAN_CONFIGURE_CAPABILITY,
        runner.CAN_ERROR_FRAMES_CAPABILITY,
        runner.CAN_RX_TIMESTAMP_CAPABILITY,
        runner.USB_CONTROL_CAPABILITY,
        runner.USB_BULK_CAPABILITY,
        runner.USB_INTERRUPT_CAPABILITY,
        runner.GPIO_LINES_CAPABILITY,
        runner.GPIO_EDGES_CAPABILITY,
        runner.CAMERA_CAPTURE_CAPABILITY,
        runner.CAMERA_FRAMES_SHM_CAPABILITY,
        runner.CAMERA_CONTROLS_CAPABILITY,
    )


@pytest.mark.parametrize(
    ("minor", "later_payload"),
    [
        (0, "enumerate_can_request"),
        (1, "enumerate_usb_request"),
        (2, "enumerate_camera_request"),
        (3, None),
    ],
)
def test_lower_minor_selects_exactly_one_later_operation_probe(
    minor: int, later_payload: str | None
) -> None:
    runner = load_runner()

    assert runner.later_operation_for_minor(minor) == later_payload


@pytest.mark.parametrize("minor", [-1, 4])
def test_capability_profile_rejects_unsupported_minor(minor: int) -> None:
    runner = load_runner()

    with pytest.raises(ValueError, match="protocol minor"):
        runner.capabilities_for_minor(minor)


def test_later_operation_probe_requires_each_dispatcher_stable_error() -> None:
    runner = load_runner()

    assert runner.later_operation_error_for_minor(0) == (
        "runtime.protocol.capability_unsupported"
    )
    assert runner.later_operation_error_for_minor(1) == (
        "runtime.protocol.unsupported_capability"
    )
    assert runner.later_operation_error_for_minor(2) == (
        "runtime.protocol.unsupported_capability"
    )


def test_explicit_narrow_profile_selects_only_advertised_operations() -> None:
    runner = load_runner()

    assert runner.operations_for_profile(
        3,
        (
            runner.SERIAL_CAPABILITY,
            runner.USB_CONTROL_CAPABILITY,
            runner.CAMERA_CAPTURE_CAPABILITY,
            runner.CAMERA_FRAMES_SHM_CAPABILITY,
        ),
    ) == (
        "serial",
        "usb.control",
        "camera.capture",
        "camera.frames",
    )


def test_default_profile_selects_every_operation_for_the_minor() -> None:
    runner = load_runner()

    assert runner.operations_for_profile(
        3, runner.capabilities_for_minor(3)
    ) == (
        "serial",
        "can.classic",
        "can.fd",
        "can.configure",
        "can.error-frames",
        "can.rx-timestamp",
        "usb.control",
        "usb.bulk",
        "usb.interrupt",
        "gpio.lines",
        "gpio.edges",
        "camera.capture",
        "camera.frames",
        "camera.controls",
    )


def test_dependent_operations_require_their_lifecycle_capability() -> None:
    runner = load_runner()

    assert runner.operations_for_profile(
        3,
        (
            runner.GPIO_EDGES_CAPABILITY,
            runner.CAMERA_FRAMES_SHM_CAPABILITY,
            runner.CAMERA_CONTROLS_CAPABILITY,
        ),
    ) == ()


@pytest.mark.parametrize(
    ("requested", "closed"),
    [
        (("usb.bulk/v1",), ("usb.control/v1", "usb.bulk/v1")),
        (("usb.interrupt/v1",), ("usb.control/v1", "usb.interrupt/v1")),
        (("gpio.edges/v1",), ("gpio.lines/v1", "gpio.edges/v1")),
        (
            ("camera.frames.shm/v1",),
            ("camera.capture/v1", "camera.frames.shm/v1"),
        ),
        (
            ("camera.controls/v1",),
            ("camera.capture/v1", "camera.controls/v1"),
        ),
        (("can.configure/v1",), ("can.classic/v1", "can.configure/v1")),
        (("can.error-frames/v1",), ("can.classic/v1", "can.error-frames/v1")),
        (("can.rx-timestamp/v1",), ("can.classic/v1", "can.rx-timestamp/v1")),
    ],
)
def test_explicit_capability_requirements_add_lifecycle_dependency_closure(
    requested: tuple[str, ...], closed: tuple[str, ...]
) -> None:
    runner = load_runner()

    assert runner.required_capabilities_for_run(3, requested) == closed


def test_can_fd_requirement_has_no_classic_or_configure_dependency() -> None:
    runner = load_runner()

    assert runner.required_capabilities_for_run(
        1, (runner.CAN_FD_CAPABILITY,)
    ) == (runner.CAN_FD_CAPABILITY,)

    assert runner.operations_for_profile(
        1, (runner.CAN_FD_CAPABILITY,)
    ) == ("can.fd",)


def test_can_modes_and_optional_capabilities_select_distinct_operations() -> None:
    runner = load_runner()

    assert runner.operations_for_profile(
        1,
        (
            runner.CAN_CLASSIC_CAPABILITY,
            runner.CAN_FD_CAPABILITY,
            runner.CAN_CONFIGURE_CAPABILITY,
            runner.CAN_ERROR_FRAMES_CAPABILITY,
            runner.CAN_RX_TIMESTAMP_CAPABILITY,
        ),
    ) == (
        "can.classic",
        "can.fd",
        "can.configure",
        "can.error-frames",
        "can.rx-timestamp",
    )


@pytest.mark.parametrize(
    ("capabilities", "payload"),
    [
        (("serial.bytes/v1",), "enumerate_serial_request"),
        (("can.classic/v1",), "enumerate_can_request"),
        (("usb.control/v1",), "enumerate_usb_request"),
        (("gpio.lines/v1",), "enumerate_gpio_request"),
        (("camera.capture/v1",), "enumerate_camera_request"),
    ],
)
def test_post_probe_health_request_uses_active_transport(
    capabilities: tuple[str, ...], payload: str
) -> None:
    runner = load_runner()

    assert runner.health_operation_for_profile(capabilities) == payload


@pytest.mark.parametrize(
    ("capabilities", "transport"),
    [
        (("serial.bytes/v1", "can.classic/v1"), "serial"),
        (("can.classic/v1",), "can"),
        (("usb.control/v1", "usb.bulk/v1"), "usb"),
        (("gpio.lines/v1",), "gpio"),
        (("camera.capture/v1",), "camera"),
    ],
)
def test_cleanup_transport_is_selected_from_active_profile(
    capabilities: tuple[str, ...], transport: str
) -> None:
    runner = load_runner()

    assert runner.cleanup_transport_for_profile(capabilities) == transport
