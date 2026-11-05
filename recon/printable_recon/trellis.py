"""Shape-only TRELLIS.2 inference from a verified local model snapshot."""

from __future__ import annotations

from contextlib import contextmanager
import gc
import json
import os
from pathlib import Path
import time
import types
from typing import Callable

import numpy as np
from PIL import Image
import torch
from torch.nn.attention import SDPBackend, sdpa_kernel

from . import features
from .trellis_memory import stream_parameters
from .trellis_errors import NoShapeError, UnavailableError


SHAPE_MODELS = {
    "sparse_structure_flow_model": ("trellis2", "ckpts/ss_flow_img_dit_1_3B_64_bf16"),
    "sparse_structure_decoder": ("trellis1", "ckpts/ss_dec_conv3d_16l8_fp16"),
    "shape_slat_flow_model_512": ("trellis2", "ckpts/slat_flow_img2shape_dit_1_3B_512_bf16"),
    "shape_slat_flow_model_1024": ("trellis2", "ckpts/slat_flow_img2shape_dit_1_3B_1024_bf16"),
    "shape_slat_decoder": ("trellis2", "ckpts/shape_dec_next_dc_f16c32_fp16"),
}
MODEL_TYPES = {
    "sparse_structure_flow_model": "SparseStructureFlowModel",
    "sparse_structure_decoder": "SparseStructureDecoder",
    "shape_slat_flow_model_512": "SLatFlowModel",
    "shape_slat_flow_model_1024": "SLatFlowModel",
    "shape_slat_decoder": "FlexiDualGridVaeDecoder",
}


class _ImageFeatures:
    """The published DINOv3 features using Printable's installed model definitions."""

    def __init__(self, directory: Path):
        self.model = features.load_dinov3(directory, torch.device("cpu"))
        self.image_size = 512

    def to(self, device):
        self.model.to(device)

    def cpu(self):
        self.model.cpu()

    def __call__(self, images: list[Image.Image]) -> torch.Tensor:
        device = next(self.model.parameters()).device
        tokens = []
        for image in images:
            pixels = features.pixels_at(image, self.image_size, device)
            global_tokens, patches = features.dinov3_tokens(self.model, pixels)
            tokens.append(torch.cat([global_tokens, patches[0].flatten(1).T], dim=0))
        return torch.stack(tokens)


