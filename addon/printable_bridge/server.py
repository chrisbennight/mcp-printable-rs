"""Bounded TCP admission that never calls Blender APIs."""

from __future__ import annotations

import logging
import math
import queue
import socket
import threading
import time

from .config import BridgeConfig
from .envelope import EnvelopeError, Request, failure, parse_request
from .execution import DEFAULT_TIMEOUT_SECONDS
from .framing import FrameError, receive_json, send_json
from .handlers import DEFAULT_RENDER_TIMEOUT_SECONDS
from .lifecycle import WorkItem
from .native_view import DEFAULT_CAPTURE_TIMEOUT_SECONDS


LOG = logging.getLogger("printable_bridge.server")


def _request_budget_seconds(
    request_timeout_seconds: float,
    request: Request,
) -> float:
    work_defaults = {
        "capture_native_view": DEFAULT_CAPTURE_TIMEOUT_SECONDS,
        "execute_code": DEFAULT_TIMEOUT_SECONDS,
        "open_project": 600,
        "attach_cad": 600,
        "export_project_blender": 120,
        "job_measure_sequence_bounds": DEFAULT_RENDER_TIMEOUT_SECONDS,
        "job_prepare_mechanical_rotation": DEFAULT_RENDER_TIMEOUT_SECONDS,
        "job_render_frame": DEFAULT_RENDER_TIMEOUT_SECONDS,
        "job_render_product": DEFAULT_RENDER_TIMEOUT_SECONDS,
        "job_render_still": DEFAULT_RENDER_TIMEOUT_SECONDS,
        "job_render_views": DEFAULT_RENDER_TIMEOUT_SECONDS,
        "job_restore_checkpoint": DEFAULT_RENDER_TIMEOUT_SECONDS,
        "job_save_checkpoint": DEFAULT_RENDER_TIMEOUT_SECONDS,
        "render_still": DEFAULT_RENDER_TIMEOUT_SECONDS,
        "render_product": DEFAULT_RENDER_TIMEOUT_SECONDS,
        "render_views": DEFAULT_RENDER_TIMEOUT_SECONDS,
        "render_diagnostic": DEFAULT_RENDER_TIMEOUT_SECONDS,
    }
    if request.command not in work_defaults:
        return request_timeout_seconds
    execution_timeout = request.params.get(
        "timeout_seconds", work_defaults[request.command]
    )
    if (
        isinstance(execution_timeout, bool)
        or not isinstance(execution_timeout, (int, float))
        or not math.isfinite(float(execution_timeout))
        or execution_timeout <= 0
    ):
        return request_timeout_seconds
    return request_timeout_seconds + float(execution_timeout)


class BridgeServer:
    def __init__(
        self,
        config: BridgeConfig,
        work_queue: queue.Queue[WorkItem],
        work_available: threading.Event | None = None,
    ):
        self._config = config
        self._work_queue = work_queue
        self._work_available = work_available
        self._stopping = threading.Event()
        self._listener: socket.socket | None = None
        self._thread: threading.Thread | None = None
        self._connection_slots = threading.BoundedSemaphore(config.max_connections)
        self._connections: set[socket.socket] = set()
        self._connections_lock = threading.Lock()
        self._admission_lock = threading.Lock()

    @property
    def alive(self) -> bool:
        return self._thread is not None and self._thread.is_alive()

    def start(self) -> None:
        if self._thread is not None:
            raise RuntimeError("bridge server already started")
        family = socket.AF_INET6 if ":" in self._config.bind else socket.AF_INET
        listener = socket.socket(family, socket.SOCK_STREAM)
        listener.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
        listener.bind((self._config.bind, self._config.port))
        listener.listen(self._config.max_connections)
        listener.settimeout(0.5)
        self._listener = listener
        self._thread = threading.Thread(
            target=self._accept_loop,
            name="printable-bridge-accept",
            daemon=True,
        )
        self._thread.start()

    def stop_accepting(self) -> None:
        with self._admission_lock:
            self._stopping.set()
        listener = self._listener
        if listener is not None:
            try:
                listener.close()
            except OSError:
                pass

    def wait_stopped(self, grace_seconds: float) -> bool:
        thread = self._thread
        if thread is not None:
            thread.join(timeout=grace_seconds)
        return thread is None or not thread.is_alive()

    def close_connections(self) -> None:
        with self._connections_lock:
            remaining = list(self._connections)
        for connection in remaining:
            try:
                connection.shutdown(socket.SHUT_RDWR)
            except OSError:
                pass
            try:
                connection.close()
            except OSError:
                pass

    def stop(self, grace_seconds: float) -> None:
        deadline = time.monotonic() + grace_seconds
        self.stop_accepting()
        self.close_connections()
        self.wait_stopped(max(0.0, deadline - time.monotonic()))

    def _accept_loop(self) -> None:
        listener = self._listener
        if listener is None:
            return
        try:
            while not self._stopping.is_set():
                try:
                    connection, _address = listener.accept()
                except socket.timeout:
                    continue
                except OSError:
                    if self._stopping.is_set():
                        return
                    raise
                with self._admission_lock:
                    if self._stopping.is_set():
                        connection.close()
                        return
                    if not self._connection_slots.acquire(blocking=False):
                        connection.close()
                        continue
                    connection.settimeout(self._config.request_timeout_seconds)
                    with self._connections_lock:
                        self._connections.add(connection)
                threading.Thread(
                    target=self._serve_connection,
                    args=(connection,),
                    name="printable-bridge-connection",
                    daemon=True,
                ).start()
        except Exception as error:
            LOG.error("bridge accept thread stopped: %s", type(error).__name__)

    def _serve_connection(self, connection: socket.socket) -> None:
        request_id = ""
        frame_deadline = time.monotonic() + self._config.request_timeout_seconds
        try:
            raw = receive_json(
                connection,
                self._config.max_frame_bytes,
                deadline=frame_deadline,
            )
            if isinstance(raw, dict) and isinstance(raw.get("id"), str):
                request_id = raw["id"]
            request = parse_request(raw)
            request_id = request.request_id
            admission_deadline = (
                time.monotonic() + self._config.request_timeout_seconds
            )
            run_budget_seconds = _request_budget_seconds(
                self._config.request_timeout_seconds, request
            )
            with self._admission_lock:
                if self._stopping.is_set():
                    item = None
                    response = failure(request_id, "Blender bridge is shutting down")
                else:
                    item = WorkItem(
                        request=request,
                        deadline=admission_deadline,
                        run_budget_seconds=run_budget_seconds,
                    )
                    try:
                        self._work_queue.put_nowait(item)
                    except queue.Full:
                        item = None
                        response = failure(request_id, "Blender bridge queue is full")
                    else:
                        if self._work_available is not None:
                            self._work_available.set()
            if item is not None:
                response = item.await_response()
            connection.settimeout(self._config.request_timeout_seconds)
            send_json(connection, response, self._config.max_frame_bytes)
        except (EnvelopeError, FrameError) as error:
            self._send_error(connection, request_id, str(error))
        except (OSError, TimeoutError):
            pass
        except Exception as error:
            LOG.error("connection handler failed: %s", type(error).__name__)
            self._send_error(connection, request_id, "internal bridge error")
        finally:
            with self._connections_lock:
                self._connections.discard(connection)
            try:
                connection.close()
            except OSError:
                pass
            self._connection_slots.release()

    def _send_error(
        self, connection: socket.socket, request_id: str, message: str
    ) -> None:
        try:
            send_json(
                connection,
                failure(request_id, message),
                self._config.max_frame_bytes,
            )
        except (FrameError, OSError, TimeoutError):
            pass
