"""Coarsen a preparation copy with bounded sorted runs on disk."""

import heapq
import mmap
from pathlib import Path

import numpy as np

VERTEX = np.dtype([("key", "<i8"), ("ordinal", "<i8")])
FACE = np.dtype([(name, "<i8") for name in ("a", "b", "c", "x", "y", "z", "ordinal")])


def release_pages(array):
    mapping = getattr(array, "_mmap", None)
    if mapping is not None:
        array.flush()
        mapping.madvise(mmap.MADV_DONTNEED)


def _blocks(path, dtype, chunk):
    with path.open("rb") as source:
        while len(rows := np.fromfile(source, dtype=dtype, count=chunk)):
            yield rows


def _rows(path, dtype, chunk):
    for block in _blocks(path, dtype, chunk):
        yield from block


def _sort(source: Path, dtype, order, directory: Path, chunk: int, fan_in: int = 8) -> Path:
    """Sort bounded runs, then merge at most fan_in buffered readers at once."""
    directory.mkdir()
    runs = []
    for number, rows in enumerate(_blocks(source, dtype, chunk)):
        rows.sort(order=order, kind="quicksort")
        path = directory / f"run-{number}.bin"
        with path.open("xb") as output:
            rows.tofile(output)
        runs.append(path)
    if not runs:
        raise ValueError("Empty mesh preparation records")
    generation = 0
    while len(runs) > 1:
        merged = []
        for start in range(0, len(runs), fan_in):
            group = runs[start:start + fan_in]
            destination = directory / f"merge-{generation}-{start}.bin"
            readers = [iter(_rows(path, dtype, chunk)) for path in group]
            heap = []

            def push(index):
                row = next(readers[index], None)
                if row is not None:
                    heapq.heappush(heap, (tuple(int(row[name]) for name in order), index, row))

            for index in range(len(readers)):
                push(index)
            buffer = np.empty(chunk, dtype=dtype)
            used = 0
            with destination.open("xb") as output:
                while heap:
                    _, index, row = heapq.heappop(heap)
                    buffer[used] = row
                    used += 1
                    if used == chunk:
                        buffer.tofile(output)
                        used = 0
                    push(index)
                buffer[:used].tofile(output)
            for path in group:
                path.unlink()
            merged.append(destination)
        runs = merged
        generation += 1
    return runs[0]


