"""Measure maps and meshes against each other, a photograph's analysis or a synthetic scene.

compare_photo.py reads the request and writes the files; this module holds the
arithmetic, so it can be tested with NumPy, Pillow and SciPy alone. The depth
benchmark's scoring code (scripts/depth_benchmark/scoring.py, installed beside
the worker's scripts as `scoring`) provides every benchmark metric, so a
synthetic score is the number the benchmark report gives for the same truth
and prediction.

Inputs, after loading, follow the analysis conventions: depth is distance along
the viewing axis, larger is farther; normals are unit vectors with x right,
y up and z towards the viewer. A 16-bit greyscale PNG is a heightmap, larger is
nearer, as the relief heightmap and the photograph depth map store it; a 32-bit
floating-point TIFF is a depth map, as the analysis stores it; a 16-bit RGB PNG
is a normal map; a mesh arrives as the render worker's exact depth, normals
and coverage seen from above, with depth in millimetres, which is converted to
metres, the benchmark's unit for metric depth.
"""

from __future__ import annotations

from dataclasses import dataclass
from pathlib import Path
import struct
import zipfile

import numpy

import scoring

from .photo_analysis import read_png16, resize_map, resize_mask

# A heightmap pixel counts as subject when it stands this share of the
# height range above the lowest height: the flat background of a relief
# is its lowest level.
SILHOUETTE_THRESHOLD = 0.02
# Normals agree when they are within this angle, as the analysis's own
# normal check counts them.
NORMAL_AGREEMENT_DEGREES = 15.0
# Aspect ratios closer than this are the same proportions.
ASPECT_TOLERANCE = 0.01
# Largest map decoded, in pixels: the photograph limit of the worker's face
# reconstruction. Header dimensions are checked before any pixel is decoded.
MAX_PIXELS = 40_000_000
REGION_NAMES = ("top left", "top", "top right", "left", "centre", "right", "bottom left", "bottom", "bottom right")


class CompareError(ValueError):
    """An input or a result that cannot be measured honestly."""


@dataclass
class Input:
    """One loaded input: depth (larger is farther), and what else it carries."""

    name: str
    source: str  # "depth map", "heightmap", "normal map" or "mesh"
    depth: numpy.ndarray | None
    normals: numpy.ndarray | None
    valid: numpy.ndarray
    kind: str

    @property
    def shape(self) -> tuple[int, int]:
        values = self.depth if self.depth is not None else self.normals
        return values.shape[:2]


# ------------------------------------------------------------------ loading
def load(name: str, path: Path, kind: str) -> Input:
    """An input from its stored file; `kind` is the depth kind a depth map or heightmap declares."""
    path = Path(path)
    head = path.read_bytes()[:32]
    if head.startswith(b"PK"):
        return _load_rendered(name, path)
    if head.startswith(b"\x89PNG\r\n\x1a\n") and head[12:16] == b"IHDR":
        bit_depth, colour = head[24], head[25]
        if bit_depth != 16 or colour not in (0, 2):
            raise CompareError(f"{name} must be a 16-bit greyscale heightmap or a 16-bit RGB normal map")
        _check_pixels(name, *struct.unpack(">II", head[16:24]))
        if colour == 2:
            try:
                levels = read_png16(path).astype(numpy.float64)
            except ValueError as error:
                raise CompareError(f"{name}: {error}") from None
            normals = _unit(levels / 65535.0 * 2.0 - 1.0)
            return Input(name, "normal map", None, normals, numpy.linalg.norm(normals, axis=-1) > 0.5, kind)
        if kind == scoring.METRIC:
            raise CompareError(f"{name} is a 16-bit PNG, which holds relative heights; use relative or logrel")
        levels = _image_array(path).astype(numpy.float64)
        # Larger is nearer in the file; larger is farther in a depth map.
        return Input(name, "heightmap", 1.0 - levels / 65535.0, None, numpy.ones(levels.shape, bool), kind)
    if head[:4] in (b"II*\x00", b"MM\x00*"):
        values = _image_array(path)
        if values.dtype.kind != "f" or values.ndim != 2:
            raise CompareError(f"{name} must be a one-channel 32-bit floating-point TIFF depth map")
        depth = values.astype(numpy.float64)
        valid = numpy.isfinite(depth) & ((depth > 0) if kind == scoring.METRIC else True)
        return Input(name, "depth map", numpy.where(valid, depth, 0.0), None, valid, kind)
    raise CompareError(f"{name} is not a PNG or TIFF map or a rendered mesh")


