"""Native project bundles retain source files and one prepared editable entrypoint."""

from contextlib import ExitStack
import hashlib
import json
import math
from pathlib import Path
import time
import zipfile

from .project_packing import ProjectPackingError
from .project_process import ProjectPreparationCancelled, prepare_in_child
from .project_staging import stage_project_inputs, validate_project_file
from .workspace import MAX_ARTIFACT_BYTES, WorkspaceError


def export_blender_bundle(workspace, workspace_root: Path, binary: str, *,
                          project_id: str, files: list[str], entrypoint: str,
                          output_path: str, timeout_seconds: float, cancelled=lambda: False):
    if (isinstance(timeout_seconds, bool) or not isinstance(timeout_seconds, (int, float))
            or not math.isfinite(timeout_seconds) or not 1 <= timeout_seconds <= 120):
        raise ProjectPackingError("native export timeout must be between 1 and 120 seconds")
    validate_project_file(output_path)
    validate_project_file(entrypoint)
    if not output_path.endswith(".zip") or not entrypoint.endswith(".blend"):
        raise WorkspaceError("native export requires a blend entrypoint and a ZIP output")
    if not isinstance(files, list) or entrypoint not in files or output_path in files:
        raise WorkspaceError("select the native entrypoint and exclude the output artifact")
    deadline = time.monotonic() + timeout_seconds

    def check_budget():
        if cancelled():
            raise ProjectPreparationCancelled("native export was cancelled")
        remaining = deadline - time.monotonic()
        if remaining <= 0:
            raise ProjectPackingError("native export exceeded its deadline")
        return remaining

    # Source checks run when staging exits, before the single archive commit.
    with ExitStack() as publication:
        with stage_project_inputs(workspace, project_id, files) as staged:
            check_budget()
            request = workspace.validate(f"projects/{project_id}/{output_path}", ".zip")
            output = publication.enter_context(workspace.stage_output(request))
            if output.destination_exists:
                raise WorkspaceError("output artifact already exists")
            manifest = {"format_version": 1, "project_id": project_id,
                        "scope": "native_blender", "entrypoint": f"prepared/{entrypoint}",
                        "files": [], "preparation": None}
            remaining_bytes = MAX_ARTIFACT_BYTES - 1024 * 1024
            with zipfile.ZipFile(output.path, "w", compression=zipfile.ZIP_STORED) as archive:
                def add_file(source, archive_path):
                    nonlocal remaining_bytes
                    size = source.stat().st_size
                    if size > remaining_bytes:
                        raise WorkspaceError("native bundle exceeds the 1 GiB artifact limit")
                    remaining_bytes -= size
                    digest = hashlib.sha256()
                    written = 0
                    with source.open("rb") as incoming, archive.open(archive_path, "w", force_zip64=True) as outgoing:
                        while chunk := incoming.read(65536):
                            check_budget()
                            written += len(chunk)
                            if written > size:
                                raise WorkspaceError("native bundle input changed while archiving")
                            digest.update(chunk)
                            outgoing.write(chunk)
                    if written != size:
                        raise WorkspaceError("native bundle input changed while archiving")
                    manifest["files"].append({"path": archive_path, "size_bytes": size,
                                              "sha256": digest.hexdigest()})

                # Retain exact original snapshots, not the copies modified by packing.
                for record in staged.files:
                    add_file(staged.root / record["path"], f"sources/{record['path']}")
                    if manifest["files"][-1]["sha256"] != record["sha256"]:
                        raise WorkspaceError("native bundle input changed before preparation")
                remaining = check_budget()
                if remaining < 0.1:
                    raise ProjectPackingError("native export exceeded its deadline")
                manifest["preparation"] = prepare_in_child(
                    binary, workspace_root / "projects" / project_id, staged.root,
                    files, entrypoint, remaining, cancelled,
                )
                add_file(staged.root / entrypoint, manifest["entrypoint"])
                archive.writestr("manifest.json", json.dumps(manifest, allow_nan=False))
            size = output.path.stat().st_size
            if size > MAX_ARTIFACT_BYTES:
                raise WorkspaceError("native bundle exceeds the 1 GiB artifact limit")
            with output.path.open("rb") as prepared:
                digest = hashlib.file_digest(prepared, "sha256").hexdigest()
        check_budget()
        output.commit_new()
    return {"project_id": project_id, "path": request.relative,
            "size_bytes": size, "sha256": digest, "manifest": manifest}
