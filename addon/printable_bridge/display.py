"""Private authenticated display owned by the Blender supervisor."""

from __future__ import annotations

import os
from pathlib import Path
import secrets
import selectors
import subprocess
import tempfile
import time


class DisplayError(RuntimeError):
    """The private display could not satisfy its lifecycle contract."""


class PrivateDisplay:
    def __init__(self, state_dir: Path, startup_timeout_seconds: float = 15):
        self._state_dir = state_dir
        self._startup_timeout_seconds = startup_timeout_seconds
        self._directory: tempfile.TemporaryDirectory[str] | None = None
        self._process: subprocess.Popen[bytes] | None = None

    @property
    def pid(self) -> int | None:
        process = self._process
        return process.pid if process is not None and process.poll() is None else None

    @property
    def environment(self) -> dict[str, str]:
        if self._directory is None or self.pid is None:
            raise DisplayError("private display is not running")
        return {
            "DISPLAY": ":99",
            "XAUTHORITY": str(Path(self._directory.name) / "Xauthority"),
        }

    def start(self) -> None:
        deadline = time.monotonic() + self._startup_timeout_seconds
        self._state_dir.mkdir(parents=True, exist_ok=True)
        self._directory = tempfile.TemporaryDirectory(prefix="display-", dir=self._state_dir)
        authority = Path(self._directory.name) / "Xauthority"
        try:
            authority.touch(mode=0o600)
            subprocess.run(
                ["xauth", "-q", "-f", str(authority)],
                input=f"add :99 MIT-MAGIC-COOKIE-1 {secrets.token_hex(16)}\n".encode(),
                stdout=subprocess.DEVNULL,
                stderr=subprocess.DEVNULL,
                check=True,
                timeout=max(0, deadline - time.monotonic()),
            )
            self._start_xvfb(authority, deadline)
        except (OSError, subprocess.SubprocessError, DisplayError) as error:
            self.close(0)
            raise DisplayError("private display startup failed") from error

    def _start_xvfb(self, authority: Path, deadline: float) -> None:
        read_descriptor, write_descriptor = os.pipe()
        try:
            self._process = subprocess.Popen(
                [
                    "Xvfb", ":99", "-screen", "0", "1600x1200x24",
                    "-nolisten", "tcp", "-noreset", "-auth", str(authority),
                    "-displayfd", str(write_descriptor),
                ],
                pass_fds=(write_descriptor,),
            )
            os.close(write_descriptor)
            write_descriptor = -1
            with selectors.DefaultSelector() as selector:
                selector.register(read_descriptor, selectors.EVENT_READ)
                readiness = bytearray()
                # Xvfb writes the display number and newline separately.
                while readiness != b"99\n":
                    remaining = deadline - time.monotonic()
                    if remaining <= 0 or not selector.select(remaining):
                        raise DisplayError("private display did not become ready")
                    chunk = os.read(read_descriptor, 16)
                    readiness.extend(chunk)
                    if not chunk or not b"99\n".startswith(readiness):
                        raise DisplayError("private display readiness was invalid")
                if self.pid is None:
                    raise DisplayError("private display readiness was invalid")
        finally:
            os.close(read_descriptor)
            if write_descriptor >= 0:
                os.close(write_descriptor)

    def close(self, timeout: float) -> None:
        process = self._process
        if process is not None:
            if process.poll() is None:
                process.terminate()
            try:
                process.wait(timeout=max(0, timeout))
            except subprocess.TimeoutExpired:
                process.kill()
                process.wait()
            self._process = None
        if self._directory is not None:
            self._directory.cleanup()
            self._directory = None
