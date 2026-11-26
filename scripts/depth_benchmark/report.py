"""Score every run and write the evidence: tables, metrics JSON and comparison sheets.

Usage: python report.py [--out DIR]
Reads work/gt/<scene>/{passes.exr,camera.json}, work/inputs/*.png and
results/<tier>/<model>/*.npz; writes <out>/report.md, <out>/metrics.json and
<out>/sheets/*.jpg (default out: results/).
"""

from __future__ import annotations

import argparse
import json
import math
from collections import defaultdict
from pathlib import Path

import numpy as np
from PIL import Image, ImageDraw, ImageFont
from scipy import ndimage

import common
import scoring

CPU_LIMIT_SECONDS = 15 * 60
FUSIONS = [("dp2-h", "moge3-vitg"), ("moge3-vitg", "moge3-vitg"), ("da3-nested-giant", "moge3-vitg"),
           ("marigold-v2", "marigold-v2"), ("moge3-vitl", "marigold-v2")]
FUSION_SIGMAS = [4.0, 16.0, 64.0, 256.0]  # 256 means normals only, levelled by the mean depth


def region(scene: str, world: np.ndarray, valid: np.ndarray) -> np.ndarray | None:
    """The part of each scene whose relief is scored, from world-space positions in metres."""
    x, y, z = world[..., 0], world[..., 1], world[..., 2]
    r = np.hypot(x, y)
    rules = {
        "relief": (np.abs(x) < 0.115) & (np.abs(y) < 0.085) & (z > 0.0055),
        "relief-shallow": (np.abs(x) < 0.115) & (np.abs(y) < 0.085) & (z > 0.0055),
        "lettering": (np.abs(x) < 0.115) & (np.abs(y) < 0.055) & (z > 0.002),
        "coin": (r < 0.0195) & (z > 0.002),
        "coin-engraved": (r < 0.0195) & (z > 0.0005),
        "medal-metal": (r < 0.033) & (z > 0.003),
        "head": (z > 0.62) & (r < 0.4),
    }
    return valid & rules[scene] if scene in rules else None


# ---------------------------------------------------------------- ground truth
def read_exr(path: Path) -> dict[str, np.ndarray]:
    import OpenEXR

    channels = {}
    with OpenEXR.File(str(path), separate_channels=True) as f:
        for part in f.parts:
            for name, ch in part.channels.items():
                channels[name] = np.asarray(ch.pixels, np.float32)
    return channels


def layer(channels: dict, name: str) -> np.ndarray:
    keys = sorted(k for k in channels if k.split(".")[-2:-1] == [name])
    if not keys:
        raise KeyError(f"no {name} pass among {sorted(channels)}")
    return np.stack([channels[k] for k in keys], -1)


def load_truth(scene: str) -> dict:
    d = common.GT / scene
    cam = json.loads((d / "camera.json").read_text())
    ch = read_exr(d / "passes.exr")
    world, normal_world = layer(ch, "Position"), layer(ch, "Normal")
    m = np.array(cam["matrix_world"], np.float64)
    inv = np.linalg.inv(m)
    pc = world.reshape(-1, 3).astype(np.float64) @ inv[:3, :3].T + inv[:3, 3]
    depth = (-pc[:, 2]).reshape(world.shape[:2])
    rot = m[:3, :3] / np.linalg.norm(m[:3, :3], axis=0, keepdims=True)
    normals = (normal_world.reshape(-1, 3).astype(np.float64) @ rot).reshape(normal_world.shape)
    normals /= np.maximum(np.linalg.norm(normals, axis=-1, keepdims=True), 1e-12)
    valid = (np.abs(world).sum(-1) > 0) & (depth > 0) & np.isfinite(depth)
    # Blender may store EXR rows bottom-up: orient the passes to match the display image.
    shown = np.asarray(Image.open(d / "rgb.png").convert("L"), np.float32)
    combined = layer(ch, "Combined")[..., :3].mean(-1)
    if np.corrcoef(combined[::-1].ravel(), shown.ravel())[0, 1] > np.corrcoef(combined.ravel(), shown.ravel())[0, 1]:
        depth, normals, valid, world = depth[::-1], normals[::-1], valid[::-1], world[::-1]
    return {"depth": depth, "normals": normals.astype(np.float32), "valid": valid,
            "region": region(scene, world, valid), "focal": float(cam["focal_px"])}


