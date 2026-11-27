"""Plan, combine and store one photograph's analysis.

An analysis runs the geometry models a request needs, each once, and combines
their outputs into depth, normals, a subject mask, a focal length and a
confidence map with summary scores. This module holds everything except
running the models: the plan, the arithmetic on NumPy arrays and the file
writers, so it can be tested without PyTorch or model weights.
analyze_photo.py runs the models.

Conventions, shared with geometry_models:

- depth is distance along the optical axis, larger is farther; metric models
  give metres, relative models an unscaled depth;
- normals are unit vectors in the camera frame, x right, y up, z towards the
  camera;
- every map is floating point in memory and is stored with 16 bits per sample
  or as 32-bit floating point; nothing passes through 8 bits.
"""

from __future__ import annotations

from dataclasses import dataclass, field
import math
from pathlib import Path
import struct

import numpy

from .geometry_models import Geometry

SUBJECTS = ("shallow", "deep")
QUALITIES = ("best", "fast")
DEVICES = ("cuda", "cpu")
SIDES = (1024, 2048)
# Models other than the depth model run at this side unless they share its run.
OTHER_SIDE = 1024
# Each model setting and the output its model must have.
MODEL_SETTINGS = {"depth_model": "depth", "mask_model": "mask", "focal_model": "focal", "normals_model": "normals"}
FOCAL_LENGTH_PX = (10.0, 100000.0)
# The consistency passes run the depth model again on the mirrored photograph
# and on the photograph at this share of its processed size.
RESCALE = 0.75
# A pixel's depth agreement falls linearly from 1 at no difference to 0 at
# this relative difference; normals from 1 at 0 degrees to 0 at this angle.
DEPTH_TOLERANCE = 0.10
NORMAL_TOLERANCE_DEGREES = 45.0
# Summary scores count the pixels within these limits.
DEPTH_AGREEMENT = 0.05
NORMAL_AGREEMENT_DEGREES = 15.0
METRIC_WARNING = ("Metric distance is a learned estimate and is often wrong by a large factor for close-up "
                  "photographs; never use it as a physical size.")
FILES = {"depth": "depth.tiff", "normals": "normals.png", "mask": "mask.png", "confidence": "confidence.png"}


class RequestError(ValueError):
    """The analysis settings do not have the documented shape."""


class MapFormatError(ValueError):
    """A map would be stored, or was found stored, with fewer than 16 bits per sample."""


@dataclass(frozen=True)
class Settings:
    """What to run, every model named: the server fills the recommended defaults."""

    subject: str
    depth_model: str
    mask_model: str
    side: int
    device: str
    quality: str = "best"
    focal_model: str | None = None
    normals_model: str | None = None
    focal_length_px: float | None = None

    @classmethod
    def parse(cls, value: object, models: dict) -> Settings:
        """Settings from a request, checked against the model list `models`."""
        if not isinstance(value, dict):
            raise RequestError("settings must be an object")
        expected = {"subject", "quality", "side", "device", "focal_length_px", *MODEL_SETTINGS}
        if set(value) != expected:
            raise RequestError(f"settings must name exactly: {', '.join(sorted(expected))}")
        if value["subject"] not in SUBJECTS:
            raise RequestError("subject must be shallow or deep")
        if value["quality"] not in QUALITIES:
            raise RequestError("quality must be best or fast")
        if value["device"] not in DEVICES:
            raise RequestError("device must be cuda or cpu")
        side = value["side"]
        if isinstance(side, bool) or side not in SIDES:
            raise RequestError("side must be 1024 or 2048")
        for key, output in MODEL_SETTINGS.items():
            name = value[key]
            if name is None and key in ("depth_model", "mask_model"):
                raise RequestError(f"{key} must name a model")
            if name is not None and (name not in models or output not in models[name].outputs):
                raise RequestError(f"{key} must name a model with a {output} output")
        focal = value["focal_length_px"]
        if focal is not None:
            if isinstance(focal, bool) or not isinstance(focal, (int, float)) or not math.isfinite(focal) \
                    or not FOCAL_LENGTH_PX[0] <= focal <= FOCAL_LENGTH_PX[1]:
                raise RequestError(f"focal_length_px must be between {FOCAL_LENGTH_PX[0]:g} and {FOCAL_LENGTH_PX[1]:g}")
            focal = float(focal)
        if (focal is None) == (value["focal_model"] is None):
            raise RequestError("give exactly one of focal_model and focal_length_px")
        return cls(value["subject"], value["depth_model"], value["mask_model"], side, value["device"],
                   value["quality"], value["focal_model"], value["normals_model"], focal)

    def as_dict(self) -> dict:
        return {"subject": self.subject, "quality": self.quality, "depth_model": self.depth_model,
                "mask_model": self.mask_model, "focal_model": self.focal_model,
                "normals_model": self.normals_model, "side": self.side, "device": self.device,
                "focal_length_px": self.focal_length_px}


