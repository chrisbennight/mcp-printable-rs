#!/usr/bin/env python3
"""Retain the dependency metadata and native sources of the compiled binaries."""

import argparse
import hashlib
import json
from pathlib import Path
import re
import struct
import subprocess
import tarfile
import tomllib
import zlib
import urllib.request
from release_identity import ReleaseIdentity


BINARIES = ("printable-server", "printable-geometry-worker", "printable-cad-worker",
            "printable-slicer-worker", "printable-recon-worker")


def sha256(path):
    with path.open("rb") as stream:
        return hashlib.file_digest(stream, "sha256").hexdigest()


def auditable_dependencies(path):
    """Read cargo-auditable's ELF metadata after stripping, without running a binary."""
    data = path.read_bytes()
    if data[:6] != b"\x7fELF\x02\x01":
        raise ValueError("release binary is not a little-endian ELF64 file")
    offset = struct.unpack_from("<Q", data, 40)[0]
    size, count, names_index = struct.unpack_from("<HHH", data, 58)
    if size < 64 or not count or names_index >= count or offset + size * count > len(data):
        raise ValueError("invalid ELF section table")
    sections = [struct.unpack_from("<IIQQQQIIQQ", data, offset + size * i) for i in range(count)]
    names_section = sections[names_index]
    names = data[names_section[4]:names_section[4] + names_section[5]]
    for section in sections:
        start = section[0]
        end = names.find(b"\0", start)
        if end < 0:
            raise ValueError("invalid ELF section name")
        if names[start:end] == b".dep-v0":
            payload = data[section[4]:section[4] + section[5]]
            decoder = zlib.decompressobj()
            decoded = decoder.decompress(payload, 16 * 1024 * 1024)
            if not decoder.eof or decoder.unused_data:
                raise ValueError("invalid or oversized auditable metadata")
            document = json.loads(decoded)
            if not isinstance(document, dict) or not document.get("packages"):
                raise ValueError("empty auditable dependency graph")
            return document
    raise ValueError("cargo-auditable metadata is absent after stripping")


def archive_trees(output, trees):
    """Archive source trees, including applied patches, without build outputs or Git internals."""
    with tarfile.open(output, "w:gz") as archive:
        for name, root in trees:
            root = root.resolve(strict=True)
            for path in sorted(root.rglob("*")):
                relative = path.relative_to(root)
                if ".git" in relative.parts or path.is_dir():
                    continue
                if path.is_symlink() and not path.resolve().is_relative_to(root):
                    raise ValueError("source archive contains a link outside its source tree")
                archive.add(path, arcname=str(Path(name) / relative), recursive=False)


