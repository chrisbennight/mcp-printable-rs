"""Restart Blender after an execution watchdog terminates the process."""

from __future__ import annotations

import ctypes
import logging
import math
import os
from pathlib import Path
import selectors
import signal
import subprocess
import sys
import time
from typing import Sequence

from .config import (
    ConfigError,
    OWNED_DISPLAY_PID_ENV,
    blender_mode,
    blender_ui_backend,
    blender_shutdown_timeout_seconds,
)
from .display import DisplayError, PrivateDisplay
from .watchdog import (
    ARM_OPERATION,
    DISARM_OPERATION,
    WATCHDOG_FD_ENV,
    WATCHDOG_MESSAGE,
    WATCHDOG_RESTART_EXIT_CODE,
)


LOG = logging.getLogger("printable_bridge.supervisor")
WATCHDOG_POLL_SECONDS = 0.2
MAX_WATCHDOG_BUFFER_BYTES = 4096
PR_SET_CHILD_SUBREAPER = 36


class WatchdogProtocolError(RuntimeError):
    """The Blender child sent an invalid watchdog control message."""


class ProcessContainmentError(RuntimeError):
    """The supervisor could not remove all Blender descendants."""


def _enable_child_subreaper() -> None:
    if sys.platform != "linux":
        return
    libc = ctypes.CDLL(None, use_errno=True)
    prctl = libc.prctl
    prctl.argtypes = [
        ctypes.c_int,
        ctypes.c_ulong,
        ctypes.c_ulong,
        ctypes.c_ulong,
        ctypes.c_ulong,
    ]
    prctl.restype = ctypes.c_int
    if prctl(PR_SET_CHILD_SUBREAPER, 1, 0, 0, 0) != 0:
        error_number = ctypes.get_errno()
        raise OSError(error_number, os.strerror(error_number))


def _direct_child_pids(parent_pid: int) -> list[int]:
    if sys.platform != "linux":
        return []
    children: list[int] = []
    with os.scandir("/proc") as entries:
        for entry in entries:
            if not entry.name.isdecimal():
                continue
            try:
                status = Path(entry.path, "status").read_text(
                    encoding="utf-8", errors="replace"
                )
            except (FileNotFoundError, ProcessLookupError):
                continue
            for line in status.splitlines():
                if not line.startswith("PPid:"):
                    continue
                try:
                    observed_parent = int(line.removeprefix("PPid:").strip())
                except ValueError:
                    break
                if observed_parent == parent_pid:
                    children.append(int(entry.name))
                break
    return children