@dataclass(frozen=True)
class Run:
    """One model loaded once and run on the photograph for one or more roles."""

    entry: str
    device: str
    side: int
    roles: tuple[str, ...]
    passes: tuple[str, ...] = ("main",)

    def as_dict(self) -> dict:
        return {"entry": self.entry, "device": self.device, "side": self.side, "roles": list(self.roles),
                "passes": list(self.passes)}


@dataclass(frozen=True)
class Plan:
    runs: tuple[Run, ...]

    def run_for(self, role: str) -> Run | None:
        return next((run for run in self.runs if role in run.roles), None)

    @property
    def entries(self) -> list[str]:
        return list(dict.fromkeys(run.entry for run in self.runs))


def plan(settings: Settings) -> Plan:
    """The models an analysis runs, each loaded once, depth first.

    The depth model runs at the requested side; another model runs at 1024 px
    unless it is the depth model, which then serves both roles from one run.
    Best quality adds the mirrored and rescaled passes of the depth model.
    """
    wanted = {"depth": settings.depth_model, "mask": settings.mask_model,
              "focal": settings.focal_model, "normals": settings.normals_model}
    grouped = {}
    for role, entry in wanted.items():
        if entry is None:
            continue
        side = settings.side if entry == settings.depth_model else OTHER_SIDE
        grouped.setdefault((entry, side), []).append(role)
    runs = []
    for (entry, side), served in grouped.items():
        passes = ("main", "flip", "rescale") if "depth" in served and settings.quality == "best" else ("main",)
        runs.append(Run(entry, settings.device, side, tuple(served), passes))
    return Plan(tuple(runs))


@dataclass
class Outcome:
    """What one run produced: the main pass, the processed size and any consistency passes.

    Each consistency pass is (depth, valid), already mapped back onto the
    main pass's pixels.
    """

    geometry: Geometry
    width: int
    height: int
    passes: dict = field(default_factory=dict)


@dataclass
class Analysis:
    depth: numpy.ndarray
    normals: numpy.ndarray
    mask: numpy.ndarray
    confidence: numpy.ndarray
    manifest: dict


def resize_map(values: numpy.ndarray, height: int, width: int, nearest: bool = False) -> numpy.ndarray:
    """An H x W or H x W x C map resized bilinearly, or by nearest neighbour, in 32-bit floating point."""
    from PIL import Image

    values = numpy.asarray(values)
    if values.shape[:2] == (height, width):
        return values.astype(numpy.float32, copy=False)
    method = Image.NEAREST if nearest else Image.BILINEAR
    planes = values[..., None] if values.ndim == 2 else values
    out = [numpy.asarray(Image.fromarray(numpy.ascontiguousarray(planes[..., index], dtype=numpy.float32))
                         .resize((width, height), method), dtype=numpy.float32)
           for index in range(planes.shape[2])]
    return out[0] if values.ndim == 2 else numpy.stack(out, axis=-1)


def resize_mask(mask: numpy.ndarray, height: int, width: int) -> numpy.ndarray:
    return resize_map(numpy.asarray(mask, dtype=numpy.float32), height, width, nearest=True) > 0.5


