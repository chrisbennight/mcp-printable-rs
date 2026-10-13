#!/usr/bin/env python3
"""Independently verify a Printable release and its retained evidence."""

import argparse
import base64
import json
import os
from pathlib import Path
import re
import shutil
import subprocess
import tarfile
from tempfile import NamedTemporaryFile, TemporaryDirectory

from release_build_materials import sha256
from release_identity import ReleaseIdentity
from release_record import ROLES, validate
from release_security import evaluate_report

PROVENANCE_TYPE = "https://slsa.dev/provenance/v1"
SBOM_TYPE = "https://spdx.dev/Document/v2.3"
EVIDENCE_TYPE = "https://github.com/chrisbennight/mcp-printable-rs/attestations/release-evidence/v1"


def verify_attestation(reference, revision, predicate_type, bundle=None, trusted_root=None,
                       verified_bundles=None):
    if not re.fullmatch(r"[0-9a-f]{40}", revision):
        raise ValueError("verification requires a full expected source revision")
    identity = ReleaseIdentity.from_environment()
    repository = identity.source.removeprefix("https://github.com/")
    if not re.fullmatch(r"[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+", repository):
        raise ValueError("attestation verification requires a GitHub source repository")
    if not any(identity.reference_pattern(role).fullmatch(reference) for role in (*ROLES, "pair")):
        raise ValueError("verification requires an immutable Printable image reference")
    command = ["gh", "attestation", "verify", "oci://" + reference, "--repo", repository,
               "--signer-workflow", repository + "/.github/workflows/ci.yml",
               "--source-ref", "refs/heads/main", "--source-digest", revision,
               "--deny-self-hosted-runners", "--predicate-type", predicate_type, "--format", "json"]
    if bundle:
        command += ["--bundle", str(bundle)]
    else:
        command += ["--bundle-from-oci"]
    if trusted_root:
        command += ["--custom-trusted-root", str(trusted_root)]
    result = subprocess.run(command, check=True, capture_output=True, text=True, timeout=180)
    verified = json.loads(result.stdout)
    if not isinstance(verified, list) or not verified:
        raise ValueError("attestation verifier returned no verified result")
    statements = []
    bundles = []
    for item in verified:
        # Parse only the bundles returned by the verifier, rather than other lines
        # that may accompany a valid bundle in a caller-provided JSONL file.
        verified_bundle = item["attestation"]["bundle"]
        envelope = verified_bundle["dsseEnvelope"]
        statement = json.loads(base64.b64decode(envelope["payload"], validate=True))
        if statement.get("predicateType") != predicate_type:
            raise ValueError("verified attestation has an unexpected predicate type")
        statements.append(statement)
        bundles.append(verified_bundle)
    if verified_bundles is not None:
        verified_bundles.extend(bundles)
    return statements


def verify_components(candidate, directory, trusted_root=None):
    identity = ReleaseIdentity.from_environment()
    validate(candidate, identity)
    for role, reference in candidate["images"].items():
        provenance = directory / role / "provenance.sigstore.json"
        sbom_bundle = directory / role / "sbom.sigstore.json"
        verify_attestation(reference, candidate["revision"], PROVENANCE_TYPE, provenance, trusted_root)
        statements = verify_attestation(reference, candidate["revision"], SBOM_TYPE, sbom_bundle, trusted_root)
        if not any(statement.get("predicate") == json.loads((directory / role / "sbom.spdx.json").read_text()) for statement in statements):
            raise ValueError("retained SBOM does not match its verified attestation")


def verify_record_index(statements, directory, revision):
    checksum = sha256(directory / "index.json")
    index = json.loads((directory / "index.json").read_text())
    expected = {"schema": 1, "revision": revision, "evidence_index_sha256": checksum, "images": index["images"]}
    if not any(statement.get("predicate") == expected for statement in statements):
        raise ValueError("retained evidence index differs from the verified signed record")


def verify_directory(directory, revision):
    identity = ReleaseIdentity.from_environment()
    candidate = json.loads((directory / "candidate.json").read_text())
    validate(candidate, identity)
    if candidate["revision"] != revision:
        raise ValueError("release source differs from the expected revision")
    index = json.loads((directory / "index.json").read_text())
    if index.get("schema") != 1 or index.get("revision") != revision or index.get("images") != candidate["images"]:
        raise ValueError("evidence index does not match the release")
    from release_evidence import file_index, verify_inventory_transfer
    actual = file_index(directory, ("index.json",))
    if actual != index.get("files"):
        raise ValueError("release evidence has missing, altered or unindexed files")
    tested = json.loads((directory / "tested-images.json").read_text())
    if tested.get("revision") != revision or set(tested.get("images", {})) != set(ROLES):
        raise ValueError("release evidence has no complete tested-image handoff")
    verify_inventory_transfer(directory, revision, tested["images"], allow_generated=True)
    policy = json.loads((directory / "scans/security-policy.json").read_text())
    kev = set(json.loads((directory / "scans/kev-input.json").read_text())["ids"])
    for role in ROLES:
        materials = json.loads((directory / role / "materials.json").read_text())
        scan = json.loads((directory / "scans" / (role + ".json")).read_text())
        for document in (materials, scan):
            image = document.get("tested_image_id", document.get("image_id"))
            if document.get("revision") != revision or document.get("role") != role or image != tested["images"][role]:
                raise ValueError("image inventory or scan differs from the tested image")
        if materials.get("unowned_native_files") != [] or not materials.get("debian"):
            raise ValueError("mandatory image inventory coverage is incomplete")
        if evaluate_report(scan["report"], policy, kev).blocking:
            raise ValueError("retained scan does not pass its retained vulnerability policy")
    return candidate


