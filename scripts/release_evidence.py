#!/usr/bin/env python3
"""Collect digest-bound image inventories, sources and notices before publication."""

import argparse
import csv
from email import policy as email_policy
from email.parser import BytesParser
import gzip
import hashlib
import io
import json
from pathlib import Path, PurePosixPath
import posixpath
import re
import shutil
import subprocess
import tarfile
from tempfile import TemporaryDirectory
from urllib.parse import quote, urlsplit
import urllib.request

from release_build_materials import auditable_dependencies, sha256
from release_record import ROLES
from release_identity import ReleaseIdentity

ROOT = Path(__file__).resolve().parents[1]
SYFT_VERSION = "1.54.1"
BINARY_NAMES = {"server": ("printable-server", "printable-geometry-worker"),
                "cad": ("printable-cad-worker",), "slicer": ("printable-slicer-worker",),
                "recon": ("printable-recon-worker",), "blender": ()}
NOTICE = re.compile(r"license|copying|copyright|notice|^OFL(?:\.|$)", re.IGNORECASE)
SOURCE_HOSTS = {"files.pythonhosted.org", "download.blender.org", "github.com", "codeload.github.com",
                "www.vtk.org", "vtk.org", "storage.googleapis.com", "launchpad.net", "launchpadlibrarian.net"}


def write_json(path, value):
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(json.dumps(value, sort_keys=True) + "\n")


def file_index(directory, excluded=()):
    result = {}
    for path in directory.rglob("*"):
        if path.is_symlink():
            raise ValueError("release evidence may not contain links")
        if path.is_file() and str(path.relative_to(directory)) not in excluded:
            result[str(path.relative_to(directory))] = {"sha256": sha256(path), "size": path.stat().st_size}
    return result


def verify_inventory_transfer(directory, revision, images, allow_generated=False):
    index = json.loads((directory / "inventory-index.json").read_text())
    if index.get("schema") != 1 or index.get("revision") != revision or index.get("images") != images:
        raise ValueError("inventory handoff does not match the tested images")
    actual = file_index(directory, ("inventory-index.json", "index.json"))
    expected = index.get("files")
    if not isinstance(expected, dict) or not expected:
        raise ValueError("inventory handoff has no file index")
    if allow_generated:
        if any(actual.get(name) != value for name, value in expected.items()):
            raise ValueError("inventory handoff has missing or altered material")
    elif actual != expected:
        raise ValueError("inventory handoff has missing, altered or unexpected material")


def canonical_name(name):
    return re.sub(r"[-_.]+", "-", name).lower()


def download_source(url, directory, expected=None):
    parsed = urlsplit(url)
    if (parsed.scheme != "https" or parsed.hostname not in SOURCE_HOSTS or parsed.username
            or parsed.password or parsed.query or parsed.fragment):
        raise ValueError("source locator must be a reviewed, credential-free HTTPS URL")
    directory.mkdir(parents=True, exist_ok=True)
    with TemporaryDirectory(dir=directory) as temporary:
        path = Path(temporary) / "source"
        digest = hashlib.sha256()
        request = urllib.request.Request(url, headers={"User-Agent": "Printable-release-evidence/1"})
        with urllib.request.urlopen(request, timeout=90) as response, path.open("wb") as output:
            final = urlsplit(response.url)
            if final.scheme != "https" or final.hostname not in SOURCE_HOSTS:
                raise ValueError("source download redirected outside approved HTTPS hosts")
            while chunk := response.read(1024 * 1024):
                digest.update(chunk)
                output.write(chunk)
        checksum = digest.hexdigest()
        if expected and checksum != expected:
            raise ValueError("source archive checksum does not match its declared digest")
        if parsed.path.endswith((".tar.gz", ".tar.xz", ".tar.bz2")):
            try:
                with tarfile.open(path) as archive:
                    if not any(member.isfile() for member in archive):
                        raise ValueError("source archive contains no files")
            except tarfile.TarError as error:
                raise ValueError("source locator did not return a source archive") from error
        destination = directory / checksum
        if not destination.exists():
            shutil.copyfile(path, destination)
    return {"url": url, "sha256": checksum, "retained": "sources/" + checksum}


def image_archive(image, destination):
    container = subprocess.check_output(["docker", "create", "--entrypoint", "/bin/true", image], text=True).strip()
    if not re.fullmatch(r"[0-9a-f]{64}", container):
        raise ValueError("Docker returned an invalid temporary container identifier")
    try:
        subprocess.run(["docker", "export", "--output", str(destination), container], check=True)
    finally:
        subprocess.run(["docker", "rm", container], check=True)


def archive_members(archive):
    members = {}
    for member in archive:
        name = member.name.removeprefix("./").rstrip("/")
        path = PurePosixPath(name)
        if path.is_absolute() or ".." in path.parts or name in members:
            raise ValueError("image filesystem has an unsafe or duplicate member")
        members[name] = member
    return members


