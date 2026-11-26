"""MoGe and Depth Pro 2 runners.

Usage: python run_main.py <model> --tier gpu|budget|cpu [--timing] [--images a,b]
"""

import argparse

import numpy as np
import torch

import checkpoints
import common


def moge(version: int, repo: str, device):
    if version == 3:
        from moge.model.v3 import MoGeModel
    else:
        from moge.model.v2 import MoGeModel
    model = MoGeModel.from_pretrained(checkpoints.hugging_face_file(repo, "model.pt")).to(device).eval()

    @torch.inference_mode()
    def run(rgb):
        t = torch.from_numpy(rgb).to(device).float().div(255).permute(2, 0, 1)
        # The GPU keeps each model's default precision; half precision is not supported on the CPU.
        out = model.infer(t, use_fp16=False) if device.type == "cpu" else model.infer(t)
        w = rgb.shape[1]
        depth = out["depth"].float().cpu().numpy()
        mask = out["mask"].cpu().numpy().astype(bool) & np.isfinite(depth)
        depth = np.where(mask, depth, np.nanmax(np.where(mask, depth, np.nan)) if mask.any() else 1.0)
        normals = None
        if out.get("normal") is not None:
            n = out["normal"].float().cpu().numpy()
            normals = np.stack([n[..., 0], -n[..., 1], -n[..., 2]], -1)  # OpenCV frame to x right, y up, z towards camera
        focal = float(out["intrinsics"][0, 0].item()) * w
        return depth, "metric", normals, focal, {"mask": mask, "masked_fraction": round(float(1 - mask.mean()), 4)}

    return run


def depth_pro_2(preset: str, checkpoint: str, device):
    from depth_pro_2 import create_model_and_transforms

    checkpoints.depth_pro_2(checkpoint, "depthpro2-fov-ew55zg4kq7.pt")
    model, transform = create_model_and_transforms(preset, device=device)

    @torch.inference_mode()
    def run(rgb):
        out = model.infer(transform(rgb), f_px=None)
        depth = out["depth"].float().cpu().numpy()
        finite = np.isfinite(depth)
        depth = np.where(finite, depth, np.nanmax(np.where(finite, depth, np.nan)))
        return depth, "metric", None, float(out["focallength_px"].item()), {"mask": finite}

    return run


MODELS = {
    "moge3-vitg": lambda d: moge(3, "Ruicheng/moge-3-vitg", d),
    "moge3-vitl": lambda d: moge(3, "Ruicheng/moge-3-vitl", d),
    "moge2-vitl-normal": lambda d: moge(2, "Ruicheng/moge-2-vitl-normal", d),
    "moge2-vitb-normal": lambda d: moge(2, "Ruicheng/moge-2-vitb-normal", d),
    "moge2-vits-normal": lambda d: moge(2, "Ruicheng/moge-2-vits-normal", d),
    "dp2-h": lambda d: depth_pro_2("depthpro2-h", "depthpro2-h-eivtm5w7m5.pt", d),
    "dp2-l": lambda d: depth_pro_2("depthpro2-l", "depthpro2-l-tegstu7fim.pt", d),
    "dp2-b": lambda d: depth_pro_2("depthpro2-b", "depthpro2-b-xb7m9egktw.pt", d),
    "dp2-s": lambda d: depth_pro_2("depthpro2-s", "depthpro2-s-bgcc6f8bg3.pt", d),
}


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("model", choices=sorted(MODELS))
    parser.add_argument("--tier", choices=common.TIERS, required=True)
    parser.add_argument("--timing", action="store_true")
    parser.add_argument("--images", default="")
    args = parser.parse_args()
    device = common.device_for(args.tier)
    try:
        run = MODELS[args.model](device)
    except Exception as error:  # a model that cannot load on this tier is a recorded result
        common.log_failure(args.tier, args.model, "load", error, args.timing)
        return
    selection = [s for s in args.images.split(",") if s] or None
    common.run_images(args.tier, args.model, run, device, selection, args.timing)


if __name__ == "__main__":
    main()
