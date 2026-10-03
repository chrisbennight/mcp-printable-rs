"""Publish the exact images saved by the successful container test job."""

import argparse
import json
import os
from pathlib import Path
import re
import subprocess
from tempfile import TemporaryDirectory

from release_identity import ReleaseIdentity
from release_record import ROLES, publish
from release_security import load_policy, verify_grype_version, load_kev_ids, scan, evaluate_report, emit_evaluation
from verify_release_image import inspect_image, verify

ROOT = Path(__file__).resolve().parents[1]

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


def publish_images(revision, expected, scan_directory=Path("target/release/scans")):
    images = tested_images(revision)
    if images != expected:
        raise ValueError("loaded images differ from the tested image IDs")
    policy = load_policy()
    verify_grype_version(policy["scanner_version"])
    kev = load_kev_ids() if policy["fail_on_kev"] else set()
    scan_directory.mkdir(parents=True, exist_ok=True)
    reports = {}
    for role, image in images.items():
        report = scan("docker:" + image)
        reports[role] = report
        evidence = {"revision": revision, "role": role, "image_id": image, "report": report}
        (scan_directory / (role + ".json")).write_text(json.dumps(evidence) + "\n")
        print(f"RELEASE_SCAN_ROLE role={role} image={image}", flush=True)
    approved = True
    for role, image in images.items():
        if not emit_evaluation(image, evaluate_report(reports[role], policy, kev), reports[role]):
            approved = False
    if not approved:
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
    if not re.fullmatch(r"[0-9a-f]{40}", revision):
        raise ValueError("source revision must be a full Git commit")
    # Publication is serialized; newer test or documentation commits need no new image.
    subprocess.check_output(
        ["git", "fetch", "--quiet", "--no-tags", "origin", "refs/heads/main"], cwd=ROOT)
    current_main = subprocess.check_output(
        ["git", "rev-parse", "FETCH_HEAD"], cwd=ROOT, text=True).strip()
    if not re.fullmatch(r"[0-9a-f]{40}", current_main):
        raise ValueError("could not resolve main for update discovery")
    subprocess.check_output(["git", "merge-base", "--is-ancestor", revision, current_main], cwd=ROOT)
    with TemporaryDirectory(prefix="printable-publication-") as directory:
        output = Path(directory) / "scope"
        subprocess.check_output(
            ["bash", str(ROOT / "scripts/ci-scope.sh")], cwd=ROOT,
            env=os.environ | {"GITHUB_EVENT_NAME": "push", "GITHUB_REF": "refs/heads/main",
                              "BASE_SHA": revision, "TARGET_SHA": current_main,
                              "GITHUB_OUTPUT": str(output)})
        values = dict(line.split("=", 1) for line in output.read_text().splitlines())
    if values["publish"] == "true":
        return
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