def read_member(archive, members, name):
    name = canonical_path(members, name)
    for _ in range(32):
        member = members.get(name)
        if member is None:
            return None
        if member.isfile():
            with archive.extractfile(member) as stream:
                return stream.read()
        if member.issym() or member.islnk():
            target = member.linkname
            name = posixpath.normpath((posixpath.dirname(name) + "/" + target)
                                     if member.issym() and not target.startswith("/") else target.lstrip("/"))
            if name == ".." or name.startswith("../"):
                raise ValueError("image filesystem link escapes its root")
            continue
        return None
    raise ValueError("image filesystem has a cyclic or excessive link chain")


def canonical_path(members, name):
    """Resolve directory aliases such as Debian's /lib -> /usr/lib for file ownership."""
    for _ in range(32):
        parts = PurePosixPath(name).parts
        for index in range(1, len(parts) + 1):
            prefix = "/".join(parts[:index])
            member = members.get(prefix)
            if member and member.issym():
                target = member.linkname
                replacement = target.lstrip("/") if target.startswith("/") else posixpath.join(posixpath.dirname(prefix), target)
                name = posixpath.normpath(replacement + "/" + "/".join(parts[index:]))
                if name == ".." or name.startswith("../"):
                    raise ValueError("image ownership path escapes its root")
                break
        else:
            return name
    raise ValueError("image ownership path has an excessive link chain")


def debian_packages(status):
    packages = []
    for stanza in status.decode().split("\n\n"):
        fields = {}
        last = None
        for line in stanza.splitlines():
            if line[:1].isspace() and last:
                fields[last] += "\n" + line
            elif ": " in line:
                last, value = line.split(": ", 1)
                fields[last] = value
        if fields.get("Status") != "install ok installed":
            continue
        name, version = fields["Package"], fields["Version"]
        source = fields.get("Source", name)
        match = re.fullmatch(r"([a-z0-9][a-z0-9+.-]*)(?: \(([^\s()]+)\))?", source)
        if not match:
            raise ValueError("invalid installed Debian source identity")
        source_name, source_version = match.group(1), match.group(2) or version
        locator = "https://snapshot.debian.org/package/" + quote(source_name, safe="") + "/" + quote(source_version, safe="") + "/"
        packages.append({"name": name, "version": version, "source_name": source_name,
                         "source_version": source_version, "source": locator,
                         "architecture": fields["Architecture"]})
    if not packages:
        raise ValueError("image has no installed Debian package inventory")
    return packages


def python_source(name, version, overrides, directory):
    name = canonical_name(name)
    public_version = version.split("+", 1)[0]
    if not re.fullmatch(r"[a-z0-9-]+", name) or not re.fullmatch(r"[0-9A-Za-z.!+-]+", version):
        raise ValueError("invalid installed Python package identity")
    with urllib.request.urlopen("https://pypi.org/pypi/" + name + "/" + quote(public_version, safe="") + "/json", timeout=30) as response:
        metadata = json.load(response)
    distributions = [item for item in metadata["urls"] if item["packagetype"] == "sdist"]
    if distributions:
        item = sorted(distributions, key=lambda value: value["filename"])[0]
        url, checksum = item["url"], item["digests"]["sha256"]
        if urlsplit(url).hostname != "files.pythonhosted.org" or not re.fullmatch(r"[0-9a-f]{64}", checksum):
            raise ValueError("Python source distribution has no trustworthy locator")
        return {"url": url, "sha256": checksum}
    key = name + "@" + public_version
    if key not in overrides:
        raise ValueError("Python package has no exact reviewed source locator: " + name + "@" + version)
    urls = overrides[key]
    if isinstance(urls, str):
        return download_source(urls, directory)
    if not isinstance(urls, list) or not urls:
        raise ValueError("reviewed Python source locator is malformed")
    sources = [download_source(url, directory) for url in urls]
    return sources[0] | {"additional_sources": sources[1:]}


def add_spdx_package(document, component, source, checksum=None):
    version = component.get("version")
    for existing in document.setdefault("packages", []):
        if existing["name"] == component["name"] and existing.get("versionInfo") == version:
            existing["sourceInfo"] = source + ("#sha256=" + checksum if checksum else "")
            if component.get("license"):
                existing["licenseDeclared"] = component["license"]
            return existing["SPDXID"]
    identifier = "SPDXRef-printable-" + hashlib.sha256((component["name"] + "@" + (version or source)).encode()).hexdigest()[:24]
    package = {"SPDXID": identifier, "name": component["name"],
               "downloadLocation": source if urlsplit(source).scheme in ("http", "https") else "NOASSERTION", "filesAnalyzed": False,
               "licenseConcluded": "NOASSERTION", "licenseDeclared": component.get("license") or "NOASSERTION",
               "copyrightText": "NOASSERTION"}
    if version:
        package["versionInfo"] = version
    if component.get("description"):
        package["description"] = component["description"]
    if component.get("primaryPackagePurpose"):
        package["primaryPackagePurpose"] = component["primaryPackagePurpose"]
    package["sourceInfo"] = source
    if checksum:
        package["checksums"] = [{"algorithm": "SHA256", "checksumValue": checksum}]
    if not any(item["SPDXID"] == identifier for item in document.setdefault("packages", [])):
        document["packages"].append(package)
        document.setdefault("relationships", []).append({"spdxElementId": "SPDXRef-DOCUMENT",
                                                         "relationshipType": "DESCRIBES", "relatedSpdxElement": identifier})
    return identifier


