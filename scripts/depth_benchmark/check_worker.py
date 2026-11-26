"""Check the photograph worker's geometry runners against the depth benchmark.

Usage, inside the worker image:
  python check_worker.py <snapshot> <marigold> <inputs> <benchmark-results> <output.json> [entry:device:side ...]

<snapshot> holds each registry entry's files under their registry directory;
<marigold> is the converted Marigold V2 directory; <inputs> holds the 1024 px
benchmark inputs and <benchmark-results> the benchmark's results directory,
with gpu, budget and cpu subdirectories. Each run is a separate
estimate_geometry.py process with the GPU budget the route needs, as the
worker would start it. For every input the outputs are compared with the
benchmark's maps for the same model: median relative depth difference
(after a scale fit for relative depth), valid-pixel agreement, focal length
difference and median normal angle. The default is every route option in
models/geometry.json. CHECK_GPU_BUDGET_MIB, when set, replaces each CUDA
route's budget, so its true peak reserved memory can be measured.
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


def compare(ours: dict, theirs) -> dict:
    out = {}
    if "depth" in ours and "depth" in theirs:
        a, b = ours["depth"], numpy.asarray(theirs["depth"], numpy.float32)
        both = numpy.isfinite(a) & numpy.isfinite(b) & (a > 0) & (b > 0)
        if "mask" in theirs:
            both &= numpy.asarray(theirs["mask"], bool)
        if ours.get("depth_kind") == "relative":
            a = a * numpy.median(b[both] / a[both])
        out["depth_median_rel_diff"] = round(float(numpy.median(numpy.abs(a[both] - b[both]) / b[both])), 5)
    if "valid" in ours and "mask" in theirs:
        mask = numpy.asarray(theirs["mask"], bool)
        union = (ours["valid"] | mask).sum()
        out["valid_agreement"] = round(float((ours["valid"] & mask).sum() / max(union, 1)), 5)
    if "focal_px" in ours and "focal_px" in theirs:
        theirs_focal = float(theirs["focal_px"])
        out["focal_rel_diff"] = round(abs(ours["focal_px"] - theirs_focal) / theirs_focal, 5)
    if "normals" in ours and "normals" in theirs:
        cos = numpy.clip((ours["normals"] * numpy.asarray(theirs["normals"])).sum(-1), -1, 1)
        out["normal_median_deg"] = round(float(numpy.degrees(numpy.median(numpy.arccos(cos)))), 3)
    return out


def read_outputs(directory: Path, result: dict) -> dict:
    ours = {key: result[key] for key in ("depth_kind", "focal_px") if key in result}
    if "depth" in result["outputs"]:
        ours["depth"] = numpy.load(directory / "depth.npy", allow_pickle=False)
    if "valid" in result["outputs"]:
        with Image.open(directory / "valid.png") as image:
            ours["valid"] = numpy.asarray(image) > 127
    if "normals" in result["outputs"]:
        ours["normals"] = _read_png16(directory / "normals.png").astype(numpy.float32) / 65535 * 2 - 1
    return ours


def _read_png16(path: Path):
    """An H x W x 3 uint16 array from the worker's unfiltered 16-bit RGB PNG."""
    import struct
    import zlib

    data, at, chunks = path.read_bytes(), 8, {}
    while at < len(data):
        length, kind = struct.unpack(">I4s", data[at:at + 8])
        chunks.setdefault(kind, []).append(data[at + 8:at + 8 + length])
        at += 12 + length
    width, height = struct.unpack(">II", chunks[b"IHDR"][0][:8])
    rows = numpy.frombuffer(zlib.decompress(b"".join(chunks[b"IDAT"])), numpy.uint8).reshape(height, 1 + width * 6)
    if rows[:, 0].any():
        raise ValueError("expected unfiltered rows")
    return rows[:, 1:].copy().view(">u2").reshape(height, width, 3).astype(numpy.uint16)


def main(argv):
    snapshot, marigold, inputs, results, output = (Path(a) for a in argv[:5])
    registry = json.loads((RECON / "models" / "geometry.json").read_text())
    if argv[5:]:
        options = [dict(zip(("entry", "device", "side"), item.split(":"))) for item in argv[5:]]
        for option in options:
            option["side"] = int(option["side"])
    else:
        options = [o for route in registry["routes"].values() for o in route]
    seen, report = set(), []
    for option in options:
        key = (option["entry"], option["device"], option["side"])
        if key in seen:
            continue
        seen.add(key)
        budget = next((o.get("gpu_peak_mib") for route in registry["routes"].values() for o in route
                       if (o["entry"], o["device"], o["side"]) == key), None)
        tier = "cpu" if option["device"] == "cpu" else ("budget" if budget and budget <= 3072 else "gpu")
        if option["device"] == "cuda" and os.environ.get("CHECK_GPU_BUDGET_MIB"):
            budget = int(os.environ["CHECK_GPU_BUDGET_MIB"])
        for image in sorted(inputs.glob("*.png")):
            with tempfile.TemporaryDirectory() as temporary:
                work = Path(temporary)
                request = {"image": str(image), "entry": option["entry"], "device": option["device"],
                           "side": option["side"], "gpu_budget_mib": budget,
                           "snapshot": str(marigold if option["entry"] == "marigold-v2" else snapshot),
                           "outputs": {"depth": str(work / "depth.npy"), "valid": str(work / "valid.png"),
                                       "normals": str(work / "normals.png")}}
                (work / "request.json").write_text(json.dumps(request))
                run = subprocess.run([PYTHON, "-I", "-B", str(RECON / "estimate_geometry.py"), str(work / "request.json")],
                                     capture_output=True, text=True, check=False)
                try:
                    printed = json.loads(run.stdout.strip().splitlines()[-1])
                except (IndexError, ValueError):
                    printed = {"error": {"code": "crash", "message": run.stderr[-2000:]}}
                row = {"entry": key[0], "device": key[1], "side": key[2], "image": image.stem, "budget_mib": budget}
                if "result" in printed:
                    result = printed["result"]
                    row.update({k: result.get(k) for k in ("seconds", "load_seconds", "gpu_peak_allocated_mib",
                                                          "gpu_peak_reserved_mib", "valid_fraction", "focal_px")})
                    reference = results / tier / key[0] / f"{image.stem}.npz"
                    if reference.exists():
                        row.update(compare(read_outputs(work, result), numpy.load(reference)))
                else:
                    row["error"] = printed["error"]
                print(json.dumps(row), flush=True)
                report.append(row)
    output.write_text(json.dumps(report, indent=1))


if __name__ == "__main__":
    main(sys.argv[1:])