def fit_scale_shift(source: numpy.ndarray, target: numpy.ndarray, mask: numpy.ndarray) -> tuple[float, float]:
    """The scale and shift that best map `source` onto `target` over `mask`.

    A least-squares fit is refitted without the pixels whose residual is more
    than three times the median residual, so a few disagreeing pixels do not
    move the alignment of the rest.
    """
    x = numpy.asarray(source, dtype=numpy.float64)[mask]
    y = numpy.asarray(target, dtype=numpy.float64)[mask]
    scale, shift = _least_squares(x, y)
    residual = numpy.abs(scale * x + shift - y)
    if residual.size:
        kept = residual <= 3 * float(numpy.median(residual)) + 1e-12
        if 0 < kept.sum() < kept.size:
            scale, shift = _least_squares(x[kept], y[kept])
    return scale, shift


def _least_squares(x: numpy.ndarray, y: numpy.ndarray) -> tuple[float, float]:
    if x.size == 0:
        return 1.0, 0.0
    if x.size < 2 or float(numpy.ptp(x)) == 0.0:
        # A constant source fixes only a scale.
        energy = float((x * x).sum())
        return (float((x * y).sum()) / energy if energy > 0 else 1.0), 0.0
    scale, shift = numpy.linalg.lstsq(numpy.stack([x, numpy.ones_like(x)], axis=1), y, rcond=None)[0]
    return float(scale), float(shift)


def relative_difference(source: numpy.ndarray, target: numpy.ndarray, mask: numpy.ndarray) -> numpy.ndarray:
    """|aligned source - target| / target per pixel, after fitting scale and shift; 0 outside `mask`."""
    mask = mask & (target > 0) & numpy.isfinite(source) & numpy.isfinite(target)
    scale, shift = fit_scale_shift(source, target, mask)
    difference = numpy.zeros(target.shape, dtype=numpy.float32)
    if scale <= 0:
        # Near and far reversed is complete disagreement, not a fit.
        difference[mask] = 1.0
        return difference
    difference[mask] = numpy.abs(scale * source[mask] + shift - target[mask]) / target[mask]
    return difference


def eroded(mask: numpy.ndarray) -> numpy.ndarray:
    """The mask without pixels that have an unmasked or missing 4-neighbour."""
    inner = numpy.zeros_like(mask)
    inner[1:-1, 1:-1] = (mask[1:-1, 1:-1] & mask[:-2, 1:-1] & mask[2:, 1:-1]
                         & mask[1:-1, :-2] & mask[1:-1, 2:])
    return inner


def normals_from_depth(depth: numpy.ndarray, focal_px: float, mask: numpy.ndarray) -> tuple[numpy.ndarray, numpy.ndarray]:
    """Camera-frame unit normals of the surface the depth describes, and where they are defined.

    Each pixel is lifted to a point with a pinhole camera of the given focal
    length centred on the image; the normal is the cross product of the
    surface's vertical and horizontal tangents, facing the camera. Normals are
    defined where the pixel and its four neighbours are all in `mask`.
    """
    height, width = depth.shape
    z = numpy.asarray(depth, dtype=numpy.float64)
    u = numpy.arange(width, dtype=numpy.float64)[None, :] - (width - 1) / 2
    v = numpy.arange(height, dtype=numpy.float64)[:, None] - (height - 1) / 2
    points = numpy.stack([u / focal_px * z, -v / focal_px * z, -z], axis=-1)
    along_u = numpy.gradient(points, axis=1)
    along_v = numpy.gradient(points, axis=0)
    normals = numpy.cross(along_v, along_u)
    length = numpy.linalg.norm(normals, axis=-1, keepdims=True)
    defined = eroded(mask) & (length[..., 0] > 0) & numpy.isfinite(length[..., 0])
    unit = numpy.where(defined[..., None], normals / numpy.where(length > 0, length, 1.0), 0.0)
    return unit.astype(numpy.float32), defined


def unit(normals: numpy.ndarray) -> numpy.ndarray:
    length = numpy.linalg.norm(normals, axis=-1, keepdims=True)
    return numpy.where(length > 1e-6, normals / numpy.maximum(length, 1e-6), 0.0).astype(numpy.float32)


def normal_angles(first: numpy.ndarray, second: numpy.ndarray) -> numpy.ndarray:
    """The angle in degrees between two unit normal maps."""
    cosine = numpy.clip((first * second).sum(axis=-1), -1.0, 1.0)
    return numpy.degrees(numpy.arccos(cosine)).astype(numpy.float32)


