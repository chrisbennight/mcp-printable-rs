"""Generate an object's shape from one photograph with Pixal3D, a stage at a time.

Each stage loads only its own network, runs it and releases it before the
next stage loads, so the GPU holds one network and the small results passed
between stages. Weights are converted to their working precision as they are
read. The colour stages are never run.
"""

from __future__ import annotations

import json
import time
from dataclasses import dataclass, field
from pathlib import Path

import numpy
import torch
from PIL import Image

from . import features, files, pixal3d
from .pixal3d import ModelError, Voxels

RESOLUTIONS = (512, 1024, 1536)
# The structure stage's occupancy is reduced to this grid before the shape stages.
STRUCTURE_GRID = 32
# The shape decoder subdivides four times, so a latent grid g gives a surface grid 16g.
SUBDIVISIONS = 4
# Above 1024, the finer grid is coarsened in steps of 128 until the shape
# latent has fewer tokens than this.
MAX_TOKENS = 49152
# Image sizes and NAF output sizes the stages were trained with.
COARSE_IMAGE = 512
FINE_IMAGE = 1024
NAF_SIZE = 512


class GenerationError(Exception):
    """The photograph gave nothing to build a shape from."""


@dataclass
class Settings:
    """The published pipeline's sampling settings and latent normalization."""

    structure: pixal3d.Sampler
    shape: pixal3d.Sampler
    mean: torch.Tensor
    std: torch.Tensor


def read_settings(directory: Path, steps: int | None) -> Settings:
    try:
        args = json.loads((directory / files.PIXAL3D_PIPELINE).read_text(encoding="utf-8"))["args"]
        samplers = [_sampler(args[name], steps) for name in ("sparse_structure_sampler", "shape_slat_sampler")]
        normalization = args["shape_slat_normalization"]
        mean = torch.tensor([float(value) for value in normalization["mean"]])
        std = torch.tensor([float(value) for value in normalization["std"]])
    except (OSError, ValueError, KeyError, TypeError) as error:
        raise ModelError(f"{files.PIXAL3D_PIPELINE} does not give the sampling settings: {error}") from error
    if mean.shape != std.shape or bool((std <= 0).any()):
        raise ModelError(f"{files.PIXAL3D_PIPELINE} gives an unusable latent normalization")
    return Settings(samplers[0], samplers[1], mean, std)


def _sampler(entry: dict, steps: int | None) -> pixal3d.Sampler:
    if entry["name"] != "FlowEulerGuidanceIntervalSampler":
        raise ValueError(f"the sampler {entry['name']} is not supported")
    params = entry["params"]
    interval = [float(value) for value in params["guidance_interval"]]
    if len(interval) != 2:
        raise ValueError("the guidance interval needs two times")
    return pixal3d.Sampler(
        steps=int(steps if steps is not None else params["steps"]),
        rescale_t=float(params["rescale_t"]),
        guidance_strength=float(params["guidance_strength"]),
        guidance_rescale=float(params.get("guidance_rescale", 0.0)),
        guidance_interval=(interval[0], interval[1]),
        sigma_min=float(entry["args"]["sigma_min"]),
    )


@dataclass
class Report:
    """What a generation did, stage by stage."""

    stages: list[dict] = field(default_factory=list)

    def stage(self, name: str, device: torch.device):
        return _Stage(self, name, device)


class _Stage:
    def __init__(self, report: Report, name: str, device: torch.device):
        self.report, self.name, self.device = report, name, device

    def __enter__(self):
        self.started = time.monotonic()
        if self.device.type == "cuda":
            torch.cuda.reset_peak_memory_stats(self.device)
        return self

    def __exit__(self, *_exc) -> None:
        entry = {"stage": self.name}
        if self.device.type == "cuda":
            # The stage's queued GPU work counts towards its own time.
            torch.cuda.synchronize(self.device)
            entry["peak_gpu_memory_bytes"] = int(torch.cuda.max_memory_allocated(self.device))
            torch.cuda.empty_cache()
        entry["seconds"] = round(time.monotonic() - self.started, 3)
        self.report.stages.append(entry)