# -------------------------------------------------------------------- fusion
def integrate(p: np.ndarray, q: np.ndarray) -> np.ndarray:
    """Frankot-Chellappa integration of gradients p (d/du) and q (d/dv) with mirrored boundaries."""
    h0, w0 = p.shape
    pe = np.block([[p, -p[:, ::-1]], [p[::-1, :], -p[::-1, ::-1]]])
    qe = np.block([[q, q[:, ::-1]], [-q[::-1, :], -q[::-1, ::-1]]])
    wu, wv = np.meshgrid(2 * np.pi * np.fft.fftfreq(pe.shape[1]), 2 * np.pi * np.fft.fftfreq(pe.shape[0]))
    denom = wu**2 + wv**2
    denom[0, 0] = 1
    z = (-1j * wu * np.fft.fft2(pe) - 1j * wv * np.fft.fft2(qe)) / denom
    z[0, 0] = 0
    return np.real(np.fft.ifft2(z))[:h0, :w0]


def fuse(depth: np.ndarray, normals: np.ndarray, focal: float, sigma: float) -> np.ndarray:
    """Depth with broad shape from the depth map and detail from integrated normals.

    Gradients come from the normals except across depth jumps, where the depth
    map's own gradient is used, so integration does not bridge occlusions.
    """
    height = -depth.astype(np.float64)
    nz = np.clip(normals[..., 2], 0.1, None)
    pixel = depth / focal
    p = -normals[..., 0] / nz * pixel
    q = normals[..., 1] / nz * pixel
    jump = scoring.dilate(scoring.edge_map(depth, 1.02))
    p = np.where(jump, np.gradient(height, axis=1), p)
    q = np.where(jump, np.gradient(height, axis=0), q)
    detail = integrate(p, q)
    if sigma >= 256:
        fused = detail + (height.mean() - detail.mean())
    else:
        fused = ndimage.gaussian_filter(height, sigma) + detail - ndimage.gaussian_filter(detail, sigma)
    return -fused


# ---------------------------------------------------------------------- loading
def load_run(tier: str, model: str, image: str) -> dict | None:
    f = common.RESULTS / tier / model / f"{image}.npz"
    if not f.exists():
        return None
    with np.load(f) as d:
        return {k: (d[k].item() if d[k].shape == () else d[k]) for k in d.files}


def models(tier: str) -> list[str]:
    root = common.RESULTS / tier
    return sorted(p.name for p in root.iterdir() if p.is_dir()) if root.exists() else []


def runs(name: str) -> list[dict]:
    f = common.RESULTS / name
    return [json.loads(line) for line in f.read_text().splitlines()] if f.exists() else []