def download_evidence(reference, destination):
    from release_evidence import image_archive
    destination.mkdir(parents=True, exist_ok=False)
    subprocess.run(["docker", "pull", "--platform", "linux/amd64", reference], check=True)
    with TemporaryDirectory(prefix="printable-record-") as temporary:
        path = Path(temporary) / "record.tar"
        image_archive(reference, path)
        with tarfile.open(path) as archive:
            seen = set()
            for member in archive:
                name = member.name.removeprefix("./")
                if not name.startswith("evidence/") or member.isdir():
                    continue
                relative = Path(name.removeprefix("evidence/"))
                if (not member.isfile() or relative.is_absolute() or ".." in relative.parts
                        or str(relative) in seen):
                    raise ValueError("release record has an unsafe evidence entry")
                seen.add(str(relative))
                target = destination / relative
                target.parent.mkdir(parents=True, exist_ok=True)
                with archive.extractfile(member) as stream, target.open("wb") as output:
                    shutil.copyfileobj(stream, output)


def validate_retention_paths(retain, record_bundle_output):
    if retain is not None and (retain.exists() or retain.is_symlink()):
        raise ValueError("retained evidence requires a new directory")
    if retain is not None:
        for parent in retain.parents:
            if parent.exists() or parent.is_symlink():
                if not parent.is_dir():
                    raise ValueError("retained evidence requires a directory parent")
                break
    if record_bundle_output is not None:
        if retain is None:
            raise ValueError("--record-bundle-output requires --retain")
        if record_bundle_output.exists() or record_bundle_output.is_symlink():
            raise ValueError("record bundle output requires a new file")
        if not record_bundle_output.parent.is_dir():
            raise ValueError("record bundle output requires an existing parent directory")
        evidence_path = retain.resolve()
        bundle_path = record_bundle_output.resolve()
        if (evidence_path == bundle_path or evidence_path in bundle_path.parents
                or bundle_path in evidence_path.parents):
            raise ValueError("record bundle output must be outside the retained evidence directory")


def verify_remote(reference, revision, directory=None, trusted_root=None, retain=None,
                  record_bundle_output=None):
    validate_retention_paths(retain, record_bundle_output)
    bundles = [] if record_bundle_output is not None else None
    verify_attestation(reference, revision, PROVENANCE_TYPE, trusted_root=trusted_root,
                       verified_bundles=bundles)
    statements = verify_attestation(reference, revision, EVIDENCE_TYPE, trusted_root=trusted_root,
                                    verified_bundles=bundles)
    with TemporaryDirectory(prefix="printable-verify-") as temporary:
        evidence = Path(temporary) / "evidence"
        download_evidence(reference, evidence)
        verify_record_index(statements, evidence, revision)
        candidate = verify_directory(evidence, revision)
        if directory is not None:
            # Compare the published layer with the verified publication candidate.
            if (evidence / "index.json").read_bytes() != (directory / "index.json").read_bytes():
                raise ValueError("published evidence differs from the verified publication candidate")
        verify_components(candidate, evidence, trusted_root)
        if retain is not None:
            shutil.copytree(evidence, retain)
        if record_bundle_output is not None:
            with NamedTemporaryFile(mode="w", dir=record_bundle_output.parent,
                                    prefix=".printable-record-bundles-") as output:
                for bundle in bundles:
                    output.write(json.dumps(bundle, separators=(",", ":")) + "\n")
                output.flush()
                # Publish complete verified bytes without replacing an existing file.
                os.link(output.name, record_bundle_output)


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("reference")
    parser.add_argument("revision", help="expected full source commit, obtained independently")
    parser.add_argument("--retain", type=Path, help="save verified evidence in a new directory")
    parser.add_argument("--record-bundle-output", type=Path,
                        help="with --retain, save verified record bundles in a new file outside that directory")
    parser.add_argument("--evidence-directory", type=Path, help="verify an already retained evidence directory")
    parser.add_argument("--record-bundle", type=Path, help="previously saved signature bundle for the release record")
    parser.add_argument("--trusted-root", type=Path, help="independently trusted Sigstore root for retained verification")
    arguments = parser.parse_args()
    if arguments.evidence_directory:
        if (not arguments.record_bundle or not arguments.trusted_root or arguments.retain
                or arguments.record_bundle_output):
            parser.error("retained verification requires --record-bundle and --trusted-root, "
                         "without --retain or --record-bundle-output")
        verify_attestation(arguments.reference, arguments.revision, PROVENANCE_TYPE,
                           arguments.record_bundle, arguments.trusted_root)
        statements = verify_attestation(arguments.reference, arguments.revision, EVIDENCE_TYPE,
                                        arguments.record_bundle, arguments.trusted_root)
        verify_record_index(statements, arguments.evidence_directory, arguments.revision)
        candidate = verify_directory(arguments.evidence_directory, arguments.revision)
        verify_components(candidate, arguments.evidence_directory, arguments.trusted_root)
    else:
        if arguments.record_bundle or arguments.trusted_root:
            parser.error("--record-bundle and --trusted-root require --evidence-directory")
        try:
            validate_retention_paths(arguments.retain, arguments.record_bundle_output)
        except ValueError as error:
            parser.error(str(error))
        verify_remote(arguments.reference, arguments.revision, retain=arguments.retain,
                      record_bundle_output=arguments.record_bundle_output)
    print("Release, component provenance, SBOM signatures and evidence hashes verified.")