@dataclass
class Shape:
    vertices: numpy.ndarray  # V x 3, in the cube from -0.5 to 0.5: x right, y up, z towards the camera
    triangles: numpy.ndarray  # T x 3
    resolution: int
    steps: dict
    tokens: dict
    fov: float
    fov_source: str
    background: str
    report: Report


def generate(
    photo: Image.Image,
    photo_fov: float | None,
    directory: Path,
    segmenter_code: Path,
    device: torch.device,
    resolution: int = 1024,
    steps: int | None = None,
    seed: int = 42,
    fov: float | None = None,
) -> Shape:
    """The shape Pixal3D generates for the subject of `photo`."""
    if resolution not in RESOLUTIONS:
        raise ValueError(f"resolution must be one of {', '.join(map(str, RESOLUTIONS))}")
    try:
        checkpoints = {stage: directory / name for stage, name in files.pixal3d_checkpoints(directory).items()}
    except ValueError as error:
        raise ModelError(str(error)) from error
    settings = read_settings(directory, steps)
    report = Report()
    generator = torch.Generator().manual_seed(seed)

    with report.stage("background", device):
        photo = features.reduce(photo)
        if photo.mode == "RGBA":
            cutout, background = photo, "transparency"
        else:
            segmenter = features.load_segmenter(segmenter_code, directory / "birefnet", device)
            cutout, background = features.remove_background(photo, segmenter, device), "removed"
            del segmenter
        try:
            framed, fraction = features.frame(cutout)
        except ValueError as error:
            raise GenerationError(str(error)) from error
    if fov is not None:
        fov_source = "request"
    else:
        fov_source = "photograph" if photo_fov is not None else "assumed"
        fov = features.framed_fov(photo_fov, fraction)
    distance = features.camera_distance(fov)

    with report.stage("image features", device), torch.no_grad():
        coarse_pixels = features.pixels_at(framed, COARSE_IMAGE, device)
        fine_pixels = features.pixels_at(framed, FINE_IMAGE, device) if resolution > 512 else None
        dino = features.load_dinov3(directory / "dinov3", device)
        coarse_global, coarse_patches = features.dinov3_tokens(dino, coarse_pixels)
        if fine_pixels is not None:
            fine_global, fine_patches = features.dinov3_tokens(dino, fine_pixels)
        del dino
        # NAF is small and used in two stages; it waits on the processor in between.
        naf = features.load_naf(directory / "naf" / "naf_release.pth", torch.device("cpu"))

    camera = (fov, distance)
    with report.stage("structure", device), torch.no_grad():
        coords = _structure(checkpoints, coarse_global, coarse_patches, camera, settings, generator, device)
    if coords.shape[0] == 0:
        raise GenerationError("the structure stage found nothing to build; try another photograph or seed")
    tokens = {"structure": int(coords.shape[0])}

    with report.stage("shape at 512", device), torch.no_grad():
        latent = _shape_latent(
            checkpoints["shape_slat_flow_model_512"], coords, coarse_global,
            _projected(coarse_patches, coords, STRUCTURE_GRID, COARSE_IMAGE, camera, naf, coarse_pixels),
            settings, generator, device,
        )
    size = STRUCTURE_GRID
    used = 512

    if resolution > 512:
        with report.stage("subdivision", device), torch.no_grad():
            decoder = pixal3d.load(pixal3d.build_shape_decoder(_config(checkpoints["shape_slat_decoder"])),
                                   _weights(checkpoints["shape_slat_decoder"]), device)
            fine = decoder.upsample(Voxels(latent, coords, size), SUBDIVISIONS)
            del decoder, latent
            used = resolution
            while True:
                size = used // 16
                coords = ((fine.float() + 0.5) / (STRUCTURE_GRID * 16) * (size - 1)).round().int().unique(dim=0)
                if coords.shape[0] < MAX_TOKENS or used == 1024:
                    break
                used -= 128
            del fine
        if coords.shape[0] == 0:
            raise GenerationError("the shape vanished when refined; try another photograph or seed")
        with report.stage(f"shape at {used}", device), torch.no_grad():
            latent = _shape_latent(
                checkpoints["shape_slat_flow_model_1024"], coords, fine_global,
                _projected(fine_patches, coords, size, FINE_IMAGE, camera, naf, fine_pixels),
                settings, generator, device,
            )
    tokens["shape"] = int(coords.shape[0])
    del naf

    with report.stage("surface", device), torch.no_grad():
        decoder = pixal3d.load(pixal3d.build_shape_decoder(_config(checkpoints["shape_slat_decoder"])),
                               _weights(checkpoints["shape_slat_decoder"]), device)
        vertices, triangles = decoder(Voxels(latent, coords, size))
        del decoder, latent
        vertices = vertices.cpu().numpy()
        triangles = triangles.cpu().numpy()
    if triangles.shape[0] == 0:
        raise GenerationError("the shape has no surface; try another photograph or seed")
    steps = {"structure": settings.structure.steps, "shape": settings.shape.steps}
    return Shape(vertices, triangles, used, steps, tokens, fov, fov_source, background, report)


