#!/usr/bin/env python3
"""Build a small bracket in a fresh installation and download its artifacts."""

import argparse
import json
from pathlib import Path
import time
import uuid

from printable_client import Client


BRACKET = """difference() {
  union() { cube([40, 30, 4]); translate([0, 0, 4]) cube([4, 30, 20]); }
  for (y = [8, 22]) translate([25, y, -1]) cylinder(h=6, d=4, $fn=32);
}"""


def run(client, output):
    output.mkdir(mode=0o700)
    prefix = "tutorial-" + uuid.uuid4().hex

    def call(tool, action, params):
        return client.call(tool, {"action": action, "params": params})

    # Preserve the live state before replacing it for this explicit tutorial.
    saved = prefix + "-before.blend"
    call("scene", "checkpoint", {"path": saved})
    call("scene", "clear", {})
    mesh = prefix + ".stl"
    call("scad_build", "mesh", {"source": BRACKET, "path": mesh})
    validation = client.call("validate_mesh", {"path": mesh})
    (output / "validation.json").write_text(json.dumps(validation, indent=2))
    call("scene", "import", {"path": mesh})
    inspection = call("inspect", "scene", {})
    (output / "scene.json").write_text(json.dumps(inspection, indent=2))
    png = prefix + ".png"
    call("view", "dimensions", {"path": png, "width": 256, "height": 256, "include_inline": False})
    blend = prefix + ".blend"
    call("scene", "checkpoint", {"path": blend})
    job = call("job", "submit", {
        "source_blend": blend, "kind": "turntable", "turntable_frames": 8,
        "width": 128, "height": 128, "frames_per_second": 8,
        "frame_timeout_seconds": 180, "encode_timeout_seconds": 120,
        "max_frame_sequence_bytes": 16 * 1024 * 1024,
        "max_video_bytes": 16 * 1024 * 1024,
    })
    job_id = job["job_id"]
    (output / "job.json").write_text(json.dumps(job, indent=2))
    print("Submitted job " + job_id + "; saved previous scene as " + saved, flush=True)
    for artifact, destination in [(mesh, "bracket.stl"), (png, "dimensions.png"), (blend, "bracket.blend")]:
        client.download(artifact, output / destination)
    deadline = time.monotonic() + 900
    while time.monotonic() < deadline:
        status = call("job", "get", {"job_id": job_id, "detail": True})
        (output / "job.json").write_text(json.dumps(status, indent=2))
        if status["state"] == "succeeded":
            break
        if status["state"] in ("failed", "cancelled"):
            raise ValueError("Tutorial render failed; inspect job.json")
        time.sleep(2)
    else:
        raise ValueError("Tutorial wait expired; the job remains available by its recorded ID")
    artifacts = call("job", "artifacts", {"job_id": job_id})
    (output / "artifacts.json").write_text(json.dumps(artifacts, indent=2))
    client.download(artifacts["video"]["path"], output / "turntable.mp4")
    return artifacts


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--url", default="http://127.0.0.1:8000/mcp")
    parser.add_argument("--bearer-file", type=Path, default=Path(".dev/mcp-bearer"))
    parser.add_argument("output", type=Path, help="new output directory; this tutorial replaces the live scene after saving a checkpoint")
    args = parser.parse_args()
    with Client(args.url, args.bearer_file) as client:
        print(json.dumps(run(client, args.output), indent=2))
