#!/usr/bin/env python3
"""Validated registry and source identity for a matching Printable image pair."""

from dataclasses import dataclass
import os
import re
from urllib.parse import urlsplit


REPOSITORIES = {"server": "mcp-printable-rs", "blender": "mcp-printable-blender",
                "cad": "mcp-printable-cad", "pair": "mcp-printable-release"}


@dataclass(frozen=True)
class ReleaseIdentity:
    registry: str = "ghcr.io"
    namespace: str = "chrisbennight"
    source: str = "https://github.com/chrisbennight/mcp-printable-rs"

    def __post_init__(self):
        if not re.fullmatch(r"[a-z0-9]+(?:[.-][a-z0-9]+)*(?::[1-9][0-9]{0,4})?", self.registry):
            raise ValueError("PRINTABLE_REGISTRY must be a lowercase registry host with optional port")
        if ":" in self.registry and int(self.registry.rsplit(":", 1)[1]) > 65535:
            raise ValueError("PRINTABLE_REGISTRY port is out of range")
        if not re.fullmatch(r"[a-z0-9]+(?:[._-][a-z0-9]+)*(?:/[a-z0-9]+(?:[._-][a-z0-9]+)*)*", self.namespace):
            raise ValueError("PRINTABLE_IMAGE_NAMESPACE must be a lowercase registry path")
        try:
            source = urlsplit(self.source)
            port = source.port
        except ValueError:
            raise ValueError("PRINTABLE_SOURCE_REPOSITORY is not a valid repository URL") from None
        if (source.scheme != "https" or not source.hostname or source.username is not None
                or source.password is not None or source.query or source.fragment
                or any(character.isspace() for character in self.source)
                or not source.path.strip("/") or port == 0):
            raise ValueError("PRINTABLE_SOURCE_REPOSITORY must be a credential-free HTTPS repository URL")

    @classmethod
    def from_environment(cls):
        return cls(os.environ.get("PRINTABLE_REGISTRY", cls.registry),
                   os.environ.get("PRINTABLE_IMAGE_NAMESPACE", cls.namespace),
                   os.environ.get("PRINTABLE_SOURCE_REPOSITORY", cls.source))

    def repository(self, role):
        return f"{self.registry}/{self.namespace}/{REPOSITORIES[role]}"

    def reference_pattern(self, role):
        return re.compile(re.escape(self.repository(role)) + r":sha-[0-9a-f]{12}@sha256:[0-9a-f]{64}\Z")


if __name__ == "__main__":
    try:
        identity = ReleaseIdentity.from_environment()
    except ValueError as error:
        raise SystemExit(str(error)) from None
    for role in ("server", "blender", "pair"):
        print(identity.repository(role))
    print(identity.source)
    print(identity.repository("cad"))
