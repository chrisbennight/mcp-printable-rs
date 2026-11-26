"""Depth Anything 3 runners.

Usage: python run_da3.py <model> --tier gpu|budget|cpu [--timing] [--images a,b]
"""

import argparse

import numpy as np
import torch

import checkpoints
import common

MODELS = {
    # name: (repository, depth kind, uses predicted intrinsics)
    "da3-nested-giant": ("depth-anything/DA3NESTED-GIANT-LARGE-1.1", "metric", True),
    "da3-mono-large": ("depth-anything/DA3MONO-LARGE", "relative", False),
    "da3-base": ("depth-anything/DA3-BASE", "relative", True),
    "da3-small": ("depth-anything/DA3-SMALL", "relative", True),
}
PROCESS_SIDE = 1008  # the default is 504; use a size close to the other models' inputs
TIMING_SIDE = 2044


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("model", choices=sorted(MODELS))
    parser.add_argument("--tier", choices=common.TIERS, required=True)
    parser.add_argument("--timing", action="store_true")
    parser.add_argument("--images", default="")
    args = parser.parse_args()
    device = common.device_for(args.tier)
    repo, kind, has_intrinsics = MODELS[args.model]
    try:
        from depth_anything_3.api import DepthAnything3

        model = DepthAnything3.from_pretrained(str(checkpoints.hugging_face(repo))).to(device).eval()
    except Exception as error:  # a model that cannot load on this tier is a recorded result
        common.log_failure(args.tier, args.model, "load", error, args.timing)
        return
    side = TIMING_SIDE if args.timing else PROCESS_SIDE

    @torch.inference_mode()
    def run(rgb):
        pred = model.inference([rgb], process_res=side, process_res_method="upper_bound_resize")
        depth = np.asarray(pred.depth[0], np.float32)
        focal = None
        if has_intrinsics and pred.intrinsics is not None:
            focal = float(pred.intrinsics[0][0, 0]) * rgb.shape[1] / depth.shape[1]
        return depth, kind, None, focal, {"process_side": int(max(depth.shape))}

    selection = [s for s in args.images.split(",") if s] or None
    common.run_images(args.tier, args.model, run, device, selection, args.timing)


if __name__ == "__main__":
    main()
