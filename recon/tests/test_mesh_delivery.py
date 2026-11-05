"""File-backed delivery preserves source geometry and the public coordinate contract."""

import hashlib
from pathlib import Path
import struct
import tempfile
import unittest

import numpy as np

from printable_recon.mesh_delivery import STL_RECORD, cluster, deliver, write_stl


class DeliveryTests(unittest.TestCase):
    def test_raw_export_is_grounded_centred_and_in_millimetres(self):
        vertices = np.array([[0, 0, 0], [1, 0, 0], [0, 1, 0]], dtype=np.float32)
        faces = np.array([[0, 1, 2]], dtype=np.int32)
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "model.stl"
            result = write_stl(path, vertices, faces, 100, chunk=1)
            data = path.read_bytes()
            self.assertEqual(result, {"triangles": 1, "size_mm": [100, 0, 100]})
            self.assertEqual(len(data), 134)
            self.assertEqual(struct.unpack("<I", data[80:84]), (1,))
            facet = np.frombuffer(data[84:], dtype=STL_RECORD)
            np.testing.assert_array_equal(facet["corners"][0], [[-50, 0, 0], [50, 0, 0], [-50, 0, 100]])
            np.testing.assert_array_equal(facet["normal"][0], [0, -1, 0])

    def test_duplicate_faces_across_chunks_preserve_first_winding(self):
        vertices = np.array([[0, 0, 0], [.1, 0, 0], [1, 0, 0], [0, 1, 0], [1.1, 0, 0]], dtype=np.float32)
        faces = np.array([[0, 2, 3], [1, 4, 3], [2, 0, 3], [0, 1, 2], [0, 3, 2]], dtype=np.int32)
        with tempfile.TemporaryDirectory() as directory:
            points, triangles = cluster(vertices, faces, .5, Path(directory) / "prepared", chunk=2)
            np.testing.assert_allclose(points, [[.05, 0, 0], [0, 1, 0], [1.05, 0, 0]], atol=1e-6)
            np.testing.assert_array_equal(triangles, [[0, 2, 1]])

    def test_invalid_geometry_is_rejected_before_opening_the_stl(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "model.stl"
            with self.assertRaisesRegex(ValueError, "Face index"):
                write_stl(path, np.eye(3, dtype=np.float32), np.array([[0, 1, 4]]), 100)
            self.assertFalse(path.exists())

    def test_full_delivery_preserves_original_mapped_arrays_and_every_face(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            np.save(root / "vertices.npy", np.eye(3, dtype=np.float32))
            np.save(root / "faces.npy", np.array([[0, 1, 2], [2, 1, 0]], dtype=np.int32))
            paths = [root / "vertices.npy", root / "faces.npy"]
            before = [hashlib.sha256(path.read_bytes()).hexdigest() for path in paths]
            result = deliver(root, root / "model.stl", root, resolution=512, size_mm=100, target_triangles=None)
            self.assertEqual(result["triangles"], 2)
            self.assertEqual(result["generated_triangles"], 2)
            self.assertFalse(result["simplified"])
            self.assertEqual((root / "model.stl").stat().st_size, 184)
            self.assertEqual(before, [hashlib.sha256(path.read_bytes()).hexdigest() for path in paths])

    def test_chunk_size_does_not_change_preparation_or_stl_bytes(self):
        rng = np.random.default_rng(12)
        vertices = rng.uniform(-.5, .5, size=(40, 3)).astype(np.float32)
        faces = np.array([rng.choice(40, 3, replace=False) for _ in range(120)], dtype=np.int32)
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            small_v, small_f = cluster(vertices, faces, .2, root / "small", chunk=1)
            large_v, large_f = cluster(vertices, faces, .2, root / "large", chunk=17)
            np.testing.assert_array_equal(small_v, large_v)
            np.testing.assert_array_equal(small_f, large_f)
            write_stl(root / "small.stl", small_v, small_f, 100, chunk=1)
            write_stl(root / "large.stl", large_v, large_f, 100, chunk=17)
            self.assertEqual((root / "small.stl").read_bytes(), (root / "large.stl").read_bytes())


if __name__ == "__main__":
    unittest.main()
