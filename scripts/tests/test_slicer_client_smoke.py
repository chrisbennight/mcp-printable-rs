"""Keep public native camera replay checks tolerant of matrix roundoff."""

import copy
import unittest

from slicer_client_smoke import verify_camera_replay


class CameraReplayTests(unittest.TestCase):
    def setUp(self):
        self.camera = {"preset": "isometric", "projection": "orthographic",
                       "margin": 1.1, "zoom": 1.25,
                       "orientation": [0.8, 0.2, 0.4, 0.4],
                       "target": [128.0, -75.0, 125.0]}

    def test_native_roundoff_preserves_the_view(self):
        replay = copy.deepcopy(self.camera)
        replay["target"][1] += 4.6e-13
        replay["zoom"] += 1e-13
        verify_camera_replay(replay, self.camera)

    def test_changed_view_is_rejected(self):
        for key, value in (("target", [129.0, -75.0, 125.0]),
                           ("orientation", [0.8, 0.2, 0.4]),
                           ("projection", "perspective"), ("zoom", float("nan"))):
            with self.subTest(key=key):
                replay = {**self.camera, key: value}
                with self.assertRaisesRegex(ValueError, "does not replay"):
                    verify_camera_replay(replay, self.camera)
