#!/usr/bin/env python3
"""Hardware-free black-box conformance for a virtual-adapter broker build."""

from __future__ import annotations

import argparse
import asyncio
import contextlib
import json
import os
from pathlib import Path
import secrets
import signal
import subprocess
import sys
import tempfile
import uuid


REPO_ROOT = Path(__file__).resolve().parents[2]
PYTHON_BINDINGS = REPO_ROOT / "bindings" / "python"
sys.path.insert(0, str(PYTHON_BINDINGS))

from seeed_hal.proto import hal_pb2  # noqa: E402
from seeed_hal.transport_unix import UnixFramedTransport  # noqa: E402


PROTOCOL_MAJOR = 1
PROTOCOL_MINOR = 0
SERIAL_CAPABILITY = "serial.bytes/v1"
TRANSFER_LIMIT = 64 * 1024
FRAME_LIMIT = 1024 * 1024


def endpoint_for_platform(directory: Path, nonce: str, os_name: str) -> str:
    if os_name == "nt":
        return rf"\\.\pipe\seeed-hal-conformance-{nonce}"
    return str(directory / "broker.sock")


def broker_command(broker: Path, endpoint: str, token_path: Path) -> list[str]:
    return [
        str(broker),
        "--endpoint",
        endpoint,
        "--auth-token-file",
        str(token_path),
    ]


def parse_readiness_endpoint(line: bytes) -> str:
    try:
        readiness = json.loads(line)
    except (UnicodeDecodeError, json.JSONDecodeError) as error:
        raise AssertionError(f"invalid broker readiness JSON: {error}") from error
    _require(readiness.get("status") == "ready", "broker did not report ready status")
    endpoint = readiness.get("endpoint")
    _require(isinstance(endpoint, str), "broker readiness endpoint was not a string")
    return endpoint


async def connect_transport(endpoint: str):
    if os.name == "nt":
        from seeed_hal.transport_windows import WindowsFramedTransport

        return await WindowsFramedTransport.connect(endpoint, FRAME_LIMIT)
    return await UnixFramedTransport.connect(endpoint, FRAME_LIMIT)


class RawClient:
    def __init__(self, transport, timeout: float) -> None:
        self.transport = transport
        self.timeout = timeout
        self.next_request_id = 1

    async def request(self, payload_name: str, payload):
        request_id = self.next_request_id
        self.next_request_id += 1
        envelope = hal_pb2.Envelope(request_id=request_id)
        getattr(envelope, payload_name).CopyFrom(payload)
        await asyncio.wait_for(
            self.transport.send(envelope.SerializeToString()), self.timeout
        )
        while True:
            frame = await asyncio.wait_for(self.transport.receive(), self.timeout)
            response = hal_pb2.Envelope()
            response.ParseFromString(frame)
            if response.request_id == request_id:
                return response
            _require(
                response.request_id == 0
                and response.WhichOneof("payload") == "runtime_event",
                f"unexpected response correlation id {response.request_id}",
            )

    async def handshake(self, token: bytes) -> None:
        response = await self.request(
            "handshake_request",
            hal_pb2.HandshakeRequest(
                startup_token=token,
                protocol_major=PROTOCOL_MAJOR,
                protocol_minor=PROTOCOL_MINOR,
                required_capabilities=[SERIAL_CAPABILITY],
                max_frame_bytes=FRAME_LIMIT,
                max_read_bytes=TRANSFER_LIMIT,
                max_write_bytes=TRANSFER_LIMIT,
            ),
        )
        _expect_payload(response, "handshake_response")
        handshake = response.handshake_response
        _require(handshake.protocol_major == PROTOCOL_MAJOR, "protocol major mismatch")
        _require(SERIAL_CAPABILITY in handshake.capabilities, "Serial capability missing")
        self.transport.set_frame_limit(handshake.max_frame_bytes)

    async def close(self) -> None:
        await asyncio.wait_for(self.transport.close(), self.timeout)


def _require(condition: bool, message: str) -> None:
    if not condition:
        raise AssertionError(message)


