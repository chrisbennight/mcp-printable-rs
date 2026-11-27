"""Comparisons of maps and meshes: against each other, a photograph's analysis and a synthetic scene."""

import contextlib
import io
import json
import math
from pathlib import Path
import tempfile
import unittest
from unittest import mock

import numpy
from PIL import Image

import compare_photo
import scoring
from estimate_depth import write_normals
from printable_recon import comparison, photo_analysis

H, W = 96, 128


def dome(shift: int = 0) -> numpy.ndarray:
    """A smooth raised dome on a flat background, its height from 0 to 1."""
    v, u = numpy.mgrid[0:H, 0:W].astype(numpy.float64)
    r2 = ((u - W / 2 - shift) ** 2 + (v - H / 2) ** 2) / 30.0 ** 2
    return numpy.where(r2 < 1, numpy.sqrt(numpy.clip(1 - r2, 0, 1)), 0.0)


def height_normals(height: numpy.ndarray, scale: float) -> numpy.ndarray:
    gx, gy = comparison.height_gradients(-height)
    return comparison.normals_from_slopes(gx, gy, scale)


def write_height(path: Path, height: numpy.ndarray) -> None:
    span = max(float(height.max() - height.min()), 1e-12)
    levels = numpy.round((height - height.min()) / span * 65535).astype(numpy.uint16)
    Image.fromarray(levels).save(path, format="PNG")


def run(request: dict, directory: Path) -> dict:
    output = directory / "out"
    output.mkdir()
    request = {"kind": "relative", **request, "output": str(output)}
    path = directory / "request.json"
    path.write_text(json.dumps(request))
    printed = io.StringIO()
    with contextlib.redirect_stdout(printed):
        compare_photo.main([str(path)])
    return json.loads(printed.getvalue())