def _check_pixels(name: str, width: int, height: int) -> None:
    if width * height > MAX_PIXELS:
        raise CompareError(f"{name} has more than {MAX_PIXELS} pixels")


# The members of each NumPy archive compare reads, with their data type and
# dimensions: "image" is a height x width map, "normals" adds three channels,
# "scalar" has none.
RENDERED_VIEW = {"depth": ("<f4", "image"), "normals": ("<f4", "normals"), "mask": ("|b1", "image"),
                 "focal_px": ("<f8", "scalar")}
BENCHMARK_TRUTH = {"depth": ("<f8", "image"), "normals": ("<f4", "normals"), "valid": ("|b1", "image"),
                   "focal": ("<f8", "scalar"), "region": ("|b1", "image")}
# Bytes allowed beyond an array's own data: its header.
ARCHIVE_HEADER_BYTES = 4096


def _check_archive(name: str, path: Path, layouts: tuple[dict, ...]) -> dict:
    """The layout a NumPy archive follows, checked from its member headers before any array is loaded.

    The archive must hold exactly one layout's members, each with its data
    type and dimensions, one image size of at most MAX_PIXELS pixels, and no
    member whose stored data is larger than its header declares.
    """
    try:
        with zipfile.ZipFile(path) as archive:
            members = {}
            for info in archive.infolist():
                if not info.filename.endswith(".npy"):
                    raise CompareError(f"{name} holds an unexpected member, {info.filename}")
                with archive.open(info) as member:
                    version = numpy.lib.format.read_magic(member)
                    read = (numpy.lib.format.read_array_header_1_0 if version == (1, 0)
                            else numpy.lib.format.read_array_header_2_0)
                    shape, fortran, dtype = read(member)
                if dtype.hasobject or fortran:
                    raise CompareError(f"{name} holds an unexpected array, {info.filename}")
                members[info.filename[:-4]] = (dtype.str, shape, info.file_size)
    except (zipfile.BadZipFile, ValueError, OSError) as error:
        if isinstance(error, CompareError):
            raise
        raise CompareError(f"{name} is not a readable NumPy archive: {error}") from None
    layout = next((layout for layout in layouts if set(layout) == set(members)), None)
    if layout is None:
        raise CompareError(f"{name} does not hold the expected arrays")
    sizes = set()
    for key, (dtype, kind) in layout.items():
        found, shape, stored = members[key]
        expected = {"image": 2, "normals": 3, "scalar": 0}[kind]
        if found != dtype or len(shape) != expected or (kind == "normals" and shape[2] != 3):
            raise CompareError(f"{name}: {key} is not a {dtype} array of the expected shape")
        if kind != "scalar":
            sizes.add(tuple(shape[:2]))
        elements = int(numpy.prod(shape, dtype=numpy.int64))
        if kind != "scalar" and shape[0] * shape[1] > MAX_PIXELS:
            raise CompareError(f"{name} has more than {MAX_PIXELS} pixels")
        if stored > elements * numpy.dtype(dtype).itemsize + ARCHIVE_HEADER_BYTES:
            raise CompareError(f"{name}: {key} holds more data than its header declares")
    if len(sizes) != 1:
        raise CompareError(f"{name}: its maps differ in size")
    return layout