def _expect_payload(envelope, expected: str):
    actual = envelope.WhichOneof("payload")
    if actual == "error":
        error = envelope.error
        raise AssertionError(f"{error.name} during {error.operation}: {error.debug_message}")
    _require(actual == expected, f"expected {expected}, received {actual}")
    return getattr(envelope, expected)


def _lease_copy(lease):
    result = hal_pb2.LeaseToken()
    result.CopyFrom(lease)
    return result


def _selector(descriptor):
    return hal_pb2.ResourceSelector(
        resource_id=descriptor.resource_id,
        minimum_identity_quality=descriptor.identity_quality,
        transport=descriptor.transport,
    )


def _serial_config():
    return hal_pb2.SerialConfig(
        baud_rate=115200,
        data_bits=hal_pb2.DATA_BITS_EIGHT,
        parity=hal_pb2.PARITY_NONE,
        stop_bits=hal_pb2.STOP_BITS_ONE,
        flow_control=hal_pb2.FLOW_CONTROL_NONE,
        read_timeout_ms=250,
    )


async def _open_serial(client: RawClient, descriptor):
    return await client.request(
        "open_serial_request",
        hal_pb2.OpenSerialRequest(
            selector=_selector(descriptor), config=_serial_config()
        ),
    )


async def exercise_contract(endpoint: str, token: bytes, timeout: float) -> None:
    first = RawClient(await connect_transport(endpoint), timeout)
    second = None
    try:
        await first.handshake(token)
        enumerated = await first.request(
            "enumerate_serial_request", hal_pb2.EnumerateSerialRequest()
        )
        resources = _expect_payload(enumerated, "enumerate_serial_response").resources
        _require(len(resources) == 1, f"expected one virtual Serial resource, got {len(resources)}")
        descriptor = resources[0]
        _require(
            descriptor.properties.get("adapter") == "virtual",
            "broker was not built with the virtual adapter",
        )

        opened = _expect_payload(await _open_serial(first, descriptor), "open_serial_response")
        first_session = opened.session_id
        first_lease = _lease_copy(opened.lease)
        _require(first_lease.generation > 0, "lease generation must be nonzero")

        payload = b"seeed-hal-black-box"
        _expect_payload(
            await first.request(
                "serial_write_request",
                hal_pb2.SerialWriteRequest(
                    session_id=first_session, lease=first_lease, data=payload
                ),
            ),
            "serial_write_response",
        )
        _expect_payload(
            await first.request(
                "serial_flush_request",
                hal_pb2.SerialFlushRequest(session_id=first_session, lease=first_lease),
            ),
            "serial_flush_response",
        )
        _expect_payload(
            await first.request(
                "set_serial_control_lines_request",
                hal_pb2.SetSerialControlLinesRequest(
                    session_id=first_session,
                    lease=first_lease,
                    data_terminal_ready=True,
                    request_to_send=True,
                ),
            ),
            "set_serial_control_lines_response",
        )
        read = _expect_payload(
            await first.request(
                "serial_read_request",
                hal_pb2.SerialReadRequest(
                    session_id=first_session,
                    lease=first_lease,
                    max_bytes=len(payload),
                ),
            ),
            "serial_read_response",
        )
        _require(read.data == payload, "virtual Serial round trip changed payload bytes")

        _expect_payload(
            await first.request(
                "close_session_request",
                hal_pb2.CloseSessionRequest(
                    session_id=first_session, lease=first_lease
                ),
            ),
            "close_session_response",
        )
        reopened = _expect_payload(await _open_serial(first, descriptor), "open_serial_response")
        _require(
            reopened.lease.generation > first_lease.generation,
            "resource reuse did not advance the fencing generation",
        )
        stale = await first.request(
            "serial_write_request",
            hal_pb2.SerialWriteRequest(
                session_id=reopened.session_id, lease=first_lease, data=b"stale"
            ),
        )
        _require(stale.WhichOneof("payload") == "error", "stale lease was accepted")
        _require(
            stale.error.name == "runtime.lease.stale_generation",
            f"unexpected stale lease error {stale.error.name}",
        )

        # Abruptly disconnect with the reopened session still owned. A fresh owner
        # must be able to reuse the resource after broker cleanup completes.
        await first.close()
        second = RawClient(await connect_transport(endpoint), timeout)
        await second.handshake(token)
        deadline = asyncio.get_running_loop().time() + timeout
        while True:
            response = await _open_serial(second, descriptor)
            if response.WhichOneof("payload") == "open_serial_response":
                break
            _require(
                response.WhichOneof("payload") == "error"
                and response.error.name == "runtime.lease.conflict",
                f"unexpected cleanup response {response.WhichOneof('payload')}",
            )
            if asyncio.get_running_loop().time() >= deadline:
                raise AssertionError("disconnect cleanup did not release the resource")
            await asyncio.sleep(0.025)
    finally:
        with contextlib.suppress(Exception):
            await first.close()
        if second is not None:
            with contextlib.suppress(Exception):
                await second.close()


