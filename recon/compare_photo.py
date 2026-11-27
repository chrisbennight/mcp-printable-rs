"""Measure maps and meshes: against each other, against a photograph's analysis,
or against a synthetic scene's exact depth and normals.

Usage: compare_photo.py <request.json>

The request holds:

- "mode": "outputs", "photo" or "synthetic";
- "inputs": a list of {"name", "file"}: the workspace path reported for the
  input and its private copy, a map or a mesh rendered by the render worker;
- "kind": "metric", "relative" or "logrel", the depth kind of every map;
- "analysis": for photo, {"depth", "normals", "mask"}: the analysis maps;
- "photograph": for photo, the photograph the analysis was made from;
- "truth": for synthetic, the scene rendered by the render worker;
- "output": an existing, empty directory for the sheet and difference maps.

One JSON object is printed: {"result": {...}} or {"error": {"code", "message"}}.
The result holds "results", per input, and the "files" written to the output
directory. Every measure is reported separately; nothing is combined into one
score.
"""

import json
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent
MODES = ("outputs", "photo", "synthetic")


def main(argv: list[str]) -> int:
    sys.path.insert(0, str(ROOT))
    import numpy

    import scoring
    from printable_recon import comparison

    if len(argv) != 1:
        return _fail("validation", "expected one request file")
    try:
        request = json.loads(Path(argv[0]).read_text(encoding="utf-8"))
        mode, kind, output = request["mode"], request["kind"], Path(request["output"])
        if mode not in MODES:
            raise ValueError(f"mode must be one of {', '.join(MODES)}")
        if kind not in scoring.KINDS:
            raise ValueError(f"kind must be one of {', '.join(scoring.KINDS)}")
        if not output.is_dir() or any(output.iterdir()):
            raise ValueError("output must be an existing, empty directory")
        entries = request["inputs"]
        if not isinstance(entries, list) or not all(isinstance(e, dict) for e in entries):
            raise ValueError("inputs must be a list of objects")
    except (OSError, ValueError, KeyError, TypeError) as error:
        return _fail("validation", f"the request is invalid: {error}")

    try:
        items = [comparison.load(entry["name"], Path(entry["file"]), kind) for entry in entries]
        files = {}
        if mode == "outputs":
            results, differences, size = comparison.compare_outputs(items)
            fitted = [comparison.resized(item, size["height"], size["width"]) for item in items]
            reference = fitted[0]
            rows = [[(item.name, comparison.grey(item.depth, item.valid)) for item in fitted]]
            difference_row = []
            for index, (result, difference) in enumerate(zip(results, differences), start=1):
                name = f"diff-{index}.tiff"
                _write_float(difference.astype(numpy.float32), output / name)
                files[f"difference_{index}"] = name
                difference_row.append((f"{result['input']} - {reference.name}",
                                       comparison.diverging(difference, reference.valid)))
            rows.append(difference_row)
        elif mode == "photo":
            analysis = request["analysis"]
            maps = comparison.load_analysis(Path(analysis["depth"]), Path(analysis["normals"]),
                                            Path(analysis["mask"]))
            height, width = maps["mask"].shape
            luminance = comparison.load_photograph(Path(request["photograph"]), height, width)
            results, views = comparison.compare_photo(items, maps, luminance)
            rows = [[("photograph", comparison.grey(-luminance, numpy.ones(luminance.shape, bool)))]]
            for result, view in zip(results, views):
                rows.append([
                    (f"{result['input']}: silhouette", comparison.overlay(view["silhouette"], view["mask"])),
                    ("shaded", comparison.shaded(view["normals"])),
                    ("normal angle, 0 to 45 deg", comparison.heat(view["angles"], view["mask"], 45.0)),
                ])
        else:
            truth = comparison.load_truth(Path(request["truth"]))
            results = comparison.compare_synthetic(items, truth)
            rows = [[("exact depth", comparison.grey(truth["depth"], truth["valid"])),
                     ("exact normals", comparison.shaded(truth["normals"]))]]
            for item in items:
                fitted = comparison.resized(item, *truth["depth"].shape)
                # The sheet shows exactly the pixels that were scored.
                scored = truth["valid"] & fitted.valid
                if fitted.depth is not None:
                    aligned, _ = scoring.align(fitted.depth, truth["depth"], scored, item.kind)
                    error = numpy.abs(aligned - truth["depth"]) / truth["depth"]
                    rows.append([(item.name, comparison.grey(aligned, scored)),
                                 ("depth error, 0 to 10 %", comparison.heat(error, scored, 0.10))])
                else:
                    angles = scoring.angle_deg(fitted.normals, truth["normals"])
                    rows.append([(item.name, comparison.shaded(fitted.normals)),
                                 ("normal angle, 0 to 45 deg", comparison.heat(angles, scored, 45.0))])
        comparison.sheet(rows).save(output / "sheet.png", format="PNG")
        files["sheet"] = "sheet.png"
    except comparison.CompareError as error:
        return _fail("validation", str(error))
    except (OSError, ValueError, KeyError) as error:
        return _fail("validation", f"an input could not be read: {error}")
    except MemoryError:
        return _fail("resource_limit", "the worker ran out of memory")
    try:
        answer = json.dumps({"result": {"results": results, "files": files}}, allow_nan=False)
    except ValueError:
        return _fail("failed", "a measure was not a finite number; check the inputs' values")
    print(answer)
    return 0


def _write_float(values, path: Path) -> None:
    from PIL import Image

    Image.fromarray(values).save(path, format="TIFF", compression="tiff_adobe_deflate")


def _fail(code: str, message: str) -> int:
    print(json.dumps({"error": {"code": code, "message": message}}))
    return 2


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
