"""Publish the exact images saved by the successful container test job."""

import argparse
import json
from pathlib import Path
import re
import subprocess

from release_identity import ReleaseIdentity
from release_record import ROLES, publish
from release_security import load_policy, verify_grype_version, load_kev_ids, scan, evaluate_report, emit_evaluation
from verify_release_image import inspect_image, verify


def tested_images(revision):
    if not re.fullmatch(r"[0-9a-f]{40}", revision):
        raise ValueError("source revision must be a full Git commit")
    identity = ReleaseIdentity.from_environment()
    images = {}
    for role in ROLES:
        document, history = inspect_image("printable-" + role + ":ci")
        image = document.get("Id", "")
        if not re.fullmatch(r"sha256:[0-9a-f]{64}", image):
            raise ValueError("invalid local image ID")
        failures = verify(role, image, revision, document, history, identity)
        if failures:
            raise ValueError(role + ": " + "; ".join(failures))
        images[role] = image
    return images


def publish_images(revision, expected):
    images = tested_images(revision)
    if images != expected:
        raise ValueError("loaded images differ from the tested image IDs")
    policy = load_policy()
    verify_grype_version(policy["scanner_version"])
    kev = load_kev_ids() if policy["fail_on_kev"] else set()
    for image in images.values():
        if not emit_evaluation(image, evaluate_report(scan("docker:" + image), policy, kev)):
            raise ValueError("image security check failed")
    identity = ReleaseIdentity.from_environment()
    references = {}
    for role, image in images.items():
        tag = identity.repository(role) + ":sha-" + revision[:12]
        subprocess.run(["docker", "tag", image, tag], check=True)
        subprocess.run(["docker", "push", tag], check=True)
        digest = subprocess.check_output(["scripts/registry-manifest-digest", tag], text=True).strip()
        if not re.fullmatch(r"sha256:[0-9a-f]{64}", digest):
            raise ValueError("invalid registry digest")
        reference = tag + "@" + digest
        subprocess.run(["docker", "pull", "--platform", "linux/amd64", reference], check=True)
        document, history = inspect_image(reference)
        if document.get("Id") != image or verify(role, reference, revision, document, history, identity):
            raise ValueError("published image does not match the tested image")
        references[role] = reference
    reference = publish({"revision": revision, "source": identity.source, "images": references}, identity)
    publish_update_tag(revision, reference, identity)
    return reference


def publish_update_tag(revision, reference, identity):
    # Main CI runs are serialized; an older rerun must not replace the discovery tag.
    current_main = subprocess.check_output(
        ["git", "ls-remote", "origin", "refs/heads/main"], text=True).split()
    if len(current_main) != 2 or not re.fullmatch(r"[0-9a-f]{40}", current_main[0]) or current_main[1] != "refs/heads/main":
        raise ValueError("could not resolve main for update discovery")
    if current_main == [revision, "refs/heads/main"]:
        channel = identity.repository("pair") + ":main"
        subprocess.run(["docker", "tag", reference, channel], check=True)
        subprocess.run(["docker", "push", channel], check=True)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("operation", choices=("record", "publish"))
    parser.add_argument("revision")
    parser.add_argument("manifest", type=Path)
    args = parser.parse_args()
    if args.operation == "record":
        images = tested_images(args.revision)
        with args.manifest.open("x") as output:
            json.dump(images, output, sort_keys=True)
            output.write("\n")
    else:
        reference = publish_images(args.revision, json.loads(args.manifest.read_text()))
        Path("release-image.txt").write_text(reference + "\n")


if __name__ == "__main__":
    main()