def summary(values: numpy.ndarray, mask: numpy.ndarray, within: float, name: str) -> dict:
    """The median of `values` over `mask` and the share of those pixels at most `within`."""
    selected = values[mask]
    if selected.size == 0:
        return {name: None, "agreement": None, "pixels": 0}
    return {name: round(float(numpy.median(selected)), 5), "agreement": round(float((selected <= within).mean()), 5),
            "pixels": int(selected.size)}


def combine(plan: Plan, settings: Settings, outcomes: dict, photograph_width: int) -> Analysis:
    """Combine each run's outcome, keyed by its Run, into the stored maps and their manifest.

    The maps take the depth run's processed size. Confidence is zero outside
    the subject mask and, inside it, the smallest per-pixel agreement of the
    checks that ran: mirrored and rescaled depth against the main depth, the
    other depth model against the chosen one, and predicted normals against
    normals derived from depth.
    """
    depth_run = plan.run_for("depth")
    depth_outcome = outcomes[depth_run]
    main = depth_outcome.geometry
    if main.depth is None:
        raise ValueError(f"{depth_run.entry} produced no depth")
    depth = numpy.asarray(main.depth, dtype=numpy.float32)
    height, width = depth.shape
    valid = numpy.ones((height, width), bool) if main.valid is None else numpy.asarray(main.valid, bool)
    mask_run = plan.run_for("mask")
    mask_geometry = outcomes[mask_run].geometry
    mask = valid & resize_mask(mask_geometry.valid, height, width)

    warnings = []
    if settings.focal_length_px is not None:
        focal_px = settings.focal_length_px * width / photograph_width
        focal = {"source": "given"}
    else:
        focal_run = plan.run_for("focal")
        focal_outcome = outcomes[focal_run]
        if focal_outcome.geometry.focal_px is None:
            raise ValueError(f"{focal_run.entry} produced no focal length")
        focal_px = focal_outcome.geometry.focal_px * width / focal_outcome.width
        focal = {"source": "estimated", "model": focal_run.entry}
    focal.update({"px": round(focal_px, 3), "photograph_px": round(focal_px * photograph_width / width, 3)})

    scores, factors, checks = {}, [], []
    for name, label in (("flip", "flip_consistency"), ("rescale", "rescale_consistency")):
        if name in depth_outcome.passes:
            other, other_valid = depth_outcome.passes[name]
            region = mask & numpy.asarray(other_valid, bool)
            difference = relative_difference(numpy.asarray(other, numpy.float32), depth, region)
            scores[label] = summary(difference, region, DEPTH_AGREEMENT, "median_relative_difference")
            # A subject pixel the other pass lost is unconfirmed, so it scores 0.
            factors.append(numpy.where(region, numpy.clip(1 - difference / DEPTH_TOLERANCE, 0, 1),
                                       numpy.where(mask, 0.0, 1.0)))
            checks.append(label)

    # A relative depth is aligned to the mask model's depth, when it has one,
    # before normals are derived, so its unknown offset does not bend them.
    shape_depth = depth
    if mask_run is not depth_run and mask_geometry.depth is not None:
        reference = resize_map(mask_geometry.depth, height, width)
        difference = relative_difference(depth, reference, mask)
        scores["model_disagreement"] = summary(difference, mask, DEPTH_AGREEMENT, "median_relative_difference")
        scores["model_disagreement"]["models"] = [depth_run.entry, mask_run.entry]
        factors.append(numpy.where(mask, numpy.clip(1 - difference / DEPTH_TOLERANCE, 0, 1), 1.0))
        checks.append("model_disagreement")
        if main.depth_kind == "relative":
            scale, shift = fit_scale_shift(depth, reference, mask & (reference > 0))
            if scale > 0:
                shape_depth = (scale * depth + shift).astype(numpy.float32)
    derived, defined = normals_from_depth(shape_depth, focal_px, mask)

    normals_run = plan.run_for("normals")
    if normals_run is not None:
        predicted = outcomes[normals_run].geometry.normals
        if predicted is None:
            raise ValueError(f"{normals_run.entry} produced no normals")
        normals = unit(resize_map(predicted, height, width))
        angles = normal_angles(derived, normals)
        region = defined & (numpy.linalg.norm(normals, axis=-1) > 0)
        scores["normal_agreement"] = summary(angles, region, NORMAL_AGREEMENT_DEGREES, "median_angle_degrees")
        factors.append(numpy.where(region, numpy.clip(1 - angles / NORMAL_TOLERANCE_DEGREES, 0, 1), 1.0))
        checks.append("normal_agreement")
        normals_source = normals_run.entry
    else:
        normals = derived
        normals_source = "depth"
        if main.depth_kind == "relative" and shape_depth is depth:
            warnings.append("Normals were derived from relative depth with an unknown offset, which can bend them.")

    confidence = numpy.zeros((height, width), numpy.float32)
    if factors:
        confidence[mask] = numpy.minimum.reduce(factors)[mask]
    else:
        confidence[mask] = 1.0
        warnings.append("No consistency check ran, so the confidence map is the subject mask alone.")
    scores["mask_fraction"] = round(float(mask.mean()), 5)
    scores["mean_confidence"] = round(float(confidence[mask].mean()), 5) if mask.any() else 0.0
    if mask.mean() < 0.01:
        warnings.append("The subject mask covers less than 1% of the photograph.")

    depth_kind = main.depth_kind or "relative"
    manifest = {
        "width": width,
        "height": height,
        "settings": settings.as_dict(),
        "runs": [run.as_dict() for run in plan.runs],
        "depth": {"model": depth_run.entry, "kind": depth_kind},
        "focal_length": focal,
        "scores": scores,
        "artifacts": {
            "depth": {"file": FILES["depth"], "format": "tiff", "sample": "float32", "bit_depth": 32, "channels": 1,
                      "units": "metres, estimated" if depth_kind == "metric" else "relative, unknown scale",
                      "convention": "distance along the optical axis, larger is farther; pixels outside the "
                                    "mask hold the model's prediction and are not subject surface"},
            "normals": {"file": FILES["normals"], "format": "png", "sample": "uint16", "bit_depth": 16,
                        "channels": 3, "frame": "x right, y up, z towards the camera",
                        "encoding": "c = 2 * p / 65535 - 1", "source": normals_source},
            "mask": {"file": FILES["mask"], "format": "png", "sample": "uint16", "bit_depth": 16, "channels": 1,
                     "encoding": "65535 subject, 0 background", "source": mask_run.entry},
            "confidence": {"file": FILES["confidence"], "format": "png", "sample": "uint16", "bit_depth": 16,
                           "channels": 1, "encoding": "confidence = p / 65535", "checks": checks},
        },
        "warnings": warnings,
    }
    if depth_kind == "metric":
        inside = depth[mask]
        manifest["metric_estimate"] = {
            "median_distance_m": round(float(numpy.median(inside)), 4) if inside.size else None,
            "warning": METRIC_WARNING}
        warnings.insert(0, METRIC_WARNING)
    return Analysis(depth, normals, mask, confidence, manifest)


