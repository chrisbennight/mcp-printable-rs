#!/usr/bin/env python3
"""Generate standard Syft SBOMs and retain a small declared component supplement."""

import argparse
import json
from pathlib import Path
import re
import shutil
import subprocess
from urllib.parse import urlsplit

from release_record import ROLES


ROOT = Path(__file__).resolve().parents[1]
SOURCE = "https://github.com/chrisbennight/mcp-printable-rs"
SYFT_VERSION = "1.54.1"


def repository_file(name):
    if not isinstance(name, str) or not name or "\\" in name:
        raise ValueError("supplement requires a repository-relative file path")
    path = Path(name)
    if path.is_absolute() or ".." in path.parts:
        raise ValueError("supplement file is outside the repository")
    source = (ROOT / path).resolve(strict=True)
    if not source.is_relative_to(ROOT.resolve()) or not source.is_file():
        raise ValueError("supplement file is outside the repository or is not a file")
    return source


def source_url(value):
    if not isinstance(value, str):
        raise ValueError("supplement requires an HTTPS source locator")
    parsed = urlsplit(value)
    if (parsed.scheme != "https" or not parsed.hostname or parsed.username
            or parsed.password or parsed.query or parsed.fragment):
        raise ValueError("supplement requires a credential-free HTTPS source locator")


def supplement(revision):
    document = json.loads((ROOT / "release/components.json").read_text())
    if (document.get("schema") != 1 or document.get("scope") != "declared"
            or not document.get("limitations") or not document.get("components")):
        raise ValueError("component supplement must declare its scope and limits")
    for component in document["components"]:
        if any(not isinstance(component.get(key), str) or not component[key].strip()
               for key in ("name", "version", "license")):
            raise ValueError("declared component has no identity or license")
        roles = component.get("roles")
        if not isinstance(roles, list) or not roles or any(role not in ROLES for role in roles):
            raise ValueError("declared component has invalid runtime roles")
        recipes = component.get("recipe_paths")
        if not isinstance(recipes, list) or not recipes:
            raise ValueError("declared component requires a source recipe locator")
        for name in recipes:
            repository_file(name)
        component["recipe_urls"] = [SOURCE + "/blob/" + revision + "/" + name for name in recipes]
        if "source_path" in component:
            repository_file(component["source_path"])
            component["source_url"] = SOURCE + "/blob/" + revision + "/" + component["source_path"]
        source_url(component.get("source_url"))
        for key in ("binary_url", "patch_url", "binding_source_url", "upstream_recipe_url"):
            if key in component:
                source_url(component[key])
        for url in component.get("notice_urls", []):
            source_url(url)
        for key in ("source_sha256", "binary_sha256", "binding_source_sha256"):
            if key in component and (not isinstance(component[key], str)
                                     or not re.fullmatch(r"[0-9a-f]{64}", component[key])):
                raise ValueError("declared component has an invalid checksum")
        if "source_commit" in component and (not isinstance(component["source_commit"], str)
                                             or not re.fullmatch(r"[0-9a-f]{40}", component["source_commit"])):
            raise ValueError("declared component has an invalid source revision")
    notices = document.get("notice_paths")
    if not isinstance(notices, list) or not notices:
        raise ValueError("component supplement requires retained notices")
    for name in notices:
        repository_file(name)
    document["revision"] = revision
    document["repository_url"] = SOURCE + "/tree/" + revision
    return document


def collect(revision, images, output):
    if (not isinstance(revision, str) or not re.fullmatch(r"[0-9a-f]{40}", revision)
            or not isinstance(images, dict) or set(images) != set(ROLES)):
        raise ValueError("evidence requires a full revision and all runtime roles")
    if any(not isinstance(image, str) or not re.fullmatch(r"sha256:[0-9a-f]{64}", image)
           for image in images.values()):
        raise ValueError("evidence requires exact tested local image IDs")
    declared = supplement(revision)
    version = subprocess.check_output(["syft", "version", "-o", "json"], text=True, timeout=30)
    if json.loads(version).get("version") != SYFT_VERSION:
        raise ValueError("the pinned Syft version is required")
    output.mkdir(parents=True, exist_ok=False)
    for role in ROLES:
        role_directory = output / role
        role_directory.mkdir()
        sbom = role_directory / "sbom.spdx.json"
        subprocess.run(["syft", "docker:" + images[role], "--scope", "squashed",
                        "-o", "spdx-json=" + str(sbom)], check=True, timeout=900)
        if sbom.stat().st_size > 16 * 1024 * 1024:
            raise ValueError("SBOM exceeds the attestation service limit")
        if json.loads(sbom.read_text()).get("spdxVersion") != "SPDX-2.3":
            raise ValueError("Syft did not produce the expected SPDX document")
    for name in declared["notice_paths"]:
        destination = output / "notices" / name
        destination.parent.mkdir(parents=True, exist_ok=True)
        shutil.copyfile(repository_file(name), destination)
    (output / "components.json").write_text(json.dumps(declared, indent=2) + "\n")
    (output / "tested-images.json").write_text(json.dumps(
        {"schema": 1, "revision": revision, "images": images}, indent=2) + "\n")


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("revision")
    parser.add_argument("images", type=Path)
    parser.add_argument("output", type=Path)
    arguments = parser.parse_args()
    collect(arguments.revision, json.loads(arguments.images.read_text()), arguments.output)
