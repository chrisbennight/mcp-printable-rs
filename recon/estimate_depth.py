"""Estimate depth, and optionally surface normals, for one photograph.

Usage: estimate_depth.py <request.json>
       estimate_depth.py --check <model-directory>
       estimate_depth.py --version

The request names the photograph, the converted Marigold V2 directory, the
depth map and optional normal map to write, the longest side to process at,
the device and a seed. One JSON object is printed: {"result": ...} or
{"error": {"code", "message"}}.

The depth map is a 16-bit greyscale PNG of Marigold V2's relative log depth
with the nearest point brightest: a pixel value p means a relative log depth
of far - (far - near) * p / 65535, where near and far are reported. Equal
steps of relative log depth are equal ratios of distance; the scale and
offset of the distances themselves are unknown. The normal map is a 16-bit
RGB PNG of unit normals in the camera's frame, x to the right, y up and z
towards the camera, each component c stored as (c + 1) / 2 * 65535.
"""

import json
import struct
import sys
import time
import zlib
from pathlib import Path

VERSION = "printable-depth 1"
DEFAULT_MAX_SIDE = 1024
MAX_SIDE_RANGE = (256, 2048)


def main(argv: list[str]) -> int:
    if argv == ["--version"]:
        print(VERSION)
        return 0
    sys.path.insert(0, str(Path(__file__).resolve().parent))
    if len(argv) == 2 and argv[0] == "--check":
        from printable_recon.files import missing_marigold_files

        print(json.dumps({"missing": missing_marigold_files(Path(argv[1]))}))
        return 0
    if len(argv) != 1:
        return _fail("validation", "expected one request file")
    try:
        request = json.loads(Path(argv[0]).read_text(encoding="utf-8"))
        image_path = Path(request["image"])
        directory = Path(request["model"])
        depth_path = Path(request["depth"])
        normals_path = Path(request["normals"]) if request.get("normals") else None
        max_side = int(request.get("max_side", DEFAULT_MAX_SIDE))
        device = str(request.get("device", "auto"))
        seed = int(request.get("seed", 0))
    except (OSError, ValueError, KeyError, TypeError) as error:
        return _fail("validation", f"the request is invalid: {error}")
    low, high = MAX_SIDE_RANGE
    if not low <= max_side <= high:
        return _fail("validation", f"max_side must be between {low} and {high}")

    import torch
    from PIL import Image

    from printable_recon import marigold

    if device == "auto":
        if not torch.cuda.is_available():
            return _fail(
                "unavailable",
                "depth estimation needs a CUDA GPU; on the processor alone it would take hours",
            )
        device = "cuda"
    try:
        image = _open(image_path)
    except (OSError, ValueError, Image.DecompressionBombError) as error:
        return _fail("validation", f"the photograph could not be read: {error}")
    started = time.monotonic()
    try:
        pixels = marigold.prepare(image, max_side)
        depth = marigold.predict(pixels, directory, "depth", torch.device(device), seed=seed)
        result = {"width": int(pixels.shape[-1]), "height": int(pixels.shape[-2])}
        result["depth"] = write_depth(depth.mean(dim=0).numpy(), depth_path)
        del depth
        if normals_path is not None:
            normals = marigold.predict(pixels, directory, "normals", torch.device(device), seed=seed)
            result["normals"] = write_normals(normals.numpy(), normals_path)
    except marigold.ModelError as error:
        return _fail("checkpoints", str(error))
    except torch.cuda.OutOfMemoryError:
        return _fail("resource_limit", "the GPU ran out of memory; lower max_side or free GPU memory")
    except MemoryError:
        return _fail("resource_limit", "the worker ran out of memory")
    result["device"] = device
    result["seconds"] = round(time.monotonic() - started, 3)
    if device == "cuda":
        result["peak_gpu_memory_bytes"] = int(torch.cuda.max_memory_allocated())
    print(json.dumps({"result": result}))
    return 0


def _open(path: Path):
    """The photograph upright, in RGB, with any transparency flattened onto black."""
    from PIL import Image, ImageOps

    with Image.open(path) as opened:
        image = ImageOps.exif_transpose(opened)
        if image.mode in ("RGBA", "LA", "PA") or "transparency" in image.info:
            image = image.convert("RGBA")
            image = Image.alpha_composite(Image.new("RGBA", image.size, (0, 0, 0, 255)), image)
        return image.convert("RGB")


def write_depth(values, path: Path) -> dict:
    """Store relative log depth as a 16-bit PNG, nearest brightest."""
    import numpy
    from PIL import Image

    near, far = float(values.min()), float(values.max())
    span = max(far - near, 1e-6)
    levels = numpy.round((far - values) / span * 65535.0).astype(numpy.uint16)
    Image.fromarray(levels).save(path, format="PNG")
    return {"near": near, "far": far}


def write_normals(values, path: Path) -> dict:
    """Store unit normals, 3 x H x W, as a 16-bit RGB PNG."""
    import numpy

    length = numpy.linalg.norm(values, axis=0, keepdims=True)
    normals = numpy.where(length > 1e-6, values / numpy.maximum(length, 1e-6), 0.0)
    levels = numpy.round((normals.transpose(1, 2, 0) + 1.0) / 2.0 * 65535.0).clip(0, 65535).astype(">u2")
    write_png16(path, levels)
    return {"encoding": "c = 2 * p / 65535 - 1", "frame": "x right, y up, z towards the camera"}


def write_png16(path: Path, pixels) -> None:
    """Write an H x W x 3 big-endian uint16 array as a 16-bit RGB PNG.

    The imaging library writes 16 bits per sample only for greyscale, so the
    file is assembled here: one unfiltered, zlib-compressed image data chunk.
    """
    import numpy

    height, width, _ = pixels.shape
    rows = numpy.zeros((height, 1 + width * 6), dtype=numpy.uint8)
    rows[:, 1:] = numpy.ascontiguousarray(pixels).view(numpy.uint8).reshape(height, width * 6)

    def chunk(kind: bytes, data: bytes) -> bytes:
        return struct.pack(">I", len(data)) + kind + data + struct.pack(">I", zlib.crc32(kind + data))

    header = struct.pack(">IIBBBBB", width, height, 16, 2, 0, 0, 0)
    path.write_bytes(
        b"\x89PNG\r\n\x1a\n"
        + chunk(b"IHDR", header)
        + chunk(b"IDAT", zlib.compress(rows.tobytes(), 6))
        + chunk(b"IEND", b"")
    )


def _fail(code: str, message: str) -> int:
    print(json.dumps({"error": {"code": code, "message": message}}))
    return 2


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
