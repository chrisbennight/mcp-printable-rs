"""Sparse voxel grids in plain PyTorch: lookup, convolution neighbours,
subdivision and the dual-grid surface.

A sparse grid is an N x 3 integer tensor of (x, y, z) voxel coordinates, each
in [0, size), with features in a matching N x C tensor. Coordinates are found
by sorting their linear keys and searching, so no hash table or compiled
extension is needed and memory stays proportional to the voxels present.
Work over many voxels is done in chunks whose temporary tensors stay near
CHUNK_BYTES.
"""

from __future__ import annotations

import torch

# The size the temporary tensors of one chunk of work aim for.
CHUNK_BYTES = 64 * 1024 * 1024

# The 27 offsets of a 3 x 3 x 3 neighbourhood, x slowest and z fastest. A
# convolution weight of shape (out, 3, 3, 3, in) holds the offset (i - 1,
# j - 1, k - 1) at [:, i, j, k, :].
OFFSETS = torch.stack(
    torch.meshgrid(torch.arange(-1, 2), torch.arange(-1, 2), torch.arange(-1, 2), indexing="ij"), dim=-1
).reshape(-1, 3)
# Child c of a subdivided voxel lies at (2x + (c & 1), 2y + (c >> 1 & 1), 2z + (c >> 2 & 1)).
CHILD_OFFSETS = torch.tensor([[child & 1, child >> 1 & 1, child >> 2 & 1] for child in range(8)])
# The four voxels sharing each axis's edge at a voxel's lower corner, in the
# order that walks around the edge.
EDGE_NEIGHBOURS = torch.tensor(
    [
        [[0, 0, 0], [0, 0, 1], [0, 1, 1], [0, 1, 0]],
        [[0, 0, 0], [1, 0, 0], [1, 0, 1], [0, 0, 1]],
        [[0, 0, 0], [0, 1, 0], [1, 1, 0], [1, 0, 0]],
    ]
)


class Lookup:
    """The row of each voxel of a sparse grid, found by its coordinates."""

    def __init__(self, coords: torch.Tensor, size: int):
        self.size = size
        self.keys, self.rows = torch.sort(self._key(coords))

    def _key(self, coords: torch.Tensor) -> torch.Tensor:
        coords = coords.long()
        return (coords[..., 0] * self.size + coords[..., 1]) * self.size + coords[..., 2]

    def find(self, coords: torch.Tensor) -> torch.Tensor:
        """The row holding each of `coords` (... x 3), or -1 where no voxel is."""
        inside = ((coords >= 0) & (coords < self.size)).all(dim=-1)
        keys = self._key(coords.clamp(0, self.size - 1))
        position = torch.searchsorted(self.keys, keys).clamp(max=self.keys.shape[0] - 1)
        found = inside & (self.keys[position] == keys)
        return torch.where(found, self.rows[position], torch.full_like(position, -1))


def chunk_rows(row_bytes: int) -> int:
    """How many rows of `row_bytes` each make one chunk of work."""
    return max(1, CHUNK_BYTES // max(1, row_bytes))


def neighbours(coords: torch.Tensor, size: int) -> torch.Tensor:
    """For each voxel, the row of each of its 27 neighbours (N x 27, int32), -1 where absent."""
    lookup = Lookup(coords, size)
    offsets = OFFSETS.to(coords.device)
    table = torch.empty(coords.shape[0], 27, dtype=torch.int32, device=coords.device)
    step = chunk_rows(27 * 3 * 8)
    for start in range(0, coords.shape[0], step):
        near = coords[start : start + step, None, :].long() + offsets[None]
        table[start : start + step] = lookup.find(near).int()
    return table


def submanifold_conv(
    feats: torch.Tensor,
    neighbour_rows: torch.Tensor,
    weight: torch.Tensor,
    bias: torch.Tensor | None,
    rows_per_chunk: int | None = None,
) -> torch.Tensor:
    """A 3 x 3 x 3 convolution evaluated only at the voxels present.

    `weight` is out x 3 x 3 x 3 x in. Each output sums, over the 27 offsets,
    the neighbour's features times that offset's weight. The voxels are taken
    in chunks so the gathered features stay small.
    """
    kernel = weight.reshape(weight.shape[0], -1).t()  # 27 * in x out
    out = torch.empty(feats.shape[0], weight.shape[0], dtype=feats.dtype, device=feats.device)
    step = rows_per_chunk or chunk_rows(27 * feats.shape[1] * feats.element_size())
    for start in range(0, feats.shape[0], step):
        rows = neighbour_rows[start : start + step].long()
        gathered = feats[rows.clamp(min=0)]  # n x 27 x in
        gathered[rows < 0] = 0
        chunk = gathered.reshape(rows.shape[0], -1) @ kernel
        out[start : start + step] = chunk if bias is None else chunk + bias
    return out


def subdivide(coords: torch.Tensor, children: torch.Tensor) -> tuple[torch.Tensor, torch.Tensor, torch.Tensor]:
    """Split each voxel into the children marked in `children` (N x 8 booleans).

    Returns the children's coordinates at twice the resolution, each child's
    parent row and each child's index among its parent's eight.
    """
    parent, child = children.nonzero(as_tuple=True)
    fine = coords[parent].long() * 2 + CHILD_OFFSETS.to(coords.device)[child]
    return fine.to(coords.dtype), parent, child


def dual_grid_mesh(
    coords: torch.Tensor,
    vertices: torch.Tensor,
    intersected: torch.Tensor,
    split: torch.Tensor,
    size: int,
) -> tuple[torch.Tensor, torch.Tensor]:
    """The surface of a flexible dual grid as vertices and triangles.

    Each voxel holds one surface vertex at `coords + vertices` (offsets in
    voxel units), whether the surface crosses the edge along each axis at its
    lower corner (`intersected`, N x 3), and a weight choosing how quads
    split into triangles (`split`, N x 1). Each crossed edge whose four
    surrounding voxels are present gives one quad of their vertices. The
    result lies in the cube from -0.5 to 0.5.
    """
    lookup = Lookup(coords, size)
    voxel, axis = intersected.nonzero(as_tuple=True)
    around = EDGE_NEIGHBOURS.to(coords.device)
    quads = []
    step = chunk_rows(4 * 3 * 8)
    for start in range(0, voxel.shape[0], step):
        edges = coords[voxel[start : start + step], None, :].long() + around[axis[start : start + step]]
        corners = lookup.find(edges)  # n x 4
        quads.append(corners[(corners >= 0).all(dim=1)])
    quads = torch.cat(quads) if quads else torch.empty(0, 4, dtype=torch.long, device=coords.device)
    points = (coords.float() + vertices) * (1.0 / size) - 0.5
    weights = split[:, 0][quads]
    first = torch.stack([quads[:, 0], quads[:, 1], quads[:, 2], quads[:, 0], quads[:, 2], quads[:, 3]], dim=1)
    second = torch.stack([quads[:, 0], quads[:, 1], quads[:, 3], quads[:, 3], quads[:, 1], quads[:, 2]], dim=1)
    across = (weights[:, 0] * weights[:, 2] > weights[:, 1] * weights[:, 3])[:, None]
    triangles = torch.where(across, first, second).reshape(-1, 3)
    return points, triangles
