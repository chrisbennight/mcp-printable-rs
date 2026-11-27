"""Scores for depth maps, normal maps and relief height fields.

Every function takes NumPy arrays and depends on NumPy alone, so the same code
runs in the benchmark container, in continuous integration and in the service's
comparison tools.

Conventions:
  depth    (H, W) distance along the camera's viewing axis; positive.
  normals  (H, W, 3) unit vectors in the camera frame: x right, y up, z towards
           the camera.
  focal    focal length in pixels; the principal point is the image centre.
  mask     (H, W) bool; True where a pixel takes part in a score.
"""

from __future__ import annotations

import math
from dataclasses import dataclass

import numpy as np

EDGE_RATIOS = (1.05, 1.10, 1.15, 1.20, 1.25)

# Kinds of depth output and the alignment each allows before it is scored.
METRIC = "metric"  # metres; scored after one scale factor
RELATIVE = "relative"  # affine in depth; scored after scale and offset
LOG_RELATIVE = "logrel"  # affine in log depth; scored after scale and offset in log depth
KINDS = (METRIC, RELATIVE, LOG_RELATIVE)


def dilate(mask: np.ndarray, iterations: int = 1) -> np.ndarray:
    """Binary dilation with a 3 x 3 square."""
    out = mask.astype(bool)
    for _ in range(iterations):
        grown = out.copy()
        grown[1:, :] |= out[:-1, :]
        grown[:-1, :] |= out[1:, :]
        grown[:, 1:] |= out[:, :-1]
        grown[:, :-1] |= out[:, 1:]
        grown[1:, 1:] |= out[:-1, :-1]
        grown[:-1, :-1] |= out[1:, 1:]
        grown[1:, :-1] |= out[:-1, 1:]
        grown[:-1, 1:] |= out[1:, :-1]
        out = grown
    return out


def erode(mask: np.ndarray, iterations: int = 1) -> np.ndarray:
    """Binary erosion with a 3 x 3 square; pixels outside the image count as False."""
    if iterations == 0:
        return mask.astype(bool)
    padded = np.pad(mask.astype(bool), iterations, constant_values=False)
    eroded = ~dilate(~padded, iterations)
    return eroded[iterations:-iterations, iterations:-iterations]


def align(pred: np.ndarray, truth: np.ndarray, mask: np.ndarray, kind: str) -> tuple[np.ndarray, dict]:
    """The prediction aligned to the truth over the mask, with the freedom its kind allows.

    Metric depth gets one scale factor (the median ratio). Relative depth gets a
    least-squares scale and offset. Log-relative depth gets a least-squares scale
    and offset in log depth. Returns the aligned depth and the fitted parameters.
    """
    if kind not in KINDS:
        raise ValueError(f"unknown depth kind {kind!r}")
    p = pred[mask].astype(np.float64)
    g = truth[mask].astype(np.float64)
    if p.size < 2:
        raise ValueError("alignment needs at least two pixels")
    if kind == METRIC:
        scale = float(np.median(g / np.maximum(p, 1e-12)))
        return (pred * scale).astype(np.float64), {"scale": scale}
    if kind == RELATIVE:
        scale, offset = _nonnegative_fit(p, g)
        aligned = np.maximum(pred * scale + offset, 1e-9)
        return aligned.astype(np.float64), {"scale": scale, "offset": offset}
    scale, offset = _nonnegative_fit(p, np.log(g))
    return np.exp(pred * scale + offset).astype(np.float64), {"scale": scale, "offset": offset}


def _nonnegative_fit(x: np.ndarray, y: np.ndarray) -> tuple[float, float]:
    """Least-squares y ~ scale * x + offset with scale >= 0.

    A negative scale would swap near and far, letting an inverted prediction
    fit perfectly; constrained, such a prediction fits as a flat one.
    """
    design = np.stack([x, np.ones_like(x)], axis=1)
    (scale, offset), *_ = np.linalg.lstsq(design, y, rcond=None)
    if scale < 0:
        return 0.0, float(np.mean(y))
    return float(scale), float(offset)


