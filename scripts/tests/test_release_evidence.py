"""The collector preserves standard inventory and binds it to the tested image set."""

import json
from pathlib import Path
import subprocess
import tempfile
import unittest
from unittest.mock import patch

from release_evidence import collect, supplement, SOURCE, SYFT_VERSION
from release_record import ROLES


REVISION = "a" * 40
IMAGES = {role: "sha256:" + str(index + 1) * 64 for index, role in enumerate(ROLES)}
SPDX = (b'{ "spdxVersion": "SPDX-2.3", "SPDXID": "SPDXRef-DOCUMENT",\n'
        b'  "name": "image", "dataLicense": "CC0-1.0", "packages": [],\n'
        b'  "documentNamespace": "https://example.com/sbom",\n'
        b'  "creationInfo": {"creators": ["Tool: syft"], "created": "2026-10-12T00:00:00Z"},\n'
        b'  "annotations": [{"comment": "preserve generator metadata and spacing"}] }\n')


class CollectorTests(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory()
        self.addCleanup(self.temporary.cleanup)
        self.root = Path(self.temporary.name)
        (self.root / "release").mkdir()
        (self.root / "Dockerfile").write_text("source recipe")
        (self.root / "LICENSE").write_bytes(b"actual notice bytes\r\n")
        self.declared = {"schema": 1, "scope": "declared", "limitations": "No runtime linkage claim.",
                         "notice_paths": ["LICENSE"], "components": [
                             {"name": "Example", "version": "1.0", "roles": ["server"],
                              "license": "MIT", "source_url": "https://example.com/source.tar.gz",
                              "source_sha256": "c" * 64, "recipe_paths": ["Dockerfile"]}]}
        self.write_manifest()
        self.output = self.root / "evidence"
        self.root_patch = patch("release_evidence.ROOT", self.root)
        self.root_patch.start()
        self.addCleanup(self.root_patch.stop)
        version_patch = patch("release_evidence.subprocess.check_output",
                              return_value=json.dumps({"version": SYFT_VERSION}))
        self.version = version_patch.start()
        self.addCleanup(version_patch.stop)

    def write_manifest(self):
        (self.root / "release/components.json").write_text(json.dumps(self.declared))

    def scan(self, argv, **kwargs):
        self.assertEqual(argv[0], "syft")
        self.assertEqual(argv[2:5], ["--scope", "squashed", "-o"])
        self.assertTrue(argv[5].startswith("spdx-json="))
        self.assertEqual(kwargs, {"check": True, "timeout": 900})
        Path(argv[5].split("=", 1)[1]).write_bytes(SPDX)

    def test_all_tested_ids_preserve_standard_output_and_declared_notices(self):
        with patch("release_evidence.subprocess.run", side_effect=self.scan) as scanner:
            collect(REVISION, IMAGES, self.output)
        self.assertEqual([call.args[0][1] for call in scanner.call_args_list],
                         ["docker:" + IMAGES[role] for role in ROLES])
        for role in ROLES:
            self.assertEqual((self.output / role / "sbom.spdx.json").read_bytes(), SPDX)
        self.assertEqual(json.loads((self.output / "tested-images.json").read_text()),
                         {"schema": 1, "revision": REVISION, "images": IMAGES})
        declared = json.loads((self.output / "components.json").read_text())
        self.assertEqual(declared["components"][0]["source_sha256"], "c" * 64)
        self.assertEqual(declared["components"][0]["recipe_urls"],
                         [SOURCE + "/blob/" + REVISION + "/Dockerfile"])
        self.assertEqual(declared["scope"], "declared")
        self.assertEqual(declared["repository_url"], SOURCE + "/tree/" + REVISION)
        self.assertEqual((self.output / "notices/LICENSE").read_bytes(),
                         (self.root / "LICENSE").read_bytes())
        self.assertEqual({p.name for p in self.output.iterdir()},
                         set(ROLES) | {"components.json", "tested-images.json", "notices"})

    def test_invalid_source_or_image_identity_fails_before_generator(self):
        cases = [("main", IMAGES), (None, IMAGES), (REVISION, []),
                 (REVISION, {role: image for role, image in IMAGES.items() if role != "recon"}),
                 (REVISION, dict(IMAGES, extra=IMAGES["server"])),
                 (REVISION, dict(IMAGES, server="printable-server:ci")),
                 (REVISION, dict(IMAGES, recon=None))]
        with patch("release_evidence.subprocess.run") as scanner:
            for revision, images in cases:
                with self.subTest(revision=revision, images=images), self.assertRaises(ValueError):
                    collect(revision, images, self.output)
            scanner.assert_not_called()
            self.version.assert_not_called()

    def test_generator_failure_propagates_without_completed_metadata(self):
        failure = subprocess.CalledProcessError(1, ["syft"])
        with patch("release_evidence.subprocess.run") as scanner:
            # The first successful generator writes its file before the next invocation fails.
            def scan_then_fail(argv, **kwargs):
                if argv[1] == "docker:" + IMAGES[ROLES[0]]:
                    self.scan(argv, **kwargs)
                else:
                    raise failure
            scanner.side_effect = scan_then_fail
            with self.assertRaises(subprocess.CalledProcessError) as raised:
                collect(REVISION, IMAGES, self.output)
        self.assertIs(raised.exception, failure)
        self.assertFalse((self.output / "tested-images.json").exists())

    def test_wrong_version_and_incomplete_output_cannot_complete_collection(self):
        self.version.return_value = json.dumps({"version": "0.0.0"})
        with patch("release_evidence.subprocess.run") as scanner:
            with self.assertRaisesRegex(ValueError, "pinned Syft"):
                collect(REVISION, IMAGES, self.output)
            scanner.assert_not_called()
        self.version.return_value = json.dumps({"version": SYFT_VERSION})
        with patch("release_evidence.subprocess.run"):
            with self.assertRaises(FileNotFoundError):
                collect(REVISION, IMAGES, self.output)
        self.assertFalse((self.output / "tested-images.json").exists())

    def test_existing_output_is_not_reused_as_fresh_inventory(self):
        self.output.mkdir()
        (self.output / "tested-images.json").write_text("stale evidence")
        with patch("release_evidence.subprocess.run") as scanner:
            with self.assertRaises(FileExistsError):
                collect(REVISION, IMAGES, self.output)
            scanner.assert_not_called()

    def test_supplement_rejects_missing_identity_notice_and_escaping_paths(self):
        changes = [("name", ""), ("roles", ["other"]), ("source_url", "https://user:password@example.com/source"),
                   ("source_sha256", "invalid"), ("recipe_paths", ["../outside"])]
        original = dict(self.declared["components"][0])
        with patch("release_evidence.subprocess.run") as scanner:
            for key, value in changes:
                self.declared["components"][0] = dict(original, **{key: value})
                self.write_manifest()
                with self.subTest(key=key), self.assertRaises(ValueError):
                    collect(REVISION, IMAGES, self.output)
            self.declared["components"][0] = original
            self.declared["notice_paths"] = ["missing-LICENSE"]
            self.write_manifest()
            with self.assertRaises(FileNotFoundError):
                collect(REVISION, IMAGES, self.output)
            scanner.assert_not_called()
            self.version.assert_not_called()

    def test_repository_asset_gets_release_revision_source_locator(self):
        (self.root / "font.ttf").write_bytes(b"declared source asset")
        component = self.declared["components"][0]
        del component["source_url"]
        component["source_path"] = "font.ttf"
        self.write_manifest()
        declared = supplement(REVISION)
        self.assertEqual(declared["components"][0]["source_url"],
                         SOURCE + "/blob/" + REVISION + "/font.ttf")


if __name__ == "__main__":
    unittest.main()
