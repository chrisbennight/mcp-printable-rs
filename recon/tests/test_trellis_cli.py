"""Stage boundaries, cache reuse and safe errors with no model downloads."""

from contextlib import redirect_stdout
from dataclasses import replace
import errno
import hashlib
import importlib.util
import io
import json
from pathlib import Path
import tempfile
import subprocess
import sys
import unittest
from unittest.mock import patch

from printable_recon.model_cache import Manifest, ModelFile, PreparationError
from printable_recon.trellis_errors import NoShapeError, UnavailableError

ROOT = Path(__file__).resolve().parents[1]
spec = importlib.util.spec_from_file_location("generate_trellis", ROOT / "generate_trellis.py")
cli = importlib.util.module_from_spec(spec)
spec.loader.exec_module(cli)


class StageTests(unittest.TestCase):
    def test_worker_isolated_python_can_start_the_stage_cli(self):
        with tempfile.TemporaryDirectory() as directory:
            request = Path(directory) / "request.json"
            request.write_text("{}")
            answer = subprocess.run([sys.executable, "-I", "-B", str(ROOT / "generate_trellis.py"), "prepare", str(request)], capture_output=True, text=True)
            self.assertEqual(answer.returncode, 2)
            self.assertEqual(json.loads(answer.stdout)["error"]["code"], "generation")
            self.assertEqual(answer.stderr, "")

    def test_preparation_reuses_only_identical_dependencies_from_existing_model_storage(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            shared = root / "existing"
            (shared / "dinov3").mkdir(parents=True)
            (shared / "dinov3/config.json").write_bytes(b"verified")
            item = ModelFile("example/image", "a" * 40, "dinov3", "config.json", hashlib.sha256(b"verified").hexdigest(), 8)
            manifest = Manifest((item,))
            request = {"cache": str(root / "cache"), "shared_models": str(shared)}
            with patch.object(Manifest, "read", return_value=manifest), patch("printable_recon.model_cache._download", side_effect=AssertionError("unexpected network")):
                result = cli.prepare(request)
            self.assertEqual((Path(result["snapshot"]) / item.relative_path).read_bytes(), b"verified")
            self.assertFalse((Path(result["snapshot"]) / item.relative_path).is_symlink())
            other = Manifest((replace(item, sha256=hashlib.sha256(b"different").hexdigest(), bytes=9),))
            with patch.object(Manifest, "read", return_value=other), patch("printable_recon.model_cache._download", side_effect=PreparationError("download required")):
                with self.assertRaisesRegex(PreparationError, "download required"):
                    cli.prepare(request)

    def test_stage_protocol_measures_memory_and_does_not_expose_diagnostics_or_endpoint_errors(self):
        with tempfile.TemporaryDirectory() as directory:
            request = Path(directory) / "request.json"
            request.write_text("{}")
            def noisy(_):
                print("private diagnostic")
                return {"triangles": 1}
            output = io.StringIO()
            with patch.dict(cli.STAGES, {"deliver": noisy}), redirect_stdout(output):
                self.assertEqual(cli.main(["deliver", str(request)]), 0)
            answer = json.loads(output.getvalue())["result"]
            self.assertGreater(answer["peak_ram_bytes"], 0)
            self.assertNotIn("private diagnostic", output.getvalue())
            for error, code, fragment in [(OSError(errno.ENOSPC, "private volume"), "resource_limit", "disk space"),
                                           (RuntimeError("private endpoint and body"), "generation", "stage failed"),
                                           (UnavailableError("requires a CUDA GPU"), "unavailable", "CUDA GPU"),
                                           (NoShapeError("no subject was found"), "no_shape", "no subject")]:
                with self.subTest(code=code):
                    output = io.StringIO()
                    with patch.dict(cli.STAGES, {"deliver": lambda _, error=error: (_ for _ in ()).throw(error)}), redirect_stdout(output):
                        self.assertEqual(cli.main(["deliver", str(request)]), 2)
                    response = json.loads(output.getvalue())["error"]
                    self.assertEqual(response["code"], code)
                    self.assertIn(fragment, response["message"])
                    self.assertNotIn("private", output.getvalue())

    def test_programmer_errors_remain_failures(self):
        with tempfile.TemporaryDirectory() as directory:
            request = Path(directory) / "request.json"
            request.write_text("{}")
            with patch.dict(cli.STAGES, {"deliver": lambda _: 1 + "invalid"}), self.assertRaises(TypeError):
                cli.main(["deliver", str(request)])

    def test_published_manifest_contains_only_shape_and_image_data_with_pinned_revisions(self):
        manifest = Manifest.read(ROOT / "models/trellis2.json")
        paths = {item.relative_path for item in manifest.files}
        self.assertEqual(len(manifest.files), 14)
        self.assertIn("trellis2/pipeline.json", paths)
        self.assertIn("birefnet/model.safetensors", paths)
        self.assertIn("dinov3/model.safetensors", paths)
        self.assertTrue(all(item.filename.endswith((".json", ".safetensors")) for item in manifest.files))
        self.assertFalse(any("tex_" in item.filename for item in manifest.files))


if __name__ == "__main__":
    unittest.main()
