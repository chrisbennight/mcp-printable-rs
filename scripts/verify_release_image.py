#!/usr/bin/env python3
"""Verify the immutable runtime contract of a release image."""

from __future__ import annotations

import json
import re
import subprocess
import sys
from dataclasses import dataclass
from typing import Any

from release_identity import ReleaseIdentity

SOURCE_REPOSITORY = ReleaseIdentity.source
REVISION_RE = re.compile(r"^[0-9a-f]{40}$")
SENSITIVE_KEY_RE = re.compile(
    r"(?:^|[._-])(?:PASSWORD|PASSWD|TOKEN|SECRET|API[._-]?KEY|"
    r"PRIVATE[._-]?KEY|ACCESS[._-]?KEY|CREDENTIALS?|AUTHORIZATION|"
    r"BEARER|PAT)(?:[._-]|$)",
    re.IGNORECASE,
)
SENSITIVE_EXACT_KEYS = {
    "DOCKER_AUTH_CONFIG",
    "MARIADB_PWD",
    "MYSQL_PWD",
    "PGPASSWORD",
    "RABBITMQ_DEFAULT_PASS",
    "REDISCLI_AUTH",
}
RUNTIME_REFERENCE_RE = re.compile(
    r"^[\"']?\$(?:[A-Za-z_][A-Za-z0-9_]*|\{[A-Za-z_][A-Za-z0-9_]*\})[\"']?$"
)
SENSITIVE_ASSIGNMENT_RE = re.compile(
    r"(?<![A-Za-z0-9])(?:password|passwd|token|secret|api[_-]?key|private[_-]?key|"
    r"access[_-]?key|credentials?|authorization|bearer|pat)"
    r"[\"']?\s*[:=]\s*(?P<value>\"[^\"\s]*\"|'[^'\s]*'|[^\s,;}]+)",
    re.IGNORECASE,
)
SENSITIVE_OPTION_RE = re.compile(
    r"(?<![A-Za-z0-9])--(?:password|passwd|token|secret|api[_-]?key|"
    r"private[_-]?key|access[_-]?key|credentials?|authorization|bearer|pat)"
    r"(?=\s|=)(?:\s*=\s*|\s+)"
    r"(?P<value>\"[^\"\s]*\"|'[^'\s]*'|[^\s;&|]+)",
    re.IGNORECASE,
)
CREDENTIAL_URI_RE = re.compile(
    r"\b[A-Za-z][A-Za-z0-9+.-]*://[^\s/@:]*:"
    r"(?P<value>[^\s/@]+)@",
)
PRIVATE_KEY_RE = re.compile(
    r"-----BEGIN (?:[A-Z0-9]+ )?PRIVATE KEY-----",
)
AUTHORIZATION_VALUE_RE = re.compile(
    r"\b(?:Basic|Bearer)\s+"
    r"(?=[A-Za-z0-9._~+/=-]{12,})(?=[A-Za-z0-9._~+/=-]*[._~+/=-])"
    r"[A-Za-z0-9._~+/=-]+",
    re.IGNORECASE,
)


def is_runtime_reference(value: str) -> bool:
    return RUNTIME_REFERENCE_RE.fullmatch(value) is not None


def is_sensitive_key(key: str) -> bool:
    return key.upper() in SENSITIVE_EXACT_KEYS or SENSITIVE_KEY_RE.search(key) is not None


def contains_credential_value(value: str) -> bool:
    for pattern in (
        SENSITIVE_ASSIGNMENT_RE,
        SENSITIVE_OPTION_RE,
        CREDENTIAL_URI_RE,
    ):
        if any(
            not is_runtime_reference(match.group("value"))
            for match in pattern.finditer(value)
        ):
            return True
    return PRIVATE_KEY_RE.search(value) is not None or AUTHORIZATION_VALUE_RE.search(
        value
    ) is not None


