"""Bounded execution support for explicit Blender Python workflows."""

from __future__ import annotations

from contextlib import redirect_stderr, redirect_stdout
import json
import math
import os
import selectors
import signal
import sys
import threading
import time
from typing import Any, Callable

from .watchdog import (
    WATCHDOG_RESTART_EXIT_CODE,
    ExecutionWatchdog,
    WatchdogError,
)


MAX_OUTPUT_BYTES = 64 * 1024
MAX_RESULT_BYTES = 1024 * 1024
DEFAULT_TIMEOUT_SECONDS = 120.0
JSON_STRING_CHUNK_CHARACTERS = 4096


class CodeExecutionError(ValueError):
    """Caller-supplied code could not be executed within its contract."""


class _BoundedText:
    encoding = "utf-8"

    def __init__(self, limit: int):
        self._limit = limit
        self._buffer = bytearray()
        self._lock = threading.Lock()
        self.truncated = False

    def write(self, value: str) -> int:
        if not isinstance(value, str):
            raise TypeError("captured output must be text")
        with self._lock:
            remaining = self._limit - len(self._buffer)
            if remaining <= 0:
                if value:
                    self.truncated = True
                return len(value)
            prefix = value[:remaining]
            encoded = prefix.encode(self.encoding)
            if len(prefix) < len(value) or len(encoded) > remaining:
                self.truncated = True
            self._buffer.extend(encoded[:remaining])
        return len(value)

    def write_bytes(self, value: bytes) -> None:
        with self._lock:
            remaining = self._limit - len(self._buffer)
            if len(value) > remaining:
                self.truncated = True
            if remaining > 0:
                self._buffer.extend(value[:remaining])

    def flush(self) -> None:
        pass

    def value(self) -> str:
        with self._lock:
            return self._buffer.decode(self.encoding, errors="replace")


class _FileDescriptorCapture:
    def __init__(
        self,
        descriptor: int,
        output: _BoundedText,
        restart_process: Callable[[int], Any],
    ):
        self._descriptor = descriptor
        self._output = output
        self._restart_process = restart_process
        self._saved_descriptor: int | None = None
        self._data_read: int | None = None
        self._stop_read: int | None = None
        self._stop_write: int | None = None
        self._thread: threading.Thread | None = None
        self._inheritable = True
        self._failed = False

    def __enter__(self) -> _FileDescriptorCapture:
        try:
            self._inheritable = os.get_inheritable(self._descriptor)
            self._saved_descriptor = os.dup(self._descriptor)
            self._data_read, data_write = os.pipe()
            self._stop_read, self._stop_write = os.pipe()
            os.set_inheritable(self._stop_read, False)
            os.set_inheritable(self._stop_write, False)
            os.dup2(data_write, self._descriptor, inheritable=True)
            os.close(data_write)
            self._thread = threading.Thread(
                target=self._drain,
                name=f"printable-output-{self._descriptor}",
                daemon=True,
            )
            self._thread.start()
        except (OSError, RuntimeError):
            self._restart_process(WATCHDOG_RESTART_EXIT_CODE)
        return self

    def __exit__(self, _error_type: Any, _error: Any, _traceback: Any) -> None:
        saved_descriptor = self._saved_descriptor
        stop_write = self._stop_write
        thread = self._thread
        if saved_descriptor is None or stop_write is None or thread is None:
            raise RuntimeError("file descriptor capture was not started")
        try:
            os.dup2(
                saved_descriptor,
                self._descriptor,
                inheritable=self._inheritable,
            )
            os.close(saved_descriptor)
            try:
                os.write(stop_write, b"x")
            except BrokenPipeError:
                pass
            os.close(stop_write)
            thread.join()
        except (OSError, RuntimeError):
            self._restart_process(WATCHDOG_RESTART_EXIT_CODE)
        if self._failed:
            self._restart_process(WATCHDOG_RESTART_EXIT_CODE)

    def _drain(self) -> None:
        data_read = self._data_read
        stop_read = self._stop_read
        if data_read is None or stop_read is None:
            return
        try:
            with selectors.DefaultSelector() as selector:
                selector.register(data_read, selectors.EVENT_READ)
                selector.register(stop_read, selectors.EVENT_READ)
                while True:
                    stopping = False
                    for key, _events in selector.select():
                        if key.fd == stop_read:
                            os.read(stop_read, 1)
                            stopping = True
                            continue
                        chunk = os.read(data_read, 8192)
                        if not chunk:
                            return
                        self._output.write_bytes(chunk)
                    if stopping:
                        os.set_blocking(data_read, False)
                        while True:
                            try:
                                chunk = os.read(data_read, 8192)
                            except BlockingIOError:
                                return
                            if not chunk:
                                return
                            self._output.write_bytes(chunk)
        except OSError:
            self._failed = True
        finally:
            try:
                os.close(data_read)
            except OSError:
                self._failed = True
            try:
                os.close(stop_read)
            except OSError:
                self._failed = True


def _safe_exception_message(error: BaseException) -> str:
    try:
        message = str(error)
    except BaseException:
        message = ""
    encoded = message[:1024].encode("utf-8", errors="replace")[:1024]
    return encoded.decode("utf-8", errors="ignore")


