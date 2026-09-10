"""Pure request validation and response construction."""

from __future__ import annotations

from dataclasses import dataclass
import re
from typing import Any
import uuid

from . import VERSION


COMMAND_PATTERN = re.compile(r"^[a-z][a-z0-9_]{0,63}$")
BRIDGE_INSTANCE_ID = str(uuid.uuid4())


class EnvelopeError(ValueError):
    """A wire envelope violates the bridge contract."""


@dataclass(frozen=True)
class Request:
    request_id: str
    command: str
    params: dict[str, Any]


def parse_request(value: Any) -> Request:
    if not isinstance(value, dict):
        raise EnvelopeError("request must be a JSON object")

    request_id = value.get("id")
    if not isinstance(request_id, str):
        raise EnvelopeError("request id must be a UUID string")
    try:
        parsed_id = uuid.UUID(request_id)
    except (ValueError, AttributeError) as error:
        raise EnvelopeError("request id must be a UUID string") from error
    if str(parsed_id) != request_id.lower():
        raise EnvelopeError("request id must use canonical UUID form")

    command = value.get("command")
    if not isinstance(command, str) or COMMAND_PATTERN.fullmatch(command) is None:
        raise EnvelopeError("command must be a lowercase registered name")

    params = value.get("params")
    if not isinstance(params, dict):
        raise EnvelopeError("params must be a JSON object")

    return Request(request_id=request_id, command=command, params=params)


def success(request_id: str, result: Any) -> dict[str, Any]:
    return {
        "id": request_id,
        "status": "success",
        "result": result,
        "addon_version": VERSION,
        "bridge_instance_id": BRIDGE_INSTANCE_ID,
    }


def failure(request_id: str, message: str) -> dict[str, Any]:
    return {
        "id": request_id,
        "status": "error",
        "error": message,
        "addon_version": VERSION,
        "bridge_instance_id": BRIDGE_INSTANCE_ID,
    }
