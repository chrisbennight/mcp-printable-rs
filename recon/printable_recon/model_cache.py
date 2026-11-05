"""Prepare pinned model data in persistent storage, without loading its code.

Downloads are resumable. Only a complete, hash-verified snapshot is published;
the cross-process lock also covers the final readiness check and publication.
"""

from __future__ import annotations

from contextlib import contextmanager
from dataclasses import asdict, dataclass
import fcntl
import hashlib
import json
import os
from pathlib import Path, PurePosixPath
import re
import tempfile
from typing import Callable, Iterator
from urllib.parse import urlsplit


class PreparationError(Exception):
    """A model cannot be prepared or its files fail integrity validation."""


def _relative(value: str) -> str:
    if (
        not isinstance(value, str)
        or not value
        or "\\" in value
        or any(part in ("", ".", "..") for part in value.split("/"))
        or PurePosixPath(value).is_absolute()
    ):
        raise ValueError("model file paths must stay within their snapshot")
    return value


@dataclass(frozen=True)
class ModelFile:
    repo: str
    revision: str
    directory: str
    filename: str
    sha256: str
    bytes: int

    def __post_init__(self) -> None:
        if not re.fullmatch(r"[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+", self.repo):
            raise ValueError("model repository must have an owner and name")
        if not re.fullmatch(r"[0-9a-f]{40}", self.revision):
            raise ValueError("model revision must be a full commit identifier")
        if not re.fullmatch(r"[0-9a-f]{64}", self.sha256):
            raise ValueError("model files need a SHA-256 digest")
        if isinstance(self.bytes, bool) or not isinstance(self.bytes, int) or self.bytes < 1:
            raise ValueError("model file size must be a positive integer")
        _relative(self.directory)
        _relative(self.filename)

    @property
    def relative_path(self) -> str:
        return f"{self.directory}/{self.filename}"


@dataclass(frozen=True)
class Manifest:
    files: tuple[ModelFile, ...]

    def __post_init__(self) -> None:
        if not self.files:
            raise ValueError("a model manifest must list its files")
        paths = [item.relative_path for item in self.files]
        if len(paths) != len(set(paths)):
            raise ValueError("model files must have distinct destinations")
        directories: dict[str, tuple[str, str]] = {}
        for item in self.files:
            identity = (item.repo, item.revision)
            if directories.setdefault(item.directory, identity) != identity:
                raise ValueError("each model directory must have one repository and revision")

    def document(self) -> dict:
        return {"format": "printable-models 1", "files": [asdict(item) for item in self.files]}

    @property
    def identifier(self) -> str:
        encoded = json.dumps(self.document(), sort_keys=True, separators=(",", ":")).encode()
        return hashlib.sha256(encoded).hexdigest()

    @classmethod
    def read(cls, path: Path) -> Manifest:
        data = json.loads(path.read_text(encoding="utf-8"))
        if not isinstance(data, dict) or data.get("format") != "printable-models 1":
            raise ValueError("unsupported model manifest format")
        return cls(tuple(ModelFile(**item) for item in data["files"]))


def _inside(root: Path, relative: str) -> Path:
    path = root / _relative(relative)
    if not path.resolve().is_relative_to(root.resolve()):
        raise PreparationError("a model cache path escapes its storage directory")
    current = root
    for part in PurePosixPath(relative).parts:
        current = current / part
        if current.is_symlink():
            raise PreparationError("managed model cache paths must not contain symbolic links")
    return path


def _verify(path: Path, item: ModelFile) -> bool:
    if not path.is_file() or path.stat().st_size != item.bytes:
        return False
    with path.open("rb") as source:
        return hashlib.file_digest(source, "sha256").hexdigest() == item.sha256


def verify(snapshot: Path, manifest: Manifest) -> None:
    """Check every model byte, including for an offline, previously prepared model."""
    for item in manifest.files:
        if not _verify(_inside(snapshot, item.relative_path), item):
            raise PreparationError(f"model file {item.relative_path} is missing or corrupt")


