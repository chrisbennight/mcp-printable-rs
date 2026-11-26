"""Marigold V2 through Printable's own photograph-worker code (the current baseline).

Usage: python run_marigold.py <converted-model-dir> --tier gpu|budget|cpu [--timing] [--images a,b]

Calls the same prepare and predict functions as recon/estimate_depth.py with
seed 0: depth and normals, each a separate streamed pass. On CPU the decoded
blocks use float32, because bfloat16 matrix arithmetic is slow or unsupported
on many processors.
"""

import argparse
import sys
from pathlib import Path

import numpy as np
import torch
from PIL import Image

sys.path.insert(0, "/src/printable-recon")
from printable_recon import marigold  # noqa: E402

import common  # noqa: E402

MODEL = "marigold-v2"


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("directory", type=Path)
    parser.add_argument("--tier", choices=common.TIERS, required=True)
    parser.add_argument("--timing", action="store_true")
    parser.add_argument("--images", default="")
    args = parser.parse_args()
    device = common.device_for(args.tier)
    dtype = torch.float32 if device.type == "cpu" else torch.bfloat16
    side = 2048 if args.timing else common.input_side()

    def run(rgb):
        pixels = marigold.prepare(Image.fromarray(rgb), side)
        depth = marigold.predict(pixels, args.directory, "depth", device, dtype=dtype, seed=0).mean(dim=0).numpy()
        normals = marigold.predict(pixels, args.directory, "normals", device, dtype=dtype, seed=0).numpy()
        return depth, "logrel", np.transpose(normals, (1, 2, 0)), None, {"process_hw": list(pixels.shape[-2:])}

    selection = [s for s in args.images.split(",") if s] or None
    common.run_images(args.tier, MODEL, run, device, selection, args.timing)


if __name__ == "__main__":
    main()
