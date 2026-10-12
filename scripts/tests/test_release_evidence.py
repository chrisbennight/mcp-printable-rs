"""Release evidence proves installed-image identity and fails on missing mandatory material."""

import base64
import hashlib
import io
import json
from pathlib import Path
import shutil
import struct
import subprocess
import tarfile
import tempfile
import unittest
from unittest.mock import patch
import zlib

from release_build_materials import BINARIES, auditable_dependencies, collect
from release_evidence import (archive_members, canonical_path, collect_role, debian_packages,
                              download_source, file_index, python_source, read_member, write_json)
from release_evidence_record import seal
from release_identity import ReleaseIdentity
from release_record import ROLES
from verify_release_evidence import (verify_attestation, verify_components, verify_directory,
                                     verify_record_index, verify_remote, PROVENANCE_TYPE)


REVISION = "a" * 40
IMAGE = "sha256:" + "b" * 64
STATUS = b"Package: libc6\nStatus: install ok installed\nArchitecture: amd64\nVersion: 2.41-12+b1\nSource: glibc (2.41-12)\n\n"


def elf(graph):
    names = b"\0.shstrtab\0.dep-v0\0"
    payload = zlib.compress(json.dumps(graph).encode())
    data = bytearray(64 + len(names) + len(payload))
    data[:6] = b"\x7fELF\x02\x01"
    offset = len(data)
    struct.pack_into("<Q", data, 40, offset)
    struct.pack_into("<HHH", data, 58, 64, 3, 1)
    data[64:64 + len(names)] = names
    data[64 + len(names):] = payload
    data.extend(struct.pack("<IIQQQQIIQQ", 0, 0, 0, 0, 0, 0, 0, 0, 0, 0))
    data.extend(struct.pack("<IIQQQQIIQQ", 1, 3, 0, 0, 64, len(names), 0, 0, 1, 0))
    data.extend(struct.pack("<IIQQQQIIQQ", 11, 1, 0, 0, 64 + len(names), len(payload), 0, 0, 1, 0))
    return bytes(data)


def make_archive(path, files, links=None):
    with tarfile.open(path, "w") as archive:
        for name, data in files.items():
            member = tarfile.TarInfo(name)
            member.size = len(data)
            archive.addfile(member, io.BytesIO(data))
        for name, target in (links or {}).items():
            member = tarfile.TarInfo(name)
            member.type = tarfile.SYMTYPE
            member.linkname = target
            archive.addfile(member)


