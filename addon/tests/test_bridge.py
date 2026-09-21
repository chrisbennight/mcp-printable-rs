from __future__ import annotations

import json
import hashlib
import math
from dataclasses import replace
import os
from pathlib import Path
import queue
import resource
import signal
import socket
import struct
import subprocess
import sys
import tempfile
import threading
import time
from types import SimpleNamespace
from typing import Any
import unittest
from unittest.mock import Mock, call, patch
import uuid

import integration_smoke
from printable_bridge import VERSION, bl_info
from printable_bridge.config import BridgeConfig, ConfigError
from printable_bridge.envelope import EnvelopeError, failure, parse_request, success
from printable_bridge.framing import (
    FrameError,
    FrameTimeout,
    encode_json,
    receive_json,
    send_json as framing_send_json,
)
from printable_bridge.handlers import (
    MAX_DIAGNOSTIC_ATTRIBUTE_VALUES,
    MAX_DIAGNOSTIC_EDGES,
    MAX_DIAGNOSTIC_FACES,
    MAX_DIAGNOSTIC_LOOPS,
    MAX_DIAGNOSTIC_VERTICES,
    MAX_PRODUCT_VERTICES,
    BlenderHandlers,
    HandlerError,
    HandlerStartupError,
)
from printable_bridge.healthcheck import main as healthcheck
from printable_bridge.lifecycle import RequestPhase, WorkItem
from printable_bridge.runtime import BridgeRuntime
from printable_bridge.server import BridgeServer, _request_budget_seconds
from printable_bridge.supervisor import BlenderSupervisor
from printable_bridge.watchdog import (
    ARM_OPERATION,
    DISARM_OPERATION,
    WATCHDOG_MESSAGE,
    NoopExecutionWatchdog,
)
from printable_bridge.workspace import (
    MAX_ARTIFACT_BYTES,
    SecureWorkspace,
    WorkspaceError,
    enforce_process_file_size_limit,
)


def config(root: Path, port: int) -> BridgeConfig:
    (root / "state").mkdir(parents=True, exist_ok=True)
    (root / "workspace").mkdir(parents=True, exist_ok=True)
    return BridgeConfig(
        bind="127.0.0.1",
        port=port,
        request_timeout_seconds=1.0,
        shutdown_timeout_seconds=1.0,
        queue_capacity=2,
        max_connections=2,
        max_frame_bytes=1024 * 1024,
        heartbeat_seconds=0.05,
        state_dir=root / "state",
        workspace_root=root / "workspace",
        render_device="CPU",
        enable_test_commands=False,
    )


def request(command: str = "get_scene_info"):
    return parse_request(
        {"id": str(uuid.uuid4()), "command": command, "params": {}}
    )


class FakeVector:
    def __init__(self, values: tuple[float, float, float]):
        self.values = values

    def __getitem__(self, index: int) -> float:
        return self.values[index]

    @property
    def x(self) -> float:
        return self.values[0]

    @property
    def y(self) -> float:
        return self.values[1]

    @property
    def z(self) -> float:
        return self.values[2]

    def __add__(self, other: FakeVector) -> FakeVector:
        return FakeVector(tuple(a + b for a, b in zip(self.values, other.values)))

    def __sub__(self, other: FakeVector) -> FakeVector:
        return FakeVector(tuple(a - b for a, b in zip(self.values, other.values)))

    def __mul__(self, scale: float) -> FakeVector:
        return FakeVector(tuple(value * scale for value in self.values))

    @property
    def length(self) -> float:
        return sum(value * value for value in self.values) ** 0.5

    def to_track_quat(self, _track: str, _up: str) -> SimpleNamespace:
        return SimpleNamespace(to_euler=lambda: (0.0, 0.0, 0.0))


class IntegrationSmokeTests(unittest.TestCase):
    def test_overdue_wait_accepts_each_visible_terminal_channel(self) -> None:
        for message in (
            "command bridge_test_wait exceeded its execution timeout",
            "bridge closed before returning a response",
        ):
            with self.subTest(message=message), patch.object(
                integration_smoke, "wait_until_ready"
            ), patch.object(
                integration_smoke,
                "command",
                side_effect=RuntimeError(message),
            ):
                integration_smoke.overdue_wait("127.0.0.1", 9876)


class ConfigTests(unittest.TestCase):
    def test_render_worker_requires_background_role_configuration(self) -> None:
        self.assertEqual(BridgeConfig.from_env({"PRINTABLE_BLENDER_ROLE": "render_worker"}).role,
                         "render_worker")
        for values in ({"PRINTABLE_BLENDER_ROLE": "unknown"},
                       {"PRINTABLE_BLENDER_ROLE": "render_worker", "PRINTABLE_BLENDER_MODE": "ui"}):
            with self.subTest(values=values), self.assertRaises(ConfigError):
                BridgeConfig.from_env(values)

    def test_defaults_are_safe_for_local_use(self) -> None:
        value = BridgeConfig.from_env({})
        self.assertEqual(value.bind, "127.0.0.1")
        self.assertEqual(value.port, 9876)
        self.assertEqual(value.render_device, "CPU")

    def test_invalid_values_fail_visibly(self) -> None:
        for env in (
            {"PRINTABLE_BLENDER_BIND": "localhost"},
            {"BLENDER_PORT": "0"},
            {"PRINTABLE_BLENDER_REQUEST_TIMEOUT_SECONDS": "0"},
            {"PRINTABLE_BLENDER_REQUEST_TIMEOUT_SECONDS": "nan"},
            {"PRINTABLE_BLENDER_REQUEST_TIMEOUT_SECONDS": "3600.1"},
            {"PRINTABLE_BLENDER_SHUTDOWN_TIMEOUT_SECONDS": "0.09"},
            {"PRINTABLE_BLENDER_SHUTDOWN_TIMEOUT_SECONDS": "300.1"},
            {"PRINTABLE_BLENDER_HEARTBEAT_SECONDS": "0.049"},
            {"PRINTABLE_BLENDER_HEARTBEAT_SECONDS": "60.1"},
            {"PRINTABLE_BLENDER_HEARTBEAT_SECONDS": "inf"},
            {"PRINTABLE_BLENDER_RENDER_DEVICE": "CUDA"},
            {"PRINTABLE_BLENDER_WORKSPACE_ROOT": "relative"},
        ):
            with self.subTest(env=env), self.assertRaises(ConfigError):
                BridgeConfig.from_env(env)


class LauncherScriptTests(unittest.TestCase):
    def test_local_launcher_supplies_repository_writable_paths(self) -> None:
        repository_root = Path(__file__).resolve().parents[2]
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            capture = root / "capture.json"
            blender = root / "fake-blender"
            blender.write_text(
                """#!/usr/bin/env python3
import json
import os
from pathlib import Path
import sys

Path(os.environ["CAPTURE_PATH"]).write_text(json.dumps({
    "state_dir": os.environ["PRINTABLE_BLENDER_STATE_DIR"],
    "workspace_root": os.environ["PRINTABLE_BLENDER_WORKSPACE_ROOT"],
    "arguments": sys.argv[1:],
    "pid": os.getpid(),
    "process_group": os.getpgrp(),
}), encoding="utf-8")
""",
                encoding="utf-8",
            )
            blender.chmod(0o755)
            environment = os.environ.copy()
            environment.pop("PRINTABLE_BLENDER_STATE_DIR", None)
            environment.pop("PRINTABLE_BLENDER_WORKSPACE_ROOT", None)
            environment.update(
                {
                    "BLENDER_BIN": str(blender),
                    "CAPTURE_PATH": str(capture),
                }
            )

            completed = subprocess.run(
                [str(repository_root / "scripts" / "run-headless-blender")],
                cwd=root,
                env=environment,
                check=False,
            )

            self.assertEqual(completed.returncode, 0)
            observed = json.loads(capture.read_text(encoding="utf-8"))
            development_root = repository_root / ".dev" / "printable-blender"
            self.assertEqual(observed["state_dir"], str(development_root / "state"))
            self.assertEqual(
                observed["workspace_root"], str(development_root / "workspace")
            )
            self.assertEqual(
                observed["arguments"][-2:],
                ["--python", str(repository_root / "addon" / "launcher.py")],
            )
            self.assertEqual(observed["process_group"], observed["pid"])

    def test_local_launcher_restarts_after_execution_watchdog_exit(self) -> None:
        repository_root = Path(__file__).resolve().parents[2]
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            counter = root / "counter"
            descendant_pid = root / "descendant.pid"
            blender = root / "fake-blender"
            blender.write_text(
                """#!/usr/bin/env python3
import os
from pathlib import Path
import subprocess
import sys

counter = Path(os.environ["COUNTER_PATH"])
attempt = int(counter.read_text(encoding="utf-8")) + 1 if counter.exists() else 1
counter.write_text(str(attempt), encoding="utf-8")
if attempt == 1:
    descendant = subprocess.Popen(
        [sys.executable, "-c", "while True: pass"],
        start_new_session=sys.platform.startswith("linux"),
    )
    Path(os.environ["DESCENDANT_PID_PATH"]).write_text(
        str(descendant.pid), encoding="utf-8"
    )
    raise SystemExit(75)
""",
                encoding="utf-8",
            )
            blender.chmod(0o755)
            environment = os.environ.copy()
            environment.update(
                {
                    "BLENDER_BIN": str(blender),
                    "COUNTER_PATH": str(counter),
                    "DESCENDANT_PID_PATH": str(descendant_pid),
                    "PRINTABLE_BLENDER_STATE_DIR": str(root / "state"),
                    "PRINTABLE_BLENDER_WORKSPACE_ROOT": str(root / "workspace"),
                }
            )

            completed = subprocess.run(
                [str(repository_root / "scripts" / "run-headless-blender")],
                cwd=root,
                env=environment,
                check=False,
            )

            self.assertEqual(completed.returncode, 0)
            self.assertEqual(counter.read_text(encoding="utf-8"), "2")
            pid = int(descendant_pid.read_text(encoding="utf-8"))
            deadline = time.monotonic() + 2.0
            while time.monotonic() < deadline:
                try:
                    os.kill(pid, 0)
                except ProcessLookupError:
                    break
                time.sleep(0.01)
            else:
                os.kill(pid, signal.SIGKILL)
                self.fail("watchdog restart left a caller descendant running")

    def test_supervisor_deadline_is_independent_of_the_blender_gil(self) -> None:
        repository_root = Path(__file__).resolve().parents[2]
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            counter = root / "counter"
            blender = root / "fake-blender"
            blender.write_text(
                """#!/usr/bin/env python3
import os
from pathlib import Path
import sys
import time

from printable_bridge.watchdog import SupervisorWatchdog

counter = Path(os.environ["COUNTER_PATH"])
attempt = int(counter.read_text(encoding="utf-8")) + 1 if counter.exists() else 1
counter.write_text(str(attempt), encoding="utf-8")
watchdog = SupervisorWatchdog.from_env()
if attempt == 1:
    watchdog.arm(time.monotonic() + 0.1)
    os._exit = lambda _code: None
    sys.settrace(None)
    sys.setswitchinterval(60.0)
    while True:
        pass
watchdog.close()
""",
                encoding="utf-8",
            )
            blender.chmod(0o755)
            environment = os.environ.copy()
            environment.update(
                {
                    "BLENDER_BIN": str(blender),
                    "COUNTER_PATH": str(counter),
                    "PRINTABLE_BLENDER_STATE_DIR": str(root / "state"),
                    "PRINTABLE_BLENDER_WORKSPACE_ROOT": str(root / "workspace"),
                }
            )

            started = time.monotonic()
            completed = subprocess.run(
                [str(repository_root / "scripts" / "run-headless-blender")],
                cwd=root,
                env=environment,
                check=False,
                timeout=5,
            )

            self.assertEqual(completed.returncode, 0)
            self.assertLess(time.monotonic() - started, 3.0)
            self.assertEqual(counter.read_text(encoding="utf-8"), "2")

    def test_supervisor_shutdown_grace_kills_an_unresponsive_blender(self) -> None:
        repository_root = Path(__file__).resolve().parents[2]
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            child_pid = root / "child-pid"
            blender = root / "fake-blender"
            blender.write_text(
                """#!/usr/bin/env python3
import os
from pathlib import Path
import signal

Path(os.environ["CHILD_PID_PATH"]).write_text(str(os.getpid()), encoding="utf-8")
signal.signal(signal.SIGTERM, lambda _signum, _frame: None)
while True:
    pass
""",
                encoding="utf-8",
            )
            blender.chmod(0o755)
            environment = os.environ.copy()
            environment.update(
                {
                    "BLENDER_BIN": str(blender),
                    "CHILD_PID_PATH": str(child_pid),
                    "PRINTABLE_BLENDER_SHUTDOWN_TIMEOUT_SECONDS": "0.1",
                    "PRINTABLE_BLENDER_STATE_DIR": str(root / "state"),
                    "PRINTABLE_BLENDER_WORKSPACE_ROOT": str(root / "workspace"),
                }
            )
            supervisor = subprocess.Popen(
                [str(repository_root / "scripts" / "run-headless-blender")],
                cwd=root,
                env=environment,
            )
            try:
                deadline = time.monotonic() + 2.0
                while not child_pid.exists() and time.monotonic() < deadline:
                    time.sleep(0.01)
                self.assertTrue(child_pid.exists())

                started = time.monotonic()
                supervisor.terminate()
                self.assertEqual(supervisor.wait(timeout=2), 0)
                self.assertLess(time.monotonic() - started, 1.0)
                with self.assertRaises(ProcessLookupError):
                    os.kill(int(child_pid.read_text(encoding="utf-8")), 0)
            finally:
                if supervisor.poll() is None:
                    supervisor.kill()
                    supervisor.wait()
                if child_pid.exists():
                    try:
                        os.kill(
                            int(child_pid.read_text(encoding="utf-8")),
                            signal.SIGKILL,
                        )
                    except ProcessLookupError:
                        pass


class SupervisorWatchdogProtocolTests(unittest.TestCase):
    def test_timely_disarm_queued_before_the_deadline_is_accepted(self) -> None:
        deadline = 100.0
        buffer = bytearray(
            WATCHDOG_MESSAGE.pack(ARM_OPERATION, deadline)
            + WATCHDOG_MESSAGE.pack(DISARM_OPERATION, deadline - 0.1)
        )

        observed_deadline, expired = BlenderSupervisor._consume_watchdog_messages(
            buffer, None
        )

        self.assertIsNone(observed_deadline)
        self.assertFalse(expired)
        self.assertEqual(buffer, bytearray())

    def test_disarm_at_the_deadline_cannot_erase_it(self) -> None:
        deadline = 100.0
        buffer = bytearray(
            WATCHDOG_MESSAGE.pack(ARM_OPERATION, deadline)
            + WATCHDOG_MESSAGE.pack(DISARM_OPERATION, deadline)
        )

        observed_deadline, expired = BlenderSupervisor._consume_watchdog_messages(
            buffer, None
        )

        self.assertEqual(observed_deadline, deadline)
        self.assertTrue(expired)
        self.assertEqual(buffer, bytearray())


class SupervisorProcessBoundaryTests(unittest.TestCase):
    def test_forced_cleanup_kills_the_blender_process_group(self) -> None:
        supervisor = BlenderSupervisor([], Path("/tmp/state"), 1.0)
        child = Mock(pid=12345)
        supervisor._child = child

        with patch("printable_bridge.supervisor.os.killpg") as kill_group:
            supervisor._kill_child()

        kill_group.assert_called_once_with(12345, signal.SIGKILL)
        child.wait.assert_called_once_with()

    def test_supervisor_reaps_adopted_children_while_blender_runs(self) -> None:
        supervisor = BlenderSupervisor([], Path("/tmp/state"), 1.0)
        supervisor._child = Mock(pid=12345)

        with (
            patch(
                "printable_bridge.supervisor._direct_child_pids",
                return_value=[12345, 23456],
            ),
            patch("printable_bridge.supervisor.os.waitpid") as waitpid,
        ):
            supervisor._reap_adopted_children()

        waitpid.assert_called_once_with(23456, os.WNOHANG)

    def test_disappearing_adopted_child_does_not_stop_blender(self) -> None:
        supervisor = BlenderSupervisor([], Path("/tmp/state"), 1.0)
        supervisor._child = Mock(pid=12345)

        with (
            patch(
                "printable_bridge.supervisor._direct_child_pids",
                return_value=[12345, 23456],
            ),
            patch(
                "printable_bridge.supervisor.os.waitpid",
                side_effect=ProcessLookupError,
            ),
        ):
            supervisor._reap_adopted_children()


class EnvelopeTests(unittest.TestCase):
    def test_request_contract(self) -> None:
        request_id = str(uuid.uuid4())
        parsed = parse_request(
            {"id": request_id, "command": "clear_scene", "params": {"x": 1}}
        )
        self.assertEqual(parsed.request_id, request_id)
        self.assertEqual(parsed.command, "clear_scene")
        self.assertEqual(parsed.params, {"x": 1})

    def test_malformed_requests_are_rejected(self) -> None:
        good_id = str(uuid.uuid4())
        for value in (
            None,
            {},
            {"id": "not-uuid", "command": "ok", "params": {}},
            {"id": good_id, "command": "Bad.Command", "params": {}},
            {"id": good_id, "command": "ok", "params": []},
        ):
            with self.subTest(value=value), self.assertRaises(EnvelopeError):
                parse_request(value)

    def test_responses_always_include_version_and_result_presence(self) -> None:
        self.assertEqual(VERSION, "0.5.0")
        self.assertEqual(bl_info["version"], (0, 5, 0))
        ok = success("id", None)
        self.assertIn("result", ok)
        self.assertTrue(ok["addon_version"])
        uuid.UUID(ok["bridge_instance_id"])
        error = failure("id", "failed")
        self.assertEqual(error["status"], "error")
        self.assertTrue(error["addon_version"])
        self.assertEqual(error["bridge_instance_id"], ok["bridge_instance_id"])


class FramingTests(unittest.TestCase):
    def test_partial_socket_reads_reassemble_one_frame(self) -> None:
        left, right = socket.socketpair()
        frame = encode_json({"hello": "world"}, 1024)

        def sender() -> None:
            for byte in frame:
                left.sendall(bytes([byte]))
            left.close()

        thread = threading.Thread(target=sender)
        thread.start()
        self.assertEqual(receive_json(right, 1024), {"hello": "world"})
        thread.join()
        right.close()

    def test_declared_oversize_is_rejected_before_payload_read(self) -> None:
        left, right = socket.socketpair()
        left.sendall(struct.pack(">I", 2048))
        with self.assertRaises(FrameError):
            receive_json(right, 1024)
        left.close()
        right.close()

    def test_invalid_json_and_nonfinite_output_are_rejected(self) -> None:
        left, right = socket.socketpair()
        left.sendall(struct.pack(">I", 1) + b"{")
        with self.assertRaises(FrameError):
            receive_json(right, 1024)
        with self.assertRaises(FrameError):
            encode_json({"value": float("nan")}, 1024)
        left.close()
        right.close()

    def test_absolute_deadline_expires_despite_individually_timely_chunks(self) -> None:
        left, right = socket.socketpair()
        left.sendall(struct.pack(">I", 3) + b"{")

        def slow_sender() -> None:
            time.sleep(0.04)
            left.sendall(b"}")
            time.sleep(0.04)
            try:
                left.sendall(b" ")
            except BrokenPipeError:
                pass

        sender = threading.Thread(target=slow_sender)
        sender.start()
        with self.assertRaises(FrameTimeout):
            receive_json(right, 1024, deadline=time.monotonic() + 0.06)
        right.close()
        sender.join()
        left.close()