# ---------------------------------------------------------------------- scoring
def score_scene(scene: str, truth: dict, tier: str) -> list[dict]:
    rows = []
    valid = truth["valid"]
    smooth = scoring.smooth_mask(truth["depth"], valid)
    for model in models(tier):
        pred = load_run(tier, model, f"gt-{scene}")
        if pred is None:
            continue
        kind = pred["kind"]
        aligned, _ = scoring.align(pred["depth"], truth["depth"], valid, kind)
        shaped, _ = scoring.align(pred["depth"], truth["depth"], valid, scoring.shape_kind(kind))
        row = {"tier": tier, "scene": scene, "model": model, "kind": kind,
               "absrel": scoring.depth_errors(shaped, truth["depth"], valid)["absrel"],
               "edge_f1": scoring.boundary_f1(aligned, truth["depth"], valid),
               "depth_normal_deg": scoring.normal_errors(
                   scoring.normals_from_depth(aligned, truth["focal"]), truth["normals"], smooth)["median_deg"]}
        if kind == scoring.METRIC:
            row["scale_error_pct"] = float((np.median(pred["depth"][valid] / truth["depth"][valid]) - 1) * 100)
        row["focal_error_pct"] = scoring.focal_error_pct(pred.get("focal_px"), truth["focal"])
        if "normals" in pred:
            row["normal_deg"] = scoring.normal_errors(pred["normals"], truth["normals"], smooth)["median_deg"]
        if truth["region"] is not None and truth["region"].sum() > 100:
            relief = scoring.relief_error(pred["depth"], truth["depth"], truth["region"], kind)
            row.update({"relief_rmse_mm": relief["rmse"] * 1000, "relief_mm": relief["relief"] * 1000,
                        "relief_pct": relief["rmse_pct"]})
        rows.append(row)
    return rows


def score_fusion(scene: str, truth: dict, tier: str) -> list[dict]:
    rows = []
    reg = truth["region"]
    if reg is None:
        return rows
    smooth = scoring.smooth_mask(truth["depth"], truth["valid"]) & reg
    for depth_model, normal_model in FUSIONS:
        d, n = load_run(tier, depth_model, f"gt-{scene}"), load_run(tier, normal_model, f"gt-{scene}")
        if d is None or n is None or "normals" not in n:
            continue
        base, _ = scoring.align(d["depth"], truth["depth"], truth["valid"], d["kind"])
        candidates = [("depth only", None, base)]
        candidates += [("fused", s, fuse(base, n["normals"], truth["focal"], s)) for s in FUSION_SIGMAS]
        for label, sigma, z in candidates:
            relief = scoring.relief_error(z, truth["depth"], reg, scoring.RELATIVE)
            angle = scoring.normal_errors(scoring.normals_from_depth(z, truth["focal"]), truth["normals"], smooth)
            rows.append({"tier": tier, "scene": scene, "depth": depth_model,
                         "normals": normal_model if label == "fused" else "none", "sigma": sigma,
                         "relief_rmse_mm": relief["rmse"] * 1000, "normal_deg": angle["median_deg"]})
    return rows


def score_photos(tier: str) -> list[dict]:
    rows = []
    for f in common.inputs():
        if f.stem.startswith("gt-"):
            continue
        w, h = Image.open(f).size
        exif = exif_focal(f.stem, w, h)
        for model in models(tier):
            pred = load_run(tier, model, f.stem)
            if pred is None:
                continue
            row = {"tier": tier, "image": f.stem, "model": model,
                   "focal_error_pct": scoring.focal_error_pct(pred.get("focal_px"), exif)}
            if pred["kind"] == scoring.METRIC:
                z = pred["depth"]
                ok = np.isfinite(z) & (z > 0)
                res = scoring.plane_residual(z, ok)
                row["non_planar_pct"] = float((np.percentile(res[ok], 95) - np.percentile(res[ok], 5))
                                              / np.median(z[ok]) * 100)
                row["median_depth_m"] = float(np.median(z[ok]))
            rows.append(row)
    return rows


def exif_focal(name: str, w: int, h: int) -> float | None:
    src = common.IMAGES / f"{name}.jpg"
    if not src.exists():
        return None
    with Image.open(src) as im:
        f35 = im.getexif().get_ifd(0x8769).get(41989)
    return scoring.focal_from_35mm(float(f35), w, h) if f35 else None


