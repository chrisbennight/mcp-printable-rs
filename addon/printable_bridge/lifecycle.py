"""Exactly-once terminal state selection for bridge requests."""

from __future__ import annotations

from dataclasses import dataclass, field
from enum import Enum
import threading
import time
from typing import Any

from .envelope import Request, failure


class RequestPhase(Enum):
    QUEUED = "queued"
    RUNNING = "running"
    COMPLETED = "completed"
    TIMED_OUT = "timed_out"
    SHUTDOWN = "shutdown"


TERMINAL_PHASES = {
    RequestPhase.COMPLETED,
    RequestPhase.TIMED_OUT,
    RequestPhase.SHUTDOWN,
}


@dataclass
class WorkItem:
    request: Request
    deadline: float
    run_budget_seconds: float
    _phase: RequestPhase = field(default=RequestPhase.QUEUED, init=False)
    _response: dict[str, Any] | None = field(default=None, init=False)
    _lock: threading.Lock = field(default_factory=threading.Lock, init=False)
    _terminal: threading.Event = field(default_factory=threading.Event, init=False)

    @classmethod
    def with_budget(cls, request: Request, seconds: float) -> WorkItem:
        return cls(
            request=request,
            deadline=time.monotonic() + seconds,
            run_budget_seconds=seconds,
        )

    @property
    def phase(self) -> RequestPhase:
        with self._lock:
            return self._phase

    def start(self, now: float | None = None) -> bool:
        current_time = time.monotonic() if now is None else now
        with self._lock:
            if self._phase is not RequestPhase.QUEUED:
                return False
            if current_time >= self.deadline:
                self._select_terminal(
                    RequestPhase.TIMED_OUT,
                    self._timeout_response(),
                )
                return False
            self.deadline = current_time + self.run_budget_seconds
            self._phase = RequestPhase.RUNNING
            return True

    def complete(
        self, response: dict[str, Any], now: float | None = None
    ) -> bool:
        current_time = time.monotonic() if now is None else now
        with self._lock:
            if self._phase is not RequestPhase.RUNNING:
                return False
            if current_time >= self.deadline:
                self._select_terminal(
                    RequestPhase.TIMED_OUT,
                    self._timeout_response(),
                )
                return False
            self._select_terminal(RequestPhase.COMPLETED, response)
            return True

    def timeout(self) -> bool:
        with self._lock:
            if self._phase in TERMINAL_PHASES:
                return False
            self._select_terminal(
                RequestPhase.TIMED_OUT,
                self._timeout_response(),
            )
            return True

    def shutdown(self) -> bool:
        with self._lock:
            if self._phase in TERMINAL_PHASES:
                return False
            self._select_terminal(
                RequestPhase.SHUTDOWN,
                failure(self.request.request_id, "Blender bridge is shutting down"),
            )
            return True

    def await_response(self) -> dict[str, Any]:
        while True:
            with self._lock:
                if self._response is not None:
                    return self._response
                deadline = self.deadline
            remaining = deadline - time.monotonic()
            if remaining <= 0:
                if self._timeout_if_due():
                    continue
                continue
            self._terminal.wait(min(remaining, threading.TIMEOUT_MAX))

    def _timeout_if_due(self, now: float | None = None) -> bool:
        current_time = time.monotonic() if now is None else now
        with self._lock:
            if self._phase in TERMINAL_PHASES or current_time < self.deadline:
                return False
            self._select_terminal(
                RequestPhase.TIMED_OUT,
                self._timeout_response(),
            )
            return True

    def _select_terminal(
        self, phase: RequestPhase, response: dict[str, Any]
    ) -> None:
        self._phase = phase
        self._response = response
        self._terminal.set()

    def _timeout_response(self) -> dict[str, Any]:
        return failure(
            self.request.request_id,
            f"command {self.request.command} exceeded its execution timeout",
        )
