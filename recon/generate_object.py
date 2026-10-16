"""Generate a 3D shape of the object in one photograph with Pixal3D.

Usage: generate_object.py <request.json>
       generate_object.py --check <model-directory>
       generate_object.py --version

The request names the photograph, the Pixal3D model directory, the STL file
to write, the resolution (512, 1024 or 1536), the sampling steps, a seed, an
optional horizontal field of view of the framed subject in degrees, the
size of the longest side in millimetres and the device. One JSON object is
printed: {"result": ...} or {"error": {"code", "message"}}.

The STL is in millimetres with Z up, the subject facing -Y as the camera
saw it, centred on the origin and standing on Z = 0. The sides the camera
did not see are the model's invention. The surface comes straight from the
generator and is not checked or repaired.
"""

import json
import os
import struct
import sys
import time
from pathlib import Path

VERSION = "printable-object 1"
RESOLUTIONS = (512, 1024, 1536)
DEFAULT_RESOLUTION = 1024
STEPS_RANGE = (1, 50)
FOV_RANGE = (5.0, 90.0)
SIZE_RANGE = (5.0, 2000.0)
DEFAULT_SIZE_MM = 100.0
DEFAULT_SEED = 42
SEED_RANGE = (0, 2**31 - 1)
# The pinned background removal code installed beside this script.
SEGMENTER_CODE = Path(__file__).resolve().parent / "vendor" / "birefnet"


def main(argv: list[str]) -> int:
    if argv == ["--version"]:
        print(VERSION)
        return 0
    sys.path.insert(0, str(Path(__file__).resolve().parent))
    if len(argv) == 2 and argv[0] == "--check":
        from printable_recon.files import missing_object_files

        print(json.dumps({"missing": missing_object_files(Path(argv[1]))}))
        return 0
    if len(argv) != 1:
        return _fail("validation", "expected one request file")
    try:
        request = json.loads(Path(argv[0]).read_text(encoding="utf-8"))
        image_path = Path(request["image"])
        directory = Path(request["model"])
        mesh_path = Path(request["mesh"])
        resolution = _whole(request.get("resolution", DEFAULT_RESOLUTION))
        steps = _whole(request["steps"]) if request.get("steps") is not None else None
        seed = _whole(request.get("seed", DEFAULT_SEED))
        fov = float(request["fov_degrees"]) if request.get("fov_degrees") is not None else None
        size_mm = float(request.get("size_mm", DEFAULT_SIZE_MM))
        device = str(request.get("device", "auto"))
    except (OSError, ValueError, KeyError, TypeError) as error:
        return _fail("validation", f"the request is invalid: {error}")
    problem = _check(resolution, steps, seed, fov, size_mm)
    if problem:
        return _fail("validation", problem)

    # Set before CUDA starts: memory released by one stage is reused by the next.
    os.environ.setdefault("PYTORCH_CUDA_ALLOC_CONF", "expandable_segments:True")
    import math

    import torch
    from PIL import Image

    from printable_recon import features, objects, pixal3d

    if device == "auto":
        if not torch.cuda.is_available():
            return _fail(
                "unavailable",
                "object generation needs a CUDA GPU; on the processor alone it would take hours",
            )
        device = "cuda"
    try:
        photo, photo_fov = features.open_photograph(image_path)
    except (OSError, ValueError, Image.DecompressionBombError) as error:
        return _fail("validation", f"the photograph could not be read: {error}")
    started = time.monotonic()
    try:
        shape = objects.generate(
            photo,
            photo_fov,
            directory,
            SEGMENTER_CODE,
            torch.device(device),
            resolution=resolution,
            steps=steps,
            seed=seed,
            fov=math.radians(fov) if fov is not None else None,
        )
    except pixal3d.ModelError as error:
        return _fail("checkpoints", str(error))
    except objects.GenerationError as error:
        return _fail("no_shape", str(error))
    except torch.cuda.OutOfMemoryError:
        return _fail("resource_limit", "the GPU ran out of memory; lower the resolution or free GPU memory")
    except MemoryError:
        return _fail("resource_limit", "the worker ran out of memory")
    extent = write_stl(shape.vertices, shape.triangles, size_mm, mesh_path)
    result = {
        "triangles": int(shape.triangles.shape[0]),
        "size_mm": extent,
        "resolution": shape.resolution,
        "tokens": shape.tokens,
        "steps": shape.steps,
        "seed": seed,
        "fov_degrees": round(math.degrees(shape.fov), 3),
        "fov_source": shape.fov_source,
        "background": shape.background,
        "device": device,
        "seconds": round(time.monotonic() - started, 3),
        "stages": shape.report.stages,
    }
    if device == "cuda":
        result["peak_gpu_memory_bytes"] = max(stage.get("peak_gpu_memory_bytes", 0) for stage in shape.report.stages)
    print(json.dumps({"result": result}))
    return 0