def _load_rendered(name: str, path: Path) -> Input:
    _check_archive(name, path, (RENDERED_VIEW,))
    with numpy.load(path, allow_pickle=False) as data:
        depth = numpy.asarray(data["depth"], numpy.float64) / 1000.0
        normals = numpy.asarray(data["normals"], numpy.float64)
        valid = numpy.asarray(data["mask"], bool)
    if not valid.any():
        raise CompareError(f"{name}: the mesh covers no pixel of the view")
    # Pixels outside the mesh take its farthest depth, as the analysis fills them.
    depth = numpy.where(valid, depth, depth[valid].max())
    return Input(name, "mesh", depth, normals, valid, scoring.METRIC)


def _image_array(path: Path) -> numpy.ndarray:
    from PIL import Image

    with Image.open(path) as image:
        _check_pixels(Path(path).name, *image.size)
        return numpy.asarray(image)


def _unit(values: numpy.ndarray) -> numpy.ndarray:
    length = numpy.linalg.norm(values, axis=-1, keepdims=True)
    return numpy.where(length > 1e-6, values / numpy.maximum(length, 1e-6), 0.0)


def load_analysis(depth: Path, normals: Path, mask: Path) -> dict:
    """The analysis maps the photo mode reads."""
    levels = _image_array(mask)
    return {"depth": _image_array(depth).astype(numpy.float64),
            "normals": _unit(read_png16(normals).astype(numpy.float64) / 65535.0 * 2.0 - 1.0),
            "mask": levels > 32767}


def load_photograph(path: Path, height: int, width: int) -> numpy.ndarray:
    """The photograph upright, as luminance from 0 to 1 at the maps' size."""
    from PIL import Image

    from estimate_depth import _open

    image = _open(path).convert("L").resize((width, height), Image.BILINEAR)
    return numpy.asarray(image, numpy.float64) / 255.0


# ------------------------------------------------------------------ geometry
def resized(item: Input, height: int, width: int) -> Input:
    """The input resampled to height x width: bilinear for values, nearest for coverage."""
    if item.shape == (height, width):
        return item
    depth = None if item.depth is None else resize_map(item.depth, height, width).astype(numpy.float64)
    normals = None if item.normals is None else _unit(resize_map(item.normals, height, width).astype(numpy.float64))
    return Input(item.name, item.source, depth, normals, resize_mask(item.valid, height, width), item.kind)


def centre_crop(shape: tuple[int, int], aspect: float) -> tuple[slice, slice]:
    """The largest centred window of `shape` with the given width / height."""
    height, width = shape
    if abs(width / height - aspect) <= ASPECT_TOLERANCE * aspect:
        return slice(0, height), slice(0, width)
    if width / height > aspect:
        keep = max(1, round(height * aspect))
        start = (width - keep) // 2
        return slice(0, height), slice(start, start + keep)
    keep = max(1, round(width / aspect))
    start = (height - keep) // 2
    return slice(start, start + keep), slice(0, width)


def silhouette(item: Input) -> numpy.ndarray:
    """A mesh's coverage, or for a map the pixels that stand above its flat background."""
    if item.source == "mesh":
        return item.valid
    height = -item.depth
    inside = height[item.valid]
    low, high = numpy.percentile(inside, 1), numpy.percentile(inside, 99)
    if high <= low:
        return numpy.zeros(item.valid.shape, bool)
    return item.valid & (height > inside.min() + SILHOUETTE_THRESHOLD * (high - low))


def boundary(mask: numpy.ndarray) -> numpy.ndarray:
    return mask & ~scoring.erode(mask, 1)


