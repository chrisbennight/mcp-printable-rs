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
from printable_recon import geometry_files, photo_analysis
from printable_recon.geometry_models import Geometry
from printable_recon.photo_analysis import MapFormatError, Outcome, RequestError, Settings

MODELS = geometry_files.read(Path(__file__).resolve().parents[1] / "models" / "geometry.json")


def settings(**changes):
    """The recommended shallow-subject settings on a GPU, with `changes`."""
    value = {"subject": "shallow", "quality": "best", "depth_model": "da3-mono-large", "mask_model": "moge3-vitl",
             "focal_model": "dp2-l", "normals_model": None, "side": 1024, "device": "cuda",
             "focal_length_px": None}
    value.update(changes)
    return value


def roles(plan):
    return {role: (run.entry, run.side) for run in plan.runs for role in run.roles}


class SettingsTests(unittest.TestCase):
    def test_settings_name_every_model_and_round_trip(self):
        value = settings(subject="deep", depth_model="dp2-h", focal_model="dp2-h", side=2048)
        self.assertEqual(Settings.parse(value, MODELS).as_dict(), value)

    def test_invalid_settings_are_refused(self):
        for changes, message in [({"subject": "flat"}, "subject"), ({"quality": "slow"}, "quality"),
                                 ({"device": "auto"}, "device"), ({"side": 1536}, "side"), ({"side": True}, "side"),
                                 ({"depth_model": "marigold-v2"}, "depth output"),
                                 ({"mask_model": "dp2-l"}, "mask output"),
                                 ({"focal_model": "da3-mono-large"}, "focal output"),
                                 ({"normals_model": "absent"}, "normals output"),
                                 ({"mask_model": None}, "mask_model"),
                                 ({"focal_model": None}, "exactly one"),
                                 ({"focal_length_px": 900.0}, "exactly one"),
                                 ({"focal_model": None, "focal_length_px": 1}, "focal_length_px"),
                                 ({"focal_model": None, "focal_length_px": True}, "focal_length_px"),
                                 ({"seed": 1}, "exactly")]:
            with self.assertRaises(RequestError, msg=changes) as raised:
                Settings.parse(settings(**changes), MODELS)
            self.assertIn(message, str(raised.exception))
        with self.assertRaises(RequestError):
            Settings.parse([], MODELS)