# ------------------------------------------------------------------------- costs
def cost_table() -> list[dict]:
    rows = []
    groups = defaultdict(list)
    for r in runs("runs.jsonl"):
        groups[(r["tier"], r["model"])].append(r)
    timing = defaultdict(list)
    for r in runs("timing2048.jsonl"):
        timing[(r["tier"], r["model"])].append(r)
    failures = defaultdict(list)
    for r in runs("failures.jsonl"):
        failures[(r["tier"], r["model"])].append(r)
    for key in sorted(set(groups) | set(timing) | set(failures)):
        tier, model = key
        g, t, fails = groups.get(key, []), timing.get(key, []), failures.get(key, [])
        row = {"tier": tier, "model": model, "images": len(g), "failures": len(fails)}
        if g:
            secs = [x["seconds"] for x in g]
            row.update({"median_s": float(np.median(secs)), "max_s": float(max(secs)),
                        "gpu_peak_gb": max(x["gpu_peak_gb"] for x in g), "rss_peak_gb": max(x["rss_peak_gb"] for x in g),
                        "read_gb_max": max(x["read_gb"] for x in g),
                        "within_15_min": all(s <= CPU_LIMIT_SECONDS for s in secs)})
        if t:
            row.update({"median_s_2048": float(np.median([x["seconds"] for x in t])),
                        "gpu_peak_gb_2048": max(x["gpu_peak_gb"] for x in t)})
        if fails:
            row["failure"] = f'{fails[0]["error"]}: {fails[0]["message"][:120]}'
        rows.append(row)
    return rows


# ---------------------------------------------------------------------- sheets
def turbo(x: np.ndarray) -> np.ndarray:
    import matplotlib

    return (matplotlib.colormaps["turbo"](np.clip(x, 0, 1))[..., :3] * 255).astype(np.uint8)


def depth_vis(z: np.ndarray) -> np.ndarray:
    v = np.log(np.maximum(z, 1e-6))
    lo, hi = np.percentile(v, 2), np.percentile(v, 98)
    return turbo(1 - (v - lo) / max(hi - lo, 1e-9))


def normal_vis(n: np.ndarray) -> np.ndarray:
    return ((n * 0.5 + 0.5).clip(0, 1) * 255).astype(np.uint8)


def shade(n: np.ndarray) -> np.ndarray:
    light = np.array([-0.45, 0.55, 0.7])
    s = (n @ (light / np.linalg.norm(light))).clip(0, 1) ** 0.9
    return (np.repeat(s[..., None], 3, -1) * 235 + 10).astype(np.uint8)


def grid(rows: list[list[np.ndarray]], labels: list[str], columns: list[str], width: int) -> Image.Image:
    font = ImageFont.load_default(size=16)
    tiles = [[Image.fromarray(a).resize((width, round(a.shape[0] * width / a.shape[1])), Image.LANCZOS) for a in r]
             for r in rows]
    th = max(t.height for r in tiles for t in r)
    left, top = 180, 26
    sheet = Image.new("RGB", (left + width * max(len(r) for r in tiles), top + th * len(tiles)), (24, 24, 24))
    draw = ImageDraw.Draw(sheet)
    for j, c in enumerate(columns):
        draw.text((left + j * width + 6, 4), c, fill=(230, 230, 230), font=font)
    for i, r in enumerate(tiles):
        draw.text((6, top + i * th + 6), labels[i], fill=(230, 230, 230), font=font)
        for j, t in enumerate(r):
            sheet.paste(t, (left + j * width, top + i * th))
    return sheet


def depth_for_display(pred: dict) -> np.ndarray:
    return np.exp(pred["depth"]) if pred["kind"] == scoring.LOG_RELATIVE else pred["depth"]


