"""Keep original native regression geometry closed and dimensionally explicit."""
from collections import Counter
import importlib.util
from pathlib import Path
import tempfile
import unittest

source = Path(__file__).resolve().parents[2] / "slicer/preparation_fixture.py"
spec = importlib.util.spec_from_file_location("printable_preparation_fixture", source)
fixture = importlib.util.module_from_spec(spec)
spec.loader.exec_module(fixture)


class PreparationFixtureTests(unittest.TestCase):
    def test_fixtures_are_closed_oriented_solids_with_known_volumes(self):
        with tempfile.TemporaryDirectory() as directory:
            for kind, expected_volume in (("flat", 400), ("bridge", 720), ("relief", 212)):
                with self.subTest(kind=kind):
                    path = Path(directory) / f"{kind}.stl"
                    report = fixture.extruded_fixture(path, kind)
                    vertices = [tuple(map(float, line.split()[1:])) for line in path.read_text().splitlines()
                                if line.startswith("vertex ")]
                    faces = [vertices[index:index + 3] for index in range(0, len(vertices), 3)]
                    edges = Counter((a, b) for face in faces
                                    for a, b in zip(face, face[1:] + face[:1]))
                    self.assertTrue(all(edges[(b, a)] == count == 1 for (a, b), count in edges.items()))
                    volume = sum(a[0] * (b[1]*c[2] - b[2]*c[1])
                                 + a[1] * (b[2]*c[0] - b[0]*c[2])
                                 + a[2] * (b[0]*c[1] - b[1]*c[0]) for a, b, c in faces) / 6
                    self.assertAlmostEqual(volume, expected_volume)
                    self.assertEqual(len(faces), report["triangles"])
                    self.assertEqual(report["bounds_mm"]["max"][:2], [20, 10])

    def test_nominal_oracle_does_not_count_retraction_restoration_as_a_tower(self):
        paths, changes = fixture.linear_deposition_paths(
            "G90\nM83\nG92 E0\nG1 X0 Y0 Z0.2\nT0\n; FEATURE: Prime tower\n"
            "G1 E-1\nG1 X10 E1\nG1 X20 E1\n")
        self.assertEqual(len(changes), 1)
        self.assertEqual(len(paths), 1)
        self.assertEqual(paths[0]["from_mm"], [10, 0, 0.2])
        with self.assertRaises(AssertionError):
            fixture.tower_paths("T0\nT1\n; two loaded materials do not prove repeated paths")
