from __future__ import annotations

import importlib.util
from pathlib import Path


RUNNER = Path(__file__).with_name("run-broker-conformance.py")


def load_runner():
    spec = importlib.util.spec_from_file_location("broker_conformance", RUNNER)
    assert spec is not None and spec.loader is not None
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


def test_endpoint_name_is_local_and_platform_specific(tmp_path: Path) -> None:
    runner = load_runner()

    assert runner.endpoint_for_platform(tmp_path, "fixed", "posix") == str(
        tmp_path / "broker.sock"
    )
    assert (
        runner.endpoint_for_platform(tmp_path, "fixed", "nt")
        == r"\\.\pipe\seeed-hal-conformance-fixed"
    )


def test_broker_command_uses_only_production_startup_arguments(tmp_path: Path) -> None:
    runner = load_runner()
    broker = tmp_path / "seeed-hal-broker"
    token = tmp_path / "token"

    command = runner.broker_command(broker, "endpoint", token)

    assert command == [
        str(broker),
        "--endpoint",
        "endpoint",
        "--auth-token-file",
        str(token),
    ]
    assert "virtual" not in " ".join(command).lower()


def test_readiness_parser_decodes_windows_endpoint_escaping() -> None:
    runner = load_runner()

    assert (
        runner.parse_readiness_endpoint(
            br'{"status":"ready","endpoint":"\\\\.\\pipe\\seeed-hal-fixed"}'
        )
        == r"\\.\pipe\seeed-hal-fixed"
    )
