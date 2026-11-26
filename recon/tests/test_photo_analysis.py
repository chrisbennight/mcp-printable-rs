"""Photograph analysis: plans, arithmetic, stored formats and the run, with fake models and no weights."""

import contextlib
import importlib.util
import io
import json
from pathlib import Path
import tempfile
import unittest
from unittest import mock

import numpy
from PIL import Image

import analyze_photo
from printable_recon import photo_analysis
from printable_recon.geometry_models import Geometry
from printable_recon.photo_analysis import MapFormatError, Outcome, RequestError, Settings
from printable_recon.registry import Host, NoRoute, Registry

MODELS = Path(__file__).resolve().parents[1] / "models"
REGISTRY = Registry.read(MODELS / "geometry.json")
LARGE_GPU = Host(24576)
SMALL_GPU = Host(3072)
NO_GPU = Host(None)


def roles(plan):
    return {role: (route["entry"], route["device"], route["side"]) for role, route in plan.routes.items()}


class SettingsTests(unittest.TestCase):
    def test_defaults_are_best_quality_without_normals(self):
        settings = Settings.parse({"subject": "deep"})
        self.assertEqual(settings.as_dict(), {"subject": "deep", "quality": "best", "interpretation": None,
                                              "normals": False, "focal_length_px": None})

    def test_invalid_settings_are_refused(self):
        for value, message in [({}, "subject"), ({"subject": "flat"}, "subject"),
                               ({"subject": "deep", "quality": "slow"}, "quality"),
                               ({"subject": "deep", "interpretation": "physical"}, "shallow subjects"),
                               ({"subject": "shallow", "interpretation": "painting"}, "interpretation"),
                               ({"subject": "shallow", "normals": 1}, "normals"),
                               ({"subject": "shallow", "focal_length_px": 1}, "focal_length_px"),
                               ({"subject": "shallow", "focal_length_px": True}, "focal_length_px"),
                               ({"subject": "shallow", "seed": 1}, "unknown"), ([], "object")]:
            with self.assertRaises(RequestError, msg=value) as raised:
                Settings.parse(value)
            self.assertIn(message, str(raised.exception))


class PlanTests(unittest.TestCase):
    def test_a_shallow_subject_runs_each_model_once_with_consistency_passes_on_depth(self):
        plan = photo_analysis.plan(Settings("shallow", normals=True), REGISTRY, LARGE_GPU)
        self.assertEqual(plan.depth_role, "depth.shallow")
        self.assertEqual(roles(plan), {"depth.shallow": ("da3-mono-large", "cuda", 1024),
                                       "mask": ("moge3-vitg", "cuda", 1024), "focal": ("dp2-l", "cuda", 1024),
                                       "normals": ("marigold-v2", "cuda", 1024)})
        self.assertEqual(plan.runs[0].passes, ("main", "flip", "rescale"))
        self.assertTrue(all(run.passes == ("main",) for run in plan.runs[1:]))
        self.assertEqual(plan.entries, ["da3-mono-large", "moge3-vitg", "dp2-l", "marigold-v2"])

    def test_a_deep_subject_runs_on_the_gpu_at_2048_px_and_shares_a_model_with_focal_when_it_can(self):
        plan = photo_analysis.plan(Settings("deep"), REGISTRY, LARGE_GPU)
        self.assertEqual(roles(plan)["depth.deep"], ("dp2-h", "cuda", 2048))
        small = photo_analysis.plan(Settings("deep"), REGISTRY, SMALL_GPU)
        self.assertEqual(roles(small)["depth.deep"], ("dp2-l", "cuda", 1024))
        self.assertTrue(small.routes["depth.deep"]["fallback"])
        depth_run = small.run_for("depth.deep")
        self.assertEqual(depth_run.roles, ("depth.deep", "focal"))
        self.assertEqual(len(small.runs), 2)

    def test_physical_artwork_takes_depth_from_the_mask_model(self):
        plan = photo_analysis.plan(Settings("shallow", interpretation="physical"), REGISTRY, LARGE_GPU)
        self.assertEqual(plan.depth_role, "mask")
        self.assertNotIn("depth.shallow", plan.routes)
        self.assertEqual(plan.run_for("mask").passes, ("main", "flip", "rescale"))

    def test_fast_quality_takes_the_fastest_fitting_routes_without_extra_passes(self):
        plan = photo_analysis.plan(Settings("deep", quality="fast"), REGISTRY, LARGE_GPU)
        self.assertEqual(roles(plan)["depth.deep"], ("dp2-h", "cuda", 1024))
        self.assertEqual(roles(plan)["mask"], ("moge2-vitl-normal", "cuda", 1024))
        self.assertTrue(all(run.passes == ("main",) for run in plan.runs))
        self.assertIn("slower than the chosen option", plan.routes["depth.deep"]["rejected"][0])

    def test_a_known_focal_length_skips_the_focal_model(self):
        plan = photo_analysis.plan(Settings("shallow", focal_length_px=900.0), REGISTRY, LARGE_GPU)
        self.assertNotIn("focal", plan.routes)
        self.assertNotIn("dp2-l", plan.entries)

    def test_without_a_gpu_every_run_is_on_the_processor(self):
        plan = photo_analysis.plan(Settings("deep", normals=True), REGISTRY, NO_GPU)
        self.assertEqual({run.device for run in plan.runs}, {"cpu"})

    def test_an_unavailable_role_is_refused(self):
        with self.assertRaises(NoRoute):
            photo_analysis.plan(Settings("deep"), REGISTRY, Host(None, 1024))


