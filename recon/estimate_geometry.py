"""Run one photograph geometry model on one photograph.

Usage: estimate_geometry.py <request.json>
       estimate_geometry.py --version

The request names the photograph, the registry entry, the verified model
snapshot, the device ("cuda" or "cpu"), the longest side to process at, the
GPU memory budget in MiB for a CUDA run, and the files to write:

- "depth": a NumPy .npy file of float32 depth, H x W;
- "valid": an 8-bit greyscale PNG, 255 where the depth is measured surface;
- "normals": a 16-bit RGB PNG of unit normals in the camera frame, x right,
  y up, z towards the camera, each component c stored as (c + 1) / 2 * 65535.

Outputs the model does not produce are not written. One JSON object is
printed: {"result": ...} or {"error": {"code", "message"}}. A CUDA run is
held to its budget: an allocation past it fails as resource_limit rather
than taking memory another worker may be using.
"""

import json
import sys
import time
from pathlib import Path

VERSION = "printable-geometry-run 1"
SIDES = (1024, 2048)


def main(argv: list[str]) -> int:
    if argv == ["--version"]:
        print(VERSION)
        return 0
    sys.path.insert(0, str(Path(__file__).resolve().parent))
    if len(argv) != 1:
        return _fail("validation", "expected one request file")
    try:
        request = json.loads(Path(argv[0]).read_text(encoding="utf-8"))
        image_path = Path(request["image"])
        entry = str(request["entry"])
        snapshot = Path(request["snapshot"])
        device_name = str(request["device"])
        side = int(request["side"])
        budget = request.get("gpu_budget_mib")
        threads = request.get("threads")
        targets = {key: Path(request["outputs"][key]) for key in ("depth", "valid", "normals")
                   if request.get("outputs", {}).get(key)}
    except (OSError, ValueError, KeyError, TypeError) as error:
        return _fail("validation", f"the request is invalid: {error}")
    if device_name not in ("cuda", "cpu") or side not in SIDES:
        return _fail("validation", "device must be cuda or cpu and side one of 1024 or 2048")
    if device_name == "cuda" and (isinstance(budget, bool) or not isinstance(budget, int) or budget < 1):
        return _fail("validation", "a CUDA run needs a positive gpu_budget_mib")
    if threads is not None and (isinstance(threads, bool) or not isinstance(threads, int) or threads < 1):
        return _fail("validation", "threads must be a positive integer")

    import torch
    from PIL import Image

    from estimate_depth import _open, write_normals
    from printable_recon import geometry_models

    if threads is not None:
        torch.set_num_threads(threads)
    if device_name == "cuda":
        if not torch.cuda.is_available():
            return _fail("unavailable", "no CUDA GPU is available to this worker")
        total = torch.cuda.get_device_properties(0).total_memory
        torch.cuda.set_per_process_memory_fraction(min(1.0, budget * 2**20 / total))
    device = torch.device(device_name)
    try:
        rgb = scaled(_open(image_path), side)
    except (OSError, ValueError, Image.DecompressionBombError) as error:
        return _fail("validation", f"the photograph could not be read: {error}")

    started = time.monotonic()
    try:
        run = geometry_models.load(entry, snapshot, device)
        loaded = time.monotonic()
        geometry = run(rgb)
    except geometry_models.ModelLoadError as error:
        return _fail("checkpoints", str(error))
    except torch.cuda.OutOfMemoryError:
        return _fail("resource_limit", f"{entry} needs more than the {budget} MiB GPU budget at {side} px")
    except MemoryError:
        return _fail("resource_limit", "the worker ran out of memory")
    finished = time.monotonic()

    result = {"entry": entry, "device": device_name, "width": int(rgb.shape[1]), "height": int(rgb.shape[0]),
              "load_seconds": round(loaded - started, 3), "seconds": round(finished - loaded, 3),
              "outputs": []}
    if geometry.depth is not None and "depth" in targets:
        import numpy

        # Through a file object, so the requested name is kept without an added .npy suffix.
        with targets["depth"].open("wb") as sink:
            numpy.save(sink, geometry.depth, allow_pickle=False)
        result["depth_kind"] = geometry.depth_kind
        result["outputs"].append("depth")
    if geometry.valid is not None and "valid" in targets:
        Image.fromarray((geometry.valid * 255).astype("uint8")).save(targets["valid"], format="PNG")
        result["valid_fraction"] = round(float(geometry.valid.mean()), 4)
        result["outputs"].append("valid")
    if geometry.normals is not None and "normals" in targets:
        write_normals(geometry.normals.transpose(2, 0, 1), targets["normals"])
        result["outputs"].append("normals")
    if geometry.focal_px is not None:
        result["focal_px"] = round(geometry.focal_px, 3)
    if device_name == "cuda":
        result["gpu_peak_allocated_mib"] = round(torch.cuda.max_memory_allocated() / 2**20)
        result["gpu_peak_reserved_mib"] = round(torch.cuda.max_memory_reserved() / 2**20)
    print(json.dumps({"result": result}))
    return 0


def scaled(image, side: int):
    """The photograph sized as the benchmark measured each route.

    At 1024 px a larger photograph is scaled down and a smaller one is kept.
    At 2048 px the photograph is scaled to that side either way: Depth Pro 2
    measured more accurate on the upscaled scenes.
    """
    import numpy
    from PIL import Image

    scale = side / max(image.size)
    if scale < 1 or (scale > 1 and side == 2048):
        image = image.resize((round(image.width * scale), round(image.height * scale)), Image.LANCZOS)
    return numpy.asarray(image, dtype=numpy.uint8)


def _fail(code: str, message: str) -> int:
    print(json.dumps({"error": {"code": code, "message": message}}))
    return 2


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
