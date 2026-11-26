"""Prepare pinned model data in persistent storage, without loading its code.

Files come from a Hub repository at a pinned commit, or from a fixed HTTPS URL
for weights a publisher hosts outside the Hub; either way the SHA-256 digest
and size are pinned. Downloads are resumable. Only a complete, hash-verified
snapshot is published; the cross-process lock also covers the final readiness
check and publication.
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
import time
from typing import Callable, Iterator
import urllib.error
import urllib.request
from urllib.parse import urlsplit


# A URL download in progress, beside the file it will become.
PARTIAL_SUFFIX = ".partial"


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


def _https_url(value: object) -> str:
    if not isinstance(value, str):
        raise ValueError("a model URL must be a string")
    parsed = urlsplit(value)
    if (
        parsed.scheme != "https"
        or not parsed.hostname
        or parsed.username is not None
        or parsed.password is not None
        or parsed.query
        or parsed.fragment
        or not parsed.path.strip("/")
    ):
        raise ValueError("a model URL must be an HTTPS file URL without credentials or a query")
    return value


@dataclass(frozen=True)
class ModelFile:
    """One pinned file: from Hub `repo` at `revision`, or from `url` when `repo` is None."""

    repo: str | None
    revision: str | None
    directory: str
    filename: str
    sha256: str
    bytes: int
    url: str | None = None

    def __post_init__(self) -> None:
        if self.url is None:
            if not isinstance(self.repo, str) or not re.fullmatch(r"[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+", self.repo):
                raise ValueError("model repository must have an owner and name")
            if not isinstance(self.revision, str) or not re.fullmatch(r"[0-9a-f]{40}", self.revision):
                raise ValueError("model revision must be a full commit identifier")
        else:
            if self.repo is not None or self.revision is not None:
                raise ValueError("a model file comes from a Hub repository or a URL, not both")
            _https_url(self.url)
        if not re.fullmatch(r"[0-9a-f]{64}", self.sha256):
            raise ValueError("model files need a SHA-256 digest")
        if isinstance(self.bytes, bool) or not isinstance(self.bytes, int) or self.bytes < 1:
            raise ValueError("model file size must be a positive integer")
        _relative(self.directory)
        _relative(self.filename)
        if self.filename.endswith(PARTIAL_SUFFIX):
            raise ValueError("model file names must not end in .partial, which names downloads in progress")

    @property
    def relative_path(self) -> str:
        return f"{self.directory}/{self.filename}"

    @classmethod
    def from_document(cls, entry: dict) -> ModelFile:
        """A file from its manifest entry, where URL files omit the Hub fields."""
        if not isinstance(entry, dict):
            raise ValueError("a model file entry must be an object")
        return cls(**{"repo": None, "revision": None, **entry})

    @property
    def source(self) -> tuple[str, ...]:
        """What one snapshot directory may be filled from."""
        return ("url",) if self.url is not None else (self.repo, self.revision)

    def document(self) -> dict:
        """The file's manifest entry; Hub files keep their original fields so identifiers are stable."""
        entry = asdict(self)
        for key in ("repo", "revision", "url"):
            if entry[key] is None:
                del entry[key]
        return entry


@dataclass(frozen=True)
class Manifest:
    files: tuple[ModelFile, ...]

    def __post_init__(self) -> None:
        if not self.files:
            raise ValueError("a model manifest must list its files")
        paths = [item.relative_path for item in self.files]
        if len(paths) != len(set(paths)):
            raise ValueError("model files must have distinct destinations")
        directories: dict[str, tuple[str, ...]] = {}
        for item in self.files:
            if directories.setdefault(item.directory, item.source) != item.source:
                raise ValueError("each model directory must have one repository and revision")

    def document(self) -> dict:
        return {"format": "printable-models 1", "files": [item.document() for item in self.files]}

    @property
    def identifier(self) -> str:
        encoded = json.dumps(self.document(), sort_keys=True, separators=(",", ":")).encode()
        return hashlib.sha256(encoded).hexdigest()

    @classmethod
    def read(cls, path: Path) -> Manifest:
        data = json.loads(path.read_text(encoding="utf-8"))
        if not isinstance(data, dict) or data.get("format") != "printable-models 1":
            raise ValueError("unsupported model manifest format")
        return cls(tuple(ModelFile.from_document(item) for item in data["files"]))


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


URL_TIMEOUT_SECONDS = 60
_CHUNK_BYTES = 1 << 20


class _HttpsRedirects(urllib.request.HTTPRedirectHandler):
    """Follow a redirect only to another HTTPS URL, so a pinned HTTPS file is never fetched in plain text."""

    def redirect_request(self, req, fp, code, msg, headers, newurl):
        if urlsplit(newurl).scheme != "https":
            raise PreparationError("a model download redirected away from HTTPS; the download was refused")
        return super().redirect_request(req, fp, code, msg, headers, newurl)