def _require_float(values: numpy.ndarray, name: str) -> numpy.ndarray:
    values = numpy.asarray(values)
    if values.dtype.kind != "f":
        raise MapFormatError(f"the {name} map must be floating point, not {values.dtype}")
    return values


def write_depth(depth: numpy.ndarray, path: Path) -> None:
    """A 32-bit floating-point greyscale TIFF."""
    from PIL import Image

    values = numpy.ascontiguousarray(_require_float(depth, "depth"), dtype=numpy.float32)
    if values.ndim != 2:
        raise MapFormatError("the depth map must have one channel")
    Image.fromarray(values).save(path, format="TIFF")


def write_unit(values: numpy.ndarray, path: Path, name: str) -> None:
    """A map of values from 0 to 1 as a 16-bit greyscale PNG."""
    values = _require_float(values, name)
    if values.ndim != 2:
        raise MapFormatError(f"the {name} map must have one channel")
    _write_grey16(numpy.round(numpy.clip(values, 0.0, 1.0) * 65535.0).astype(numpy.uint16), path)


def write_mask(mask: numpy.ndarray, path: Path) -> None:
    """A boolean mask as a 16-bit greyscale PNG, 65535 inside."""
    mask = numpy.asarray(mask)
    if mask.dtype != bool or mask.ndim != 2:
        raise MapFormatError(f"the mask must be a two-dimensional boolean array, not {mask.dtype}")
    _write_grey16(numpy.where(mask, 65535, 0).astype(numpy.uint16), path)