def contains_package(document, parent, child):
    relationship = {"spdxElementId": parent, "relationshipType": "CONTAINS", "relatedSpdxElement": child}
    if relationship not in document.setdefault("relationships", []):
        document["relationships"].append(relationship)


def declared_build_materials(component, source, sources, document, notice_root=None):
    """Retain upstream declarations without asserting optional or static linkage from filenames."""
    policy = component.get("build_inventory")
    if not policy:
        return []
    records = []
    with tarfile.open(sources / source["sha256"]) as archive:
        members = archive_members(archive)
        roots = {name.split("/", 1)[0] for name in members}
        if len(roots) != 1:
            raise ValueError("bundle source archive has no unique source root")
        prefix = next(iter(roots)) + "/"
        if policy["kind"] == "blender-cmake":
            raw = read_member(archive, members, prefix + policy["path"])
            if raw is None or hashlib.sha256(raw).hexdigest() != policy["sha256"]:
                raise ValueError("declared Blender build specification differs from its reviewed source")
            values = {match[1]: match[2].strip().strip('"')
                      for match in re.finditer(r'^set\((\w+)\s+("(?:\\.|[^"\\])*"|[^()"\\]*)\s*\)', raw.decode(), re.MULTILINE)}
            license_keys = set(re.findall(r"^set\((\w+_LICENSE)\s", raw.decode(), re.MULTILINE))
            if license_keys != {key for key in values if key.endswith("_LICENSE")}:
                raise ValueError("declared Blender license inventory has unsupported assignments")
            def resolve(value):
                for _ in range(20):
                    updated = re.sub(r"\$\{([^}]+)\}", lambda match: values.get(match[1], match[0]), value)
                    if updated == value:
                        return value
                    value = updated
                raise ValueError("declared source variables do not converge")
            for key in sorted(value.removesuffix("_LICENSE") for value in values if value.endswith("_LICENSE")):
                name = values.get(key + "_NAME") or key
                version = resolve(values.get(key + "_VERSION", ""))
                license_value = resolve(values[key + "_LICENSE"])
                uri = resolve(values.get(key + "_URI", ""))
                checksum = resolve(values.get(key + "_HASH", ""))
                algorithm = values.get(key + "_HASH_TYPE")
                reviewed = next((native for native in component.get("native_components", [])
                                 if native.get("build_spec_key") == key), None)
                if not uri and reviewed:
                    uri = reviewed["source_url"]
                    checksum = reviewed["source_checksum"]["value"]
                    algorithm = reviewed["source_checksum"]["algorithm"]
                record = {"name": component["name"] + " declared build dependency: " + name,
                          "upstream_name": name, "license": license_value.removeprefix("SPDX:") if license_value != "Public Domain" else "NOASSERTION",
                          "available_license": license_value, "build_spec": policy["path"], "build_spec_key": key,
                          "source": uri if uri and "${" not in uri else source["url"],
                          "source_archive": source}
                if version and "${" not in version:
                    record["version"] = version
                if checksum and algorithm:
                    record["source_checksum"] = {"algorithm": algorithm, "value": checksum}
                records.append(record)
            licensing = []
            if policy.get("license_inventory"):
                if notice_root is None:
                    raise ValueError("declared Blender distribution license inventory is absent")
                path = policy["license_inventory"]
                text = (notice_root / path).read_text()
                license_value, license_url = "NOASSERTION", ""
                fonts = False
                for line in text.splitlines():
                    if line == "## Fonts":
                        fonts = True
                    heading = re.match(r"^## \[.+\]\((.+)\)$", line)
                    if heading:
                        fonts = False
                        license_url = heading[1]
                        identifier = re.fullmatch(r"https://spdx.org/licenses/(.+)\.html", license_url)
                        license_value = identifier[1] if identifier else "NOASSERTION"
                    row = re.match(r"^\| \[(.+?)\]\((.+)\)[¹²³]? \| ([^|]+) \|(?: (.*?) \|)?$", line)
                    if row:
                        record = {"name": row[1], "homepage": row[2], "license": license_value,
                                  "license_url": license_url, "copyright": row[4] or "NOASSERTION"}
                        record["available_license" if fonts else "declared_version"] = row[3].strip()
                        licensing.append(record)
                if not licensing:
                    raise ValueError("declared Blender distribution license inventory is empty")
                records.append({"name": component["name"] + " distribution license inventory", "license": "NOASSERTION",
                                "source": source["url"], "source_archive": source,
                                "distribution_notice": path, "declared_components": licensing})
            vendor_directories = sorted({name.removeprefix(prefix).split("/")[1] for name, member in members.items()
                                         if member.isfile() and name.startswith(prefix + "extern/")
                                         and len(name.removeprefix(prefix).split("/")) > 2})
            for upstream in vendor_directories:
                directory = "extern/" + upstream + "/"
                normalized = re.sub(r"[^a-z0-9]", "", upstream.lower())
                matches = [item for item in licensing if re.sub(r"[^a-z0-9]", "", item["name"].lower()) == normalized]
                notices = [name.removeprefix(prefix) for name, member in members.items()
                           if member.isfile() and name.startswith(prefix + directory)
                           and NOTICE.search(PurePosixPath(name).name)]
                record = {"name": component["name"] + " vendored source: " + upstream,
                          "upstream_name": upstream, "license": "NOASSERTION", "source": source["url"],
                          "source_archive": source, "source_subdirectory": directory, "source_notices": notices}
                if len(matches) == 1:
                    declaration = matches[0]
                    record["distribution_declaration"] = declaration
                    record["license"] = declaration["license"]
                    version = re.match(r"^(v?\d+(?:[._]\d+)+(?:[a-z]\d*)?)(?:$|\s)", declaration.get("declared_version", ""))
                    if version:
                        record["version"] = version[1]
                records.append(record)
        elif policy["kind"] == "orca-cmake":
            specifications = [(name, member) for name, member in members.items()
                              if member.isfile() and name.startswith(prefix + "deps/") and name.endswith(".cmake")]
            for name, member in sorted(specifications):
                raw = archive.extractfile(member).read()
                declarations = []
                for line in raw.decode().splitlines():
                    line = line.strip()
                    if re.match(r"^(URL|URL_HASH|GIT_REPOSITORY|GIT_TAG|SOURCE_DIR)\s+", line):
                        declarations.append(line.split("#", 1)[0].rstrip())
                if not declarations:
                    continue
                relative = name.removeprefix(prefix)
                upstream = relative.split("/")[1]
                record = {"name": component["name"] + " declared build dependency: " + upstream,
                          "upstream_name": upstream, "license": "NOASSERTION", "source": source["url"],
                          "source_archive": source, "build_spec": relative,
                          "build_spec_sha256": hashlib.sha256(raw).hexdigest(),
                          "source_declarations": declarations}
                urls = list(dict.fromkeys(line.split(None, 1)[1].strip('"') for line in declarations if line.startswith("URL ")))
                hashes = list(dict.fromkeys(line.split(None, 1)[1].strip('"') for line in declarations if line.startswith("URL_HASH ")))
                if len(urls) == len(hashes) == 1 and re.fullmatch(r"SHA256=[0-9a-fA-F]{64}", hashes[0]):
                    record["source"] = urls[0]
                    record["source_checksum"] = {"algorithm": "SHA256", "value": hashes[0].split("=", 1)[1].lower()}
                tags = list(dict.fromkeys(line.split(None, 1)[1].strip('"') for line in declarations if line.startswith("GIT_TAG ")))
                if len(tags) == 1:
                    record["declared_source_ref"] = tags[0]
                override = policy.get("source_overrides", {}).get(upstream)
                if override:
                    if override["declared_source_ref"] not in tags or not re.fullmatch(r"[0-9a-f]{40}", override["source_commit"]):
                        raise ValueError("declared dependency source revision differs from its reviewed reference")
                    record.update(override)
                if upstream in policy.get("declared_versions", {}):
                    version = policy["declared_versions"][upstream]
                    marker = re.sub(r"[^a-z0-9]", "", version.lower())
                    references = [re.sub(r"[^a-z0-9]", "", value.lower()) for value in [*urls, *tags]]
                    if not marker or not any(marker in reference for reference in references):
                        raise ValueError("declared dependency version has no matching source reference")
                    record["version"] = version
                records.append(record)
            vendor_directories = sorted({name.removeprefix(prefix).split("/")[1] for name, member in members.items()
                                         if member.isfile() and name.startswith(prefix + "deps_src/")})
            for upstream in vendor_directories:
                directory = "deps_src/" + upstream + "/"
                notices = [name.removeprefix(prefix) for name, member in members.items()
                           if member.isfile() and name.startswith(prefix + directory)
                           and NOTICE.search(PurePosixPath(name).name)]
                records.append({"name": component["name"] + " vendored source: " + upstream,
                                "upstream_name": upstream, "license": "NOASSERTION", "source": source["url"],
                                "source_archive": source, "source_subdirectory": directory, "source_notices": notices})
        else:
            raise ValueError("bundle declared build inventory has an unknown format")
        if not records:
            raise ValueError("bundle declared build inventory is empty")
        parent = add_spdx_package(document, component, source["url"])
        for record in records:
            record["classification"] = "declared build material"
            record["binary_linkage"] = "not independently confirmed; declarations may be optional or platform-specific"
            record["description"] = record["classification"] + "; " + record["binary_linkage"]
            if record.get("source_resolution"):
                record["description"] += "; " + record["source_resolution"]
            record["primaryPackagePurpose"] = "SOURCE"
            locator = record["source"]
            if locator == source["url"]:
                locator += "#sha256=" + source["sha256"]
            elif record.get("source_checksum"):
                checksum = record["source_checksum"]
                locator += "#" + checksum["algorithm"].lower() + "=" + checksum["value"]
            identifier = add_spdx_package(document, record, locator)
            document.setdefault("relationships", []).append({"spdxElementId": identifier,
                "relationshipType": "BUILD_DEPENDENCY_OF", "relatedSpdxElement": parent,
                "comment": record["binary_linkage"]})
    return records


