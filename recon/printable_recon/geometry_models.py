"""Load and run the photograph geometry models listed in models/geometry.json.

Each loader takes a verified model snapshot and a device and returns a
function from an RGB photograph, as an H x W x 3 uint8 array, to a
`Geometry`. Every output is at the photograph's own size. The model code is
vendored at pinned revisions; this module only adapts its inputs and outputs
to one convention:

- depth: metres along the optical axis for metric models, or an unscaled
  depth for relative ones; masked or infinitely far pixels hold the largest
  finite depth and are excluded by `valid`;
- valid: the pixels the model reports as measurable surface;
- focal_px: the estimated focal length in pixels of the photograph;
- normals: unit vectors in the camera frame, x right, y up, z towards the
  camera.

The vendored packages are imported only inside the loaders, so this module
can be imported, and its conventions tested, without them.
"""

from __future__ import annotations

from dataclasses import dataclass, replace
import json
from pathlib import Path
import sys
from typing import Callable

import numpy

VENDOR = Path(__file__).resolve().parents[1] / "vendor"
# MoGe-3 needs a newer, Triton-only FlexGEMM than the one TRELLIS.2 is built
# with. Both are named flex_gemm; MoGe's copy is put first on the path only in
# the process that runs MoGe-3.
MOGE_FLEX_GEMM = VENDOR / "moge-flex-gemm"

DEPTH_PRO_2_PRESETS = {"dp2-h": ("depthpro2-h", "depthpro2-h-eivtm5w7m5.pt"),
                       "dp2-l": ("depthpro2-l", "depthpro2-l-tegstu7fim.pt")}
DEPTH_PRO_2_FOV = "depthpro2-fov-ew55zg4kq7.pt"
MOGE_VERSIONS = {"moge3-vitg": 3, "moge3-vitl": 3, "moge2-vitl-normal": 2}
# Depth Anything 3 works on multiples of its 14-pixel patch; this is the
# largest such side within 1024.
DA3_PROCESS_SIDE = 1008


class ModelLoadError(Exception):
    """A model's files or code could not be loaded."""


@dataclass
class Geometry:
    depth: numpy.ndarray | None = None
    depth_kind: str | None = None
    valid: numpy.ndarray | None = None
    focal_px: float | None = None
    normals: numpy.ndarray | None = None


Runner = Callable[[numpy.ndarray], Geometry]


def load(entry: str, snapshot: Path, device) -> Runner:
    """The runner for a registry entry, with its weights read from `snapshot`.

    For marigold-v2, `snapshot` is the directory convert_marigold.py wrote
    from the downloaded files.
    """
    if entry == "marigold-v2":
        return _marigold(snapshot, device)
    if entry == "da3-mono-large":
        return _depth_anything_3(snapshot / "da3-mono-large", device)
    if entry in DEPTH_PRO_2_PRESETS:
        return _depth_pro_2(entry, snapshot / "depthpro2", device)
    if entry in MOGE_VERSIONS:
        return _moge(MOGE_VERSIONS[entry], snapshot / entry / "model.pt", device)
    raise ModelLoadError(f"no runner for model {entry!r}")


def filled(depth: numpy.ndarray, valid: numpy.ndarray) -> tuple[numpy.ndarray, numpy.ndarray]:
    """Depth with every invalid or non-finite pixel set to the largest valid depth, and the final mask."""
    depth = numpy.asarray(depth, dtype=numpy.float32)
    valid = numpy.asarray(valid, dtype=bool) & numpy.isfinite(depth) & (depth > 0)
    far = float(depth[valid].max()) if valid.any() else 1.0
    return numpy.where(valid, depth, numpy.float32(far)).astype(numpy.float32), valid


def opencv_to_camera(normals: numpy.ndarray) -> numpy.ndarray:
    """Normals from OpenCV's frame (x right, y down, z forward) to x right, y up, z towards the camera."""
    normals = numpy.asarray(normals, dtype=numpy.float32)
    return numpy.stack([normals[..., 0], -normals[..., 1], -normals[..., 2]], axis=-1)


def resized(values: numpy.ndarray, height: int, width: int) -> numpy.ndarray:
    """Bilinear resize of an H x W or H x W x C float array, as the benchmark compared outputs."""
    import torch

    if values.shape[:2] == (height, width):
        return values
    tensor = torch.from_numpy(numpy.ascontiguousarray(values, dtype=numpy.float32))
    tensor = tensor[None, None] if tensor.ndim == 2 else tensor.permute(2, 0, 1)[None]
    out = torch.nn.functional.interpolate(tensor, size=(height, width), mode="bilinear", align_corners=False)[0]
    return (out[0] if values.ndim == 2 else out.permute(1, 2, 0)).numpy()


def _vendor(*names: str) -> None:
    for name in reversed(names):
        path = str(VENDOR / name)
        if path not in sys.path:
            sys.path.insert(0, path)


