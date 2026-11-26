"""The geometry runner's conventions and request handling, with a fake model and no weights."""

import contextlib
import io
import json
from pathlib import Path
import tempfile
import unittest
from unittest import mock

import numpy
from PIL import Image

import estimate_geometry
from printable_recon import geometry_models
from printable_recon.geometry_models import Geometry


class ConventionTests(unittest.TestCase):
    def test_invalid_and_infinite_depth_is_filled_with_the_farthest_valid_depth(self):
        depth = numpy.array([[1.0, numpy.inf], [2.0, 3.0]])
        valid = numpy.array([[True, True], [True, False]])
        filled, mask = geometry_models.filled(depth, valid)
        self.assertEqual(filled.dtype, numpy.float32)
        numpy.testing.assert_array_equal(mask, [[True, False], [True, False]])
        numpy.testing.assert_array_equal(filled, [[1.0, 2.0], [2.0, 2.0]])

    def test_opencv_normals_become_y_up_z_towards_the_camera(self):
        facing_camera = numpy.array([[[0.0, 0.0, -1.0], [0.0, -1.0, 0.0]]])
        numpy.testing.assert_array_equal(geometry_models.opencv_to_camera(facing_camera),
                                         [[[0.0, 0.0, 1.0], [0.0, 1.0, 0.0]]])

    def test_outputs_are_resized_to_the_photograph(self):
        self.assertEqual(geometry_models.resized(numpy.ones((4, 6), numpy.float32), 8, 12).shape, (8, 12))
        self.assertEqual(geometry_models.resized(numpy.ones((4, 6, 3), numpy.float32), 8, 12).shape, (8, 12, 3))

    def test_moge3_is_refused_on_the_processor(self):
        import torch

        with self.assertRaisesRegex(geometry_models.ModelLoadError, "only on a CUDA GPU"):
            geometry_models.load("moge3-vitl", Path("/nonexistent"), torch.device("cpu"))

    def test_unknown_entries_are_refused(self):
        with self.assertRaises(geometry_models.ModelLoadError):
            geometry_models.load("albedo-net", Path("/nonexistent"), None)


class RequestTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.directory = Path(self.temp.name)
        Image.new("RGB", (400, 200), (90, 120, 150)).save(self.directory / "photo.jpg")

    def run_request(self, **overrides):
        request = {"image": str(self.directory / "photo.jpg"), "entry": "moge2-vitl-normal",
                   "snapshot": str(self.directory), "device": "cpu", "side": 1024,
                   "outputs": {name: str(self.directory / f"out-{name}") for name in ("depth", "valid", "normals")}}
        request.update(overrides)
        path = self.directory / "request.json"
        path.write_text(json.dumps(request))
        printed = io.StringIO()
        with contextlib.redirect_stdout(printed):
            status = estimate_geometry.main([str(path)])
        return status, json.loads(printed.getvalue())

    def test_a_run_writes_each_output_at_the_processed_size(self):
        def runner(rgb):
            height, width = rgb.shape[:2]
            normals = numpy.zeros((height, width, 3), numpy.float32)
            normals[..., 2] = 1.0
            return Geometry(depth=numpy.full((height, width), 2.0, numpy.float32), depth_kind="metric",
                            valid=numpy.ones((height, width), bool), focal_px=800.0, normals=normals)

        with mock.patch.object(geometry_models, "load", return_value=runner) as load:
            status, printed = self.run_request()
        self.assertEqual(status, 0, printed)
        result = printed["result"]
        self.assertEqual((result["width"], result["height"]), (400, 200))
        self.assertEqual(sorted(result["outputs"]), ["depth", "normals", "valid"])
        self.assertEqual((result["depth_kind"], result["focal_px"], result["valid_fraction"]), ("metric", 800.0, 1.0))
        self.assertEqual(load.call_args.args[0], "moge2-vitl-normal")
        depth = numpy.load(self.directory / "out-depth", allow_pickle=False)
        self.assertEqual(depth.shape, (200, 400))
        with Image.open(self.directory / "out-valid") as valid:
            self.assertEqual((valid.size, valid.mode), ((400, 200), "L"))

    def test_small_photographs_are_enlarged_only_for_the_2048_routes(self):
        small = Image.new("RGB", (400, 200))
        self.assertEqual(estimate_geometry.scaled(small, 1024).shape, (200, 400, 3))
        self.assertEqual(estimate_geometry.scaled(small, 2048).shape, (1024, 2048, 3))
        self.assertEqual(estimate_geometry.scaled(Image.new("RGB", (3000, 1500)), 1024).shape, (512, 1024, 3))

    def test_outputs_a_model_does_not_produce_are_not_written(self):
        def runner(rgb):
            return Geometry(depth=numpy.ones(rgb.shape[:2], numpy.float32), depth_kind="relative")

        with mock.patch.object(geometry_models, "load", return_value=runner):
            status, printed = self.run_request()
        self.assertEqual(status, 0, printed)
        self.assertEqual(printed["result"]["outputs"], ["depth"])
        self.assertFalse((self.directory / "out-normals").exists())

    def test_invalid_requests_are_refused_before_loading_a_model(self):
        cases = [({"device": "mps"}, "device must be"), ({"side": 512}, "side one of"),
                 ({"threads": 0}, "threads"), ({"outputs": None}, "outputs must be an object")]
        for overrides, message in cases:
            with mock.patch.object(geometry_models, "load") as load:
                status, printed = self.run_request(**overrides)
            self.assertEqual(status, 2, overrides)
            self.assertEqual(printed["error"]["code"], "validation")
            self.assertIn(message, printed["error"]["message"])
            load.assert_not_called()

    def test_a_model_that_cannot_load_is_a_checkpoint_error(self):
        with mock.patch.object(geometry_models, "load", side_effect=geometry_models.ModelLoadError("missing")):
            status, printed = self.run_request()
        self.assertEqual((status, printed["error"]["code"]), (2, "checkpoints"))


if __name__ == "__main__":
    unittest.main()