def frame_subject(image: Image.Image) -> Image.Image:
    """Apply TRELLIS's square framing to an already segmented RGBA image."""
    if image.mode != "RGBA":
        raise ValueError("TRELLIS framing requires a segmented RGBA image")
    image = features.reduce(image)
    alpha = np.asarray(image)[:, :, 3]
    solid = np.argwhere(alpha > 0.8 * 255)
    if not len(solid):
        raise NoShapeError("no subject was found in the photograph")
    left, top = solid[:, 1].min(), solid[:, 0].min()
    right, bottom = solid[:, 1].max(), solid[:, 0].max()
    side = int(max(right - left, bottom - top))
    if side < 1:
        raise NoShapeError("the subject is too small in the photograph")
    cx, cy = float(left + right) / 2, float(top + bottom) / 2
    pixels = np.asarray(image.crop((cx - side // 2, cy - side // 2, cx + side // 2, cy + side // 2))).astype(np.float32) / 255
    return Image.fromarray((pixels[:, :, :3] * pixels[:, :, 3:4] * 255).astype(np.uint8))


def _chunk_feed_forward(model, rows: int = 1024) -> None:
    from trellis2.modules.sparse.basic import VarLenTensor
    from trellis2.modules.sparse.transformer.blocks import SparseFeedForwardNet

    def forward(self, x):
        out = torch.empty_like(x.feats)
        for start in range(0, len(x.feats), rows):
            piece = VarLenTensor(x.feats[start:start + rows])
            out[start:start + rows] = self.mlp(piece).feats
        return x.replace(out)

    for module in model.modules():
        if isinstance(module, SparseFeedForwardNet):
            module.forward = types.MethodType(forward, module)


def _pipeline(snapshot: Path):
    from trellis2 import models
    from trellis2.pipelines import Trellis2ImageTo3DPipeline
    from trellis2.pipelines.samplers import FlowEulerGuidanceIntervalSampler

    args = json.loads((snapshot / "trellis2/pipeline.json").read_text(encoding="utf-8"))["args"]
    networks, streaming = {}, {}
    for stage, (directory, stem) in SHAPE_MODELS.items():
        base = snapshot / directory / stem
        config = json.loads(base.with_suffix(".json").read_text(encoding="utf-8"))
        if config["name"] != MODEL_TYPES[stage]:
            raise ValueError("the TRELLIS checkpoint describes an unsupported network")
        network = getattr(models, MODEL_TYPES[stage])(**config["args"]).eval()
        streaming[stage] = stream_parameters(network, base.with_suffix(".safetensors"), blocks="flow_model" in stage)
        networks[stage] = network

    def sampler(name):
        config = args[name]
        if config["name"] != "FlowEulerGuidanceIntervalSampler":
            raise ValueError("the TRELLIS pipeline describes an unsupported sampler")
        return FlowEulerGuidanceIntervalSampler(**config["args"])

    pipeline = Trellis2ImageTo3DPipeline(
        models=networks,
        sparse_structure_sampler=sampler("sparse_structure_sampler"),
        sparse_structure_sampler_params=args["sparse_structure_sampler"]["params"],
        shape_slat_sampler=sampler("shape_slat_sampler"),
        shape_slat_sampler_params=args["shape_slat_sampler"]["params"],
        shape_slat_normalization=args["shape_slat_normalization"],
        image_cond_model=_ImageFeatures(snapshot / "dinov3"),
        low_vram=True,
    )
    _chunk_feed_forward(networks["shape_slat_flow_model_1024"])
    pipeline.cuda()
    return pipeline, streaming


def generate(
    snapshot: Path,
    image: Image.Image,
    output: Path,
    *,
    resolution: int,
    steps: int | None,
    seed: int,
    vram_budget_bytes: int = 4 * 1024**3,
    progress: Callable[[dict], None] | None = None,
) -> dict:
    """Persist raw geometry, releasing all inference storage before delivery."""
    if vram_budget_bytes < 1:
        raise ValueError("the TRELLIS GPU memory budget must be positive")
    if not torch.cuda.is_available():
        raise UnavailableError("TRELLIS object generation requires a CUDA GPU")
    if torch.cuda.get_device_capability()[0] < 8:
        raise UnavailableError("the TRELLIS low-memory profile requires an Ampere or newer NVIDIA GPU")
    os.environ.update({
        "ATTN_BACKEND": "sdpa", "SPARSE_ATTN_BACKEND": "sdpa", "SPARSE_CONV_BACKEND": "flex_gemm",
        "TRELLIS_DECODE_TILE": "64", "TRELLIS_DECODE_TILE_SPLIT": "2", "TRELLIS_DECODE_TILE_MARGIN": "16",
        "TRELLIS_DECODE_MLP_CHUNK": "4096", "O_VOXEL_MESH_CHUNK": "100000",
    })
    torch.set_num_threads(int(os.environ.get("OMP_NUM_THREADS", "8")))
    total_memory = torch.cuda.get_device_properties(0).total_memory
    torch.cuda.set_per_process_memory_fraction(min(1.0, vram_budget_bytes / total_memory))
    framed = frame_subject(image)
    stages = []

    @contextmanager
    def stage(name):
        if progress is not None:
            progress({"phase": name})
        torch.cuda.synchronize()
        torch.cuda.reset_peak_memory_stats()
        started = time.monotonic()
        yield
        torch.cuda.synchronize()
        stages.append({"stage": name, "seconds": time.monotonic() - started,
                       "peak_gpu_memory_bytes": torch.cuda.max_memory_allocated(),
                       "reserved_gpu_memory_bytes": torch.cuda.max_memory_reserved()})

    sampler_params = {} if steps is None else {"steps": steps}
    # Reject the dense-matrix fallback, including during float32 image
    # conditioning. Sparse attention applies its stricter fused-kernel policy.
    with torch.inference_mode(), sdpa_kernel([SDPBackend.FLASH_ATTENTION, SDPBackend.EFFICIENT_ATTENTION, SDPBackend.CUDNN_ATTENTION]):
        with stage("load_models"):
            pipeline, streaming = _pipeline(snapshot)
        with stage("image_features"):
            torch.manual_seed(seed)
            adapter = pipeline.image_cond_model
            adapter.to(pipeline.device)

            def condition(side):
                adapter.image_size = side
                tokens = adapter([framed])
                return {"cond": tokens, "neg_cond": torch.zeros_like(tokens)}

            cond = condition(512)
            fine_cond = condition(1024) if resolution > 512 else None
            pipeline.image_cond_model = None
            del adapter
            gc.collect()
            torch.cuda.empty_cache()
        with stage("structure"):
            coords = pipeline.sample_sparse_structure(cond, 32, 1, sampler_params)
            structure_tokens = len(coords)
            if not structure_tokens:
                raise NoShapeError("the model produced no structure; try another photograph or seed")
        with stage("shape_sampling"):
            if resolution == 512:
                latent = pipeline.sample_shape_slat(cond, pipeline.models["shape_slat_flow_model_512"], coords, sampler_params)
                actual_resolution = 512
            else:
                latent, actual_resolution = pipeline.sample_shape_slat_cascade(
                    cond, fine_cond, pipeline.models["shape_slat_flow_model_512"],
                    pipeline.models["shape_slat_flow_model_1024"], 512, resolution, coords, sampler_params,
                )
            shape_tokens = len(latent.feats)
            del cond, fine_cond, coords
            gc.collect()
            torch.cuda.empty_cache()
        with stage("shape_decoding"):
            decoder = pipeline.models["shape_slat_decoder"]
            decoder.set_resolution(actual_resolution)
            decoder.cuda()
            mesh = decoder(latent, return_subs=False)[0]
            decoder.cpu()
            if not len(mesh.faces):
                raise NoShapeError("the model produced no surface; try another photograph or seed")
            output.mkdir(exist_ok=False)
            np.save(output / "vertices.npy", mesh.vertices.cpu().numpy())
            np.save(output / "faces.npy", mesh.faces.cpu().numpy())
    return {"model": "trellis2", "resolution": actual_resolution, "seed": seed,
            "steps": {"structure": steps if steps is not None else pipeline.sparse_structure_sampler_params["steps"],
                      "shape": steps if steps is not None else pipeline.shape_slat_sampler_params["steps"]},
            "tokens": {"structure": structure_tokens, "shape": shape_tokens},
            "stages": stages, "disk_streaming": streaming,
            "peak_gpu_memory_bytes": max(item["peak_gpu_memory_bytes"] for item in stages)}
