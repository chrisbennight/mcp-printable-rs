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
    args = parser.parse_args()
    root = Path(__file__).resolve().parent.parent
    for image in (args.server_image, args.blender_image):
        if not image or image.startswith("-"):
            parser.error("an image reference is required")
    for image, source, destination in (
        (args.server_image, root / "LICENSE", "/usr/share/doc/printable/LICENSE"),
        (args.blender_image, root / "LICENSE", "/usr/share/doc/printable/LICENSE"),
        (args.server_image, root / "crates/printable-imaging/assets/LICENSE-Fira-OFL.txt",
         "/usr/share/doc/printable/LICENSE-Fira-OFL.txt"),
    ):
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