class PlanTests(unittest.TestCase):
    def plan(self, **changes):
        return photo_analysis.plan(Settings.parse(settings(**changes), MODELS))

    def test_each_model_runs_once_with_consistency_passes_on_depth(self):
        plan = self.plan(normals_model="marigold-v2")
        self.assertEqual(roles(plan), {"depth": ("da3-mono-large", 1024), "mask": ("moge3-vitl", 1024),
                                       "focal": ("dp2-l", 1024), "normals": ("marigold-v2", 1024)})
        self.assertEqual(plan.runs[0].passes, ("main", "flip", "rescale"))
        self.assertTrue(all(run.passes == ("main",) for run in plan.runs[1:]))
        self.assertEqual(plan.entries, ["da3-mono-large", "moge3-vitl", "dp2-l", "marigold-v2"])
        self.assertEqual({run.device for run in plan.runs}, {"cuda"})

    def test_only_the_depth_model_takes_the_requested_side_and_shared_models_run_once(self):
        plan = self.plan(subject="deep", depth_model="dp2-h", focal_model="dp2-h", side=2048)
        self.assertEqual(roles(plan), {"depth": ("dp2-h", 2048), "focal": ("dp2-h", 2048),
                                       "mask": ("moge3-vitl", 1024)})
        self.assertEqual(len(plan.runs), 2)

    def test_depth_can_come_from_the_mask_model(self):
        plan = self.plan(depth_model="moge3-vitl")
        self.assertEqual(plan.run_for("depth"), plan.run_for("mask"))
        self.assertEqual(plan.run_for("mask").passes, ("main", "flip", "rescale"))

    def test_fast_quality_skips_the_extra_passes(self):
        self.assertTrue(all(run.passes == ("main",) for run in self.plan(quality="fast").runs))

    def test_a_known_focal_length_skips_the_focal_model(self):
        plan = self.plan(focal_model=None, focal_length_px=900.0)
        self.assertIsNone(plan.run_for("focal"))
        self.assertNotIn("dp2-l", plan.entries)


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

    def planned(self, **changes):
        chosen = Settings.parse(settings(**changes), MODELS)
        return chosen, photo_analysis.plan(chosen)

    def test_confidence_is_zero_outside_the_mask_and_falls_where_a_check_disagrees(self):
        chosen, plan = self.planned()
        flipped = self.depth.copy()
        flipped[6, 8] *= 2
        outcomes = {plan.run_for("depth"): outcome(self.depth, passes={
                        "flip": (flipped, numpy.ones_like(self.subject)),
                        "rescale": (self.depth, numpy.ones_like(self.subject))}),
                    plan.run_for("mask"): outcome(self.depth / 2, "metric", valid=self.subject),
                    plan.run_for("focal"): outcome(self.depth, "metric", focal=600.0)}
        analysis = photo_analysis.combine(plan, chosen, outcomes, photograph_width=48)
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
        self.assertEqual(manifest["scores"]["model_disagreement"]["models"], ["da3-mono-large", "moge3-vitl"])
        self.assertEqual(manifest["depth"], {"model": "da3-mono-large", "kind": "relative"})
        self.assertNotIn("metric_estimate", manifest)
        self.assertEqual(manifest["artifacts"]["normals"]["source"], "depth")

    def test_metric_depth_carries_the_close_up_warning_and_a_given_focal_length(self):
        chosen, plan = self.planned(subject="deep", quality="fast", depth_model="dp2-h", focal_model=None,
                                    focal_length_px=2000.0, normals_model="marigold-v2")
        towards = numpy.zeros((8, 12, 3), numpy.float32)
        towards[..., 2] = 1
        flat = numpy.full((16, 24), 2.0, numpy.float32)
        outcomes = {plan.run_for("depth"): outcome(flat, "metric"),
                    plan.run_for("mask"): outcome(flat, "metric", valid=self.subject),
                    plan.run_for("normals"): outcome(flat, normals=towards)}
        analysis = photo_analysis.combine(plan, chosen, outcomes, photograph_width=96)
        manifest = analysis.manifest
        self.assertEqual(manifest["focal_length"], {"source": "given", "px": 500.0, "photograph_px": 2000.0})
        self.assertEqual(manifest["metric_estimate"]["median_distance_m"], 2.0)
        self.assertEqual(manifest["warnings"][0], photo_analysis.METRIC_WARNING)
        self.assertEqual(manifest["artifacts"]["depth"]["units"], "metres, estimated")
        self.assertEqual(manifest["artifacts"]["normals"]["source"], "marigold-v2")
        self.assertEqual(manifest["scores"]["normal_agreement"]["median_angle_degrees"], 0.0)
        self.assertEqual(analysis.normals.shape, (16, 24, 3))

    def test_without_a_check_the_confidence_is_the_mask_and_says_so(self):
        chosen, plan = self.planned(quality="fast", depth_model="moge3-vitl", focal_model=None,
                                    focal_length_px=900.0)
        analysis = photo_analysis.combine(plan, chosen, {plan.run_for("mask"): outcome(
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
        chosen = Settings.parse(settings(quality="fast", depth_model="moge3-vitl", focal_model=None,
                                         focal_length_px=900.0), MODELS)
        plan = photo_analysis.plan(chosen)
        depth = numpy.full((12, 20), 1.5, numpy.float32)
        depth[0, 0] = 1.25
        valid = numpy.zeros((12, 20), bool)
        valid[2:10, 3:17] = True
        analysis = photo_analysis.combine(plan, chosen, {plan.run_for("mask"): outcome(depth, "metric", valid)}, 20)
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

    def test_invalid_requests_are_refused_before_any_model_loads(self):
        status, printed = self.call([self.request(settings={"subject": "deep"})])
        self.assertEqual((status, printed["error"]["code"]), (2, "validation"))
        status, printed = self.call([self.request(settings=settings(), image="photo.jpg",
                                                  output=str(self.directory), prepared={})])
        self.assertEqual((status, printed["error"]["code"]), (2, "validation"))
        status, printed = self.call(["--plan", self.request(settings=settings())])
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
                   "settings": settings(device="cpu", mask_model="moge2-vitl-normal"),
                   "prepared": {name: {"snapshot": str(self.directory)} for name in MODELS}}
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
