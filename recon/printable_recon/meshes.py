"""Bring a generated surface down to a triangle budget.

The generator's surface follows its voxels, so a detailed subject at a high
resolution has tens of millions of triangles. Quadric edge collapse keeps
the shape best with fewer triangles but needs a few hundred bytes of memory
per triangle, so a surface larger than CLUSTER_ABOVE triangles is first
coarsened cheaply by merging the vertices that share a cell of a grid a few
voxels across.
"""

from __future__ import annotations

import math

import numpy
import torch

# Surfaces with more triangles than this are coarsened on a grid before
# quadric edge collapse, which then needs about 1.5 GB of memory at most.
CLUSTER_ABOVE = 4_000_000


def cluster(vertices: torch.Tensor, triangles: torch.Tensor, cell: float) -> tuple[torch.Tensor, torch.Tensor]:
    """Merge the vertices in each cell of a grid `cell` wide into their mean.

    Triangles whose corners merge are dropped, as are repeats of a triangle
    already kept. Vertices lie in the cube from -0.5 to 0.5.
    """
    cells = torch.floor((vertices + 0.5) / cell).long()
    across = int(math.ceil(1.0 / cell)) + 2
    keys = (cells[:, 0] * across + cells[:, 1]) * across + cells[:, 2]
    _, owner = torch.unique(keys, return_inverse=True)
    del cells, keys
    count = int(owner.max()) + 1 if owner.numel() else 0
    sums = torch.zeros(count, 3, dtype=torch.float64, device=vertices.device)
    sums.index_add_(0, owner, vertices.double())
    weights = torch.bincount(owner, minlength=count).double()
    merged = (sums / weights.clamp(min=1)[:, None]).to(vertices.dtype)
    corners = owner[triangles]
    del owner, sums, weights
    distinct = (corners[:, 0] != corners[:, 1]) & (corners[:, 1] != corners[:, 2]) & (corners[:, 0] != corners[:, 2])
    corners = corners[distinct]
    _, group = torch.unique(torch.sort(corners, dim=1).values, dim=0, return_inverse=True)
    first = torch.full((int(group.max()) + 1 if group.numel() else 0,), corners.shape[0], device=corners.device)
    first.scatter_reduce_(0, group, torch.arange(corners.shape[0], device=corners.device), reduce="amin")
    return merged, corners[torch.sort(first).values]


def reduce(
    vertices: torch.Tensor, triangles: torch.Tensor, size: int, target: int
) -> tuple[numpy.ndarray, numpy.ndarray]:
    """The surface with about `target` triangles, or as few as it can have.

    `size` is the voxel grid's resolution, which sets the coarsening grid.
    Quadric edge collapse stops early on parts it cannot simplify without
    folding the surface, so the result may keep more than `target`.
    """
    import fast_simplification

    if triangles.shape[0] > max(CLUSTER_ABOVE, target):
        # Triangles fall roughly with the square of the cell width.
        voxels = math.ceil(math.sqrt(triangles.shape[0] / CLUSTER_ABOVE))
        vertices, triangles = cluster(vertices, triangles, voxels / size)
    points = vertices.double().cpu().numpy()
    faces = triangles.cpu().numpy().astype(numpy.int64)
    del vertices, triangles
    if faces.shape[0] > target:
        points, faces = fast_simplification.simplify(points, faces, target_count=target)
    return points.astype(numpy.float32), faces
