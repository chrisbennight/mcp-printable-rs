"""Run the photograph analysis on the depth benchmark inputs and report its cost and scores.

Usage, inside the worker image:
  python check_analysis.py <snapshot> <marigold> <inputs> <benchmark-results> <output.json> [settings ...]

<snapshot> holds each model's files under their directory in
models/geometry.json; <marigold> is the converted Marigold V2 directory;
<inputs> holds the 1024 px benchmark inputs and <benchmark-results> the
benchmark's results directory, with gpu and cpu subdirectories. Each settings
argument is a JSON object of changes to the recommended shallow-subject GPU
settings, for example '{"subject": "deep", "depth_model": "dp2-h",
"focal_model": "dp2-h", "side": 2048}'; the default checks the recommended
shallow and deep settings, a fast shallow analysis, MoGe-3 depth and a
shallow analysis with normals. Each analysis is a separate analyze_photo.py
process, as the worker would start it. For every input the report gives the
time, the peak GPU memory, the model runs, the summary scores and, where the
benchmark kept a mask from the same model at the same size, the intersection
over union of the subject masks.
"""

import json
import subprocess
import sys
import tempfile
from pathlib import Path

import numpy
from PIL import Image

RECON = Path("/opt/printable/recon")
PYTHON = "/opt/recon/bin/python"
RECOMMENDED = {"subject": "shallow", "quality": "best", "depth_model": "da3-mono-large", "mask_model": "moge3-vitl",
               "focal_model": "dp2-l", "normals_model": None, "side": 1024, "device": "cuda",
               "focal_length_px": None}
DEFAULT_CHANGES = [{}, {"quality": "fast"},
                   {"subject": "deep", "depth_model": "dp2-h", "focal_model": "dp2-h", "side": 2048},
                   {"depth_model": "moge3-vitl"}, {"normals_model": "marigold-v2"}]


def mask_agreement(directory: Path, settings: dict, results: Path, stem: str):
    tier = "cpu" if settings["device"] == "cpu" else "gpu"
    reference = results / tier / settings["mask_model"] / f"{stem}.npz"
    if not reference.exists():
        return None
    theirs = numpy.load(reference)
    if "mask" not in theirs:
        return None
    with Image.open(directory / "mask.png") as image:
        ours = numpy.asarray(image).astype(numpy.uint32) == 65535
    theirs = numpy.asarray(theirs["mask"], bool)
    if ours.shape != theirs.shape:
        return None
    return round(float((ours & theirs).sum() / max((ours | theirs).sum(), 1)), 5)


def main(argv):
    snapshot, marigold, inputs, results, output = (Path(a) for a in argv[:5])
    presets = [{**RECOMMENDED, **json.loads(value)} for value in argv[5:]] \
        or [{**RECOMMENDED, **changes} for changes in DEFAULT_CHANGES]
    listed = json.loads((RECON / "models" / "geometry.json").read_text())
    prepared = {name: {"snapshot": str(snapshot)} for name in listed["models"]}
    prepared["marigold-v2"]["converted"] = str(marigold)
    report = []
    for settings in presets:
        for image in sorted(inputs.glob("*.png")):
            with tempfile.TemporaryDirectory() as temporary:
                work = Path(temporary)
                (work / "maps").mkdir()
                request = {"image": str(image), "settings": settings, "threads": 8, "prepared": prepared,
                           "output": str(work / "maps")}
                (work / "request.json").write_text(json.dumps(request))
                run = subprocess.run([PYTHON, "-I", "-B", str(RECON / "analyze_photo.py"), str(work / "request.json")],
                                     capture_output=True, text=True, check=False)
                try:
                    printed = json.loads(run.stdout.strip().splitlines()[-1])
                except (IndexError, ValueError):
                    printed = {"error": {"code": "crash", "message": run.stderr[-2000:]}}
                row = {"settings": settings, "image": image.stem}
                if "result" in printed:
                    manifest = printed["result"]["manifest"]
                    row.update({key: manifest.get(key) for key in ("seconds", "gpu_peak_allocated_mib", "timings",
                                                                  "scores", "focal_length", "warnings")})
                    row["runs"] = manifest["runs"]
                    row["mask_iou_with_benchmark"] = mask_agreement(work / "maps", settings, results, image.stem)
                else:
                    row["error"] = printed["error"]
                print(json.dumps(row), flush=True)
                report.append(row)
    output.write_text(json.dumps(report, indent=1))


if __name__ == "__main__":
    main(sys.argv[1:])