def outline_distance(first: numpy.ndarray, second: numpy.ndarray) -> dict:
    """Distances in pixels from each outline to the other: mean of both directions and their 95th percentile."""
    from scipy import ndimage

    a, b = boundary(first), boundary(second)
    if not a.any() or not b.any():
        raise CompareError("an outline is empty, so outline distance cannot be measured")
    to_b = ndimage.distance_transform_edt(~b)[a]
    to_a = ndimage.distance_transform_edt(~a)[b]
    both = numpy.concatenate([to_b, to_a])
    mean = 0.5 * (float(to_b.mean()) + float(to_a.mean()))
    diagonal = float(numpy.hypot(*first.shape))
    return {"mean_px": mean, "p95_px": float(numpy.percentile(both, 95)),
            "percent_of_diagonal": 100.0 * mean / diagonal}


def height_gradients(depth: numpy.ndarray) -> tuple[numpy.ndarray, numpy.ndarray]:
    """Slopes of the height field (-depth) along x (right) and y (up), per pixel."""
    height = -depth
    return numpy.gradient(height, axis=1), -numpy.gradient(height, axis=0)


def normals_from_slopes(gx: numpy.ndarray, gy: numpy.ndarray, scale: float) -> numpy.ndarray:
    return _unit(numpy.stack([-scale * gx, -scale * gy, numpy.ones_like(gx)], axis=-1))


def fitted_normals(depth: numpy.ndarray, target: numpy.ndarray, mask: numpy.ndarray) -> tuple[numpy.ndarray, float]:
    """Normals of the height field with the one non-negative height scale that best matches `target`'s slopes.

    A relief compresses depth, so its heights are scaled before they are
    compared; the scale cannot be negative, so an inverted relief fits as a
    flat one rather than as a correct one.
    """
    gx, gy = height_gradients(depth)
    nz = numpy.clip(target[..., 2], 0.1, None)
    tx, ty = -target[..., 0] / nz, -target[..., 1] / nz
    energy = float((gx[mask] ** 2 + gy[mask] ** 2).sum())
    scale = max(0.0, float((gx[mask] * tx[mask] + gy[mask] * ty[mask]).sum()) / energy) if energy > 0 else 0.0
    return normals_from_slopes(gx, gy, scale), scale


def shading_fit(normals: numpy.ndarray, luminance: numpy.ndarray, mask: numpy.ndarray) -> dict:
    """Correlation of luminance with the best single directional light on these normals.

    Luminance is fitted as a + b . n by least squares; the correlation is
    between the fitted shading and the photograph. Normals that do not vary,
    a flat or inverted relief, explain none of the shading and score 0. A
    photograph whose brightness does not vary cannot be measured.
    """
    n = normals[mask]
    y = luminance[mask]
    if n.shape[0] < 4:
        raise CompareError("too few subject pixels to measure shading")
    if numpy.std(y) == 0:
        raise CompareError("the photograph's brightness does not vary inside the subject, so shading cannot be measured")
    design = numpy.concatenate([numpy.ones((n.shape[0], 1)), n], axis=1)
    if numpy.ptp(n, axis=0).max() < 1e-9:
        return {"correlation": 0.0, "light": None}
    coefficients, *_ = numpy.linalg.lstsq(design, y, rcond=None)
    fitted = design @ coefficients
    if numpy.std(fitted) == 0:
        return {"correlation": 0.0, "light": None}
    light = coefficients[1:]
    norm = float(numpy.linalg.norm(light))
    return {"correlation": float(numpy.corrcoef(fitted, y)[0, 1]),
            "light": [float(v) for v in light / norm] if norm > 0 else None}