class LifecycleTests(unittest.TestCase):
    def test_completion_selects_the_only_terminal_response(self) -> None:
        item = WorkItem.with_budget(request(), 1.0)
        self.assertTrue(item.start())
        expected = success(item.request.request_id, {"ok": True})
        self.assertTrue(item.complete(expected))
        self.assertFalse(item.timeout())
        self.assertFalse(item.shutdown())
        self.assertEqual(item.await_response(), expected)
        self.assertEqual(item.phase, RequestPhase.COMPLETED)

    def test_timeout_wins_over_late_completion(self) -> None:
        item = WorkItem.with_budget(request("clear_scene"), 0.01)
        self.assertTrue(item.start())
        response = item.await_response()
        self.assertEqual(response["status"], "error")
        self.assertIn("timeout", response["error"])
        self.assertFalse(item.complete(success(item.request.request_id, {})))
        self.assertEqual(item.phase, RequestPhase.TIMED_OUT)

    def test_completion_after_deadline_selects_timeout_before_waiter(self) -> None:
        item = WorkItem(
            request=request("clear_scene"),
            deadline=10.0,
            run_budget_seconds=1.0,
        )
        self.assertTrue(item.start(now=9.0))

        completed = item.complete(
            success(item.request.request_id, {"late": True}), now=10.0
        )

        self.assertFalse(completed)
        self.assertEqual(item.phase, RequestPhase.TIMED_OUT)
        self.assertIn("timeout", item.await_response()["error"])

    def test_expired_queue_item_never_starts(self) -> None:
        item = WorkItem(
            request=request(),
            deadline=time.monotonic() - 1,
            run_budget_seconds=3600.0,
        )
        self.assertFalse(item.start())
        self.assertEqual(item.phase, RequestPhase.TIMED_OUT)

    def test_start_receives_a_fresh_run_budget_after_queue_admission(self) -> None:
        item = WorkItem(
            request=request("render_still"),
            deadline=10.0,
            run_budget_seconds=3601.0,
        )

        self.assertTrue(item.start(now=9.5))

        self.assertEqual(item.deadline, 3610.5)
        self.assertTrue(
            item.complete(
                success(item.request.request_id, {"rendered": True}),
                now=3610.0,
            )
        )

    def test_shutdown_releases_running_waiter(self) -> None:
        item = WorkItem.with_budget(request(), 10.0)
        self.assertTrue(item.start())
        self.assertTrue(item.shutdown())
        self.assertEqual(item.await_response()["status"], "error")
        self.assertEqual(item.phase, RequestPhase.SHUTDOWN)

    def test_racing_terminal_selectors_choose_exactly_one(self) -> None:
        item = WorkItem.with_budget(request(), 1.0)
        self.assertTrue(item.start())
        barrier = threading.Barrier(3)
        results: list[bool] = []

        def complete() -> None:
            barrier.wait()
            results.append(item.complete(success(item.request.request_id, {})))

        def timeout() -> None:
            barrier.wait()
            results.append(item.timeout())

        first = threading.Thread(target=complete)
        second = threading.Thread(target=timeout)
        first.start()
        second.start()
        barrier.wait()
        first.join()
        second.join()
        self.assertEqual(results.count(True), 1)
        self.assertEqual(results.count(False), 1)
        self.assertIn(item.phase, {RequestPhase.COMPLETED, RequestPhase.TIMED_OUT})