def image_sheet(tier: str, image: str, truth: dict | None) -> Image.Image:
    rgb = common.load_rgb(common.INPUTS / f"{image}.png")
    blank = np.zeros_like(rgb)
    w = rgb.shape[1]
    if truth is not None:
        rows = [[rgb, depth_vis(truth["depth"]), normal_vis(truth["normals"]),
                 shade(scoring.normals_from_depth(truth["depth"], truth["focal"])), blank]]
        labels = ["input / truth"]
    else:
        rows, labels = [[rgb, blank, blank, blank, blank]], ["input"]
    for model in models(tier):
        pred = load_run(tier, model, image)
        if pred is None:
            continue
        if truth is not None:
            z, _ = scoring.align(pred["depth"], truth["depth"], truth["valid"], pred["kind"])
            focal = truth["focal"]
        else:
            z, focal = depth_for_display(pred), pred.get("focal_px") or 0.9 * w
        from_depth = scoring.normals_from_depth(z, focal)
        own = pred.get("normals")
        rows.append([depth_vis(z), normal_vis(own) if own is not None else blank,
                     shade(own) if own is not None else blank, shade(from_depth),
                     turbo(scoring.angle_deg(from_depth, truth["normals"]) / 45) if truth is not None else blank])
        labels.append(model)
    columns = ["depth (near = red)", "model normals", "lit (model normals)", "lit (from depth)",
               "normal error 0-45 deg" if truth is not None else ""]
    return grid(rows, labels, columns, 300)


def overview(tier: str, images: list[str]) -> Image.Image:
    mlist = models(tier)
    rows = []
    for image in images:
        rgb = common.load_rgb(common.INPUTS / f"{image}.png")
        row = [rgb]
        for model in mlist:
            pred = load_run(tier, model, image)
            if pred is None:
                row.append(np.zeros_like(rgb))
                continue
            n = pred.get("normals")
            if n is None:
                n = scoring.normals_from_depth(depth_for_display(pred), pred.get("focal_px") or 0.9 * rgb.shape[1])
            row.append(shade(n))
        rows.append(row)
    return grid(rows, images, ["input"] + mlist, 220)


# ---------------------------------------------------------------------- tables
def fmt(v, spec="{:.2f}"):
    if v is None or (isinstance(v, float) and math.isnan(v)):
        return ""
    if isinstance(v, bool):
        return "yes" if v else "no"
    return spec.format(v) if isinstance(v, float) else str(v)


def interval(values: list) -> str:
    ci = scoring.bootstrap_mean(values)
    return "" if ci is None else f"{ci.mean:.2f} ({ci.low:.2f}-{ci.high:.2f})"


def table(rows: list[dict], columns: list[tuple[str, str]]) -> str:
    out = ["| " + " | ".join(c[0] for c in columns) + " |", "|" + " --- |" * len(columns)]
    for r in rows:
        out.append("| " + " | ".join(fmt(r.get(key)) for _, key in columns) + " |")
    return "\n".join(out)


