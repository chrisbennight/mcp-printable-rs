"""Descriptor-rooted workspace staging for Blender file operations."""

from __future__ import annotations

from contextlib import contextmanager
from dataclasses import dataclass
import os
from pathlib import Path, PurePosixPath
import resource
import secrets
import stat
from typing import Iterator


MAX_ARTIFACT_BYTES = 1024 * 1024 * 1024
RESERVED_WORKSPACE_ROOT = ".printable"
DIRECTORY_FLAGS = os.O_RDONLY | os.O_DIRECTORY | os.O_CLOEXEC | os.O_NOFOLLOW
FILE_READ_FLAGS = os.O_RDONLY | os.O_CLOEXEC | os.O_NOFOLLOW | os.O_NONBLOCK


class WorkspaceError(ValueError):
    """A workspace operation cannot preserve its confinement contract."""


def enforce_process_file_size_limit() -> None:
    try:
        soft, hard = resource.getrlimit(resource.RLIMIT_FSIZE)
        limited = (
            MAX_ARTIFACT_BYTES
            if soft == resource.RLIM_INFINITY
            else min(soft, MAX_ARTIFACT_BYTES)
        )
        if limited <= 0:
            raise WorkspaceError("process file size limit prevents artifact staging")
        resource.setrlimit(resource.RLIMIT_FSIZE, (limited, hard))
    except (OSError, ValueError) as error:
        raise WorkspaceError("process file size limit could not be enforced") from error


@dataclass(frozen=True)
class WorkspacePath:
    relative: str
    parts: tuple[str, ...]
    suffix: str


class StagedOutput:
    def __init__(
        self,
        workspace: SecureWorkspace,
        stage_path: Path,
        parent_fd: int,
        leaf: str,
        destination_exists: bool,
    ):
        self.workspace = workspace
        self.path = stage_path
        self.parent_fd = parent_fd
        self.leaf = leaf
        self.destination_exists = destination_exists
        self.committed = False
        self.committed_identity: tuple[int, int] | None = None

    def commit(self) -> None:
        if self.committed:
            raise WorkspaceError("staged output was already committed")
        self.committed_identity = self.workspace._commit_output(
            self.path,
            self.parent_fd,
            self.leaf,
            rollback_on_failure=not self.destination_exists,
        )
        self.committed = True

    def rollback(self) -> None:
        if not self.committed or self.committed_identity is None:
            raise WorkspaceError("staged output was not committed")
        self.workspace._rollback_output(
            self.parent_fd, self.leaf, self.committed_identity
        )
        self.committed = False
        self.committed_identity = None


