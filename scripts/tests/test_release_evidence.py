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
                              declared_build_materials, download_source, file_index, identify_bundled_native, identify_first_party,
                              python_source, read_member, verify_component_inputs, write_json)
from release_evidence_record import seal
from release_identity import ReleaseIdentity
from release_native_materials import capture, validate_record
from release_record import ROLES
from verify_release_evidence import (verify_attestation, verify_components, verify_directory,
                                     verify_record_index, verify_remote, PROVENANCE_TYPE)


REVISION = "a" * 40
IMAGE = "sha256:" + "b" * 64
STATUS = b"Package: libc6\nStatus: install ok installed\nArchitecture: amd64\nVersion: 2.41-12+b1\nSource: glibc (2.41-12)\n\n"
KEYRING_STATUS = (b"Package: debian-archive-keyring\nStatus: install ok installed\nArchitecture: all\n"
                  b"Version: 2025.1\nConffiles:\n"
                  b" /etc/apt/trusted.gpg.d/debian-archive-trixie-stable.asc 91c5c0b4f6878239622087c64866c909\n"
                  b"Description: Debian archive certificates\n The release signing certificates.\n\n")


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

    def test_empty_debian_field_keeps_continuations_out_of_package_identity(self):
        removed = b"Package: removed\nStatus: deinstall ok config-files\nConffiles:\n /etc/removed.conf digest\n\n"
        packages = debian_packages(STATUS + KEYRING_STATUS + removed)
        self.assertEqual([item["name"] for item in packages], ["libc6", "debian-archive-keyring"])
        keyring = packages[1]
        self.assertEqual(keyring["version"], "2025.1")
        self.assertEqual(keyring["source_name"], "debian-archive-keyring")
        self.assertEqual(keyring["source_version"], "2025.1")
        self.assertEqual(keyring["source"], "https://snapshot.debian.org/package/debian-archive-keyring/2025.1/")
        self.assertEqual(keyring["architecture"], "all")

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
    def run_collector(self, directory, change=None, components=None, recipe=None, scanner_artifacts=None):
        root = Path(directory)
        (root / "release").mkdir()
        write_json(root / "release/components.json", {"components": components or []})
        (root / "Dockerfile").write_text(recipe if recipe is not None else "\n".join(
            component["asset_sha256"] for component in components or [] if component.get("asset_sha256")))
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
        artifacts = scanner_artifacts if scanner_artifacts is not None else [
            {"name": "libc6", "version": "2.41-12+b1"}, {"name": "used", "version": "1.0.0"}]
        def scanner(command, **kwargs):
            for value in command:
                if value.startswith("syft-json="):
                    write_json(Path(value.split("=", 1)[1]), {"descriptor": {"version": "1.54.1"},
                        "artifacts": artifacts})
                if value.startswith("spdx-json="):
                    write_json(Path(value.split("=", 1)[1]), {"spdxVersion": "SPDX-2.3", "SPDXID": "SPDXRef-DOCUMENT",
                        "packages": [{"SPDXID": "SPDXRef-fixture-" + str(index), "name": item["name"],
                                      "versionInfo": item["version"]} for index, item in enumerate(artifacts)]})
        def export(image, destination):
            self.assertEqual(image, IMAGE)
            shutil.copyfile(fixture, destination)
        output = root / "output"
        sources = output / "sources"
        sources.mkdir(parents=True)
        with patch("release_evidence.ROOT", root), patch("release_evidence.image_archive", side_effect=export), \
                patch("release_evidence.subprocess.run", side_effect=scanner), \
                patch("release_evidence.download_source", return_value={"url": "https://github.com/owner/source", "sha256": "c" * 64}):
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

    def test_installed_debian_empty_field_matches_scanner_and_missing_package_still_fails(self):
        def installed_keyring(files):
            files["var/lib/dpkg/status"] = STATUS + KEYRING_STATUS
            files["usr/share/doc/debian-archive-keyring/copyright"] = b"Debian keyring copyright"
            files["var/lib/dpkg/info/debian-archive-keyring.list"] = b"/etc/apt/trusted.gpg.d/debian-archive-trixie-stable.asc\n"
            files["etc/apt/trusted.gpg.d/debian-archive-trixie-stable.asc"] = b"archive certificate fixture"
        artifacts = [{"name": "libc6", "version": "2.41-12+b1"}, {"name": "used", "version": "1.0.0"},
                     {"name": "debian-archive-keyring", "version": "2025.1"}]
        with tempfile.TemporaryDirectory() as directory:
            output = self.run_collector(directory, installed_keyring, scanner_artifacts=artifacts)
            material = json.loads((output / "server/materials.json").read_text())
            keyring = next(item for item in material["debian"] if item["name"] == "debian-archive-keyring")
            self.assertEqual(keyring["version"], "2025.1")
            self.assertEqual(keyring["source"], "https://snapshot.debian.org/package/debian-archive-keyring/2025.1/")
            self.assertTrue((output / "server" / keyring["notice"]).is_file())
            spdx = json.loads((output / "server/sbom.spdx.json").read_text())
            package = next(item for item in spdx["packages"] if item["name"] == "debian-archive-keyring")
            self.assertEqual(package["versionInfo"], "2025.1")
            self.assertEqual(package["sourceInfo"], keyring["source"])
        with tempfile.TemporaryDirectory() as directory:
            with self.assertRaisesRegex(ValueError, "installed Debian package is absent.*debian-archive-keyring"):
                self.run_collector(directory, installed_keyring)

    def test_unowned_native_library_stops_publication_preparation(self):
        with tempfile.TemporaryDirectory() as directory:
            with self.assertRaisesRegex(ValueError, "no inventory owner"):
                self.run_collector(directory, lambda files: files.update({"opt/unknown.so": b"\x7fELFunknown"}))

    def test_approved_distribution_directory_does_not_identify_an_added_library(self):
        component = {"name": "Example bundle", "version": "1.0", "roles": ["server"],
                     "license": "MIT", "source_url": "https://github.com/owner/source",
                     "paths": ["opt/example/AppRun"], "notice_prefix": "opt/example/"}
        with tempfile.TemporaryDirectory() as directory, self.assertRaisesRegex(ValueError, "individual component identity"):
            self.run_collector(directory, lambda files: files.update({"opt/example/AppRun": b"launcher",
                "opt/example/LICENSE": b"notice", "opt/example/libunknown.so": b"\x7fELFunknown"}), [component])

    def test_selfie_segmenter_has_exact_asset_identity_and_missing_or_changed_bytes_fail(self):
        repository = Path(__file__).resolve().parents[2]
        manifest = json.loads((repository / "release/components.json").read_text())
        component = next(item for item in manifest["components"]
                         if item["name"] == "MediaPipe multiclass selfie segmenter model")
        expected = "c6748b1253a99067ef71f7e26ca71096cd449baefa8f101900ea23016507e0e0"
        self.assertEqual(component["asset_sha256"], expected)
        self.assertEqual(component["source_sha256"], expected)
        self.assertEqual(component["version"], "float32/1")
        self.assertEqual(component["roles"], ["recon"])
        self.assertEqual(component["license"], "Apache-2.0")
        self.assertEqual(component["source_url"],
            "https://storage.googleapis.com/mediapipe-models/image_segmenter/selfie_multiclass_256x256/float32/1/selfie_multiclass_256x256.tflite")
        verify_component_inputs(component)
        fixture_bytes = b"selfie model fixture"
        fixture_checksum = hashlib.sha256(fixture_bytes).hexdigest()
        fixture_component = component | {"roles": ["server"], "asset_sha256": fixture_checksum,
                                         "source_sha256": fixture_checksum}
        installed = component["paths"][0]
        for change in (None, "missing", "changed", "notice"):
            with self.subTest(change=change), tempfile.TemporaryDirectory() as directory:
                def modify(files):
                    files.update({installed: fixture_bytes, "opt/recon/package/LICENSE": b"Apache notice",
                                  "usr/share/doc/printable/NOTICE-recon": b"model notice"})
                    if change == "missing": files.pop(installed)
                    elif change == "changed": files[installed] = b"altered model"
                    elif change == "notice": files.pop("usr/share/doc/printable/NOTICE-recon")
                if change:
                    with self.assertRaisesRegex(ValueError, "component.*absent|checksum mismatch"):
                        self.run_collector(directory, modify, [fixture_component])
                else:
                    output = self.run_collector(directory, modify, [fixture_component])
                    spdx = json.loads((output / "server/sbom.spdx.json").read_text())
                    model = next(item for item in spdx["packages"] if item["name"] == component["name"])
                    self.assertEqual(model["versionInfo"], component["version"])
                    self.assertEqual(model["licenseDeclared"], component["license"])
                    self.assertEqual(model["checksums"], [{"algorithm": "SHA256", "checksumValue": fixture_checksum}])

    def test_missing_native_sources_and_changed_binary_fail(self):
        for change in (lambda files: files.pop("usr/share/doc/printable/build/native-sources.tar.gz"),
                       lambda files: files.update({"usr/local/bin/printable-server": b"\x7fELFchanged"})):
            with self.subTest(change=change), tempfile.TemporaryDirectory() as directory:
                with self.assertRaises(ValueError):
                    self.run_collector(directory, change)