def shape_kind(kind: str) -> str:
    """The alignment used when only shape matters (relief regions, like-for-like depth error).

    Relief height is chosen by the user in millimetres, so every model gets a
    scale and an offset; log-relative output keeps its log-depth alignment.
    """
    return LOG_RELATIVE if kind == LOG_RELATIVE else RELATIVE


def depth_errors(depth: np.ndarray, truth: np.ndarray, mask: np.ndarray) -> dict:
    """Mean absolute relative error and the share of pixels within 5 % (both aligned beforehand)."""
    d = depth[mask]
    g = truth[mask]
    ratio = np.maximum(d / g, g / d)
    return {"absrel": float(np.mean(np.abs(d - g) / g)), "delta_1.05": float(np.mean(ratio < 1.05))}


def points(depth: np.ndarray, focal: float) -> np.ndarray:
    """Camera-frame points (x right, y up, z towards the camera) for a depth map."""
    h, w = depth.shape
    v, u = np.mgrid[0:h, 0:w].astype(np.float64)
    cx, cy = (w - 1) / 2.0, (h - 1) / 2.0
    z = depth.astype(np.float64)
    return np.stack([(u - cx) * z / focal, -(v - cy) * z / focal, -z], axis=-1)


def normals_from_depth(depth: np.ndarray, focal: float) -> np.ndarray:
    """Unit surface normals facing the camera, from central differences of the point map."""
    p = points(depth, focal)
    du = np.gradient(p, axis=1)
    dv = np.gradient(p, axis=0)
    n = np.cross(du, dv)
    n /= np.maximum(np.linalg.norm(n, axis=-1, keepdims=True), 1e-12)
    facing_away = np.sum(n * p, axis=-1) > 0
    n[facing_away] *= -1
    return n


def angle_deg(a: np.ndarray, b: np.ndarray) -> np.ndarray:
    """Per-pixel angle in degrees between two unit-vector fields."""
    return np.degrees(np.arccos(np.clip(np.sum(a * b, axis=-1), -1.0, 1.0)))


def normal_errors(pred: np.ndarray, truth: np.ndarray, mask: np.ndarray) -> dict:
    """Median angle and the share of pixels within 11.25 degrees."""
    a = angle_deg(pred, truth)[mask]
    return {"median_deg": float(np.median(a)), "within_11": float(np.mean(a < 11.25))}


def edge_map(depth: np.ndarray, ratio: float) -> np.ndarray:
    """Pixels on either side of a jump in depth larger than the given ratio."""
    log_depth = np.log(np.maximum(depth, 1e-12))
    threshold = math.log(ratio)
    edges = np.zeros(depth.shape, bool)
    dx = np.abs(np.diff(log_depth, axis=1)) > threshold
    dy = np.abs(np.diff(log_depth, axis=0)) > threshold
    edges[:, 1:] |= dx
    edges[:, :-1] |= dx
    edges[1:, :] |= dy
    edges[:-1, :] |= dy
    return edges


def boundary_f1(pred: np.ndarray, truth: np.ndarray, mask: np.ndarray) -> float | None:
    """Mean F1 of depth-jump edges over several jump ratios, with one pixel of tolerance.

    A ratio at which the truth has no jumps scores 0 when the prediction invents
    jumps there, and is skipped when neither has any. Returns None when no
    ratio is scored.
    """
    inner = erode(mask, 2)
    scores = []
    for ratio in EDGE_RATIOS:
        g = edge_map(truth, ratio) & inner
        p = edge_map(pred, ratio) & inner
        if not g.any():
            if p.any():
                scores.append(0.0)
            continue
        precision = (p & dilate(g)).sum() / max(p.sum(), 1)
        recall = (g & dilate(p)).sum() / g.sum()
        scores.append(0.0 if precision + recall == 0 else 2 * precision * recall / (precision + recall))
    return float(np.mean(scores)) if scores else None


def smooth_mask(truth: np.ndarray, valid: np.ndarray) -> np.ndarray:
    """Valid pixels away from depth jumps, where surface normals are well defined."""
    filled = np.where(valid, truth, np.max(truth[valid]) * 10)
    return valid & ~dilate(edge_map(filled, EDGE_RATIOS[0]), 2)


