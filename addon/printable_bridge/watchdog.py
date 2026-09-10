"""Execution-deadline channel owned by the Blender supervisor process."""

from __future__ import annotations

import math
import os
import stat
import struct
import time
from typing import Protocol


WATCHDOG_FD_ENV = "PRINTABLE_BLENDER_WATCHDOG_FD"
WATCHDOG_RESTART_EXIT_CODE = 75
WATCHDOG_MESSAGE = struct.Struct("!cd")
ARM_OPERATION = b"A"
DISARM_OPERATION = b"D"


class WatchdogError(RuntimeError):
    """The supervisor watchdog channel is missing or unusable."""


class ExecutionWatchdog(Protocol):
    def arm(self, deadline: float) -> None: ...

    def disarm(self) -> None: ...

    def close(self) -> None: ...


class SupervisorWatchdog:
    def __init__(self, descriptor: int):
        self._descriptor = descriptor
        self._write = os.write
        self._close = os.close
        self._monotonic = time.monotonic
        self._closed = False

    @classmethod
    def from_env(cls) -> SupervisorWatchdog:
        raw = os.environ.pop(WATCHDOG_FD_ENV, None)
        if raw is None:
            raise WatchdogError("supervisor watchdog descriptor is missing")
        try:
            descriptor = int(raw)
            mode = os.fstat(descriptor).st_mode
        except (OSError, ValueError) as error:
            raise WatchdogError("supervisor watchdog descriptor is invalid") from error
        if descriptor < 0 or not stat.S_ISFIFO(mode):
            raise WatchdogError("supervisor watchdog descriptor is invalid")
        return cls(descriptor)

    def arm(self, deadline: float) -> None:
        if not math.isfinite(deadline) or deadline <= 0:
            raise WatchdogError("execution watchdog deadline is invalid")
        self._send(WATCHDOG_MESSAGE.pack(ARM_OPERATION, deadline))

    def disarm(self) -> None:
        self._send(WATCHDOG_MESSAGE.pack(DISARM_OPERATION, self._monotonic()))

    def close(self) -> None:
        if self._closed:
            return
        self._closed = True
        try:
            self._close(self._descriptor)
        except OSError as error:
            raise WatchdogError("cannot close supervisor watchdog channel") from error

    def _send(self, payload: bytes) -> None:
        if self._closed:
            raise WatchdogError("supervisor watchdog channel is closed")
        remaining = memoryview(payload)
        try:
            while remaining:
                written = self._write(self._descriptor, remaining)
                if written <= 0:
                    raise OSError("watchdog pipe accepted no bytes")
                remaining = remaining[written:]
        except OSError as error:
            raise WatchdogError("cannot notify supervisor watchdog") from error


class NoopExecutionWatchdog:
    """Test boundary for handler tests that do not launch the supervisor."""

    def arm(self, deadline: float) -> None:
        if not math.isfinite(deadline) or deadline <= 0:
            raise WatchdogError("execution watchdog deadline is invalid")

    def disarm(self) -> None:
        pass

    def close(self) -> None:
        pass
