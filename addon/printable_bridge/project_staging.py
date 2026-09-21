"""Consistent, project-scoped inputs for an isolated native export process."""

from contextlib import contextmanager
from dataclasses import dataclass
import hashlib
from pathlib import Path
import tempfile

from .workspace import WorkspaceError


MAX_FILES = 256
MAX_SOURCE_BYTES = 1024 * 1024 * 1024


@dataclass(frozen=True)
class StagedProject:
    root: Path
    files: tuple[dict, ...]


def validate_project_file(name):
    if not isinstance(name, str) or len(name.encode("utf-8")) > 1024 or "\\" in name or any(
        ord(character) < 32 or ord(character) == 127 for character in name
    ) or any(part in {"", ".", ".."} or part.startswith(".") or part.lower() in {
        "secrets", "credentials", "secrets.json", "credentials.json"
    } for part in name.split("/")) or not Path(name).suffix:
        raise WorkspaceError("export input must be a non-hidden project-relative artifact path")


@contextmanager
def stage_project_inputs(workspace, project_id: str, files: list[str], *, check_budget=lambda: None):
    """Commit outputs only after this context exits with source checks intact.

    Preparation may change its private copies, never the original project files.
    Metadata and hashes describe the retained inputs before native preparation.
    """
    if not isinstance(project_id, str) or not 1 <= len(project_id) <= 64 or any(
        character not in "abcdefghijklmnopqrstuvwxyz0123456789_-" for character in project_id
    ):
        raise WorkspaceError("invalid export project_id")
    if not isinstance(files, list) or not 1 <= len(files) <= MAX_FILES:
        raise WorkspaceError("select 1–256 project inputs for preparation")
    requests = {}
    for name in files:
        validate_project_file(name)
        if name in requests:
            raise WorkspaceError("export inputs must be unique files with an extension")
        requests[name] = workspace.validate(f"projects/{project_id}/{name}", Path(name).suffix)

    identities = {}
    remaining = MAX_SOURCE_BYTES
    with tempfile.TemporaryDirectory(prefix="printable-project-export-") as directory:
        root = Path(directory)
        metadata = []
        for name, request in sorted(requests.items()):
            check_budget()
            identity = workspace.input_identity(request)
            if identity[2] > remaining:
                raise WorkspaceError("project export inputs exceed the 1 GiB budget")
            with workspace.stage_input(request, check_budget=check_budget) as source:
                if workspace.input_identity(request) != identity:
                    raise WorkspaceError("project export source changed while staging")
                destination = root / name
                destination.parent.mkdir(parents=True, exist_ok=True)
                digest = hashlib.sha256()
                with source.open("rb") as content, destination.open("wb") as copied:
                    while chunk := content.read(65536):
                        check_budget()
                        digest.update(chunk)
                        copied.write(chunk)
            identities[name] = identity
            remaining -= identity[2]
            metadata.append({"path": name, "size_bytes": identity[2], "sha256": digest.hexdigest()})

        def verify_sources():
            for name, request in requests.items():
                check_budget()
                if workspace.input_identity(request) != identities[name]:
                    raise WorkspaceError("project export source changed during preparation")

        verify_sources()
        yield StagedProject(root=root, files=tuple(metadata))
        verify_sources()