class BlenderSupervisor:
    def __init__(
        self,
        command: Sequence[str],
        state_dir: Path,
        shutdown_timeout_seconds: float,
        display: PrivateDisplay | None = None,
    ):
        self._command = list(command)
        self._state_dir = state_dir
        self._shutdown_timeout_seconds = shutdown_timeout_seconds
        self._child: subprocess.Popen[bytes] | None = None
        self._stopping = False
        self._shutdown_deadline: float | None = None
        self._display = display

    def run(self) -> int:
        try:
            _enable_child_subreaper()
        except (AttributeError, OSError) as error:
            LOG.error("Blender process containment failed: %s", type(error).__name__)
            return 1
        signal.signal(signal.SIGINT, self._stop)
        signal.signal(signal.SIGTERM, self._stop)
        exit_code = 1
        try:
            if self._display is not None:
                self._display.start()
            exit_code = self._run_blender()
        except DisplayError as error:
            LOG.error("Blender display failed: %s", error)
        finally:
            if self._display is not None:
                timeout = self._shutdown_timeout_seconds
                if self._shutdown_deadline is not None:
                    timeout = max(0, self._shutdown_deadline - time.monotonic())
                try:
                    self._display.close(timeout)
                except OSError as error:
                    LOG.error("Blender display cleanup failed: %s", type(error).__name__)
                    exit_code = 1
        return exit_code

    def _run_blender(self) -> int:
        while not self._stopping:
            try:
                self._remove_health_markers()
            except OSError:
                return 1
            child_environment = os.environ.copy()
            child_environment[OWNED_DISPLAY_PID_ENV] = "0"
            if self._display is not None:
                child_environment.update(self._display.environment)
                child_environment[OWNED_DISPLAY_PID_ENV] = str(self._display.pid or 0)
            read_descriptor, write_descriptor = os.pipe()
            child_environment[WATCHDOG_FD_ENV] = str(write_descriptor)
            try:
                self._child = subprocess.Popen(
                    self._command,
                    env=child_environment,
                    pass_fds=(write_descriptor,),
                    start_new_session=True,
                )
            except OSError as error:
                os.close(read_descriptor)
                os.close(write_descriptor)
                LOG.error("Blender launch failed: %s", type(error).__name__)
                return 1
            os.close(write_descriptor)
            if self._stopping:
                self._forward(signal.SIGTERM)
            try:
                return_code = self._wait_for_child(read_descriptor)
                self._kill_child()
            except (OSError, DisplayError, ProcessContainmentError, WatchdogProtocolError) as error:
                LOG.error("Blender watchdog failed: %s", type(error).__name__)
                try:
                    self._kill_child()
                except (OSError, ProcessContainmentError) as cleanup_error:
                    LOG.error(
                        "Blender process cleanup failed: %s",
                        type(cleanup_error).__name__,
                    )
                return 1
            finally:
                os.close(read_descriptor)
            self._child = None
            if self._stopping:
                return 0
            if return_code != WATCHDOG_RESTART_EXIT_CODE:
                return return_code
            LOG.warning("Blender execution watchdog fired; restarting Blender")
        return 0

    def _wait_for_child(self, read_descriptor: int) -> int:
        child = self._child
        if child is None:
            raise RuntimeError("Blender child is not running")
        watchdog_deadline: float | None = None
        buffer = bytearray()
        with selectors.DefaultSelector() as selector:
            selector.register(read_descriptor, selectors.EVENT_READ)
            pipe_open = True
            while True:
                self._reap_adopted_children()
                if self._display is not None and self._display.pid is None:
                    raise DisplayError("private display stopped")
                return_code = child.poll()
                if return_code is not None:
                    return return_code
                now = time.monotonic()
                timeout = WATCHDOG_POLL_SECONDS
                if watchdog_deadline is not None:
                    timeout = min(timeout, max(0.0, watchdog_deadline - now))
                if self._shutdown_deadline is not None:
                    timeout = min(
                        timeout,
                        max(0.0, self._shutdown_deadline - now),
                    )
                events = selector.select(timeout)
                if events:
                    chunk = os.read(read_descriptor, MAX_WATCHDOG_BUFFER_BYTES)
                    if chunk:
                        buffer.extend(chunk)
                        if len(buffer) > MAX_WATCHDOG_BUFFER_BYTES:
                            raise WatchdogProtocolError("watchdog message is oversized")
                        watchdog_deadline, watchdog_expired = (
                            self._consume_watchdog_messages(
                                buffer, watchdog_deadline
                            )
                        )
                        if watchdog_expired:
                            return WATCHDOG_RESTART_EXIT_CODE
                    elif pipe_open:
                        selector.unregister(read_descriptor)
                        pipe_open = False
                if (
                    watchdog_deadline is not None
                    and time.monotonic() >= watchdog_deadline
                ):
                    return WATCHDOG_RESTART_EXIT_CODE
                if (
                    self._shutdown_deadline is not None
                    and time.monotonic() >= self._shutdown_deadline
                ):
                    LOG.warning(
                        "Blender did not stop within its shutdown grace; killing it"
                    )
                    return 0

    @staticmethod
    def _consume_watchdog_messages(
        buffer: bytearray, watchdog_deadline: float | None
    ) -> tuple[float | None, bool]:
        while buffer:
            if len(buffer) < WATCHDOG_MESSAGE.size:
                return watchdog_deadline, False
            operation, timestamp = WATCHDOG_MESSAGE.unpack(
                bytes(buffer[: WATCHDOG_MESSAGE.size])
            )
            if not math.isfinite(timestamp) or timestamp <= 0:
                raise WatchdogProtocolError("watchdog timestamp is invalid")
            del buffer[: WATCHDOG_MESSAGE.size]
            if operation == DISARM_OPERATION:
                if watchdog_deadline is None:
                    raise WatchdogProtocolError("watchdog is not armed")
                if timestamp >= watchdog_deadline:
                    return watchdog_deadline, True
                watchdog_deadline = None
                continue
            if operation != ARM_OPERATION:
                raise WatchdogProtocolError("unknown watchdog operation")
            if watchdog_deadline is not None:
                raise WatchdogProtocolError("watchdog is already armed")
            watchdog_deadline = timestamp
        return watchdog_deadline, False

    def _stop(self, signum: int, _frame: object) -> None:
        if self._shutdown_deadline is None:
            self._shutdown_deadline = (
                time.monotonic() + self._shutdown_timeout_seconds
            )
        self._stopping = True
        self._forward(signum)

    def _forward(self, signum: int) -> None:
        child = self._child
        if child is None:
            return
        try:
            os.killpg(child.pid, signum)
        except ProcessLookupError:
            pass

    def _kill_child(self) -> None:
        child = self._child
        if child is None:
            return
        try:
            os.killpg(child.pid, signal.SIGKILL)
        except ProcessLookupError:
            pass
        child.wait()
        self._kill_adopted_children()

    def _kill_adopted_children(self) -> None:
        cleanup_deadline = time.monotonic() + self._shutdown_timeout_seconds
        while True:
            display_pid = self._display.pid if self._display is not None else None
            children = [pid for pid in _direct_child_pids(os.getpid()) if pid != display_pid]
            if not children:
                return
            for pid in children:
                try:
                    os.kill(pid, signal.SIGKILL)
                except ProcessLookupError:
                    pass
            for pid in children:
                try:
                    os.waitpid(pid, os.WNOHANG)
                except (ChildProcessError, ProcessLookupError):
                    pass
            if time.monotonic() >= cleanup_deadline:
                raise ProcessContainmentError(
                    "caller descendants exceeded the cleanup grace"
                )
            time.sleep(0.01)

    def _reap_adopted_children(self) -> None:
        blender_pid = self._child.pid if self._child is not None else None
        display_pid = self._display.pid if self._display is not None else None
        for pid in _direct_child_pids(os.getpid()):
            if pid in {blender_pid, display_pid}:
                continue
            try:
                os.waitpid(pid, os.WNOHANG)
            except (ChildProcessError, ProcessLookupError):
                pass

    def _remove_health_markers(self) -> None:
        for name in ("ready.json", "live.json"):
            try:
                (self._state_dir / name).unlink(missing_ok=True)
            except OSError as error:
                LOG.error(
                    "Cannot clear stale Blender health marker: %s",
                    type(error).__name__,
                )
                raise


