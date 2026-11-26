"""Geometry model preparation and status, with fake downloads and no model code."""

import hashlib
import importlib.util
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import types
import unittest
from unittest.mock import patch

from printable_recon import geometry_files

ROOT = Path(__file__).resolve().parents[1]
spec = importlib.util.spec_from_file_location("prepare_geometry", ROOT / "prepare_geometry.py")
cli = importlib.util.module_from_spec(spec)
spec.loader.exec_module(cli)

CONTENT = b"pinned weights"


def hub_file(directory):
    return {"repo": "example/model", "revision": "a" * 40, "directory": directory, "filename": "model.pt",
            "sha256": hashlib.sha256(CONTENT).hexdigest(), "bytes": len(CONTENT)}


def models():
    return geometry_files.parse({"format": geometry_files.FORMAT, "models": {
        "plain": {"model": "Plain", "outputs": {"depth": "metric", "mask": "valid_pixels", "focal": "pixels"},
                  "files": [hub_file("plain")]},
        "converted": {"model": "Converted", "outputs": {"normals": "camera"}, "conversion": "marigold-v2",
                      "files": [hub_file("qwen-image-edit-2509"), hub_file("marigold-v2-0")]},
    }})


class PrepareGeometryTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.cache = Path(self.temp.name) / "cache"
        self.models = models()
        self.downloads = []
        self.conversions = []
        self.addCleanup(patch.stopall)
        patch.dict(os.environ, {"HF_ENDPOINT": "https://models.example.test/hub"}).start()

        def convert(qwen, marigold, output):
            self.conversions.append((qwen.name, marigold.name))
            output.mkdir()
            return {"output": str(output)}

        fake = types.ModuleType("convert_marigold")
        fake.convert = convert
        patch.dict(sys.modules, {"convert_marigold": fake}).start()

    def download(self, item, directory, endpoint):
        self.downloads.append((item.directory, endpoint))
        path = directory / item.filename
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_bytes(CONTENT)
        return path

    def test_preparation_downloads_named_models_and_converts_once(self):
        request = {"cache": str(self.cache), "models": ["converted", "plain", "converted"]}
        answer = cli.prepare(request, self.models, download=self.download)["models"]
        self.assertEqual(sorted(self.downloads),
                         [(name, "https://models.example.test/hub") for name in ("marigold-v2-0", "plain", "qwen-image-edit-2509")])
        identifier = self.models["converted"].manifest.identifier
        self.assertEqual(answer["converted"]["converted"], str(self.cache / "converted" / identifier))
        self.assertEqual(answer["converted"]["snapshot"], str(self.cache / "snapshots" / identifier))
        self.assertNotIn("converted", answer["plain"])
        self.assertEqual(self.conversions, [("qwen-image-edit-2509", "marigold-v2-0")])

        def offline(*_):
            self.fail("a prepared model attempted a download")
        self.assertEqual(cli.prepare(request, self.models, download=offline)["models"], answer)
        self.assertEqual(len(self.conversions), 1)

    def test_a_conversion_left_partial_is_redone(self):
        identifier = self.models["converted"].manifest.identifier
        partial = self.cache / "converted" / f"{identifier}.partial"
        partial.mkdir(parents=True)
        cli.prepare({"cache": str(self.cache), "models": ["converted"]}, self.models, download=self.download)
        self.assertFalse(partial.exists())
        self.assertTrue((self.cache / "converted" / identifier).is_dir())
        self.assertEqual(len(self.conversions), 1)

    def test_preparation_requests_are_checked_before_any_download(self):
        for request in ({"models": ["plain"]}, {"cache": str(self.cache), "models": "plain"},
                        {"cache": str(self.cache), "models": ["plain", "absent"]}):
            with self.assertRaises(cli.RequestError):
                cli.prepare(request, self.models, download=self.download)
        self.assertEqual(self.downloads, [])

    def test_status_counts_a_converted_model_prepared_only_once_converted(self):
        self.assertEqual(cli.status(self.cache, self.models)["models"]["plain"]["state"], "not_prepared")
        cli.prepare({"cache": str(self.cache), "models": ["plain", "converted"]}, self.models, download=self.download)
        states = cli.status(self.cache, self.models)["models"]
        self.assertEqual(states["plain"]["state"], "prepared")
        self.assertEqual(states["converted"]["state"], "prepared")
        identifier = self.models["converted"].manifest.identifier
        (self.cache / "converted" / identifier).rmdir()
        self.assertEqual(cli.status(self.cache, self.models)["models"]["converted"],
                         {"state": "not_prepared", "last_verified_unix_ms": None})

    def test_worker_isolated_python_reports_status_from_the_shipped_model_list(self):
        script = str(ROOT / "prepare_geometry.py")
        answer = subprocess.run([sys.executable, "-I", "-B", script, "--status", str(self.cache)],
                                capture_output=True, text=True, env={})
        self.assertEqual(answer.returncode, 0, answer.stdout)
        states = json.loads(answer.stdout)["models"]
        self.assertEqual(set(states), set(geometry_files.read(ROOT / "models/geometry.json")))
        self.assertTrue(all(state["state"] == "not_prepared" for state in states.values()))
        request = Path(self.temp.name) / "request.json"
        request.write_text(json.dumps({"cache": str(self.cache), "models": ["absent"]}))
        answer = subprocess.run([sys.executable, "-I", "-B", script, "prepare", str(request)],
                                capture_output=True, text=True, env={})
        self.assertEqual(answer.returncode, 2)
        self.assertEqual(json.loads(answer.stdout)["error"]["code"], "validation")
        answer = subprocess.run([sys.executable, "-I", "-B", script, "--select", str(request)],
                                capture_output=True, text=True, env={})
        self.assertEqual(answer.returncode, 2)

if __name__ == "__main__":
    unittest.main()