class WorkspaceTests(unittest.TestCase):
    def test_process_file_limit_is_enforced_before_blender_writes(self) -> None:
        unlimited = resource.RLIM_INFINITY
        for current, expected in (
            (unlimited, MAX_ARTIFACT_BYTES),
            (MAX_ARTIFACT_BYTES * 2, MAX_ARTIFACT_BYTES),
            (MAX_ARTIFACT_BYTES // 2, MAX_ARTIFACT_BYTES // 2),
        ):
            with self.subTest(current=current), patch(
                "printable_bridge.workspace.resource.getrlimit",
                return_value=(current, unlimited),
            ), patch(
                "printable_bridge.workspace.resource.setrlimit"
            ) as set_limit:
                enforce_process_file_size_limit()

            set_limit.assert_called_once_with(
                resource.RLIMIT_FSIZE,
                (expected, unlimited),
            )

    def test_process_file_limit_stops_staging_growth(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            output = Path(temporary) / "oversized.stl"
            completed = subprocess.run(
                [
                    sys.executable,
                    "-c",
                    """
import signal
import sys
from pathlib import Path
import errno
import printable_bridge.workspace as workspace

workspace.MAX_ARTIFACT_BYTES = 1024
signal.signal(signal.SIGXFSZ, signal.SIG_IGN)
workspace.enforce_process_file_size_limit()
try:
    Path(sys.argv[1]).write_bytes(b"x" * 2048)
except OSError as error:
    if error.errno != errno.EFBIG:
        raise
else:
    raise SystemExit("file-size limit allowed an oversized staging file")
if Path(sys.argv[1]).stat().st_size > workspace.MAX_ARTIFACT_BYTES:
    raise SystemExit("staging file exceeded the enforced limit")
""",
                    str(output),
                ],
                check=False,
                env=os.environ.copy(),
            )

            self.assertEqual(completed.returncode, 0)

    def test_input_is_snapshotted_from_a_descriptor_and_symlinks_are_refused(
        self,
    ) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            base = Path(temporary)
            root = base / "workspace"
            root.mkdir()
            (root / "safe.stl").write_text("solid safe", encoding="utf-8")
            workspace = SecureWorkspace(root, base / "state" / "staging")
            request_path = workspace.validate("safe.stl", ".stl")
            with workspace.stage_input(request_path) as staged:
                (root / "safe.stl").write_text("changed", encoding="utf-8")
                self.assertEqual(staged.read_text(encoding="utf-8"), "solid safe")
            outside = base / "outside"
            outside.mkdir()
            (outside / "secret.stl").write_text("secret", encoding="utf-8")
            (root / "linked.stl").symlink_to(outside / "secret.stl")
            with self.assertRaises(WorkspaceError):
                with workspace.stage_input(
                    workspace.validate("linked.stl", ".stl")
                ):
                    pass
            workspace.close()

    def test_escape_is_rejected_before_directories_are_created(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            base = Path(temporary)
            root = base / "workspace"
            root.mkdir()
            workspace = SecureWorkspace(root, base / "state" / "staging")
            outside = base / "uncreated" / "nested"
            for raw in ("../uncreated/nested/output.stl", "a" * 1025):
                with self.subTest(raw=raw), self.assertRaises(WorkspaceError):
                    workspace.validate(raw, ".stl")
            self.assertFalse(outside.exists())
            workspace.close()

    def test_reserved_namespace_requires_the_internal_path_capability(self) -> None:
        public_path = "renders/output.png"
        reserved_path = ".printable/jobs/output.png"

        with self.assertRaisesRegex(WorkspaceError, "reserved"):
            SecureWorkspace.validate(reserved_path, ".png")
        with self.assertRaises(WorkspaceError):
            SecureWorkspace.validate_reserved(public_path, ".png")
        request = SecureWorkspace.validate_reserved(reserved_path, ".png")
        self.assertEqual(request.relative, reserved_path)

    def test_output_commit_replaces_final_symlink_without_following_it(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            base = Path(temporary)
            root = base / "workspace"
            root.mkdir()
            workspace = SecureWorkspace(root, base / "state" / "staging")
            outside = base / "outside.stl"
            outside.write_bytes(b"outside")
            (root / "artifact.stl").symlink_to(outside)
            request_path = workspace.validate("artifact.stl", ".stl")
            with workspace.stage_output(request_path) as output:
                output.path.write_bytes(b"inside")
                output.commit()
            self.assertEqual(outside.read_bytes(), b"outside")
            self.assertFalse((root / "artifact.stl").is_symlink())
            self.assertEqual((root / "artifact.stl").read_bytes(), b"inside")
            workspace.close()

    def test_output_parent_symlink_cannot_redirect_commit(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            base = Path(temporary)
            root = base / "workspace"
            root.mkdir()
            outside = base / "outside"
            outside.mkdir()
            (root / "escape").symlink_to(outside, target_is_directory=True)
            workspace = SecureWorkspace(root, base / "state" / "staging")
            request_path = workspace.validate("escape/artifact.stl", ".stl")
            with self.assertRaises(WorkspaceError):
                with workspace.stage_output(request_path) as output:
                    output.path.write_bytes(b"inside")
                    output.commit()
            self.assertFalse((outside / "artifact.stl").exists())
            workspace.close()

    def test_batch_commit_retains_prior_outputs_when_a_later_commit_fails(
        self,
    ) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            base = Path(temporary)
            root = base / "workspace"
            root.mkdir()
            workspace = SecureWorkspace(root, base / "state" / "staging")
            first_request = workspace.validate("batch/first.png", ".png")
            second_request = workspace.validate("batch/second.png", ".png")
            original_commit = workspace._commit_output
            attempts = 0

            def fail_second_commit(
                stage_path: Path,
                parent_fd: int,
                leaf: str,
                *,
                rollback_on_failure: bool,
                create_only: bool = False,
                check_budget=lambda: None,
            ) -> tuple[int, int]:
                nonlocal attempts
                attempts += 1
                if attempts == 2:
                    raise WorkspaceError("injected batch commit failure")
                return original_commit(
                    stage_path,
                    parent_fd,
                    leaf,
                    rollback_on_failure=rollback_on_failure,
                    create_only=create_only,
                    check_budget=check_budget,
                )

            with (
                workspace.stage_output(first_request) as first,
                workspace.stage_output(second_request) as second,
                patch.object(
                    workspace,
                    "_commit_output",
                    side_effect=fail_second_commit,
                ),
                self.assertRaisesRegex(
                    WorkspaceError, "injected batch commit failure"
                ),
            ):
                first.path.write_bytes(b"first")
                second.path.write_bytes(b"second")
                workspace.commit_batch([first, second])

            self.assertEqual((root / "batch" / "first.png").read_bytes(), b"first")
            self.assertFalse((root / "batch" / "second.png").exists())
            workspace.close()


class RenderDeviceTests(unittest.TestCase):
    def test_graphics_backend_reports_the_runtime_driver(self) -> None:
        platform = SimpleNamespace(
            backend_type_get=lambda: "OPENGL",
            device_type_get=lambda: "NVIDIA",
            renderer_get=lambda: "GeForce RTX 4060 Ti",
            vendor_get=lambda: "NVIDIA Corporation",
            version_get=lambda: "4.6.0",
        )

        with patch.dict(sys.modules, {"gpu": SimpleNamespace(platform=platform)}):
            observed = BlenderHandlers._graphics_backend()

        self.assertEqual(
            observed,
            {
                "backend": "OPENGL",
                "device_type": "NVIDIA",
                "renderer": "GeForce RTX 4060 Ti",
                "vendor": "NVIDIA Corporation",
                "version": "4.6.0",
            },
        )

    def test_cpu_configuration_is_applied_to_cycles(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            scene = SimpleNamespace(cycles=SimpleNamespace(device=None))
            bpy = SimpleNamespace(data=SimpleNamespace(scenes=[scene]))
            handlers = BlenderHandlers(config(root, 9876), lambda: False, bpy)
            self.assertEqual(scene.cycles.device, "CPU")
            handlers.close()

    def test_optix_configuration_enables_only_optix_devices(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            value = replace(config(root, 9876), render_device="OPTIX")
            scene = SimpleNamespace(cycles=SimpleNamespace(device=None))
            cpu = SimpleNamespace(name="AMD Ryzen", type="CPU", use=True)
            gpu = SimpleNamespace(name="RTX 4060 Ti", type="OPTIX", use=False)
            preferences = SimpleNamespace(
                compute_device_type=None,
                devices=[cpu, gpu],
                get_devices=lambda: None,
            )
            bpy = SimpleNamespace(
                app=SimpleNamespace(version_string="5.2.0", background=True),
                data=SimpleNamespace(scenes=[scene]),
                context=SimpleNamespace(
                    preferences=SimpleNamespace(
                        addons={"cycles": SimpleNamespace(preferences=preferences)}
                    )
                ),
            )
            handlers = BlenderHandlers(value, lambda: False, bpy)
            self.assertEqual(preferences.compute_device_type, "OPTIX")
            self.assertFalse(cpu.use)
            self.assertTrue(gpu.use)
            self.assertEqual(scene.cycles.device, "GPU")
            self.assertEqual(
                handlers.dispatch("bridge_status", {})["cycles_devices"],
                [
                    {"name": "AMD Ryzen", "type": "CPU", "enabled": False},
                    {
                        "name": "RTX 4060 Ti",
                        "type": "OPTIX",
                        "enabled": True,
                    },
                ],
            )
            handlers.close()

    def test_optix_without_a_device_fails_startup(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            value = replace(config(root, 9876), render_device="OPTIX")
            preferences = SimpleNamespace(
                compute_device_type=None,
                devices=[],
                get_devices=lambda: None,
            )
            bpy = SimpleNamespace(
                data=SimpleNamespace(
                    scenes=[SimpleNamespace(cycles=SimpleNamespace(device=None))]
                ),
                context=SimpleNamespace(
                    preferences=SimpleNamespace(
                        addons={"cycles": SimpleNamespace(preferences=preferences)}
                    )
                ),
            )
            with self.assertRaisesRegex(RuntimeError, "no compatible device"):
                BlenderHandlers(value, lambda: False, bpy)

    def test_eevee_selection_negotiates_the_runtime_identifier(self) -> None:
        class Render:
            engine = "ORIGINAL"

            def __setattr__(self, name: str, value: object) -> None:
                if name == "engine" and value == "BLENDER_EEVEE":
                    raise TypeError("unsupported enum")
                super().__setattr__(name, value)

        render = Render()

        BlenderHandlers._select_eevee_engine(render)

        self.assertEqual(render.engine, "BLENDER_EEVEE_NEXT")


class HandlerValidationTests(unittest.TestCase):
    @staticmethod
    def _mesh(name: str) -> SimpleNamespace:
        return SimpleNamespace(
            name=name,
            type="MESH",
            location=(0.0, 0.0, 0.0),
            rotation_euler=(0.0, 0.0, 0.0),
            scale=(1.0, 1.0, 1.0),
            dimensions=(2.0, 2.0, 2.0),
            data=SimpleNamespace(vertices=[None] * 8, polygons=[None] * 6),
        )

    @staticmethod
    def _rotation_fixture(
        root: Path,
    ) -> tuple[SimpleNamespace, SimpleNamespace, SimpleNamespace]:
        class Matrix:
            def __init__(self, label: str):
                self.label = label

            def copy(self) -> Matrix:
                return Matrix(self.label)

            def inverted(self) -> Matrix:
                return Matrix(f"inverse({self.label})")

        target = SimpleNamespace(
            name="Leaf",
            parent=None,
            children=[],
            animation_data=None,
            constraints=[],
            rigid_body=None,
            rigid_body_constraint=None,
            matrix_world=Matrix("leaf-world"),
            matrix_parent_inverse=Matrix("leaf-parent-inverse"),
        )
        controller = SimpleNamespace(
            name="HingePivot",
            matrix_world=Matrix("pivot-world"),
            animation_data=None,
            animation_data_clear=Mock(),
        )
        fcurves = [
            SimpleNamespace(
                data_path="rotation_axis_angle",
                array_index=index,
                keyframe_points=[],
                modifiers=[],
                update=Mock(),
            )
            for index in range(4)
        ]
        channelbag = SimpleNamespace(fcurves=fcurves)
        strip = SimpleNamespace(channelbag=Mock(return_value=channelbag))
        action = SimpleNamespace(
            layers=[SimpleNamespace(strips=[strip])],
            users=1,
        )
        action_slot = object()
        animation_data = SimpleNamespace(action=action, action_slot=action_slot)

        def insert_keyframe(*, data_path: str, frame: int) -> bool:
            controller.animation_data = animation_data
            for fcurve in fcurves:
                fcurve.keyframe_points.append(
                    SimpleNamespace(
                        co=[
                            float(frame),
                            float(controller.rotation_axis_angle[fcurve.array_index]),
                        ],
                        interpolation="BEZIER",
                    )
                )
            return data_path == "rotation_axis_angle"

        controller.keyframe_insert = Mock(side_effect=insert_keyframe)
        objects = SimpleNamespace(
            get=lambda name: target if name == "Leaf" else None,
            new=Mock(return_value=controller),
            remove=Mock(),
        )
        scene = SimpleNamespace(
            cycles=SimpleNamespace(device=None),
            frame_current=17,
            frame_subframe=0.375,
            frame_set=Mock(),
            collection=SimpleNamespace(objects=SimpleNamespace(link=Mock())),
        )
        edit = SimpleNamespace(keyframe_new_interpolation_type="BEZIER")
        bpy = SimpleNamespace(
            data=SimpleNamespace(
                scenes=[scene],
                objects=objects,
                actions=SimpleNamespace(remove=Mock()),
            ),
            context=SimpleNamespace(
                scene=scene,
                preferences=SimpleNamespace(edit=edit),
                view_layer=SimpleNamespace(update=Mock()),
            ),
        )
        return bpy, target, controller

    def test_rotation_authoring_preserves_world_transform_and_keys_axis_angle(
        self,
    ) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            bpy, target, controller = self._rotation_fixture(root)
            handlers = BlenderHandlers(config(root, 9876), lambda: False, bpy)

            result = handlers.dispatch(
                "animate_rotation",
                {
                    "objects": ["Leaf"],
                    "controller_name": "HingePivot",
                    "pivot": [1.0, 2.0, 3.0],
                    "axis": [0.0, 0.0, 2.0],
                    "angle_degrees": 90.0,
                },
            )

            bpy.data.objects.new.assert_called_once_with("HingePivot", None)
            bpy.context.scene.collection.objects.link.assert_called_once_with(controller)
            bpy.context.view_layer.update.assert_called_once_with()
            self.assertIs(target.parent, controller)
            self.assertEqual(target.matrix_world.label, "leaf-world")
            self.assertEqual(
                target.matrix_parent_inverse.label, "inverse(pivot-world)"
            )
            self.assertEqual(controller.location, (1.0, 2.0, 3.0))
            self.assertEqual(controller.rotation_mode, "AXIS_ANGLE")
            self.assertAlmostEqual(controller.rotation_axis_angle[0], math.pi / 2)
            self.assertEqual(controller.rotation_axis_angle[1:], (0.0, 0.0, 1.0))
            self.assertEqual(
                controller.keyframe_insert.call_args_list,
                [
                    call(data_path="rotation_axis_angle", frame=1),
                    call(data_path="rotation_axis_angle", frame=250),
                ],
            )
            self.assertTrue(
                all(
                    point.interpolation == "LINEAR"
                    for fcurve in controller.animation_data.action.layers[0]
                    .strips[0]
                    .channelbag(controller.animation_data.action_slot)
                    .fcurves
                    for point in fcurve.keyframe_points
                )
            )
            self.assertEqual(
                bpy.context.preferences.edit.keyframe_new_interpolation_type,
                "BEZIER",
            )
            bpy.context.scene.frame_set.assert_called_once_with(17, subframe=0.375)
            self.assertEqual(
                result,
                {
                    "controller": "HingePivot",
                    "objects": ["Leaf"],
                    "pivot": [1.0, 2.0, 3.0],
                    "axis": [0.0, 0.0, 1.0],
                    "angle_degrees": 90.0,
                    "frame_start": 1,
                    "frame_end": 250,
                    "interpolation": "LINEAR",
                },
            )
            handlers.close()

    def test_product_presentation_exposure_and_light_controls(self) -> None:
        for exposure, intensity in ((-10.0, 0.0), (1.25, 0.5), (10.0, 10.0)):
            normalized = BlenderHandlers._validate_product_presentation(
                {"profile": "studio_neutral", "exposure_stops": exposure,
                 "light_intensity_scale": intensity}, ["Body"]
            )
            self.assertEqual(normalized["exposure_stops"], exposure)
            self.assertEqual(normalized["light_intensity_scale"], intensity)
        for field, values in (
            ("exposure_stops", (-10.1, 10.1, float("nan"), float("inf"), True, "1")),
            ("light_intensity_scale", (-0.1, 10.1, float("nan"), float("inf"), True, "1")),
        ):
            for value in values:
                with self.subTest(field=field, value=value), self.assertRaises(HandlerError):
                    BlenderHandlers._validate_product_presentation(
                        {"profile": "engineering", field: value}, ["Body"]
                    )

    def test_product_presentation_validates_mappings_and_applies_profile_defaults(
        self,
    ) -> None:
        normalized = BlenderHandlers._validate_product_presentation(
            {"profile": "engineering"},
            ["Body", "Insert"],
        )
        self.assertEqual(
            normalized,
            {
                "profile": "engineering",
                "exposure_stops": 0.0,
                "light_intensity_scale": 1.0,
                "view": {
                    "azimuth_degrees": 45.0,
                    "elevation_degrees": 25.0,
                },
                "surface_shading": "preserve",
                "materials": [],
            },
        )

        for presentation, message in (
            (
                {
                    "profile": "studio_neutral",
                    "materials": [
                        {
                            "objects": ["Body"],
                            "base_color_srgb": [0.2, 0.3, 0.4],
                            "metallic": 0.0,
                            "roughness": 0.5,
                        },
                        {
                            "objects": ["Body"],
                            "base_color_srgb": [0.4, 0.3, 0.2],
                            "metallic": 0.0,
                            "roughness": 0.5,
                        },
                    ],
                },
                "assigned more than once",
            ),
            (
                {
                    "profile": "studio_dark",
                    "materials": [
                        {
                            "objects": ["Missing"],
                            "base_color_srgb": [0.2, 0.3, 0.4],
                            "metallic": 0.0,
                            "roughness": 0.5,
                        }
                    ],
                },
                "not selected",
            ),
            (
                {
                    "profile": "studio_neutral",
                    "view": {"elevation_degrees": -1.0},
                },
                "ground cannot occlude",
            ),
        ):
            with self.subTest(message=message), self.assertRaisesRegex(
                HandlerError, message
            ):
                BlenderHandlers._validate_product_presentation(
                    presentation,
                    ["Body", "Insert"],
                )

    def test_product_fallback_fills_empty_material_slots_without_hiding_real_materials(
        self,
    ) -> None:
        fallback = object()
        source = object()
        cases = (
            ([], (False, True), [fallback], [0, 0]),
            ([None, None], (False, True), [fallback, fallback], [0, 1]),
            ([source, None], (True, True), [source, fallback], [0, 1]),
            ([source], (True, False), [source], [0, 1]),
        )
        for slots, expected_outcome, expected_slots, expected_indices in cases:
            polygons = [
                SimpleNamespace(material_index=0),
                SimpleNamespace(material_index=1),
            ]
            mesh = SimpleNamespace(materials=["underlying"], polygons=polygons)
            with self.subTest(slots=slots):
                outcome = BlenderHandlers._assign_product_materials(
                    mesh, list(slots), fallback
                )
                self.assertEqual(outcome, expected_outcome)
                self.assertEqual(mesh.materials, expected_slots)
                self.assertEqual(
                    [polygon.material_index for polygon in polygons],
                    expected_indices,
                )

        object_material = object()
        effective = BlenderHandlers._effective_product_materials(
            SimpleNamespace(
                material_slots=[
                    SimpleNamespace(material=None),
                    SimpleNamespace(material=object_material),
                ]
            ),
            SimpleNamespace(materials=["underlying"]),
        )
        self.assertEqual(effective, [None, object_material])

    def test_render_views_accepts_product_presentation_as_one_atomic_batch(
        self,
    ) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            scene = SimpleNamespace(cycles=SimpleNamespace(device=None))
            bpy = SimpleNamespace(
                data=SimpleNamespace(scenes=[scene]),
                context=SimpleNamespace(scene=scene),
            )
            handlers = BlenderHandlers(config(root, 9876), lambda: False, bpy)
            expected = {
                "views": [{"path": "renders/front.png"}],
                "presentation": {"profile": "studio_dark", "views": [{}]},
            }
            with (
                patch.object(
                    handlers,
                    "_renderable_product_names",
                    return_value=["Body"],
                ),
                patch.object(
                    handlers,
                    "_render_product_view_batch",
                    return_value=expected,
                ) as render_batch,
            ):
                result = handlers.dispatch(
                    "render_views",
                    {
                        "views": [
                            {
                                "path": "renders/front.png",
                                "label": "FRONT",
                                "direction": [0.0, -1.0, 0.0],
                            }
                        ],
                        "presentation": {"profile": "studio_dark"},
                        "width": 64,
                        "height": 48,
                    },
                )

            self.assertEqual(result, expected)
            render_batch.assert_called_once()
            self.assertEqual(
                render_batch.call_args.args[2]["profile"], "studio_dark"
            )
            handlers.close()

    def test_presented_view_batch_retains_outputs_when_later_promotion_fails(
        self,
    ) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            render_pre = [object()]
            scene = SimpleNamespace(cycles=SimpleNamespace(device=None))
            bpy = SimpleNamespace(
                app=SimpleNamespace(
                    handlers=SimpleNamespace(render_pre=render_pre)
                ),
                data=SimpleNamespace(scenes=[scene]),
            )
            watchdog = Mock()
            handlers = BlenderHandlers(
                config(root, 9876), lambda: False, bpy, watchdog
            )
            requests = [
                handlers._workspace.validate("renders/first.png", ".png"),
                handlers._workspace.validate("renders/second.png", ".png"),
            ]
            views = [
                (requests[0], "FIRST", (1.0, 0.0, 0.0)),
                (requests[1], "SECOND", (0.0, 1.0, 0.0)),
            ]
            corners = [
                FakeVector((-1.0, -1.0, 0.0)),
                FakeVector((1.0, 1.0, 1.0)),
            ]
            original_commit = handlers._workspace._commit_output
            attempts = 0

            def fail_second_commit(
                stage_path: Path,
                parent_fd: int,
                leaf: str,
                *,
                rollback_on_failure: bool,
                create_only: bool = False,
                check_budget=lambda: None,
            ) -> tuple[int, int]:
                nonlocal attempts
                attempts += 1
                if attempts == 2:
                    raise WorkspaceError("injected product batch commit failure")
                return original_commit(
                    stage_path,
                    parent_fd,
                    leaf,
                    rollback_on_failure=rollback_on_failure,
                    create_only=create_only,
                    check_budget=check_budget,
                )

            def render_to_stage(
                output_path: Path, *_args: object
            ) -> dict[str, object]:
                output_path.write_bytes(b"product-png")
                return {
                    "size_bytes": len(b"product-png"),
                    "engine": "BLENDER_EEVEE_NEXT",
                    "presentation": {},
                }

            with (
                patch.object(
                    handlers,
                    "_product_geometry_preflight",
                    return_value={},
                ),
                patch.object(
                    handlers,
                    "_render_bounds",
                    return_value=(
                        corners,
                        FakeVector((0.0, 0.0, 0.5)),
                        3.0,
                    ),
                ),
                patch.object(
                    handlers,
                    "_product_source_signature",
                    return_value=("unchanged",),
                ) as source_signature,
                patch.object(
                    handlers,
                    "_product_datablock_counts",
                    return_value=(1, 1, 1),
                ),
                patch.object(
                    handlers,
                    "_render_product_staged",
                    side_effect=render_to_stage,
                ),
                patch.object(
                    handlers._workspace,
                    "_commit_output",
                    side_effect=fail_second_commit,
                ),
                self.assertRaisesRegex(
                    WorkspaceError, "injected product batch commit failure"
                ),
            ):
                handlers._render_product_view_batch(
                    views,
                    ["Body"],
                    {"profile": "engineering"},
                    64,
                    48,
                    5.0,
                    "BLENDER_EEVEE_NEXT",
                    None,
                    None,
                )

            self.assertEqual((handlers._config.workspace_root / "renders" / "first.png").read_bytes(), b"product-png")
            self.assertFalse((handlers._config.workspace_root / "renders" / "second.png").exists())
            self.assertEqual(
                list(handlers._workspace._staging_root.iterdir()), []
            )
            self.assertEqual(source_signature.call_count, 2)
            handlers.close()

    def test_presented_view_batch_attests_each_clean_source_verified_view(
        self,
    ) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            scene = SimpleNamespace(cycles=SimpleNamespace(device=None))
            bpy = SimpleNamespace(
                app=SimpleNamespace(
                    handlers=SimpleNamespace(render_pre=[])
                ),
                data=SimpleNamespace(scenes=[scene]),
            )
            handlers = BlenderHandlers(config(root, 9876), lambda: False, bpy)
            request = handlers._workspace.validate("renders/front.png", ".png")
            corners = [
                FakeVector((-1.0, -1.0, 0.0)),
                FakeVector((1.0, 1.0, 1.0)),
            ]

            def render_to_stage(
                output_path: Path, *_args: object
            ) -> dict[str, object]:
                output_path.write_bytes(b"product-png")
                return {
                    "size_bytes": len(b"product-png"),
                    "engine": "BLENDER_EEVEE_NEXT",
                    "presentation": {"profile": "engineering"},
                }

            with (
                patch.object(
                    handlers,
                    "_product_geometry_preflight",
                    return_value={},
                ),
                patch.object(
                    handlers,
                    "_render_bounds",
                    return_value=(
                        corners,
                        FakeVector((0.0, 0.0, 0.5)),
                        3.0,
                    ),
                ),
                patch.object(
                    handlers,
                    "_product_source_signature",
                    return_value=("unchanged",),
                ),
                patch.object(
                    handlers,
                    "_product_datablock_counts",
                    return_value=(1, 1, 1),
                ),
                patch.object(
                    handlers,
                    "_render_product_staged",
                    side_effect=render_to_stage,
                ),
            ):
                result = handlers._render_product_view_batch(
                    [(request, "FRONT", (0.0, -1.0, 0.0))],
                    ["Body"],
                    {"profile": "engineering"},
                    64,
                    48,
                    5.0,
                    "BLENDER_EEVEE_NEXT",
                    None,
                    None,
                )

            self.assertEqual(
                result["presentation"]["views"],
                [
                    {
                        "profile": "engineering",
                        "source_state_verified": True,
                        "cleanup_verified": True,
                    }
                ],
            )
            self.assertTrue(
                (root / "workspace" / "renders" / "front.png").is_file()
            )
            handlers.close()

    def test_product_framing_bounds_must_be_consistent_and_contain_geometry(
        self,
    ) -> None:
        source = {
            "minimum": [-1.0, -1.0, -1.0],
            "maximum": [1.0, 1.0, 1.0],
        }
        enclosing = {
            "minimum": [-2.0, -2.0, -2.0],
            "maximum": [2.0, 2.0, 2.0],
            "dimensions": [4.0, 4.0, 4.0],
            "center": [0.0, 0.0, 0.0],
            "diagonal": math.sqrt(48.0),
            "coordinate_space": "world",
            "unit": "blender_unit",
        }
        with patch.dict(
            sys.modules,
            {"mathutils": SimpleNamespace(Vector=FakeVector)},
        ):
            _corners, center, diagonal, normalized = (
                BlenderHandlers._product_framing_bounds(
                    enclosing,
                    source,
                    require_contains_geometry=True,
                )
            )
            self.assertEqual(center.values, (0.0, 0.0, 0.0))
            self.assertEqual(diagonal, math.sqrt(48.0))
            self.assertEqual(normalized, enclosing)

            too_small = {
                **enclosing,
                "maximum": [0.5, 2.0, 2.0],
                "dimensions": [2.5, 4.0, 4.0],
                "center": [-0.75, 0.0, 0.0],
                "diagonal": math.hypot(2.5, 4.0, 4.0),
            }
            with self.assertRaisesRegex(HandlerError, "do not contain"):
                BlenderHandlers._product_framing_bounds(
                    too_small,
                    source,
                    require_contains_geometry=True,
                )
            _corners, _center, _diagonal, normalized = (
                BlenderHandlers._product_framing_bounds(
                    too_small,
                    source,
                    require_contains_geometry=False,
                )
            )
            self.assertEqual(normalized, too_small)

    def test_product_render_does_not_promote_when_cleanup_fails(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            watchdog = Mock()
            scene = SimpleNamespace(cycles=SimpleNamespace(device=None))
            original_render_handler = object()
            render_pre = [original_render_handler]
            bpy = SimpleNamespace(
                app=SimpleNamespace(
                    handlers=SimpleNamespace(render_pre=render_pre)
                ),
                data=SimpleNamespace(scenes=[scene]),
            )
            handlers = BlenderHandlers(
                config(root, 9876), lambda: False, bpy, watchdog
            )
            corners = [
                FakeVector((-2.0, -1.0, 0.0)),
                FakeVector((2.0, 1.0, 1.0)),
            ]

            def render_to_stage(output_path: Path, *_args: object) -> dict[str, object]:
                output_path.write_bytes(b"product-png")
                return {
                    "size_bytes": len(b"product-png"),
                    "engine": "BLENDER_EEVEE_NEXT",
                    "presentation": {},
                }

            with (
                patch.object(
                    handlers,
                    "_product_geometry_preflight",
                    return_value={
                        "instances": 1,
                        "unique_evaluated_meshes": 1,
                        "vertices": 8,
                        "edges": 12,
                        "faces": 6,
                        "loops": 24,
                        "attribute_values": 0,
                        "material_slots": 0,
                    },
                ),
                patch.object(
                    handlers,
                    "_render_bounds",
                    return_value=(corners, FakeVector((0.0, 0.0, 0.5)), 5.0),
                ),
                patch.object(
                    handlers,
                    "_product_source_signature",
                    return_value=("unchanged",),
                ),
                patch.object(
                    handlers,
                    "_product_datablock_counts",
                    return_value=(1, 1, 1),
                ),
                patch.object(
                    handlers,
                    "_build_and_render_product_presentation",
                    side_effect=render_to_stage,
                ),
                patch.object(
                    handlers,
                    "_cleanup_product_presentation",
                    side_effect=RuntimeError("cleanup failed"),
                ),
                self.assertRaisesRegex(RuntimeError, "cleanup failed"),
            ):
                handlers.dispatch(
                    "render_product",
                    {
                        "path": "renders/product.png",
                        "objects": ["Body"],
                        "presentation": {"profile": "studio_neutral"},
                        "width": 64,
                        "height": 48,
                    },
                )

            self.assertFalse(
                (root / "workspace" / "renders" / "product.png").exists()
            )
            watchdog.arm.assert_called_once()
            watchdog.disarm.assert_called_once()
            self.assertEqual(render_pre, [original_render_handler])
            handlers.close()

    def test_product_geometry_budget_preflights_instance_multiplication(
        self,
    ) -> None:
        class Sized:
            def __init__(self, length: int) -> None:
                self.length = length

            def __len__(self) -> int:
                return self.length

            def __iter__(self):
                return iter(())

        mesh = SimpleNamespace(
            vertices=Sized(MAX_PRODUCT_VERTICES // 2 + 1),
            edges=Sized(12),
            polygons=Sized(6),
            loops=Sized(24),
            attributes=[],
            materials=[],
        )
        geometry = SimpleNamespace(
            name="Body",
            type="MESH",
            hide_render=False,
            visible_camera=True,
            data=mesh,
        )
        depsgraph = SimpleNamespace(
            object_instances=[
                SimpleNamespace(object=geometry, show_self=True),
                SimpleNamespace(object=geometry, show_self=True),
            ]
        )
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            class Scenes(list):
                pass

            scenes = Scenes([SimpleNamespace(cycles=SimpleNamespace(device=None))])
            scenes.new = Mock()
            copied_meshes = SimpleNamespace(new_from_object=Mock())
            bpy = SimpleNamespace(
                data=SimpleNamespace(
                    scenes=scenes,
                    meshes=copied_meshes,
                ),
                context=SimpleNamespace(
                    evaluated_depsgraph_get=Mock(return_value=depsgraph)
                ),
            )
            handlers = BlenderHandlers(config(root, 9876), lambda: False, bpy)

            with self.assertRaisesRegex(
                HandlerError, "evaluated vertices"
            ):
                handlers._product_geometry_preflight({"Body"})

            scenes.new.assert_not_called()
            copied_meshes.new_from_object.assert_not_called()
            handlers.close()

    def test_rotation_authoring_reports_values_stored_in_the_action(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            bpy, _target, controller = self._rotation_fixture(root)
            insert_keyframe = controller.keyframe_insert.side_effect

            def coerce_ending_angle(*, data_path: str, frame: int) -> bool:
                inserted = insert_keyframe(data_path=data_path, frame=frame)
                if frame == 250:
                    controller.animation_data.action.layers[0].strips[0].channelbag(
                        controller.animation_data.action_slot
                    ).fcurves[0].keyframe_points[-1].co[1] = math.radians(45.0)
                return inserted

            controller.keyframe_insert.side_effect = coerce_ending_angle
            handlers = BlenderHandlers(config(root, 9876), lambda: False, bpy)

            result = handlers.dispatch(
                "animate_rotation",
                {
                    "objects": ["Leaf"],
                    "controller_name": "HingePivot",
                    "pivot": [1.0, 2.0, 3.0],
                    "axis": [0.0, 0.0, 1.0],
                    "angle_degrees": 90.0,
                },
            )

            self.assertAlmostEqual(result["angle_degrees"], 45.0)
            handlers.close()

    def test_mechanical_job_exports_complete_scene_and_authors_owned_motion(
        self,
    ) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            bpy, moving, controller = self._rotation_fixture(root)
            mesh_data = lambda: SimpleNamespace(
                animation_data=None, shape_keys=None
            )
            for obj in [moving]:
                obj.type = "MESH"
                obj.data = mesh_data()
                obj.modifiers = []
                obj.instance_type = "NONE"
                obj.hide_render = False
                obj.visible_camera = True
                obj.users_collection = []
                obj.select_set = Mock()
            fixed = SimpleNamespace(
                name="Base",
                type="MESH",
                parent=None,
                children=[],
                animation_data=None,
                constraints=[],
                rigid_body=None,
                rigid_body_constraint=None,
                modifiers=[],
                data=mesh_data(),
                instance_type="NONE",
                hide_render=False,
                visible_camera=True,
                users_collection=[],
                select_set=Mock(),
            )
            mechanical_objects = [fixed, moving]
            for obj in mechanical_objects:
                obj.selected = False
                obj.select_set = Mock(
                    side_effect=lambda selected, obj=obj: setattr(
                        obj, "selected", selected
                    )
                )
            bpy.data.objects.get = lambda name: {
                "Base": fixed,
                "Leaf": moving,
            }.get(name)

            def new_controller(name: str, _data: object) -> SimpleNamespace:
                controller.name = name
                return controller

            bpy.data.objects.new.side_effect = new_controller
            bpy.context.scene.objects = [fixed, moving]
            visible_collection = bpy.context.scene.collection
            visible_collection.hide_render = False
            visible_collection.children = []
            bpy.context.view_layer.objects = SimpleNamespace(active=None)
            bpy.context.view_layer.use = True
            render_collection = SimpleNamespace(
                hide_render=False,
                animation_data=None,
                objects=[moving],
            )
            fixed_render_collection = SimpleNamespace(
                hide_render=False,
                animation_data=None,
                objects=[fixed],
            )
            fixed_render_layer = SimpleNamespace(
                collection=fixed_render_collection,
                children=[],
                exclude=False,
                holdout=False,
                indirect_only=False,
            )
            render_layer = SimpleNamespace(
                collection=render_collection,
                children=[fixed_render_layer],
                exclude=False,
                holdout=False,
                indirect_only=False,
            )
            bpy.context.view_layer.layer_collection = render_layer
            def deselect_all(**_kwargs: Any) -> None:
                for obj in mechanical_objects:
                    obj.selected = False

            bpy.ops = SimpleNamespace(
                object=SimpleNamespace(select_all=Mock(side_effect=deselect_all)),
                wm=SimpleNamespace(stl_export=Mock()),
            )

            exported_groups: list[list[str]] = []

            def export_stl(**kwargs: Any) -> set[str]:
                selected = [obj.name for obj in mechanical_objects if obj.selected]
                exported_groups.append(selected)
                Path(kwargs["filepath"]).write_bytes(
                    f"solid {','.join(selected)}\nendsolid\n".encode()
                )
                return {"FINISHED"}

            bpy.ops.wm.stl_export.side_effect = export_stl
            watchdog = Mock()
            handlers = BlenderHandlers(
                config(root, 9876), lambda: False, bpy, watchdog
            )
            params = {
                "fixed_objects": ["Base"],
                "moving_objects": ["Leaf"],
                "controller_name": "MechanicalPivot",
                "pivot": [0.0, 0.0, 0.0],
                "axis": [0.0, 0.0, 2.0],
                "angle_degrees": 90.0,
                "frame_start": 1,
                "frame_end": 3,
                "fixed_path": ".printable/jobs/test/analysis/fixed.stl",
                "moving_path": ".printable/jobs/test/analysis/moving.stl",
                "max_output_bytes": 1024,
                "timeout_seconds": 5.0,
            }
            for target, attribute, unsafe_value in [
                (fixed, "type", "CURVE"),
                (fixed, "parent", controller),
                (fixed, "children", [controller]),
                (fixed, "animation_data", object()),
                (fixed, "constraints", [object()]),
                (fixed, "rigid_body", object()),
                (fixed, "rigid_body_constraint", object()),
                (fixed, "modifiers", [object()]),
                (fixed.data, "animation_data", object()),
                (fixed.data, "shape_keys", object()),
                (fixed, "instance_type", "COLLECTION"),
                (fixed, "hide_render", True),
                (fixed, "visible_camera", False),
            ]:
                with self.subTest(attribute=attribute):
                    safe_value = getattr(target, attribute)
                    setattr(target, attribute, unsafe_value)
                    with self.assertRaises(HandlerError):
                        handlers.dispatch("job_prepare_mechanical_rotation", params)
                    setattr(target, attribute, safe_value)
            bpy.ops.wm.stl_export.assert_not_called()
            bpy.data.objects.new.assert_not_called()

            for target, attribute, unsafe_value in [
                (render_collection, "hide_render", True),
                (render_collection, "animation_data", object()),
                (render_layer, "exclude", True),
                (render_layer, "holdout", True),
                (render_layer, "indirect_only", True),
                (fixed_render_collection, "animation_data", object()),
                (fixed_render_layer, "holdout", True),
                (bpy.context.view_layer, "use", False),
            ]:
                with self.subTest(render_layer_attribute=attribute):
                    safe_value = getattr(target, attribute)
                    setattr(target, attribute, unsafe_value)
                    with self.assertRaisesRegex(HandlerError, "active render view layer"):
                        handlers.dispatch("job_prepare_mechanical_rotation", params)
                    setattr(target, attribute, safe_value)
            fixed_render_collection.objects = []
            with self.assertRaisesRegex(HandlerError, "active render view layer"):
                handlers.dispatch("job_prepare_mechanical_rotation", params)
            fixed_render_collection.objects = [fixed]
            bpy.ops.wm.stl_export.assert_not_called()
            bpy.data.objects.new.assert_not_called()

            for extra, message in [
                (
                    SimpleNamespace(
                        name="CollectionInstance",
                        type="EMPTY",
                        instance_type="COLLECTION",
                    ),
                    "must not contain instancers",
                ),
                (
                    SimpleNamespace(
                        name="RenderableCurve",
                        type="CURVE",
                        instance_type="NONE",
                    ),
                    "unsupported non-mesh objects",
                ),
            ]:
                with self.subTest(extra=extra.name):
                    bpy.context.scene.objects.append(extra)
                    with self.assertRaisesRegex(HandlerError, message):
                        handlers.dispatch("job_prepare_mechanical_rotation", params)
                    bpy.context.scene.objects.pop()
            bpy.ops.wm.stl_export.assert_not_called()
            bpy.data.objects.new.assert_not_called()

            bpy.context.scene.objects.append(
                SimpleNamespace(name="Unclassified", type="MESH")
            )
            with self.assertRaisesRegex(HandlerError, "unclassified scene meshes"):
                handlers.dispatch("job_prepare_mechanical_rotation", params)
            bpy.ops.wm.stl_export.assert_not_called()
            bpy.data.objects.new.assert_not_called()
            bpy.context.scene.objects.pop()

            result = handlers.dispatch("job_prepare_mechanical_rotation", params)

            self.assertEqual(
                result["fixed_path"],
                ".printable/jobs/test/analysis/fixed.stl",
            )
            self.assertEqual(
                result["moving_path"],
                ".printable/jobs/test/analysis/moving.stl",
            )
            self.assertEqual(result["motion"]["controller"], "MechanicalPivot")
            self.assertEqual(result["motion"]["axis"], [0.0, 0.0, 1.0])
            self.assertEqual(bpy.ops.wm.stl_export.call_count, 2)
            self.assertEqual(exported_groups, [["Base"], ["Leaf"]])
            self.assertIs(moving.parent, controller)
            self.assertTrue(
                (
                    root
                    / "workspace"
                    / ".printable/jobs/test/analysis/fixed.stl"
                ).is_file()
            )
            self.assertTrue(
                (
                    root
                    / "workspace"
                    / ".printable/jobs/test/analysis/moving.stl"
                ).is_file()
            )
            watchdog.arm.assert_called_once()
            watchdog.disarm.assert_called_once_with()
            handlers.close()

    def test_rotation_authoring_rolls_back_a_partial_keyframe_failure(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            bpy, target, controller = self._rotation_fixture(root)
            controller.keyframe_insert.side_effect = [True, False]
            handlers = BlenderHandlers(config(root, 9876), lambda: False, bpy)

            with self.assertRaisesRegex(HandlerError, "ending rotation keyframe"):
                handlers.dispatch(
                    "animate_rotation",
                    {
                        "objects": ["Leaf"],
                        "controller_name": "HingePivot",
                        "pivot": [0.0, 0.0, 0.0],
                        "axis": [0.0, 0.0, 1.0],
                        "angle_degrees": 90.0,
                    },
                )

            self.assertIsNone(target.parent)
            self.assertEqual(target.matrix_world.label, "leaf-world")
            self.assertEqual(
                target.matrix_parent_inverse.label, "leaf-parent-inverse"
            )
            bpy.data.objects.remove.assert_called_once_with(controller, do_unlink=True)
            self.assertEqual(
                bpy.context.preferences.edit.keyframe_new_interpolation_type,
                "BEZIER",
            )
            bpy.context.scene.frame_set.assert_called_once_with(17, subframe=0.375)
            handlers.close()

    def test_rotation_authoring_reports_unexpected_cleanup_failure(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            bpy, target, controller = self._rotation_fixture(root)
            controller.keyframe_insert.side_effect = [True, False]
            bpy.data.objects.remove.side_effect = LookupError("cleanup failed")
            handlers = BlenderHandlers(config(root, 9876), lambda: False, bpy)

            with self.assertRaisesRegex(HandlerError, "rollback was incomplete"):
                handlers.dispatch(
                    "animate_rotation",
                    {
                        "objects": ["Leaf"],
                        "controller_name": "HingePivot",
                        "pivot": [0.0, 0.0, 0.0],
                        "axis": [0.0, 0.0, 1.0],
                        "angle_degrees": 90.0,
                    },
                )

            self.assertIsNone(target.parent)
            self.assertEqual(target.matrix_world.label, "leaf-world")
            bpy.context.scene.frame_set.assert_called_once_with(17, subframe=0.375)
            handlers.close()

    def test_rotation_authoring_rejects_invalid_motion_before_object_creation(
        self,
    ) -> None:
        invalid_requests = [
            {
                "objects": ["Leaf", "Leaf"],
                "controller_name": "HingePivot",
                "pivot": [0.0, 0.0, 0.0],
                "axis": [0.0, 0.0, 1.0],
                "angle_degrees": 90.0,
            },
            {
                "objects": ["Leaf"],
                "controller_name": "HingePivot",
                "pivot": [0.0, 0.0, 0.0],
                "axis": [0.0, 0.0, 0.0],
                "angle_degrees": 90.0,
            },
            {
                "objects": ["Leaf"],
                "controller_name": "HingePivot",
                "pivot": [0.0, 0.0, 0.0],
                "axis": [0.0, 0.0, 1.0],
                "angle_degrees": 0.0,
            },
            {
                "objects": ["Leaf"],
                "controller_name": "HingePivot",
                "pivot": [0.0, 0.0, 0.0],
                "axis": [0.0, 0.0, 1.0],
                "angle_degrees": 10**1000,
            },
            {
                "objects": ["Leaf"],
                "controller_name": "HingePivot",
                "pivot": [0.0, 0.0, 0.0],
                "axis": [0.0, 0.0, 1.0],
                "angle_degrees": 90.0,
                "frame_start": 10,
                "frame_end": 10,
            },
        ]
        for params in invalid_requests:
            with self.subTest(params=params), tempfile.TemporaryDirectory() as temporary:
                root = Path(temporary)
                bpy, _, _ = self._rotation_fixture(root)
                handlers = BlenderHandlers(config(root, 9876), lambda: False, bpy)

                with self.assertRaises(HandlerError):
                    handlers.dispatch("animate_rotation", params)

                bpy.data.objects.new.assert_not_called()
                handlers.close()

    def test_rotation_authoring_rejects_competing_motion_and_controller(self) -> None:
        params = {
            "objects": ["Leaf"],
            "controller_name": "HingePivot",
            "pivot": [0.0, 0.0, 0.0],
            "axis": [0.0, 0.0, 1.0],
            "angle_degrees": 90.0,
        }
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            bpy, target, _ = self._rotation_fixture(root)
            target.parent = SimpleNamespace(name="ExistingParent")
            handlers = BlenderHandlers(config(root, 9876), lambda: False, bpy)

            with self.assertRaisesRegex(HandlerError, "already has a parent"):
                handlers.dispatch("animate_rotation", params)

            bpy.data.objects.new.assert_not_called()
            handlers.close()

        for attribute, value, message in [
            ("children", [SimpleNamespace(name="ExistingChild")], "children"),
            ("animation_data", SimpleNamespace(action=object()), "animation data"),
            ("constraints", [SimpleNamespace(name="Copy Location")], "constraints"),
            ("rigid_body", SimpleNamespace(type="ACTIVE"), "rigid body simulation"),
            (
                "rigid_body_constraint",
                SimpleNamespace(type="HINGE"),
                "rigid body constraint",
            ),
        ]:
            with self.subTest(attribute=attribute), tempfile.TemporaryDirectory() as temporary:
                root = Path(temporary)
                bpy, target, _ = self._rotation_fixture(root)
                setattr(target, attribute, value)
                handlers = BlenderHandlers(config(root, 9876), lambda: False, bpy)

                with self.assertRaisesRegex(HandlerError, message):
                    handlers.dispatch("animate_rotation", params)

                bpy.data.objects.new.assert_not_called()
                handlers.close()

        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            bpy, target, controller = self._rotation_fixture(root)
            bpy.data.objects.get = lambda name: {
                "Leaf": target,
                "HingePivot": controller,
            }.get(name)
            handlers = BlenderHandlers(config(root, 9876), lambda: False, bpy)

            with self.assertRaisesRegex(HandlerError, "object already exists"):
                handlers.dispatch("animate_rotation", params)

            bpy.data.objects.new.assert_not_called()
            handlers.close()

    def test_job_cycles_render_applies_samples_and_reports_the_device(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            watchdog = Mock()
            started = time.monotonic()
            render = SimpleNamespace(
                engine="unchanged",
                resolution_x=0,
                resolution_y=0,
                resolution_percentage=0,
                image_settings=SimpleNamespace(file_format=None),
                filepath=None,
            )
            scene = SimpleNamespace(
                cycles=SimpleNamespace(device=None, samples=0),
                render=render,
            )

            def render_still(*, write_still: bool) -> set[str]:
                self.assertTrue(write_still)
                Path(render.filepath).write_bytes(b"png")
                return {"FINISHED"}

            bpy = SimpleNamespace(
                data=SimpleNamespace(scenes=[scene]),
                context=SimpleNamespace(scene=scene),
                ops=SimpleNamespace(
                    render=SimpleNamespace(render=render_still),
                ),
            )
            handlers = BlenderHandlers(
                config(root, 9876), lambda: False, bpy, watchdog
            )

            with patch.object(handlers, "_ensure_camera_and_light"):
                result = handlers.dispatch(
                    "job_render_still",
                    {
                        "path": ".printable/jobs/cycles.png",
                        "width": 640,
                        "height": 480,
                        "engine": "CYCLES",
                        "samples": 32,
                    },
                )

            self.assertEqual(render.engine, "CYCLES")
            self.assertEqual(scene.cycles.device, "CPU")
            self.assertEqual(scene.cycles.samples, 32)
            self.assertEqual(result["render_device"], "CPU")
            self.assertEqual(result["samples"], 32)
            self.assertEqual(result["size_bytes"], 3)
            self.assertEqual(result["media_type"], "image/png")
            watchdog.arm.assert_called_once()
            self.assertGreaterEqual(watchdog.arm.call_args.args[0], started + 3599.0)
            watchdog.disarm.assert_called_once_with()
            self.assertEqual(
                (
                    root / "workspace" / ".printable" / "jobs" / "cycles.png"
                ).read_bytes(),
                b"png",
            )
            handlers.close()

    def test_render_frame_sets_timeline_frame_and_enforces_output_budget(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            watchdog = Mock()
            render = SimpleNamespace(
                engine="unchanged",
                resolution_x=0,
                resolution_y=0,
                resolution_percentage=0,
                image_settings=SimpleNamespace(file_format=None),
                filepath=None,
            )
            frame_set = Mock()
            scene = SimpleNamespace(
                cycles=SimpleNamespace(device=None, samples=0),
                render=render,
                frame_set=frame_set,
            )

            def render_still(*, write_still: bool) -> set[str]:
                self.assertTrue(write_still)
                Path(render.filepath).write_bytes(b"frame-png")
                return {"FINISHED"}

            bpy = SimpleNamespace(
                data=SimpleNamespace(scenes=[scene]),
                context=SimpleNamespace(scene=scene),
                ops=SimpleNamespace(render=SimpleNamespace(render=render_still)),
            )
            handlers = BlenderHandlers(
                config(root, 9876), lambda: False, bpy, watchdog
            )
            with (
                patch.object(handlers, "_ensure_camera_and_light"),
                self.assertRaisesRegex(HandlerError, "output byte budget"),
            ):
                handlers.dispatch(
                    "job_render_frame",
                    {
                        "path": ".printable/jobs/frame.png",
                        "frame": 42,
                        "max_output_bytes": 8,
                    },
                )

            frame_set.assert_called_once_with(42)
            self.assertFalse(
                (root / "workspace" / ".printable" / "jobs" / "frame.png").exists()
            )
            watchdog.disarm.assert_called_once_with()
            handlers.close()

    def test_job_render_views_commits_a_batch_and_restores_scene_state(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            watchdog = Mock()
            original_camera = object()
            render = SimpleNamespace(
                engine="ORIGINAL",
                resolution_x=320,
                resolution_y=200,
                resolution_percentage=75,
                image_settings=SimpleNamespace(
                    file_format="JPEG", color_mode="RGBA", color_depth="16"
                ),
                filepath="original.jpg",
            )
            scene = SimpleNamespace(
                camera=original_camera,
                cycles=SimpleNamespace(device="CPU", samples=16),
                render=render,
                objects=[SimpleNamespace(type="LIGHT")],
                collection=SimpleNamespace(objects=SimpleNamespace(link=Mock())),
            )
            camera_data = SimpleNamespace(users=0, type="PERSP")
            camera = SimpleNamespace(data=camera_data)
            cameras = SimpleNamespace(new=Mock(return_value=camera_data), remove=Mock())
            objects = SimpleNamespace(new=Mock(return_value=camera), remove=Mock())

            def render_still(*, write_still: bool) -> set[str]:
                self.assertTrue(write_still)
                self.assertEqual(render.image_settings.color_mode, "RGB")
                self.assertEqual(render.image_settings.color_depth, "8")
                Path(render.filepath).write_bytes(b"png")
                return {"FINISHED"}

            bpy = SimpleNamespace(
                data=SimpleNamespace(scenes=[scene], cameras=cameras, objects=objects),
                context=SimpleNamespace(scene=scene),
                ops=SimpleNamespace(render=SimpleNamespace(render=render_still)),
            )
            handlers = BlenderHandlers(
                config(root, 9876), lambda: False, bpy, watchdog
            )

            with (
                patch.object(
                    handlers,
                    "_render_bounds",
                    return_value=(
                        [FakeVector((-1.0, -1.0, -1.0)), FakeVector((1.0, 1.0, 1.0))],
                        FakeVector((0.0, 0.0, 0.0)),
                        2.0,
                    ),
                ),
                patch.object(handlers, "_frame_review_camera") as frame,
            ):
                result = handlers.dispatch(
                    "job_render_views",
                    {
                        "views": [
                            {
                                "path": ".printable/jobs/front.png",
                                "label": "FRONT",
                                "direction": [0.0, -1.0, 0.0],
                            },
                            {
                                "path": ".printable/jobs/right.png",
                                "label": "RIGHT",
                                "direction": [1.0, 0.0, 0.0],
                            },
                        ],
                        "width": 64,
                        "height": 48,
                    },
                )

            self.assertEqual(
                [item["label"] for item in result["views"]], ["FRONT", "RIGHT"]
            )
            self.assertEqual(result["engine"], "BLENDER_EEVEE")
            self.assertEqual(result["bounds"]["minimum"], [-1.0, -1.0, -1.0])
            self.assertEqual(result["bounds"]["maximum"], [1.0, 1.0, 1.0])
            self.assertEqual(result["bounds"]["dimensions"], [2.0, 2.0, 2.0])
            self.assertEqual(result["bounds"]["coordinate_space"], "world")
            self.assertEqual(result["bounds"]["unit"], "blender_unit")
            self.assertEqual(scene.camera, original_camera)
            self.assertEqual(
                (
                    render.engine,
                    render.resolution_x,
                    render.resolution_y,
                    render.resolution_percentage,
                    render.image_settings.file_format,
                    render.image_settings.color_mode,
                    render.image_settings.color_depth,
                    render.filepath,
                    scene.cycles.device,
                    scene.cycles.samples,
                ),
                (
                    "ORIGINAL",
                    320,
                    200,
                    75,
                    "JPEG",
                    "RGBA",
                    "16",
                    "original.jpg",
                    "CPU",
                    16,
                ),
            )
            self.assertEqual(frame.call_count, 2)
            objects.remove.assert_called_once_with(camera, do_unlink=True)
            cameras.remove.assert_called_once_with(camera_data)
            watchdog.arm.assert_called_once()
            watchdog.disarm.assert_called_once_with()
            self.assertEqual(
                (
                    root / "workspace" / ".printable" / "jobs" / "front.png"
                ).read_bytes(),
                b"png",
            )
            self.assertEqual(
                (
                    root / "workspace" / ".printable" / "jobs" / "right.png"
                ).read_bytes(),
                b"png",
            )
            handlers.close()

    def test_render_views_rejects_invalid_batches_before_scene_mutation(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            scene = SimpleNamespace(cycles=SimpleNamespace(device=None))
            bpy = SimpleNamespace(
                data=SimpleNamespace(scenes=[scene]),
                context=SimpleNamespace(scene=scene),
            )
            handlers = BlenderHandlers(config(root, 9876), lambda: False, bpy)
            valid = {
                "path": "renders/front.png",
                "label": "FRONT",
                "direction": [0.0, -1.0, 0.0],
            }
            for params in (
                {"views": []},
                {"views": [valid, valid]},
                {"views": [{**valid, "direction": [0.0, 0.0, 0.0]}]},
                {
                    "views": [valid, {**valid, "path": "renders/right.png"}],
                    "width": 8192,
                    "height": 8192,
                },
                {"views": [valid], "width": 8192, "height": 1025},
            ):
                with self.subTest(params=params), self.assertRaises(HandlerError):
                    handlers.dispatch("render_views", params)
            self.assertFalse((root / "workspace" / "renders").exists())
            handlers.close()

    def test_render_views_does_not_promote_outputs_when_scene_cleanup_fails(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            watchdog = Mock()
            render = SimpleNamespace(
                engine="ORIGINAL",
                resolution_x=320,
                resolution_y=200,
                resolution_percentage=100,
                image_settings=SimpleNamespace(
                    file_format="PNG", color_mode="RGBA", color_depth="16"
                ),
                filepath="original.png",
            )
            scene = SimpleNamespace(
                camera=None,
                cycles=SimpleNamespace(device="CPU", samples=16),
                render=render,
                objects=[SimpleNamespace(type="LIGHT")],
                collection=SimpleNamespace(objects=SimpleNamespace(link=Mock())),
            )
            camera_data = SimpleNamespace(users=0, type="PERSP")
            camera = SimpleNamespace(data=camera_data)

            def render_still(*, write_still: bool) -> set[str]:
                self.assertTrue(write_still)
                Path(render.filepath).write_bytes(b"png")
                return {"FINISHED"}

            bpy = SimpleNamespace(
                data=SimpleNamespace(
                    scenes=[scene],
                    cameras=SimpleNamespace(new=Mock(return_value=camera_data), remove=Mock()),
                    objects=SimpleNamespace(new=Mock(return_value=camera), remove=Mock()),
                ),
                context=SimpleNamespace(scene=scene),
                ops=SimpleNamespace(render=SimpleNamespace(render=render_still)),
            )
            handlers = BlenderHandlers(
                config(root, 9876), lambda: False, bpy, watchdog
            )

            with (
                patch.object(
                    handlers,
                    "_render_bounds",
                    return_value=(
                        [FakeVector((-1.0, -1.0, -1.0)), FakeVector((1.0, 1.0, 1.0))],
                        FakeVector((0.0, 0.0, 0.0)),
                        2.0,
                    ),
                ),
                patch.object(handlers, "_frame_review_camera"),
                patch.object(
                    handlers,
                    "_remove_review_lights",
                    side_effect=RuntimeError("cleanup failed"),
                ),
                self.assertRaisesRegex(RuntimeError, "cleanup failed"),
            ):
                handlers.dispatch(
                    "render_views",
                    {
                        "views": [
                            {
                                "path": "renders/front.png",
                                "label": "FRONT",
                                "direction": [0.0, -1.0, 0.0],
                            }
                        ],
                        "width": 64,
                        "height": 48,
                    },
                )

            watchdog.disarm.assert_called_once_with()
            self.assertFalse((root / "workspace" / "renders" / "front.png").exists())
            handlers.close()

    def test_render_diagnostic_commits_only_after_cleanup(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            watchdog = Mock()
            original_camera = object()
            render = SimpleNamespace(
                engine="ORIGINAL",
                resolution_x=320,
                resolution_y=200,
                resolution_percentage=75,
                image_settings=SimpleNamespace(
                    file_format="JPEG", color_mode="RGBA", color_depth="16"
                ),
                filepath="original.jpg",
            )
            light = SimpleNamespace(type="LIGHT", hide_render=False)
            scene = SimpleNamespace(
                camera=original_camera,
                cycles=SimpleNamespace(device="CPU", samples=16),
                render=render,
                objects=[light],
                collection=SimpleNamespace(objects=SimpleNamespace(link=Mock())),
            )
            camera_data = SimpleNamespace(users=0, type="PERSP")
            camera = SimpleNamespace(data=camera_data)
            cameras = SimpleNamespace(new=Mock(return_value=camera_data), remove=Mock())
            objects = SimpleNamespace(new=Mock(return_value=camera), remove=Mock())
            meshes = SimpleNamespace(remove=Mock())
            materials = SimpleNamespace(remove=Mock())
            update = Mock()

            def render_still(*, write_still: bool) -> set[str]:
                self.assertTrue(write_still)
                Path(render.filepath).write_bytes(b"diagnostic-png")
                return {"FINISHED"}

            bpy = SimpleNamespace(
                data=SimpleNamespace(
                    scenes=[scene],
                    cameras=cameras,
                    objects=objects,
                    meshes=meshes,
                    materials=materials,
                ),
                context=SimpleNamespace(
                    scene=scene,
                    view_layer=SimpleNamespace(update=update),
                    evaluated_depsgraph_get=Mock(return_value=object()),
                ),
                ops=SimpleNamespace(render=SimpleNamespace(render=render_still)),
            )
            handlers = BlenderHandlers(
                config(root, 9876), lambda: False, bpy, watchdog
            )
            corners = [
                FakeVector((-2.0, -3.0, -4.0)),
                FakeVector((2.0, 3.0, 4.0)),
            ]
            diagnostic_object = SimpleNamespace(type="MESH", hide_render=False)

            def create_geometry(
                _mode: str,
                _names: set[str] | None,
                _options: dict[str, object],
                _counts: tuple[int, int, int, int, int, int],
                created_objects: list[object],
                _meshes: list[object],
                _materials: list[object],
            ) -> dict[str, object]:
                created_objects.append(diagnostic_object)
                return {"section_faces": 2, "section_area": 12.0}

            with (
                patch.object(handlers, "_missing_renderable_names", return_value=[]),
                patch.object(
                    handlers,
                    "_diagnostic_geometry_counts",
                    return_value=(1, 8, 12, 6, 18, 0),
                ),
                patch.object(
                    handlers,
                    "_render_bounds",
                    return_value=(corners, FakeVector((0.0, 0.0, 0.0)), 10.0),
                ),
                patch.object(
                    handlers,
                    "_create_diagnostic_geometry",
                    side_effect=create_geometry,
                ),
                patch.object(handlers, "_frame_review_camera"),
            ):
                result = handlers.dispatch(
                    "render_diagnostic",
                    {
                        "path": "diagnostics/section.png",
                        "mode": "cross_section",
                        "objects": ["Body"],
                        "axis": "Z",
                        "width": 64,
                        "height": 48,
                    },
                )
                self.assertEqual(scene.camera, original_camera)
                self.assertEqual(render.filepath, "original.jpg")
                self.assertEqual(objects.remove.call_count, 2)
                objects.remove.assert_any_call(camera, do_unlink=True)
                objects.remove.assert_any_call(diagnostic_object, do_unlink=True)
                with (
                    patch.object(
                        handlers,
                        "_cleanup_diagnostic_render",
                        side_effect=RuntimeError("cleanup failed"),
                    ),
                    self.assertRaisesRegex(RuntimeError, "cleanup failed"),
                ):
                    handlers.dispatch(
                        "render_diagnostic",
                        {
                            "path": "diagnostics/not-promoted.png",
                            "mode": "cross_section",
                            "objects": ["Body"],
                            "axis": "Z",
                            "width": 64,
                            "height": 48,
                        },
                    )

            self.assertEqual(result["mode"], "cross_section")
            self.assertEqual(result["analysis"]["section_faces"], 2)
            self.assertEqual(result["source_bounds"]["dimensions"], [4.0, 6.0, 8.0])
            self.assertEqual(
                (root / "workspace" / "diagnostics" / "section.png").read_bytes(),
                b"diagnostic-png",
            )
            self.assertFalse(
                (root / "workspace" / "diagnostics" / "not-promoted.png").exists()
            )
            self.assertEqual(watchdog.disarm.call_count, 2)
            handlers.close()

    def test_render_diagnostic_validates_before_scene_mutation(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            watchdog = Mock()
            render_call = Mock()
            scene = SimpleNamespace(cycles=SimpleNamespace(device=None))
            bpy = SimpleNamespace(
                data=SimpleNamespace(scenes=[scene]),
                ops=SimpleNamespace(render=SimpleNamespace(render=render_call)),
            )
            handlers = BlenderHandlers(
                config(root, 9876), lambda: False, bpy, watchdog
            )

            for params in (
                {"path": "diagnostics/a.png", "mode": "invalid"},
                {
                    "path": "diagnostics/b.png",
                    "mode": "overhang",
                    "objects": ["Body", "Body"],
                },
                {
                    "path": "diagnostics/c.png",
                    "mode": "overhang",
                    "width": 8192,
                    "height": 1025,
                },
            ):
                with self.subTest(params=params), self.assertRaises(HandlerError):
                    handlers.dispatch("render_diagnostic", params)

            render_call.assert_not_called()
            watchdog.arm.assert_not_called()
            self.assertFalse((root / "workspace" / "diagnostics").exists())
            handlers.close()

    def test_render_diagnostic_normalizes_large_directions_before_mutation(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            watchdog = Mock()
            scene = SimpleNamespace(cycles=SimpleNamespace(device=None))
            bpy = SimpleNamespace(data=SimpleNamespace(scenes=[scene]))
            handlers = BlenderHandlers(
                config(root, 9876), lambda: False, bpy, watchdog
            )

            with patch.object(
                handlers, "_render_diagnostic_armed", return_value={}
            ) as render:
                handlers.dispatch(
                    "render_diagnostic",
                    {
                        "path": "diagnostics/normalized.png",
                        "mode": "overhang",
                        "build_direction": [1e308, 0.0, 0.0],
                        "view_direction": [0.0, -1e308, 0.0],
                    },
                )
                handlers.dispatch(
                    "render_diagnostic",
                    {
                        "path": "diagnostics/tiny-direction.png",
                        "mode": "overhang",
                        "build_direction": [1e-300, 0.0, 0.0],
                    },
                )
                handlers.dispatch(
                    "render_diagnostic",
                    {
                        "path": "diagnostics/max-direction.png",
                        "mode": "overhang",
                        "build_direction": [sys.float_info.max] * 3,
                    },
                )

            first_call, second_call, third_call = render.call_args_list
            options = first_call.args[3]
            self.assertEqual(options["build_direction"], (1.0, 0.0, 0.0))
            self.assertEqual(first_call.args[4], (0.0, -1.0, 0.0))
            self.assertEqual(
                second_call.args[3]["build_direction"], (1.0, 0.0, 0.0)
            )
            expected = 3.0**-0.5
            for component in third_call.args[3]["build_direction"]:
                self.assertAlmostEqual(component, expected)
            handlers.close()

    def test_render_diagnostic_watchdog_covers_preflight_failure(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            watchdog = Mock()
            render_call = Mock()
            scene = SimpleNamespace(cycles=SimpleNamespace(device=None))
            bpy = SimpleNamespace(
                data=SimpleNamespace(scenes=[scene]),
                ops=SimpleNamespace(render=SimpleNamespace(render=render_call)),
            )
            handlers = BlenderHandlers(
                config(root, 9876), lambda: False, bpy, watchdog
            )

            with (
                patch.object(
                    handlers,
                    "_missing_renderable_names",
                    side_effect=HandlerError("preflight stopped"),
                ),
                self.assertRaisesRegex(HandlerError, "preflight stopped"),
            ):
                handlers.dispatch(
                    "render_diagnostic",
                    {
                        "path": "diagnostics/preflight.png",
                        "mode": "overhang",
                        "objects": ["Body"],
                        "timeout_seconds": 0.25,
                    },
                )

            watchdog.arm.assert_called_once()
            deadline = watchdog.arm.call_args.args[0]
            self.assertGreater(deadline, time.monotonic() - 1.0)
            watchdog.disarm.assert_called_once_with()
            render_call.assert_not_called()
            self.assertFalse((root / "workspace" / "diagnostics").exists())
            handlers.close()

    def test_cross_section_contours_require_closed_manifold_loops(self) -> None:
        class Vertex:
            pass

        class Edge:
            def __init__(self, first: Vertex, second: Vertex):
                self.verts = (first, second)

            def other_vert(self, vertex: Vertex) -> Vertex:
                return self.verts[1] if vertex is self.verts[0] else self.verts[0]

        def loop(size: int) -> tuple[list[Vertex], list[Edge]]:
            vertices = [Vertex() for _ in range(size)]
            edges = [
                Edge(vertices[index], vertices[(index + 1) % size])
                for index in range(size)
            ]
            return vertices, edges

        outer_vertices, outer_edges = loop(4)
        inner_vertices, inner_edges = loop(3)
        contours = BlenderHandlers._ordered_cut_contours(
            outer_edges + inner_edges
        )
        self.assertEqual(sorted(len(contour) for contour in contours), [3, 4])
        self.assertEqual(
            {vertex for contour in contours for vertex in contour},
            set(outer_vertices + inner_vertices),
        )

        with self.assertRaisesRegex(HandlerError, "open or non-manifold"):
            BlenderHandlers._ordered_cut_contours(outer_edges[:-1])

    def test_cross_section_tessellation_maps_indices_to_cut_vertices(self) -> None:
        class Point:
            def __init__(self, coordinates: tuple[float, float, float]) -> None:
                self.coordinates = coordinates

            def copy(self) -> Point:
                return Point(self.coordinates)

        class Vertex:
            def __init__(self, coordinates: tuple[float, float, float]) -> None:
                self.co = Point(coordinates)

        class Edge:
            def __init__(self, first: Vertex, second: Vertex) -> None:
                self.verts = (first, second)

            def other_vert(self, vertex: Vertex) -> Vertex:
                return self.verts[1] if vertex is self.verts[0] else self.verts[0]

        vertices = [
            Vertex((0.0, 0.0, 0.0)),
            Vertex((1.0, 0.0, 0.0)),
            Vertex((0.0, 1.0, 0.0)),
        ]
        edges = [
            Edge(vertices[index], vertices[(index + 1) % len(vertices)])
            for index in range(len(vertices))
        ]
        created_faces: list[tuple[Vertex, ...]] = []

        def tessellate(polylines: list[list[Point]]) -> list[tuple[int, ...]]:
            self.assertEqual(len(polylines), 1)
            self.assertEqual(len(polylines[0]), 3)
            return [(0, 1, 2)]

        def create_face(face_vertices: list[Vertex]) -> tuple[Vertex, ...]:
            face = tuple(face_vertices)
            created_faces.append(face)
            return face

        geometry = SimpleNamespace(tessellate_polygon=tessellate)
        bm = SimpleNamespace(faces=SimpleNamespace(new=create_face))
        with patch.dict(
            sys.modules,
            {"mathutils": SimpleNamespace(geometry=geometry), "mathutils.geometry": geometry},
        ):
            filled = BlenderHandlers._fill_cross_section(bm, edges)

        self.assertEqual(filled, created_faces)
        self.assertEqual(set(filled[0]), set(vertices))

        for invalid_index in (-1, len(vertices), 1.5, True):
            with (
                self.subTest(invalid_index=invalid_index),
                patch.object(
                    geometry,
                    "tessellate_polygon",
                    return_value=[(0, 1, invalid_index)],
                ),
                patch.dict(
                    sys.modules,
                    {
                        "mathutils": SimpleNamespace(geometry=geometry),
                        "mathutils.geometry": geometry,
                    },
                ),
                self.assertRaisesRegex(HandlerError, "unknown vertex index"),
            ):
                BlenderHandlers._fill_cross_section(bm, edges)

    def test_diagnostic_subset_name_discovery_stops_after_requested_names(self) -> None:
        requested = SimpleNamespace(
            name="Requested",
            type="MESH",
            hide_render=False,
            visible_camera=True,
        )

        def instances():
            yield SimpleNamespace(object=requested, show_self=True)
            raise AssertionError("name discovery scanned beyond the bounded request")

        depsgraph = SimpleNamespace(object_instances=instances())
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            scene = SimpleNamespace(cycles=SimpleNamespace(device=None))
            bpy = SimpleNamespace(
                data=SimpleNamespace(scenes=[scene]),
                context=SimpleNamespace(
                    evaluated_depsgraph_get=Mock(return_value=depsgraph)
                ),
            )
            handlers = BlenderHandlers(config(root, 9876), lambda: False, bpy)
            self.assertEqual(handlers._missing_renderable_names({"Requested"}), [])
            handlers.close()

    def test_diagnostic_faces_from_instances_share_one_combined_mesh_buffer(self) -> None:
        class Vertex:
            def __init__(self, coordinates: tuple[float, float, float]) -> None:
                self.co = coordinates

        class Face:
            def __init__(self, vertices: list[Vertex], material_index: int) -> None:
                self.verts = vertices
                self.material_index = material_index

        vertices: list[tuple[float, float, float]] = []
        faces: list[tuple[int, ...]] = []
        material_indices: list[int] = []
        first = [
            Vertex((0.0, 0.0, 0.0)),
            Vertex((1.0, 0.0, 0.0)),
            Vertex((0.0, 1.0, 0.0)),
        ]
        second = [
            Vertex((2.0, 0.0, 0.0)),
            Vertex((3.0, 0.0, 0.0)),
            Vertex((2.0, 1.0, 0.0)),
        ]

        for instance_vertices, material_index in ((first, 0), (second, 2)):
            BlenderHandlers._append_diagnostic_faces(
                SimpleNamespace(faces=[Face(instance_vertices, material_index)]),
                vertices,
                faces,
                material_indices,
            )

        self.assertEqual(len(vertices), 6)
        self.assertEqual(faces, [(0, 1, 2), (3, 4, 5)])
        self.assertEqual(material_indices, [0, 2])

    def test_overhang_angles_and_categories_cover_exact_boundaries(self) -> None:
        class Normal:
            def __init__(self, dot: float):
                self._dot = dot

            def dot(self, _direction: object) -> float:
                return self._dot

        self.assertEqual(
            BlenderHandlers._overhang_angle_degrees(Normal(1.0), object()), 0.0
        )
        self.assertAlmostEqual(
            BlenderHandlers._overhang_angle_degrees(
                Normal(-(2.0**0.5) / 2.0), object()
            ),
            45.0,
        )
        self.assertEqual(
            BlenderHandlers._overhang_angle_degrees(Normal(-1.0), object()), 90.0
        )
        self.assertEqual(BlenderHandlers._overhang_category(45.0, 45.0), ("supported", 0))
        self.assertEqual(BlenderHandlers._overhang_category(45.1, 45.0), ("warning", 1))
        self.assertEqual(BlenderHandlers._overhang_category(60.0, 45.0), ("warning", 1))
        self.assertEqual(BlenderHandlers._overhang_category(60.1, 45.0), ("severe", 2))

    def test_review_lights_scale_with_scene_size_and_clean_up(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            scene = SimpleNamespace(
                cycles=SimpleNamespace(device=None),
                collection=SimpleNamespace(objects=SimpleNamespace(link=Mock())),
            )
            light_data = [
                SimpleNamespace(users=0),
                SimpleNamespace(users=0),
            ]
            lights = SimpleNamespace(
                new=Mock(side_effect=light_data),
                remove=Mock(),
            )
            light_objects = [
                SimpleNamespace(location=None, rotation_euler=None),
                SimpleNamespace(location=None, rotation_euler=None),
            ]
            objects = SimpleNamespace(
                new=Mock(side_effect=light_objects),
                remove=Mock(),
            )
            bpy = SimpleNamespace(
                data=SimpleNamespace(scenes=[scene], lights=lights, objects=objects),
                context=SimpleNamespace(scene=scene),
            )
            handlers = BlenderHandlers(config(root, 9876), lambda: False, bpy)

            with patch.dict(
                sys.modules, {"mathutils": SimpleNamespace(Vector=FakeVector)}
            ):
                created = handlers._create_review_lights(
                    FakeVector((0.0, 0.0, 0.0)), 2.0
                )

            self.assertEqual([item.energy for item in light_data], [4000.0, 2000.0])
            self.assertEqual([item.size for item in light_data], [2.0, 2.0])
            handlers._remove_review_lights(created)
            self.assertEqual(objects.remove.call_count, 2)
            self.assertEqual(lights.remove.call_count, 2)
            handlers.close()

    def test_review_lights_clean_up_partial_creation(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            scene = SimpleNamespace(
                cycles=SimpleNamespace(device=None),
                collection=SimpleNamespace(objects=SimpleNamespace(link=Mock())),
            )
            light_data = [
                SimpleNamespace(users=0),
                SimpleNamespace(users=0),
            ]
            lights = SimpleNamespace(
                new=Mock(side_effect=light_data),
                remove=Mock(),
            )
            first_light = SimpleNamespace(location=None, rotation_euler=None)
            objects = SimpleNamespace(
                new=Mock(side_effect=[first_light, RuntimeError("creation failed")]),
                remove=Mock(),
            )
            bpy = SimpleNamespace(
                data=SimpleNamespace(scenes=[scene], lights=lights, objects=objects),
                context=SimpleNamespace(scene=scene),
            )
            handlers = BlenderHandlers(config(root, 9876), lambda: False, bpy)

            with (
                patch.dict(
                    sys.modules, {"mathutils": SimpleNamespace(Vector=FakeVector)}
                ),
                self.assertRaisesRegex(RuntimeError, "creation failed"),
            ):
                handlers._create_review_lights(FakeVector((0.0, 0.0, 0.0)), 1.0)

            objects.remove.assert_called_once_with(first_light, do_unlink=True)
            self.assertEqual(lights.remove.call_count, 2)
            handlers.close()

    def test_render_bounds_reject_non_finite_geometry(self) -> None:
        class IdentityMatrix:
            def __matmul__(self, vector: FakeVector) -> FakeVector:
                return vector

        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            geometry = SimpleNamespace(
                type="MESH",
                hide_render=False,
                visible_camera=True,
                data=SimpleNamespace(
                    vertices=[SimpleNamespace(co=(float("nan"), 0.0, 0.0))]
                ),
            )
            depsgraph = SimpleNamespace(
                object_instances=[
                    SimpleNamespace(
                        object=geometry,
                        show_self=True,
                        matrix_world=IdentityMatrix(),
                    )
                ]
            )
            scene = SimpleNamespace(
                cycles=SimpleNamespace(device=None),
            )
            bpy = SimpleNamespace(
                data=SimpleNamespace(scenes=[scene]),
                context=SimpleNamespace(
                    scene=scene,
                    evaluated_depsgraph_get=Mock(return_value=depsgraph),
                ),
            )
            handlers = BlenderHandlers(config(root, 9876), lambda: False, bpy)

            with (
                patch.dict(
                    sys.modules, {"mathutils": SimpleNamespace(Vector=FakeVector)}
                ),
                self.assertRaisesRegex(HandlerError, "non-finite bounds"),
            ):
                handlers._render_bounds()

            handlers.close()

    def test_render_bounds_include_dependency_graph_instances(self) -> None:
        class TranslationMatrix:
            def __matmul__(self, vector: FakeVector) -> FakeVector:
                return vector + FakeVector((10.0, -5.0, 3.0))

        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            geometry = SimpleNamespace(
                type="MESH",
                hide_render=False,
                visible_camera=True,
                data=SimpleNamespace(
                    vertices=[
                        SimpleNamespace(co=(-1.0, -2.0, -3.0)),
                        SimpleNamespace(co=(1.0, 2.0, 3.0)),
                    ]
                ),
            )
            instance = SimpleNamespace(
                is_instance=True,
                object=geometry,
                show_self=True,
                matrix_world=TranslationMatrix(),
            )
            depsgraph = SimpleNamespace(object_instances=[instance])
            scene = SimpleNamespace(
                cycles=SimpleNamespace(device=None),
                objects=[SimpleNamespace(type="EMPTY")],
            )
            bpy = SimpleNamespace(
                data=SimpleNamespace(scenes=[scene]),
                context=SimpleNamespace(
                    scene=scene,
                    evaluated_depsgraph_get=Mock(return_value=depsgraph),
                ),
            )
            handlers = BlenderHandlers(config(root, 9876), lambda: False, bpy)

            with patch.dict(
                sys.modules, {"mathutils": SimpleNamespace(Vector=FakeVector)}
            ):
                corners, center, diagonal = handlers._render_bounds()

            self.assertEqual(len(corners), 8)
            self.assertEqual(center.values, (10.0, -5.0, 3.0))
            self.assertAlmostEqual(diagonal, 56.0**0.5)
            handlers.close()

    def test_bounds_metadata_is_consistent_with_serialized_extrema(self) -> None:
        minimum = (-28.0, -11.0, 0.0)
        maximum = (51.79999923706055, 11.0, 10.0)

        bounds = BlenderHandlers._bounds_metadata(
            [minimum, maximum],
            (11.899999618530273, 0.0, 5.0),
            83.37889737423822,
        )

        dimensions = [
            high - low for low, high in zip(bounds["minimum"], bounds["maximum"])
        ]
        self.assertEqual(bounds["dimensions"], dimensions)
        self.assertEqual(
            bounds["center"],
            [
                (low + high) * 0.5
                for low, high in zip(bounds["minimum"], bounds["maximum"])
            ],
        )
        self.assertEqual(bounds["diagonal"], math.hypot(*dimensions))

    def test_product_orthographic_scale_fits_landscape_projection(self) -> None:
        aspect = 4.0 / 3.0
        scale = BlenderHandlers._product_orthographic_scale(73.0, 55.0, aspect)

        self.assertGreaterEqual(scale, 73.0 * 1.15)
        self.assertAlmostEqual(scale / aspect, 55.0 * 1.15)

    def test_product_orthographic_scale_fits_portrait_projection(self) -> None:
        aspect = 3.0 / 4.0
        scale = BlenderHandlers._product_orthographic_scale(55.0, 73.0, aspect)

        self.assertAlmostEqual(scale * aspect, 55.0 * 1.15)
        self.assertGreaterEqual(scale, 73.0 * 1.15)

    def test_render_bounds_measure_rotated_vertices_not_transformed_local_boxes(
        self,
    ) -> None:
        class RotatedMatrix:
            def __matmul__(self, vector: FakeVector) -> FakeVector:
                x, y, z = vector.values
                return FakeVector((x - y, x + y, z))

        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            geometry = SimpleNamespace(
                type="MESH",
                hide_render=False,
                visible_camera=True,
                data=SimpleNamespace(
                    vertices=[
                        SimpleNamespace(co=(0.0, 0.0, 0.0)),
                        SimpleNamespace(co=(2.0, 0.0, 0.0)),
                        SimpleNamespace(co=(0.0, 1.0, 0.0)),
                    ]
                ),
            )
            depsgraph = SimpleNamespace(
                object_instances=[
                    SimpleNamespace(
                        object=geometry,
                        show_self=True,
                        matrix_world=RotatedMatrix(),
                    )
                ]
            )
            scene = SimpleNamespace(cycles=SimpleNamespace(device=None))
            bpy = SimpleNamespace(
                data=SimpleNamespace(scenes=[scene]),
                context=SimpleNamespace(
                    scene=scene,
                    evaluated_depsgraph_get=Mock(return_value=depsgraph),
                ),
            )
            handlers = BlenderHandlers(config(root, 9876), lambda: False, bpy)

            with patch.dict(
                sys.modules, {"mathutils": SimpleNamespace(Vector=FakeVector)}
            ):
                corners, center, diagonal = handlers._render_bounds()

            self.assertEqual(len(corners), 8)
            self.assertEqual(center.values, (0.5, 1.0, 0.0))
            self.assertAlmostEqual(diagonal, 13.0**0.5)
            handlers.close()

    def test_diagnostic_transform_corrects_orientation_reversing_winding(self) -> None:
        class Matrix:
            def __init__(self, determinant: float):
                self._determinant = determinant

            def determinant(self) -> float:
                return self._determinant

        transform = Mock()
        reverse_faces = Mock()
        bmesh_module = SimpleNamespace(
            ops=SimpleNamespace(transform=transform, reverse_faces=reverse_faces)
        )
        bm = SimpleNamespace(
            verts=[object(), object(), object()],
            faces=[object()],
            normal_update=Mock(),
        )
        matrix = Matrix(-1.0)

        BlenderHandlers._transform_diagnostic_mesh(bm, matrix, bmesh_module)

        transform.assert_called_once_with(bm, matrix=matrix, verts=bm.verts)
        reverse_faces.assert_called_once_with(bm, faces=bm.faces)
        bm.normal_update.assert_called_once_with()

        transform.reset_mock()
        reverse_faces.reset_mock()
        bm.normal_update.reset_mock()
        matrix = Matrix(1.0)
        BlenderHandlers._transform_diagnostic_mesh(bm, matrix, bmesh_module)
        transform.assert_called_once_with(bm, matrix=matrix, verts=bm.verts)
        reverse_faces.assert_not_called()
        bm.normal_update.assert_called_once_with()

        transform.reset_mock()
        reverse_faces.reset_mock()
        bm.normal_update.reset_mock()
        matrix = Matrix(1e-15)
        BlenderHandlers._transform_diagnostic_mesh(bm, matrix, bmesh_module)
        transform.assert_called_once_with(bm, matrix=matrix, verts=bm.verts)
        reverse_faces.assert_not_called()
        bm.normal_update.assert_called_once_with()

        with self.assertRaisesRegex(HandlerError, "non-invertible"):
            BlenderHandlers._transform_diagnostic_mesh(
                bm, Matrix(0.0), bmesh_module
            )

    def test_diagnostic_geometry_limit_is_checked_before_bmesh_allocation(self) -> None:
        class Sized:
            def __init__(self, length: int):
                self._length = length

            def __len__(self) -> int:
                return self._length

            def __iter__(self):
                return iter(
                    SimpleNamespace(groups=()) for _ in range(self._length)
                )

        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            depsgraph = SimpleNamespace(object_instances=[])
            materials = SimpleNamespace(new=Mock())
            scene = SimpleNamespace(cycles=SimpleNamespace(device=None))
            bpy = SimpleNamespace(
                data=SimpleNamespace(scenes=[scene], materials=materials),
                context=SimpleNamespace(
                    scene=scene,
                    evaluated_depsgraph_get=Mock(return_value=depsgraph),
                ),
            )
            handlers = BlenderHandlers(config(root, 9876), lambda: False, bpy)
            bmesh_module = SimpleNamespace(new=Mock())
            cases = [
                ("vertices", "vertices", MAX_DIAGNOSTIC_VERTICES),
                ("edges", "edges", MAX_DIAGNOSTIC_EDGES),
                ("faces", "polygons", MAX_DIAGNOSTIC_FACES),
                ("loops", "loops", MAX_DIAGNOSTIC_LOOPS),
                ("copied attribute values", "attributes", MAX_DIAGNOSTIC_ATTRIBUTE_VALUES),
            ]
            for message, field, limit in cases:
                data = SimpleNamespace(
                    vertices=Sized(3),
                    edges=Sized(3),
                    polygons=Sized(1),
                    loops=Sized(3),
                    attributes=[],
                )
                if field == "attributes":
                    data.attributes = [
                        SimpleNamespace(
                            data_type="FLOAT", data=Sized(limit + 1)
                        )
                    ]
                else:
                    setattr(data, field, Sized(limit + 1))
                geometry = SimpleNamespace(
                    name="Oversized",
                    type="MESH",
                    hide_render=False,
                    visible_camera=True,
                    data=data,
                )
                depsgraph.object_instances = [
                    SimpleNamespace(object=geometry, show_self=True)
                ]
                with (
                    self.subTest(field=field),
                    patch.dict(
                        sys.modules,
                        {
                            "bmesh": bmesh_module,
                            "mathutils": SimpleNamespace(Vector=FakeVector),
                        },
                    ),
                    self.assertRaisesRegex(HandlerError, message),
                ):
                    handlers._create_diagnostic_geometry(
                        "overhang",
                        None,
                        {
                            "build_direction": (0.0, 0.0, 1.0),
                            "overhang_angle_degrees": 45.0,
                        },
                        None,
                        [],
                        [],
                        [],
                    )

                bmesh_module.new.assert_not_called()
                materials.new.assert_not_called()

            for case, vertices, attributes, message in (
                (
                    "vertex_groups",
                    [
                        SimpleNamespace(
                            groups=Sized(MAX_DIAGNOSTIC_ATTRIBUTE_VALUES + 1)
                        )
                    ],
                    [],
                    "copied attribute values",
                ),
                (
                    "string_attribute",
                    Sized(3),
                    [SimpleNamespace(data_type="STRING", data=Sized(1))],
                    "unbounded string attribute",
                ),
            ):
                geometry = SimpleNamespace(
                    name="Oversized",
                    type="MESH",
                    hide_render=False,
                    visible_camera=True,
                    data=SimpleNamespace(
                        vertices=vertices,
                        edges=Sized(3),
                        polygons=Sized(1),
                        loops=Sized(3),
                        attributes=attributes,
                    ),
                )
                depsgraph.object_instances = [
                    SimpleNamespace(object=geometry, show_self=True)
                ]
                with (
                    self.subTest(case=case),
                    patch.dict(
                        sys.modules,
                        {
                            "bmesh": bmesh_module,
                            "mathutils": SimpleNamespace(Vector=FakeVector),
                        },
                    ),
                    self.assertRaisesRegex(HandlerError, message),
                ):
                    handlers._create_diagnostic_geometry(
                        "overhang",
                        None,
                        {
                            "build_direction": (0.0, 0.0, 1.0),
                            "overhang_angle_degrees": 45.0,
                        },
                        None,
                        [],
                        [],
                        [],
                    )

                bmesh_module.new.assert_not_called()
                materials.new.assert_not_called()
            handlers.close()

    def test_diagnostic_expanded_topology_is_checked_before_combined_buffers(self) -> None:
        class Sized:
            def __init__(self, length: int, values: list[object]) -> None:
                self.length = length
                self.values = values

            def __len__(self) -> int:
                return self.length

            def __iter__(self):
                return iter(self.values)

        vertices = Sized(3, [object(), object(), object()])
        edges = Sized(3, [object(), object(), object()])
        face = SimpleNamespace(loops=[object(), object(), object()], material_index=0)
        bm = SimpleNamespace(
            verts=vertices,
            edges=edges,
            faces=[face],
            from_mesh=Mock(),
            free=Mock(),
        )

        def expand_geometry(
            *_args: object, **_kwargs: object
        ) -> dict[str, list[object]]:
            vertices.length = MAX_DIAGNOSTIC_VERTICES + 1
            return {"geom_cut": []}

        bmesh_module = SimpleNamespace(
            new=Mock(return_value=bm),
            ops=SimpleNamespace(bisect_plane=Mock(side_effect=expand_geometry)),
        )
        geometry = SimpleNamespace(
            name="Body",
            type="MESH",
            hide_render=False,
            visible_camera=True,
            data=object(),
        )
        instance = SimpleNamespace(object=geometry, show_self=True, matrix_world=object())
        depsgraph = SimpleNamespace(object_instances=[instance])

        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            scene = SimpleNamespace(cycles=SimpleNamespace(device=None))
            meshes = SimpleNamespace(new=Mock())
            bpy = SimpleNamespace(
                data=SimpleNamespace(scenes=[scene], meshes=meshes),
                context=SimpleNamespace(
                    evaluated_depsgraph_get=Mock(return_value=depsgraph)
                ),
            )
            handlers = BlenderHandlers(config(root, 9876), lambda: False, bpy)

            with (
                patch.dict(
                    sys.modules,
                    {
                        "bmesh": bmesh_module,
                        "mathutils": SimpleNamespace(Vector=FakeVector),
                    },
                ),
                patch.object(handlers, "_create_diagnostic_material", return_value=object()),
                patch.object(handlers, "_transform_diagnostic_mesh"),
                patch.object(handlers, "_append_diagnostic_faces") as append_faces,
                self.assertRaisesRegex(HandlerError, "rendered geometry exceeds.*vertices"),
            ):
                handlers._create_diagnostic_geometry(
                    "cross_section",
                    None,
                    {"axis": "Z", "axis_index": 2, "position": 0.0},
                    (1, 3, 3, 1, 3, 0),
                    [],
                    [],
                    [],
                )

            append_faces.assert_not_called()
            meshes.new.assert_not_called()
            bm.free.assert_called_once_with()
            handlers.close()

    def test_diagnostic_preflight_does_not_allocate_non_mesh_conversion(self) -> None:
        geometry = SimpleNamespace(
            name="Curve",
            type="CURVE",
            hide_render=False,
            visible_camera=True,
            to_mesh=Mock(),
        )
        depsgraph = SimpleNamespace(
            object_instances=[SimpleNamespace(object=geometry, show_self=True)]
        )
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            scene = SimpleNamespace(cycles=SimpleNamespace(device=None))
            bpy = SimpleNamespace(
                data=SimpleNamespace(scenes=[scene]),
                context=SimpleNamespace(
                    evaluated_depsgraph_get=Mock(return_value=depsgraph)
                ),
            )
            handlers = BlenderHandlers(config(root, 9876), lambda: False, bpy)

            with (
                patch.object(handlers, "_missing_renderable_names", return_value=[]),
                patch.object(handlers, "_render_bounds") as render_bounds,
                self.assertRaisesRegex(HandlerError, "requires mesh geometry"),
            ):
                handlers.dispatch(
                    "render_diagnostic",
                    {"path": "diagnostics/curve.png", "mode": "overhang"},
                )

            geometry.to_mesh.assert_not_called()
            render_bounds.assert_not_called()
            handlers.close()

    def test_render_visibility_uses_evaluated_instance_state(self) -> None:
        def geometry(**overrides: object) -> SimpleNamespace:
            fields = {
                "type": "MESH",
                "hide_render": False,
                "visible_camera": True,
            }
            fields.update(overrides)
            return SimpleNamespace(**fields)

        instance = lambda obj, show_self=True: SimpleNamespace(
            object=obj,
            show_self=show_self,
        )

        self.assertTrue(
            BlenderHandlers._is_renderable_geometry(instance(geometry()))
        )
        self.assertFalse(
            BlenderHandlers._is_renderable_geometry(
                instance(geometry(), show_self=False)
            )
        )
        self.assertFalse(
            BlenderHandlers._is_renderable_geometry(
                instance(geometry(hide_render=True))
            )
        )
        self.assertFalse(
            BlenderHandlers._is_renderable_geometry(
                instance(geometry(visible_camera=False))
            )
        )
        self.assertFalse(
            BlenderHandlers._is_renderable_geometry(
                instance(geometry(type="EMPTY"))
            )
        )

    def test_scene_info_returns_a_bounded_page(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            meshes = [self._mesh(f"Mesh{index}") for index in range(5)]
            scene = SimpleNamespace(name="Scene", objects=meshes)
            bpy = SimpleNamespace(
                app=SimpleNamespace(version_string="5.2.0", background=True),
                data=SimpleNamespace(
                    scenes=[SimpleNamespace(cycles=SimpleNamespace(device=None))],
                ),
                context=SimpleNamespace(
                    scene=scene,
                    view_layer=SimpleNamespace(objects=SimpleNamespace(active=meshes[0])),
                ),
            )
            handlers = BlenderHandlers(config(root, 9876), lambda: False, bpy)

            page = handlers.dispatch("get_scene_info", {"offset": 1, "limit": 2})

            self.assertEqual(page["object_count"], 5)
            self.assertEqual([item["name"] for item in page["objects"]], ["Mesh1", "Mesh2"])
            self.assertEqual(page["next_offset"], 3)
            handlers.close()

    def test_primitive_rejects_name_collision_before_creation(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            existing = self._mesh("Existing")
            create = Mock()
            bpy = SimpleNamespace(
                data=SimpleNamespace(
                    scenes=[SimpleNamespace(cycles=SimpleNamespace(device=None))],
                    objects=SimpleNamespace(
                        get=lambda name: existing if name == "Existing" else None
                    ),
                ),
                ops=SimpleNamespace(
                    mesh=SimpleNamespace(primitive_cube_add=create),
                ),
            )
            handlers = BlenderHandlers(config(root, 9876), lambda: False, bpy)

            with self.assertRaisesRegex(HandlerError, "object already exists"):
                handlers.dispatch(
                    "create_primitive",
                    {"primitive": "cube", "name": "Existing"},
                )

            create.assert_not_called()
            handlers.close()

    def test_eevee_render_rejects_cycles_samples_before_side_effects(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            render = SimpleNamespace(engine="unchanged")
            scene = SimpleNamespace(
                cycles=SimpleNamespace(device=None),
                render=render,
            )
            render_operation = Mock()
            bpy = SimpleNamespace(
                data=SimpleNamespace(scenes=[scene]),
                context=SimpleNamespace(scene=scene),
                ops=SimpleNamespace(
                    render=SimpleNamespace(render=render_operation),
                ),
            )
            handlers = BlenderHandlers(config(root, 9876), lambda: False, bpy)

            with self.assertRaisesRegex(HandlerError, "only valid for CYCLES"):
                handlers.dispatch(
                    "render_still",
                    {
                        "path": "renders/eevee.png",
                        "engine": "EEVEE",
                        "samples": 32,
                    },
                )

            self.assertEqual(render.engine, "unchanged")
            render_operation.assert_not_called()
            self.assertFalse((root / "workspace" / "renders").exists())
            handlers.close()

    def test_render_rejects_invalid_engine_before_side_effects(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            render = SimpleNamespace(engine="unchanged")
            scene = SimpleNamespace(
                cycles=SimpleNamespace(device=None),
                render=render,
            )
            render_operation = Mock()
            bpy = SimpleNamespace(
                data=SimpleNamespace(scenes=[scene]),
                context=SimpleNamespace(scene=scene),
                ops=SimpleNamespace(
                    render=SimpleNamespace(render=render_operation),
                ),
            )
            handlers = BlenderHandlers(config(root, 9876), lambda: False, bpy)

            for engine in ("WORKBENCH", ["CYCLES"]):
                with self.subTest(engine=engine), self.assertRaisesRegex(
                    HandlerError, "engine must be"
                ):
                    handlers.dispatch(
                        "render_still",
                        {"path": "renders/invalid.png", "engine": engine},
                    )

            self.assertEqual(render.engine, "unchanged")
            render_operation.assert_not_called()
            self.assertFalse((root / "workspace" / "renders").exists())
            handlers.close()

    def test_invalid_primitive_name_does_not_create_an_object(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            operation = Mock()
            bpy = SimpleNamespace(
                data=SimpleNamespace(
                    scenes=[SimpleNamespace(cycles=SimpleNamespace(device=None))]
                ),
                ops=SimpleNamespace(
                    mesh=SimpleNamespace(primitive_cube_add=operation)
                ),
            )
            handlers = BlenderHandlers(config(root, 9876), lambda: False, bpy)
            with self.assertRaises(HandlerError):
                handlers.dispatch("create_primitive", {"name": ""})
            operation.assert_not_called()
            handlers.close()

    def test_primitive_rejects_backend_invalid_segment_count_before_creation(
        self,
    ) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            operation = Mock()
            bpy = SimpleNamespace(
                data=SimpleNamespace(
                    scenes=[SimpleNamespace(cycles=SimpleNamespace(device=None))]
                ),
                ops=SimpleNamespace(
                    mesh=SimpleNamespace(primitive_cylinder_add=operation)
                ),
            )
            handlers = BlenderHandlers(config(root, 9876), lambda: False, bpy)

            with self.assertRaisesRegex(HandlerError, "between 3 and 1024"):
                handlers.dispatch(
                    "create_primitive", {"primitive": "cylinder", "vertices": 2}
                )

            operation.assert_not_called()
            handlers.close()

    def test_invalid_render_arguments_do_not_mutate_scene_or_workspace(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            render = SimpleNamespace(engine="unchanged")
            scene = SimpleNamespace(
                cycles=SimpleNamespace(device=None),
                render=render,
            )
            bpy = SimpleNamespace(data=SimpleNamespace(scenes=[scene]))
            value = config(root, 9876)
            handlers = BlenderHandlers(value, lambda: False, bpy)
            with self.assertRaises(HandlerError):
                handlers.dispatch(
                    "render_still",
                    {"path": "new/render.png", "width": 0, "height": 256},
                )
            for params, message in (
                (
                    {
                        "path": "new/product.png",
                        "objects": ["Body"],
                        "presentation": {"profile": "studio_neutral"},
                        "width": 8192,
                        "height": 8192,
                    },
                    "pixel output limit",
                ),
                (
                    {
                        "path": "new/product.png",
                        "objects": ["Body"],
                        "presentation": {
                            "profile": "studio_dark",
                            "view": {"elevation_degrees": -1.0},
                        },
                    },
                    "ground cannot occlude",
                ),
            ):
                with self.assertRaisesRegex(HandlerError, message):
                    handlers.dispatch("render_product", params)
            self.assertEqual(render.engine, "unchanged")
            self.assertFalse((value.workspace_root / "new").exists())
            handlers.close()

    def test_unsafe_render_parent_is_rejected_before_scene_mutation(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            value = config(root, 9876)
            outside = root / "outside"
            outside.mkdir()
            (value.workspace_root / "unsafe").symlink_to(
                outside,
                target_is_directory=True,
            )
            render = SimpleNamespace(engine="unchanged")
            scene = SimpleNamespace(
                cycles=SimpleNamespace(device=None),
                render=render,
            )
            render_operation = Mock()
            bpy = SimpleNamespace(
                data=SimpleNamespace(scenes=[scene]),
                context=SimpleNamespace(scene=scene),
                ops=SimpleNamespace(
                    render=SimpleNamespace(render=render_operation),
                ),
            )
            handlers = BlenderHandlers(value, lambda: False, bpy)

            with self.assertRaises(WorkspaceError):
                handlers.dispatch(
                    "render_still",
                    {"path": "unsafe/render.png", "width": 256, "height": 256},
                )

            self.assertEqual(render.engine, "unchanged")
            render_operation.assert_not_called()
            self.assertFalse((outside / "render.png").exists())
            handlers.close()

    def test_clear_scene_preserves_objects_still_used_by_another_scene(self) -> None:
        class Links(list):
            def unlink(self, value: SimpleNamespace) -> None:
                value.users -= 1
                super().remove(value)

        class Datablocks(list):
            def __init__(self, values: list[SimpleNamespace]):
                super().__init__(values)
                self.remove_calls: list[tuple[SimpleNamespace, bool]] = []

            def remove(self, value: SimpleNamespace, *, do_unlink: bool) -> None:
                self.remove_calls.append((value, do_unlink))
                for linked in getattr(value, "objects", []):
                    linked.users -= 1
                super().remove(value)

        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            current_only = SimpleNamespace(users=1)
            shared_object = SimpleNamespace(users=1)
            exclusive_nested = SimpleNamespace(users=1)
            shared_collection = SimpleNamespace(
                users=2, children=Links([]), objects=Links([shared_object])
            )
            exclusive_collection = SimpleNamespace(
                users=1,
                children=Links([]),
                objects=Links([exclusive_nested]),
            )
            root_collection = SimpleNamespace(
                objects=Links([current_only]),
                children=Links([shared_collection, exclusive_collection]),
            )
            scene = SimpleNamespace(
                objects=[current_only, shared_object, exclusive_nested],
                collection=root_collection,
                cycles=SimpleNamespace(device=None),
            )
            other_root_collection = SimpleNamespace(
                objects=Links([]), children=Links([shared_collection])
            )
            other_scene = SimpleNamespace(
                collection=other_root_collection,
                cycles=SimpleNamespace(device=None),
            )
            exclusive_layer_collection = SimpleNamespace(
                name="Exclusive", collection=exclusive_collection
            )
            root_layer_collection = SimpleNamespace(
                name="Scene Collection", children=[exclusive_layer_collection]
            )
            view_layer = SimpleNamespace(
                active_layer_collection=SimpleNamespace(name="Exclusive"),
                layer_collection=root_layer_collection,
                objects=SimpleNamespace(active=current_only),
                update=Mock(),
            )
            objects = Datablocks([current_only, shared_object, exclusive_nested])
            collections = Datablocks([shared_collection, exclusive_collection])

            def batch_remove(values: list[SimpleNamespace]) -> None:
                for value in values:
                    objects.remove(value, do_unlink=False)

            bpy = SimpleNamespace(
                data=SimpleNamespace(
                    scenes=[scene, other_scene],
                    objects=objects,
                    collections=collections,
                    batch_remove=Mock(side_effect=batch_remove),
                ),
                context=SimpleNamespace(scene=scene, view_layer=view_layer),
            )
            handlers = BlenderHandlers(config(root, 9876), lambda: False, bpy)
            result = handlers.dispatch("clear_scene", {})
            self.assertEqual(result, {"removed_objects": 3, "freed_objects": 2})
            self.assertEqual(objects, [shared_object])
            self.assertEqual(
                objects.remove_calls,
                [(current_only, False), (exclusive_nested, False)],
            )
            bpy.data.batch_remove.assert_called_once_with(
                [current_only, exclusive_nested]
            )
            self.assertEqual(collections, [shared_collection, exclusive_collection])
            self.assertEqual(collections.remove_calls, [])
            self.assertEqual(root_collection.objects, [])
            self.assertEqual(root_collection.children, [exclusive_collection])
            self.assertEqual(exclusive_collection.objects, [])
            self.assertEqual(shared_collection.objects, [shared_object])
            self.assertEqual(other_root_collection.children, [shared_collection])
            self.assertIs(
                view_layer.active_layer_collection, exclusive_layer_collection
            )
            self.assertIsNone(view_layer.objects.active)
            self.assertEqual(view_layer.update.call_count, 2)
            handlers.close()

    def test_clear_scene_keeps_a_valid_active_collection(self) -> None:
        class CollectionLinks(list):
            def link(self, value: SimpleNamespace) -> None:
                value.users += 1
                self.append(value)

        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            root_collection = SimpleNamespace(
                objects=[], children=CollectionLinks([])
            )
            scene = SimpleNamespace(
                objects=[],
                collection=root_collection,
                cycles=SimpleNamespace(device=None),
            )
            workspace_collection = SimpleNamespace(
                name="Printable", users=0, children=[], objects=[]
            )
            root_layer_collection = SimpleNamespace(children=[])

            def update_layer() -> None:
                root_layer_collection.children = [
                    SimpleNamespace(collection=collection)
                    for collection in root_collection.children
                ]

            view_layer = SimpleNamespace(
                active_layer_collection=None,
                layer_collection=root_layer_collection,
                objects=SimpleNamespace(active=None),
                update=Mock(side_effect=update_layer),
            )
            collections = SimpleNamespace(
                new=Mock(return_value=workspace_collection)
            )
            bpy = SimpleNamespace(
                data=SimpleNamespace(
                    scenes=[scene],
                    objects=[],
                    collections=collections,
                    batch_remove=Mock(),
                ),
                context=SimpleNamespace(scene=scene, view_layer=view_layer),
            )
            handlers = BlenderHandlers(config(root, 9876), lambda: False, bpy)

            result = handlers.dispatch("clear_scene", {})

            self.assertEqual(result, {"removed_objects": 0, "freed_objects": 0})
            collections.new.assert_called_once_with("Printable")
            self.assertEqual(root_collection.children, [workspace_collection])
            self.assertIs(
                view_layer.active_layer_collection,
                root_layer_collection.children[0],
            )
            self.assertEqual(view_layer.update.call_count, 2)
            bpy.data.batch_remove.assert_not_called()
            handlers.close()

    def test_rename_rejects_collision_before_mutating_either_object(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            first = self._mesh("First")
            second = self._mesh("Second")
            objects = SimpleNamespace(
                get=lambda name: {"First": first, "Second": second}.get(name)
            )
            bpy = SimpleNamespace(
                data=SimpleNamespace(
                    scenes=[SimpleNamespace(cycles=SimpleNamespace(device=None))],
                    objects=objects,
                )
            )
            handlers = BlenderHandlers(config(root, 9876), lambda: False, bpy)

            with self.assertRaisesRegex(HandlerError, "object already exists"):
                handlers.dispatch(
                    "rename_object",
                    {"name": "First", "new_name": "Second"},
                )

            self.assertEqual(first.name, "First")
            self.assertEqual(second.name, "Second")
            handlers.close()

    def test_boolean_validates_name_collision_before_adding_modifier(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            target = self._mesh("Target")
            target.modifiers = SimpleNamespace(new=Mock())
            operand = self._mesh("Operand")
            collision = self._mesh("Existing")
            objects = SimpleNamespace(
                get=lambda name: {
                    "Target": target,
                    "Operand": operand,
                    "Existing": collision,
                }.get(name)
            )
            select_all = Mock()
            bpy = SimpleNamespace(
                data=SimpleNamespace(
                    scenes=[SimpleNamespace(cycles=SimpleNamespace(device=None))],
                    objects=objects,
                ),
                ops=SimpleNamespace(object=SimpleNamespace(select_all=select_all)),
            )
            handlers = BlenderHandlers(config(root, 9876), lambda: False, bpy)

            with self.assertRaisesRegex(HandlerError, "object already exists"):
                handlers.dispatch(
                    "boolean",
                    {
                        "target": "Target",
                        "operand": "Operand",
                        "operation": "UNION",
                        "result_name": "Existing",
                    },
                )

            target.modifiers.new.assert_not_called()
            select_all.assert_not_called()
            handlers.close()

    def test_job_restore_stages_checkpoint_and_disables_embedded_scripts(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            value = config(root, 9876)
            checkpoint = value.workspace_root / ".printable" / "jobs" / "checkpoint.blend"
            checkpoint.parent.mkdir(parents=True)
            checkpoint.write_bytes(b"blend")
            scene = SimpleNamespace(name="Scene", objects=[], frame_current=17)
            active = SimpleNamespace(active=None)
            open_mainfile = Mock(return_value={"FINISHED"})
            bpy = SimpleNamespace(
                app=SimpleNamespace(version_string="5.2.0", background=True),
                data=SimpleNamespace(
                    scenes=[SimpleNamespace(cycles=SimpleNamespace(device=None))],
                ),
                context=SimpleNamespace(
                    scene=scene,
                    view_layer=SimpleNamespace(objects=active),
                ),
                ops=SimpleNamespace(
                    wm=SimpleNamespace(open_mainfile=open_mainfile),
                ),
            )
            handlers = BlenderHandlers(value, lambda: False, bpy)

            restored = handlers.dispatch(
                "job_restore_checkpoint",
                {"path": ".printable/jobs/checkpoint.blend", "expected_sha256": hashlib.sha256(b"blend").hexdigest()},
            )

            self.assertEqual(restored["objects"], [])
            self.assertEqual(restored["frame_current"], 17)
            open_mainfile.reset_mock()
            with self.assertRaisesRegex(HandlerError, "source digest"):
                handlers.dispatch("job_restore_checkpoint", {
                    "path": ".printable/jobs/checkpoint.blend", "expected_sha256": "0" * 64,
                })
            open_mainfile.assert_not_called()
            handlers.dispatch("job_restore_checkpoint", {"path": ".printable/jobs/checkpoint.blend"})
            called_path = Path(open_mainfile.call_args.kwargs["filepath"])
            self.assertNotEqual(called_path, value.workspace_root / "checkpoint.blend")
            self.assertFalse(called_path.exists())
            self.assertEqual(
                open_mainfile.call_args.kwargs,
                {
                    "filepath": str(called_path),
                    "load_ui": False,
                    "use_scripts": False,
                },
            )
            handlers.close()

    def test_job_save_checkpoints_the_live_session_in_reserved_storage(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            value = config(root, 9876)

            def save_mainfile(**kwargs: object) -> set[str]:
                Path(str(kwargs["filepath"])).write_bytes(b"live-session")
                return {"FINISHED"}

            save_as_mainfile = Mock(side_effect=save_mainfile)
            bpy = SimpleNamespace(
                data=SimpleNamespace(
                    scenes=[SimpleNamespace(cycles=SimpleNamespace(device=None))],
                ),
                ops=SimpleNamespace(
                    wm=SimpleNamespace(save_as_mainfile=save_as_mainfile),
                ),
            )
            handlers = BlenderHandlers(value, lambda: False, bpy)

            saved = handlers.dispatch(
                "job_save_checkpoint",
                {"path": ".printable/jobs/job-id/pre-job-session.blend"},
            )

            destination = (
                value.workspace_root
                / ".printable"
                / "jobs"
                / "job-id"
                / "pre-job-session.blend"
            )
            self.assertEqual(
                saved, {"path": ".printable/jobs/job-id/pre-job-session.blend"}
            )
            self.assertEqual(destination.read_bytes(), b"live-session")
            self.assertTrue(save_as_mainfile.call_args.kwargs["copy"])
            self.assertFalse(save_as_mainfile.call_args.kwargs["check_existing"])
            handlers.close()


class HealthcheckTests(unittest.TestCase):
    def test_fresh_matching_markers_are_healthy(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            state = Path(temporary)
            (state / "ready.json").write_text('{"pid":42}', encoding="utf-8")
            (state / "live.json").write_text(
                json.dumps(
                    {
                        "pid": 42,
                        "server_alive": True,
                        "main_thread_alive": True,
                        "monotonic_seconds": time.monotonic(),
                    }
                ),
                encoding="utf-8",
            )
            with patch.dict(
                "os.environ", {"PRINTABLE_BLENDER_STATE_DIR": str(state)}, clear=False
            ):
                self.assertEqual(healthcheck(), 0)

    def test_stale_or_dead_markers_are_unhealthy(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            state = Path(temporary)
            (state / "ready.json").write_text('{"pid":42}', encoding="utf-8")
            for server_alive, observed in (
                (False, time.monotonic()),
                (True, time.monotonic() - 30),
            ):
                (state / "live.json").write_text(
                    json.dumps(
                        {
                            "pid": 42,
                            "server_alive": server_alive,
                            "main_thread_alive": True,
                            "monotonic_seconds": observed,
                        }
                    ),
                    encoding="utf-8",
                )
                with self.subTest(
                    server_alive=server_alive, observed=observed
                ), patch.dict(
                    "os.environ",
                    {"PRINTABLE_BLENDER_STATE_DIR": str(state)},
                    clear=False,
                ):
                    self.assertEqual(healthcheck(), 1)

    def test_overdue_main_thread_is_unhealthy(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            state = Path(temporary)
            (state / "ready.json").write_text('{"pid":42}', encoding="utf-8")
            (state / "live.json").write_text(
                json.dumps(
                    {
                        "pid": 42,
                        "server_alive": True,
                        "main_thread_alive": False,
                        "monotonic_seconds": time.monotonic(),
                    }
                ),
                encoding="utf-8",
            )
            with patch.dict(
                "os.environ", {"PRINTABLE_BLENDER_STATE_DIR": str(state)}, clear=False
            ):
                self.assertEqual(healthcheck(), 1)


class RuntimeShutdownTests(unittest.TestCase):
    def test_startup_failure_logs_the_safe_diagnostic(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            value = replace(
                config(Path(temporary), 9876),
                render_device="OPTIX",
            )
            runtime = BridgeRuntime(value, NoopExecutionWatchdog())
            diagnostic = "OPTIX requested but no compatible device is available"

            with patch(
                "printable_bridge.runtime.BlenderHandlers",
                side_effect=HandlerStartupError(diagnostic),
            ), patch(
                "printable_bridge.runtime.enforce_process_file_size_limit"
            ), self.assertLogs("printable_bridge.runtime", level="ERROR") as logs:
                self.assertEqual(runtime.run(), 1)

            self.assertIn(diagnostic, "\n".join(logs.output))

    def test_health_snapshot_marks_an_overdue_main_thread_unhealthy(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            runtime = BridgeRuntime(
                config(Path(temporary), 9876), NoopExecutionWatchdog()
            )
            runtime._server = SimpleNamespace(alive=True)
            runtime._current = WorkItem(
                request=request("render_still"),
                deadline=time.monotonic() - 1,
                run_budget_seconds=1.0,
            )

            snapshot = runtime._health_snapshot()

            self.assertFalse(snapshot["main_thread_alive"])
            self.assertEqual(snapshot["state"], "busy")
            self.assertEqual(snapshot["command"], "render_still")

    def test_signal_handler_defers_terminal_transition_to_main_thread(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            runtime = BridgeRuntime(
                config(Path(temporary), 9876), NoopExecutionWatchdog()
            )
            current = Mock()
            runtime._current = current

            runtime._handle_signal()

            self.assertTrue(runtime._shutdown_requested)
            self.assertTrue(runtime._shutdown.is_set())
            self.assertTrue(runtime._pump_wakeup.is_set())
            self.assertIsNotNone(runtime._shutdown_deadline)
            current.shutdown.assert_not_called()
            runtime._begin_shutdown()
            current.shutdown.assert_called_once_with()

    def test_signal_wakes_idle_pump_without_waiting_for_heartbeat(self) -> None:
        entered_wait = threading.Event()

        class ObservedEvent:
            def __init__(self) -> None:
                self._event = threading.Event()

            def wait(self, timeout: float) -> bool:
                entered_wait.set()
                return self._event.wait(timeout)

            def set(self) -> None:
                self._event.set()

            def clear(self) -> None:
                self._event.clear()

        with tempfile.TemporaryDirectory() as temporary:
            value = replace(
                config(Path(temporary), 9876),
                heartbeat_seconds=60.0,
            )
            runtime = BridgeRuntime(value, NoopExecutionWatchdog())
            runtime._pump_wakeup = ObservedEvent()
            runtime._server = SimpleNamespace(alive=True)
            runtime._handlers = Mock()
            pump = threading.Thread(target=runtime._pump, daemon=True)
            pump.start()

            self.assertTrue(entered_wait.wait(timeout=0.5))
            runtime._handle_signal()
            pump.join(timeout=0.5)

            self.assertFalse(pump.is_alive())
            self.assertIsNotNone(runtime._shutdown_deadline)
            self.assertGreater(runtime._shutdown_deadline, time.monotonic())

    def test_enqueue_between_wakeup_and_clear_is_drained_immediately(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            runtime = BridgeRuntime(
                config(Path(temporary), 9876), NoopExecutionWatchdog()
            )
            item = WorkItem.with_budget(request(), 1.0)
            wait_calls = 0

            class EnqueueOnWait:
                def wait(self, _timeout: float) -> bool:
                    nonlocal wait_calls
                    wait_calls += 1
                    runtime._work_queue.put_nowait(item)
                    return True

                def set(self) -> None:
                    pass

                def clear(self) -> None:
                    pass

            def dispatch(*_args: object) -> dict[str, object]:
                runtime._handle_signal()
                return {}

            runtime._pump_wakeup = EnqueueOnWait()
            runtime._server = SimpleNamespace(alive=True)
            runtime._handlers = Mock()
            runtime._handlers.dispatch.side_effect = dispatch

            runtime._pump()

            runtime._handlers.dispatch.assert_called_once_with(
                item.request.command, item.request.params
            )
            self.assertEqual(wait_calls, 1)

    def test_item_dequeued_during_signal_is_not_dispatched(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            runtime = BridgeRuntime(
                config(Path(temporary), 9876), NoopExecutionWatchdog()
            )
            item = WorkItem.with_budget(request(), 1.0)
            work_queue = Mock()

            def receive_during_signal(*_args: object, **_kwargs: object) -> WorkItem:
                runtime._handle_signal()
                return item

            work_queue.get_nowait.side_effect = receive_during_signal
            runtime._work_queue = work_queue
            runtime._server = SimpleNamespace(alive=True)
            runtime._handlers = Mock()

            runtime._pump()

            self.assertEqual(item.phase, RequestPhase.SHUTDOWN)
            runtime._handlers.dispatch.assert_not_called()
            work_queue.task_done.assert_called_once_with()


class ServerTests(unittest.TestCase):
    def test_native_export_can_complete_after_the_ordinary_bridge_timeout(self) -> None:
        export = parse_request({
            "id": str(uuid.uuid4()), "command": "export_project_blender",
            "params": {"project_id": "organic", "files": ["model.blend"],
                       "entrypoint": "model.blend", "output_path": "export.zip",
                       "timeout_seconds": 120},
        })
        item = WorkItem(request=export, deadline=30.0,
                        run_budget_seconds=_request_budget_seconds(30.0, export))
        self.assertTrue(item.start(now=1.0))
        response = success(export.request_id, {"path": "projects/organic/export.zip"})
        self.assertTrue(item.complete(response, now=100.0))
        self.assertEqual(item.await_response(), response)
        queued = WorkItem(request=export, deadline=30.0,
                          run_budget_seconds=_request_budget_seconds(30.0, export))
        self.assertFalse(queued.start(now=31.0))

    def test_caller_work_time_is_added_to_long_running_request_budgets(self) -> None:
        for params, expected in (({}, 35.0), ({"timeout_seconds": 30.0}, 35.0),
                                 ({"timeout_seconds": 120.0}, 125.0)):
            capture = parse_request({"id": str(uuid.uuid4()), "command": "capture_native_view",
                                     "params": {"path": "capture.png", **params}})
            self.assertEqual(_request_budget_seconds(5.0, capture), expected)
        execute = parse_request(
            {
                "id": str(uuid.uuid4()),
                "command": "execute_code",
                "params": {"code": "result = 1", "timeout_seconds": 3600.0},
            }
        )
        execute_default = parse_request(
            {
                "id": str(uuid.uuid4()),
                "command": "execute_code",
                "params": {"code": "result = 1"},
            }
        )
        render = parse_request(
            {
                "id": str(uuid.uuid4()),
                "command": "render_still",
                "params": {"path": "preview.png", "timeout_seconds": 7200.0},
            }
        )
        render_default = parse_request(
            {
                "id": str(uuid.uuid4()),
                "command": "render_still",
                "params": {"path": "preview.png"},
            }
        )
        product = parse_request(
            {
                "id": str(uuid.uuid4()),
                "command": "render_product",
                "params": {
                    "path": "product.png",
                    "objects": ["Body"],
                    "presentation": {"profile": "studio_neutral"},
                    "timeout_seconds": 9000.0,
                },
            }
        )
        job_frame = parse_request(
            {
                "id": str(uuid.uuid4()),
                "command": "job_render_frame",
                "params": {
                    "path": ".printable/jobs/frame.png",
                    "frame": 1,
                    "timeout_seconds": 10_800.0,
                },
            }
        )
        mechanical_prepare = parse_request(
            {
                "id": str(uuid.uuid4()),
                "command": "job_prepare_mechanical_rotation",
                "params": {"timeout_seconds": 12_000.0},
            }
        )
        views = parse_request(
            {
                "id": str(uuid.uuid4()),
                "command": "render_views",
                "params": {"views": [], "timeout_seconds": 14_400.0},
            }
        )
        diagnostic = parse_request(
            {
                "id": str(uuid.uuid4()),
                "command": "render_diagnostic",
                "params": {
                    "path": "diagnostic.png",
                    "mode": "overhang",
                    "timeout_seconds": 21_600.0,
                },
            }
        )
        diagnostic_default = parse_request(
            {
                "id": str(uuid.uuid4()),
                "command": "render_diagnostic",
                "params": {"path": "diagnostic.png", "mode": "overhang"},
            }
        )
        job_save = parse_request(
            {
                "id": str(uuid.uuid4()),
                "command": "job_save_checkpoint",
                "params": {"path": ".printable/jobs/id/session.blend"},
            }
        )
        job_restore = parse_request(
            {
                "id": str(uuid.uuid4()),
                "command": "job_restore_checkpoint",
                "params": {"path": ".printable/jobs/id/session.blend"},
            }
        )

        self.assertEqual(_request_budget_seconds(120.0, execute), 3720.0)
        self.assertEqual(_request_budget_seconds(120.0, execute_default), 240.0)
        self.assertEqual(_request_budget_seconds(120.0, render), 7320.0)
        self.assertEqual(_request_budget_seconds(120.0, render_default), 3720.0)
        self.assertEqual(_request_budget_seconds(120.0, product), 9120.0)
        self.assertEqual(_request_budget_seconds(120.0, job_frame), 10_920.0)
        self.assertEqual(
            _request_budget_seconds(120.0, mechanical_prepare), 12_120.0
        )
        self.assertEqual(_request_budget_seconds(120.0, views), 14_520.0)
        self.assertEqual(_request_budget_seconds(120.0, diagnostic), 21_720.0)
        self.assertEqual(
            _request_budget_seconds(120.0, diagnostic_default), 3720.0
        )
        self.assertEqual(_request_budget_seconds(120.0, job_save), 3720.0)
        self.assertEqual(_request_budget_seconds(120.0, job_restore), 3720.0)
        self.assertEqual(_request_budget_seconds(120.0, request()), 120.0)

    def test_valid_request_round_trips_through_main_thread_queue(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            probe = socket.socket()
            probe.bind(("127.0.0.1", 0))
            port = probe.getsockname()[1]
            probe.close()
            work: queue.Queue[WorkItem] = queue.Queue(maxsize=2)
            work_available = threading.Event()
            server = BridgeServer(config(root, port), work, work_available)
            server.start()
            correlation_id = str(uuid.uuid4())

            def main_thread() -> None:
                self.assertTrue(work_available.wait(timeout=1))
                item = work.get_nowait()
                self.assertTrue(item.start())
                item.complete(success(item.request.request_id, {"objects": []}))
                work.task_done()

            pump = threading.Thread(target=main_thread)
            pump.start()
            connection = socket.create_connection(("127.0.0.1", port), timeout=1)
            connection.sendall(
                encode_json(
                    {
                        "id": correlation_id,
                        "command": "get_scene_info",
                        "params": {},
                    },
                    1024 * 1024,
                )
            )
            response = receive_json(connection, 1024 * 1024)
            self.assertEqual(response["id"], correlation_id)
            self.assertEqual(response["result"], {"objects": []})
            connection.close()
            pump.join()
            server.stop(1.0)

    def test_long_render_expires_in_queue_on_the_ordinary_admission_budget(
        self,
    ) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            probe = socket.socket()
            probe.bind(("127.0.0.1", 0))
            port = probe.getsockname()[1]
            probe.close()
            value = replace(
                config(root, port),
                request_timeout_seconds=0.05,
                heartbeat_seconds=0.01,
            )
            work: queue.Queue[WorkItem] = queue.Queue(maxsize=2)
            server = BridgeServer(value, work)
            server.start()
            connection = socket.create_connection(("127.0.0.1", port), timeout=1)
            started = time.monotonic()
            connection.sendall(
                encode_json(
                    {
                        "id": str(uuid.uuid4()),
                        "command": "render_still",
                        "params": {
                            "path": "preview.png",
                            "timeout_seconds": 3600.0,
                        },
                    },
                    1024 * 1024,
                )
            )

            response = receive_json(connection, 1024 * 1024)
            elapsed = time.monotonic() - started
            item = work.get_nowait()

            self.assertEqual(response["status"], "error")
            self.assertIn("timeout", response["error"])
            self.assertLess(elapsed, 0.5)
            self.assertEqual(item.phase, RequestPhase.TIMED_OUT)
            self.assertFalse(item.start())
            work.task_done()
            connection.close()
            server.stop(1.0)

    def test_execute_response_uses_ordinary_transport_budget(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            probe = socket.socket()
            probe.bind(("127.0.0.1", 0))
            port = probe.getsockname()[1]
            probe.close()
            work: queue.Queue[WorkItem] = queue.Queue(maxsize=2)
            work_available = threading.Event()
            server = BridgeServer(config(root, port), work, work_available)
            observed_timeouts: list[float | None] = []

            def record_send(
                connection: socket.socket, value: object, max_frame_bytes: int
            ) -> None:
                observed_timeouts.append(connection.gettimeout())
                framing_send_json(connection, value, max_frame_bytes)

            def main_thread() -> None:
                self.assertTrue(work_available.wait(timeout=1))
                item = work.get_nowait()
                self.assertTrue(item.start())
                item.complete(success(item.request.request_id, {"completed": True}))
                work.task_done()

            with patch("printable_bridge.server.send_json", record_send):
                server.start()
                pump = threading.Thread(target=main_thread)
                pump.start()
                connection = socket.create_connection(
                    ("127.0.0.1", port), timeout=1
                )
                connection.sendall(
                    encode_json(
                        {
                            "id": str(uuid.uuid4()),
                            "command": "execute_code",
                            "params": {
                                "code": "result = 1",
                                "timeout_seconds": 10_000_000_000.0,
                            },
                        },
                        1024 * 1024,
                    )
                )
                response = receive_json(connection, 1024 * 1024)

            self.assertEqual(response["result"], {"completed": True})
            self.assertEqual(len(observed_timeouts), 1)
            self.assertIsNotNone(observed_timeouts[0])
            self.assertGreater(observed_timeouts[0], 0.0)
            self.assertLessEqual(observed_timeouts[0], 1.0)
            connection.close()
            pump.join()
            server.stop(1.0)

    def test_connection_finishing_after_shutdown_is_not_admitted(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            probe = socket.socket()
            probe.bind(("127.0.0.1", 0))
            port = probe.getsockname()[1]
            probe.close()
            work: queue.Queue[WorkItem] = queue.Queue(maxsize=2)
            server = BridgeServer(config(root, port), work)
            request_started = threading.Event()
            release_request = threading.Event()
            correlation_id = str(uuid.uuid4())

            def delayed_receive(*_args: object, **_kwargs: object) -> object:
                request_started.set()
                self.assertTrue(release_request.wait(timeout=1))
                return {
                    "id": correlation_id,
                    "command": "get_scene_info",
                    "params": {},
                }

            with patch("printable_bridge.server.receive_json", delayed_receive):
                server.start()
                connection = socket.create_connection(
                    ("127.0.0.1", port), timeout=1
                )
                self.assertTrue(request_started.wait(timeout=1))
                server.stop_accepting()
                self.assertTrue(server.wait_stopped(1.0))
                release_request.set()
                response = receive_json(connection, 1024 * 1024)

            self.assertEqual(response["status"], "error")
            self.assertIn("shutting down", response["error"])
            self.assertTrue(work.empty())
            connection.close()
            server.close_connections()

    def test_idle_partial_frame_is_closed_immediately_on_stop(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            probe = socket.socket()
            probe.bind(("127.0.0.1", 0))
            port = probe.getsockname()[1]
            probe.close()
            work: queue.Queue[WorkItem] = queue.Queue(maxsize=2)
            server = BridgeServer(config(root, port), work)
            server.start()
            connection = socket.create_connection(("127.0.0.1", port), timeout=1)
            connection.sendall(b"\x00")
            accepted_deadline = time.monotonic() + 1
            while time.monotonic() < accepted_deadline:
                with server._connections_lock:
                    if server._connections:
                        break
                time.sleep(0.005)
            else:
                self.fail("server did not admit the partial-frame connection")

            started = time.monotonic()
            server.stop(1.0)
            elapsed = time.monotonic() - started

            self.assertLess(elapsed, 0.75)
            connection.settimeout(0.2)
            try:
                received = connection.recv(1)
            except ConnectionResetError:
                received = b""
            self.assertEqual(received, b"")
            connection.close()


if __name__ == "__main__":
    unittest.main()