def summary(rows: list[dict], tier: str) -> list[dict]:
    """Per-model means over scenes with 95 % bootstrap intervals."""
    out = []
    for model in sorted({r["model"] for r in rows if r["tier"] == tier}):
        r = [x for x in rows if x["tier"] == tier and x["model"] == model]
        out.append({
            "model": model,
            "absrel_pct": interval([x["absrel"] * 100 for x in r]),
            "depth_normal_deg": interval([x["depth_normal_deg"] for x in r]),
            "normal_deg": interval([x.get("normal_deg") for x in r]),
            "relief_pct": interval([x.get("relief_pct") for x in r]),
            "edge_f1": interval([x.get("edge_f1") for x in r]),
            "focal_abs_pct": interval([abs(x["focal_error_pct"]) if x.get("focal_error_pct") is not None else None
                                       for x in r]),
            "scenes": len(r),
        })
    return out


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--out", type=Path, default=common.RESULTS)
    args = parser.parse_args()
    sheets = args.out / "sheets"
    sheets.mkdir(parents=True, exist_ok=True)
    scenes = sorted(p.name for p in common.GT.iterdir() if (p / "passes.exr").exists())
    tiers = [t for t in common.TIERS if models(t)]
    scene_rows, fusion_rows, photo_rows = [], [], []
    for scene in scenes:
        truth = load_truth(scene)
        for tier in tiers:
            scene_rows += score_scene(scene, truth, tier)
            fusion_rows += score_fusion(scene, truth, tier)
        if "gpu" in tiers:
            image_sheet("gpu", f"gt-{scene}", truth).save(sheets / f"gt-{scene}.jpg", quality=88)
    for tier in tiers:
        photo_rows += score_photos(tier)
    photos = [f.stem for f in common.inputs() if not f.stem.startswith("gt-")]
    if "gpu" in tiers:
        for image in photos:
            image_sheet("gpu", image, None).save(sheets / f"{image}.jpg", quality=88)
        for i in range(0, len(photos), 5):
            overview("gpu", photos[i:i + 5]).save(sheets / f"overview-{i // 5 + 1}.jpg", quality=88)
        overview("gpu", [f"gt-{s}" for s in scenes]).save(sheets / "overview-scenes.jpg", quality=88)
    costs = cost_table()
    summaries = {t: summary(scene_rows, t) for t in tiers}
    (args.out / "metrics.json").write_text(json.dumps(
        {"scenes": scenes, "tiers": tiers, "summary": summaries, "scene_rows": scene_rows,
         "fusion_rows": fusion_rows, "photo_rows": photo_rows, "costs": costs}, indent=1))

    md = ["# Depth benchmark results", "",
          f"Scenes: {', '.join(scenes)}. Means over scenes with 95 % bootstrap intervals in brackets.", ""]
    for tier in tiers:
        md += [f"## Quality, {tier} tier", "", table(summaries[tier], [
            ("Model", "model"), ("Depth error %, scale and offset", "absrel_pct"),
            ("Normals from depth, deg", "depth_normal_deg"), ("Model normals, deg", "normal_deg"),
            ("Relief error %", "relief_pct"), ("Edge F1", "edge_f1"), ("Focal error %", "focal_abs_pct"),
            ("Scenes", "scenes")]), ""]
    md += ["## Cost", "", "Times exclude model loading and one warm-up image.", "", table(costs, [
        ("Tier", "tier"), ("Model", "model"), ("Images", "images"), ("Median s", "median_s"), ("Max s", "max_s"),
        ("GPU GB", "gpu_peak_gb"), ("RAM GB", "rss_peak_gb"), ("Read GB", "read_gb_max"),
        ("Within 15 min", "within_15_min"), ("Median s, 2048 px", "median_s_2048"),
        ("GPU GB, 2048 px", "gpu_peak_gb_2048"), ("Failures", "failures"), ("First failure", "failure")]), ""]
    md += ["## Per scene", "", table(scene_rows, [
        ("Tier", "tier"), ("Scene", "scene"), ("Model", "model"), ("Depth error", "absrel"),
        ("Normals from depth", "depth_normal_deg"), ("Model normals", "normal_deg"),
        ("Relief RMSE mm", "relief_rmse_mm"), ("Relief mm", "relief_mm"), ("Edge F1", "edge_f1"),
        ("Scale error %", "scale_error_pct"), ("Focal error %", "focal_error_pct")]), ""]
    md += ["## Depth and normal fusion", "", "Sigma is the blur, in pixels, separating broad shape from detail; "
           "256 means normals only.", "", table(fusion_rows, [
               ("Tier", "tier"), ("Scene", "scene"), ("Depth", "depth"), ("Normals", "normals"),
               ("Sigma", "sigma"), ("Relief RMSE mm", "relief_rmse_mm"), ("Normal error deg", "normal_deg")]), ""]
    md += ["## Photographs", "", table(photo_rows, [
        ("Tier", "tier"), ("Image", "image"), ("Model", "model"), ("Focal error % vs EXIF", "focal_error_pct"),
        ("Invented depth %", "non_planar_pct"), ("Median distance m", "median_depth_m")]), ""]
    (args.out / "report.md").write_text("\n".join(md))
    print("\n".join(md[:40]))


if __name__ == "__main__":
    main()
