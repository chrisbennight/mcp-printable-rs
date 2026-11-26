"""Geometry model selection, preparation and status, with fake downloads and no model code."""

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

from printable_recon.registry import ROLES, Registry

ROOT = Path(__file__).resolve().parents[1]
spec = importlib.util.spec_from_file_location("prepare_geometry", ROOT / "prepare_geometry.py")
cli = importlib.util.module_from_spec(spec)
spec.loader.exec_module(cli)

CONTENT = b"pinned weights"


def hub_file(directory):
    return {"repo": "example/model", "revision": "a" * 40, "directory": directory, "filename": "model.pt",
            "sha256": hashlib.sha256(CONTENT).hexdigest(), "bytes": len(CONTENT)}


def registry():
    entries = {
        "plain": {"model": "Plain", "outputs": {"depth": "metric", "mask": "valid_pixels", "focal": "pixels"},
                  "files": [hub_file("plain")]},
        "converted": {"model": "Converted", "outputs": {"normals": "camera"}, "conversion": "marigold-v2",
                      "files": [hub_file("qwen-image-edit-2509"), hub_file("marigold-v2-0")]},
    }
    options = [
        {"entry": "plain", "device": "cuda", "side": 2048, "seconds": 1.0, "gpu_peak_mib": 8000, "ram_peak_mib": 4000},
        {"entry": "plain", "device": "cpu", "side": 1024, "seconds": 20.0, "ram_peak_mib": 6000},
    ]
    normals = [{"entry": "converted", "device": "cuda", "side": 1024, "seconds": 6.0, "gpu_peak_mib": 2048,
                "ram_peak_mib": 2000}]
    routes = {role: normals if role == "normals" else options for role in ROLES}
    return Registry.parse({"format": "printable-geometry 1", "entries": entries, "routes": routes})


class PrepareGeometryTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.cache = Path(self.temp.name) / "cache"
        self.registry = registry()
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

    def test_selection_reports_each_role_its_fallback_and_refusals(self):
        answer = cli.select({"gpu_budget_mib": 3072, "ram_budget_mib": None}, self.registry)["roles"]
        self.assertEqual(set(answer), set(ROLES))
        self.assertEqual(answer["depth.deep"]["entry"], "plain")
        self.assertEqual(answer["depth.deep"]["device"], "cpu")
        self.assertEqual(answer["depth.deep"]["side"], 1024)
        self.assertTrue(answer["depth.deep"]["fallback"])
        self.assertIn("8000 MiB of GPU memory", answer["depth.deep"]["rejected"][0])
        self.assertEqual(answer["normals"], {"entry": "converted", "device": "cuda", "side": 1024,
                                             "fallback": False, "rejected": []})
        without_gpu = cli.select({"gpu_budget_mib": None, "ram_budget_mib": 1024}, self.registry)["roles"]
        self.assertIn("no normals model fits this worker", without_gpu["normals"]["error"])
        for request in ({"gpu_budget_mib": 0}, {"gpu_budget_mib": True}, {"ram_budget_mib": "1"}, []):
            with self.assertRaises(cli.RequestError):
                cli.select(request, self.registry)

    def test_preparation_downloads_named_entries_and_converts_once(self):
        request = {"cache": str(self.cache), "entries": ["converted", "plain", "converted"]}
        answer = cli.prepare(request, self.registry, download=self.download)["entries"]
        self.assertEqual(sorted(self.downloads),
                         [(name, "https://models.example.test/hub") for name in ("marigold-v2-0", "plain", "qwen-image-edit-2509")])
        identifier = self.registry.entries["converted"].manifest.identifier
        self.assertEqual(answer["converted"]["converted"], str(self.cache / "converted" / identifier))
        self.assertEqual(answer["converted"]["snapshot"], str(self.cache / "snapshots" / identifier))
        self.assertNotIn("converted", answer["plain"])
        self.assertEqual(self.conversions, [("qwen-image-edit-2509", "marigold-v2-0")])

        def offline(*_):
            self.fail("a prepared entry attempted a download")
        self.assertEqual(cli.prepare(request, self.registry, download=offline)["entries"], answer)
        self.assertEqual(len(self.conversions), 1)

    def test_a_conversion_left_partial_is_redone(self):
        identifier = self.registry.entries["converted"].manifest.identifier
        partial = self.cache / "converted" / f"{identifier}.partial"
        partial.mkdir(parents=True)
        cli.prepare({"cache": str(self.cache), "entries": ["converted"]}, self.registry, download=self.download)
        self.assertFalse(partial.exists())
        self.assertTrue((self.cache / "converted" / identifier).is_dir())
        self.assertEqual(len(self.conversions), 1)

    def test_preparation_requests_are_checked_before_any_download(self):
        for request in ({"entries": ["plain"]}, {"cache": str(self.cache), "entries": "plain"},
                        {"cache": str(self.cache), "entries": ["plain", "absent"]}):
            with self.assertRaises(cli.RequestError):
                cli.prepare(request, self.registry, download=self.download)
        self.assertEqual(self.downloads, [])

    def test_status_counts_a_converted_entry_prepared_only_once_converted(self):
        self.assertEqual(cli.status(self.cache, self.registry)["entries"]["plain"]["state"], "not_prepared")
        cli.prepare({"cache": str(self.cache), "entries": ["plain", "converted"]}, self.registry, download=self.download)
        entries = cli.status(self.cache, self.registry)["entries"]
        self.assertEqual(entries["plain"]["state"], "prepared")
        self.assertEqual(entries["converted"]["state"], "prepared")
        identifier = self.registry.entries["converted"].manifest.identifier
        (self.cache / "converted" / identifier).rmdir()
        self.assertEqual(cli.status(self.cache, self.registry)["entries"]["converted"],
                         {"state": "not_prepared", "last_verified_unix_ms": None})

    def test_worker_isolated_python_selects_and_reports_status_from_the_shipped_registry(self):
        request = Path(self.temp.name) / "request.json"
        request.write_text(json.dumps({"gpu_budget_mib": 3072, "ram_budget_mib": None}))
        script = str(ROOT / "prepare_geometry.py")
        answer = subprocess.run([sys.executable, "-I", "-B", script, "--select", str(request)],
                                capture_output=True, text=True, env={})
        self.assertEqual(answer.returncode, 0, answer.stdout)
        roles = json.loads(answer.stdout)["result"]["roles"]
        self.assertEqual(set(roles), set(ROLES))
        self.assertEqual(roles["focal"]["entry"], "dp2-l")
        answer = subprocess.run([sys.executable, "-I", "-B", script, "--status", str(self.cache)],
                                capture_output=True, text=True, env={})
        self.assertEqual(answer.returncode, 0)
        entries = json.loads(answer.stdout)["entries"]
        self.assertEqual(set(entries), set(Registry.read(ROOT / "models/geometry.json").entries))
        self.assertTrue(all(state["state"] == "not_prepared" for state in entries.values()))
        request.write_text(json.dumps({"cache": str(self.cache), "entries": ["absent"]}))
        answer = subprocess.run([sys.executable, "-I", "-B", script, "prepare", str(request)],
                                capture_output=True, text=True, env={})
        self.assertEqual(answer.returncode, 2)
        self.assertEqual(json.loads(answer.stdout)["error"]["code"], "validation")


if __name__ == "__main__":
    unittest.main()
