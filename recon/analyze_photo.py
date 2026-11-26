"""Analyse one photograph with the geometry models: depth, normals, subject mask,
focal length and a confidence map with summary scores.

Usage: analyze_photo.py --plan <request.json>
       analyze_photo.py <request.json>
       analyze_photo.py --version

--plan reads {"settings": {...}, "gpu_budget_mib": int | null,
"ram_budget_mib": int | null} and reports the routes and model runs the
analysis would use, and the registry entries to prepare. It loads no model.

A run reads the same fields plus:

- "image": the photograph;
- "prepared": the prepare_geometry.py answer's "entries" object, naming each
  entry's verified snapshot and, for a converted entry, its converted model;
- "threads": CPU threads for numerical work;
- "output": an existing, empty directory for the maps.

Settings are "subject" (shallow or deep), "quality" (best or fast),
"interpretation" (physical or sculpture, shallow subjects only), "normals"
(true for a predicted normal map) and "focal_length_px" (a known focal length
in pixels of the photograph).

The models run one at a time in this process, each loaded once and released
before the next, under one GPU budget for the whole process. The maps are
written as printable_recon.photo_analysis describes and the manifest is
printed: {"result": {"manifest": ...}} or {"error": {"code", "message"}}.
"""

from contextlib import redirect_stdout
import gc
import json
import sys
import time
from pathlib import Path

VERSION = "printable-photo-analysis 1"
ROOT = Path(__file__).resolve().parent
REGISTRY = ROOT / "models/geometry.json"


class Failure(Exception):
    """A failure reported with its protocol code."""

    def __init__(self, code: str, message: str):
        super().__init__(message)
        self.code, self.message = code, message


def _budget(request: dict, key: str):
    value = request.get(key)
    if value is not None and (isinstance(value, bool) or not isinstance(value, int) or value < 1):
        raise ValueError(f"{key} must be a positive integer or null")
    return value


def main(argv: list[str]) -> int:
    if argv == ["--version"]:
        print(VERSION)
        return 0
    sys.path.insert(0, str(ROOT))
    from printable_recon import photo_analysis
    from printable_recon.registry import Host, NoRoute, Registry, RegistryError

    planning = len(argv) == 2 and argv[0] == "--plan"
    if not planning and len(argv) != 1:
        return _fail("validation", "expected --plan <request> or <request>")
    try:
        request = json.loads(Path(argv[-1]).read_text(encoding="utf-8"))
        if not isinstance(request, dict):
            raise ValueError("the request must be an object")
        settings = photo_analysis.Settings.parse(request.get("settings"))
        host = Host(_budget(request, "gpu_budget_mib"), _budget(request, "ram_budget_mib"))
    except (OSError, ValueError) as error:
        return _fail("validation", f"the request is invalid: {error}")
    try:
        registry = Registry.read(REGISTRY)
    except (OSError, ValueError, RegistryError):
        return _fail("unavailable", "the geometry model registry could not be read")
    try:
        chosen = photo_analysis.plan(settings, registry, host)
    except NoRoute as error:
        return _fail("unavailable", str(error))
    if planning:
        print(json.dumps({"result": chosen.as_dict()}))
        return 0
    try:
        image_path = Path(request["image"])
        output = Path(request["output"])
        prepared = request["prepared"]
        threads = request.get("threads")
        models = {}
        for entry in chosen.entries:
            files = prepared[entry]
            models[entry] = Path(files.get("converted") or files["snapshot"])
        if threads is not None and (isinstance(threads, bool) or not isinstance(threads, int) or threads < 1):
            raise ValueError("threads must be a positive integer")
        if not output.is_dir() or any(output.iterdir()):
            raise ValueError("output must be an existing, empty directory")
    except (OSError, ValueError, KeyError, TypeError, AttributeError) as error:
        return _fail("validation", f"the request is invalid: {error}")
    return _analyse(chosen, settings, host, image_path, models, threads, output)