class SourceBuiltNativeTests(unittest.TestCase):
    def fixture(self, root):
        source = root / "source.tar.gz"
        source.write_bytes(b"exact reviewed source")
        digest = hashlib.sha256(source.read_bytes()).hexdigest()
        component = {"name": "OpenCV", "version": "5.0.0.93", "roles": ["server"], "license": "Apache-2.0",
                     "source_url": "https://files.pythonhosted.org/source/opencv.tar.gz", "source_sha256": digest,
                     "notice_prefix": "opt/opencv-python/", "paths": ["opt/opencv-python/LICENSE-OpenCV"],
                     "source_built_native": {"recipe_stage": "recon-opencv",
                         "native_output_path": "opt/opencv-python/cv2/python-3/cv2.abi3.so",
                         "record_path": "opt/opencv-python/printable-native-build.json"}}
        recipe = root / "recipe.Dockerfile"
        recipe.write_text('FROM fixture AS recon-opencv\nARG SOURCE_URL=' + component["source_url"]
            + '\nARG SOURCE_SHA256=' + digest + '\nRUN cmake -DBUILD_LIST=core,imgproc,python3'
            + ' -DBUILD_SHARED_LIBS=OFF -DPYTHON3_LIMITED_API=ON\nFROM fixture AS runtime\n')
        install_root = root / "installed"
        extension = install_root / "cv2/python-3/cv2.abi3.so"
        extension.parent.mkdir(parents=True)
        extension.write_bytes(b"\x7fELFactual compiled output")
        install_manifest = root / "install_manifest.txt"
        install_manifest.write_text(str(extension) + "\n")
        cache = root / "CMakeCache.txt"
        cache.write_text("BUILD_LIST:STRING=core,imgproc,python3\nBUILD_SHARED_LIBS:BOOL=OFF\nPYTHON3_LIMITED_API:BOOL=ON\n")
        return component, recipe, source, install_root, install_manifest, cache

    def test_capture_reads_real_outputs_and_cache_instead_of_assuming_an_upstream_binary_digest(self):
        with tempfile.TemporaryDirectory() as directory:
            component, recipe, source, root, manifest, cache = self.fixture(Path(directory))
            first = capture(component, REVISION, recipe, source, root, manifest, cache)
            extension = root / "cv2/python-3/cv2.abi3.so"
            extension.write_bytes(b"\x7fELFa different valid build output")
            second = capture(component, REVISION, recipe, source, root, manifest, cache)
            self.assertNotEqual(first["native_files"][0]["sha256"], second["native_files"][0]["sha256"])
            self.assertEqual(first["source_sha256"], second["source_sha256"])
            self.assertEqual(second["native_files"][0]["size"], extension.stat().st_size)
            self.assertEqual(second["cmake_settings"]["BUILD_SHARED_LIBS"], "OFF")

    def test_capture_rejects_missing_extra_uninstalled_outputs_and_actual_input_mismatches(self):
        for change in ("missing", "extra", "uninstalled", "source", "cache"):
            with self.subTest(change=change), tempfile.TemporaryDirectory() as directory:
                component, recipe, source, root, manifest, cache = self.fixture(Path(directory))
                if change == "missing": (root / "cv2/python-3/cv2.abi3.so").unlink()
                elif change == "extra": (root / "unknown.so").write_bytes(b"\x7fELFextra")
                elif change == "uninstalled": manifest.write_text("/opt/elsewhere/file\n")
                elif change == "source": source.write_bytes(b"changed source")
                elif change == "cache": cache.write_text(cache.read_text().replace("OFF", "ON"))
                with self.assertRaisesRegex(ValueError, "source-built"):
                    capture(component, REVISION, recipe, source, root, manifest, cache)

    def test_record_rejects_inconsistent_identity_recipe_revision_and_output_material(self):
        for field in ("name", "version", "revision", "source_url", "source_sha256", "recipe_sha256", "cmake_settings",
                      "empty", "extra", "path", "size", "sha256", "installed_paths", "install_manifest_sha256"):
            with self.subTest(field=field), tempfile.TemporaryDirectory() as directory:
                component, recipe, source, root, manifest, cache = self.fixture(Path(directory))
                record = capture(component, REVISION, recipe, source, root, manifest, cache)
                if field == "empty": record["native_files"] = []
                elif field == "extra": record["native_files"].append(dict(record["native_files"][0]))
                elif field == "path": record["native_files"][0]["path"] = "opt/opencv-python/unknown.so"
                elif field == "size": record["native_files"][0]["size"] = 0
                elif field == "sha256": record["native_files"][0]["sha256"] = "invalid"
                elif field == "installed_paths": record[field] = []
                else: record[field] = "inconsistent"
                with self.assertRaisesRegex(ValueError, "source-built"):
                    validate_record(record, component, REVISION, recipe)

    def test_full_collector_binds_source_build_record_to_tested_image_and_rejects_changed_files(self):
        for change in (None, "missing", "changed", "extra", "record", "size", "source"):
            with self.subTest(change=change), tempfile.TemporaryDirectory() as directory:
                component, recipe, source, root, manifest, cache = self.fixture(Path(directory))
                record = capture(component, REVISION, recipe, source, root, manifest, cache)
                installed = component["source_built_native"]["native_output_path"]
                record_path = component["source_built_native"]["record_path"]
                def modify(files):
                    if change == "size": record["native_files"][0]["size"] += 1
                    elif change == "source": record["source_sha256"] = "d" * 64
                    files.update({installed: (root / "cv2/python-3/cv2.abi3.so").read_bytes(),
                                  record_path: json.dumps(record).encode(), "opt/opencv-python/LICENSE-OpenCV": b"Apache notice"})
                    if change == "missing": files.pop(installed)
                    elif change == "changed": files[installed] = b"\x7fELFaltered after build"
                    elif change == "extra": files["opt/opencv-python/unidentified.so"] = b"\x7fELFextra"
                    elif change == "record": files.pop(record_path)
                if change:
                    with self.assertRaisesRegex(ValueError, "source-built"):
                        ImageInventoryTests.run_collector(self, directory, modify, [component], recipe.read_text())
                else:
                    output = ImageInventoryTests.run_collector(self, directory, modify, [component], recipe.read_text())
                    material = json.loads((output / "server/materials.json").read_text())
                    built = next(item for item in material["supplements"] if item["name"] == "OpenCV")
                    self.assertEqual(built["native_build"]["tested_image_id"], IMAGE)
                    retained = output / built["native_build"]["retained"]
                    self.assertEqual(hashlib.sha256(retained.read_bytes()).hexdigest(), built["native_build"]["record_sha256"])
                    spdx = json.loads((output / "server/sbom.spdx.json").read_text())
                    package = next(item for item in spdx["packages"] if item["name"] == "OpenCV")
                    self.assertIn(component["source_sha256"], package["sourceInfo"])
                    self.assertTrue(any(item["fileName"] == "/" + installed for item in spdx["files"]))