@dataclass(frozen=True)
class ImageContract:
    repository: str
    role: str
    user: str
    healthcheck: list[str]
    entrypoint: list[str]
    environment: tuple[str, ...]


CONTRACTS = {
    "cad": ImageContract(
        repository="mcp-printable-cad",
        role="cad-worker",
        user="10001",
        healthcheck=["CMD", "/usr/local/bin/printable-cad-worker", "--healthcheck"],
        entrypoint=["/usr/bin/tini", "--", "/usr/local/bin/printable-cad-worker"],
        environment=("PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin",),
    ),
    "server": ImageContract(
        repository="mcp-printable-rs",
        role="server",
        user="10001",
        healthcheck=[
            "CMD",
            "/usr/local/bin/printable-server",
            "--healthcheck",
        ],
        entrypoint=[
            "/usr/bin/tini",
            "--",
            "/usr/local/bin/printable-server",
            "--transport",
            "streamable-http",
        ],
        environment=(
            "PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin",
            "PRINTABLE_HTTP_HOST=0.0.0.0",
            "PRINTABLE_HTTP_PORT=8000",
            "PRINTABLE_GEOMETRY_WORKER_MEMORY_MIB=1024",
            "FFMPEG_BIN=/usr/bin/ffmpeg",
            "OPENSCAD_BIN=/usr/local/bin/openscad-headless",
            "HOME=/tmp/printable",
        ),
    ),
    "blender": ImageContract(
        repository="mcp-printable-blender",
        role="blender",
        user="10001:10001",
        healthcheck=[
            "CMD",
            "python3",
            "/opt/printable/addon/printable_bridge/healthcheck.py",
        ],
        entrypoint=[
            "/usr/bin/tini",
            "--",
            "/opt/printable/scripts/run-headless-blender",
        ],
        environment=(
            "PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin",
            "BLENDER_BIN=/opt/blender/blender",
            "HOME=/home/printable",
            "PYTHONDONTWRITEBYTECODE=1",
            "PRINTABLE_BLENDER_BIND=0.0.0.0",
            "PRINTABLE_BLENDER_STATE_DIR=/run/printable-blender",
            "PRINTABLE_BLENDER_WORKSPACE_ROOT=/workspace",
        ),
    ),
}


def expected_reference(role: str, identity: ReleaseIdentity | None = None) -> re.Pattern[str]:
    return (identity or ReleaseIdentity()).reference_pattern(role)


