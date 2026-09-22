#!/usr/bin/env python3
"""Resolve an NVIDIA host process by container cgroup, including rootless Docker."""

from pathlib import Path
import re
import subprocess
import sys


def belongs_to_container(cgroups, container_id):
    # Docker's systemd and cgroupfs drivers embed the complete container ID.
    return any(part in (container_id, "docker-" + container_id + ".scope")
               for line in cgroups.splitlines()
               for part in line.split(":", 2)[-1].split("/"))


def main():
    container_id, gpu = sys.argv[1:]
    if not re.fullmatch(r"[0-9a-f]{64}", container_id) or not re.fullmatch(r"[0-9]+", gpu):
        raise ValueError("expected a full container ID and GPU index")
    pids = subprocess.check_output(["nvidia-smi", "--id=" + gpu,
                                   "--query-compute-apps=pid", "--format=csv,noheader,nounits"], text=True)
    matches = []
    for value in pids.splitlines():
        pid = value.strip()
        if not pid.isdecimal():
            continue
        try:
            cgroups = (Path("/proc") / pid / "cgroup").read_text()
            process = (Path("/proc") / pid / "comm").read_text().strip()
        except FileNotFoundError:
            # A process may exit between the NVIDIA query and the procfs read.
            continue
        if process == "blender" and belongs_to_container(cgroups, container_id):
            matches.append(pid)
    if len(matches) != 1:
        raise ValueError("expected exactly one NVIDIA Blender process in the test container")
    print(matches[0])


if __name__ == "__main__":
    main()
