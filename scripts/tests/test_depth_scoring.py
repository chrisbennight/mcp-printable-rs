import math
import unittest

import numpy as np

from depth_benchmark import scoring

FOCAL = 500.0


def plane_depth(h, w, distance=2.0, tilt_x=0.0, tilt_y=0.0):
    """Depth of the plane z_cam = -(distance + tilt_x * X + tilt_y * Y) seen by a pinhole camera."""
    v, u = np.mgrid[0:h, 0:w].astype(np.float64)
    rx = (u - (w - 1) / 2) / FOCAL
    ry = -(v - (h - 1) / 2) / FOCAL
    return distance / (1.0 - tilt_x * rx - tilt_y * ry)


def sphere_depth(h, w, centre_distance=3.0, radius=1.0):
    """Depth and outward normals of the near side of a sphere centred on the optical axis."""
    v, u = np.mgrid[0:h, 0:w].astype(np.float64)
    d = np.stack([(u - (w - 1) / 2) / FOCAL, -(v - (h - 1) / 2) / FOCAL, -np.ones((h, w))], axis=-1)
    d /= np.linalg.norm(d, axis=-1, keepdims=True)
    centre = np.array([0.0, 0.0, -centre_distance])
    b = d @ centre
    disc = b**2 - (centre @ centre - radius**2)
    hit = disc > 0
    t = np.where(hit, b - np.sqrt(np.maximum(disc, 0)), np.nan)
    p = d * t[..., None]
    normals = (p - centre) / radius
    return -p[..., 2], normals, hit


class AlignmentTest(unittest.TestCase):
    def setUp(self):
        self.truth = plane_depth(40, 50, tilt_x=0.3)
        self.mask = np.ones(self.truth.shape, bool)

    def test_metric_recovers_scale(self):
        aligned, params = scoring.align(self.truth * 0.25, self.truth, self.mask, scoring.METRIC)
        self.assertAlmostEqual(params["scale"], 4.0, places=9)
        np.testing.assert_allclose(aligned, self.truth, rtol=1e-12)

    def test_relative_recovers_scale_and_offset(self):
        aligned, _ = scoring.align(self.truth * 3.0 - 1.5, self.truth, self.mask, scoring.RELATIVE)
        np.testing.assert_allclose(aligned, self.truth, rtol=1e-9)

    def test_log_relative_recovers_power_and_scale(self):
        pred = 0.5 * np.log(self.truth) + 2.0
        aligned, _ = scoring.align(pred, self.truth, self.mask, scoring.LOG_RELATIVE)
        np.testing.assert_allclose(aligned, self.truth, rtol=1e-9)

    def test_unknown_kind_is_rejected(self):
        with self.assertRaises(ValueError):
            scoring.align(self.truth, self.truth, self.mask, "inverse")

    def test_depth_errors_are_zero_for_exact_depth(self):
        errors = scoring.depth_errors(self.truth, self.truth, self.mask)
        self.assertEqual(errors["absrel"], 0.0)
        self.assertEqual(errors["delta_1.05"], 1.0)


class NormalTest(unittest.TestCase):
    def test_fronto_parallel_plane_faces_the_camera(self):
        n = scoring.normals_from_depth(plane_depth(30, 30), FOCAL)
        np.testing.assert_allclose(n[..., 2], 1.0, atol=1e-9)

    def test_tilted_plane_has_its_analytic_normal(self):
        depth = plane_depth(30, 40, tilt_x=0.5)
        expected = np.array([0.5, 0.0, 1.0]) / math.hypot(0.5, 1.0)
        n = scoring.normals_from_depth(depth, FOCAL)
        # arccos loses precision near zero angle, so allow a ten-thousandth of a degree.
        self.assertLess(scoring.angle_deg(n, np.broadcast_to(expected, n.shape)).max(), 1e-4)

    def test_sphere_normals_match_within_a_degree(self):
        depth, normals, hit = sphere_depth(120, 120)
        inner = scoring.erode(hit, 2)
        filled = np.where(hit, depth, np.nanmax(depth))
        estimated = scoring.normals_from_depth(filled, FOCAL)
        errors = scoring.normal_errors(estimated, normals, inner)
        self.assertLess(errors["median_deg"], 1.0)

    def test_normal_errors_are_zero_for_identical_fields(self):
        n = np.zeros((5, 5, 3))
        n[..., 2] = 1.0
        errors = scoring.normal_errors(n, n, np.ones((5, 5), bool))
        self.assertEqual(errors, {"median_deg": 0.0, "within_11": 1.0})


