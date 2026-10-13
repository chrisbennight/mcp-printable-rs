"""Publish the exact images saved by the successful container test job."""

import argparse
import json
import os
from pathlib import Path
import re
import subprocess
import shutil
from tempfile import TemporaryDirectory
from urllib.parse import urlsplit

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
    transfer = json.loads((evidence_directory / "tested-images.json").read_text())
    if transfer != {"schema": 1, "revision": revision, "images": images}:
        raise ValueError("image inventories differ from the tested image IDs")
    for role in ROLES:
        sbom = json.loads((evidence_directory / role / "sbom.spdx.json").read_text())
        if sbom.get("spdxVersion") != "SPDX-2.3":
            raise ValueError("required image SBOM is not an SPDX document")
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


def verify_attestation(reference, revision, identity, predicate_type="https://slsa.dev/provenance/v1"):
    if not re.fullmatch(r"[0-9a-f]{40}", revision):
        raise ValueError("source revision must be a full Git commit")
    source = urlsplit(identity.source)
    if source.hostname != "github.com":
        raise ValueError("GitHub attestations require a GitHub source repository")
    repository = source.path.strip("/")
    subprocess.run([
        "gh", "attestation", "verify", "oci://" + reference, "--bundle-from-oci",
        "--repo", repository, "--signer-workflow", repository + "/.github/workflows/ci.yml",
        "--source-ref", "refs/heads/main", "--source-digest", revision,
        "--deny-self-hosted-runners", "--predicate-type", predicate_type,
    ], check=True)


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
    parser.add_argument("operation", choices=("record", "push", "record-release", "complete"))
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
    elif args.operation == "record-release":
        candidate = json.loads(args.manifest.read_text())
        if candidate["revision"] != args.revision:
            raise ValueError("evidence source revision differs from publication")
        identity = ReleaseIdentity.from_environment()
        from release_record import validate
        validate(candidate, identity)
        for reference in candidate["images"].values():
            verify_attestation(reference, args.revision, identity)
            verify_attestation(reference, args.revision, identity, "https://spdx.dev/Document/v2.3")
        reference = publish(candidate, identity, args.evidence_directory)
        Path("release-image.txt").write_text(reference + "\n")
        with Path(os.environ["GITHUB_OUTPUT"]).open("a") as output:
            output.write("name=" + reference.split(":sha-", 1)[0] + "\n")
            output.write("digest=" + reference.split("@", 1)[1] + "\n")
    else:
        reference = args.manifest.read_text().strip()
        identity = ReleaseIdentity.from_environment()
        if identity.reference_pattern("pair").fullmatch(reference) is None:
            raise ValueError("release record has an unexpected reference")
        verify_attestation(reference, args.revision, identity)
        publish_update_tag(args.revision, reference, identity)
        Path("release-image.txt").write_text(reference + "\n")


if __name__ == "__main__":
    main()
