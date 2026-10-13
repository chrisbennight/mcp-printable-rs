#!/usr/bin/env python3
"""Verify required notices survive container packaging without changing their bytes."""

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
    parser.add_argument("--recon-image")
    args = parser.parse_args()
    root = Path(__file__).resolve().parent.parent
    images = [args.server_image, args.blender_image]
    if args.cad_image is not None:
        images.append(args.cad_image)
    if args.slicer_image is not None:
        images.append(args.slicer_image)
    if args.recon_image is not None:
        images.append(args.recon_image)
    for image in images:
        if not image or image.startswith("-"):
            parser.error("an image reference is required")
    notices = [(image, root / "LICENSE", "/usr/share/doc/printable/LICENSE") for image in images]
    notices.append((args.server_image, root / "crates/printable-imaging/assets/LICENSE-Fira-OFL.txt",
                    "/usr/share/doc/printable/LICENSE-Fira-OFL.txt"))
    if args.recon_image is not None:
        notices.append((args.recon_image, root / "recon/NOTICE",
                        "/usr/share/doc/printable/NOTICE-recon"))
    for image, source, destination in notices:
        expected = hashlib.sha256(source.read_bytes()).hexdigest()
        result = subprocess.run([
            "docker", "run", "--rm", "--network", "none", "--read-only", "--cap-drop", "ALL",
            "--security-opt", "no-new-privileges", "--pids-limit", "32",
            "--entrypoint", "/usr/bin/sha256sum", image, destination,
        ], check=True, capture_output=True, text=True)
        if result.stdout.split()[0] != expected:
            raise SystemExit("Packaged notice differs from its source")
    for source, destinations in (
        ("/usr/share/doc/virtualgl-3.1.5/LICENSE.txt",
         ("/usr/share/doc/virtualgl/copyright", "/opt/VirtualGL/licenses/LICENSE.txt")),
        ("/usr/share/doc/virtualgl-3.1.5/LICENSE-FLTK.txt",
         ("/opt/VirtualGL/licenses/LICENSE-FLTK.txt",)),
        ("/usr/share/doc/virtualgl-3.1.5/LGPL.txt",
         ("/opt/VirtualGL/licenses/COPYING-LGPL.txt",)),
    ):
        result = subprocess.run([
            "docker", "run", "--rm", "--network", "none", "--read-only", "--cap-drop", "ALL",
            "--security-opt", "no-new-privileges", "--pids-limit", "32",
            "--entrypoint", "/usr/bin/sha256sum", args.blender_image, source, *destinations,
        ], check=True, capture_output=True, text=True)
        hashes = [line.split()[0] for line in result.stdout.splitlines()]
        if len(hashes) != 1 + len(destinations) or any(value != hashes[0] for value in hashes[1:]):
            raise SystemExit("Packaged VirtualGL notice differs from its installed vendor source")
    print("IMAGE_NOTICES_OK")


if __name__ == "__main__":
    main()