# ------------------------------------------------------------------ modes
def compare_outputs(items: list[Input]) -> tuple[list[dict], list[numpy.ndarray], dict]:
    """Each later input against the first: aligned difference statistics overall and in nine regions.

    Returns the results, the difference maps (depth units of the first input,
    positive where the later input lies farther) and the common size.
    """
    if any(item.depth is None for item in items):
        raise CompareError("outputs compares depth maps, heightmaps and meshes; a normal map has no heights")
    reference = items[0]
    height, width = reference.shape
    for item in items[1:]:
        h, w = item.shape
        if abs(w / h - width / height) > ASPECT_TOLERANCE * width / height:
            raise CompareError(f"{item.name} does not have the proportions of {reference.name}")
    items = [resized(item, height, width) for item in items]
    reference = items[0]
    results, differences = [], []
    for item in items[1:]:
        valid = reference.valid & item.valid
        if valid.sum() < 2:
            raise CompareError(f"{item.name} and {reference.name} share no valid pixels")
        scale, offset = _scale_and_offset(item.depth[valid], reference.depth[valid])
        fit = {"scale": scale, "offset": offset}
        difference = numpy.where(valid, item.depth * scale + offset - reference.depth, 0.0)
        span = float(numpy.percentile(reference.depth[valid], 99) - numpy.percentile(reference.depth[valid], 1))
        results.append({
            "input": item.name, "reference": reference.name, "alignment": fit,
            "correlation": _correlation(item.depth[valid], reference.depth[valid]),
            "whole": _difference_stats(difference, valid, span),
            "regions": {name: _difference_stats(difference, valid & cell, span)
                        for name, cell in zip(REGION_NAMES, _cells(height, width))},
        })
        differences.append(difference)
    return results, differences, {"width": width, "height": height}


def _scale_and_offset(x: numpy.ndarray, y: numpy.ndarray) -> tuple[float, float]:
    """Least-squares y ~ scale * x + offset, with no floor on the values, so signed maps align exactly.

    The scale may not be negative, as in the benchmark's alignment: an
    inverted input fits as a flat one and its difference stays visible.
    """
    design = numpy.stack([x, numpy.ones_like(x)], axis=1)
    (scale, offset), *_ = numpy.linalg.lstsq(design, y, rcond=None)
    if scale < 0:
        return 0.0, float(numpy.mean(y))
    return float(scale), float(offset)


def _correlation(a: numpy.ndarray, b: numpy.ndarray) -> float | None:
    if a.size < 2 or numpy.std(a) == 0 or numpy.std(b) == 0:
        return None
    return float(numpy.corrcoef(a, b)[0, 1])