def _whole(value) -> int:
    if isinstance(value, bool) or not isinstance(value, int):
        raise ValueError(f"{value!r} is not a whole number")
    return value


def _check(resolution: int, steps: int | None, seed: int, fov: float | None, size_mm: float) -> str | None:
    if resolution not in RESOLUTIONS:
        return f"resolution must be one of {', '.join(map(str, RESOLUTIONS))}"
    if steps is not None and not STEPS_RANGE[0] <= steps <= STEPS_RANGE[1]:
        return f"steps must be between {STEPS_RANGE[0]} and {STEPS_RANGE[1]}"
    if not SEED_RANGE[0] <= seed <= SEED_RANGE[1]:
        return f"seed must be between {SEED_RANGE[0]} and {SEED_RANGE[1]}"
    if fov is not None and not FOV_RANGE[0] <= fov <= FOV_RANGE[1]:
        return f"fov_degrees must be between {FOV_RANGE[0]:g} and {FOV_RANGE[1]:g}"
    if not SIZE_RANGE[0] <= size_mm <= SIZE_RANGE[1]:
        return f"size_mm must be between {SIZE_RANGE[0]:g} and {SIZE_RANGE[1]:g}"
    return None


def write_stl(vertices, triangles, size_mm: float, path: Path) -> list[float]:
    """Write the shape as a binary STL in millimetres and return its extent.

    The generator's cube has x right, y up and z towards the camera; the STL
    has Z up and the camera looking along +Y. The longest side becomes
    `size_mm`, the shape is centred on the origin and its lowest point is at
    Z = 0.
    """
    import numpy

    points = numpy.stack([vertices[:, 0], -vertices[:, 2], vertices[:, 1]], axis=1).astype(numpy.float64)
    used = points[numpy.unique(triangles)]
    low, high = used.min(axis=0), used.max(axis=0)
    scale = size_mm / max(float((high - low).max()), 1e-9)
    offset = numpy.array([(low[0] + high[0]) / 2, (low[1] + high[1]) / 2, low[2]])
    points = ((points - offset) * scale).astype(numpy.float32)
    corners = points[triangles]  # T x 3 x 3
    normals = numpy.cross(corners[:, 1] - corners[:, 0], corners[:, 2] - corners[:, 0])
    lengths = numpy.linalg.norm(normals, axis=1, keepdims=True)
    normals = numpy.where(lengths > 0, normals / numpy.maximum(lengths, 1e-30), 0.0).astype(numpy.float32)
    record = numpy.dtype([("normal", "<f4", 3), ("corners", "<f4", (3, 3)), ("attribute", "<u2")])
    rows = numpy.zeros(triangles.shape[0], dtype=record)
    rows["normal"] = normals
    rows["corners"] = corners
    with path.open("wb") as stl:
        stl.write(b"Printable Pixal3D shape".ljust(80, b" "))
        stl.write(struct.pack("<I", triangles.shape[0]))
        rows.tofile(stl)
    return [round(float(side), 3) for side in (high - low) * scale]


def _fail(code: str, message: str) -> int:
    print(json.dumps({"error": {"code": code, "message": message}}))
    return 2


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