def write_normals(normals: numpy.ndarray, path: Path) -> None:
    """Unit camera-frame normals, H x W x 3, as a 16-bit RGB PNG."""
    from estimate_depth import write_normals as write

    normals = _require_float(normals, "normals")
    if normals.ndim != 3 or normals.shape[2] != 3:
        raise MapFormatError("the normal map must have three channels")
    write(normals.transpose(2, 0, 1), path)


def _write_grey16(levels: numpy.ndarray, path: Path) -> None:
    from PIL import Image

    Image.fromarray(levels).save(path, format="PNG")


def stored_format(path: Path) -> dict:
    """The stored sample format of a PNG or TIFF map, read from its own header."""
    data = Path(path).read_bytes()[:64]
    if data.startswith(b"\x89PNG\r\n\x1a\n") and data[12:16] == b"IHDR":
        bit_depth, colour = data[24], data[25]
        channels = {0: 1, 2: 3, 4: 2, 6: 4}.get(colour)
        if channels is None:
            raise MapFormatError(f"{Path(path).name} is a palette PNG")
        return {"format": "png", "bit_depth": bit_depth, "channels": channels,
                "sample": f"uint{bit_depth}"}
    if data[:4] in (b"II*\x00", b"MM\x00*"):
        from PIL import Image

        with Image.open(path) as image:
            bits = image.tag_v2.get(258, (8,))
            bits = bits[0] if isinstance(bits, tuple) else bits
            sample = image.tag_v2.get(339, 1)
            sample = sample[0] if isinstance(sample, tuple) else sample
            channels = len(image.getbands())
        kind = {1: "uint", 2: "int", 3: "float"}.get(sample, "unknown")
        return {"format": "tiff", "bit_depth": int(bits), "channels": channels, "sample": f"{kind}{bits}"}
    raise MapFormatError(f"{Path(path).name} is neither a PNG nor a TIFF")


def verify(path: Path, expected: dict) -> None:
    """Refuse a stored map with fewer than 16 bits per sample or another format than its manifest says."""
    found = stored_format(path)
    if found["bit_depth"] < 16:
        raise MapFormatError(f"{Path(path).name} has {found['bit_depth']}-bit samples; maps need 16 or more")
    for key in ("format", "bit_depth", "channels", "sample"):
        if found[key] != expected[key]:
            raise MapFormatError(f"{Path(path).name} is stored as {found}, not as its manifest says")


def write_all(analysis: Analysis, directory: Path) -> dict:
    """Write every map into `directory`, check each stored format and return the manifest."""
    directory = Path(directory)
    artifacts = analysis.manifest["artifacts"]
    write_depth(analysis.depth, directory / FILES["depth"])
    write_normals(analysis.normals, directory / FILES["normals"])
    write_mask(analysis.mask, directory / FILES["mask"])
    write_unit(analysis.confidence, directory / FILES["confidence"], "confidence")
    for name, description in artifacts.items():
        verify(directory / description["file"], description)
    return analysis.manifest


def read_png16(path: Path) -> numpy.ndarray:
    """An H x W x 3 uint16 array from an unfiltered 16-bit RGB PNG, as write_normals stores it."""
    import zlib

    data, at, chunks = Path(path).read_bytes(), 8, {}
    while at < len(data):
        length, kind = struct.unpack(">I4s", data[at:at + 8])
        chunks.setdefault(kind, []).append(data[at + 8:at + 8 + length])
        at += 12 + length
    width, height, bit_depth, colour = struct.unpack(">IIBB", chunks[b"IHDR"][0][:10])
    if (bit_depth, colour) != (16, 2):
        raise MapFormatError(f"{Path(path).name} is not a 16-bit RGB PNG")
    rows = numpy.frombuffer(zlib.decompress(b"".join(chunks[b"IDAT"])), numpy.uint8).reshape(height, 1 + width * 6)
    if rows[:, 0].any():
        raise MapFormatError(f"{Path(path).name} uses PNG row filters")
    return numpy.ascontiguousarray(rows[:, 1:]).view(">u2").reshape(height, width, 3).astype(numpy.uint16)
