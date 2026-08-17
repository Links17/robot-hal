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
PROTOCOL_MINOR_MINIMUM = 1
PROTOCOL_MINOR_MAXIMUM = 1
PROTOCOL_MINOR = PROTOCOL_MINOR_MAXIMUM
SERIAL_CAPABILITY = "serial.bytes/v1"
CAN_CLASSIC_CAPABILITY = "can.classic/v1"
CAN_FD_CAPABILITY = "can.fd/v1"
TRANSFER_LIMIT = 64 * 1024
FRAME_LIMIT = 1024 * 1024
DIAGNOSTIC_LIMIT = 64 * 1024


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


async def capture_stream_tail(reader: asyncio.StreamReader, limit: int) -> bytes:
    _require(limit > 0, "diagnostic byte limit must be greater than zero")
    tail = bytearray()
    while chunk := await reader.read(4096):
        tail.extend(chunk)
        if len(tail) > limit:
            del tail[:-limit]
    return bytes(tail)


async def _finish_task_with_cap(task: asyncio.Task, timeout: float):
    done, _pending = await asyncio.wait({task}, timeout=timeout)
    if not done:
        task.cancel()
        task.add_done_callback(_consume_task_result)
        raise asyncio.TimeoutError
    return task.result()


def _consume_task_result(task: asyncio.Task) -> None:
    with contextlib.suppress(BaseException):
        task.result()


async def _await_with_cap(awaitable, timeout: float):
    return await _finish_task_with_cap(asyncio.ensure_future(awaitable), timeout)


async def cleanup_process(
    process: asyncio.subprocess.Process,
    diagnostics: asyncio.Task[bytes],
    timeout: float,
) -> bytes:
    if process.returncode is None:
        with contextlib.suppress(ProcessLookupError):
            process.kill()
        process_wait = asyncio.create_task(process.wait())
        try:
            await _finish_task_with_cap(process_wait, timeout)
        except asyncio.TimeoutError:
            pass
    try:
        return await _finish_task_with_cap(diagnostics, timeout)
    except asyncio.TimeoutError:
        return b"diagnostic capture timed out"


def apply_windows_private_dacl(path: Path) -> None:
    import ntsecuritycon
    import win32api
    import win32con
    import win32security

    process_token = win32security.OpenProcessToken(
        win32api.GetCurrentProcess(), win32con.TOKEN_QUERY
    )
    user = win32security.GetTokenInformation(
        process_token, win32security.TokenUser
    )[0]
    system = win32security.CreateWellKnownSid(
        win32security.WinLocalSystemSid, None
    )
    administrators = win32security.CreateWellKnownSid(
        win32security.WinBuiltinAdministratorsSid, None
    )
    dacl = win32security.ACL()
    for trustee in (user, system, administrators):
        dacl.AddAccessAllowedAce(
            win32security.ACL_REVISION,
            ntsecuritycon.FILE_ALL_ACCESS,
            trustee,
        )
    information = (
        win32security.OWNER_SECURITY_INFORMATION
        | win32security.DACL_SECURITY_INFORMATION
        | win32security.PROTECTED_DACL_SECURITY_INFORMATION
    )
    win32security.SetNamedSecurityInfo(
        str(path),
        win32security.SE_FILE_OBJECT,
        information,
        user,
        None,
        dacl,
        None,
    )


async def prepare_private_token(
    directory: Path,
    token_path: Path,
    token: bytes,
    *,
    os_name: str,
    timeout: float,
) -> None:
    if os_name == "nt":
        await _await_with_cap(
            asyncio.to_thread(apply_windows_private_dacl, directory), timeout
        )
    else:
        await _await_with_cap(asyncio.to_thread(directory.chmod, 0o700), timeout)
    await _await_with_cap(asyncio.to_thread(token_path.write_bytes, token), timeout)
    if os_name == "nt":
        await _await_with_cap(
            asyncio.to_thread(apply_windows_private_dacl, token_path), timeout
        )
    else:
        await _await_with_cap(asyncio.to_thread(token_path.chmod, 0o600), timeout)


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

        async def exchange():
            await self.transport.send(envelope.SerializeToString())
            while True:
                frame = await self.transport.receive()
                response = hal_pb2.Envelope()
                response.ParseFromString(frame)
                if response.request_id == request_id:
                    return response
                _require(
                    response.request_id == 0
                    and response.WhichOneof("payload") == "runtime_event",
                    f"unexpected response correlation id {response.request_id}",
                )

        return await _await_with_cap(exchange(), self.timeout)

    async def handshake(self, token: bytes) -> None:
        response = await self.request(
            "handshake_request",
            hal_pb2.HandshakeRequest(
                startup_token=token,
                protocol_major=PROTOCOL_MAJOR,
                protocol_minor=PROTOCOL_MINOR,
                required_capabilities=[SERIAL_CAPABILITY, CAN_CLASSIC_CAPABILITY, CAN_FD_CAPABILITY],
                max_frame_bytes=FRAME_LIMIT,
                max_read_bytes=TRANSFER_LIMIT,
                max_write_bytes=TRANSFER_LIMIT,
                protocol_minor_minimum=PROTOCOL_MINOR_MINIMUM,
                protocol_minor_maximum=PROTOCOL_MINOR_MAXIMUM,
            ),
        )
        _expect_payload(response, "handshake_response")
        handshake = response.handshake_response
        _require(handshake.protocol_major == PROTOCOL_MAJOR, "protocol major mismatch")
        _require(
            PROTOCOL_MINOR_MINIMUM <= handshake.protocol_minor <= PROTOCOL_MINOR_MAXIMUM,
            "selected protocol minor is outside the offered range",
        )
        _require(SERIAL_CAPABILITY in handshake.capabilities, "Serial capability missing")
        _require(CAN_CLASSIC_CAPABILITY in handshake.capabilities, "Classic CAN capability missing")
        _require(CAN_FD_CAPABILITY in handshake.capabilities, "CAN FD capability missing")
        self.transport.set_frame_limit(handshake.max_frame_bytes)

    async def close(self) -> None:
        await _await_with_cap(self.transport.close(), self.timeout)


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