def main(argv: Sequence[str] | None = None) -> int:
    arguments = list(sys.argv[1:] if argv is None else argv)
    if len(arguments) != 2:
        LOG.error("usage: supervisor BLENDER_BIN REPOSITORY_ROOT")
        return 2
    blender_bin, repository_root = arguments
    state_dir = Path(
        os.environ.get(
            "PRINTABLE_BLENDER_STATE_DIR",
            str(Path(repository_root) / ".dev" / "printable-blender" / "state"),
        )
    )
    if not state_dir.is_absolute():
        LOG.error("PRINTABLE_BLENDER_STATE_DIR must be absolute")
        return 2
    try:
        shutdown_timeout_seconds = blender_shutdown_timeout_seconds()
        mode = blender_mode()
        backend = blender_ui_backend() if mode == "ui" else None
    except ConfigError as error:
        LOG.error("invalid supervisor configuration: %s", error)
        return 2
    command = [
        blender_bin,
        *(["--background"] if mode == "background" else [
            "--gpu-backend", "opengl", "--window-geometry", "0", "0", "1600", "1200",
        ]),
        "--factory-startup",
        "--disable-autoexec",
        "--offline-mode",
        "-noaudio",
        "--python-exit-code",
        "70",
        "--python",
        str(Path(repository_root) / "addon" / "launcher.py"),
    ]
    if backend == "egl":
        command = ["/opt/VirtualGL/bin/vglrun", "-d", "egl0", "-c", "proxy", *command]
    display = (
        PrivateDisplay(state_dir, min(15, shutdown_timeout_seconds))
        if mode == "ui" else None
    )
    return BlenderSupervisor(command, state_dir, shutdown_timeout_seconds, display=display).run()


if __name__ == "__main__":
    logging.basicConfig(level=logging.INFO)
    raise SystemExit(main())
