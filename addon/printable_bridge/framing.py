"""Bounded length-prefixed JSON framing with no Blender dependency."""

from __future__ import annotations

import json
import socket
import struct
import time
from typing import Any


PREFIX_BYTES = 4


class FrameError(ValueError):
    """A frame is truncated, oversized, or not valid JSON."""


class FrameTimeout(TimeoutError):
    """The absolute receive budget expired."""


def _receive_exact(
    connection: socket.socket, length: int, deadline: float | None
) -> bytes:
    chunks: list[bytes] = []
    remaining = length
    while remaining:
        if deadline is not None:
            budget = deadline - time.monotonic()
            if budget <= 0:
                raise FrameTimeout("frame receive deadline expired")
            connection.settimeout(budget)
        try:
            chunk = connection.recv(remaining)
        except socket.timeout as error:
            raise FrameTimeout("frame receive deadline expired") from error
        if not chunk:
            raise FrameError("connection closed before the frame was complete")
        chunks.append(chunk)
        remaining -= len(chunk)
    return b"".join(chunks)


def receive_json(
    connection: socket.socket,
    max_frame_bytes: int,
    deadline: float | None = None,
) -> Any:
    prefix = _receive_exact(connection, PREFIX_BYTES, deadline)
    (length,) = struct.unpack(">I", prefix)
    if length == 0:
        raise FrameError("empty frames are not allowed")
    if length > max_frame_bytes:
        raise FrameError(f"frame exceeds the {max_frame_bytes}-byte limit")
    payload = _receive_exact(connection, length, deadline)
    try:
        return json.loads(payload.decode("utf-8"))
    except (UnicodeDecodeError, json.JSONDecodeError) as error:
        raise FrameError("frame payload must be UTF-8 JSON") from error


def encode_json(value: Any, max_frame_bytes: int) -> bytes:
    try:
        payload = json.dumps(
            value, ensure_ascii=False, separators=(",", ":"), allow_nan=False
        ).encode("utf-8")
    except (TypeError, ValueError) as error:
        raise FrameError("response is not JSON serializable") from error
    if not payload:
        raise FrameError("empty frames are not allowed")
    if len(payload) > max_frame_bytes:
        raise FrameError(f"frame exceeds the {max_frame_bytes}-byte limit")
    return struct.pack(">I", len(payload)) + payload


def send_json(connection: socket.socket, value: Any, max_frame_bytes: int) -> None:
    connection.sendall(encode_json(value, max_frame_bytes))
