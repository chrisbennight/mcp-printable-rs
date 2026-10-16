"""The Pixal3D networks that turn image features into a shape.

Pixal3D (MIT) builds on the TRELLIS.2 backbone. Generation runs in stages,
each with its own network:

- a flow transformer samples a 16^3 grid of structure latents, which the
  structure decoder turns into the occupied voxels of a 32^3 grid;
- a flow transformer samples a shape latent on those voxels;
- for detail beyond 512, the shape decoder predicts which voxels subdivide,
  and a second flow transformer samples the shape latent on the finer voxels;
- the shape decoder subdivides the voxels to the output resolution and
  predicts a flexible dual grid, whose surface is the mesh.

These are inference-only definitions of those networks, with parameter names
matching the published checkpoints. A sparse latent is a list of voxels, so
the flow transformer treats both kinds of latent as a sequence of tokens with
integer grid positions, and the shape decoder's convolutions run on the
voxels present, in plain PyTorch.
"""

from __future__ import annotations

import json
import struct
from dataclasses import dataclass
from pathlib import Path

import numpy
import torch
import torch.nn.functional as F
from torch import nn

from . import voxels


class ModelError(Exception):
    """A model file is missing, unreadable or does not match its network."""


class LayerNorm32(nn.LayerNorm):
    """Layer normalization computed in float32 whatever the input precision."""

    def forward(self, x: torch.Tensor) -> torch.Tensor:
        return super().forward(x.float()).to(x.dtype)


class ChannelLayerNorm32(LayerNorm32):
    """Layer normalization over the channels of a B x C x ... tensor."""

    def forward(self, x: torch.Tensor) -> torch.Tensor:
        dims = x.dim()
        x = x.permute(0, *range(2, dims), 1).contiguous()
        x = super().forward(x)
        return x.permute(0, dims - 1, *range(1, dims - 1)).contiguous()


class MultiHeadRMSNorm(nn.Module):
    def __init__(self, dim: int, heads: int):
        super().__init__()
        self.scale = dim**0.5
        self.gamma = nn.Parameter(torch.ones(heads, dim))

    def forward(self, x: torch.Tensor) -> torch.Tensor:
        return (F.normalize(x.float(), dim=-1) * self.gamma * self.scale).to(x.dtype)


def rope_phases(coords: torch.Tensor, head_dim: int) -> torch.Tensor:
    """Rotary phases for integer grid positions (N x 3), as N x head_dim/2 unit complex numbers.

    Each axis takes an equal share of the frequencies; any left over are
    padded with zero phase.
    """
    per_axis = head_dim // 2 // 3
    freqs = torch.arange(per_axis, dtype=torch.float32, device=coords.device) / per_axis
    freqs = 1.0 / (10000.0**freqs)
    angles = torch.outer(coords.reshape(-1), freqs)
    phases = torch.polar(torch.ones_like(angles), angles).reshape(coords.shape[0], -1)
    padding = head_dim // 2 - phases.shape[-1]
    if padding:
        ones = torch.ones(coords.shape[0], padding, device=coords.device)
        phases = torch.cat([phases, torch.polar(ones, torch.zeros_like(ones))], dim=-1)
    return phases


def rotate(x: torch.Tensor, phases: torch.Tensor) -> torch.Tensor:
    """Apply rotary phases (N x D/2) to N x heads x D queries or keys."""
    pairs = torch.view_as_complex(x.float().reshape(*x.shape[:-1], -1, 2))
    return torch.view_as_real(pairs * phases.unsqueeze(-2)).reshape(x.shape).to(x.dtype)


def attention(q: torch.Tensor, k: torch.Tensor, v: torch.Tensor) -> torch.Tensor:
    """Attention over L x heads x D sequences, returning L x heads x D."""
    out = F.scaled_dot_product_attention(
        q.unsqueeze(0).permute(0, 2, 1, 3), k.unsqueeze(0).permute(0, 2, 1, 3), v.unsqueeze(0).permute(0, 2, 1, 3)
    )
    return out.permute(0, 2, 1, 3)[0]