def plane_residual(values: np.ndarray, mask: np.ndarray) -> np.ndarray:
    """Values minus their least-squares plane in image coordinates over the mask."""
    h, w = values.shape
    v, u = np.mgrid[0:h, 0:w]
    design = np.stack([u[mask], v[mask], np.ones(int(mask.sum()))], axis=1).astype(np.float64)
    coef, *_ = np.linalg.lstsq(design, values[mask].astype(np.float64), rcond=None)
    return values - (coef[0] * u + coef[1] * v + coef[2])


def relief_error(pred: np.ndarray, truth: np.ndarray, region: np.ndarray, kind: str) -> dict:
    """Height error in a relief region, in the truth's units, after shape alignment and plane removal.

    Also returns the region's relief depth (1st to 99th percentile of height above
    its best-fit plane) and the error as a percentage of it.
    """
    aligned, _ = align(pred, truth, region, shape_kind(kind))
    residual = plane_residual(aligned - truth, region)
    height = plane_residual(-truth, region)
    rmse = float(np.sqrt(np.mean(residual[region] ** 2)))
    relief = float(np.percentile(height[region], 99) - np.percentile(height[region], 1))
    return {"rmse": rmse, "relief": relief, "rmse_pct": 100.0 * rmse / relief if relief > 0 else None}


def scene_scores(pred: dict, truth: dict) -> dict:
    """Every score of one prediction against one scene's truth, as the benchmark report tabulates it.

    pred: "depth" and its "kind", optional "normals" and "focal_px".
    truth: "depth", "normals", "valid", "focal" and "region" (None when the
    scene has no relief region).
    """
    valid = truth["valid"]
    smooth = smooth_mask(truth["depth"], valid)
    kind = pred["kind"]
    aligned, _ = align(pred["depth"], truth["depth"], valid, kind)
    shaped, _ = align(pred["depth"], truth["depth"], valid, shape_kind(kind))
    row = {"kind": kind,
           "absrel": depth_errors(shaped, truth["depth"], valid)["absrel"],
           "edge_f1": boundary_f1(aligned, truth["depth"], valid),
           "depth_normal_deg": normal_errors(
               normals_from_depth(aligned, truth["focal"]), truth["normals"], smooth)["median_deg"]}
    if kind == METRIC:
        row["scale_error_pct"] = float((np.median(pred["depth"][valid] / truth["depth"][valid]) - 1) * 100)
    row["focal_error_pct"] = focal_error_pct(pred.get("focal_px"), truth["focal"])
    if pred.get("normals") is not None:
        row["normal_deg"] = normal_errors(pred["normals"], truth["normals"], smooth)["median_deg"]
    region = truth.get("region")
    if region is not None and region.sum() > 100:
        relief = relief_error(pred["depth"], truth["depth"], region, kind)
        row.update({"relief_rmse_mm": relief["rmse"] * 1000, "relief_mm": relief["relief"] * 1000,
                    "relief_pct": relief["rmse_pct"]})
    return row


def focal_error_pct(estimate: float | None, truth: float | None) -> float | None:
    if not estimate or not truth:
        return None
    return 100.0 * (estimate / truth - 1.0)


def focal_from_35mm(focal_35mm: float, width: int, height: int) -> float:
    """Focal length in pixels from an EXIF 35 mm-equivalent focal length (diagonal match)."""
    return float(focal_35mm) * math.hypot(width, height) / math.hypot(36.0, 24.0)


@dataclass(frozen=True)
class Interval:
    mean: float
    low: float
    high: float


def bootstrap_mean(values: list[float], samples: int = 2000, seed: int = 0) -> Interval | None:
    """Mean with a 95 % percentile bootstrap interval, resampling whole scenes."""
    data = np.asarray([v for v in values if v is not None and not math.isnan(v)], dtype=np.float64)
    if data.size == 0:
        return None
    if data.size == 1:
        return Interval(float(data[0]), float(data[0]), float(data[0]))
    rng = np.random.default_rng(seed)
    means = data[rng.integers(0, data.size, size=(samples, data.size))].mean(axis=1)
    return Interval(float(data.mean()), float(np.percentile(means, 2.5)), float(np.percentile(means, 97.5)))
