"""Compare two benchmark runs at different input sizes, scene by scene.

Usage: python3 compare_sides.py <first metrics.json> <second metrics.json> [--tier gpu]

For every model, prints the mean of each scene score in both runs and the
mean paired change (second minus first) with a 95 % bootstrap interval over
scenes. Then does the same for every depth-only and fusion row. Needs NumPy.
"""

from __future__ import annotations

import argparse
import json
from collections import defaultdict
from pathlib import Path

import scoring

SCENE_METRICS = ("normal_deg", "depth_normal_deg", "relief_pct", "edge_f1")
FUSION_METRICS = ("normal_deg", "relief_rmse_mm")


def rows(metrics: dict, name: str) -> list[dict]:
    """Rows of a report table, whether stored as dictionaries or as columns and rows."""
    table = metrics[name]
    if isinstance(table, dict):
        return [dict(zip(table["columns"], r)) for r in table["rows"]]
    return table


def paired(first: dict, second: dict, metric: str) -> tuple[float, float, scoring.Interval] | None:
    keys = [k for k in first if k in second and first[k].get(metric) is not None and second[k].get(metric) is not None]
    if not keys:
        return None
    a = [first[k][metric] for k in keys]
    b = [second[k][metric] for k in keys]
    change = scoring.bootstrap_mean([y - x for x, y in zip(a, b)])
    return sum(a) / len(a), sum(b) / len(b), change


def report(first: list[dict], second: list[dict], group: tuple[str, ...], metrics: tuple[str, ...], tier: str):
    def index(table):
        out = defaultdict(dict)
        for r in table:
            if r["tier"] == tier:
                out[tuple(r[g] for g in group)][r["scene"]] = r
        return out

    a, b = index(first), index(second)
    for metric in metrics:
        print(f"\n{metric}: first -> second, change (95 % interval)")
        for key in sorted(set(a) & set(b), key=str):
            result = paired(a[key], b[key], metric)
            if result is None:
                continue
            m1, m2, c = result
            print(f"  {' / '.join(str(k) for k in key):48s} {m1:8.2f} -> {m2:8.2f}  "
                  f"{c.mean:+.2f} ({c.low:+.2f} to {c.high:+.2f})")


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("first", type=Path)
    parser.add_argument("second", type=Path)
    parser.add_argument("--tier", default="gpu")
    args = parser.parse_args()
    first, second = json.loads(args.first.read_text()), json.loads(args.second.read_text())
    print("Scene scores")
    report(rows(first, "scene_rows"), rows(second, "scene_rows"), ("model",), SCENE_METRICS, args.tier)
    print("\nDepth-only and fusion scores on relief regions")
    report(rows(first, "fusion_rows"), rows(second, "fusion_rows"), ("depth", "normals", "sigma"), FUSION_METRICS,
           args.tier)


if __name__ == "__main__":
    main()
