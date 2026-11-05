"""Run one TRELLIS preparation, foreground, generation or delivery stage.

The Rust worker supplies private paths and starts each stage in a fresh process.
Only the final JSON response goes to stdout; upstream diagnostic output is
discarded because it can contain operator endpoint and filesystem details.
"""

from contextlib import redirect_stdout, redirect_stderr
import errno
import json
import os
from pathlib import Path
import resource
import shutil
import sys
import time

ROOT = Path(__file__).resolve().parent
MANIFEST = ROOT / "models/trellis2.json"
VERSION = "printable-trellis2 1"


def prepare(request):
    from printable_recon.model_cache import Manifest, _download, _verify, prepare as prepare_cache

    manifest = Manifest.read(MANIFEST)
    shared = Path(request["shared_models"]) if request.get("shared_models") else None

    def download(item, directory, endpoint):
        # Reuse matching image dependencies from an operator's existing model
        # volume. Only verified, regular copies enter the managed snapshot.
        if shared is not None and item.directory in ("dinov3", "birefnet"):
            source = shared / item.relative_path
            if _verify(source, item):
                destination = directory / item.filename
                destination.parent.mkdir(parents=True, exist_ok=True)
                shutil.copyfile(source, destination)
                return destination
        return _download(item, directory, endpoint)

    snapshot = prepare_cache(Path(request["cache"]), manifest, download=download)
    return {"snapshot": str(snapshot), "manifest": manifest.identifier}


def foreground(request):
    import torch
    from printable_recon import features
    from printable_recon.trellis_errors import UnavailableError

    photo, _ = features.open_photograph(Path(request["image"]))
    background = "alpha" if photo.mode == "RGBA" else "removed"
    if photo.mode != "RGBA":
        if not torch.cuda.is_available():
            raise UnavailableError("TRELLIS foreground preparation requires a CUDA GPU")
        torch.cuda.set_per_process_memory_fraction(min(1.0, request["vram_budget_bytes"] / torch.cuda.get_device_properties(0).total_memory))
        model = features.load_segmenter(ROOT / "vendor/birefnet", Path(request["snapshot"]) / "birefnet", torch.device("cuda"))
        photo = features.remove_background(features.reduce(photo), model, torch.device("cuda"))
    photo.save(request["cutout"])
    return {"background": background, "peak_gpu_memory_bytes": torch.cuda.max_memory_allocated() if torch.cuda.is_initialized() else 0}


def generate(request):
    from PIL import Image
    from printable_recon.trellis import generate as generate_shape

    cache = Path(request["scratch"]) / "flex-gemm-autotune.json"
    shutil.copyfile(ROOT / "vendor/flex-gemm-autotune.json", cache)
    os.environ.update(FLEX_GEMM_AUTOTUNE_CACHE_PATH=str(cache), FLEX_GEMM_AUTOSAVE_AUTOTUNE_CACHE="0")
    with Image.open(request["cutout"]) as photo:
        photo.load()
        return generate_shape(Path(request["snapshot"]), photo, Path(request["raw"]),
                              resolution=request["resolution"], steps=request.get("steps"), seed=request["seed"],
                              vram_budget_bytes=request["vram_budget_bytes"])


def deliver(request):
    from printable_recon.mesh_delivery import deliver as deliver_mesh

    return deliver_mesh(Path(request["raw"]), Path(request["mesh"]), Path(request["scratch"]),
                        resolution=request["resolution"], size_mm=request["size_mm"],
                        target_triangles=request.get("target_triangles"))


STAGES = {"prepare": prepare, "foreground": foreground, "generate": generate, "deliver": deliver}


def main(argv):
    if argv == ["--version"]:
        print(VERSION)
        return 0
    if len(argv) != 2 or argv[0] not in STAGES:
        print(json.dumps({"error": {"code": "validation", "message": "expected a TRELLIS stage and request file"}}))
        return 2
    os.environ.setdefault("PYTORCH_CUDA_ALLOC_CONF", "expandable_segments:True")
    sys.path[:0] = [str(ROOT), str(ROOT / "vendor/trellis2")]
    from printable_recon.model_cache import PreparationError
    from printable_recon.trellis_errors import NoShapeError, UnavailableError
    started = time.monotonic()
    try:
        request = json.loads(Path(argv[1]).read_text(encoding="utf-8"))
        with open(os.devnull, "w") as diagnostics, redirect_stdout(diagnostics), redirect_stderr(diagnostics):
            result = STAGES[argv[0]](request)
        # Linux reports ru_maxrss in KiB. This measures the process high-water
        # mark, including model loading and mapped checkpoint pages.
        result.update(seconds=round(time.monotonic() - started, 3),
                      peak_ram_bytes=resource.getrusage(resource.RUSAGE_SELF).ru_maxrss * 1024)
        print(json.dumps({"result": result}))
        return 0
    except PreparationError as error:
        print(json.dumps({"error": {"code": "checkpoints", "message": str(error)}}))
        return 2
    except (NoShapeError, UnavailableError) as error:
        print(json.dumps({"error": {"code": "no_shape" if isinstance(error, NoShapeError) else "unavailable", "message": str(error)}}))
        return 2
    except (OSError, ValueError, KeyError, MemoryError, RuntimeError) as error:
        if isinstance(error, OSError) and error.errno in (errno.ENOSPC, errno.EDQUOT):
            code, message = "resource_limit", "insufficient disk space for TRELLIS models, scratch data or output"
        elif isinstance(error, MemoryError) or "out of memory" in str(error).lower():
            code, message = "resource_limit", "the TRELLIS stage exceeded available memory; reduce resolution or free resources"
        else:
            code, message = "generation", "the TRELLIS stage failed; check its model, image and resource settings"
        print(json.dumps({"error": {"code": code, "message": message}}))
        return 2


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