class ArithmeticTests(unittest.TestCase):
    def test_scale_and_shift_are_removed_before_depths_are_compared(self):
        target = numpy.linspace(1, 3, 64, dtype=numpy.float32).reshape(8, 8)
        mask = numpy.ones((8, 8), bool)
        difference = photo_analysis.relative_difference(target * 4 + 2, target, mask)
        self.assertLess(float(difference.max()), 1e-5)
        bent = target.copy()
        bent[0, 0] *= 2
        self.assertGreater(float(photo_analysis.relative_difference(bent, target, mask)[0, 0]), 0.3)

    def test_a_plane_facing_the_camera_has_normals_towards_it(self):
        mask = numpy.ones((9, 11), bool)
        normals, defined = photo_analysis.normals_from_depth(numpy.full((9, 11), 2.0), 500.0, mask)
        numpy.testing.assert_allclose(normals[defined], [[0, 0, 1]] * int(defined.sum()), atol=1e-6)
        self.assertFalse(defined[0].any() or defined[:, -1].any())

    def test_a_floor_below_the_camera_has_normals_pointing_up(self):
        height, width, focal = 40, 30, 50.0
        v = numpy.arange(height, dtype=numpy.float64)[:, None] - (height - 1) / 2
        rows = numpy.repeat(v, width, axis=1)
        below = rows > 2
        # The plane y = -1: a pixel v rows below the centre sees it at depth focal / v.
        depth = numpy.where(below, focal / numpy.maximum(rows, 1e-3), 1.0)
        normals, defined = photo_analysis.normals_from_depth(depth, focal, below)
        numpy.testing.assert_allclose(normals[defined], [[0, 1, 0]] * int(defined.sum()), atol=1e-4)

    def test_angles_between_normals_are_in_degrees(self):
        up = numpy.array([[[0, 1, 0]]], numpy.float32)
        towards = numpy.array([[[0, 0, 1]]], numpy.float32)
        self.assertAlmostEqual(float(photo_analysis.normal_angles(up, towards)[0, 0]), 90.0, places=4)


def outcome(depth, kind="relative", valid=None, focal=None, normals=None, passes=None):
    height, width = depth.shape
    geometry = Geometry(depth=depth.astype(numpy.float32), depth_kind=kind,
                        valid=numpy.ones(depth.shape, bool) if valid is None else valid,
                        focal_px=focal, normals=normals)
    return Outcome(geometry, width, height, passes or {})


