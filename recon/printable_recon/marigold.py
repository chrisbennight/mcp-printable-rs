"""Marigold V2 depth and surface normals from one photograph in little GPU memory.

Marigold V2 adapts the Qwen-Image-Edit-2509 diffusion transformer to predict
depth or surface normals in a single step. The photograph's VAE latents are
taken as the noisy sample at timestep 0.499, one transformer pass predicts
the flow towards the answer, and the decoded latents are the estimate.

The base transformer has about 20 billion parameters, held as NF4 four-bit
weights. This module never loads it whole. convert_marigold.py stores each
of its blocks in its own file. While the transformer runs, each block is read
from disk, decoded to the working precision, given the task's adapter,
evaluated and released, and the next block is read in the background
meanwhile. Peak GPU memory is one block, the small parts outside the blocks,
the VAE and the activations, at the cost of reading the weights once per
estimate.
"""

from __future__ import annotations

import json
from concurrent.futures import Future, ThreadPoolExecutor
from pathlib import Path

import numpy
import torch
from PIL import Image
from safetensors import safe_open

from . import files, nf4

# Marigold V2 runs its one step at this timestep, computed in bfloat16 as in
# training.
TIMESTEP = 499.0
# Image sides are multiples of this many pixels: the VAE reduces 8 pixels to
# one latent and the transformer pairs latents into 2 x 2 patches.
MULTIPLE = 16
# The VAE works in overlapping tiles of this many pixels, so its activations
# stay small at any image size.
VAE_TILE = 512
VAE_TILE_STRIDE = 384


class ModelError(Exception):
    """The converted model directory is missing a file or does not match."""


def quantized(name: str, shape: tuple[int, ...]) -> bool:
    """Whether Marigold V2 holds this base transformer weight in NF4.

    Every linear layer was loaded in four bits except the modulation layer of
    the first block's image stream; biases and norms keep 16 bits.
    """
    return (
        len(shape) == 2
        and name.endswith(".weight")
        and not name.startswith("transformer_blocks.0.img_mod.")
    )


def read(path: Path, selected: list[str] | None = None) -> dict[str, torch.Tensor]:
    """Tensors stored in a safetensors file, all of them or the selected ones."""
    try:
        with safe_open(str(path), framework="pt", device="cpu") as stored:
            return {name: stored.get_tensor(name) for name in (selected if selected is not None else stored.keys())}
    # safetensors reports damaged and unreadable files with its own error type.
    except Exception as error:
        raise ModelError(f"{path.name} could not be read: {error}") from error


def names(path: Path) -> list[str]:
    """The names of the tensors in a safetensors file."""
    try:
        with safe_open(str(path), framework="pt", device="cpu") as stored:
            return list(stored.keys())
    except Exception as error:
        raise ModelError(f"{path.name} could not be read: {error}") from error


def decode(stored: dict[str, torch.Tensor], device: torch.device, dtype: torch.dtype) -> dict[str, torch.Tensor]:
    """Weights on `device` in `dtype`, with NF4 weights expanded.

    A weight `name` held in NF4 is stored as `name.nf4` (packed indices),
    `name.scale` (block scales) and `name.shape`; others are stored as they are.
    """
    weights = {}
    for name, tensor in stored.items():
        if name.endswith(".nf4"):
            base = name[: -len(".nf4")]
            shape = tuple(int(size) for size in stored[f"{base}.shape"].tolist())
            weights[base] = nf4.dequantize(tensor.to(device), stored[f"{base}.scale"], shape, dtype)
        elif not name.endswith((".scale", ".shape")):
            weights[name] = tensor.to(device, dtype)
    return weights


def attach(
    module: torch.nn.Module,
    adapter: dict[str, torch.Tensor],
    prefix: str,
    device: torch.device,
    dtype: torch.dtype,
) -> list[torch.utils.hooks.RemovableHandle]:
    """Add each low-rank adapter's output to the layer it adapts.

    The adapter holds `lora.<prefix><layer>.a` and `.b` for each adapted layer,
    with `.b` already multiplied by the adapter's alpha over its rank. As in
    training, the update is computed beside the base layer rather than merged
    into its weight, where rounding to the working precision could lose it.
    """
    handles = []
    layers = dict(module.named_modules())
    for name in adapter:
        if not name.endswith(".a"):
            continue
        layer = layers.get(name[len(f"lora.{prefix}") : -len(".a")])
        if layer is None:
            raise ModelError(f"the adapter names {name[len('lora.') : -len('.a')]}, which the model lacks")
        down = adapter[name].to(device, dtype)
        up = adapter[f"{name[: -len('.a')]}.b"].to(device, dtype)
        handles.append(layer.register_forward_hook(_adapted(down, up)))
    return handles


