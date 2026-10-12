#!/usr/bin/env python3
"""Seal a complete evidence payload after verifying every component signature."""

from release_record import ROLES
from release_evidence import file_index, write_json
from verify_release_evidence import verify_components, verify_directory


def seal(candidate, directory):
    verify_components(candidate, directory)
    required = {"candidate.json", "tested-images.json", "inventory-index.json", "scans/security-policy.json",
                "scans/vendor-fixes.vex.json", "scans/kev-input.json"}
    for role in ROLES:
        required |= {role + "/sbom.spdx.json", role + "/syft.json", role + "/materials.json",
                     role + "/filesystem.json.gz", role + "/provenance.sigstore.json",
                     role + "/sbom.sigstore.json", "scans/" + role + ".json"}
    files = file_index(directory, ("index.json",))
    if not required.issubset(files):
        raise ValueError("release evidence has mandatory omissions")
    write_json(directory / "index.json", {"schema": 1, "revision": candidate["revision"],
                                          "images": candidate["images"], "files": files})
    verify_directory(directory, candidate["revision"])
