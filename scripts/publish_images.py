"""Publish the exact images saved by the successful container test job."""

import argparse
import json
import os
from pathlib import Path
import re
import subprocess
import shutil
from tempfile import TemporaryDirectory

from release_identity import ReleaseIdentity
from release_record import ROLES, publish
from release_security import (load_policy, verify_grype_version, load_kev_ids, scan, evaluate_report,
                              emit_evaluation, POLICY_PATH, VENDOR_FIXES_PATH)
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


def publish_images(revision, expected, evidence_directory=Path("target/release/evidence")):
    images = tested_images(revision)
    if images != expected:
        raise ValueError("loaded images differ from the tested image IDs")
    from release_evidence import verify_inventory_transfer
    verify_inventory_transfer(evidence_directory, revision, images)
    transfer = json.loads((evidence_directory / "tested-images.json").read_text())
    if transfer != {"schema": 1, "revision": revision, "images": images}:
        raise ValueError("image inventories differ from the tested image IDs")
    for role in ROLES:
        material = json.loads((evidence_directory / role / "materials.json").read_text())
        if (material.get("revision") != revision or material.get("tested_image_id") != images[role]
                or material.get("role") != role or material.get("unowned_native_files") != []
                or not isinstance(material.get("debian"), list) or not material["debian"]):
            raise ValueError("required image inventory is missing or does not match the candidate")
        if not (evidence_directory / role / "sbom.spdx.json").is_file():
            raise ValueError("required image SBOM is absent")
    policy = load_policy()
    verify_grype_version(policy["scanner_version"])
    kev = load_kev_ids() if policy["fail_on_kev"] else set()
    scan_directory = evidence_directory / "scans"
    scan_directory.mkdir(parents=True, exist_ok=True)
    shutil.copyfile(POLICY_PATH, scan_directory / "security-policy.json")
    shutil.copyfile(VENDOR_FIXES_PATH, scan_directory / "vendor-fixes.vex.json")
    (scan_directory / "kev-input.json").write_text(json.dumps({"source": "CISA KEV", "ids": sorted(kev)}) + "\n")
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
    candidate = {"revision": revision, "source": identity.source, "images": references}
    (evidence_directory / "candidate.json").write_text(json.dumps(candidate, sort_keys=True) + "\n")
    return candidate


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
    parser.add_argument("operation", choices=("record", "push", "record-evidence", "complete"))
    parser.add_argument("revision")
    parser.add_argument("manifest", type=Path)
    parser.add_argument("--evidence-directory", type=Path, default=Path("target/release/evidence"))
    args = parser.parse_args()
    if args.operation == "record":
        images = tested_images(args.revision)
        with args.manifest.open("x") as output:
            json.dump(images, output, sort_keys=True)
            output.write("\n")
    elif args.operation == "push":
        candidate = publish_images(args.revision, json.loads(args.manifest.read_text()), args.evidence_directory)
        with Path(os.environ["GITHUB_OUTPUT"]).open("a") as output:
            for role, reference in candidate["images"].items():
                output.write(role + "_name=" + reference.split(":sha-", 1)[0] + "\n")
                output.write(role + "_digest=" + reference.split("@", 1)[1] + "\n")
    elif args.operation == "record-evidence":
        from release_evidence_record import seal
        candidate = json.loads(args.manifest.read_text())
        if candidate["revision"] != args.revision:
            raise ValueError("evidence source revision differs from publication")
        seal(candidate, args.evidence_directory)
        from release_build_materials import sha256
        Path("target/release/record-evidence.json").write_text(json.dumps(
            {"schema": 1, "revision": args.revision,
             "evidence_index_sha256": sha256(args.evidence_directory / "index.json"),
             "images": candidate["images"]}, sort_keys=True) + "\n")
        reference = publish(candidate, ReleaseIdentity.from_environment(), args.evidence_directory)
        Path("release-image.txt").write_text(reference + "\n")
        with Path(os.environ["GITHUB_OUTPUT"]).open("a") as output:
            output.write("name=" + reference.split(":sha-", 1)[0] + "\n")
            output.write("digest=" + reference.split("@", 1)[1] + "\n")
    else:
        from verify_release_evidence import verify_remote
        reference = args.manifest.read_text().strip()
        verify_remote(reference, args.revision, args.evidence_directory)
        publish_update_tag(args.revision, reference, ReleaseIdentity.from_environment())
        Path("release-image.txt").write_text(reference + "\n")


if __name__ == "__main__":
    main()