class SecureWorkspace:
    def __init__(self, root: Path, staging_root: Path):
        try:
            self._root_fd = os.open(root, DIRECTORY_FLAGS)
        except OSError as error:
            raise WorkspaceError("Blender workspace root is not a safe directory") from error
        self._staging_root = staging_root
        try:
            staging_root.mkdir(mode=0o700, parents=True, exist_ok=True)
            self._staging_fd = os.open(staging_root, DIRECTORY_FLAGS)
            os.fchmod(self._staging_fd, 0o700)
        except OSError as error:
            os.close(self._root_fd)
            raise WorkspaceError("Blender staging root is not a safe directory") from error

    def close(self) -> None:
        root_fd = getattr(self, "_root_fd", None)
        staging_fd = getattr(self, "_staging_fd", None)
        if root_fd is not None:
            os.close(root_fd)
            self._root_fd = None
        if staging_fd is not None:
            os.close(staging_fd)
            self._staging_fd = None

    @staticmethod
    def validate(raw: object, suffix: str) -> WorkspacePath:
        return SecureWorkspace._validate(raw, suffix, reserved=False)

    @staticmethod
    def validate_reserved(raw: object, suffix: str) -> WorkspacePath:
        return SecureWorkspace._validate(raw, suffix, reserved=True)

    @staticmethod
    def _validate(raw: object, suffix: str, *, reserved: bool) -> WorkspacePath:
        if not isinstance(raw, str) or not raw or len(raw.encode("utf-8")) > 1024:
            raise WorkspaceError("path must be a non-empty workspace-relative path")
        if "\x00" in raw or "\\" in raw:
            raise WorkspaceError("path contains an unsupported character")
        path = PurePosixPath(raw)
        if path.is_absolute() or not path.parts or any(
            part in {"", ".", ".."} for part in path.parts
        ):
            raise WorkspaceError("path must stay within the Blender workspace")
        is_reserved = path.parts[0] == RESERVED_WORKSPACE_ROOT
        if is_reserved and not reserved:
            raise WorkspaceError("path is reserved for Printable internal state")
        if reserved and not is_reserved:
            raise WorkspaceError("path must stay within Printable internal state")
        normalized_suffix = suffix.lower()
        if path.suffix.lower() != normalized_suffix:
            raise WorkspaceError(f"path must end in {normalized_suffix}")
        return WorkspacePath(
            relative="/".join(path.parts),
            parts=tuple(path.parts),
            suffix=normalized_suffix,
        )

    @contextmanager
    def stage_input(self, request: WorkspacePath) -> Iterator[Path]:
        with self._open_parent(request.parts, create=False) as (parent_fd, leaf):
            try:
                source_fd = os.open(leaf, FILE_READ_FLAGS, dir_fd=parent_fd)
            except OSError as error:
                raise WorkspaceError("input artifact is unavailable or unsafe") from error
            try:
                before = os.fstat(source_fd)
                if not stat.S_ISREG(before.st_mode):
                    raise WorkspaceError("input artifact must be a regular file")
                stage_path = self._new_stage_path(request.suffix)
                try:
                    with os.fdopen(os.dup(source_fd), "rb") as source, stage_path.open(
                        "wb"
                    ) as destination:
                        self._copy_limited(source, destination)
                        destination.flush()
                        os.fsync(destination.fileno())
                    after = os.fstat(source_fd)
                    if self._changed_during_copy(before, after):
                        raise WorkspaceError("input artifact changed while being staged")
                    yield stage_path
                finally:
                    self._remove_stage(stage_path)
            finally:
                os.close(source_fd)

    def input_identity(self, request: WorkspacePath) -> tuple[int, int, int, int, int]:
        """Observe a confined regular input for later source-change checks."""
        with self._open_parent(request.parts, create=False) as (parent_fd, leaf):
            try:
                descriptor = os.open(leaf, FILE_READ_FLAGS, dir_fd=parent_fd)
            except OSError as error:
                raise WorkspaceError("input artifact is unavailable or unsafe") from error
            try:
                current = os.fstat(descriptor)
                if not stat.S_ISREG(current.st_mode):
                    raise WorkspaceError("input artifact must be a regular file")
                return (current.st_dev, current.st_ino, current.st_size,
                        current.st_mtime_ns, current.st_ctime_ns)
            finally:
                os.close(descriptor)

    @contextmanager
    def stage_output(self, request: WorkspacePath) -> Iterator[StagedOutput]:
        with self._open_parent(request.parts, create=True) as (parent_fd, leaf):
            destination_exists = self._validate_output_leaf(parent_fd, leaf)
            stage_path = self._new_stage_path(request.suffix)
            output = StagedOutput(
                self, stage_path, parent_fd, leaf, destination_exists
            )
            try:
                yield output
            finally:
                self._remove_stage(stage_path)

    def commit_batch(self, outputs: list[StagedOutput]) -> None:
        if not outputs:
            raise WorkspaceError("artifact batch must not be empty")
        if any(
            output.workspace is not self
            or output.committed
            or output.destination_exists
            for output in outputs
        ):
            raise WorkspaceError(
                "artifact batch requires new uncommitted workspace outputs"
            )
        committed: list[StagedOutput] = []
        try:
            for output in outputs:
                output.commit()
                committed.append(output)
        except Exception:
            rollback_error: Exception | None = None
            for output in reversed(committed):
                try:
                    output.rollback()
                except Exception as error:
                    if rollback_error is None:
                        rollback_error = error
            if rollback_error is not None:
                raise WorkspaceError(
                    "artifact batch commit and rollback both failed"
                ) from rollback_error
            raise

    @contextmanager
    def _open_parent(
        self, parts: tuple[str, ...], *, create: bool
    ) -> Iterator[tuple[int, str]]:
        if self._root_fd is None:
            raise WorkspaceError("Blender workspace is closed")
        current_fd = os.dup(self._root_fd)
        try:
            for component in parts[:-1]:
                if create:
                    try:
                        os.mkdir(component, mode=0o770, dir_fd=current_fd)
                    except FileExistsError:
                        pass
                    except OSError as error:
                        raise WorkspaceError(
                            "output directory could not be created safely"
                        ) from error
                try:
                    next_fd = os.open(component, DIRECTORY_FLAGS, dir_fd=current_fd)
                except OSError as error:
                    raise WorkspaceError("workspace path contains an unsafe directory") from error
                os.close(current_fd)
                current_fd = next_fd
            yield current_fd, parts[-1]
        finally:
            os.close(current_fd)

    def _new_stage_path(self, suffix: str) -> Path:
        if self._staging_fd is None:
            raise WorkspaceError("Blender workspace is closed")
        for _attempt in range(16):
            name = f"artifact-{secrets.token_hex(16)}{suffix}"
            try:
                descriptor = os.open(
                    name,
                    os.O_WRONLY | os.O_CREAT | os.O_EXCL | os.O_CLOEXEC | os.O_NOFOLLOW,
                    0o600,
                    dir_fd=self._staging_fd,
                )
            except FileExistsError:
                continue
            except OSError as error:
                raise WorkspaceError("private staging file could not be created") from error
            os.close(descriptor)
            return self._staging_root / name
        raise WorkspaceError("private staging name allocation failed")

    @staticmethod
    def _validate_output_leaf(parent_fd: int, leaf: str) -> bool:
        try:
            target = os.stat(leaf, dir_fd=parent_fd, follow_symlinks=False)
        except FileNotFoundError:
            return False
        except OSError as error:
            raise WorkspaceError("output destination could not be validated") from error
        if not (stat.S_ISREG(target.st_mode) or stat.S_ISLNK(target.st_mode)):
            raise WorkspaceError("output destination is not a replaceable file")
        return True

    def _commit_output(
        self,
        stage_path: Path,
        parent_fd: int,
        leaf: str,
        *,
        rollback_on_failure: bool,
    ) -> tuple[int, int]:
        try:
            source_fd = os.open(stage_path, FILE_READ_FLAGS)
        except OSError as error:
            raise WorkspaceError("Blender did not produce the requested artifact") from error
        try:
            source_stat = os.fstat(source_fd)
            if not stat.S_ISREG(source_stat.st_mode) or source_stat.st_size == 0:
                raise WorkspaceError("Blender produced an empty or invalid artifact")
            if source_stat.st_size > MAX_ARTIFACT_BYTES:
                raise WorkspaceError("Blender artifact exceeds the staging limit")
            temporary = f".{leaf}.tmp-{secrets.token_hex(16)}"
            destination_fd: int | None = None
            committed_identity: tuple[int, int] | None = None
            try:
                destination_fd = os.open(
                    temporary,
                    os.O_WRONLY
                    | os.O_CREAT
                    | os.O_EXCL
                    | os.O_CLOEXEC
                    | os.O_NOFOLLOW,
                    0o660,
                    dir_fd=parent_fd,
                )
                with os.fdopen(os.dup(source_fd), "rb") as source, os.fdopen(
                    destination_fd, "wb", closefd=False
                ) as destination:
                    self._copy_limited(source, destination)
                    destination.flush()
                    os.fsync(destination.fileno())
                prepared = os.fstat(destination_fd)
                prepared_identity = (prepared.st_dev, prepared.st_ino)
                os.close(destination_fd)
                destination_fd = None
                os.replace(
                    temporary,
                    leaf,
                    src_dir_fd=parent_fd,
                    dst_dir_fd=parent_fd,
                )
                committed_identity = prepared_identity
                committed = os.stat(
                    leaf, dir_fd=parent_fd, follow_symlinks=False
                )
                if (committed.st_dev, committed.st_ino) != committed_identity:
                    raise WorkspaceError(
                        "committed artifact identity changed unexpectedly"
                    )
                os.fsync(parent_fd)
                return committed_identity
            except Exception:
                if destination_fd is not None:
                    os.close(destination_fd)
                rollback_error: Exception | None = None
                if rollback_on_failure and committed_identity is not None:
                    try:
                        self._rollback_output(
                            parent_fd, leaf, committed_identity
                        )
                    except Exception as error:
                        rollback_error = error
                try:
                    os.unlink(temporary, dir_fd=parent_fd)
                except FileNotFoundError:
                    pass
                except OSError as cleanup_error:
                    raise WorkspaceError(
                        "artifact commit and temporary cleanup both failed"
                    ) from cleanup_error
                if rollback_error is not None:
                    raise WorkspaceError(
                        "artifact commit and destination rollback both failed"
                    ) from rollback_error
                raise
        finally:
            os.close(source_fd)

    @staticmethod
    def _rollback_output(
        parent_fd: int, leaf: str, expected_identity: tuple[int, int]
    ) -> None:
        try:
            target = os.stat(leaf, dir_fd=parent_fd, follow_symlinks=False)
        except FileNotFoundError:
            return
        except OSError as error:
            raise WorkspaceError(
                "committed batch artifact could not be inspected for rollback"
            ) from error
        if (target.st_dev, target.st_ino) != expected_identity:
            raise WorkspaceError(
                "committed batch artifact changed before rollback"
            )
        try:
            os.unlink(leaf, dir_fd=parent_fd)
            os.fsync(parent_fd)
        except OSError as error:
            raise WorkspaceError(
                "committed batch artifact could not be rolled back"
            ) from error

    @staticmethod
    def _copy_limited(source: object, destination: object) -> None:
        copied = 0
        while True:
            chunk = source.read(1024 * 1024)
            if not chunk:
                return
            copied += len(chunk)
            if copied > MAX_ARTIFACT_BYTES:
                raise WorkspaceError("artifact exceeds the staging limit")
            destination.write(chunk)

    @staticmethod
    def _changed_during_copy(before: os.stat_result, after: os.stat_result) -> bool:
        return (
            before.st_dev,
            before.st_ino,
            before.st_size,
            before.st_mtime_ns,
        ) != (
            after.st_dev,
            after.st_ino,
            after.st_size,
            after.st_mtime_ns,
        )

    @staticmethod
    def _remove_stage(path: Path) -> None:
        try:
            path.unlink()
        except FileNotFoundError:
            pass
