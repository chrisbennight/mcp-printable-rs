"""photo.compare scores a map against a benchmark scene with the benchmark report's own numbers."""

from pathlib import Path
import sys
import tempfile
import unittest
from unittest import mock

import numpy as np

ROOT = Path(__file__).resolve().parents[2]
# The benchmark report imports its siblings by name, as the worker imports scoring.
sys.path[:0] = [str(ROOT / "scripts" / "depth_benchmark"), str(ROOT / "recon")]

import report  # noqa: E402
import scoring  # noqa: E402
from printable_recon import comparison  # noqa: E402


class BenchmarkSceneTests(unittest.TestCase):
    def test_a_map_scored_through_compare_matches_the_report(self):
        h, w = 60, 80
        v, u = np.mgrid[0:h, 0:w].astype(np.float64)
        bump = 0.004 * np.exp(-(((u - 40) / 9.0) ** 2 + ((v - 30) / 9.0) ** 2))
        depth = 0.5 + 0.0004 * v - bump
        valid = np.ones((h, w), bool)
        valid[:3] = False
        truth = {"depth": depth, "normals": scoring.normals_from_depth(depth, 120.0).astype(np.float32),
                 "valid": valid, "focal": 120.0, "region": valid & (bump > 0.0005)}
        rng = np.random.default_rng(1)
        prediction = (1.5 * depth + 0.2 + rng.normal(0, 0.0003, (h, w))).astype(np.float32)
        with tempfile.TemporaryDirectory() as directory:
            results = Path(directory) / "results"
            (results / "gpu" / "model-a").mkdir(parents=True)
            np.savez_compressed(results / "gpu" / "model-a" / "gt-relief.npz", depth=prediction, kind="relative")
            with mock.patch.object(report.common, "RESULTS", results):
                rows = report.score_scene("relief", truth, "gpu")
            scoring.save_truth(Path(directory) / "truth.zip", truth)
            stored = comparison.load_truth(Path(directory) / "truth.zip")
        # Maps are read in double precision, as compare reads a TIFF.
        item = comparison.Input("map.tiff", "depth map", prediction.astype(np.float64), None, valid, "relative")
        scored = comparison.compare_synthetic([item], stored)[0]
        expected = rows[0]
        for key in ("absrel", "edge_f1", "depth_normal_deg", "relief_pct", "relief_rmse_mm", "relief_mm"):
            self.assertEqual(scored[key], expected[key], key)


if __name__ == "__main__":
    unittest.main()