class EdgeTest(unittest.TestCase):
    def step(self, column):
        depth = np.full((30, 40), 2.0)
        depth[:, column:] = 3.0
        return depth

    def test_identical_edges_score_one(self):
        truth = self.step(20)
        self.assertAlmostEqual(scoring.boundary_f1(truth, truth, np.ones(truth.shape, bool)), 1.0)

    def test_one_pixel_shift_is_tolerated(self):
        self.assertAlmostEqual(scoring.boundary_f1(self.step(21), self.step(20), np.ones((30, 40), bool)), 1.0)

    def test_missing_edge_scores_zero(self):
        flat = np.full((30, 40), 2.0)
        self.assertEqual(scoring.boundary_f1(flat, self.step(20), np.ones((30, 40), bool)), 0.0)

    def test_no_true_edges_returns_none(self):
        flat = np.full((30, 40), 2.0)
        self.assertIsNone(scoring.boundary_f1(flat, flat, np.ones((30, 40), bool)))

    def test_smooth_mask_excludes_depth_jumps(self):
        truth = self.step(20)
        smooth = scoring.smooth_mask(truth, np.ones(truth.shape, bool))
        self.assertFalse(smooth[:, 17:23].any())
        self.assertTrue(smooth[:, :15].all())


class ReliefTest(unittest.TestCase):
    def setUp(self):
        h, w = 60, 80
        v, u = np.mgrid[0:h, 0:w]
        bump = 0.01 * np.exp(-(((u - 40) / 10.0) ** 2 + ((v - 30) / 10.0) ** 2))
        self.truth = plane_depth(h, w, distance=0.5, tilt_y=0.2) - bump
        self.region = np.ones((h, w), bool)

    def test_exact_relief_has_no_error(self):
        result = scoring.relief_error(self.truth, self.truth, self.region, scoring.METRIC)
        self.assertLess(result["rmse"], 1e-12)
        self.assertAlmostEqual(result["relief"], 0.01, delta=0.002)

    def test_scaled_and_offset_relief_has_no_error(self):
        result = scoring.relief_error(self.truth * 7.0 + 3.0, self.truth, self.region, scoring.METRIC)
        self.assertLess(result["rmse"], 1e-9)

    def test_flattened_relief_reports_its_loss(self):
        flat = plane_depth(60, 80, distance=0.5, tilt_y=0.2)
        result = scoring.relief_error(flat, self.truth, self.region, scoring.RELATIVE)
        self.assertGreater(result["rmse_pct"], 10.0)

    def test_inverted_relief_scores_no_better_than_a_flat_one(self):
        inverted = 2 * self.truth.mean() - self.truth
        flat = np.full_like(self.truth, self.truth.mean())
        inverted_error = scoring.relief_error(inverted, self.truth, self.region, scoring.RELATIVE)
        flat_error = scoring.relief_error(flat, self.truth, self.region, scoring.RELATIVE)
        self.assertGreater(inverted_error["rmse_pct"], 10.0)
        self.assertAlmostEqual(inverted_error["rmse"], flat_error["rmse"], places=9)

    def test_inverted_log_depth_is_not_rewarded(self):
        aligned, params = scoring.align(-np.log(self.truth), self.truth, self.region, scoring.LOG_RELATIVE)
        self.assertEqual(params["scale"], 0.0)
        self.assertGreater(scoring.depth_errors(aligned, self.truth, self.region)["absrel"], 0.0)

    def test_plane_residual_removes_a_plane(self):
        h, w = 20, 30
        v, u = np.mgrid[0:h, 0:w]
        residual = scoring.plane_residual(0.3 * u - 0.2 * v + 5.0, np.ones((h, w), bool))
        self.assertLess(np.abs(residual).max(), 1e-9)


class MorphologyTest(unittest.TestCase):
    def test_dilate_and_erode(self):
        mask = np.zeros((7, 7), bool)
        mask[3, 3] = True
        self.assertEqual(int(scoring.dilate(mask).sum()), 9)
        self.assertTrue(np.array_equal(scoring.erode(scoring.dilate(mask)), mask))

    def test_erode_treats_outside_as_empty(self):
        self.assertEqual(int(scoring.erode(np.ones((5, 5), bool)).sum()), 9)


class FocalAndIntervalTest(unittest.TestCase):
    def test_focal_from_35mm_matches_a_full_frame_sensor(self):
        # A 36 x 24 mm sensor imaged at 3600 x 2400 pixels has 100 pixels per millimetre.
        self.assertAlmostEqual(scoring.focal_from_35mm(50, 3600, 2400), 5000.0, places=6)

    def test_focal_error_pct(self):
        self.assertAlmostEqual(scoring.focal_error_pct(110.0, 100.0), 10.0)
        self.assertIsNone(scoring.focal_error_pct(None, 100.0))

    def test_bootstrap_is_deterministic_and_brackets_the_mean(self):
        values = [1.0, 2.0, 3.0, 4.0, 5.0]
        first = scoring.bootstrap_mean(values)
        self.assertEqual(first, scoring.bootstrap_mean(values))
        self.assertEqual(first.mean, 3.0)
        self.assertLessEqual(first.low, 3.0)
        self.assertGreaterEqual(first.high, 3.0)

    def test_bootstrap_ignores_missing_values(self):
        self.assertEqual(scoring.bootstrap_mean([2.0, None, float("nan")]).mean, 2.0)
        self.assertIsNone(scoring.bootstrap_mean([None]))


if __name__ == "__main__":
    unittest.main()