def _cells(height: int, width: int) -> list[numpy.ndarray]:
    rows = numpy.minimum(numpy.arange(height) * 3 // height, 2)
    columns = numpy.minimum(numpy.arange(width) * 3 // width, 2)
    return [(rows[:, None] == r) & (columns[None, :] == c) for r in range(3) for c in range(3)]


def _difference_stats(difference: numpy.ndarray, mask: numpy.ndarray, span: float) -> dict:
    values = difference[mask]
    if values.size == 0:
        return {"pixels": 0}
    rmse = float(numpy.sqrt(numpy.mean(values ** 2)))
    stats = {"pixels": int(values.size), "rmse": rmse, "mean": float(values.mean()),
             "mean_abs": float(numpy.abs(values).mean()), "p95_abs": float(numpy.percentile(numpy.abs(values), 95))}
    stats["rmse_percent_of_range"] = 100.0 * rmse / span if span > 0 else None
    return stats


def compare_photo(items: list[Input], analysis: dict, luminance: numpy.ndarray) -> tuple[list[dict], list[dict]]:
    """Each input against the analysis and the photograph, every measure reported separately.

    Returns the results and, for the sheet, each input's silhouette, normals,
    angle map and crop of the analysis.
    """
    results, views = [], []
    for item in items:
        if item.depth is None:
            raise CompareError(f"{item.name} is a normal map; photo compares heightmaps, depth maps and meshes")
        rows, columns = centre_crop(analysis["mask"].shape, item.shape[1] / item.shape[0])
        mask, target = analysis["mask"][rows, columns], analysis["normals"][rows, columns]
        light = luminance[rows, columns]
        item = resized(item, *mask.shape)
        shape = silhouette(item)
        union = int((shape | mask).sum())
        if not union:
            raise CompareError(f"neither {item.name} nor the analysis has any subject pixels")
        inside = scoring.erode(mask & item.valid, 2)
        if inside.sum() < 4:
            raise CompareError(f"{item.name} and the analysis's subject mask have too little overlap to compare")
        if item.source == "mesh":
            # A rendered mesh has its own exact normals in the photograph's camera frame.
            normals, scale = item.normals, None
        else:
            normals, scale = fitted_normals(item.depth, target, inside)
        angles = scoring.angle_deg(normals, target)
        selected = angles[inside]
        shading_region = inside & scoring.erode(shape, 2)
        results.append({
            "input": item.name, "source": item.source,
            "crop": {"x": columns.start, "y": rows.start, "width": columns.stop - columns.start,
                     "height": rows.stop - rows.start},
            "silhouette_iou": float((shape & mask).sum() / union),
            "outline_distance": outline_distance(shape, mask),
            "normal_angle": {"median_deg": float(numpy.median(selected)),
                             "within_15_deg": float((selected <= NORMAL_AGREEMENT_DEGREES).mean()),
                             "height_scale": scale, "pixels": int(selected.size)},
            "shading": {**shading_fit(normals, light, shading_region),
                        "analysis_correlation": shading_fit(target, light, shading_region)["correlation"],
                        "pixels": int(shading_region.sum())},
        })
        views.append({"silhouette": shape, "mask": mask, "normals": normals, "angles": numpy.where(inside, angles, 0.0),
                      "photograph": light})
    return results, views


def compare_synthetic(items: list[Input], truth: dict) -> list[dict]:
    """Each input scored against the scene's exact depth and normals with the benchmark's scene scores."""
    height, width = truth["depth"].shape
    results = []
    for item in items:
        item = resized(item, height, width)
        if item.depth is None:
            smooth = scoring.smooth_mask(truth["depth"], truth["valid"]) & item.valid
            if not smooth.any():
                raise CompareError(f"{item.name} has no values where the scene is visible")
            row = {"normal_deg": scoring.normal_errors(item.normals, truth["normals"], smooth)["median_deg"]}
        else:
            # Only pixels where the prediction has a value are scored.
            valid = truth["valid"] & item.valid
            if valid.sum() < 2:
                raise CompareError(f"{item.name} has no values where the scene is visible")
            region = truth["region"] & item.valid if truth["region"] is not None else None
            # The benchmark stores every prediction in 32-bit floating point.
            pred = {"depth": item.depth.astype(numpy.float32), "kind": item.kind}
            if item.normals is not None:
                pred["normals"] = item.normals.astype(numpy.float32)
            row = scoring.scene_scores(pred, {**truth, "valid": valid, "region": region})
        for key, value in row.items():
            if isinstance(value, float) and not numpy.isfinite(value):
                raise CompareError(f"{item.name} gave a non-finite {key}; check the map's values")
        results.append({"input": item.name, "source": item.source, **row})
    return results


def load_truth(path: Path) -> dict:
    """A scene's exact maps, depth in metres, as scoring.scene_scores reads them.

    Either the depth benchmark's own converted truth for one of its scenes
    (scoring.save_truth writes it beside the scene), whose relief region is the
    benchmark's, or a mesh rendered by the render worker, depth in
    millimetres, whose relief region is every pixel the mesh covers.
    """
    layout = _check_archive("the scene", path, (BENCHMARK_TRUTH, RENDERED_VIEW))
    if layout is BENCHMARK_TRUTH:
        try:
            return scoring.load_truth(path)
        except (KeyError, ValueError) as error:
            raise CompareError(f"the scene's truth archive is not the benchmark's: {error}") from None
    with numpy.load(path, allow_pickle=False) as data:
        valid = numpy.asarray(data["mask"], bool)
        depth = numpy.asarray(data["depth"], numpy.float64) / 1000.0
        focal = float(data["focal_px"])
        normals = numpy.asarray(data["normals"], numpy.float64)
    if valid.sum() < 2:
        raise CompareError("the scene mesh covers too little of its view to score")
    depth = numpy.where(valid, depth, depth[valid].max())
    return {"depth": depth, "normals": normals, "valid": valid, "focal": focal, "region": valid}


# ------------------------------------------------------------------ pictures
def grey(depth: numpy.ndarray, valid: numpy.ndarray) -> numpy.ndarray:
    """Depth as an 8-bit picture, nearest brightest, over the valid pixels."""
    inside = depth[valid]
    if inside.size == 0:
        return numpy.zeros(depth.shape + (3,), numpy.uint8)
    low, high = numpy.percentile(inside, 1), numpy.percentile(inside, 99)
    level = numpy.clip((high - depth) / max(high - low, 1e-12), 0, 1)
    level = numpy.where(valid, level, 0.0)
    return numpy.repeat((level * 255).astype(numpy.uint8)[..., None], 3, axis=-1)


def diverging(values: numpy.ndarray, mask: numpy.ndarray) -> numpy.ndarray:
    """Signed values as blue (negative) to white to red (positive), symmetric about zero."""
    span = float(numpy.percentile(numpy.abs(values[mask]), 99)) if mask.any() else 0.0
    t = numpy.clip(values / span, -1, 1) if span > 0 else numpy.zeros(values.shape)
    out = numpy.ones(values.shape + (3,))
    out[..., 0] -= numpy.clip(-t, 0, 1)
    out[..., 1] -= numpy.abs(t)
    out[..., 2] -= numpy.clip(t, 0, 1)
    out[~mask] = 0.5
    return (out * 255).astype(numpy.uint8)


def heat(values: numpy.ndarray, mask: numpy.ndarray, top: float) -> numpy.ndarray:
    """Values from 0 (black) to `top` (yellow) and above."""
    t = numpy.clip(values / top, 0, 1)
    out = numpy.stack([numpy.clip(2 * t, 0, 1), numpy.clip(2 * t - 1, 0, 1), numpy.zeros_like(t)], axis=-1)
    out[~mask] = 0.25
    return (out * 255).astype(numpy.uint8)


def overlay(shape: numpy.ndarray, mask: numpy.ndarray) -> numpy.ndarray:
    """Green where both agree, red where only the analysis has subject, blue where only the input has."""
    out = numpy.zeros(shape.shape + (3,), numpy.uint8)
    out[shape & mask] = (60, 200, 60)
    out[mask & ~shape] = (220, 50, 50)
    out[shape & ~mask] = (50, 90, 230)
    return out


def shaded(normals: numpy.ndarray) -> numpy.ndarray:
    """Normals lit from the upper left, as a grey picture."""
    light = numpy.array([-0.4, 0.5, 0.77])
    level = numpy.clip((normals * light / numpy.linalg.norm(light)).sum(-1), 0, 1)
    return numpy.repeat((level * 255).astype(numpy.uint8)[..., None], 3, axis=-1)


def sheet(rows: list[list[tuple[str, numpy.ndarray]]], panel: int = 320):
    """A grid of labelled panels, one list per row, as a Pillow image."""
    from PIL import Image, ImageDraw

    label = 18
    columns = max(len(row) for row in rows)
    canvas = Image.new("RGB", (columns * panel, len(rows) * (panel + label)), (32, 32, 32))
    draw = ImageDraw.Draw(canvas)
    for r, row in enumerate(rows):
        for c, (text, pixels) in enumerate(row):
            picture = Image.fromarray(pixels)
            picture.thumbnail((panel, panel))
            x = c * panel + (panel - picture.width) // 2
            y = r * (panel + label) + label + (panel - picture.height) // 2
            canvas.paste(picture, (x, y))
            draw.text((c * panel + 4, r * (panel + label) + 3), text[:48], fill=(230, 230, 230))
    return canvas
