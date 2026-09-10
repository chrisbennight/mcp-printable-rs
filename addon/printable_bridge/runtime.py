"""Persistent main-thread queue pump and health markers."""

from __future__ import annotations

import json
import logging
import os
from pathlib import Path
import queue
import signal
import threading
import time
from collections.abc import Callable
from typing import Any

from . import VERSION
from .config import BridgeConfig, owned_display_pid
from .envelope import failure, success
from .handlers import BlenderHandlers, HandlerError, HandlerStartupError
from .lifecycle import WorkItem
from .server import BridgeServer
from .state import SceneStateError
from .watchdog import ExecutionWatchdog, WatchdogError
from .workspace import WorkspaceError, enforce_process_file_size_limit


LOG = logging.getLogger("printable_bridge.runtime")


class BridgeRuntime:
    def __init__(self, config: BridgeConfig, execution_watchdog: ExecutionWatchdog):
        self._config = config
        self._execution_watchdog = execution_watchdog
        self._work_queue: queue.Queue[WorkItem] = queue.Queue(
            maxsize=config.queue_capacity
        )
        self._shutdown = threading.Event()
        self._pump_wakeup = threading.Event()
        self._shutdown_requested = False
        self._shutdown_deadline: float | None = None
        self._server = BridgeServer(config, self._work_queue, self._pump_wakeup)
        self._handlers: BlenderHandlers | None = None
        self._current: WorkItem | None = None
        self._health_thread: threading.Thread | None = None
        self._health_failed = threading.Event()
        self._ready_path = config.state_dir / "ready.json"
        self._live_path = config.state_dir / "live.json"
        self._last_pump = time.monotonic()
        self._ui_mode = False
        self._ui_exit: Callable[[int], None] | None = None

    def start_ui(self, timers: Any, on_exit: Callable[[int], None]) -> None:
        """Serve one queued command per persistent Blender timer callback."""
        self._ui_mode = True
        self._ui_exit = on_exit
        try:
            self._start()
            timers.register(self._ui_tick, first_interval=0.01, persistent=True)
        except Exception:
            self._finish(1)
            raise

    def _ui_tick(self) -> float | None:
        exit_code = 0
        try:
            if not self._shutdown_requested:
                self._pump_once()
            if not self._shutdown_requested:
                return 0.01
        except Exception as error:
            LOG.error("Blender event loop failed: %s", type(error).__name__)
            exit_code = 1
        try:
            exit_code = self._finish(exit_code)
        except Exception as error:
            LOG.error("Blender event loop cleanup failed: %s", type(error).__name__)
            exit_code = 1
        if self._ui_exit is not None:
            self._ui_exit(exit_code)
        return None

    def _start(self) -> None:
        self._install_signal_handlers()
        enforce_process_file_size_limit()
        self._prepare_directories()
        self._handlers = BlenderHandlers(
            self._config,
            lambda: self._shutdown_requested,
            execution_watchdog=self._execution_watchdog,
            owned_display_pid=owned_display_pid(),
        )
        self._handlers.install_state_observers()
        self._server.start()
        self._write_marker(
            self._ready_path,
            {
                "pid": os.getpid(),
                "bind": self._config.bind,
                "port": self._config.port,
                "addon_version": VERSION,
            },
        )
        self._last_pump = time.monotonic()
        self._health_thread = threading.Thread(
            target=self._report_health,
            name="printable-bridge-health",
            daemon=True,
        )
        self._health_thread.start()

    def run(self) -> int:
        exit_code = 0
        try:
            self._start()
            self._pump()
        except (HandlerStartupError, WorkspaceError) as error:
            LOG.error("bridge startup failed: %s", error)
            exit_code = 1
        except Exception as error:
            LOG.error(
                "bridge runtime failed: %s",
                type(error).__name__,
            )
            exit_code = 1
        finally:
            exit_code = self._finish(exit_code)
        return exit_code

    def _finish(self, exit_code: int) -> int:
        shutdown_deadline = self._shutdown_deadline or (
            time.monotonic() + self._config.shutdown_timeout_seconds
        )
        self._begin_shutdown()
        self._server.stop_accepting()
        self._fail_queued()
        self._server.close_connections()
        if self._health_thread is not None:
            self._health_thread.join(timeout=self._remaining(shutdown_deadline))
        if not self._server.wait_stopped(self._remaining(shutdown_deadline)):
            LOG.error("bridge accept thread did not stop within its shutdown budget")
            exit_code = 1
        if self._handlers is not None:
            self._handlers.close()
        try:
            self._execution_watchdog.close()
        except WatchdogError as error:
            LOG.error("execution watchdog cleanup failed: %s", error)
            exit_code = 1
        self._remove_marker(self._ready_path)
        self._remove_marker(self._live_path)
        return exit_code

    def _handle_signal(self, *_args: Any) -> None:
        if self._shutdown_deadline is None:
            self._shutdown_deadline = (
                time.monotonic() + self._config.shutdown_timeout_seconds
            )
        self._shutdown_requested = True
        self._shutdown.set()
        self._pump_wakeup.set()

    def _begin_shutdown(self) -> None:
        self._shutdown_requested = True
        self._shutdown.set()
        current = self._current
        if current is not None:
            current.shutdown()

    def _pump(self) -> None:
        while not self._shutdown_requested:
            if not self._pump_once():
                self._pump_wakeup.wait(self._config.heartbeat_seconds)
                self._pump_wakeup.clear()

    def _pump_once(self) -> bool:
        self._last_pump = time.monotonic()
        handlers = self._handlers
        if handlers is None:
            raise RuntimeError("bridge handlers are not initialized")
        if not self._server.alive:
            raise RuntimeError("bridge accept thread stopped")
        if self._health_failed.is_set():
            raise RuntimeError("bridge health reporter stopped")
        try:
            item = self._work_queue.get_nowait()
        except queue.Empty:
            return False
        self._current = item
        try:
            if self._shutdown_requested:
                item.shutdown()
                return True
            if not item.start():
                return True
            if self._shutdown_requested:
                item.shutdown()
                return True
            try:
                result = handlers.dispatch(item.request.command, item.request.params)
                if isinstance(result, dict):
                    result = {**result, "scene_state": handlers.scene_state}
                response = success(item.request.request_id, result)
            except SceneStateError as error:
                response = failure(item.request.request_id, str(error))
                response["error_code"] = error.code
                response["scene_state"] = handlers.scene_state
            except (HandlerError, WorkspaceError) as error:
                response = failure(item.request.request_id, str(error))
                response["scene_state"] = handlers.scene_state
            except Exception as error:
                LOG.error(
                    "Blender command %s failed: %s",
                    item.request.command,
                    type(error).__name__,
                )
                response = failure(item.request.request_id, "Blender command failed")
                response["scene_state"] = handlers.scene_state
            if self._shutdown_requested:
                item.shutdown()
            else:
                item.complete(response)
        finally:
            self._last_pump = time.monotonic()
            self._current = None
            self._work_queue.task_done()
        return True

    def _report_health(self) -> None:
        try:
            while not self._shutdown.is_set():
                self._write_marker(
                    self._live_path,
                    self._health_snapshot(),
                )
                self._shutdown.wait(self._config.heartbeat_seconds)
        except OSError as error:
            LOG.error("bridge health reporter failed: %s", type(error).__name__)
            self._health_failed.set()

    def _health_snapshot(self) -> dict[str, Any]:
        observed = time.monotonic()
        current = self._current
        return {
            "pid": os.getpid(),
            "monotonic_seconds": observed,
            "server_alive": self._server.alive,
            "main_thread_alive": (
                observed < current.deadline
                if current is not None
                else not self._ui_mode
                or observed - self._last_pump <= max(5.0, self._config.heartbeat_seconds * 4)
            ),
            "state": "busy" if current is not None else "idle",
            "command": current.request.command if current is not None else None,
        }

    def _fail_queued(self) -> None:
        while True:
            try:
                item = self._work_queue.get_nowait()
            except queue.Empty:
                return
            item.shutdown()
            self._work_queue.task_done()

    def _install_signal_handlers(self) -> None:
        signal.signal(signal.SIGTERM, self._handle_signal)
        signal.signal(signal.SIGINT, self._handle_signal)
        signal.signal(signal.SIGXFSZ, signal.SIG_IGN)

    def _prepare_directories(self) -> None:
        self._config.state_dir.mkdir(parents=True, exist_ok=True)
        self._config.workspace_root.mkdir(parents=True, exist_ok=True)
        self._remove_marker(self._ready_path)
        self._remove_marker(self._live_path)

    @staticmethod
    def _write_marker(path: Path, value: dict[str, Any]) -> None:
        temporary = path.with_suffix(".tmp")
        temporary.write_text(
            json.dumps(value, separators=(",", ":")), encoding="utf-8"
        )
        temporary.replace(path)

    @staticmethod
    def _remove_marker(path: Path) -> None:
        try:
            path.unlink()
        except FileNotFoundError:
            pass

    @staticmethod
    def _remaining(deadline: float) -> float:
        return max(0.0, deadline - time.monotonic())