_OPENER = urllib.request.build_opener(_HttpsRedirects)


def _download_url(item: ModelFile, directory: Path, opener: Callable = _OPENER.open) -> Path:
    """Fetch a pinned-URL file, resuming a partial download when the server allows ranges.

    The partial file is renamed into place only once it has the pinned size;
    a response that ends early keeps it for the next attempt to resume.
    """
    target = _inside(directory, item.filename)
    partial = _inside(directory, item.filename + PARTIAL_SUFFIX)
    target.parent.mkdir(parents=True, exist_ok=True)
    offset = partial.stat().st_size if partial.is_file() else 0
    if offset > item.bytes:
        partial.unlink()
        offset = 0
    if offset < item.bytes:
        request = urllib.request.Request(item.url, headers={"Range": f"bytes={offset}-"} if offset else {})
        try:
            with opener(request, timeout=URL_TIMEOUT_SECONDS) as response:
                if offset and getattr(response, "status", 200) != 206:
                    offset = 0  # the server ignored the range; start again
                flags = os.O_WRONLY | os.O_CREAT | os.O_NOFOLLOW | (os.O_APPEND if offset else os.O_TRUNC)
                with os.fdopen(os.open(partial, flags, 0o644), "ab") as sink:
                    written = offset
                    while chunk := response.read(_CHUNK_BYTES):
                        written += len(chunk)
                        if written > item.bytes:
                            raise PreparationError(f"model download for {item.relative_path} is larger than pinned")
                        sink.write(chunk)
        except urllib.error.HTTPError as error:
            raise PreparationError(
                f"model download failed for {item.relative_path} (HTTP {error.code}); check network access"
            ) from None
        except (urllib.error.URLError, TimeoutError, ConnectionError):
            raise PreparationError(
                f"model download failed for {item.relative_path}; check network access and retry"
            ) from None
    if partial.stat().st_size != item.bytes:
        raise PreparationError(f"model download for {item.relative_path} ended early; retrying resumes it")
    os.replace(partial, target)
    return target


def _download(item: ModelFile, directory: Path, hub_endpoint: str) -> Path:
    if item.url is not None:
        return _download_url(item, directory)
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
            token=os.environ.get("HF_TOKEN", False),
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


def _preparation_file(root: Path, manifest: Manifest) -> Path:
    return root / f"preparation-{manifest.identifier}.json"


def _record_preparation(root: Path, manifest: Manifest, state: str) -> None:
    document = {"format": "printable-preparation 1", "manifest": manifest.identifier, "state": state,
                "last_verified_unix_ms": time.time_ns() // 1_000_000 if state == "prepared" else None}
    temporary = tempfile.NamedTemporaryFile(mode="w", dir=root, prefix=".preparation-", encoding="utf-8", delete=False)
    try:
        with temporary:
            json.dump(document, temporary)
        os.replace(temporary.name, _preparation_file(root, manifest))
    finally:
        Path(temporary.name).unlink(missing_ok=True)


@contextmanager
def _preparing(root: Path, manifest: Manifest) -> Iterator[None]:
    _record_preparation(root, manifest, "preparing")
    try:
        yield
    except (PreparationError, OSError, ValueError):
        _record_preparation(root, manifest, "failed")
        raise
    else:
        _record_preparation(root, manifest, "prepared")


def preparation_status(root: Path, manifest: Manifest) -> dict:
    """Report the last verification attempt without rereading model tensors.

    A prepared state records completed verification, not a promise that an
    operator has not changed files since. Every inference preparation verifies
    them again. An abandoned attempt is interrupted once its lock is free.
    """
    unknown = {"state": "failed", "last_verified_unix_ms": None}
    try:
        path = _inside(root, _preparation_file(root, manifest).name)
        with path.open("rb") as source:
            payload = source.read(4097)
        if len(payload) > 4096:
            return unknown
        document = json.loads(payload)
        state = document["state"]
        verified = document["last_verified_unix_ms"]
        if (document.get("format") != "printable-preparation 1" or document.get("manifest") != manifest.identifier
                or state not in ("preparing", "prepared", "failed")
                or (state != "prepared" and verified is not None)
                or (state == "prepared" and (isinstance(verified, bool) or not isinstance(verified, int) or verified < 1))):
            return unknown
        if state == "preparing":
            with _inside(root, "prepare.lock").open("a+b") as lock:
                try:
                    fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
                except BlockingIOError:
                    pass
                else:
                    fcntl.flock(lock, fcntl.LOCK_UN)
                    state = "interrupted"
        return {"state": state, "last_verified_unix_ms": verified}
    except FileNotFoundError:
        return {"state": "not_prepared", "last_verified_unix_ms": None}
    except (PreparationError, OSError, ValueError, KeyError, TypeError):
        return unknown


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
    with _locked(root), _preparing(root, manifest):
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
