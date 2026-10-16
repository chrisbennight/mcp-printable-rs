"""Four-bit NormalFloat (NF4) weights, as bitsandbytes stores them.

A weight is flattened row by row and split into blocks of 64 values. Each
block keeps its largest magnitude as a 32-bit scale and every value as the
index of the nearest of 16 fixed levels between -1 and 1. Two indices share a
byte, the first in the high four bits. Rounding to the nearest level gives
the same weights bitsandbytes uses for a model loaded in NF4 without double
quantization, which is how Marigold V2 holds its base transformer.
"""

from __future__ import annotations

import torch

BLOCK = 64

# The levels bitsandbytes names "nf4".
LEVELS = (
    -1.0,
    -0.6961928009986877,
    -0.5250730514526367,
    -0.39491748809814453,
    -0.28444138169288635,
    -0.18477343022823334,
    -0.09105003625154495,
    0.0,
    0.07958029955625534,
    0.16093020141124725,
    0.24611230194568634,
    0.33791524171829224,
    0.44070982933044434,
    0.5626170039176941,
    0.7229568362236023,
    1.0,
)


def fits(tensor: torch.Tensor) -> bool:
    """Whether `tensor` splits into whole blocks."""
    return tensor.numel() > 0 and tensor.numel() % BLOCK == 0


def quantize(tensor: torch.Tensor) -> tuple[torch.Tensor, torch.Tensor]:
    """Packed indices (uint8, half as many as values) and per-block scales (float32)."""
    if not fits(tensor):
        raise ValueError(f"{tensor.numel()} values do not split into blocks of {BLOCK}")
    levels = torch.tensor(LEVELS, dtype=torch.float32, device=tensor.device)
    blocks = tensor.detach().reshape(-1, BLOCK).float()
    scales = blocks.abs().amax(dim=1)
    # An all-zero block keeps a zero scale; any index then decodes to zero.
    normalized = blocks / scales.clamp_min(torch.finfo(torch.float32).tiny).unsqueeze(1)
    # The levels are sorted, so the nearest is one of the two around each
    # value; comparing with the midpoints finds it without a 16-wide search.
    midpoints = (levels[1:] + levels[:-1]) / 2
    indices = torch.bucketize(normalized.reshape(-1), midpoints, out_int32=True).to(torch.uint8)
    packed = (indices[0::2] << 4) | indices[1::2]
    return packed, scales


def dequantize(
    packed: torch.Tensor, scales: torch.Tensor, shape: torch.Size | tuple[int, ...], dtype: torch.dtype
) -> torch.Tensor:
    """The weights `quantize` encoded, on the device of `packed`."""
    levels = torch.tensor(LEVELS, dtype=torch.float32, device=packed.device)
    indices = torch.stack((packed >> 4, packed & 0x0F), dim=1).reshape(-1).int()
    values = levels.index_select(0, indices).reshape(-1, BLOCK) * scales.to(packed.device, torch.float32).unsqueeze(1)
    return values.reshape(shape).to(dtype)