def verify(
    role: str,
    image: str,
    expected_revision: str,
    document: dict[str, Any],
    history: list[str],
    identity: ReleaseIdentity | None = None,
) -> list[str]:
    identity = identity or ReleaseIdentity()
    contract = CONTRACTS[role]
    failures: list[str] = []
    config = document.get("Config")
    if not isinstance(config, dict):
        return ["image inspect has no Config object"]

    if not expected_reference(role, identity).fullmatch(image):
        failures.append(f"reference is not an immutable {role} release image")
    if not REVISION_RE.fullmatch(expected_revision):
        failures.append("expected revision is not a full lowercase Git commit")
    if document.get("Os") != "linux":
        failures.append("image OS is not linux")
    if document.get("Architecture") != "amd64":
        failures.append("image architecture is not amd64")
    if config.get("User") != contract.user:
        failures.append(f"image user is not {contract.user}")

    healthcheck = config.get("Healthcheck")
    health_test = healthcheck.get("Test") if isinstance(healthcheck, dict) else None
    if health_test != contract.healthcheck:
        failures.append("image health command does not match the runtime contract")
    if config.get("Entrypoint") != contract.entrypoint:
        failures.append("image entrypoint does not match the runtime contract")
    if config.get("Cmd") not in (None, []):
        failures.append("image has an unexpected default command")

    labels = config.get("Labels")
    labels = labels if isinstance(labels, dict) else {}
    expected_labels = {
        "org.opencontainers.image.source": identity.source,
        "org.opencontainers.image.revision": expected_revision,
        "org.printable.role": contract.role,
    }
    for key, expected in expected_labels.items():
        if labels.get(key) != expected:
            failures.append(f"image label {key} does not match the release contract")
    if set(labels) != set(expected_labels):
        failures.append("image labels do not match the exact runtime contract")

    env = config.get("Env")
    if not isinstance(env, list):
        failures.append("image environment is not an inspectable list")
        env = []
    parsed_env: dict[str, str] = {}
    for item in env:
        if not isinstance(item, str):
            failures.append("image environment contains a non-string entry")
            continue
        key, separator, value = item.partition("=")
        if not separator or not key or key in parsed_env:
            failures.append("image environment contains a malformed or duplicate entry")
            continue
        parsed_env[key] = value
        if (
            value
            and is_sensitive_key(key)
            and not is_runtime_reference(value)
        ):
            failures.append(f"image environment embeds a value for sensitive key {key}")
        elif value and contains_credential_value(value):
            failures.append(
                f"image environment entry {key} contains credential-shaped material"
            )
    expected_env = dict(item.split("=", 1) for item in contract.environment)
    if parsed_env != expected_env:
        failures.append("image environment does not match the exact runtime contract")

    for key, value in labels.items():
        if (
            is_sensitive_key(str(key))
            and isinstance(value, str)
            and value
            and not is_runtime_reference(value)
        ):
            failures.append(f"image labels embed a value for sensitive key {key}")
        elif isinstance(value, str) and contains_credential_value(value):
            failures.append(
                f"image label {key} contains credential-shaped material"
            )

    if any(contains_credential_value(entry) for entry in history):
        failures.append("image build history appears to embed credential material")

    return failures


def inspect_image(image: str) -> tuple[dict[str, Any], list[str]]:
    inspected = subprocess.run(
        ["docker", "image", "inspect", image],
        check=True,
        capture_output=True,
        text=True,
    )
    payload = json.loads(inspected.stdout)
    if not isinstance(payload, list) or len(payload) != 1 or not isinstance(payload[0], dict):
        raise ValueError("docker image inspect returned an unexpected document")

    history_result = subprocess.run(
        [
            "docker",
            "history",
            "--no-trunc",
            "--format",
            "{{json .CreatedBy}}",
            image,
        ],
        check=True,
        capture_output=True,
        text=True,
    )
    history = [
        json.loads(line)
        for line in history_result.stdout.splitlines()
        if line.strip()
    ]
    if not all(isinstance(entry, str) for entry in history):
        raise ValueError("docker history returned a non-string command")
    return payload[0], history


def main() -> int:
    if len(sys.argv) != 4 or sys.argv[1] not in CONTRACTS:
        print(
            "usage: verify_release_image.py <server|blender|cad> "
            "<tag@sha256:digest> <source-revision>",
            file=sys.stderr,
        )
        return 2
    role, image, expected_revision = sys.argv[1:]
    try:
        identity = ReleaseIdentity.from_environment()
    except ValueError:
        print("release registry or source identity is invalid", file=sys.stderr)
        return 2
    if not expected_reference(role, identity).fullmatch(image):
        print(f"refusing a mutable or unexpected {role} image reference", file=sys.stderr)
        return 2
    if not REVISION_RE.fullmatch(expected_revision):
        print("source revision must be 40 lowercase hexadecimal characters", file=sys.stderr)
        return 2

    try:
        document, history = inspect_image(image)
    except (json.JSONDecodeError, OSError, subprocess.CalledProcessError, ValueError) as error:
        print(f"could not inspect immutable {role} image: {error}", file=sys.stderr)
        return 1

    failures = verify(role, image, expected_revision, document, history, identity)
    if failures:
        for failure in failures:
            print(f"release image policy: {failure}", file=sys.stderr)
        return 1
    print(f"IMAGE_POLICY_OK role={role} image={image}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