def cluster(vertices, faces, cell, output, chunk=100_000):
    if not np.isfinite(cell) or cell <= 0 or chunk < 1:
        raise ValueError("Clustering needs a positive cell size and chunk size")
    if vertices.ndim != 2 or vertices.shape[1] != 3 or faces.ndim != 2 or faces.shape[1] != 3:
        raise ValueError("Expected three coordinates per vertex and three indices per face")
    if vertices.dtype.kind != "f" or faces.dtype.kind not in "iu":
        raise ValueError("Expected floating-point coordinates and integer indices")
    if not len(vertices) or not len(faces):
        raise ValueError("Empty generated surface")
    output.mkdir(exist_ok=False)
    minimum, maximum = np.full(3, np.inf), np.full(3, -np.inf)
    for start in range(0, len(vertices), chunk):
        values = vertices[start:start + chunk]
        if not np.isfinite(values).all():
            raise ValueError("Non-finite mesh coordinates")
        minimum = np.minimum(minimum, values.min(axis=0))
        maximum = np.maximum(maximum, values.max(axis=0))
        release_pages(vertices)
    origin = (np.floor(minimum / cell) - 1) * cell
    sides = np.ceil((maximum - origin) / cell).astype(np.int64) + 2
    if int(sides[0]) * int(sides[1]) * int(sides[2]) >= np.iinfo(np.int64).max:
        raise ValueError("Clustering grid exceeds the integer key range")
    source = output / "vertex-records.bin"
    with source.open("xb") as stream:
        for start in range(0, len(vertices), chunk):
            grid = np.floor((vertices[start:start + chunk] - origin) / cell).astype(np.int64)
            if np.any(grid < 0) or np.any(grid >= sides):
                raise ValueError("Vertex lies outside the clustering key grid")
            rows = np.empty(len(grid), dtype=VERTEX)
            rows["key"] = (grid[:, 0] * sides[1] + grid[:, 1]) * sides[2] + grid[:, 2]
            rows["ordinal"] = np.arange(start, start + len(grid))
            rows.tofile(stream)
            release_pages(vertices)
    ordered = _sort(source, VERTEX, ["key", "ordinal"], output / "vertex-sort", chunk)
    owners = np.lib.format.open_memmap(output / "owners.npy", mode="w+", dtype=np.int64, shape=(len(vertices),))
    points_path = output / "points.bin"
    point_count, previous, count = 0, None, 0
    total = np.zeros(3, dtype=np.float64)
    buffer = np.empty((chunk, 3), dtype=vertices.dtype)
    used = 0
    with points_path.open("xb") as stream:
        for rows in _blocks(ordered, VERTEX, chunk):
            coordinates = vertices[rows["ordinal"]]
            identifiers = np.empty(len(rows), dtype=np.int64)
            for index, row in enumerate(rows):
                key = int(row["key"])
                if previous is not None and key != previous:
                    buffer[used] = total / count
                    used += 1
                    if used == chunk:
                        buffer.tofile(stream)
                        used = 0
                    point_count += 1
                    total.fill(0)
                    count = 0
                previous = key
                total += coordinates[index]
                count += 1
                identifiers[index] = point_count
            owners[rows["ordinal"]] = identifiers
            release_pages(vertices)
            release_pages(owners)
        buffer[used] = total / count
        buffer[:used + 1].tofile(stream)
        point_count += 1
    points = np.lib.format.open_memmap(output / "vertices.npy", mode="w+", dtype=vertices.dtype, shape=(point_count, 3))
    cursor = 0
    for values in _blocks(points_path, vertices.dtype, chunk * 3):
        values = values.reshape(-1, 3)
        points[cursor:cursor + len(values)] = values
        cursor += len(values)
        release_pages(points)
    records_path = output / "face-records.bin"
    with records_path.open("xb") as stream:
        for start in range(0, len(faces), chunk):
            source_faces = faces[start:start + chunk]
            if source_faces.size and (source_faces.min() < 0 or source_faces.max() >= len(vertices)):
                raise ValueError("Face index outside the vertex array")
            corners = owners[source_faces]
            keep = (corners[:, 0] != corners[:, 1]) & (corners[:, 1] != corners[:, 2]) & (corners[:, 0] != corners[:, 2])
            corners = corners[keep]
            canonical = np.sort(corners, axis=1)
            rows = np.empty(len(corners), dtype=FACE)
            for axis, name in enumerate(("a", "b", "c")):
                rows[name] = canonical[:, axis]
            for axis, name in enumerate(("x", "y", "z")):
                rows[name] = corners[:, axis]
            rows["ordinal"] = np.arange(start, start + len(source_faces))[keep]
            rows.tofile(stream)
            release_pages(faces)
            release_pages(owners)
    if records_path.stat().st_size == 0:
        raise ValueError("Clustering removed every face")
    ordered = _sort(records_path, FACE, ["a", "b", "c", "ordinal"], output / "face-sort", chunk)
    selected, previous, kept = output / "selected.bin", None, 0
    with selected.open("xb") as stream:
        for rows in _blocks(ordered, FACE, chunk):
            canonical = np.stack([rows[name] for name in ("a", "b", "c")], axis=1)
            keep = np.ones(len(rows), dtype=bool)
            if previous is not None:
                keep[0] = np.any(canonical[0] != previous)
            keep[1:] = np.any(canonical[1:] != canonical[:-1], axis=1)
            rows[keep].tofile(stream)
            kept += int(keep.sum())
            previous = canonical[-1].copy()
    ordered = _sort(selected, FACE, ["ordinal"], output / "order-sort", chunk)
    result = np.lib.format.open_memmap(output / "faces.npy", mode="w+", dtype=np.int64, shape=(kept, 3))
    cursor = 0
    for rows in _blocks(ordered, FACE, chunk):
        result[cursor:cursor + len(rows)] = np.stack([rows[name] for name in ("x", "y", "z")], axis=1)
        cursor += len(rows)
        release_pages(result)
    return points, result
