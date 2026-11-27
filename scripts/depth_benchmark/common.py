"""Shared inputs, execution tiers, resource measurement and the result format.

Every runner writes results/<tier>/<model>/<image>.npz with:
  depth    float32 (H, W) at the input's size; metres for metric models
  kind     "metric", "relative" or "logrel" (see scoring.KINDS)
  normals  float32 (H, W, 3), optional; camera frame x right, y up, z towards camera
  focal_px float, optional; focal length estimated by the model, in input pixels
  mask     bool (H, W), optional; pixels the model reports as valid
and appends one line per image to results/<tier>/runs.jsonl with time, peak
GPU memory, peak resident memory and bytes read from storage. Runs at 2048
pixels log to results/timing-<tier>/. Each run also records the images it was
asked for (expected.jsonl) and any failure (failures.jsonl), so a run that
stopped early is visible. A tier's directory holds only that tier's latest run.

Tiers:
  gpu     the GPU with no memory limit
  budget  the GPU limited to BENCH_GPU_BUDGET_GB (default 3) of allocator memory
  cpu     no GPU; BENCH_CPU_THREADS threads (default: all cores)
"""

from __future__ import annotations

import json
import os
import resource
import time
from pathlib import Path
from typing import TYPE_CHECKING

import numpy as np

if TYPE_CHECKING:
    from PIL import Image

ROOT = Path(os.environ.get("BENCH_ROOT", "/bench"))
IMAGES = ROOT / "images"
GT = ROOT / "work" / "gt"
INPUTS = ROOT / "work" / "inputs"
INPUTS_2048 = ROOT / "work" / "inputs2048"
RESULTS = ROOT / "results"
TIERS = ("gpu", "budget", "cpu")
TIMING_IMAGES = ("02-dog-fur", "03-marble-bust", "14-chess-clutter")


def input_side() -> int:
    return int(os.environ.get("BENCH_SIDE", "1024"))


def prepare_inputs() -> list[str]:
    """Write every photograph and rendered scene at one shared size, so all models see identical pixels.

    Inputs are rewritten on every call and earlier inputs removed first, so a
    re-rendered scene, replaced photograph or removed file never leaves a stale
    input behind.
    """
    for directory in (INPUTS, INPUTS_2048):
        for old in directory.glob("*.png"):
            old.unlink()
    INPUTS.mkdir(parents=True, exist_ok=True)
    side = input_side()
    names = []
    for src in sorted(IMAGES.glob("*.jpg")) + sorted(GT.glob("*/rgb.png")):
        name = f"gt-{src.parent.name}" if src.name == "rgb.png" else src.stem
        _resized(src, side).save(INPUTS / f"{name}.png")
        names.append(name)
    INPUTS_2048.mkdir(parents=True, exist_ok=True)
    for name in TIMING_IMAGES:
        src = IMAGES / f"{name}.jpg"
        if src.exists():
            _resized(src, 2048, upscale=True).save(INPUTS_2048 / f"{name}.png")
    return names


def _resized(src: Path, side: int, upscale: bool = False) -> Image.Image:
    from PIL import Image, ImageOps

    with Image.open(src) as im:
        im = ImageOps.exif_transpose(im).convert("RGB")
        scale = side / max(im.size)
        if scale < 1 or upscale:
            im = im.resize((round(im.width * scale), round(im.height * scale)), Image.LANCZOS)
        return im


def inputs(selection: list[str] | None = None, timing: bool = False) -> list[Path]:
    files = sorted((INPUTS_2048 if timing else INPUTS).glob("*.png"))
    if selection:
        wanted = set(selection)
        files = [f for f in files if f.stem in wanted]
    return files


def load_rgb(path: Path) -> np.ndarray:
    from PIL import Image

    with Image.open(path) as im:
        return np.asarray(im.convert("RGB"))


def resize_to(arr: np.ndarray, h: int, w: int) -> np.ndarray:
    """Bilinear resize of an (H, W) or (H, W, C) float array."""
    import torch

    if arr.shape[:2] == (h, w):
        return arr
    t = torch.from_numpy(np.ascontiguousarray(arr, dtype=np.float32))
    t = t[None, None] if t.ndim == 2 else t.permute(2, 0, 1)[None]
    out = torch.nn.functional.interpolate(t, size=(h, w), mode="bilinear", align_corners=False)[0]
    return (out[0] if arr.ndim == 2 else out.permute(1, 2, 0)).numpy()


def device_for(tier: str):
    """Configure torch for the tier and return the device to run on."""
    import torch

    if tier not in TIERS:
        raise ValueError(f"unknown tier {tier!r}")
    if tier == "cpu":
        threads = int(os.environ.get("BENCH_CPU_THREADS", "0")) or os.cpu_count() or 1
        torch.set_num_threads(threads)
        return torch.device("cpu")
    if not torch.cuda.is_available():
        raise RuntimeError(f"tier {tier} needs a CUDA GPU")
    if tier == "budget":
        budget = float(os.environ.get("BENCH_GPU_BUDGET_GB", "3")) * 2**30
        total = torch.cuda.get_device_properties(0).total_memory
        torch.cuda.set_per_process_memory_fraction(min(1.0, budget / total), 0)
    return torch.device("cuda")