def spdx_file(document, parent, path, checksum):
    identifier = "SPDXRef-file-" + hashlib.sha256(path.encode()).hexdigest()[:24]
    record = {"SPDXID": identifier, "fileName": "/" + path,
              "checksums": [{"algorithm": "SHA256", "checksumValue": checksum}],
              "licenseConcluded": "NOASSERTION", "licenseInfoInFiles": ["NOASSERTION"],
              "copyrightText": "NOASSERTION"}
    if not any(item["SPDXID"] == identifier for item in document.setdefault("files", [])):
        document["files"].append(record)
    contains_package(document, parent, identifier)


def identify_bundled_native(component, inventory, ownership, document, supplements, sources=None):
    """A distribution directory proves placement; exact reviewed files prove component identity."""
    parent = add_spdx_package(document, component, component["source_url"])
    files = {item["path"]: item for item in inventory if item.get("elf")}
    identified = set()
    for native in [component, *component.get("native_components", [])]:
        expected = native.get("native_files", [])
        if not expected:
            if native is not component:
                raise ValueError("bundled native component has no reviewed files")
            continue
        if not all(native.get(field) for field in ("name", "version", "license", "source_url")):
            raise ValueError("bundled native component has incomplete identity or source information")
        source = urlsplit(native["source_url"])
        if source.scheme != "https" or not source.hostname or source.username or source.password:
            raise ValueError("bundled native component has no public HTTPS source locator")
        native_source = native["source_url"]
        if native.get("source_checksum"):
            checksum = native["source_checksum"]
            lengths = {"MD5": 32, "SHA256": 64}
            length = lengths.get(checksum.get("algorithm"))
            if length is None or not re.fullmatch(r"[0-9a-f]{" + str(length) + "}", checksum.get("value", "")):
                raise ValueError("bundled native source checksum is malformed")
            native_source += "#" + checksum["algorithm"].lower() + "=" + checksum["value"]
        if native.get("retain_source_descriptor"):
            if sources is None or native["source_checksum"]["algorithm"] != "SHA256":
                raise ValueError("bundled native source descriptor cannot be retained")
            retained = download_source(native["source_url"], sources, native["source_checksum"]["value"])
            native = native | {"retained_source_descriptor": retained}
        if native["license"].startswith("LicenseRef-"):
            text = native.get("license_text")
            if not text:
                raise ValueError("bundled native custom license text is absent")
            document.setdefault("hasExtractedLicensingInfos", []).append({"licenseId": native["license"],
                "name": native["name"] + " license", "extractedText": text})
        native_id = add_spdx_package(document, native, native_source)
        if native is not component:
            contains_package(document, parent, native_id)
            supplements.append(native | {"distribution_owner": component["name"] + "@" + component["version"]})
        for notice in native.get("required_notices", []):
            if not any(item["path"] == notice for item in inventory):
                raise ValueError("bundled native component notice is absent")
        for expected_file in expected:
            path, checksum = expected_file["path"], expected_file["sha256"]
            if (path in identified or not path.startswith(component["notice_prefix"])
                    or not re.fullmatch(r"[0-9a-f]{64}", checksum)):
                raise ValueError("bundled native file identity is ambiguous or malformed")
            actual = files.get(path)
            if actual is None or actual["sha256"] != checksum:
                raise ValueError("bundled native file differs from its reviewed component: " + path)
            identified.add(path)
            ownership[path] = "component:" + native["name"] + "@" + native["version"]
            actual["component_spdx_id"] = native_id
            actual["distribution_owner"] = component["name"] + "@" + component["version"]
            spdx_file(document, native_id, path, checksum)
    for path, actual in files.items():
        if path.startswith(component["notice_prefix"]):
            if path not in ownership:
                raise ValueError("bundled native file has no individual component identity: " + path)
            actual["distribution_owner"] = component["name"] + "@" + component["version"]
            if path not in identified:
                name, version = ownership[path].split(":", 1)[1].rsplit("@", 1)
                matches = [package for package in document.get("packages", [])
                           if canonical_name(package["name"]) == canonical_name(name)
                           and package.get("versionInfo") == version]
                exact = [package for package in matches if package["name"] == name]
                if exact:
                    matches = exact
                if len(matches) != 1:
                    raise ValueError("bundled native inventory owner has no unique SPDX identity: " + path)
                actual["component_spdx_id"] = matches[0]["SPDXID"]
                contains_package(document, parent, matches[0]["SPDXID"])
                spdx_file(document, matches[0]["SPDXID"], path, actual["sha256"])