class _JsonBudget:
    def __init__(self, limit: int):
        self.remaining = limit

    def consume(self, size: int) -> None:
        if size > self.remaining:
            raise CodeExecutionError(
                f"result exceeds the {MAX_RESULT_BYTES}-byte limit"
            )
        self.remaining -= size


def _normalize_json_string(value: str, budget: _JsonBudget) -> str:
    budget.consume(2)
    for offset in range(0, len(value), JSON_STRING_CHUNK_CHARACTERS):
        chunk = value[offset : offset + JSON_STRING_CHUNK_CHARACTERS]
        encoded = json.dumps(chunk, ensure_ascii=False).encode("utf-8")
        budget.consume(len(encoded) - 2)
    return str.encode(value, "utf-8").decode("utf-8")


def _normalize_json_key(value: Any, budget: _JsonBudget) -> str:
    if isinstance(value, str):
        return _normalize_json_string(value, budget)
    if value is None:
        return _normalize_json_string("null", budget)
    if isinstance(value, bool):
        return _normalize_json_string("true" if value else "false", budget)
    if isinstance(value, int):
        if value.bit_length() > (budget.remaining + 1) * 4:
            budget.consume(budget.remaining + 1)
        return _normalize_json_string(str(value), budget)
    if isinstance(value, float) and math.isfinite(value):
        return _normalize_json_string(json.dumps(value, allow_nan=False), budget)
    raise CodeExecutionError("result must be finite JSON data")


def _normalize_json(value: Any, budget: _JsonBudget, active: set[int]) -> Any:
    if value is None:
        budget.consume(4)
        return None
    if isinstance(value, bool):
        budget.consume(4 if value else 5)
        return value
    if isinstance(value, str):
        return _normalize_json_string(value, budget)
    if isinstance(value, int):
        if value.bit_length() > (budget.remaining + 1) * 4:
            budget.consume(budget.remaining + 1)
        encoded = str(value)
        budget.consume(len(encoded))
        return value
    if isinstance(value, float):
        if not math.isfinite(value):
            raise CodeExecutionError("result must be finite JSON data")
        encoded = json.dumps(value, allow_nan=False)
        budget.consume(len(encoded))
        return value

    identity = id(value)
    if identity in active:
        raise CodeExecutionError("result must be finite JSON data")
    active.add(identity)
    try:
        if isinstance(value, (list, tuple)):
            budget.consume(2)
            normalized = []
            for index, item in enumerate(value):
                if index:
                    budget.consume(1)
                normalized.append(_normalize_json(item, budget, active))
            return normalized
        if isinstance(value, dict):
            budget.consume(2)
            normalized_object: dict[str, Any] = {}
            for index, (key, item) in enumerate(value.items()):
                if index:
                    budget.consume(1)
                normalized_key = _normalize_json_key(key, budget)
                budget.consume(1)
                normalized_object[normalized_key] = _normalize_json(
                    item, budget, active
                )
            return normalized_object
    finally:
        active.remove(identity)
    raise CodeExecutionError("result must be finite JSON data")


def _json_result(value: Any) -> Any:
    try:
        return _normalize_json(value, _JsonBudget(MAX_RESULT_BYTES), set())
    except CodeExecutionError:
        raise
    except (RecursionError, TypeError, ValueError, OverflowError) as error:
        raise CodeExecutionError("result must be finite JSON data") from error


