"""Run the photograph analysis on the depth benchmark inputs and report its cost and scores.

Usage, inside the worker image:
  python check_analysis.py <snapshot> <marigold> <inputs> <benchmark-results> <output.json> [settings ...]

<snapshot> holds each registry entry's files under their registry directory;
<marigold> is the converted Marigold V2 directory; <inputs> holds the 1024 px
benchmark inputs and <benchmark-results> the benchmark's results directory,
with gpu, budget and cpu subdirectories. Each settings argument is a JSON
object for the analysis, for example '{"subject": "deep", "quality": "fast"}';
the default is shallow and deep subjects at both qualities, a physical shallow
analysis, and a shallow analysis with normals. Each analysis is a separate
analyze_photo.py process with the GPU budget in CHECK_GPU_BUDGET_MIB (default
3072), as the worker would start it. For every input the report gives the
time, the peak GPU memory, the routes, the summary scores and, where the
benchmark kept a mask from the same model at the same size, the intersection
over union of the subject masks.
"""

import json
import os
import subprocess
import sys
import tempfile
from pathlib import Path

import numpy
from PIL import Image

RECON = Path("/opt/printable/recon")
PYTHON = "/opt/recon/bin/python"
DEFAULT_SETTINGS = [{"subject": "shallow"}, {"subject": "shallow", "quality": "fast"},
                    {"subject": "deep"}, {"subject": "deep", "quality": "fast"},
                    {"subject": "shallow", "interpretation": "physical"},
                    {"subject": "shallow", "normals": True}]


def mask_agreement(directory: Path, manifest: dict, results: Path, stem: str, budget: int):
    mask_route = manifest["routes"]["mask"]
    tier = "cpu" if mask_route["device"] == "cpu" else ("budget" if budget <= 3072 else "gpu")
    reference = results / tier / mask_route["entry"] / f"{stem}.npz"
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
    presets = [json.loads(value) for value in argv[5:]] or DEFAULT_SETTINGS
    budget = int(os.environ.get("CHECK_GPU_BUDGET_MIB", "3072"))
    registry = json.loads((RECON / "models" / "geometry.json").read_text())
    prepared = {name: {"snapshot": str(snapshot)} for name in registry["entries"]}
    prepared["marigold-v2"]["converted"] = str(marigold)
    report = []
    for settings in presets:
        for image in sorted(inputs.glob("*.png")):
            with tempfile.TemporaryDirectory() as temporary:
                work = Path(temporary)
                (work / "maps").mkdir()
                request = {"image": str(image), "settings": settings, "gpu_budget_mib": budget,
                           "ram_budget_mib": None, "threads": 8, "prepared": prepared, "output": str(work / "maps")}
                (work / "request.json").write_text(json.dumps(request))
                run = subprocess.run([PYTHON, "-I", "-B", str(RECON / "analyze_photo.py"), str(work / "request.json")],
                                     capture_output=True, text=True, check=False)
                try:
                    printed = json.loads(run.stdout.strip().splitlines()[-1])
                except (IndexError, ValueError):
                    printed = {"error": {"code": "crash", "message": run.stderr[-2000:]}}
                row = {"settings": settings, "image": image.stem, "budget_mib": budget}
                if "result" in printed:
                    manifest = printed["result"]["manifest"]
                    row.update({key: manifest.get(key) for key in ("seconds", "gpu_peak_allocated_mib", "timings",
                                                                  "scores", "focal_length", "warnings")})
                    row["routes"] = {role: [route["entry"], route["device"], route["side"]]
                                     for role, route in manifest["routes"].items()}
                    row["mask_iou_with_benchmark"] = mask_agreement(work / "maps", manifest, results, image.stem, budget)
                else:
                    row["error"] = printed["error"]
                print(json.dumps(row), flush=True)
                report.append(row)
    output.write_text(json.dumps(report, indent=1))


if __name__ == "__main__":
    main(sys.argv[1:])
