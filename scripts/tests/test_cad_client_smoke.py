"""Verify recovery evidence without contacting a service."""

import hashlib
import json
from pathlib import Path
import tempfile
import unittest

from cad_client_smoke import verify_restored


class FileClient:
    def __init__(self, files):
        self.files = files
        self.downloads = []

    def download(self, path, destination):
        self.downloads.append(path)
        destination.write_bytes(self.files[path])


class CadRecoveryTests(unittest.TestCase):
    def fixture(self, output):
        files = {}
        for name in ("model", "import_step"):
            directory = "projects/test/builds/" + name
            artifacts = []
            for suffix in ("step", "stl", "glb", "json"):
                path = directory + "/output." + suffix
                content = (name + suffix).encode()
                files[path] = content
                artifacts.append({"artifact": {"path": path},
                                  "sha256": hashlib.sha256(content).hexdigest()})
            report = {"build_directory": directory, "artifacts": artifacts}
            encoded = json.dumps(report).encode()
            files[directory + "/report.json"] = encoded
            (output / (name + "-report.json")).write_bytes(encoded)
        return FileClient(files)

    def test_both_builds_and_every_artifact_are_verified(self):
        with tempfile.TemporaryDirectory() as directory:
            output = Path(directory)
            client = self.fixture(output)
            verify_restored(client, output)
            self.assertEqual(set(client.downloads), set(client.files))
            self.assertEqual(len(client.downloads), 10)

    def test_changed_artifact_is_rejected(self):
        with tempfile.TemporaryDirectory() as directory:
            output = Path(directory)
            client = self.fixture(output)
            client.files["projects/test/builds/import_step/output.glb"] = b"changed"
            with self.assertRaisesRegex(ValueError, "recorded digest"):
                verify_restored(client, output)

    def test_changed_report_is_rejected(self):
        with tempfile.TemporaryDirectory() as directory:
            output = Path(directory)
            client = self.fixture(output)
            client.files["projects/test/builds/model/report.json"] = b"{}"
            with self.assertRaisesRegex(ValueError, "report differs"):
                verify_restored(client, output)