async def _wait_for_readiness(process, endpoint: str, timeout: float) -> None:
    _require(process.stdout is not None, "broker stdout was not captured")
    line = await asyncio.wait_for(process.stdout.readline(), timeout)
    if not line:
        raise AssertionError("broker exited before publishing readiness")
    reported_endpoint = parse_readiness_endpoint(line)
    _require(reported_endpoint == endpoint, "broker readiness named a different endpoint")


async def _graceful_shutdown(process, timeout: float) -> None:
    if process.returncode is not None:
        raise AssertionError(f"broker exited early with status {process.returncode}")
    if os.name == "nt":
        process.send_signal(signal.CTRL_BREAK_EVENT)
    else:
        process.send_signal(signal.SIGINT)
    await asyncio.wait_for(process.wait(), timeout)
    _require(process.returncode == 0, f"broker shutdown returned {process.returncode}")


async def run(broker: Path, timeout: float) -> None:
    broker = broker.resolve()
    _require(broker.is_file(), f"broker executable not found: {broker}")
    with tempfile.TemporaryDirectory(prefix="seeed-hal-conformance-") as temporary:
        directory = Path(temporary)
        if os.name != "nt":
            directory.chmod(0o700)
        nonce = uuid.uuid4().hex
        endpoint = endpoint_for_platform(directory, nonce, os.name)
        token_path = directory / "startup-token"
        token = secrets.token_bytes(32)
        token_path.write_bytes(token)
        if os.name != "nt":
            token_path.chmod(0o600)
        creationflags = subprocess.CREATE_NEW_PROCESS_GROUP if os.name == "nt" else 0
        process = await asyncio.create_subprocess_exec(
            *broker_command(broker, endpoint, token_path),
            stdout=asyncio.subprocess.PIPE,
            stderr=asyncio.subprocess.PIPE,
            creationflags=creationflags,
        )
        try:
            await _wait_for_readiness(process, endpoint, timeout)
            await exercise_contract(endpoint, token, timeout)
            await _graceful_shutdown(process, timeout)
        finally:
            if process.returncode is None:
                process.kill()
                with contextlib.suppress(asyncio.TimeoutError):
                    await asyncio.wait_for(process.wait(), timeout)
            if process.stderr is not None:
                stderr = (await process.stderr.read()).decode("utf-8", errors="replace")
                if process.returncode not in (0, None) and stderr:
                    print(stderr, file=sys.stderr, end="")


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--broker", type=Path, required=True)
    parser.add_argument("--timeout", type=float, default=10.0)
    args = parser.parse_args()
    if args.timeout <= 0:
        parser.error("--timeout must be greater than zero")
    return args


def main() -> int:
    args = parse_args()
    try:
        asyncio.run(run(args.broker, args.timeout))
    except (AssertionError, asyncio.TimeoutError, OSError) as error:
        print(f"broker conformance failed: {error}", file=sys.stderr)
        return 1
    print("broker conformance passed: 9 contract checks")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