class SelfAttention(nn.Module):
    def __init__(self, channels: int, heads: int):
        super().__init__()
        self.heads = heads
        self.to_qkv = nn.Linear(channels, channels * 3)
        self.q_rms_norm = MultiHeadRMSNorm(channels // heads, heads)
        self.k_rms_norm = MultiHeadRMSNorm(channels // heads, heads)
        self.to_out = nn.Linear(channels, channels)

    def forward(self, x: torch.Tensor, phases: torch.Tensor) -> torch.Tensor:
        q, k, v = self.to_qkv(x).reshape(x.shape[0], 3, self.heads, -1).unbind(dim=1)
        q = rotate(self.q_rms_norm(q), phases)
        k = rotate(self.k_rms_norm(k), phases)
        return self.to_out(attention(q, k, v).reshape(x.shape[0], -1))


class CrossAttention(nn.Module):
    def __init__(self, channels: int, context_channels: int, heads: int):
        super().__init__()
        self.heads = heads
        self.to_q = nn.Linear(channels, channels)
        self.to_kv = nn.Linear(context_channels, channels * 2)
        self.q_rms_norm = MultiHeadRMSNorm(channels // heads, heads)
        self.k_rms_norm = MultiHeadRMSNorm(channels // heads, heads)
        self.to_out = nn.Linear(channels, channels)

    def forward(self, x: torch.Tensor, context: torch.Tensor) -> torch.Tensor:
        q = self.q_rms_norm(self.to_q(x).reshape(x.shape[0], self.heads, -1))
        k, v = self.to_kv(context).reshape(context.shape[0], 2, self.heads, -1).unbind(dim=1)
        return self.to_out(attention(q, self.k_rms_norm(k), v).reshape(x.shape[0], -1))


class ProjectedAttention(nn.Module):
    """Attention to the image's global tokens plus each token's own projected image features."""

    def __init__(self, channels: int, context_channels: int, projected_channels: int, heads: int):
        super().__init__()
        self.cross_attn_block = CrossAttention(channels, context_channels, heads)
        self.proj_linear = nn.Linear(projected_channels, channels)

    def forward(self, x: torch.Tensor, context: torch.Tensor, projected: torch.Tensor) -> torch.Tensor:
        return self.proj_linear(projected) + self.cross_attn_block(x, context)


def in_rows(fn, x: torch.Tensor, width: int) -> torch.Tensor:
    """A row-wise `fn` applied to the rows of `x` a chunk at a time, so its
    widest intermediate, `width` columns, stays near the voxel module's chunk
    size."""
    step = voxels.chunk_rows(width * x.element_size())
    first = fn(x[:step])
    if x.shape[0] <= step:
        return first
    out = first.new_empty(x.shape[0], *first.shape[1:])
    out[:step] = first
    for start in range(step, x.shape[0], step):
        out[start : start + step] = fn(x[start : start + step])
    return out


class FeedForward(nn.Module):
    def __init__(self, channels: int, ratio: float):
        super().__init__()
        self.hidden = int(channels * ratio)
        self.mlp = nn.Sequential(
            nn.Linear(channels, self.hidden), nn.GELU(approximate="tanh"), nn.Linear(self.hidden, channels)
        )

    def forward(self, x: torch.Tensor) -> torch.Tensor:
        return in_rows(self.mlp, x, self.hidden)


class FlowBlock(nn.Module):
    """Self-attention, image attention and a feed-forward layer, modulated by the timestep."""

    def __init__(self, channels: int, context_channels: int, projected_channels: int, heads: int, ratio: float):
        super().__init__()
        self.norm1 = LayerNorm32(channels, elementwise_affine=False, eps=1e-6)
        self.norm2 = LayerNorm32(channels, elementwise_affine=True, eps=1e-6)
        self.norm3 = LayerNorm32(channels, elementwise_affine=False, eps=1e-6)
        self.self_attn = SelfAttention(channels, heads)
        self.cross_attn = ProjectedAttention(channels, context_channels, projected_channels, heads)
        self.mlp = FeedForward(channels, ratio)
        self.modulation = nn.Parameter(torch.zeros(6 * channels))

    def forward(
        self, x: torch.Tensor, mod: torch.Tensor, context: torch.Tensor, projected: torch.Tensor, phases: torch.Tensor
    ) -> torch.Tensor:
        shift_msa, scale_msa, gate_msa, shift_mlp, scale_mlp, gate_mlp = (
            (self.modulation + mod).type(mod.dtype).chunk(6, dim=1)
        )
        h = self.norm1(x) * (1 + scale_msa) + shift_msa
        x = x + self.self_attn(h, phases) * gate_msa
        x = x + self.cross_attn(self.norm2(x), context, projected)
        h = self.norm3(x) * (1 + scale_mlp) + shift_mlp
        return x + self.mlp(h) * gate_mlp


class TimestepEmbedder(nn.Module):
    def __init__(self, channels: int, frequencies: int = 256):
        super().__init__()
        self.frequencies = frequencies
        self.mlp = nn.Sequential(nn.Linear(frequencies, channels), nn.SiLU(), nn.Linear(channels, channels))

    def forward(self, t: torch.Tensor) -> torch.Tensor:
        half = self.frequencies // 2
        freqs = torch.exp(-numpy.log(10000) * torch.arange(start=0, end=half, dtype=torch.float32) / half)
        args = t[:, None].float() * freqs.to(t.device)[None]
        return self.mlp(torch.cat([torch.cos(args), torch.sin(args)], dim=-1))


class FlowTransformer(nn.Module):
    """A flow-matching transformer over latent tokens at integer grid positions.

    The structure stage's tokens are every cell of a dense grid; the shape
    stages' tokens are the occupied voxels. Every token attends to all
    others, to the image's global tokens, and adds its own projected image
    features in every block.
    """

    def __init__(
        self,
        resolution: int,
        in_channels: int,
        out_channels: int,
        model_channels: int,
        cond_channels: int,
        proj_in_channels: int,
        num_blocks: int,
        num_heads: int,
        mlp_ratio: float,
    ):
        super().__init__()
        self.resolution = resolution
        self.in_channels = in_channels
        self.head_dim = model_channels // num_heads
        self.working = torch.float32
        self.t_embedder = TimestepEmbedder(model_channels)
        self.adaLN_modulation = nn.Sequential(nn.SiLU(), nn.Linear(model_channels, 6 * model_channels))
        self.input_layer = nn.Linear(in_channels, model_channels)
        self.blocks = nn.ModuleList(
            FlowBlock(model_channels, cond_channels, proj_in_channels, num_heads, mlp_ratio) for _ in range(num_blocks)
        )
        self.out_layer = nn.Linear(model_channels, out_channels)

    def forward(
        self, x: torch.Tensor, t: torch.Tensor, coords: torch.Tensor, context: torch.Tensor, projected: torch.Tensor
    ) -> torch.Tensor:
        """The predicted flow for N x in_channels tokens at `coords` (N x 3) and time `t` (1)."""
        h = self.input_layer(x).type(self.working)
        mod = self.adaLN_modulation(self.t_embedder(t)).type(self.working)
        context = context.type(self.working)
        projected = projected.type(self.working)
        phases = rope_phases(coords, self.head_dim)
        for block in self.blocks:
            h = block(h, mod, context, projected, phases)
        h = h.type(x.dtype)
        return self.out_layer(F.layer_norm(h, h.shape[-1:]))


def dense_coords(resolution: int, device: torch.device) -> torch.Tensor:
    """Every cell of a resolution^3 grid, x slowest and z fastest."""
    axis = torch.arange(resolution, device=device)
    return torch.stack(torch.meshgrid(axis, axis, axis, indexing="ij"), dim=-1).reshape(-1, 3)


class ResBlock3d(nn.Module):
    def __init__(self, channels: int):
        super().__init__()
        self.norm1 = ChannelLayerNorm32(channels)
        self.norm2 = ChannelLayerNorm32(channels)
        self.conv1 = nn.Conv3d(channels, channels, 3, padding=1)
        self.conv2 = nn.Conv3d(channels, channels, 3, padding=1)

    def forward(self, x: torch.Tensor) -> torch.Tensor:
        h = self.conv1(F.silu(self.norm1(x)))
        h = self.conv2(F.silu(self.norm2(h)))
        return h + x


class UpsampleBlock3d(nn.Module):
    def __init__(self, channels: int, out_channels: int):
        super().__init__()
        self.conv = nn.Conv3d(channels, out_channels * 8, 3, padding=1)

    def forward(self, x: torch.Tensor) -> torch.Tensor:
        x = self.conv(x)
        batch, channels, height, width, depth = x.shape
        x = x.reshape(batch, channels // 8, 2, 2, 2, height, width, depth).permute(0, 1, 5, 2, 6, 3, 7, 4)
        return x.reshape(batch, channels // 8, height * 2, width * 2, depth * 2)


class StructureDecoder(nn.Module):
    """Decodes the structure latent to occupancy logits on a grid four times finer."""

    def __init__(
        self,
        out_channels: int,
        latent_channels: int,
        num_res_blocks: int,
        channels: list[int],
        num_res_blocks_middle: int,
    ):
        super().__init__()
        self.working = torch.float32
        self.input_layer = nn.Conv3d(latent_channels, channels[0], 3, padding=1)
        self.middle_block = nn.Sequential(*(ResBlock3d(channels[0]) for _ in range(num_res_blocks_middle)))
        self.blocks = nn.ModuleList()
        for index, width in enumerate(channels):
            self.blocks.extend(ResBlock3d(width) for _ in range(num_res_blocks))
            if index < len(channels) - 1:
                self.blocks.append(UpsampleBlock3d(width, channels[index + 1]))
        self.out_layer = nn.Sequential(
            ChannelLayerNorm32(channels[-1]), nn.SiLU(), nn.Conv3d(channels[-1], out_channels, 3, padding=1)
        )

    def forward(self, x: torch.Tensor) -> torch.Tensor:
        h = self.middle_block(self.input_layer(x).type(self.working))
        for block in self.blocks:
            h = block(h)
        return self.out_layer(h.type(x.dtype))


class Voxels:
    """Features on a sparse grid, with the neighbour table its convolutions share."""

    def __init__(self, feats: torch.Tensor, coords: torch.Tensor, size: int):
        self.feats = feats
        self.coords = coords
        self.size = size
        self._neighbours: torch.Tensor | None = None

    def replace(self, feats: torch.Tensor) -> Voxels:
        same = Voxels(feats, self.coords, self.size)
        same._neighbours = self._neighbours
        return same

    def neighbours(self) -> torch.Tensor:
        if self._neighbours is None:
            self._neighbours = voxels.neighbours(self.coords, self.size)
        return self._neighbours


class SparseConv3d(nn.Module):
    """A 3 x 3 x 3 convolution evaluated only at the voxels present."""

    def __init__(self, channels: int, out_channels: int):
        super().__init__()
        self.weight = nn.Parameter(torch.zeros(out_channels, 3, 3, 3, channels))
        self.bias = nn.Parameter(torch.zeros(out_channels))

    def forward(self, x: Voxels) -> Voxels:
        return x.replace(voxels.submanifold_conv(x.feats, x.neighbours(), self.weight, self.bias))


class ConvNeXtBlock3d(nn.Module):
    def __init__(self, channels: int, ratio: float = 4.0):
        super().__init__()
        self.norm = LayerNorm32(channels, elementwise_affine=True, eps=1e-6)
        self.conv = SparseConv3d(channels, channels)
        self.hidden = int(channels * ratio)
        self.mlp = nn.Sequential(nn.Linear(channels, self.hidden), nn.SiLU(), nn.Linear(self.hidden, channels))

    def forward(self, x: Voxels) -> Voxels:
        h = in_rows(lambda rows: self.mlp(self.norm(rows)), self.conv(x).feats, self.hidden)
        h += x.feats
        return x.replace(h)


class SubdivideBlock3d(nn.Module):
    """Predicts which of each voxel's eight children exist, and their features."""

    def __init__(self, channels: int, out_channels: int):
        super().__init__()
        self.out_channels = out_channels
        self.norm1 = LayerNorm32(channels, elementwise_affine=True, eps=1e-6)
        self.norm2 = LayerNorm32(out_channels, elementwise_affine=False, eps=1e-6)
        self.conv1 = SparseConv3d(channels, out_channels * 8)
        self.conv2 = SparseConv3d(out_channels, out_channels)
        self.to_subdiv = nn.Linear(channels, 8)

    def forward(self, x: Voxels) -> Voxels:
        children = self.to_subdiv(x.feats) > 0
        h = self.conv1(x.replace(F.silu(self.norm1(x.feats)))).feats
        coords, parent, child = voxels.subdivide(x.coords, children)
        h = h.reshape(h.shape[0] * 8, h.shape[1] // 8)[parent * 8 + child]
        skip = x.feats.reshape(x.feats.shape[0] * 8, x.feats.shape[1] // 8)[parent * 8 + child]
        skip = skip.repeat_interleave(self.out_channels // skip.shape[1], dim=1)
        fine = Voxels(F.silu(self.norm2(h)), coords, x.size * 2)
        return fine.replace(self.conv2(fine).feats + skip)


class ShapeDecoder(nn.Module):
    """Decodes a shape latent on sparse voxels to a flexible dual grid, subdividing on the way."""

    def __init__(self, model_channels: list[int], latent_channels: int, num_blocks: list[int]):
        super().__init__()
        self.working = torch.float32
        self.from_latent = nn.Linear(latent_channels, model_channels[0])
        self.blocks = nn.ModuleList()
        for index, count in enumerate(num_blocks):
            level = nn.ModuleList(ConvNeXtBlock3d(model_channels[index]) for _ in range(count))
            if index < len(num_blocks) - 1:
                level.append(SubdivideBlock3d(model_channels[index], model_channels[index + 1]))
            self.blocks.append(level)
        self.output_layer = nn.Linear(model_channels[-1], 7)

    def _run(self, x: Voxels, levels: int) -> Voxels:
        h = x.replace(self.from_latent(x.feats).type(self.working))
        for level in self.blocks[:levels]:
            for block in level:
                h = block(h)
        return h

    def upsample(self, x: Voxels, times: int) -> torch.Tensor:
        """The voxels the latent subdivides into, `times` levels down."""
        return self._run(x, times).coords

    def forward(self, x: Voxels) -> tuple[torch.Tensor, torch.Tensor]:
        """The surface as vertices in [-0.5, 0.5] and triangles."""
        h = self._run(x, len(self.blocks))
        feats = self.output_layer(F.layer_norm(h.feats.type(x.feats.dtype), h.feats.shape[-1:]))
        margin = 0.5
        offsets = (1 + 2 * margin) * torch.sigmoid(feats[:, 0:3]) - margin
        return voxels.dual_grid_mesh(h.coords, offsets, feats[:, 3:6] > 0, F.softplus(feats[:, 6:7]), h.size)


# The precision-sensitive layers that run in the working precision; norms,
# modulation vectors and the layers outside the network's torso stay float32.
_CAST = (nn.Linear, nn.Conv3d, SparseConv3d)


def _set_working(model: nn.Module, torso: list[nn.Module], dtype: torch.dtype) -> None:
    model.working = dtype
    for part in torso:
        for layer in part.modules():
            if isinstance(layer, _CAST):
                for parameter in layer.parameters(recurse=False):
                    parameter.data = parameter.data.to(dtype)


def _config(path: Path, name: str, expected: dict) -> dict:
    """A checkpoint's network arguments, which must name the expected network and settings."""
    try:
        config = json.loads(path.read_text(encoding="utf-8"))
    except (OSError, ValueError) as error:
        raise ModelError(f"{path.name} could not be read: {error}") from error
    if not isinstance(config, dict) or config.get("name") != name or not isinstance(config.get("args"), dict):
        raise ModelError(f"{path.name} does not describe a {name}")
    args = config["args"]
    for key, value in expected.items():
        if args.get(key, value) != value:
            raise ModelError(f"{path.name} sets {key} to {args.get(key)!r}; this worker supports only {value!r}")
    return args


def _int(args: dict, key: str, path: Path) -> int:
    value = args.get(key)
    if not isinstance(value, int) or isinstance(value, bool) or value < 1:
        raise ModelError(f"{path.name} needs a positive whole number for {key}")
    return value


def _ints(args: dict, key: str, path: Path, minimum: int = 1) -> list[int]:
    value = args.get(key)
    if (
        not isinstance(value, list)
        or not value
        or any(not isinstance(item, int) or isinstance(item, bool) or item < minimum for item in value)
    ):
        raise ModelError(f"{path.name} needs a list of whole numbers for {key}")
    return value


def build_flow(path: Path) -> FlowTransformer:
    """A flow transformer on the meta device, from its configuration file."""
    flow_names = {"SparseStructureFlowModel", "ElasticSLatFlowModel", "SLatFlowModel"}
    try:
        name = json.loads(path.read_text(encoding="utf-8")).get("name")
    except (OSError, ValueError, AttributeError) as error:
        raise ModelError(f"{path.name} could not be read: {error}") from error
    if name not in flow_names:
        raise ModelError(f"{path.name} does not describe a flow transformer")
    args = _config(
        path,
        name,
        {"pe_mode": "rope", "share_mod": True, "qk_rms_norm": True, "qk_rms_norm_cross": True, "image_attn_mode": "proj"},
    )
    if args.get("rope_freq", [1.0, 10000.0]) != [1.0, 10000.0]:
        raise ModelError(f"{path.name} sets rope_freq, which this worker does not support")
    channels = _int(args, "model_channels", path)
    heads = args.get("num_heads") or channels // args.get("num_head_channels", 64)
    ratio = args.get("mlp_ratio", 4)
    if not isinstance(heads, int) or heads < 1 or channels % heads or (channels // heads) % 2:
        raise ModelError(f"{path.name} gives a head count that does not divide its channels")
    if not isinstance(ratio, (int, float)) or isinstance(ratio, bool) or ratio <= 0:
        raise ModelError(f"{path.name} needs a positive mlp_ratio")
    cond = _int(args, "cond_channels", path)
    with torch.device("meta"):
        model = FlowTransformer(
            resolution=_int(args, "resolution", path),
            in_channels=_int(args, "in_channels", path),
            out_channels=_int(args, "out_channels", path),
            model_channels=channels,
            cond_channels=cond,
            proj_in_channels=args.get("proj_in_channels") or cond,
            num_blocks=_int(args, "num_blocks", path),
            num_heads=heads,
            mlp_ratio=float(ratio),
        )
    working = {"float32": torch.float32, "bfloat16": torch.bfloat16, "float16": torch.float16}.get(
        args.get("dtype", "float32")
    )
    if working is None:
        raise ModelError(f"{path.name} names an unknown dtype")
    _set_working(model, [model.blocks], working)
    return model


def build_structure_decoder(path: Path) -> StructureDecoder:
    args = _config(path, "SparseStructureDecoder", {"norm_type": "layer"})
    with torch.device("meta"):
        model = StructureDecoder(
            out_channels=_int(args, "out_channels", path),
            latent_channels=_int(args, "latent_channels", path),
            num_res_blocks=_int(args, "num_res_blocks", path),
            channels=_ints(args, "channels", path),
            num_res_blocks_middle=args.get("num_res_blocks_middle", 2),
        )
    if args.get("use_fp16", False):
        _set_working(model, [model.blocks, model.middle_block], torch.float16)
    return model


def build_shape_decoder(path: Path) -> ShapeDecoder:
    args = _config(path, "FlexiDualGridVaeDecoder", {"voxel_margin": 0.5})
    channels = _ints(args, "model_channels", path)
    blocks = _ints(args, "num_blocks", path, minimum=0)
    block_type = args.get("block_type")
    up_block_type = args.get("up_block_type")
    if (
        len(blocks) != len(channels)
        or block_type != ["SparseConvNeXtBlock3d"] * len(channels)
        or up_block_type != ["SparseResBlockC2S3d"] * (len(channels) - 1)
        or any(item != {} for item in args.get("block_args", [{}] * len(channels)))
        or any(later * 8 % earlier for earlier, later in zip(channels, channels[1:]))
    ):
        raise ModelError(f"{path.name} describes a shape decoder this worker does not support")
    with torch.device("meta"):
        model = ShapeDecoder(channels, _int(args, "latent_channels", path), blocks)
    if args.get("use_fp16", False):
        _set_working(model, [model.blocks], torch.float16)
    return model


# Checkpoint entries that the networks compute for themselves.
_DERIVED = {"rope_phases"}
_DTYPES = {
    "F64": torch.float64,
    "F32": torch.float32,
    "F16": torch.float16,
    "BF16": torch.bfloat16,
    "I64": torch.int64,
    "I32": torch.int32,
    "I16": torch.int16,
    "I8": torch.int8,
    "U8": torch.uint8,
    "BOOL": torch.bool,
    "C64": torch.complex64,
}
# A safetensors header larger than this is not a checkpoint this worker reads.
_MAX_HEADER_BYTES = 64 * 1024 * 1024


def load(model: nn.Module, path: Path, device: torch.device, rename=None) -> nn.Module:
    """Give a network built on the meta device its weights from a safetensors file.

    Each tensor is read from the file on its own, without mapping the file
    into memory, and converted straight to its parameter's precision on
    `device`, so the process never holds more than one stored tensor beside
    the network. The file must hold exactly the network's parameters, under
    their own names or the names `rename` gives them.
    """
    expected = model.state_dict()
    rename = rename or (lambda name: name)
    weights = {}
    try:
        with path.open("rb") as stored:
            size = stored.seek(0, 2)
            stored.seek(0)
            (length,) = struct.unpack("<Q", stored.read(8))
            if length > min(_MAX_HEADER_BYTES, size - 8):
                raise ModelError(f"{path.name} is not a safetensors file")
            header = json.loads(stored.read(length))
            header.pop("__metadata__", None)
            entries = {rename(name): entry for name, entry in header.items() if name not in _DERIVED}
            if set(entries) != set(expected):
                missing = len(set(expected) - set(entries))
                unexpected = len(set(entries) - set(expected))
                raise ModelError(
                    f"{path.name} does not match its network: {missing} missing and {unexpected} unexpected weights"
                )
            for name, parameter in expected.items():
                weights[name] = _read(stored, 8 + length, size, entries[name], name, path).to(
                    device=device, dtype=parameter.dtype
                )
    except (OSError, ValueError, KeyError, TypeError, struct.error) as error:
        raise ModelError(f"{path.name} could not be read: {error}") from error
    for name, parameter in expected.items():
        if tuple(weights[name].shape) != tuple(parameter.shape):
            raise ModelError(f"{path.name} gives {name} the shape {tuple(weights[name].shape)}")
    model.load_state_dict(weights, strict=True, assign=True)
    return model.requires_grad_(False).eval()


def _read(stored, start: int, size: int, entry: dict, name: str, path: Path) -> torch.Tensor:
    dtype = _DTYPES.get(entry["dtype"])
    shape = [int(side) for side in entry["shape"]]
    begin, end = (int(offset) for offset in entry["data_offsets"])
    if dtype is None:
        raise ModelError(f"{path.name} stores {name} as {entry['dtype']}")
    count = 1
    for side in shape:
        count *= side
    width = torch.empty((), dtype=dtype).element_size()
    if begin < 0 or end - begin != count * width or start + end > size:
        raise ModelError(f"{path.name} gives {name} a size that does not match its shape")
    raw = torch.empty(end - begin, dtype=torch.uint8)
    stored.seek(start + begin)
    if stored.readinto(raw.numpy()) != end - begin:
        raise ModelError(f"{path.name} ends inside {name}")
    return raw.view(dtype).reshape(shape)


@dataclass(frozen=True)
class Sampler:
    """Euler sampling of a flow with classifier-free guidance inside a time interval."""

    steps: int
    rescale_t: float
    guidance_strength: float
    guidance_rescale: float
    guidance_interval: tuple[float, float]
    sigma_min: float


def sample(model, noise: torch.Tensor, settings: Sampler, std) -> torch.Tensor:
    """Euler steps of a flow from noise at t = 1 to a sample at t = 0.

    `model(x, t, positive)` predicts the flow at time `t` (scaled by 1000)
    with the image (`positive=True`) or without it. Inside the guidance
    interval the two are extrapolated by the guidance strength, and the
    result is pulled back towards the conditional prediction's spread,
    measured by `std`.
    """
    sigma = settings.sigma_min
    rescale_t = settings.rescale_t
    times = numpy.linspace(1, 0, settings.steps + 1)
    times = (rescale_t * times / (1 + (rescale_t - 1) * times)).tolist()
    x = noise
    for t, t_prev in zip(times[:-1], times[1:]):
        timestep = torch.tensor([1000 * t], device=x.device, dtype=torch.float32)
        low, high = settings.guidance_interval
        strength = settings.guidance_strength if low <= t <= high else 1
        if strength == 1:
            pred = model(x, timestep, True)
        elif strength == 0:
            pred = model(x, timestep, False)
        else:
            positive = model(x, timestep, True)
            negative = model(x, timestep, False)
            pred = strength * positive + (1 - strength) * negative
            rescale = settings.guidance_rescale
            if rescale > 0:
                x_0_pos = (1 - sigma) * x - (sigma + (1 - sigma) * t) * positive
                x_0_cfg = (1 - sigma) * x - (sigma + (1 - sigma) * t) * pred
                x_0_rescaled = x_0_cfg * (std(x_0_pos) / std(x_0_cfg))
                x_0 = rescale * x_0_rescaled + (1 - rescale) * x_0_cfg
                pred = ((1 - sigma) * x - x_0) / (sigma + (1 - sigma) * t)
            del positive, negative
        x = x - (t - t_prev) * pred
    return x


def dense_std(x: torch.Tensor) -> torch.Tensor:
    """The sample standard deviation of a 1 x C x ... grid."""
    return x.std(dim=list(range(1, x.ndim)), keepdim=True)


def token_std(x: torch.Tensor) -> torch.Tensor:
    """The population standard deviation of N x C tokens, as the sparse latents measure it."""
    mean = x.mean(dim=1, keepdim=True).mean(dim=0, keepdim=True)
    mean2 = (x**2).mean(dim=1, keepdim=True).mean(dim=0, keepdim=True)
    return (mean2 - mean**2).sqrt()