async def _exercise_can(client: RawClient) -> None:
    enumerated = await client.request("enumerate_can_request", hal_pb2.EnumerateCanRequest())
    resources = _expect_payload(enumerated, "enumerate_can_response").resources
    _require(len(resources) == 1, f"expected one virtual CAN resource, got {len(resources)}")
    descriptor = resources[0]
    _require(descriptor.properties.get("adapter") == "virtual", "CAN adapter was not virtual")
    opened = _expect_payload(
        await client.request(
            "open_can_request",
            hal_pb2.OpenCanRequest(
                selector=_selector(descriptor),
                mode=hal_pb2.LEASE_MODE_CONTROL,
                config=hal_pb2.CanOpenConfig(
                    attach=hal_pb2.CanLinkExpectation(mode=hal_pb2.CAN_MODE_CLASSIC)
                ),
                filters=hal_pb2.CanFilterSet(),
            ),
        ),
        "open_can_response",
    )
    _expect_payload(
        await client.request(
            "close_session_request",
            hal_pb2.CloseSessionRequest(session_id=opened.session_id, lease=opened.lease),
        ),
        "close_session_response",
    )


async def exercise_contract(endpoint: str, token: bytes, timeout: float) -> None:
    first = RawClient(
        await _await_with_cap(connect_transport(endpoint), timeout), timeout
    )
    second = None
    try:
        await first.handshake(token)
        await _exercise_can(first)
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

        async def wait_for_resource_reuse() -> None:
            nonlocal second
            second = RawClient(await connect_transport(endpoint), timeout)
            await second.handshake(token)
            while True:
                response = await _open_serial(second, descriptor)
                if response.WhichOneof("payload") == "open_serial_response":
                    break
                _require(
                    response.WhichOneof("payload") == "error"
                    and response.error.name == "runtime.lease.conflict",
                    f"unexpected cleanup response {response.WhichOneof('payload')}",
                )
                await asyncio.sleep(0.025)

        await _await_with_cap(wait_for_resource_reuse(), timeout)
    finally:
        with contextlib.suppress(Exception):
            await first.close()
        if second is not None:
            with contextlib.suppress(Exception):
                await second.close()


async def _wait_for_readiness(process, endpoint: str, timeout: float) -> None:
    _require(process.stdout is not None, "broker stdout was not captured")
    line = await _await_with_cap(process.stdout.readline(), timeout)
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
    await _await_with_cap(process.wait(), timeout)
    _require(process.returncode == 0, f"broker shutdown returned {process.returncode}")


async def run(broker: Path, timeout: float) -> None:
    broker = broker.resolve()
    _require(broker.is_file(), f"broker executable not found: {broker}")
    with tempfile.TemporaryDirectory(prefix="seeed-hal-conformance-") as temporary:
        directory = Path(temporary)
        nonce = uuid.uuid4().hex
        endpoint = endpoint_for_platform(directory, nonce, os.name)
        token_path = directory / "startup-token"
        token = secrets.token_bytes(32)
        await prepare_private_token(
            directory,
            token_path,
            token,
            os_name=os.name,
            timeout=timeout,
        )
        creationflags = subprocess.CREATE_NEW_PROCESS_GROUP if os.name == "nt" else 0
        spawn = asyncio.create_task(
            asyncio.create_subprocess_exec(
                *broker_command(broker, endpoint, token_path),
                stdout=asyncio.subprocess.PIPE,
                stderr=asyncio.subprocess.PIPE,
                creationflags=creationflags,
            )
        )
        process = await _finish_task_with_cap(spawn, timeout)
        _require(process.stderr is not None, "broker stderr was not captured")
        diagnostics = asyncio.create_task(
            capture_stream_tail(process.stderr, DIAGNOSTIC_LIMIT)
        )
        try:
            await _wait_for_readiness(process, endpoint, timeout)
            await exercise_contract(endpoint, token, timeout)
            await _graceful_shutdown(process, timeout)
        finally:
            stderr = (await cleanup_process(process, diagnostics, timeout)).decode(
                "utf-8", errors="replace"
            )
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
    print("broker conformance passed: Serial and CAN contract checks")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
