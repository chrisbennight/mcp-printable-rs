#!/usr/bin/env python3
"""Exercise the installation on isolated, software-rendered CI containers."""

import argparse
import json
import os
from pathlib import Path
import secrets
import shutil
import subprocess
import tempfile
import uuid

from printable_client import Client
from quickstart import run


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("server_image")
    parser.add_argument("blender_image")
    parser.add_argument("--evidence-dir", type=Path, help="new directory for the tutorial's verified artifacts")
    parser.add_argument("--recovery", action="store_true", help="also test backup restore, damaged metadata, and bounded storage exhaustion")
    args = parser.parse_args()
    if args.evidence_dir is not None:
        args.evidence_dir.mkdir(mode=0o700)
    root = Path(__file__).resolve().parent.parent
    environment = dict(os.environ, PRINTABLE_SERVER_IMAGE=args.server_image,
                       PRINTABLE_BLENDER_IMAGE=args.blender_image)
    rendered = subprocess.run(
        ["docker", "compose", "-f", str(root / "compose.yaml"), "config", "--format", "json"],
        env=environment, check=True, capture_output=True, text=True,
    )
    config = json.loads(rendered.stdout)
    project = "printable-install-test-" + uuid.uuid4().hex
    config["name"] = project
    for name, network in config["networks"].items():
        network["name"] = project + "_" + name
    for name, volume in config["volumes"].items():
        volume["name"] = project + "_" + name
    for name in ("blender", "render-worker"):
        service = config["services"][name]
        service.pop("deploy")
        service["environment"]["PRINTABLE_BLENDER_RENDER_DEVICE"] = "CPU"
    config["services"]["blender"]["environment"]["PRINTABLE_BLENDER_UI_BACKEND"] = "software"
    config["services"]["server"]["ports"][0]["published"] = "0"
    with tempfile.TemporaryDirectory(prefix=project) as directory:
        directory = Path(directory)
        credential = directory / "bearer"
        credential.write_text(secrets.token_hex(32) + "\n")
        credential.chmod(0o444)
        config["secrets"]["mcp_bearer"]["file"] = str(credential)
        compose_file = directory / "compose.json"
        compose_file.write_text(json.dumps(config))
        compose = ["docker", "compose", "-p", project, "-f", str(compose_file)]

        def endpoint():
            published = subprocess.run(compose + ["port", "server", "8000"],
                                       check=True, capture_output=True, text=True).stdout.strip()
            return "http://" + published + "/mcp"

        try:
            subprocess.run(compose + ["up", "-d", "--wait", "--wait-timeout", "600"], check=True)
            with Client(endpoint(), credential) as client:
                run(client, directory / "bracket")
            if args.evidence_dir is not None:
                for artifact in (directory / "bracket").iterdir():
                    shutil.copyfile(artifact, args.evidence_dir / artifact.name)
            subprocess.run(compose + ["restart", "server"], check=True)
            subprocess.run(compose + ["up", "-d", "--wait", "--wait-timeout", "600"], check=True)
            job = json.loads((directory / "bracket" / "job.json").read_text())
            with Client(endpoint(), credential) as client:
                recovered = client.call("job", {"action": "get", "params": {"job_id": job["job_id"]}})
                if recovered["state"] != "succeeded":
                    raise ValueError("Completed render was not recovered after restart")
            print("INSTALLATION_OK: direct modeling, four verified downloads, completed-job restart")
            if args.recovery:
                from installation_recovery import exercise
                exercise(config, compose_file, directory, args.server_image, args.blender_image, credential)
        except subprocess.CalledProcessError:
            subprocess.run(compose + ["logs", "--no-color", "--tail", "80", "blender", "render-worker"], check=True)
            raise
        finally:
            subprocess.run(compose + ["down", "--volumes", "--timeout", "30"], check=True)
            remaining = subprocess.run([
                "docker", "volume", "ls", "--filter", "label=com.docker.compose.project=" + project,
                "--format", "{{.Name}}",
            ], check=True, capture_output=True, text=True).stdout.splitlines()
            for volume in remaining:
                if volume not in {project + suffix for suffix in ("_workspace", "_restored", "_pressure")}:
                    raise ValueError("Unexpected volume in isolated fixture cleanup")
                subprocess.run(["docker", "volume", "rm", volume], check=True)


if __name__ == "__main__":
    main()
