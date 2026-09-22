#!/usr/bin/env python3
"""Carry immutable image identity between build, GPU qualification, and publication."""

import argparse
import hashlib
import json
from pathlib import Path
import re
import subprocess

from release_identity import ReleaseIdentity


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


def digest(candidate):
    encoded = json.dumps(candidate, sort_keys=True, separators=(",", ":")).encode()
    return hashlib.sha256(encoded).hexdigest()


def write_new(path, document):
    with Path(path).open("x") as output:
        json.dump(document, output, sort_keys=True, indent=2)
        output.write("\n")


def validate_proof(candidate, proof):
    if proof != {"candidate_sha256": digest(candidate), "gpu": "passed"}:
        raise ValueError("GPU qualification does not match this candidate")


def publish(candidate, proof, identity):
    validate_proof(candidate, proof)
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
    print("QUALIFIED_RELEASE=" + reference)
    return reference


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    sub = parser.add_subparsers(dest="operation", required=True)
    create = sub.add_parser("create")
    create.add_argument("output")
    create.add_argument("revision")
    create.add_argument("images", nargs=4)
    for operation in ("qualify", "publish"):
        command = sub.add_parser(operation)
        command.add_argument("candidate")
        command.add_argument("proof")
    args = parser.parse_args()
    identity = ReleaseIdentity.from_environment()
    if args.operation == "create":
        candidate = validate({"revision": args.revision, "source": identity.source,
                              "images": dict(zip(ROLES, args.images))}, identity)
        write_new(args.output, candidate)
        return
    candidate = validate(json.loads(Path(args.candidate).read_text()), identity)
    checkout = subprocess.check_output(["git", "rev-parse", "HEAD"], text=True).strip()
    if checkout != candidate["revision"]:
        raise ValueError("checkout does not match the candidate source revision")
    if args.operation == "qualify":
        image = candidate["images"]["blender"]
        subprocess.run(["docker", "pull", "--platform", "linux/amd64", image], check=True)
        subprocess.run(["python3", "scripts/verify_release_image.py", "blender", image, checkout], check=True)
        subprocess.run(["scripts/smoke-blender-gpu", image], check=True)
        write_new(args.proof, {"candidate_sha256": digest(candidate), "gpu": "passed"})
    else:
        proof = json.loads(Path(args.proof).read_text())
        publish(candidate, proof, identity)


if __name__ == "__main__":
    main()