def execute_code(
    source: Any,
    timeout_seconds: Any,
    bpy_module: Any,
    workspace_root: str,
    shutdown_requested: Callable[[], bool],
    watchdog: ExecutionWatchdog,
    *,
    owned_display_pid: int | None = None,
) -> dict[str, Any]:
    if not isinstance(source, str) or not source:
        raise CodeExecutionError("code must be a non-empty string")
    if isinstance(timeout_seconds, bool) or not isinstance(
        timeout_seconds, (int, float)
    ):
        raise CodeExecutionError("timeout_seconds must be a positive finite number")
    timeout = float(timeout_seconds)
    if not math.isfinite(timeout) or timeout <= 0:
        raise CodeExecutionError("timeout_seconds must be a positive finite number")

    monotonic = time.monotonic
    started = monotonic()
    deadline = started + timeout
    previous_trace = sys.gettrace()
    set_trace = sys.settrace
    terminate_process = os._exit
    current_frames = sys._current_frames
    process_scandir = os.scandir
    process_open = open
    kill_process = os.kill
    wait_process = os.waitpid
    sleep = time.sleep
    linux_runtime = sys.platform == "linux"
    kill_signal = signal.SIGKILL
    nohang = os.WNOHANG
    supervisor_pid = os.getppid()
    current_pid = os.getpid()

    def process_subtree() -> tuple[set[int], dict[int, int]]:
        if not linux_runtime:
            return {supervisor_pid, current_pid}, {current_pid: supervisor_pid}
        parents: dict[int, int] = {}
        with process_scandir("/proc") as entries:
            for entry in entries:
                if not entry.name.isdecimal():
                    continue
                try:
                    with process_open(
                        f"/proc/{entry.name}/status",
                        encoding="utf-8",
                        errors="replace",
                    ) as status:
                        for line in status:
                            if line.startswith("PPid:"):
                                parents[int(entry.name)] = int(
                                    line.removeprefix("PPid:").strip()
                                )
                                break
                except (FileNotFoundError, ProcessLookupError):
                    continue
        members = {supervisor_pid}
        while True:
            added = {
                pid
                for pid, parent in parents.items()
                if parent in members and pid not in members
            }
            if not added:
                return members, parents
            members.update(added)

    baseline_processes: set[int] = set()

    def cleanup_background_processes() -> bool:
        found = False
        while True:
            current_processes, parents = process_subtree()
            background = current_processes - baseline_processes - {current_pid}
            if not background:
                return found
            found = True
            for pid in background:
                try:
                    kill_process(pid, kill_signal)
                except ProcessLookupError:
                    pass
            for pid in background:
                if parents.get(pid) != current_pid:
                    continue
                try:
                    wait_process(pid, nohang)
                except ChildProcessError:
                    pass
            sleep(0.01)

    def execution_trace(_frame: Any, _event: str, _argument: Any) -> Any:
        if shutdown_requested():
            terminate_process(0)
        if monotonic() >= deadline:
            terminate_process(WATCHDOG_RESTART_EXIT_CODE)
        return execution_trace

    try:
        watchdog.arm(deadline)
    except WatchdogError as error:
        raise CodeExecutionError("execution watchdog is unavailable") from error
    try:
        set_trace(execution_trace)
        try:
            baseline_processes, _baseline_parents = process_subtree()
        except (OSError, ValueError) as error:
            raise CodeExecutionError(
                "execution process containment is unavailable"
            ) from error
        allowed_processes = {supervisor_pid, current_pid}
        if owned_display_pid is not None:
            if _baseline_parents.get(owned_display_pid) != supervisor_pid:
                raise CodeExecutionError("owned display process is unavailable")
            allowed_processes.add(owned_display_pid)
        if baseline_processes - allowed_processes:
            terminate_process(WATCHDOG_RESTART_EXIT_CODE)
        baseline_processes = allowed_processes
        try:
            compiled = compile(source, "<printable_blender_execute>", "exec")
        except SyntaxError as error:
            if error.lineno is not None:
                raise CodeExecutionError(
                    f"syntax error on line {error.lineno}: {error.msg}"
                ) from error
            raise CodeExecutionError("code could not be compiled") from error
        except (MemoryError, OverflowError, RecursionError, ValueError) as error:
            raise CodeExecutionError("code could not be compiled") from error

        stdout = _BoundedText(MAX_OUTPUT_BYTES)
        stderr = _BoundedText(MAX_OUTPUT_BYTES)
        namespace: dict[str, Any] = {
            "__name__": "__printable_blender_execute__",
            "bpy": bpy_module,
            "workspace_root": workspace_root,
            "result": None,
        }
        with (
            _FileDescriptorCapture(1, stdout, terminate_process),
            _FileDescriptorCapture(2, stderr, terminate_process),
            redirect_stdout(stdout),
            redirect_stderr(stderr),
        ):
            baseline_threads = set(current_frames())
            execution_error: CodeExecutionError | None = None
            normalized_result: Any = None
            try:
                exec(compiled, namespace, namespace)
            except (GeneratorExit, KeyboardInterrupt, SystemExit) as error:
                execution_error = CodeExecutionError(
                    f"code raised {type(error).__name__}"
                )
            except Exception as error:
                message = _safe_exception_message(error)
                detail = f": {message}" if message else ""
                execution_error = CodeExecutionError(
                    f"code raised {type(error).__name__}{detail}"
                )
            except BaseException as error:
                execution_error = CodeExecutionError(
                    f"code raised {type(error).__name__}"
                )

            if execution_error is None:
                try:
                    normalized_result = _json_result(namespace.get("result"))
                except CodeExecutionError as error:
                    execution_error = error
                except BaseException as error:
                    execution_error = CodeExecutionError(
                        f"result raised {type(error).__name__}"
                    )
            if set(current_frames()) - baseline_threads:
                terminate_process(WATCHDOG_RESTART_EXIT_CODE)
            try:
                background_processes = cleanup_background_processes()
            except (OSError, ValueError):
                terminate_process(WATCHDOG_RESTART_EXIT_CODE)
            if background_processes and execution_error is None:
                execution_error = CodeExecutionError(
                    "code left background processes running; wait for them before returning"
                )
            if execution_error is not None:
                raise execution_error
        response = {
            "result": normalized_result,
            "stdout": stdout.value(),
            "stderr": stderr.value(),
            "stdout_truncated": stdout.truncated,
            "stderr_truncated": stderr.truncated,
            "elapsed_ms": round((monotonic() - started) * 1000),
        }
    finally:
        try:
            set_trace(previous_trace)
        finally:
            try:
                watchdog.disarm()
            except WatchdogError:
                terminate_process(WATCHDOG_RESTART_EXIT_CODE)
    return response
