#!/usr/bin/env python3
"""Publish the image-set record consumed by installations."""

import json
import re
import subprocess

ROLES = ("server", "blender", "cad", "slicer")


def validate(candidate, identity):
    if not isinstance(candidate, dict) or set(candidate) != {"revision", "source", "images"}:
        raise ValueError("invalid candidate fields")
    revision = candidate["revision"]
    if not isinstance(revision, str) or not re.fullmatch(r"[0-9a-f]{40}", revision):
        raise ValueError("candidate requires a full source revision")
    if candidate["source"] != identity.source:
        raise ValueError("candidate source does not match configured source")
    images = candidate["images"]
    if not isinstance(images, dict) or set(images) != set(ROLES):
        raise ValueError("candidate must contain exactly four runtime images")
    for role, reference in images.items():
        expected = identity.repository(role) + ":sha-" + revision[:12] + "@sha256:"
        if not isinstance(reference, str) or not re.fullmatch(re.escape(expected) + r"[0-9a-f]{64}", reference):
            raise ValueError("candidate image does not match its role, revision, and digest")
    return candidate


def publish(candidate, identity):
    validate(candidate, identity)
    revision = candidate["revision"]
    tag = identity.repository("pair") + ":sha-" + revision[:12]
    command = ["docker", "buildx", "build", "--platform", "linux/amd64",
               "--provenance=false", "--push", "--file", "release/Dockerfile",
               "--build-arg", "SOURCE_REVISION=" + revision]
    for role in ROLES:
        image, image_digest = candidate["images"][role].split("@")
        command += ["--build-arg", role.upper() + "_IMAGE=" + image,
                    "--build-arg", role.upper() + "_DIGEST=" + image_digest]
    subprocess.run(command + ["--tag", tag, "release"], check=True)
    result = subprocess.check_output(["scripts/registry-manifest-digest", tag], text=True).strip()
    if not re.fullmatch(r"sha256:[0-9a-f]{64}", result):
        raise ValueError("registry returned an invalid release-record digest")
    reference = tag + "@" + result
    subprocess.run(["docker", "pull", "--platform", "linux/amd64", reference], check=True)
    document = json.loads(subprocess.check_output(["docker", "image", "inspect", reference], text=True))[0]
    labels = document["Config"]["Labels"]
    if labels.get("org.opencontainers.image.revision") != revision:
        raise ValueError("published record has an unexpected revision")
    for role in ROLES:
        image, image_digest = candidate["images"][role].split("@")
        if (labels.get("org.printable." + role + ".image") != image
                or labels.get("org.printable." + role + ".digest") != image_digest):
            raise ValueError("published record has an unexpected component")
    print("RELEASE_IMAGE=" + reference)
    return reference