class SyntheticTests(unittest.TestCase):
    def test_an_agent_depth_map_scores_as_the_benchmark_scores_it(self):
        # A benchmark-style relief plaque: a tilted plane with a raised bump,
        # seen by a pinhole camera.
        v, u = numpy.mgrid[0:H, 0:W].astype(numpy.float64)
        # The render worker stores depth in millimetres and both maps in 32-bit floating point.
        stored = ((0.5 + 0.0005 * v - 0.01 * dome()) * 1000.0).astype(numpy.float32)
        normals = scoring.normals_from_depth(stored.astype(numpy.float64), 140.0).astype(numpy.float32)
        truth_depth = stored.astype(numpy.float64) / 1000.0
        focal = 140.0
        truth = {"depth": truth_depth, "normals": normals.astype(numpy.float64),
                 "valid": numpy.ones((H, W), bool), "focal": focal}
        truth["region"] = truth["valid"]
        rng = numpy.random.default_rng(0)
        agent = (3.0 * truth_depth + 1.0 + rng.normal(0, 0.0005, (H, W))).astype(numpy.float32)
        with tempfile.TemporaryDirectory() as temporary:
            directory = Path(temporary)
            numpy.savez(directory / "truth.npz", depth=stored, normals=normals, mask=truth["valid"],
                        focal_px=numpy.float64(focal))
            Image.fromarray(agent).save(directory / "agent.tiff", format="TIFF")
            answer = run({"mode": "synthetic", "truth": str(directory / "truth.npz"),
                          "inputs": [{"name": "maps/agent.tiff", "file": str(directory / "agent.tiff")}]},
                         directory)
            self.assertTrue((directory / "out/sheet.png").is_file())
        expected = scoring.scene_scores({"depth": agent, "kind": "relative"}, truth)
        result = answer["result"]["results"][0]
        self.assertEqual(result["source"], "depth map")
        for key, value in expected.items():
            if isinstance(value, float):
                self.assertTrue(math.isclose(result[key], value, rel_tol=1e-9, abs_tol=1e-12), key)
            else:
                self.assertEqual(result[key], value, key)
        self.assertIn("relief_pct", result)

    def test_the_benchmarks_own_truth_keeps_its_relief_region(self):
        v, u = numpy.mgrid[0:H, 0:W].astype(numpy.float64)
        depth = 0.5 + 0.0005 * v - 0.01 * dome()
        truth = {"depth": depth, "normals": scoring.normals_from_depth(depth, 140.0).astype(numpy.float32),
                 "valid": numpy.ones((H, W), bool), "focal": 140.0, "region": dome() > 0.2}
        agent = (2.0 * depth + 0.3).astype(numpy.float32)
        with tempfile.TemporaryDirectory() as temporary:
            directory = Path(temporary)
            scoring.save_truth(directory / "truth.zip", truth)
            Image.fromarray(agent).save(directory / "agent.tiff", format="TIFF")
            answer = run({"mode": "synthetic", "truth": str(directory / "truth.zip"),
                          "inputs": [{"name": "agent.tiff", "file": str(directory / "agent.tiff")}]}, directory)
        expected = scoring.scene_scores({"depth": agent, "kind": "relative"}, truth)
        result = answer["result"]["results"][0]
        for key, value in expected.items():
            self.assertEqual(result[key], value, key)

    def test_a_mesh_seen_through_the_scenes_camera_scores_as_identical(self):
        # The render worker gives a candidate mesh the scene's camera, so the
        # same mesh renders to the same archive and has no error.
        v, u = numpy.mgrid[0:H, 0:W].astype(numpy.float64)
        stored = ((0.5 + 0.0005 * v - 0.01 * dome()) * 1000.0).astype(numpy.float32)
        normals = scoring.normals_from_depth(stored.astype(numpy.float64), 140.0).astype(numpy.float32)
        with tempfile.TemporaryDirectory() as temporary:
            view = Path(temporary) / "view.zip"
            with view.open("wb") as stream:
                numpy.savez(stream, depth=stored, normals=normals, mask=numpy.ones((H, W), bool),
                            focal_px=numpy.float64(140.0))
            truth = comparison.load_truth(view)
            mesh = comparison.load("same.stl", view, "relative")
        result = comparison.compare_synthetic([mesh], truth)[0]
        self.assertLess(result["absrel"], 1e-6)
        self.assertLess(result["depth_normal_deg"], 0.01)
        self.assertLess(result["normal_deg"], 0.01)
        self.assertAlmostEqual(result["scale_error_pct"], 0.0, places=4)

    def test_pixels_without_a_value_are_not_scored(self):
        depth = 0.5 - 0.01 * dome()
        truth = {"depth": depth, "normals": scoring.normals_from_depth(depth, 140.0),
                 "valid": numpy.ones((H, W), bool), "focal": 140.0, "region": None}
        partial = depth.copy()
        partial[:, :20] = numpy.nan
        valid = numpy.isfinite(partial)
        item = comparison.Input("d.tiff", "depth map", numpy.where(valid, partial, 0.0), None, valid, "relative")
        result = comparison.compare_synthetic([item], truth)[0]
        self.assertLess(result["absrel"], 1e-6)
        empty = comparison.Input("d.tiff", "depth map", numpy.zeros((H, W)), None, numpy.zeros((H, W), bool),
                                 "relative")
        with self.assertRaisesRegex(comparison.CompareError, "no values"):
            comparison.compare_synthetic([empty], truth)

    def test_a_normal_map_is_scored_by_normal_angle(self):
        truth_depth = 0.5 - 0.01 * dome()
        truth = {"depth": truth_depth, "normals": scoring.normals_from_depth(truth_depth, 140.0),
                 "valid": numpy.ones((H, W), bool), "focal": 140.0, "region": None}
        item = comparison.Input("n.png", "normal map", None, truth["normals"], truth["valid"], "relative")
        result = comparison.compare_synthetic([item], truth)[0]
        self.assertAlmostEqual(result["normal_deg"], 0.0, places=3)