def identify_first_party(component, archive, members, revision, document):
    source_root = ROOT / component["source_path"]
    excluded = component.get("exclude", [])
    paths = sorted(source_root.rglob("*")) if source_root.is_dir() else [source_root]
    expected = {}
    for path in paths:
        if not path.is_file() or path.is_symlink():
            continue
        relative = path.relative_to(source_root).as_posix() if source_root.is_dir() else ""
        if ("__pycache__" in path.parts or path.suffix == ".pyc"
                or any(relative == prefix or relative.startswith(prefix + "/") for prefix in excluded)):
            continue
        installed = component["installed_path"] + ("/" + relative if relative else "")
        expected[installed] = sha256(path)
    if not expected:
        raise ValueError("first-party component source files are absent")
    installed_files = {path for path, member in members.items()
                       if (path == component["installed_path"] or path.startswith(component["installed_path"] + "/"))
                       and not member.isdir()}
    if set(expected) != installed_files:
        raise ValueError("first-party installed file set differs from its source: " + component["name"])
    for path, checksum in expected.items():
        data = read_member(archive, members, path)
        if data is None or hashlib.sha256(data).hexdigest() != checksum:
            raise ValueError("first-party installed file differs from its source: " + path)
    notice = read_member(archive, members, component["notice"])
    if notice is None or notice != (ROOT / "LICENSE").read_bytes():
        raise ValueError("first-party license notice is absent or changed")
    source = ReleaseIdentity.from_environment().source + ("/tree/" if source_root.is_dir() else "/blob/") + revision + "/" + component["source_path"]
    record = component | {"version": revision, "source": source, "files": expected,
                          "notices": ["notices/" + component["notice"]]}
    identifier = add_spdx_package(document, record, source)
    for path, checksum in expected.items():
        spdx_file(document, identifier, path, checksum)
    return record


