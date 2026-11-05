"""Prepare large generated surfaces with file-backed arrays and streamed STL output.

Only preparation copies are coarsened. The neural output remains unchanged.
"""
from __future__ import annotations

from pathlib import Path
import struct

import numpy as np

ROW = np.dtype([(name, '<i8') for name in ['a', 'b', 'c', 'x', 'y', 'z', 'ordinal']])

def release_pages(array):
    import mmap
    mapping = getattr(array, '_mmap', None)
    if mapping is not None:
        array.flush()
        mapping.madvise(mmap.MADV_DONTNEED)

def cluster(vertices, faces, cell, output, chunk=100000):
    if not np.isfinite(cell) or cell <= 0 or chunk < 1:
        raise ValueError('Clustering needs a positive cell size and chunk size')
    output.mkdir(exist_ok=False)
    if vertices.ndim != 2 or vertices.shape[1] != 3 or faces.ndim != 2 or (faces.shape[1] != 3):
        raise ValueError('Expected three coordinates per vertex and three indices per face')
    if vertices.dtype.kind != 'f' or faces.dtype.kind not in 'iu':
        raise ValueError('Expected floating-point coordinates and integer indices')
    minimum = np.full(3, np.inf)
    maximum = np.full(3, -np.inf)
    for start in range(0, len(vertices), chunk):
        values = vertices[start:start + chunk]
        if not np.isfinite(values).all():
            raise ValueError('Non-finite mesh coordinates')
        minimum = np.minimum(minimum, values.min(axis=0))
        maximum = np.maximum(maximum, values.max(axis=0))
    origin = (np.floor(minimum / cell) - 1) * cell
    sides = np.ceil((maximum - origin) / cell).astype(np.int64) + 2
    if int(sides[0]) * int(sides[1]) * int(sides[2]) >= np.iinfo(np.int64).max:
        raise ValueError('Clustering grid exceeds the integer key range')
    keys = np.lib.format.open_memmap(output / 'keys.npy', mode='w+', dtype=np.int64, shape=(len(vertices),))
    for start in range(0, len(vertices), chunk):
        grid = np.floor((vertices[start:start + chunk] - origin) / cell).astype(np.int64)
        if np.any(grid < 0) or np.any(grid >= sides):
            raise ValueError('Vertex lies outside the clustering key grid')
        keys[start:start + chunk] = (grid[:, 0] * sides[1] + grid[:, 1]) * sides[2] + grid[:, 2]
    keys.flush()
    unique = np.unique(keys)
    release_pages(keys)
    release_pages(vertices)
    owners = np.lib.format.open_memmap(output / 'owners.npy', mode='w+', dtype=np.int64, shape=(len(vertices),))
    sums = np.zeros((len(unique), 3), dtype=np.float64)
    counts = np.zeros(len(unique), dtype=np.int64)
    for start in range(0, len(vertices), chunk):
        ids = np.searchsorted(unique, keys[start:start + chunk])
        owners[start:start + chunk] = ids
        np.add.at(sums, ids, vertices[start:start + chunk])
        np.add.at(counts, ids, 1)
    owners.flush()
    del unique
    points = np.lib.format.open_memmap(output / 'vertices.npy', mode='w+', dtype=vertices.dtype, shape=sums.shape)
    for start in range(0, len(sums), chunk):
        points[start:start + chunk] = sums[start:start + chunk] / counts[start:start + chunk, None]
    points.flush()
    del sums, counts
    release_pages(keys)
    release_pages(vertices)
    release_pages(owners)
    records_path = output / 'face-records.bin'
    total = 0
    with records_path.open('xb') as stream:
        for start in range(0, len(faces), chunk):
            source = faces[start:start + chunk]
            if source.size and (source.min() < 0 or source.max() >= len(vertices)):
                raise ValueError('Face index outside the vertex array')
            corners = owners[source]
            keep = (corners[:, 0] != corners[:, 1]) & (corners[:, 1] != corners[:, 2]) & (corners[:, 0] != corners[:, 2])
            corners = corners[keep]
            canonical = np.sort(corners, axis=1)
            records = np.empty(len(corners), dtype=ROW)
            for axis, name in enumerate(['a', 'b', 'c']):
                records[name] = canonical[:, axis]
            for axis, name in enumerate(['x', 'y', 'z']):
                records[name] = corners[:, axis]
            records['ordinal'] = np.arange(start, start + len(source), dtype=np.int64)[keep]
            records.tofile(stream)
            total += len(records)
    release_pages(faces)
    release_pages(owners)
    if total == 0:
        raise ValueError('Clustering removed every face')
    records = np.memmap(records_path, mode='r+', dtype=ROW, shape=(total,))
    records.sort(order=['a', 'b', 'c', 'ordinal'], kind='quicksort')
    records.flush()
    selected_path = output / 'selected.bin'
    previous = None
    kept = 0
    with selected_path.open('xb') as stream:
        for start in range(0, total, chunk):
            part = records[start:start + chunk]
            canonical = np.stack([part[name] for name in ['a', 'b', 'c']], axis=1)
            keep = np.ones(len(part), dtype=bool)
            if previous is not None:
                keep[0] = np.any(canonical[0] != previous)
            keep[1:] = np.any(canonical[1:] != canonical[:-1], axis=1)
            part[keep].tofile(stream)
            kept += int(keep.sum())
            previous = canonical[-1].copy()
    del records
    selected = np.memmap(selected_path, mode='r+', dtype=ROW, shape=(kept,))
    selected.sort(order='ordinal', kind='quicksort')
    selected.flush()
    result_faces = np.lib.format.open_memmap(output / 'faces.npy', mode='w+', dtype=np.int64, shape=(kept, 3))
    for start in range(0, kept, chunk):
        part = selected[start:start + chunk]
        result_faces[start:start + chunk] = np.stack([part[name] for name in ['x', 'y', 'z']], axis=1)
    result_faces.flush()
    release_pages(result_faces)
    release_pages(points)
    return (points, result_faces)