class PhotoTests(unittest.TestCase):
    """The photo mode tells a correct relief from an inverted or a shifted one."""

    def answer(self, height: numpy.ndarray) -> dict:
        truth = dome()
        normals = height_normals(truth, 40.0)
        light = numpy.array([-0.3, 0.4, 0.87])
        luminance = numpy.clip(0.1 + 0.8 * (normals * light).sum(-1), 0, 1)
        with tempfile.TemporaryDirectory() as temporary:
            directory = Path(temporary)
            photo_analysis.write_depth((2.0 - 0.1 * truth).astype(numpy.float32), directory / "depth.tiff")
            write_normals(normals.transpose(2, 0, 1), directory / "normals.png")
            photo_analysis.write_mask(truth > 0, directory / "mask.png")
            Image.fromarray((luminance * 255).astype(numpy.uint8)).save(directory / "photo.png")
            write_height(directory / "relief.height.png", height)
            answer = run({"mode": "photo", "photograph": str(directory / "photo.png"),
                          "analysis": {name: str(directory / file) for name, file in
                                       [("depth", "depth.tiff"), ("normals", "normals.png"), ("mask", "mask.png")]},
                          "inputs": [{"name": "relief.height.png", "file": str(directory / "relief.height.png")}]},
                         directory)
            self.assertTrue((directory / "out/sheet.png").is_file())
        return answer["result"]["results"][0]

    def test_correct_inverted_and_shifted_reliefs_are_told_apart(self):
        correct = self.answer(dome())
        inverted = self.answer(-dome())
        shifted = self.answer(dome(shift=12))
        self.assertGreater(correct["silhouette_iou"], 0.95)
        self.assertLess(correct["outline_distance"]["mean_px"], 1.5)
        self.assertLess(correct["normal_angle"]["median_deg"], 3.0)
        self.assertGreater(correct["shading"]["correlation"], 0.95)
        self.assertGreater(inverted["normal_angle"]["median_deg"], correct["normal_angle"]["median_deg"] + 20)
        self.assertEqual(inverted["shading"]["correlation"], 0.0)
        self.assertEqual(inverted["normal_angle"]["height_scale"], 0.0)
        self.assertLess(shifted["silhouette_iou"], correct["silhouette_iou"] - 0.2)
        self.assertGreater(shifted["outline_distance"]["mean_px"], correct["outline_distance"]["mean_px"] + 5)
        self.assertGreater(shifted["normal_angle"]["median_deg"], correct["normal_angle"]["median_deg"] + 10)

    def test_a_mesh_silhouette_is_its_coverage(self):
        # A flat mesh seen straight on has no raised area, but covers the disk.
        disk = dome() > 0
        up = numpy.zeros((H, W, 3))
        up[..., 2] = 1.0
        flat = comparison.Input("disk.stl", "mesh", numpy.full((H, W), 0.1), up, disk, "metric")
        numpy.testing.assert_array_equal(comparison.silhouette(flat), disk)
        normals = height_normals(dome(), 40.0)
        light = numpy.clip(0.1 + 0.8 * (normals * numpy.array([-0.3, 0.4, 0.87])).sum(-1), 0, 1)
        result = comparison.compare_photo([flat], {"mask": disk, "normals": normals}, light)[0][0]
        self.assertEqual(result["silhouette_iou"], 1.0)
        self.assertEqual(result["outline_distance"]["mean_px"], 0.0)
        self.assertEqual(result["shading"]["correlation"], 0.0)

    def test_a_mesh_is_measured_by_its_rendered_normals(self):
        disk = dome() > 0
        normals = height_normals(dome(), 40.0)
        light = numpy.clip(0.1 + 0.8 * (normals * numpy.array([-0.3, 0.4, 0.87])).sum(-1), 0, 1)
        # A flat depth with the dome's exact normals: slopes would find nothing.
        mesh = comparison.Input("dome.stl", "mesh", numpy.full((H, W), 0.1), normals, disk, "metric")
        result = comparison.compare_photo([mesh], {"mask": disk, "normals": normals}, light)[0][0]
        self.assertLess(result["normal_angle"]["median_deg"], 0.01)
        self.assertIsNone(result["normal_angle"]["height_scale"])
        self.assertGreater(result["shading"]["correlation"], 0.99)

    def test_a_photograph_without_shading_is_an_error(self):
        disk = dome() > 0
        item = comparison.Input("h.png", "heightmap", 1.0 - dome(), None, numpy.ones((H, W), bool), "relative")
        with self.assertRaisesRegex(comparison.CompareError, "brightness does not vary"):
            comparison.compare_photo([item], {"mask": disk, "normals": height_normals(dome(), 40.0)},
                                     numpy.full((H, W), 0.5))

    def test_a_different_aspect_is_compared_with_the_centre_of_the_photograph(self):
        self.assertEqual(comparison.centre_crop((96, 128), 1.0), (slice(0, 96), slice(16, 112)))
        self.assertEqual(comparison.centre_crop((96, 128), 128 / 96), (slice(0, 96), slice(0, 128)))