def _adapted(down: torch.Tensor, up: torch.Tensor):
    def hook(_layer: torch.nn.Module, inputs: tuple, output: torch.Tensor) -> torch.Tensor:
        update = torch.nn.functional.linear(torch.nn.functional.linear(inputs[0].to(down.dtype), down), up)
        return output + update

    return hook


class StreamedTransformer:
    """The Qwen image transformer with its blocks read from disk one at a time."""

    def __init__(self, directory: Path, task: Path, device: torch.device, dtype: torch.dtype):
        from diffusers import QwenImageTransformer2DModel

        self.directory = directory
        self.task = task
        self.device = device
        self.dtype = dtype
        try:
            config = json.loads((directory / "transformer" / "config.json").read_text(encoding="utf-8"))
            with torch.device("meta"):
                skeleton = QwenImageTransformer2DModel.from_config(config)
            # The rotary embedding computes its tables when built, so the
            # parts outside the blocks are built for real, and small.
            self.model = QwenImageTransformer2DModel.from_config({**config, "num_layers": 0})
        except (OSError, ValueError, TypeError) as error:
            raise ModelError(f"the transformer configuration could not be used: {error}") from error
        self.blocks = skeleton.transformer_blocks
        self.blocks.requires_grad_(False)
        self.model.requires_grad_(False)
        self.adapter_names = [name for name in names(task) if name.startswith("lora.")]
        base = decode(read(directory / "transformer" / "base.safetensors"), device, dtype)
        _assign(self.model, base, "the transformer's outer layers")
        attach(self.model, self._adapter(""), "", device, dtype)
        self.model.eval()
        self.model.transformer_blocks = self.blocks
        self.pool = ThreadPoolExecutor(max_workers=1)
        self.pending: dict[int, Future] = {}
        self.adapted: dict[int, list[torch.utils.hooks.RemovableHandle]] = {}
        for index, block in enumerate(self.blocks):
            block.register_forward_pre_hook(self._load(index))
            block.register_forward_hook(self._release(index))

    def _adapter(self, prefix: str) -> dict[str, torch.Tensor]:
        names = [name for name in self.adapter_names if name.startswith(f"lora.{prefix}")]
        if prefix == "":
            names = [name for name in names if not name.startswith("lora.transformer_blocks.")]
        return read(self.task, names)

    def _read(self, index: int) -> tuple[dict[str, torch.Tensor], dict[str, torch.Tensor]]:
        return read(self.directory / files.block_file(index)), self._adapter(f"transformer_blocks.{index}.")

    def _load(self, index: int):
        def hook(module: torch.nn.Module, _args) -> None:
            pending = self.pending.pop(index, None)
            stored, adapter = pending.result() if pending is not None else self._read(index)
            if index + 1 < len(self.blocks):
                self.pending[index + 1] = self.pool.submit(self._read, index + 1)
            weights = decode(stored, self.device, self.dtype)
            del stored
            _assign(module, weights, f"block {index}")
            self.adapted[index] = attach(module, adapter, f"transformer_blocks.{index}.", self.device, self.dtype)

        return hook

    def _release(self, index: int):
        def hook(module: torch.nn.Module, _args, _output) -> None:
            for handle in self.adapted.pop(index, []):
                handle.remove()
            module.to_empty(device="meta")

        return hook

    def __call__(
        self, latents: torch.Tensor, prompt: torch.Tensor, mask: torch.Tensor, height: int, width: int
    ) -> torch.Tensor:
        """The predicted flow for packed `latents` of a `height` x `width` latent image."""
        self.pending[0] = self.pool.submit(self._read, 0)
        timestep = torch.full((latents.shape[0],), TIMESTEP, dtype=torch.bfloat16, device=self.device) / 1000.0
        try:
            return self.model(
                hidden_states=latents.to(self.dtype),
                encoder_hidden_states=prompt.to(self.device, self.dtype),
                encoder_hidden_states_mask=mask.to(self.device),
                timestep=timestep,
                img_shapes=[[(1, height // 2, width // 2)]] * latents.shape[0],
                return_dict=False,
            )[0]
        finally:
            for pending in self.pending.values():
                pending.cancel()
            self.pending.clear()
            self.pool.shutdown(wait=True)


def _assign(module: torch.nn.Module, weights: dict[str, torch.Tensor], name: str) -> None:
    """Make `weights` the module's own tensors, which must match it exactly."""
    expected = set(module.state_dict().keys())
    if set(weights) != expected:
        missing = len(expected - set(weights))
        unexpected = len(set(weights) - expected)
        raise ModelError(f"{name} do not match the model: {missing} missing and {unexpected} unexpected weights")
    try:
        module.load_state_dict(weights, strict=False, assign=True)
    except RuntimeError as error:
        raise ModelError(f"{name} do not match the model: {error}") from error


def load_vae(directory: Path, decoder: dict[str, torch.Tensor], device: torch.device, dtype: torch.dtype):
    """The Qwen image VAE with the task's decoder, tiled to bound its memory."""
    from diffusers import AutoencoderKLQwenImage

    try:
        config = json.loads((directory / "vae" / "config.json").read_text(encoding="utf-8"))
        with torch.device("meta"):
            vae = AutoencoderKLQwenImage.from_config(config)
    except (OSError, ValueError, TypeError) as error:
        raise ModelError(f"the VAE configuration could not be used: {error}") from error
    weights = read(directory / "vae" / "diffusion_pytorch_model.safetensors")
    unknown = set(decoder) - set(weights)
    if unknown:
        raise ModelError(f"the task's VAE decoder names {len(unknown)} weights the VAE lacks")
    weights.update(decoder)
    _assign(vae, {name: tensor.to(device, dtype) for name, tensor in weights.items()}, "the VAE weights")
    vae.requires_grad_(False).eval()
    vae.enable_tiling(
        tile_sample_min_height=VAE_TILE,
        tile_sample_min_width=VAE_TILE,
        tile_sample_stride_height=VAE_TILE_STRIDE,
        tile_sample_stride_width=VAE_TILE_STRIDE,
    )
    return vae


def prepare(image: Image.Image, max_side: int) -> torch.Tensor:
    """An RGB photograph as a 1 x 3 x H x W tensor in [-1, 1].

    It is scaled down, never up, so its longer side is at most `max_side`, and
    both sides are rounded to multiples of 16 pixels.
    """
    scale = min(1.0, max_side / max(image.size))
    width, height = (max(MULTIPLE, int(side * scale / MULTIPLE + 0.5) * MULTIPLE) for side in image.size)
    if (width, height) != image.size:
        image = image.resize((width, height), Image.Resampling.LANCZOS)
    pixels = numpy.asarray(image, dtype=numpy.float32) / 127.5 - 1.0
    return torch.from_numpy(pixels).permute(2, 0, 1).unsqueeze(0)


def pack(latents: torch.Tensor) -> torch.Tensor:
    """B x C x H x W latents as B x (H/2 * W/2) x 4C patches, as the transformer reads them."""
    batch, channels, height, width = latents.shape
    patches = latents.view(batch, channels, height // 2, 2, width // 2, 2).permute(0, 2, 4, 1, 3, 5)
    return patches.reshape(batch, (height // 2) * (width // 2), channels * 4)


def unpack(patches: torch.Tensor, height: int, width: int) -> torch.Tensor:
    """The inverse of `pack`, with the VAE's single frame axis restored."""
    batch, _, features = patches.shape
    latents = patches.view(batch, height // 2, width // 2, features // 4, 2, 2).permute(0, 3, 1, 4, 2, 5)
    return latents.reshape(batch, features // 4, 1, height, width)


def predict(
    pixels: torch.Tensor,
    directory: Path,
    task: str,
    device: torch.device,
    dtype: torch.dtype = torch.bfloat16,
    seed: int = 0,
) -> torch.Tensor:
    """Marigold V2's decoded estimate for a prepared photograph: 3 x H x W, float32 on the CPU."""
    task_path = directory / files.task_file(task)
    stored = names(task_path)
    if "prompt.embeds" not in stored or "prompt.mask" not in stored:
        raise ModelError(f"{task_path.name} has no prompt")
    prompt = read(task_path, ["prompt.embeds", "prompt.mask"])
    decoder = read(task_path, [name for name in stored if name.startswith("vae.")])
    vae = load_vae(directory, {name[len("vae.") :]: tensor for name, tensor in decoder.items()}, device, dtype)
    del decoder
    with torch.no_grad():
        latents = vae.encode(pixels.to(device, dtype).unsqueeze(2)).latent_dist.sample(
            generator=torch.Generator(device=device).manual_seed(seed)
        )
        shape = (1, latents.shape[1], 1, 1, 1)
        mean = torch.tensor(vae.config.latents_mean, device=device, dtype=dtype).view(shape)
        inverse_std = (1.0 / torch.tensor(vae.config.latents_std, device=device, dtype=dtype)).view(shape)
        latents = (latents - mean) * inverse_std
        height, width = latents.shape[-2:]
        transformer = StreamedTransformer(directory, task_path, device, dtype)
        flow = transformer(
            pack(latents[:, :, 0]),
            prompt["prompt.embeds"][:1],
            prompt["prompt.mask"][:1].bool(),
            height,
            width,
        )
        del transformer
        latents = latents - unpack(flow, height, width).to(dtype)
        decoded = vae.decode(latents / inverse_std + mean).sample[:, :, 0]
    return decoded[0].float().cpu()
