"""Prepare large generated surfaces with file-backed arrays and streamed STL output.

Only preparation copies are coarsened. The neural output remains unchanged.
"""
from __future__ import annotations

from pathlib import Path
import struct

import numpy as np

from .mesh_coarsening import cluster, release_pages


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
        release_pages(vertices)
        release_pages(faces)
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
            release_pages(vertices)
            release_pages(faces)
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