class CombineTests(unittest.TestCase):
    def setUp(self):
        self.depth = numpy.repeat(numpy.linspace(1.5, 2.5, 24, dtype=numpy.float32)[None], 16, axis=0)
        self.subject = numpy.zeros((16, 24), bool)
        self.subject[4:12, 6:18] = True

    def shallow(self, **settings):
        settings = Settings("shallow", **settings)
        plan = photo_analysis.plan(settings, REGISTRY, LARGE_GPU)
        return settings, plan

    def test_confidence_is_zero_outside_the_mask_and_falls_where_a_check_disagrees(self):
        settings, plan = self.shallow()
        flipped = self.depth.copy()
        flipped[6, 8] *= 2
        outcomes = {plan.run_for("depth.shallow"): outcome(self.depth, passes={
                        "flip": (flipped, numpy.ones_like(self.subject)),
                        "rescale": (self.depth, numpy.ones_like(self.subject))}),
                    plan.run_for("mask"): outcome(self.depth / 2, "metric", valid=self.subject),
                    plan.run_for("focal"): outcome(self.depth, "metric", focal=600.0)}
        analysis = photo_analysis.combine(plan, settings, outcomes, photograph_width=48)
        self.assertEqual(float(analysis.confidence[~self.subject].max()), 0.0)
        self.assertEqual(float(analysis.confidence[6, 8]), 0.0)
        self.assertEqual(float(analysis.confidence[8, 12]), 1.0)
        manifest = analysis.manifest
        self.assertEqual(manifest["focal_length"], {"source": "estimated", "model": "dp2-l", "px": 600.0,
                                                    "photograph_px": 1200.0})
        self.assertEqual(manifest["artifacts"]["confidence"]["checks"],
                         ["flip_consistency", "rescale_consistency", "model_disagreement"])
        self.assertEqual(manifest["scores"]["rescale_consistency"]["agreement"], 1.0)
        self.assertLess(manifest["scores"]["flip_consistency"]["agreement"], 1.0)
        self.assertEqual(manifest["scores"]["model_disagreement"]["models"], ["da3-mono-large", "moge3-vitg"])
        self.assertEqual(manifest["depth"], {"role": "depth.shallow", "model": "da3-mono-large", "kind": "relative"})
        self.assertNotIn("metric_estimate", manifest)
        self.assertEqual(manifest["artifacts"]["normals"]["source"], "depth")

    def test_metric_depth_carries_the_close_up_warning_and_a_given_focal_length(self):
        settings = Settings("deep", quality="fast", focal_length_px=2000.0, normals=True)
        plan = photo_analysis.plan(settings, REGISTRY, LARGE_GPU)
        towards = numpy.zeros((8, 12, 3), numpy.float32)
        towards[..., 2] = 1
        flat = numpy.full((16, 24), 2.0, numpy.float32)
        outcomes = {plan.run_for("depth.deep"): outcome(flat, "metric"),
                    plan.run_for("mask"): outcome(flat, "metric", valid=self.subject),
                    plan.run_for("normals"): outcome(flat, normals=towards)}
        analysis = photo_analysis.combine(plan, settings, outcomes, photograph_width=96)
        manifest = analysis.manifest
        self.assertEqual(manifest["focal_length"], {"source": "given", "px": 500.0, "photograph_px": 2000.0})
        self.assertEqual(manifest["metric_estimate"]["median_distance_m"], 2.0)
        self.assertEqual(manifest["warnings"][0], photo_analysis.METRIC_WARNING)
        self.assertEqual(manifest["artifacts"]["depth"]["units"], "metres, estimated")
        self.assertEqual(manifest["artifacts"]["normals"]["source"], "marigold-v2")
        self.assertEqual(manifest["scores"]["normal_agreement"]["median_angle_degrees"], 0.0)
        self.assertEqual(analysis.normals.shape, (16, 24, 3))

    def test_without_a_check_the_confidence_is_the_mask_and_says_so(self):
        settings = Settings("shallow", quality="fast", interpretation="physical", focal_length_px=900.0)
        plan = photo_analysis.plan(settings, REGISTRY, LARGE_GPU)
        analysis = photo_analysis.combine(plan, settings, {plan.run_for("mask"): outcome(
            self.depth, "metric", valid=self.subject)}, photograph_width=24)
        numpy.testing.assert_array_equal(analysis.confidence, self.subject.astype(numpy.float32))
        self.assertIn("No consistency check ran", " ".join(analysis.manifest["warnings"]))


class StoredFormatTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.directory = Path(self.temp.name)

    def test_writers_refuse_eight_bit_and_integer_maps(self):
        eight = numpy.zeros((4, 4), numpy.uint8)
        with self.assertRaises(MapFormatError):
            photo_analysis.write_depth(eight, self.directory / "depth.tiff")
        with self.assertRaises(MapFormatError):
            photo_analysis.write_unit(eight, self.directory / "confidence.png", "confidence")
        with self.assertRaises(MapFormatError):
            photo_analysis.write_mask(eight, self.directory / "mask.png")
        with self.assertRaises(MapFormatError):
            photo_analysis.write_normals(numpy.zeros((4, 4, 3), numpy.uint8), self.directory / "normals.png")
        self.assertEqual(list(self.directory.iterdir()), [])

    def test_an_eight_bit_map_on_disk_is_refused(self):
        Image.new("L", (4, 4)).save(self.directory / "mask.png")
        Image.fromarray(numpy.zeros((4, 4), numpy.uint8)).save(self.directory / "depth.tiff")
        for name, expected in (("mask.png", {"format": "png", "bit_depth": 16, "channels": 1, "sample": "uint16"}),
                               ("depth.tiff", {"format": "tiff", "bit_depth": 32, "channels": 1,
                                               "sample": "float32"})):
            with self.assertRaises(MapFormatError, msg=name) as raised:
                photo_analysis.verify(self.directory / name, expected)
            self.assertIn("8-bit", str(raised.exception))

    def test_every_map_is_stored_at_16_bits_or_as_float_and_reads_back(self):
        settings = Settings("shallow", quality="fast", interpretation="physical", focal_length_px=900.0)
        plan = photo_analysis.plan(settings, REGISTRY, LARGE_GPU)
        depth = numpy.full((12, 20), 1.5, numpy.float32)
        depth[0, 0] = 1.25
        valid = numpy.zeros((12, 20), bool)
        valid[2:10, 3:17] = True
        analysis = photo_analysis.combine(plan, settings, {plan.run_for("mask"): outcome(depth, "metric", valid)}, 20)
        manifest = photo_analysis.write_all(analysis, self.directory)
        for name, description in manifest["artifacts"].items():
            stored = photo_analysis.stored_format(self.directory / description["file"])
            self.assertGreaterEqual(stored["bit_depth"], 16, name)
        with Image.open(self.directory / "depth.tiff") as image:
            self.assertEqual(image.mode, "F")
            numpy.testing.assert_array_equal(numpy.asarray(image), depth)
        with Image.open(self.directory / "mask.png") as image:
            numpy.testing.assert_array_equal(numpy.asarray(image).astype(numpy.uint32) == 65535, valid)
        normals = photo_analysis.read_png16(self.directory / "normals.png").astype(numpy.float64) / 65535 * 2 - 1
        self.assertGreater(float(normals[6, 10, 2]), 0.99)


class CommandTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.directory = Path(self.temp.name)

    def call(self, argv):
        printed = io.StringIO()
        with contextlib.redirect_stdout(printed):
            status = analyze_photo.main(argv)
        return status, json.loads(printed.getvalue())

    def request(self, **fields):
        path = self.directory / "request.json"
        path.write_text(json.dumps(fields))
        return str(path)

    def test_a_plan_names_the_entries_to_prepare_without_loading_a_model(self):
        status, printed = self.call(["--plan", self.request(settings={"subject": "deep", "normals": True},
                                                            gpu_budget_mib=3072, ram_budget_mib=None)])
        self.assertEqual(status, 0, printed)
        self.assertEqual(printed["result"]["entries"], ["dp2-l", "moge3-vitl", "marigold-v2"])
        self.assertEqual(printed["result"]["runs"][0]["passes"], ["main", "flip", "rescale"])

    def test_invalid_requests_and_missing_routes_are_refused(self):
        status, printed = self.call(["--plan", self.request(settings={"subject": "flat"})])
        self.assertEqual((status, printed["error"]["code"]), (2, "validation"))
        status, printed = self.call(["--plan", self.request(settings={"subject": "deep"}, ram_budget_mib=600)])
        self.assertEqual((status, printed["error"]["code"]), (2, "unavailable"))
        status, printed = self.call([self.request(settings={"subject": "deep"}, gpu_budget_mib=3072,
                                                  image="photo.jpg", output=str(self.directory), prepared={})])
        self.assertEqual((status, printed["error"]["code"]), (2, "validation"))


@unittest.skipUnless(importlib.util.find_spec("torch"), "the run needs PyTorch, as in the worker image")
class RunTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.directory = Path(self.temp.name)
        Image.new("RGB", (400, 200), (90, 120, 150)).save(self.directory / "photo.jpg")
        (self.directory / "out").mkdir()

    def test_each_model_is_loaded_once_and_the_maps_are_written(self):
        from printable_recon import geometry_models

        calls = []

        def load(entry, snapshot, device):
            def run(rgb):
                calls.append((entry, rgb.shape))
                height, width = rgb.shape[:2]
                valid = numpy.zeros((height, width), bool)
                valid[height // 4: 3 * height // 4, width // 4: 3 * width // 4] = True
                return Geometry(depth=numpy.full((height, width), 2.0, numpy.float32),
                                depth_kind="relative" if entry == "da3-mono-large" else "metric",
                                valid=valid, focal_px=300.0)
            return run

        request = {"image": str(self.directory / "photo.jpg"), "output": str(self.directory / "out"),
                   "settings": {"subject": "shallow"}, "gpu_budget_mib": None,
                   "prepared": {name: {"snapshot": str(self.directory)} for name in REGISTRY.entries}}
        path = self.directory / "request.json"
        path.write_text(json.dumps(request))
        printed = io.StringIO()
        with mock.patch.object(geometry_models, "load", side_effect=load) as loaded, \
                contextlib.redirect_stdout(printed):
            status = analyze_photo.main([str(path)])
        answer = json.loads(printed.getvalue())
        self.assertEqual(status, 0, answer)
        self.assertEqual([call.args[0] for call in loaded.call_args_list], ["da3-mono-large", "moge2-vitl-normal",
                                                                            "dp2-l"])
        # The depth model ran on the photograph, mirrored and at three quarters of its size.
        self.assertEqual([shape for entry, shape in calls if entry == "da3-mono-large"],
                         [(200, 400, 3), (200, 400, 3), (150, 300, 3)])
        manifest = answer["result"]["manifest"]
        self.assertEqual(manifest["analyzer"], analyze_photo.VERSION)
        for description in manifest["artifacts"].values():
            self.assertGreaterEqual(photo_analysis.stored_format(
                self.directory / "out" / description["file"])["bit_depth"], 16)


if __name__ == "__main__":
    unittest.main()