class BundledComponentTests(unittest.TestCase):
    def test_declared_static_material_does_not_claim_confirmed_runtime_linkage(self):
        spec = b'set(FFTW_VERSION 3.3.10)\nset(FFTW_URI\n https://fftw.org/fftw-${FFTW_VERSION}.tar.gz\n)\nset(FFTW_LICENSE SPDX:GPL-2.0-or-later)\nset(FFTW_HASH abcdef)\nset(FFTW_HASH_TYPE MD5)\nset(LLVM_VERSION 20.1.8)\nset(LLVM_LICENSE SPDX:Apache-2.0 WITH LLVM-exception)\n'
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            archive = root / ("c" * 64)
            make_archive(archive, {"blender/build/versions.cmake": spec, "blender/extern/vendor/LICENSE": b"MIT notice",
                                  "blender/extern/vendor/source.h": b"actual vendored source"})
            source = {"sha256": "c" * 64, "url": "https://download.blender.org/source/blender.tar.xz"}
            component = {"name": "Blender", "version": "5.2.0", "license": "GPL-3.0-or-later",
                         "build_inventory": {"kind": "blender-cmake", "path": "build/versions.cmake",
                                             "sha256": hashlib.sha256(spec).hexdigest()}}
            notice_root = root / "notices"
            notice_root.mkdir()
            (notice_root / "licenses.md").write_text('## Fonts\n| [Font](https://example.org/font) | [SIL](https://spdx.org/licenses/OFL-1.1.html) |\n'
                '| [Noto Fonts](https://fonts.google.com/noto) | [SIL Open Font License 1.1](https://spdx.org/licenses/OFL-1.1.html) | `Copyright 2018 The Noto Project Authors (github.com/googlei18n/noto-fonts)`|\n'
                '| [Composite Font](https://example.org/composite) | [Custom License](https://example.org/license) + [Bitstream](https://spdx.org/licenses/Bitstream-Vera.html) | Font copyright |\n'
                '## [MIT](https://spdx.org/licenses/MIT.html)\n| [Vendor](https://example.org/vendor) | 1.2.3 | Copyright vendor |\n')
            component["build_inventory"]["license_inventory"] = "licenses.md"
            document = {}
            records = declared_build_materials(component, source, root, document, notice_root)
            self.assertEqual(records[0]["version"], "3.3.10")
            self.assertEqual(records[0]["source"], "https://fftw.org/fftw-3.3.10.tar.gz")
            self.assertEqual(records[0]["classification"], "declared build material")
            self.assertEqual(next(item for item in records if item["upstream_name"] == "LLVM")["license"], "Apache-2.0 WITH LLVM-exception")
            vendor = next(item for item in records if item.get("upstream_name") == "vendor")
            self.assertEqual(vendor["version"], "1.2.3")
            self.assertEqual(vendor["source_subdirectory"], "extern/vendor/")
            self.assertEqual(vendor["source_notices"], ["extern/vendor/LICENSE"])
            declarations = next(item["declared_components"] for item in records if "declared_components" in item)
            self.assertNotIn("declared_version", declarations[0])
            noto = next(item for item in declarations if item["name"] == "Noto Fonts")
            self.assertEqual(noto["homepage"], "https://fonts.google.com/noto")
            self.assertEqual(noto["license"], "OFL-1.1")
            self.assertEqual(noto["copyright"], "`Copyright 2018 The Noto Project Authors (github.com/googlei18n/noto-fonts)`")
            self.assertNotIn("declared_version", noto)
            composite = next(item for item in declarations if item["name"] == "Composite Font")
            self.assertEqual(composite["license"], "NOASSERTION")
            self.assertIn("Custom License", composite["available_license"])
            self.assertNotIn("native_files", records[0])
            self.assertIn("not independently confirmed", records[0]["binary_linkage"])
            self.assertTrue(any(item["relationshipType"] == "BUILD_DEPENDENCY_OF" for item in document["relationships"]))
            license_inventory = notice_root / "licenses.md"
            license_inventory.write_text(license_inventory.read_text() + "| [Unreadable component](https://example.org/source) |\n")
            with self.assertRaisesRegex(ValueError, "unsupported component row"):
                declared_build_materials(component, source, root, {}, notice_root)
            component["build_inventory"]["sha256"] = "d" * 64
            with self.assertRaisesRegex(ValueError, "build specification"):
                declared_build_materials(component, source, root, {})

    def test_declared_orca_sources_retain_hashes_and_vendored_notices_without_inventing_versions(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            make_archive(root / ("c" * 64), {"orca/deps/ZLIB/ZLIB.cmake":
                b'URL https://github.com/madler/zlib/archive/refs/tags/v1.2.13.zip\nURL_HASH SHA256=' + b'd' * 64 + b'\n',
                "orca/deps/wxWidgets/wxWidgets.cmake": b'GIT_REPOSITORY https://github.com/owner/wx\nGIT_TAG v3.3.2\n',
                "orca/deps_src/embedded/LICENSE": b"actual embedded license", "orca/deps_src/embedded/source.h": b"source"})
            source = {"sha256": "c" * 64, "url": "https://github.com/owner/OrcaSlicer/archive/" + REVISION + ".tar.gz"}
            component = {"name": "OrcaSlicer", "version": "release", "license": "AGPL-3.0-only",
                         "build_inventory": {"kind": "orca-cmake", "declared_versions": {"ZLIB": "1.2.13"},
                             "source_overrides": {"wxWidgets": {"source": "https://github.com/owner/wx/archive/" + REVISION + ".tar.gz",
                                 "source_commit": REVISION, "declared_source_ref": "v3.3.2",
                                 "source_resolution": "Observed declared branch; compiled commit unconfirmed."}}}}
            document = {}
            records = declared_build_materials(component, source, root, document)
            external = next(item for item in records if item["upstream_name"] == "ZLIB")
            vendored = next(item for item in records if item["upstream_name"] == "embedded")
            self.assertEqual(external["version"], "1.2.13")
            self.assertEqual(external["source_checksum"]["value"], "d" * 64)
            self.assertEqual(vendored["source_notices"], ["deps_src/embedded/LICENSE"])
            self.assertNotIn("version", vendored)
            package = next(item for item in document["packages"] if "vendored source" in item["name"])
            self.assertNotIn("versionInfo", package)
            self.assertTrue(package["sourceInfo"].endswith("#sha256=" + "c" * 64))
            wx = next(item for item in records if item["upstream_name"] == "wxWidgets")
            self.assertEqual(wx["source_commit"], REVISION)
            self.assertIn("compiled commit unconfirmed", wx["description"])
            component["build_inventory"]["source_overrides"]["wxWidgets"]["declared_source_ref"] = "different"
            with self.assertRaisesRegex(ValueError, "source revision"):
                declared_build_materials(component, source, root, {})

    def fixture(self):
        digest = hashlib.sha256(b"native library").hexdigest()
        native = {"name": "Actual upstream library", "version": "2.3.4", "license": "MIT",
                  "source_url": "https://github.com/owner/library/archive/v2.3.4.tar.gz",
                  "source_checksum": {"algorithm": "SHA256", "value": "c" * 64},
                  "native_files": [{"path": "opt/bundle/library.so.9", "sha256": digest}]}
        component = {"name": "Distribution", "version": "1.0", "license": "GPL-3.0-or-later",
                     "source_url": "https://github.com/owner/distribution/archive/v1.0.tar.gz",
                     "notice_prefix": "opt/bundle/", "native_components": [native]}
        inventory = [{"path": "opt/bundle/library.so.9", "sha256": digest, "elf": True}]
        return component, inventory

    def test_file_identity_retains_upstream_version_source_and_distribution_relationship(self):
        component, inventory = self.fixture()
        document, ownership, supplements = {}, {}, []
        identify_bundled_native(component, inventory, ownership, document, supplements)
        library = next(item for item in document["packages"] if item["name"] == "Actual upstream library")
        distribution = next(item for item in document["packages"] if item["name"] == "Distribution")
        self.assertEqual(library["versionInfo"], "2.3.4")
        self.assertTrue(library["sourceInfo"].endswith("#sha256=" + "c" * 64))
        self.assertEqual(inventory[0]["distribution_owner"], "Distribution@1.0")
        self.assertEqual(inventory[0]["component_spdx_id"], library["SPDXID"])
        self.assertEqual(ownership[inventory[0]["path"]], "component:Actual upstream library@2.3.4")
        self.assertIn({"spdxElementId": distribution["SPDXID"], "relationshipType": "CONTAINS",
                       "relatedSpdxElement": library["SPDXID"]}, document["relationships"])
        self.assertEqual(document["files"][0]["checksums"][0]["checksumValue"], inventory[0]["sha256"])

    def test_unknown_changed_missing_and_ambiguous_native_files_fail(self):
        for change in (lambda c, i: i.append({"path": "opt/bundle/unknown.so", "sha256": "d" * 64, "elf": True}),
                       lambda c, i: i[0].update(sha256="d" * 64), lambda c, i: i.clear(),
                       lambda c, i: c["native_components"].append(dict(c["native_components"][0])),
                       lambda c, i: c["native_components"][0].update(source_url=""),
                       lambda c, i: c["native_components"][0].update(version="")):
            with self.subTest(change=change), self.assertRaises(ValueError):
                component, inventory = self.fixture()
                change(component, inventory)
                identify_bundled_native(component, inventory, {}, {}, [])

    def test_native_source_descriptor_is_retained_and_missing_custom_license_fails(self):
        component, inventory = self.fixture()
        native = component["native_components"][0]
        native["retain_source_descriptor"] = True
        supplements = []
        with patch("release_evidence.download_source", return_value={"retained": "sources/" + "c" * 64}) as download:
            identify_bundled_native(component, inventory, {}, {}, supplements, Path("sources"))
        self.assertEqual(download.call_args.args[2], "c" * 64)
        self.assertEqual(supplements[0]["retained_source_descriptor"]["retained"], "sources/" + "c" * 64)
        native.pop("retain_source_descriptor")
        native["license"] = "LicenseRef-example"
        with self.assertRaisesRegex(ValueError, "custom license text"):
            identify_bundled_native(component, inventory, {}, {}, [])

    def test_first_party_installed_files_and_license_match_the_commit_sources(self):
        for change in (None, "missing", "changed", "extra", "notice"):
            with self.subTest(change=change), tempfile.TemporaryDirectory() as directory:
                root = Path(directory)
                (root / "addon").mkdir()
                (root / "addon/bridge.py").write_bytes(b"actual source")
                (root / "addon/tests").mkdir()
                (root / "addon/tests/test.py").write_bytes(b"excluded test")
                (root / "LICENSE").write_bytes(b"MIT source license")
                files = {"opt/printable/addon/bridge.py": b"actual source", "usr/share/doc/printable/LICENSE": b"MIT source license"}
                if change == "missing": files.pop("opt/printable/addon/bridge.py")
                elif change == "changed": files["opt/printable/addon/bridge.py"] = b"changed"
                elif change == "extra": files["opt/printable/addon/untracked.py"] = b"extra"
                elif change == "notice": files["usr/share/doc/printable/LICENSE"] = b"wrong license"
                component = {"name": "Printable bridge", "source_path": "addon", "installed_path": "opt/printable/addon",
                             "exclude": ["tests"], "license": "MIT", "notice": "usr/share/doc/printable/LICENSE"}
                fixture = root / "image.tar"
                make_archive(fixture, files)
                with patch("release_evidence.ROOT", root), tarfile.open(fixture) as archive:
                    members, document = archive_members(archive), {}
                    if change:
                        with self.assertRaisesRegex(ValueError, "first-party"):
                            identify_first_party(component, archive, members, REVISION, document)
                    else:
                        record = identify_first_party(component, archive, members, REVISION, document)
                        self.assertEqual(record["version"], REVISION)
                        self.assertEqual(record["source"], ReleaseIdentity.source + "/tree/" + REVISION + "/addon")
                        self.assertEqual(document["packages"][0]["licenseDeclared"], "MIT")
                        self.assertEqual(set(record["files"]), {"opt/printable/addon/bridge.py"})
                        self.assertEqual(len(document["files"]), 1)

    def test_first_party_launcher_uses_exact_installed_bytes_and_file_source_locator(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            (root / "scripts").mkdir()
            (root / "scripts/launcher").write_bytes(b"#!/bin/sh\nexec python3 bridge\n")
            (root / "LICENSE").write_bytes(b"MIT notice")
            fixture = root / "image.tar"
            make_archive(fixture, {"opt/printable/launcher": (root / "scripts/launcher").read_bytes(),
                                   "usr/share/doc/printable/LICENSE": b"MIT notice"})
            component = {"name": "Printable launcher", "source_path": "scripts/launcher",
                         "installed_path": "opt/printable/launcher", "license": "MIT",
                         "notice": "usr/share/doc/printable/LICENSE"}
            with patch("release_evidence.ROOT", root), tarfile.open(fixture) as archive:
                document = {}
                record = identify_first_party(component, archive, archive_members(archive), REVISION, document)
            self.assertEqual(record["source"], ReleaseIdentity.source + "/blob/" + REVISION + "/scripts/launcher")
            self.assertEqual(document["files"][0]["checksums"][0]["checksumValue"],
                             hashlib.sha256((root / "scripts/launcher").read_bytes()).hexdigest())


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