def _depth_anything_3(directory: Path, device) -> Runner:
    import torch
    from safetensors.torch import load_file

    _vendor("depth-anything-3")
    try:
        from depth_anything_3.cfg import create_object, load_config
        from depth_anything_3.registry import MODEL_REGISTRY
        from depth_anything_3.utils.io.input_processor import InputProcessor
        from depth_anything_3.utils.io.output_processor import OutputProcessor

        name = json.loads((directory / "config.json").read_text(encoding="utf-8")).get("model_name", "da3mono-large")
        network = create_object(load_config(MODEL_REGISTRY[name]))
        stored = load_file(str(directory / "model.safetensors"))
        state = {key.removeprefix("model."): value for key, value in stored.items()}
        network.load_state_dict(state, strict=True)
    except (ImportError, KeyError, OSError, RuntimeError, ValueError) as error:
        raise ModelLoadError(f"Depth Anything 3 could not be loaded: {error}") from error
    network = network.to(device).eval()
    inputs, outputs = InputProcessor(), OutputProcessor()

    @torch.inference_mode()
    def run(rgb: numpy.ndarray) -> Geometry:
        height, width = rgb.shape[:2]
        pixels, _, _ = inputs([rgb], None, None, DA3_PROCESS_SIDE, "upper_bound_resize", num_workers=1)
        pixels = pixels.to(device)[None].float()
        dtype = torch.bfloat16 if device.type == "cuda" and torch.cuda.is_bf16_supported() else torch.float16
        with torch.autocast(device_type=device.type, dtype=dtype, enabled=device.type == "cuda"):
            raw = network(pixels, None, None, [], False, False, "first")
        depth = numpy.asarray(outputs(raw).depth[0], dtype=numpy.float32)
        depth, valid = filled(resized(depth, height, width), numpy.ones((height, width), dtype=bool))
        return Geometry(depth=depth, depth_kind="relative", valid=valid)

    return run


def _depth_pro_2(entry: str, directory: Path, device) -> Runner:
    import torch

    _vendor("depth-pro-2")
    preset, checkpoint = DEPTH_PRO_2_PRESETS[entry]
    try:
        from depth_pro_2 import PRESETS, create_model_and_transforms

        config = replace(PRESETS[preset], checkpoint_uri=str(directory / checkpoint),
                         fov_checkpoint_uri=str(directory / DEPTH_PRO_2_FOV))
        model, transform = create_model_and_transforms(config, device=device)
    except (ImportError, KeyError, OSError, RuntimeError, ValueError) as error:
        raise ModelLoadError(f"Depth Pro 2 could not be loaded: {error}") from error

    @torch.inference_mode()
    def run(rgb: numpy.ndarray) -> Geometry:
        out = model.infer(transform(rgb), f_px=None)
        depth, valid = filled(out["depth"].float().cpu().numpy(), numpy.ones(rgb.shape[:2], dtype=bool))
        return Geometry(depth=depth, depth_kind="metric", valid=valid, focal_px=float(out["focallength_px"].item()))

    return run


def _moge(version: int, checkpoint: Path, device) -> Runner:
    import torch

    _vendor("moge", *(["moge-flex-gemm"] if version == 3 else []))
    try:
        if version == 3:
            from moge.model.v3 import MoGeModel
        else:
            from moge.model.v2 import MoGeModel
        model = MoGeModel.from_pretrained(checkpoint).to(device).eval()
    except (ImportError, KeyError, OSError, RuntimeError, ValueError) as error:
        raise ModelLoadError(f"MoGe could not be loaded: {error}") from error

    @torch.inference_mode()
    def run(rgb: numpy.ndarray) -> Geometry:
        pixels = torch.from_numpy(rgb).to(device).float().div(255).permute(2, 0, 1)
        # The GPU keeps each model's default precision; half precision is not supported on the processor.
        out = model.infer(pixels, use_fp16=False) if device.type == "cpu" else model.infer(pixels)
        depth, valid = filled(out["depth"].float().cpu().numpy(), out["mask"].cpu().numpy())
        normals = out.get("normal")
        return Geometry(
            depth=depth,
            depth_kind="metric",
            valid=valid,
            focal_px=float(out["intrinsics"][0, 0].item()) * rgb.shape[1],
            normals=None if normals is None else opencv_to_camera(normals.float().cpu().numpy()),
        )

    return run


def _marigold(directory: Path, device) -> Runner:
    import torch
    from PIL import Image

    from . import marigold

    # Bfloat16 matrix arithmetic is slow or unsupported on many processors.
    dtype = torch.float32 if device.type == "cpu" else torch.bfloat16

    def run(rgb: numpy.ndarray) -> Geometry:
        height, width = rgb.shape[:2]
        pixels = marigold.prepare(Image.fromarray(rgb), max(height, width))
        try:
            normals = marigold.predict(pixels, directory, "normals", device, dtype=dtype, seed=0).numpy()
        except marigold.ModelError as error:
            raise ModelLoadError(str(error)) from error
        normals = resized(numpy.transpose(normals, (1, 2, 0)), height, width)
        length = numpy.linalg.norm(normals, axis=-1, keepdims=True)
        return Geometry(normals=numpy.where(length > 1e-6, normals / numpy.maximum(length, 1e-6), 0.0))

    return run