class BuildMetadataTests(unittest.TestCase):
    def collect_fixture(self, root, reverse=False, cache="MANIFOLD_PAR:BOOL=OFF", extra_native=False):
        workspace = root / "crates/example"
        registry = root / "registry/dependency"
        workspace.mkdir(parents=True)
        registry.mkdir(parents=True)
        (workspace / "LICENSE").write_text("workspace MIT notice")
        (registry / "COPYRIGHT.txt").write_text("registry custom license")
        packages = [
            {"name": "example", "version": "1.0.0", "source": None, "license": "MIT",
             "manifest_path": str(workspace / "Cargo.toml")},
            {"name": "dependency", "version": "2.0.0", "license": None, "license_file": "COPYRIGHT.txt",
             "source": "registry+https://github.com/rust-lang/crates.io-index",
             "manifest_path": str(registry / "Cargo.toml")}]
        if reverse:
            packages.reverse()
        (root / "Cargo.lock").write_text('[[package]]\nname = "example"\nversion = "1.0.0"\n'
            '[[package]]\nname = "dependency"\nversion = "2.0.0"\nchecksum = "' + "c" * 64 + '"\n')
        build = root / "target/release/build/native/out"
        manifold = build / "manifold-src"
        clipper = build / "build/_deps/clipper2-src"
        for path in (manifold, clipper):
            path.mkdir(parents=True)
            (path / "LICENSE").write_text("native notice")
            (path / "source.cpp").write_text("patched source")
        (build / "build/CMakeCache.txt").write_text(cache + "\nBUILD_SHARED_LIBS:BOOL=OFF\n")
        if extra_native:
            (build / "build/_deps/unreviewed-src").mkdir()
        graph = {"packages": [{"name": p["name"], "version": p["version"], "kind": "runtime"}
                              for p in packages]}
        for binary in BINARIES:
            (root / "target/release" / binary).write_bytes(elf(graph))
        def command(argv, **kwargs):
            if argv[0] == "cargo":
                return json.dumps({"packages": packages})
            if argv[0] == "rustc":
                return "rustc 1.97.1\nrelease: 1.97.1\ncommit-hash: " + "d" * 40 + "\n"
            if argv[1] == "rev-parse":
                return REVISION + "\n"
            if argv[1:3] == ["diff", "--binary"]:
                return b"actual applied patch"
            self.fail("unexpected command: " + str(argv))
        with patch("release_build_materials.subprocess.check_output", side_effect=command), \
                patch("release_build_materials.urllib.request.urlopen", side_effect=lambda *args, **kwargs: io.BytesIO(b"Rust notice")):
            collect(root, root / "output", REVISION, ReleaseIdentity.source)
        return root / "output"

    def test_collect_preserves_repository_and_package_source_identity(self):
        for reverse in (False, True):
            with self.subTest(reverse=reverse), tempfile.TemporaryDirectory() as directory:
                output = self.collect_fixture(Path(directory), reverse=reverse)
                build = json.loads((output / "build.json").read_text())
                self.assertEqual(build["source"], ReleaseIdentity.source)
                self.assertEqual(build["packages"]["example@1.0.0"]["source"], ReleaseIdentity.source + "/tree/" + REVISION)
                self.assertEqual(build["packages"]["dependency@2.0.0"]["source"],
                                 "https://static.crates.io/crates/dependency/dependency-2.0.0.crate")
                self.assertEqual(build["packages"]["dependency@2.0.0"]["sha256"], "c" * 64)
                self.assertEqual(set(build["binaries"]), set(BINARIES))
                self.assertEqual({p["name"] for p in build["native"]}, {"Manifold", "Clipper2"})
                self.assertEqual(build["toolchain"]["source_commit"], "d" * 40)
                with tarfile.open(output / "cargo-notices.tar.gz") as archive:
                    self.assertEqual(set(archive.getnames()), {"example@1.0.0/LICENSE", "dependency@2.0.0/COPYRIGHT.txt"})
                with tarfile.open(output / "native-sources.tar.gz") as archive:
                    self.assertEqual(archive.extractfile("manifold/source.cpp").read(), b"patched source")

    def test_unreviewed_native_dependency_and_parallel_build_fail(self):
        for options in ({"cache": "MANIFOLD_PAR:BOOL=ON"}, {"extra_native": True}):
            with self.subTest(options=options), tempfile.TemporaryDirectory() as directory:
                with self.assertRaisesRegex(ValueError, "reviewed"):
                    self.collect_fixture(Path(directory), **options)

    def test_dependency_graph_survives_and_missing_metadata_fails(self):
        graph = {"packages": [{"name": "example", "version": "1.0.0", "kind": "runtime"}]}
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "binary"
            path.write_bytes(elf(graph))
            self.assertEqual(auditable_dependencies(path), graph)
            path.write_bytes(path.read_bytes().replace(b".dep-v0", b".absent"))
            with self.assertRaisesRegex(ValueError, "absent"):
                auditable_dependencies(path)

    def test_debian_sources_retain_distribution_version_and_patches(self):
        package = debian_packages(STATUS)[0]
        self.assertEqual(package["version"], "2.41-12+b1")
        self.assertEqual(package["source_version"], "2.41-12")
        self.assertEqual(package["source"], "https://snapshot.debian.org/package/glibc/2.41-12/")
        with self.assertRaises(ValueError):
            debian_packages(b"Package: removed\nStatus: deinstall ok config-files\n")

    def test_merged_usr_and_shared_copyright_paths_are_resolved_safely(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "image.tar"
            make_archive(path, {"usr/lib/example.so": b"library", "usr/share/doc/main/copyright": b"license"},
                         {"lib": "usr/lib", "usr/share/doc/alias": "main"})
            with tarfile.open(path) as archive:
                members = archive_members(archive)
                self.assertEqual(canonical_path(members, "lib/example.so"), "usr/lib/example.so")
                self.assertEqual(read_member(archive, members, "usr/share/doc/alias/copyright"), b"license")


class SourceLocatorTests(unittest.TestCase):
    def test_credentials_and_non_https_sources_fail_before_request(self):
        for url in ("https://name:credential@github.com/repo/source", "http://github.com/repo/source",
                    "https://unreviewed.example/source"):
            with self.subTest(url=url), patch("release_evidence.urllib.request.urlopen") as request:
                with self.assertRaises(ValueError):
                    download_source(url, Path("unused"))
                request.assert_not_called()

    def test_changed_source_archive_is_not_retained(self):
        response = io.BytesIO(b"changed source")
        response.url = "https://github.com/owner/repo/archive/" + REVISION + ".tar.gz"
        with tempfile.TemporaryDirectory() as directory, patch("release_evidence.urllib.request.urlopen", return_value=response):
            with self.assertRaisesRegex(ValueError, "checksum"):
                download_source(response.url, Path(directory), "0" * 64)
            self.assertEqual(list(Path(directory).iterdir()), [])

    def test_source_locator_returning_html_is_rejected(self):
        response = io.BytesIO(b"<html>download unavailable</html>")
        response.url = "https://github.com/owner/repo/archive/" + REVISION + ".tar.gz"
        with tempfile.TemporaryDirectory() as directory, patch("release_evidence.urllib.request.urlopen", return_value=response):
            with self.assertRaisesRegex(ValueError, "source archive"):
                download_source(response.url, Path(directory))
            self.assertEqual(list(Path(directory).iterdir()), [])

    def test_wheel_only_source_is_specific_to_the_installed_version(self):
        metadata = io.BytesIO(json.dumps({"urls": []}).encode())
        with patch("release_evidence.urllib.request.urlopen", return_value=metadata), \
                patch("release_evidence.download_source") as download:
            with self.assertRaisesRegex(ValueError, "reviewed source"):
                python_source("torch", "2.15.0+cpu", {"torch@2.14.1": "https://github.com/pinned"}, Path("unused"))
            download.assert_not_called()


class ImageInventoryTests(unittest.TestCase):
    def run_collector(self, directory, change=None):
        root = Path(directory)
        (root / "release").mkdir()
        write_json(root / "release/components.json", {"components": []})
        write_json(root / "release/python-sources.json", {})
        (root / "crates/printable-imaging/assets").mkdir(parents=True)
        (root / "crates/printable-imaging/assets/FiraSans-Regular.ttf").write_bytes(b"font")
        graph = {"packages": [{"name": "used", "version": "1.0.0", "kind": "runtime"}]}
        binary = elf(graph)
        native = b"retained native sources"
        checksum = hashlib.sha256(native).hexdigest()
        build = {"revision": REVISION, "source": ReleaseIdentity.source, "tool": "cargo-auditable 0.7.7",
                 "packages": {"used@1.0.0": {"name": "used", "version": "1.0.0", "license": "MIT",
                                             "source": "https://static.crates.io/crates/used/used-1.0.0.crate"},
                              "unused@2.0.0": {"name": "unused", "version": "2.0.0", "license": "MIT", "source": "unused"}},
                 "binaries": {name: {"sha256": hashlib.sha256(binary).hexdigest(), "dependencies": graph}
                              for name in ("printable-server", "printable-geometry-worker")},
                 "toolchain": {"name": "Rust standard library", "version": "1.97.1", "license": "Apache-2.0 OR MIT",
                               "source": "https://github.com/rust-lang/rust/tree/" + REVISION + "/library", "notices": ["rust-notices/LICENSE-MIT"]},
                 "native": [{"name": "Manifold", "version": REVISION, "license": "Apache-2.0", "source": "commit",
                             "retained_source": {"path": "native-sources.tar.gz", "sha256": checksum}},
                            {"name": "Clipper2", "version": REVISION, "license": "BSL-1.0", "source": "commit",
                             "retained_source": {"path": "native-sources.tar.gz", "sha256": checksum}}]}
        files = {"var/lib/dpkg/status": STATUS, "usr/share/doc/libc6/copyright": b"Debian copyright",
                 "var/lib/dpkg/info/libc6:amd64.list": b"/lib/libc.so\n", "usr/lib/libc.so": b"\x7fELFnative",
                 "usr/local/bin/printable-server": binary, "usr/local/bin/printable-geometry-worker": binary,
                 "usr/share/doc/printable/build/build.json": json.dumps(build).encode(),
                 "usr/share/doc/printable/build/native-sources.tar.gz": native,
                 "usr/share/doc/printable/build/cargo-notices.tar.gz": b"dependency notices",
                 "usr/share/doc/printable/build/rust-notices/LICENSE-MIT": b"Rust MIT",
                 "usr/share/doc/printable/LICENSE-Fira-OFL.txt": b"font license"}
        if change:
            change(files)
        fixture = root / "image.tar"
        make_archive(fixture, files, {"lib": "usr/lib"})
        def scanner(command, **kwargs):
            for value in command:
                if value.startswith("syft-json="):
                    write_json(Path(value.split("=", 1)[1]), {"descriptor": {"version": "1.54.1"},
                        "artifacts": [{"name": "libc6", "version": "2.41-12+b1"}, {"name": "used", "version": "1.0.0"}]})
                if value.startswith("spdx-json="):
                    write_json(Path(value.split("=", 1)[1]), {"spdxVersion": "SPDX-2.3", "SPDXID": "SPDXRef-DOCUMENT", "packages": []})
        def export(image, destination):
            self.assertEqual(image, IMAGE)
            shutil.copyfile(fixture, destination)
        output = root / "output"
        sources = output / "sources"
        sources.mkdir(parents=True)
        with patch("release_evidence.ROOT", root), patch("release_evidence.image_archive", side_effect=export), \
                patch("release_evidence.subprocess.run", side_effect=scanner):
            collect_role("server", IMAGE, REVISION, output, sources)
        return output

    def test_inventory_uses_compiled_graph_and_retains_actual_native_source(self):
        with tempfile.TemporaryDirectory() as directory:
            output = self.run_collector(directory)
            spdx = json.loads((output / "server/sbom.spdx.json").read_text())
            names = {package["name"] for package in spdx["packages"]}
            self.assertIn("used", names)
            self.assertNotIn("unused", names)
            self.assertIn("Manifold", names)
            self.assertIn("Fira Sans", names)
            material = json.loads((output / "server/materials.json").read_text())
            self.assertEqual(material["tested_image_id"], IMAGE)
            self.assertEqual(material["unowned_native_files"], [])
            self.assertTrue(list((output / "sources").iterdir()))

    def test_unowned_native_library_stops_publication_preparation(self):
        with tempfile.TemporaryDirectory() as directory:
            with self.assertRaisesRegex(ValueError, "no inventory owner"):
                self.run_collector(directory, lambda files: files.update({"opt/unknown.so": b"\x7fELFunknown"}))

    def test_missing_native_sources_and_changed_binary_fail(self):
        for change in (lambda files: files.pop("usr/share/doc/printable/build/native-sources.tar.gz"),
                       lambda files: files.update({"usr/local/bin/printable-server": b"\x7fELFchanged"})):
            with self.subTest(change=change), tempfile.TemporaryDirectory() as directory:
                with self.assertRaises(ValueError):
                    self.run_collector(directory, change)


class SignedEvidenceTests(unittest.TestCase):
    def fixture(self, directory):
        identity = ReleaseIdentity()
        candidate = {"revision": REVISION, "source": identity.source,
                     "images": {role: identity.repository(role) + ":sha-" + REVISION[:12] + "@" + IMAGE for role in ROLES}}
        write_json(directory / "candidate.json", candidate)
        write_json(directory / "tested-images.json", {"schema": 1, "revision": REVISION,
                                                     "images": {role: IMAGE for role in ROLES}})
        write_json(directory / "scans/security-policy.json", {"fail_on_severities": ["high"],
                   "ignored_vulnerabilities": [], "severity_requires_fix": True, "fail_on_kev": True})
        write_json(directory / "scans/vendor-fixes.vex.json", {})
        write_json(directory / "scans/kev-input.json", {"ids": []})
        for role in ROLES:
            write_json(directory / "scans" / (role + ".json"), {"revision": REVISION, "role": role,
                                                                "image_id": IMAGE, "report": {"matches": []}})
            write_json(directory / role / "materials.json", {"revision": REVISION, "role": role,
                       "tested_image_id": IMAGE, "debian": [{"name": "fixture"}], "unowned_native_files": []})
            for name in ("sbom.spdx.json", "syft.json", "provenance.sigstore.json", "sbom.sigstore.json"):
                write_json(directory / role / name, {})
            (directory / role / "filesystem.json.gz").write_bytes(b"inventory")
        write_json(directory / "inventory-index.json", {"schema": 1, "revision": REVISION,
                  "images": {role: IMAGE for role in ROLES}, "files": file_index(directory)})
        return candidate

    def test_sealed_index_detects_changed_missing_and_added_material(self):
        for action in ("change", "delete", "add"):
            with self.subTest(action=action), tempfile.TemporaryDirectory() as temporary:
                root = Path(temporary)
                candidate = self.fixture(root)
                with patch("release_evidence_record.verify_components"):
                    seal(candidate, root)
                self.assertEqual(verify_directory(root, REVISION), candidate)
                path = root / "cad/materials.json"
                if action == "change": path.write_text("changed")
                elif action == "delete": path.unlink()
                else: (root / "unindexed").write_text("added")
                with self.assertRaisesRegex(ValueError, "missing, altered or unindexed"):
                    verify_directory(root, REVISION)

    def test_missing_signature_and_failed_verification_never_seal(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            candidate = self.fixture(root)
            (root / "cad/sbom.sigstore.json").unlink()
            with patch("release_evidence_record.verify_components"), self.assertRaisesRegex(ValueError, "omissions"):
                seal(candidate, root)
            self.assertFalse((root / "index.json").exists())
            with patch("release_evidence_record.verify_components", side_effect=ValueError("bad signer")), self.assertRaises(ValueError):
                seal(candidate, root)
            self.assertFalse((root / "index.json").exists())

    def test_consumer_verifier_enforces_workflow_ref_source_and_digest(self):
        reference = ReleaseIdentity().repository("server") + ":sha-" + REVISION[:12] + "@" + IMAGE
        payload = base64.b64encode(json.dumps({"predicateType": PROVENANCE_TYPE}).encode()).decode()
        result = subprocess.CompletedProcess([], 0, stdout=json.dumps(
            [{"attestation": {"bundle": {"dsseEnvelope": {"payload": payload}}}}]))
        with patch("verify_release_evidence.subprocess.run", return_value=result) as run:
            verify_attestation(reference, REVISION, PROVENANCE_TYPE, Path("bundle.json"))
        arguments = run.call_args.args[0]
        self.assertIn("oci://" + reference, arguments)
        self.assertEqual(arguments[arguments.index("--source-digest") + 1], REVISION)
        self.assertEqual(arguments[arguments.index("--source-ref") + 1], "refs/heads/main")
        self.assertIn("--deny-self-hosted-runners", arguments)
        self.assertEqual(arguments[arguments.index("--signer-workflow") + 1],
                         "chrisbennight/mcp-printable-rs/.github/workflows/ci.yml")

    def test_sbom_contents_must_match_verified_signed_predicate(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            candidate = self.fixture(root)
            payload = base64.b64encode(json.dumps({"predicate": {"name": "signed"}}).encode()).decode()
            write_json(root / "server/sbom.sigstore.json", {"dsseEnvelope": {"payload": payload}})
            with patch("verify_release_evidence.verify_attestation", return_value=[{"predicate": {"name": "signed"}}]), self.assertRaisesRegex(ValueError, "retained SBOM"):
                verify_components(candidate, root)

    def test_independently_retained_index_must_match_signed_record_even_if_rehashed(self):
        from release_build_materials import sha256
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            candidate = self.fixture(root)
            with patch("release_evidence_record.verify_components"):
                seal(candidate, root)
            statement = {"predicate": {"schema": 1, "revision": REVISION,
                                       "evidence_index_sha256": sha256(root / "index.json"),
                                       "images": candidate["images"]}}
            verify_record_index([statement], root, REVISION)
            with patch("release_evidence_record.verify_components"):
                (root / "added-material").write_text("unauthenticated")
                seal(candidate, root)
            self.assertEqual(verify_directory(root, REVISION), candidate)
            with self.assertRaisesRegex(ValueError, "verified signed record"):
                verify_record_index([statement], root, REVISION)

    def test_record_signature_failure_prevents_fetching_untrusted_payload(self):
        with patch("verify_release_evidence.verify_attestation", side_effect=subprocess.CalledProcessError(1, ["gh"])), \
                patch("verify_release_evidence.download_evidence") as download:
            with self.assertRaises(subprocess.CalledProcessError):
                verify_remote("reference", REVISION)
            download.assert_not_called()