def collect(root, output, revision, repository_source):
    if not re.fullmatch(r"[0-9a-f]{40}", revision):
        raise ValueError("build materials require a full source revision")
    ReleaseIdentity(source=repository_source)
    output.mkdir(parents=True, exist_ok=True)
    metadata = json.loads(subprocess.check_output(
        ["cargo", "metadata", "--locked", "--format-version", "1"], cwd=root, text=True))
    lock = tomllib.loads((root / "Cargo.lock").read_text())
    checksums = {(p["name"], p["version"]): p.get("checksum") for p in lock["package"]}
    packages = {}
    notices = []
    for package in metadata["packages"]:
        package_source = package.get("source")
        if package_source is not None and package_source != "registry+https://github.com/rust-lang/crates.io-index":
            raise ValueError("release metadata requires a reviewed locator for non-crates.io dependencies")
        key = package["name"] + "@" + package["version"]
        checksum = checksums.get((package["name"], package["version"]))
        if package_source and not re.fullmatch(r"[0-9a-f]{64}", checksum or ""):
            raise ValueError("registry dependency has no lockfile checksum")
        locator = ("https://static.crates.io/crates/" + package["name"] + "/" + package["name"]
                   + "-" + package["version"] + ".crate") if package_source else (
                       repository_source + "/tree/" + revision)
        packages[key] = {"name": package["name"], "version": package["version"],
                         "license": package.get("license"), "source": locator, "sha256": checksum}
        package_root = Path(package["manifest_path"]).parent
        license_file = package.get("license_file")
        if license_file:
            license_path = package_root / license_file
            if not license_path.resolve(strict=True).is_relative_to(package_root.resolve()):
                raise ValueError("dependency license file is outside its source tree")
            notices.append((key + "/" + str(license_path.relative_to(package_root)), license_path))
        for path in sorted(package_root.rglob("*")):
            if path.is_file() and re.search(r"license|copying|notice", path.name, re.IGNORECASE):
                notice = (key + "/" + str(path.relative_to(package_root)), path)
                if notice not in notices:
                    notices.append(notice)
        if not package.get("license") and not package.get("license_file"):
            raise ValueError("dependency has neither declared license nor license file: " + key)
    graphs = {}
    for binary in BINARIES:
        path = root / "target/release" / binary
        graph = auditable_dependencies(path)
        for package in graph["packages"]:
            if package["name"] + "@" + package["version"] not in packages:
                raise ValueError("auditable package has no source and license metadata")
        graphs[binary] = {"sha256": sha256(path), "dependencies": graph}
    trees = []
    native = []
    build_root = root / "target/release/build"
    for path in sorted(build_root.rglob("manifold-src")):
        commit = subprocess.check_output(["git", "rev-parse", "HEAD"], cwd=path, text=True).strip()
        if not re.fullmatch(r"[0-9a-f]{40}", commit):
            raise ValueError("native source has no immutable commit")
        trees.append(("manifold", path))
        native.append({"name": "Manifold", "version": commit, "license": "Apache-2.0",
                       "source": "https://github.com/elalish/manifold/tree/" + commit,
                       "patch_sha256": hashlib.sha256(subprocess.check_output(["git", "diff", "--binary", "HEAD"], cwd=path)).hexdigest()})
    for path in sorted(build_root.rglob("clipper2-src")):
        commit = subprocess.check_output(["git", "rev-parse", "HEAD"], cwd=path, text=True).strip()
        if not re.fullmatch(r"[0-9a-f]{40}", commit):
            raise ValueError("Clipper2 source has no immutable commit")
        trees.append(("clipper2", path))
        native.append({"name": "Clipper2", "version": commit, "license": "BSL-1.0",
                       "source": "https://github.com/AngusJohnson/Clipper2/tree/" + commit,
                       "patch_sha256": hashlib.sha256(subprocess.check_output(["git", "diff", "--binary", "HEAD"], cwd=path)).hexdigest()})
    if {p["name"] for p in native} != {"Manifold", "Clipper2"} or len(trees) != 2:
        raise ValueError("expected Manifold and Clipper2 source trees were not captured")
    archive_trees(output / "native-sources.tar.gz", trees)
    with tarfile.open(output / "cargo-notices.tar.gz", "w:gz") as archive:
        for name, path in notices:
            archive.add(path, arcname=name, recursive=False)
    native_hash = sha256(output / "native-sources.tar.gz")
    for package in native:
        package["retained_source"] = {"path": "native-sources.tar.gz", "sha256": native_hash}
    options = {}
    for path in sorted(build_root.rglob("CMakeCache.txt")):
        values = [line for line in path.read_text().splitlines()
                  if re.match(r"(?:MANIFOLD_|BUILD_SHARED_LIBS:)", line)]
        if values:
            options[path.parent.name] = values
            if any(re.match(r"MANIFOLD_(?:PAR|USE_BUILTIN_TBB):[^=]+=ON$", value) for value in values):
                raise ValueError("parallel Manifold requires reviewed TBB source coverage")
    for path in sorted(build_root.rglob("_deps")):
        if any(child.is_dir() and child.name.endswith("-src") and child.name != "clipper2-src"
               for child in path.iterdir()):
            raise ValueError("native build fetched an unreviewed source dependency")
    rustc = subprocess.check_output(["rustc", "--version", "--verbose"], text=True)
    compiler = dict(line.split(": ", 1) for line in rustc.splitlines() if ": " in line)
    compiler_commit = compiler.get("commit-hash", "")
    if not re.fullmatch(r"[0-9a-f]{40}", compiler_commit):
        raise ValueError("Rust toolchain has no immutable source revision")
    toolchain_notices = output / "rust-notices"
    toolchain_notices.mkdir()
    for name in ("LICENSE-APACHE", "LICENSE-MIT", "COPYRIGHT"):
        url = "https://raw.githubusercontent.com/rust-lang/rust/" + compiler_commit + "/" + name
        with urllib.request.urlopen(url, timeout=60) as response:
            (toolchain_notices / name).write_bytes(response.read())
    toolchain = {"name": "Rust standard library", "version": compiler["release"],
                 "license": "Apache-2.0 OR MIT", "source": "https://github.com/rust-lang/rust/tree/" + compiler_commit + "/library",
                 "source_commit": compiler_commit, "notices": ["rust-notices/" + name for name in ("LICENSE-APACHE", "LICENSE-MIT", "COPYRIGHT")]}
    record = {"schema": 1, "revision": revision, "source": repository_source, "tool": "cargo-auditable 0.7.7", "rustc": rustc,
              "packages": packages, "binaries": graphs, "native": native, "native_options": options, "toolchain": toolchain}
    (output / "build.json").write_text(json.dumps(record, sort_keys=True) + "\n")


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("revision")
    parser.add_argument("--root", type=Path, default=Path("/app"))
    parser.add_argument("--output", type=Path, default=Path("/release-materials"))
    parser.add_argument("--source", default=ReleaseIdentity.source)
    arguments = parser.parse_args()
    collect(arguments.root, arguments.output, arguments.revision, arguments.source)
