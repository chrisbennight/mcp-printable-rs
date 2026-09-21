#!/usr/bin/env python3
"""Verify first-party and font notices survive container packaging."""

import argparse
import hashlib
from pathlib import Path
import subprocess


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("server_image")
    parser.add_argument("blender_image")
    parser.add_argument("--cad-image")
    parser.add_argument("--slicer-image")
    args = parser.parse_args()
    root = Path(__file__).resolve().parent.parent
    images = [args.server_image, args.blender_image]
    if args.cad_image is not None:
        images.append(args.cad_image)
    if args.slicer_image is not None:
        images.append(args.slicer_image)
    for image in images:
        if not image or image.startswith("-"):
            parser.error("an image reference is required")
    notices = [(image, root / "LICENSE", "/usr/share/doc/printable/LICENSE") for image in images]
    notices.append((args.server_image, root / "crates/printable-imaging/assets/LICENSE-Fira-OFL.txt",
                    "/usr/share/doc/printable/LICENSE-Fira-OFL.txt"))
    for image, source, destination in notices:
        expected = hashlib.sha256(source.read_bytes()).hexdigest()
        result = subprocess.run([
            "docker", "run", "--rm", "--network", "none", "--read-only", "--cap-drop", "ALL",
            "--security-opt", "no-new-privileges", "--pids-limit", "32",
            "--entrypoint", "/usr/bin/sha256sum", image, destination,
        ], check=True, capture_output=True, text=True)
        if result.stdout.split()[0] != expected:
            raise SystemExit("Packaged notice differs from its source")
    print("IMAGE_NOTICES_OK")


if __name__ == "__main__":
    main()