def _projected(patches, coords, grid, side, camera, naf=None, pixels=None) -> torch.Tensor:
    """The image features each voxel sees: DINOv3's, and NAF's upsampled ones when `naf` is given."""
    fov, distance = camera
    positions = features.project(coords, grid, fov, distance, side)
    low = features.sample(patches, positions)
    if naf is None:
        return low
    naf.to(patches.device)
    try:
        high = naf(pixels, patches, NAF_SIZE)
    finally:
        naf.cpu()
    return torch.cat([low, features.sample(high, positions)], dim=-1)


def _structure(checkpoints, global_tokens, patches, camera, settings: Settings, generator, device) -> torch.Tensor:
    """The occupied voxels of the structure grid."""
    flow = pixal3d.load(pixal3d.build_flow(_config(checkpoints["sparse_structure_flow_model"])),
                        _weights(checkpoints["sparse_structure_flow_model"]), device)
    grid = flow.resolution
    cells = pixal3d.dense_coords(grid, device)
    context = _projected(patches, cells, grid, COARSE_IMAGE, camera)
    noise = torch.randn(1, flow.in_channels, grid, grid, grid, generator=generator).to(device)

    def structure_flow(x, t, positive):
        tokens = x.reshape(1, x.shape[1], -1).permute(0, 2, 1)[0]
        out = flow(tokens, t, cells, *_condition(global_tokens, context, positive))
        return out.t().reshape(x.shape)

    latent = pixal3d.sample(structure_flow, noise, settings.structure, pixal3d.dense_std)
    # Release the transformer before the decoder loads.
    structure_flow = flow = None
    decoder = pixal3d.load(pixal3d.build_structure_decoder(_config(checkpoints["sparse_structure_decoder"])),
                           _weights(checkpoints["sparse_structure_decoder"]), device)
    occupied = decoder(latent) > 0
    if occupied.shape[2] != STRUCTURE_GRID:
        ratio = occupied.shape[2] // STRUCTURE_GRID
        occupied = torch.nn.functional.max_pool3d(occupied.float(), ratio, ratio, 0) > 0.5
    return torch.argwhere(occupied[0, 0]).int()


def _shape_latent(path, coords, global_tokens, context, settings: Settings, generator, device) -> torch.Tensor:
    flow = pixal3d.load(pixal3d.build_flow(_config(path)), _weights(path), device)
    noise = torch.randn(coords.shape[0], flow.in_channels, generator=generator).to(device)

    def shape_flow(x, t, positive):
        return flow(x, t, coords, *_condition(global_tokens, context, positive))

    latent = pixal3d.sample(shape_flow, noise, settings.shape, pixal3d.token_std)
    return latent * settings.std.to(device)[None] + settings.mean.to(device)[None]


def _condition(global_tokens: torch.Tensor, context: torch.Tensor, positive: bool):
    if positive:
        return global_tokens, context
    return torch.zeros_like(global_tokens), torch.zeros_like(context)


def _config(base: Path) -> Path:
    return base.with_name(base.name + ".json")


def _weights(base: Path) -> Path:
    return base.with_name(base.name + ".safetensors")