class OutputsTests(unittest.TestCase):
    def test_differences_are_aligned_and_reported_by_region(self):
        with tempfile.TemporaryDirectory() as temporary:
            directory = Path(temporary)
            write_height(directory / "a.png", dome())
            write_height(directory / "b.png", 0.5 * dome())
            write_height(directory / "c.png", dome(shift=12))
            answer = run({"mode": "outputs", "inputs": [
                {"name": name, "file": str(directory / name)} for name in ("a.png", "b.png", "c.png")]}, directory)
            same, moved = answer["result"]["results"]
            self.assertLess(same["whole"]["rmse_percent_of_range"], 0.1)
            self.assertAlmostEqual(same["correlation"], 1.0, places=6)
            self.assertGreater(moved["whole"]["rmse_percent_of_range"], 10)
            self.assertEqual(len(moved["regions"]), 9)
            self.assertGreater(abs(moved["regions"]["right"]["mean"]), abs(moved["regions"]["top left"]["mean"]))
            self.assertEqual(answer["result"]["files"], {"difference_1": "diff-1.tiff", "difference_2": "diff-2.tiff",
                                                        "sheet": "sheet.png"})
            difference = numpy.asarray(Image.open(directory / "out/diff-2.tiff"))
            self.assertEqual(difference.dtype, numpy.float32)
            self.assertEqual(difference.shape, (H, W))

    def test_identical_signed_maps_have_no_difference(self):
        with tempfile.TemporaryDirectory() as temporary:
            directory = Path(temporary)
            signed = (dome() - 0.7).astype(numpy.float32)
            for name in ("a.tiff", "b.tiff"):
                Image.fromarray(signed).save(directory / name, format="TIFF")
            answer = run({"mode": "outputs", "inputs": [
                {"name": name, "file": str(directory / name)} for name in ("a.tiff", "b.tiff")]}, directory)
            result = answer["result"]["results"][0]
            self.assertLess(result["whole"]["rmse"], 1e-6)
            self.assertAlmostEqual(result["alignment"]["scale"], 1.0, places=5)
            difference = numpy.asarray(Image.open(directory / "out/diff-1.tiff"))
            self.assertLess(numpy.abs(difference).max(), 1e-6)

    def test_inputs_of_other_proportions_or_without_heights_are_refused(self):
        with tempfile.TemporaryDirectory() as temporary:
            directory = Path(temporary)
            write_height(directory / "a.png", dome())
            write_height(directory / "square.png", numpy.zeros((64, 64)))
            write_normals(numpy.zeros((3, H, W)) + numpy.array([0, 0, 1.0])[:, None, None], directory / "n.png")
            for second, message in [("square.png", "proportions"), ("n.png", "normal map")]:
                attempt = directory / second.replace(".", "-")
                attempt.mkdir()
                answer = run({"mode": "outputs", "inputs": [{"name": "a.png", "file": str(directory / "a.png")},
                                                            {"name": second, "file": str(directory / second)}]},
                             attempt)
                self.assertEqual(answer["error"]["code"], "validation")
                self.assertIn(message, answer["error"]["message"])


class LoadingTests(unittest.TestCase):
    def test_formats(self):
        with tempfile.TemporaryDirectory() as temporary:
            directory = Path(temporary)
            Image.fromarray(numpy.zeros((4, 4), numpy.uint8)).save(directory / "eight.png")
            with self.assertRaisesRegex(comparison.CompareError, "16-bit"):
                comparison.load("eight.png", directory / "eight.png", "relative")
            write_height(directory / "h.png", dome())
            with self.assertRaisesRegex(comparison.CompareError, "relative heights"):
                comparison.load("h.png", directory / "h.png", "metric")
            loaded = comparison.load("h.png", directory / "h.png", "relative")
            self.assertEqual((loaded.source, loaded.depth.min(), loaded.depth.max()), ("heightmap", 0.0, 1.0))
            depth = numpy.where(dome() > 0, 2.0, 3.0).astype(numpy.float32)
            numpy.savez(directory / "mesh.npz", depth=depth, normals=numpy.zeros((H, W, 3), numpy.float32),
                        mask=dome() > 0, focal_px=numpy.float64(0.0))
            mesh = comparison.load("part.stl", directory / "mesh.npz", "relative")
            self.assertEqual((mesh.source, mesh.kind, float(mesh.depth.max())), ("mesh", "metric", 0.002))
            with mock.patch.object(comparison, "MAX_PIXELS", 100):
                with self.assertRaisesRegex(comparison.CompareError, "more than 100 pixels"):
                    comparison.load("h.png", directory / "h.png", "relative")
                with self.assertRaisesRegex(comparison.CompareError, "more than 100 pixels"):
                    comparison.load("part.stl", directory / "mesh.npz", "relative")
            for change, message in [({"depth": numpy.zeros((H, W))}, "f4"),
                                    ({"extra": numpy.zeros(3)}, "expected arrays"),
                                    ({"normals": numpy.zeros((H, W), numpy.float32)}, "shape"),
                                    ({"mask": numpy.zeros((H, 10), bool)}, "differ")]:
                arrays = {"depth": numpy.zeros((H, W), numpy.float32), "normals": numpy.zeros((H, W, 3), numpy.float32),
                          "mask": numpy.ones((H, W), bool), "focal_px": numpy.float64(0.0), **change}
                numpy.savez(directory / "odd.npz", **arrays)
                with self.subTest(change=sorted(change)), self.assertRaisesRegex(comparison.CompareError, message):
                    comparison.load("part.stl", directory / "odd.npz", "relative")
            (directory / "text.png").write_text("not an image")
            with self.assertRaisesRegex(comparison.CompareError, "not a PNG"):
                comparison.load("text.png", directory / "text.png", "relative")


if __name__ == "__main__":
    unittest.main()