STL_RECORD = np.dtype([("normal", "<f4", 3), ("corners", "<f4", (3, 3)), ("attribute", "<u2")])


def _corners(vertices, faces):
    if faces.size and (faces.min() < 0 or faces.max() >= len(vertices)):
        raise ValueError("Face index outside the vertex array")
    points = vertices[faces].astype(np.float64)
    if not np.isfinite(points).all():
        raise ValueError("Non-finite mesh coordinates")
    return np.stack([points[:, :, 0], -points[:, :, 2], points[:, :, 1]], axis=2)


def write_stl(path: Path, vertices, faces, size_mm: float, *, chunk: int = 100_000) -> dict:
    """Stream a Z-up, grounded, centred STL in millimetres from native Y-up geometry."""
    if vertices.ndim != 2 or vertices.shape[1] != 3 or faces.ndim != 2 or faces.shape[1] != 3:
        raise ValueError("Expected three coordinates per vertex and three indices per face")
    if vertices.dtype.kind != "f" or faces.dtype.kind not in "iu":
        raise ValueError("Expected floating-point coordinates and integer indices")
    if not len(faces) or not len(vertices):
        raise ValueError("Empty generated surface")
    if len(faces) > 0xffffffff:
        raise ValueError("The triangle count exceeds the binary STL format")
    if chunk < 1 or not np.isfinite(size_mm) or size_mm <= 0:
        raise ValueError("STL export needs a positive size and chunk size")
    low, high = np.full(3, np.inf), np.full(3, -np.inf)
    for start in range(0, len(faces), chunk):
        points = _corners(vertices, faces[start:start + chunk])
        low = np.minimum(low, points.min(axis=(0, 1)))
        high = np.maximum(high, points.max(axis=(0, 1)))
    longest = float((high - low).max())
    if longest <= 0:
        raise ValueError("The generated surface has no physical extent")
    scale = size_mm / longest
    offset = np.array([(low[0] + high[0]) / 2, (low[1] + high[1]) / 2, low[2]])
    with path.open("xb") as stream:
        stream.write(b"Printable TRELLIS.2 generated surface; unrepaired".ljust(80, b" "))
        stream.write(struct.pack("<I", len(faces)))
        for start in range(0, len(faces), chunk):
            corners = ((_corners(vertices, faces[start:start + chunk]) - offset) * scale).astype(np.float32)
            normals = np.cross(corners[:, 1] - corners[:, 0], corners[:, 2] - corners[:, 0])
            lengths = np.linalg.norm(normals, axis=1, keepdims=True)
            records = np.zeros(len(corners), dtype=STL_RECORD)
            records["normal"] = normals / np.maximum(lengths, 1e-30)
            records["corners"] = corners
            records.tofile(stream)
    return {"triangles": len(faces), "size_mm": [round(float(side), 3) for side in (high - low) * scale]}


def deliver(
    raw: Path,
    destination: Path,
    scratch: Path,
    *,
    resolution: int,
    size_mm: float,
    target_triangles: int | None,
    simplifier_faces: int = 4_000_000,
) -> dict:
    """Deliver all generated faces, or simplify a separate preparation copy.

    Coarsening bounds the final in-memory simplifier's input. It does not impose
    an acceptance ceiling on the original generated geometry. Full-detail export
    bypasses coarsening and simplification and writes every original triangle.
    """
    vertices = np.load(raw / "vertices.npy", mmap_mode="r", allow_pickle=False)
    faces = np.load(raw / "faces.npy", mmap_mode="r", allow_pickle=False)
    generated = len(faces)
    passes = []
    if target_triangles is not None:
        if resolution < 1 or target_triangles < 1 or simplifier_faces < 1:
            raise ValueError("Mesh preparation needs positive resolution and triangle budgets")
        voxels = max(1, int(np.ceil(np.sqrt(generated / simplifier_faces))))
        while len(faces) > simplifier_faces:
            prepared = scratch / f"grid-{voxels}"
            points, triangles = cluster(vertices, faces, voxels / resolution, prepared)
            passes.append({"voxels": voxels, "triangles": len(triangles)})
            if len(triangles) <= simplifier_faces:
                vertices, faces = points, triangles
                break
            del points, triangles
            voxels *= 2
        used, corners = np.unique(faces, return_inverse=True)
        vertices = vertices[used].astype(np.float64)
        faces = corners.reshape(-1, 3).astype(np.int64)
        if len(faces) > target_triangles:
            import fast_simplification
            vertices, faces = fast_simplification.simplify(vertices, faces, target_count=target_triangles)
    result = write_stl(destination, vertices, faces, size_mm)
    result.update(generated_triangles=generated, simplified=target_triangles is not None, preparation=passes)
    return result
