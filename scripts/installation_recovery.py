"""Backup, damaged-metadata, and disk-pressure checks on owned test volumes."""

import base64
import hashlib
import json
import re
import subprocess

from printable_client import Client
from cad_client_smoke import verify_restored as verify_cad_restored


def exercise(config, compose_file, fixture_dir, server_image, blender_image, credential):
    project = config["name"]
    if not re.fullmatch(r"printable-install-test-[a-f0-9]{32}", project):
        raise ValueError("Recovery tests require an isolated installation fixture")
    compose = ["docker", "compose", "-p", project, "-f", str(compose_file)]
    original_volume = config["volumes"]["workspace"]["name"]
    if original_volume != project + "_workspace":
        raise ValueError("Unexpected source volume")
    job = json.loads((fixture_dir / "bracket/job.json").read_text())
    artifacts = json.loads((fixture_dir / "bracket/artifacts.json").read_text())
    if not re.fullmatch(r"[a-f0-9]{32}", job["job_id"]):
        raise ValueError("Unexpected fixture job identifier")

    def endpoint():
        port = subprocess.run(compose + ["port", "server", "8000"], check=True,
                              capture_output=True, text=True).stdout.strip()
        return "http://" + port + "/mcp"

    def container(image, volume, program, *arguments, readonly=False, **streams):
        mount = f"type=volume,source={volume},target=/workspace"
        if readonly:
            mount += ",readonly"
        subprocess.run([
            "docker", "run", "--rm", "-i", "--network", "none", "--read-only",
            "--cap-drop", "ALL", "--security-opt", "no-new-privileges", "--user", "10001:10001",
            "--mount", mount, "--entrypoint", program, image, *arguments,
        ], check=True, **streams)

    subprocess.run(compose + ["stop"], check=True)
    archive = fixture_dir / "workspace.tar"
    with archive.open("xb") as output:
        container(server_image, original_volume, "/bin/tar", "-C", "/workspace", "-cf", "-", ".",
                  readonly=True, stdout=output)
    subprocess.run(compose + ["down"], check=True)
    restored_volume = project + "_restored"
    config["volumes"]["workspace"]["name"] = restored_volume
    compose_file.write_text(json.dumps(config))
    subprocess.run(compose + ["run", "--rm", "--no-deps", "workspace-init"], check=True)
    with archive.open("rb") as source:
        container(server_image, restored_volume, "/bin/tar", "--no-same-owner", "-C", "/workspace",
                  "-xf", "-", stdin=source)
    subprocess.run(compose + ["up", "-d", "--wait", "--wait-timeout", "600"], check=True)
    restored_video = fixture_dir / "restored.mp4"
    with Client(endpoint(), credential) as client:
        recovered = client.call("job", {"action": "get", "params": {"job_id": job["job_id"]}})
        if recovered["state"] != "succeeded":
            raise ValueError("Restored job did not retain its completed state")
        client.download(artifacts["video"]["path"], restored_video)
        verify_cad_restored(client, fixture_dir / "cad")
    with restored_video.open("rb") as restored, (fixture_dir / "bracket/turntable.mp4").open("rb") as original:
        if hashlib.file_digest(restored, "sha256").digest() != hashlib.file_digest(original, "sha256").digest():
            raise ValueError("Restored video differs from the verified original")
    print("BACKUP_RESTORE_OK", flush=True)

    subprocess.run(compose + ["stop"], check=True)
    metadata = "/workspace/.printable/jobs/" + job["job_id"] + "/job.json"
    container(blender_image, restored_volume, "python3", "-c",
              "from pathlib import Path; import sys; Path(sys.argv[1]).write_text('{')", metadata)
    subprocess.run(compose + ["up", "-d", "--wait", "--wait-timeout", "600"], check=True)
    with Client(endpoint(), credential) as client:
        status = client.call("status", {"detail": True})
        if status["render_jobs"]["recovery_integrity"]["status"] != "blocked":
            raise ValueError("Damaged metadata was not reported as blocked rendering")
        client.call("edit", {"action": "primitive", "params": {
            "primitive": "cube", "name": "recovery-live-check", "size": 1}})
    print("DAMAGED_METADATA_ISOLATION_OK", flush=True)

    subprocess.run(compose + ["down"], check=True)
    pressure_volume = project + "_pressure"
    config["volumes"]["workspace"] = {
        "name": pressure_volume, "driver": "local",
        "driver_opts": {"type": "tmpfs", "device": "tmpfs", "o": "size=64k,uid=10001,gid=10001"},
    }
    compose_file.write_text(json.dumps(config))
    subprocess.run(compose + ["up", "-d", "--wait", "--wait-timeout", "600"], check=True)
    fill = """import errno
try:
    with open('/workspace/pressure-fill', 'wb') as stream:
        stream.write(b'x' * 1048576)
except OSError as error:
    if error.errno != errno.ENOSPC:
        raise
else:
    raise RuntimeError('fixture storage was not bounded')
"""
    container(blender_image, pressure_volume, "python3", "-c", fill)
    arguments = {"action": "write", "params": {
        "path": "pressure.stl", "data_base64": base64.b64encode(b"fixture").decode()}}
    with Client(endpoint(), credential) as client:
        failed = client.rpc("tools/call", {"name": "artifact", "arguments": arguments})
        if not failed.get("isError") or failed["structuredContent"]["error"]["code"] != "io":
            raise ValueError("Full storage did not produce the expected explicit I/O failure")
        container(server_image, pressure_volume, "/usr/bin/test", "!", "-e", "/workspace/pressure.stl")
        container(blender_image, pressure_volume, "python3", "-c",
                  "from pathlib import Path; Path('/workspace/pressure-fill').unlink()")
        client.call("artifact", arguments)
        client.download("pressure.stl", fixture_dir / "pressure-recovered.stl")
    print("DISK_PRESSURE_RECOVERY_OK", flush=True)