def verify_component_inputs(component):
    recipe = (ROOT / ("blender/Dockerfile" if component["name"] in ("Blender", "VirtualGL") else "Dockerfile")).read_text()
    for field in ("binary_sha256", "source_sha256", "asset_sha256"):
        if field == "source_sha256" and component["name"] in ("Blender", "OrcaSlicer"):
            continue
        if component.get(field) and component[field] not in recipe:
            raise ValueError("reviewed component checksum differs from its build recipe")
    if component["name"] == "Blender" and "ARG BLENDER_VERSION=" + component["version"] + "\n" not in recipe:
        raise ValueError("reviewed Blender version differs from its build recipe")
    if component["name"] == "OrcaSlicer" and component["binary_url"] not in recipe:
        raise ValueError("reviewed OrcaSlicer artifact differs from its build recipe")


def collect_role(role, image, revision, output, sources):
    role_dir = output / role
    role_dir.mkdir(parents=True, exist_ok=True)
    subprocess.run(["syft", "docker:" + image, "--scope", "squashed",
                    "-o", "syft-json=" + str(role_dir / "syft.json"),
                    "-o", "spdx-json=" + str(role_dir / "sbom.spdx.json")], check=True, timeout=900)
    syft = json.loads((role_dir / "syft.json").read_text())
    if syft.get("descriptor", {}).get("version") != SYFT_VERSION or not syft.get("artifacts"):
        raise ValueError("SBOM generator version or package inventory is invalid")
    spdx = json.loads((role_dir / "sbom.spdx.json").read_text())
    if spdx.get("spdxVersion") != "SPDX-2.3":
        raise ValueError("image SBOM must use SPDX 2.3")
    detected = {(canonical_name(item["name"]), item["version"]) for item in syft["artifacts"]}
    component_manifest = json.loads((ROOT / "release/components.json").read_text())
    components = component_manifest["components"]
    overrides = json.loads((ROOT / "release/python-sources.json").read_text())
    inventory, notices, ownership, installed_python = [], [], {}, []
    supplements = []
    with TemporaryDirectory(prefix="printable-inventory-") as temporary:
        archive_path = Path(temporary) / "filesystem.tar"
        image_archive(image, archive_path)
        with tarfile.open(archive_path) as archive:
            members = archive_members(archive)
            for name, member in members.items():
                if member.isfile():
                    with archive.extractfile(member) as stream:
                        prefix = stream.read(4)
                        digest = hashlib.sha256(prefix)
                        while chunk := stream.read(1024 * 1024):
                            digest.update(chunk)
                    inventory.append({"path": name, "sha256": digest.hexdigest(), "size": member.size,
                                      "elf": prefix == b"\x7fELF"})
                    if NOTICE.search(PurePosixPath(name).name) or name.startswith("usr/share/common-licenses/"):
                        target = role_dir / "notices" / name
                        target.parent.mkdir(parents=True, exist_ok=True)
                        with archive.extractfile(member) as stream, target.open("wb") as destination:
                            shutil.copyfileobj(stream, destination)
                        notices.append(name)
                elif member.issym() or member.islnk():
                    inventory.append({"path": name, "link": member.linkname, "type": "symlink" if member.issym() else "hardlink"})
            status = read_member(archive, members, "var/lib/dpkg/status")
            if status is None:
                raise ValueError("installed Debian status file is absent")
            debian = debian_packages(status)
            for package in debian:
                name = package["name"]
                if name == "virtualgl" and role == "blender":
                    package["source"] = next(c["source_url"] for c in components if c["name"] == "VirtualGL")
                copyright_path = "usr/share/doc/" + name + "/copyright"
                copyright_data = read_member(archive, members, copyright_path)
                if copyright_data is None:
                    raise ValueError("installed Debian package has no retained copyright notice: " + name)
                target = role_dir / "notices" / copyright_path
                target.parent.mkdir(parents=True, exist_ok=True)
                target.write_bytes(copyright_data)
                package["notice"] = "notices/" + copyright_path
                lists = ["var/lib/dpkg/info/" + name + ".list",
                         "var/lib/dpkg/info/" + name + ":" + package["architecture"] + ".list"]
                for path in lists:
                    data = read_member(archive, members, path)
                    if data:
                        for filename in data.decode().splitlines():
                            ownership[canonical_path(members, filename.lstrip("/"))] = "deb:" + name + "@" + package["version"]
            for name in members:
                if not name.endswith(".dist-info/METADATA"):
                    continue
                data = read_member(archive, members, name)
                metadata = BytesParser(policy=email_policy.default).parsebytes(data)
                package_name, version = metadata["Name"], metadata["Version"]
                if not package_name or not version:
                    raise ValueError("installed Python distribution has invalid metadata")
                directory = posixpath.dirname(name)
                package_notices = [path for path in notices if path.startswith(directory + "/")]
                license_value = metadata.get("License-Expression") or metadata.get("License")
                classifiers = metadata.get_all("Classifier", [])
                if not package_notices and not license_value and not any(c.startswith("License ::") for c in classifiers):
                    raise ValueError("installed Python package has no license information: " + package_name)
                source = python_source(package_name, version, overrides, sources)
                if (canonical_name(package_name), version) not in detected:
                    raise ValueError("installed Python package is absent from the image SBOM: " + package_name)
                package = {"name": package_name, "version": version, "license": license_value,
                           "license_classifiers": [c for c in classifiers if c.startswith("License ::")],
                           "source": source, "notices": package_notices}
                installed_python.append(package)
                record = read_member(archive, members, directory + "/RECORD")
                if record is None:
                    raise ValueError("installed Python distribution has no file ownership record")
                for row in csv.reader(io.StringIO(record.decode())):
                    path = posixpath.normpath(posixpath.dirname(directory) + "/" + row[0])
                    if path == ".." or path.startswith("../"):
                        raise ValueError("Python file ownership record escapes image root")
                    ownership[canonical_path(members, path)] = "python:" + package_name + "@" + version
            build_data = read_member(archive, members, "usr/share/doc/printable/build/build.json")
            if BINARY_NAMES[role]:
                if build_data is None:
                    raise ValueError("compiled Rust build material is absent")
                build = json.loads(build_data)
                if (build.get("revision") != revision or build.get("tool") != "cargo-auditable 0.7.7"
                        or build.get("source") != ReleaseIdentity.from_environment().source):
                    raise ValueError("Rust build material has a different source or tool")
                if {component["name"] for component in build.get("native", [])} != {"Manifold", "Clipper2"}:
                    raise ValueError("compiled native dependency coverage is incomplete")
                for binary in BINARY_NAMES[role]:
                    data = read_member(archive, members, "usr/local/bin/" + binary)
                    expected = build["binaries"][binary]
                    if data is None or hashlib.sha256(data).hexdigest() != expected["sha256"]:
                        raise ValueError("compiled binary differs from its captured auditable graph")
                    ownership["usr/local/bin/" + binary] = "printable:" + revision
                    for package in expected["dependencies"]["packages"]:
                        if package.get("kind", "runtime") == "runtime":
                            info = build["packages"][package["name"] + "@" + package["version"]]
                            if (canonical_name(package["name"]), package["version"]) not in detected:
                                raise ValueError("auditable runtime dependency is absent from the image SBOM")
                            add_spdx_package(spdx, info, info["source"], info.get("sha256"))
                for component in build["native"]:
                    source = component["retained_source"]
                    data = read_member(archive, members, "usr/share/doc/printable/build/" + source["path"])
                    if data is None or hashlib.sha256(data).hexdigest() != source["sha256"]:
                        raise ValueError("retained native sources do not match the build manifest")
                    (sources / source["sha256"]).write_bytes(data)
                    component["retained_source"]["path"] = "sources/" + source["sha256"]
                    supplements.append(component)
                    add_spdx_package(spdx, component, component["source"])
                (role_dir / "build.json").write_bytes(build_data)
                toolchain = build["toolchain"]
                retained_toolchain_notices = []
                for notice in toolchain["notices"]:
                    path = "usr/share/doc/printable/build/" + notice
                    if read_member(archive, members, path) is None:
                        raise ValueError("Rust standard library notice is absent")
                    retained_toolchain_notices.append("notices/" + path)
                toolchain["notices"] = retained_toolchain_notices
                supplements.append(toolchain)
                add_spdx_package(spdx, toolchain, toolchain["source"])
                data = read_member(archive, members, "usr/share/doc/printable/build/cargo-notices.tar.gz")
                if not data:
                    raise ValueError("Rust dependency notices are absent")
                (role_dir / "cargo-notices.tar.gz").write_bytes(data)
            for component in components:
                if role not in component["roles"]:
                    continue
                verify_component_inputs(component)
                if any(read_member(archive, members, path) is None for path in component["paths"]):
                    raise ValueError("required distributed component is absent: " + component["name"])
                component_notices = [path for path in notices if path.startswith(component["notice_prefix"])]
                if not component_notices:
                    raise ValueError("required distributed component has no notice: " + component["name"])
                if any(path not in notices for path in component.get("required_notices", [])):
                    raise ValueError("required distributed component license notice is absent")
                for path, checksum in component.get("notice_sha256", {}).items():
                    data = read_member(archive, members, path)
                    if data is None or hashlib.sha256(data).hexdigest() != checksum:
                        raise ValueError("distributed component license notice differs from its reviewed source")
                if component.get("asset_sha256"):
                    data = read_member(archive, members, component["paths"][0])
                    if hashlib.sha256(data).hexdigest() != component["asset_sha256"]:
                        raise ValueError("distributed asset checksum mismatch")
                source = download_source(component["source_url"], sources, component.get("source_sha256"))
                supplements.append(component | {"source": source, "notices": component_notices})
                add_spdx_package(spdx, component, source["url"], component.get("asset_sha256"))
                supplements.extend(declared_build_materials(component, source, sources, spdx, role_dir / "notices"))
                identify_bundled_native(component, inventory, ownership, spdx, supplements, sources)
            for component in component_manifest.get("first_party_components", []):
                if role in component["roles"]:
                    supplements.append(identify_first_party(component, archive, members, revision, spdx))
            if role == "server":
                font = ROOT / "crates/printable-imaging/assets/FiraSans-Regular.ttf"
                notice = "usr/share/doc/printable/LICENSE-Fira-OFL.txt"
                if read_member(archive, members, notice) is None:
                    raise ValueError("embedded Fira font notice is absent")
                component = {"name": "Fira Sans", "version": revision, "license": "OFL-1.1",
                             "source": ReleaseIdentity.from_environment().source + "/blob/" + revision
                             + "/crates/printable-imaging/assets/FiraSans-Regular.ttf", "sha256": sha256(font), "notices": [notice]}
                supplements.append(component)
                add_spdx_package(spdx, component, component["source"], component["sha256"])
    if role in ("cad", "recon", "blender") and not installed_python:
        raise ValueError("Python runtime has no installed distribution inventory")
    for item in inventory:
        if item.get("elf"):
            if item["path"] not in ownership:
                raise ValueError("native executable or library has no inventory owner: " + item["path"])
            item["owner"] = ownership[item["path"]]
    for package in debian:
        if (canonical_name(package["name"]), package["version"]) not in detected:
            raise ValueError("installed Debian package is absent from the image SBOM: " + package["name"])
    for package in spdx.get("packages", []):
        matches = [item for item in debian if item["name"] == package["name"] and item["version"] == package.get("versionInfo")]
        if matches:
            package["sourceInfo"] = matches[0]["source"]
        matches = [item for item in installed_python if canonical_name(item["name"]) == canonical_name(package["name"])
                   and item["version"] == package.get("versionInfo")]
        if matches:
            package["sourceInfo"] = matches[0]["source"]["url"] + "#sha256=" + matches[0]["source"]["sha256"]
    with gzip.open(role_dir / "filesystem.json.gz", "wt") as stream:
        json.dump(inventory, stream, sort_keys=True)
    write_json(role_dir / "materials.json", {"schema": 1, "revision": revision, "role": role,
                                             "tested_image_id": image, "debian": debian,
                                             "python": installed_python, "supplements": supplements,
                                             "unowned_native_files": []})
    write_json(role_dir / "sbom.spdx.json", spdx)
    if (role_dir / "sbom.spdx.json").stat().st_size > 16 * 1024 * 1024:
        raise ValueError("SBOM exceeds the attestation service limit")