def _analyse(chosen, settings, host, image_path: Path, models: dict, threads, output: Path) -> int:
    import torch
    from PIL import Image

    from estimate_depth import _open
    from printable_recon import photo_analysis

    if threads is not None:
        torch.set_num_threads(threads)
    uses_gpu = any(run.device == "cuda" for run in chosen.runs)
    if uses_gpu:
        if host.gpu_budget_mib is None or not torch.cuda.is_available():
            return _fail("unavailable", "no CUDA GPU is available to this worker")
        total = torch.cuda.get_device_properties(0).total_memory
        torch.cuda.set_per_process_memory_fraction(min(1.0, host.gpu_budget_mib * 2**20 / total))
    try:
        photograph = _open(image_path)
    except (OSError, ValueError, Image.DecompressionBombError) as error:
        return _fail("validation", f"the photograph could not be read: {error}")

    started = time.monotonic()
    # Model code may print progress; only the final answer goes to stdout.
    try:
        with redirect_stdout(sys.stderr):
            outcomes, timings = _run_models(chosen, host, photograph, models, uses_gpu)
    except Failure as failure:
        return _fail(failure.code, failure.message)

    try:
        analysis = photo_analysis.combine(chosen, settings, outcomes, photograph.width)
        manifest = photo_analysis.write_all(analysis, output)
    except (ValueError, OSError) as error:
        return _fail("failed", f"the analysis could not be completed: {error}")
    manifest["analyzer"] = VERSION
    manifest["photograph"] = {"width": photograph.width, "height": photograph.height}
    manifest["timings"] = timings
    manifest["seconds"] = round(time.monotonic() - started, 3)
    if uses_gpu:
        manifest["gpu_peak_allocated_mib"] = round(torch.cuda.max_memory_allocated() / 2**20)
    print(json.dumps({"result": {"manifest": manifest}}))
    return 0


def _run_models(chosen, host, photograph, models: dict, uses_gpu: bool):
    """Each run's outcome and timing; a failure is raised as Failure."""
    import numpy
    import torch
    from PIL import Image

    from estimate_geometry import scaled
    from printable_recon import geometry_models, photo_analysis

    outcomes, timings = {}, []
    for run in chosen.runs:
        run_started = time.monotonic()
        try:
            rgb = scaled(photograph, run.side)
            height, width = rgb.shape[:2]
            runner = geometry_models.load(run.entry, models[run.entry], torch.device(run.device))
            outcome = photo_analysis.Outcome(runner(rgb), width, height)
            if "flip" in run.passes:
                mirrored = runner(numpy.ascontiguousarray(rgb[:, ::-1]))
                outcome.passes["flip"] = (mirrored.depth[:, ::-1], _valid(mirrored, height, width)[:, ::-1])
            if "rescale" in run.passes:
                small = Image.fromarray(rgb).resize((max(1, round(width * photo_analysis.RESCALE)),
                                                     max(1, round(height * photo_analysis.RESCALE))), Image.LANCZOS)
                reduced = runner(numpy.asarray(small, dtype=numpy.uint8))
                outcome.passes["rescale"] = (
                    photo_analysis.resize_map(reduced.depth, height, width),
                    photo_analysis.resize_mask(_valid(reduced, *reduced.depth.shape), height, width))
        except geometry_models.ModelLoadError as error:
            raise Failure("checkpoints", str(error)) from error
        except torch.cuda.OutOfMemoryError:
            raise Failure("resource_limit",
                          f"{run.entry} needs more than the {host.gpu_budget_mib} MiB GPU budget at {run.side} px") from None
        except MemoryError:
            raise Failure("resource_limit", "the worker ran out of memory") from None
        finally:
            # One model at a time: release this one before loading the next.
            runner = None
            gc.collect()
            if uses_gpu:
                torch.cuda.empty_cache()
        outcomes[run] = outcome
        timings.append({"entry": run.entry, "device": run.device, "side": run.side, "passes": list(run.passes),
                        "seconds": round(time.monotonic() - run_started, 3)})
    return outcomes, timings


def _valid(geometry, height: int, width: int):
    import numpy

    if geometry.valid is None:
        return numpy.ones((height, width), bool)
    return numpy.asarray(geometry.valid, bool)


def _fail(code: str, message: str) -> int:
    print(json.dumps({"error": {"code": code, "message": message}}))
    return 2


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
