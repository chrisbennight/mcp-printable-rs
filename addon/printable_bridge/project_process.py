"""Bounded Blender child for native preparation, separate from the live scene."""

import json
import math
import os
from pathlib import Path
import signal
import subprocess
import tempfile
import time

from .project_packing import ProjectPackingError


class ProjectPreparationCancelled(ProjectPackingError):
    pass


def prepare_in_child(binary: str, original_root: Path, staged_root: Path,
                     files: list[str], entrypoint: str, timeout_seconds: float,
                     cancelled=lambda: False):
    if (isinstance(timeout_seconds, bool) or not isinstance(timeout_seconds, (int, float))
            or not math.isfinite(timeout_seconds) or not 0.1 <= timeout_seconds <= 120):
        raise ProjectPackingError("native preparation timeout must be between 0.1 and 120 seconds")
    if not Path(binary).is_absolute():
        raise ProjectPackingError("native preparation requires the installed Blender binary")
    if cancelled():
        raise ProjectPreparationCancelled("native preparation was cancelled before launch")
    with tempfile.TemporaryDirectory(prefix="printable-native-export-") as temporary:
        control = Path(temporary)
        request = control / "request.json"
        response = control / "result.json"
        payload = json.dumps({"original_root": str(original_root), "staged_root": str(staged_root),
                              "files": files, "entrypoint": entrypoint}, allow_nan=False)
        if len(payload.encode("utf-8")) > 64 * 1024:
            raise ProjectPackingError("native preparation request exceeds its metadata limit")
        request.write_text(payload, encoding="utf-8")
        worker = Path(__file__).with_name("project_prepare_worker.py")
        command = [binary, "--background", "--factory-startup", "--disable-autoexec",
                   "--threads", "2", "--python-exit-code", "1", "--python", str(worker),
                   "--", str(request), str(response)]
        # Child credentials and private service configuration are not inherited.
        environment = {"PATH": os.defpath, "TMPDIR": str(control), "LANG": "C.UTF-8"}
        deadline = time.monotonic() + timeout_seconds
        with subprocess.Popen(command, env=environment, cwd=control,
                              stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL,
                              stderr=subprocess.DEVNULL, start_new_session=True) as child:
            try:
                while child.poll() is None:
                    if cancelled():
                        raise ProjectPreparationCancelled("native preparation was cancelled")
                    remaining = deadline - time.monotonic()
                    if remaining <= 0:
                        raise ProjectPackingError("native preparation exceeded its deadline")
                    try:
                        child.wait(timeout=min(0.1, remaining))
                    except subprocess.TimeoutExpired:
                        continue
                if child.returncode != 0:
                    raise ProjectPackingError("native preparation failed; no output was committed")
            finally:
                if child.poll() is None:
                    try:
                        os.killpg(child.pid, signal.SIGKILL)
                    except ProcessLookupError:
                        pass
                    child.wait()
        if not response.is_file() or response.stat().st_size > 64 * 1024:
            raise ProjectPackingError("native preparation returned no bounded metadata")
        result = json.loads(response.read_text(encoding="utf-8"))
        if not isinstance(result, dict):
            raise ProjectPackingError("native preparation returned invalid metadata")
        if "error" in result:
            raise ProjectPackingError(result["error"])
        if result.get("entrypoint") != entrypoint or result.get("registered_external_files") != 0:
            raise ProjectPackingError("native preparation did not confirm the selected entrypoint")
        return result