def collect(revision, images, output):
    if not re.fullmatch(r"[0-9a-f]{40}", revision) or set(images) != set(ROLES):
        raise ValueError("evidence requires a full revision and all runtime roles")
    if any(not re.fullmatch(r"sha256:[0-9a-f]{64}", image) for image in images.values()):
        raise ValueError("evidence requires exact tested local image IDs")
    version = subprocess.check_output(["syft", "version", "-o", "json"], text=True, timeout=30)
    if json.loads(version).get("version") != SYFT_VERSION:
        raise ValueError("the pinned Syft version is required")
    sources = output / "sources"
    sources.mkdir(parents=True, exist_ok=True)
    for role in ROLES:
        collect_role(role, images[role], revision, output, sources)
    write_json(output / "tested-images.json", {"schema": 1, "revision": revision, "images": images})
    for name in ("Cargo.toml", "Cargo.lock", "rust-toolchain.toml", "Dockerfile", "blender/Dockerfile", "cad/requirements.txt", "recon/requirements.txt",
                 "LICENSE", "THIRD_PARTY_NOTICES.md", "release/components.json", "release/python-sources.json"):
        target = output / "build-inputs" / name
        target.parent.mkdir(parents=True, exist_ok=True)
        shutil.copyfile(ROOT / name, target)
    write_json(output / "inventory-index.json", {"schema": 1, "revision": revision, "images": images,
                                                "files": file_index(output, ("inventory-index.json",))})


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("revision")
    parser.add_argument("images", type=Path)
    parser.add_argument("output", type=Path)
    arguments = parser.parse_args()
    collect(arguments.revision, json.loads(arguments.images.read_text()), arguments.output)
