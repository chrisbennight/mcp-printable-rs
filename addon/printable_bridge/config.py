"""Validated runtime configuration for the Blender bridge."""

from __future__ import annotations

from dataclasses import dataclass
import ipaddress
import math
import os
from pathlib import Path
from typing import Mapping


class ConfigError(ValueError):
    """Configuration cannot be used safely."""


OWNED_DISPLAY_PID_ENV = "PRINTABLE_BLENDER_OWNED_DISPLAY_PID"


def owned_display_pid() -> int | None:
    value = _integer(os.environ, OWNED_DISPLAY_PID_ENV, 0, 0, 4194304)
    return value or None


def blender_mode(env: Mapping[str, str] | None = None) -> str:
    values = os.environ if env is None else env
    mode = values.get("PRINTABLE_BLENDER_MODE", "background")
    if mode not in {"background", "ui"}:
        raise ConfigError("PRINTABLE_BLENDER_MODE must be background or ui")
    return mode


def blender_ui_backend(env: Mapping[str, str] | None = None) -> str:
    values = os.environ if env is None else env
    backend = values.get("PRINTABLE_BLENDER_UI_BACKEND", "software")
    if backend not in {"software", "egl"}:
        raise ConfigError("PRINTABLE_BLENDER_UI_BACKEND must be software or egl")
    return backend


def _integer(env: Mapping[str, str], name: str, default: int, minimum: int, maximum: int) -> int:
    raw = env.get(name, str(default))
    try:
        value = int(raw)
    except ValueError as error:
        raise ConfigError(f"{name} must be an integer") from error
    if not minimum <= value <= maximum:
        raise ConfigError(f"{name} must be between {minimum} and {maximum}")
    return value


def _bounded_float(
    env: Mapping[str, str],
    name: str,
    default: float,
    minimum: float,
    maximum: float,
) -> float:
    raw = env.get(name, str(default))
    try:
        value = float(raw)
    except ValueError as error:
        raise ConfigError(f"{name} must be a number") from error
    if not math.isfinite(value) or not minimum <= value <= maximum:
        raise ConfigError(f"{name} must be between {minimum:g} and {maximum:g}")
    return value


def blender_shutdown_timeout_seconds(
    env: Mapping[str, str] | None = None,
) -> float:
    values = os.environ if env is None else env
    return _bounded_float(
        values,
        "PRINTABLE_BLENDER_SHUTDOWN_TIMEOUT_SECONDS",
        10.0,
        0.1,
        300.0,
    )


@dataclass(frozen=True)
class BridgeConfig:
    bind: str
    port: int
    request_timeout_seconds: float
    shutdown_timeout_seconds: float
    queue_capacity: int
    max_connections: int
    max_frame_bytes: int
    heartbeat_seconds: float
    state_dir: Path
    workspace_root: Path
    render_device: str
    enable_test_commands: bool
    role: str = "live"

    @classmethod
    def from_env(cls, env: Mapping[str, str] | None = None) -> BridgeConfig:
        values = os.environ if env is None else env
        role = values.get("PRINTABLE_BLENDER_ROLE", "live")
        if role not in {"live", "render_worker"}:
            raise ConfigError("PRINTABLE_BLENDER_ROLE must be live or render_worker")
        if role == "render_worker" and blender_mode(values) != "background":
            raise ConfigError("render_worker requires background Blender")
        bind = values.get("PRINTABLE_BLENDER_BIND", "127.0.0.1")
        try:
            ipaddress.ip_address(bind)
        except ValueError as error:
            raise ConfigError("PRINTABLE_BLENDER_BIND must be an IPv4 or IPv6 address") from error

        render_device = values.get("PRINTABLE_BLENDER_RENDER_DEVICE", "CPU").upper()
        if render_device not in {"CPU", "OPTIX"}:
            raise ConfigError("PRINTABLE_BLENDER_RENDER_DEVICE must be CPU or OPTIX")

        state_dir = Path(values.get("PRINTABLE_BLENDER_STATE_DIR", "/run/printable-blender"))
        workspace_root = Path(
            values.get("PRINTABLE_BLENDER_WORKSPACE_ROOT", "/workspace")
        )
        if not state_dir.is_absolute():
            raise ConfigError("PRINTABLE_BLENDER_STATE_DIR must be absolute")
        if not workspace_root.is_absolute():
            raise ConfigError("PRINTABLE_BLENDER_WORKSPACE_ROOT must be absolute")

        return cls(
            bind=bind,
            port=_integer(values, "BLENDER_PORT", 9876, 1, 65535),
            request_timeout_seconds=_bounded_float(
                values,
                "PRINTABLE_BLENDER_REQUEST_TIMEOUT_SECONDS",
                120.0,
                0.1,
                3600.0,
            ),
            shutdown_timeout_seconds=blender_shutdown_timeout_seconds(values),
            queue_capacity=_integer(
                values, "PRINTABLE_BLENDER_QUEUE_CAPACITY", 16, 1, 1024
            ),
            max_connections=_integer(
                values, "PRINTABLE_BLENDER_MAX_CONNECTIONS", 32, 1, 1024
            ),
            max_frame_bytes=_integer(
                values,
                "PRINTABLE_BLENDER_MAX_FRAME_BYTES",
                50 * 1024 * 1024,
                1024,
                64 * 1024 * 1024,
            ),
            heartbeat_seconds=_bounded_float(
                values,
                "PRINTABLE_BLENDER_HEARTBEAT_SECONDS",
                1.0,
                0.05,
                60.0,
            ),
            state_dir=state_dir,
            workspace_root=workspace_root,
            render_device=render_device,
            role=role,
            enable_test_commands=values.get(
                "PRINTABLE_BLENDER_ENABLE_TEST_COMMANDS", "false"
            ).lower()
            in {"1", "true", "yes"},
        )
