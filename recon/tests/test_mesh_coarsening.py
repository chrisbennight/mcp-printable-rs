"""External grouping preserves the original coarsener's deterministic geometry."""

from pathlib import Path
import tempfile
import unittest

import numpy as np

from printable_recon.mesh_coarsening import FACE, _blocks, _sort, cluster


class CoarseningTests(unittest.TestCase):
    def test_multiple_merge_levels_preserve_sort_keys_and_rows(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            rng = np.random.default_rng(42)
            records = np.zeros(150, dtype=FACE)
            for name in ("a", "b", "c", "x", "y", "z"):
                records[name] = rng.integers(0, 8, len(records))
            records["ordinal"] = np.arange(len(records))
            source = root / "source.bin"
            records.tofile(source)
            output = _sort(source, FACE, ["a", "b", "c", "ordinal"], root / "runs", chunk=2)
            actual = np.concatenate(list(_blocks(output, FACE, 7)))
            expected = np.sort(records, order=["a", "b", "c", "ordinal"])
            np.testing.assert_array_equal(actual, expected)

    def test_group_centres_and_face_order_match_an_independent_small_reference(self):
        rng = np.random.default_rng(42)
        vertices = rng.random((250, 3), dtype=np.float32)
        faces = rng.integers(0, len(vertices), (450, 3), dtype=np.int64)
        faces[1] = faces[0, ::-1]
        cell = 0.2
        origin = (np.floor(vertices.min(0) / cell) - 1) * cell
        grid = np.floor((vertices - origin) / cell).astype(np.int64)
        sides = np.ceil((vertices.max(0) - origin) / cell).astype(np.int64) + 2
        keys = (grid[:, 0] * sides[1] + grid[:, 1]) * sides[2] + grid[:, 2]
        unique, owners = np.unique(keys, return_inverse=True)
        sums = np.zeros((len(unique), 3), dtype=np.float64)
        counts = np.zeros(len(unique), dtype=np.int64)
        np.add.at(sums, owners, vertices)
        np.add.at(counts, owners, 1)
        expected_vertices = (sums / counts[:, None]).astype(vertices.dtype)
        seen, expected_faces = set(), []
        for face in owners[faces]:
            key = tuple(sorted(int(value) for value in face))
            if len(set(key)) == 3 and key not in seen:
                expected_faces.append(face)
                seen.add(key)
        with tempfile.TemporaryDirectory() as directory:
            for chunk in (3, 17, 100_000):
                points, triangles = cluster(vertices, faces, cell, Path(directory) / str(chunk), chunk=chunk)
                np.testing.assert_array_equal(points, expected_vertices)
                np.testing.assert_array_equal(triangles, expected_faces)


if __name__ == "__main__":
    unittest.main()
