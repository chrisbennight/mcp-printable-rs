"""Analyse one photograph with the geometry models: depth, normals, subject mask,
focal length and a confidence map with summary scores.

Usage: analyze_photo.py <request.json>
       analyze_photo.py --version

The request holds:

- "settings": "subject" (shallow or deep), "quality" (best adds mirrored and
  rescaled consistency passes of the depth model; fast skips them),
  "depth_model", "mask_model", "focal_model" and "normals_model" (names in
  models/geometry.json; focal_model is null when "focal_length_px" gives a
  known focal length in photograph pixels, normals_model null for normals
  derived from depth), "side" (1024 or 2048, the depth model's processing
  size) and "device" (cuda or cpu). Every key is required; the server fills
  the recommended defaults;
- "image": the photograph;
- "prepared": the prepare_geometry.py answer's "models" object, naming each
  model's verified snapshot and, for a converted model, its converted form;
- "threads": CPU threads for numerical work;
- "output": an existing, empty directory for the maps.

The models run one at a time in this process, each loaded once and released
before the next. The maps are written as printable_recon.photo_analysis
describes and the manifest is printed: {"result": {"manifest": ...}} or
{"error": {"code", "message"}}. The manifest reports peak GPU memory, so
callers can see what a choice costs.
"""

from contextlib import redirect_stdout
import gc
import json
import sys
import time
from pathlib import Path

VERSION = "printable-photo-analysis 1"
ROOT = Path(__file__).resolve().parent
MODELS = ROOT / "models/geometry.json"


class Failure(Exception):
    """A failure reported with its protocol code."""

    def __init__(self, code: str, message: str):
        super().__init__(message)
        self.code, self.message = code, message


def main(argv: list[str]) -> int:
    if argv == ["--version"]:
        print(VERSION)
        return 0
    sys.path.insert(0, str(ROOT))
    from printable_recon import geometry_files, photo_analysis

    if len(argv) != 1:
        return _fail("validation", "expected one request file")
    try:
        models = geometry_files.read(MODELS)
    except (OSError, ValueError):
        return _fail("unavailable", "the geometry model list could not be read")
    try:
        request = json.loads(Path(argv[0]).read_text(encoding="utf-8"))
        if not isinstance(request, dict):
            raise ValueError("the request must be an object")
        settings = photo_analysis.Settings.parse(request.get("settings"), models)
        chosen = photo_analysis.plan(settings)
        image_path = Path(request["image"])
        output = Path(request["output"])
        prepared = request["prepared"]
        threads = request.get("threads")
        snapshots = {}
        for entry in chosen.entries:
            files = prepared[entry]
            snapshots[entry] = Path(files.get("converted") or files["snapshot"])
        if threads is not None and (isinstance(threads, bool) or not isinstance(threads, int) or threads < 1):
            raise ValueError("threads must be a positive integer")
        if not output.is_dir() or any(output.iterdir()):
            raise ValueError("output must be an existing, empty directory")
    except (OSError, ValueError, KeyError, TypeError, AttributeError) as error:
        return _fail("validation", f"the request is invalid: {error}")
    return _analyse(chosen, settings, image_path, snapshots, threads, output)


def _analyse(chosen, settings, image_path: Path, snapshots: dict, threads, output: Path) -> int:
    import torch
    from PIL import Image

    from estimate_depth import _open
    from printable_recon import photo_analysis

    if threads is not None:
        torch.set_num_threads(threads)
    uses_gpu = settings.device == "cuda"
    if uses_gpu and not torch.cuda.is_available():
        return _fail("unavailable", "no CUDA GPU is available to this worker")
    try:
        photograph = _open(image_path)
    except (OSError, ValueError, Image.DecompressionBombError) as error:
        return _fail("validation", f"the photograph could not be read: {error}")

    started = time.monotonic()
    # Model code may print progress; only the final answer goes to stdout.
    try:
        with redirect_stdout(sys.stderr):
            outcomes, timings = _run_models(chosen, photograph, snapshots, uses_gpu)
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


def _run_models(chosen, photograph, snapshots: dict, uses_gpu: bool):
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
            focal_fraction = (settings.focal_length_px / photograph.width
                              if settings.focal_length_px is not None else None)
            runner = geometry_models.load(run.entry, snapshots[run.entry], torch.device(run.device), focal_fraction)
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
                          f"the GPU ran out of memory running {run.entry} at {run.side} px") from None
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