def _read_bytes() -> int:
    """Bytes this process has caused to be read from storage (Linux), or 0 where unavailable."""
    try:
        for line in Path("/proc/self/io").read_text().splitlines():
            if line.startswith("read_bytes:"):
                return int(line.split()[1])
    except OSError:
        pass
    return 0


class Measure:
    """Wall time, peak GPU memory, process peak resident memory and storage reads for one call."""

    def __init__(self, device):
        import torch

        self.torch = torch
        self.cuda = device.type == "cuda"

    def __enter__(self):
        if self.cuda:
            self.torch.cuda.synchronize()
            self.torch.cuda.reset_peak_memory_stats()
        self.read0 = _read_bytes()
        self.start = time.perf_counter()
        return self

    def __exit__(self, *exc):
        if self.cuda:
            self.torch.cuda.synchronize()
        self.seconds = time.perf_counter() - self.start
        self.gpu_peak = int(self.torch.cuda.max_memory_allocated()) if self.cuda else 0
        self.rss_peak = resource.getrusage(resource.RUSAGE_SELF).ru_maxrss * 1024  # kilobytes on Linux
        self.read_bytes = _read_bytes() - self.read0


def log_set(tier: str, timing: bool) -> str:
    """The results directory of one tier's quality runs or timing runs."""
    return f"timing-{tier}" if timing else tier


def _log(directory: str, name: str, line: dict) -> None:
    (RESULTS / directory).mkdir(parents=True, exist_ok=True)
    with open(RESULTS / directory / name, "a") as f:
        f.write(json.dumps(line) + "\n")
    print(json.dumps(line), flush=True)


def save(tier, model, image, depth, kind, m: Measure, normals=None, focal_px=None, mask=None, extra=None) -> None:
    out = RESULTS / tier / model
    out.mkdir(parents=True, exist_ok=True)
    data = {"depth": np.asarray(depth, np.float32), "kind": kind}
    if normals is not None:
        n = np.asarray(normals, np.float32)
        n /= np.maximum(np.linalg.norm(n, axis=-1, keepdims=True), 1e-8)
        data["normals"] = n
    if focal_px is not None:
        data["focal_px"] = float(focal_px)
    if mask is not None:
        data["mask"] = np.asarray(mask, bool)
    np.savez_compressed(out / f"{image}.npz", **data)
    _log(tier, "runs.jsonl", {
        "tier": tier, "model": model, "image": image, "side": int(max(np.shape(depth))),
        "seconds": round(m.seconds, 3), "gpu_peak_gb": round(m.gpu_peak / 2**30, 3),
        "rss_peak_gb": round(m.rss_peak / 2**30, 3), "read_gb": round(m.read_bytes / 2**30, 3),
        "focal_px": None if focal_px is None else round(float(focal_px), 1), **(extra or {}),
    })


def log_timing(tier, model, image, side, m: Measure) -> None:
    _log(log_set(tier, True), "runs.jsonl", {
        "tier": tier, "model": model, "image": image, "side": side, "seconds": round(m.seconds, 3),
        "gpu_peak_gb": round(m.gpu_peak / 2**30, 3), "rss_peak_gb": round(m.rss_peak / 2**30, 3),
        "read_gb": round(m.read_bytes / 2**30, 3),
    })


def log_failure(tier, model, image, error: BaseException, timing: bool = False) -> None:
    _log(log_set(tier, timing), "failures.jsonl", {"tier": tier, "model": model, "image": image,
                            "error": type(error).__name__, "message": str(error)[:300]})


def run_images(tier: str, model: str, run, device, selection=None, timing=False) -> None:
    """Run a model over the inputs with one unrecorded warm-up, recording each result or failure."""
    files = inputs(selection, timing)
    _log(log_set(tier, timing), "expected.jsonl", {"tier": tier, "model": model, "images": [f.stem for f in files]})
    if not files:
        return
    try:
        run(load_rgb(files[0]))
    except Exception as error:  # recorded so the tier table shows why a model does not run
        log_failure(tier, model, "warm-up", error, timing)
        return
    for f in files:
        rgb = load_rgb(f)
        try:
            with Measure(device) as m:
                result = run(rgb)
        except Exception as error:  # an out-of-memory or unsupported-device error is a result here
            log_failure(tier, model, f.stem, error, timing)
            continue
        if timing:
            log_timing(tier, model, f.stem, int(max(rgb.shape[:2])), m)
            continue
        depth, kind, normals, focal, extra = result
        extra = dict(extra)
        mask = extra.pop("mask", None)
        h, w = rgb.shape[:2]
        depth = resize_to(depth, h, w)
        if normals is not None:
            normals = resize_to(normals, h, w)
        if mask is not None:
            mask = resize_to(np.asarray(mask, np.float32), h, w) > 0.5
        save(tier, model, f.stem, depth, kind, m, normals, focal, mask, extra)