def endpoint() -> str:
    """The operator's Hub endpoint; credentials must not be embedded in its URL."""
    value = os.environ.get("HF_ENDPOINT", "https://huggingface.co").rstrip("/")
    parsed = urlsplit(value)
    if (
        parsed.scheme not in ("http", "https")
        or not parsed.hostname
        or parsed.username is not None
        or parsed.password is not None
        or parsed.query
        or parsed.fragment
    ):
        raise PreparationError("HF_ENDPOINT must be an HTTP(S) base URL without credentials or a query")
    return value


@contextmanager
def _locked(root: Path) -> Iterator[None]:
    root.mkdir(parents=True, exist_ok=True)
    with _inside(root, "prepare.lock").open("a+b") as lock:
        fcntl.flock(lock, fcntl.LOCK_EX)
        try:
            yield
        finally:
            fcntl.flock(lock, fcntl.LOCK_UN)


def _download(item: ModelFile, directory: Path, hub_endpoint: str) -> Path:
    from huggingface_hub import hf_hub_download
    from huggingface_hub.errors import HfHubHTTPError, LocalEntryNotFoundError
    import httpx

    try:
        return Path(hf_hub_download(
            repo_id=item.repo,
            revision=item.revision,
            filename=item.filename,
            local_dir=directory,
            endpoint=hub_endpoint,
        ))
    except HfHubHTTPError as error:
        status = error.response.status_code
        raise PreparationError(
            f"model download failed for {item.relative_path} (HTTP {status}); check access and endpoint settings"
        ) from None
    except (httpx.TransportError, LocalEntryNotFoundError):
        raise PreparationError(
            f"model download failed for {item.relative_path}; check endpoint reachability and download timeouts"
        ) from None


def prepare(
    root: Path,
    manifest: Manifest,
    *,
    download: Callable[[ModelFile, Path, str], Path] = _download,
    progress: Callable[[dict], None] | None = None,
) -> Path:
    """Return a verified immutable snapshot; reuse it without contacting the Hub.

    The downloader writes one declared file beneath its supplied directory.
    Download failures leave a private partial snapshot for a later retry. A
    corrupt published snapshot fails explicitly rather than mutating files that
    another inference process may currently be using.
    """
    hub_endpoint = endpoint()
    identifier = manifest.identifier
    with _locked(root):
        snapshot = _inside(root, f"snapshots/{identifier}")
        if snapshot.exists():
            verify(snapshot, manifest)
            return snapshot
        pending = _inside(root, f"downloads/{identifier}")
        pending.mkdir(parents=True, exist_ok=True)
        total = sum(item.bytes for item in manifest.files)
        complete = 0
        for item in manifest.files:
            path = _inside(pending, item.relative_path)
            if not _verify(path, item):
                # Hub metadata must not cause reuse of a known corrupt file.
                path.unlink(missing_ok=True)
                directory = _inside(pending, item.directory)
                directory.mkdir(parents=True, exist_ok=True)
                downloaded = download(item, directory, hub_endpoint)
                if downloaded.resolve() != path.resolve() or not _verify(path, item):
                    raise PreparationError(f"model file {item.relative_path} failed download verification")
            complete += item.bytes
            if progress is not None:
                progress({"phase": "preparing_models", "completed_bytes": complete, "total_bytes": total})
        # Replace the entry itself so retained symlinks cannot redirect writes
        # into another file, including an already-verified model checkpoint.
        metadata = tempfile.NamedTemporaryFile(mode="w", dir=pending, prefix=".manifest-", encoding="utf-8", delete=False)
        try:
            with metadata:
                metadata.write(json.dumps(manifest.document(), indent=2) + "\n")
            os.replace(metadata.name, pending / "manifest.json")
        finally:
            Path(metadata.name).unlink(missing_ok=True)
        snapshot.parent.mkdir(parents=True, exist_ok=True)
        pending.rename(snapshot)
        return snapshot
