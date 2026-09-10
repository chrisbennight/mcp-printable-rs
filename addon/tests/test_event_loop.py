from __future__ import annotations

from pathlib import Path
import tempfile
import time
import unittest
from unittest.mock import Mock, patch

from printable_bridge.config import ConfigError, blender_mode
from printable_bridge.lifecycle import RequestPhase, WorkItem
from printable_bridge.runtime import BridgeRuntime
from printable_bridge.supervisor import main as supervisor_main
from printable_bridge.watchdog import NoopExecutionWatchdog
from tests.test_bridge import config, request


class EventLoopTests(unittest.TestCase):
    def setUp(self) -> None:
        temporary = tempfile.TemporaryDirectory()
        self.addCleanup(temporary.cleanup)
        self.runtime = BridgeRuntime(
            config(Path(temporary.name), 9876), NoopExecutionWatchdog()
        )
        self.runtime._server = Mock(alive=True)
        self.runtime._handlers = Mock()
        self.runtime._handlers.dispatch.return_value = {}
        self.runtime._ui_mode = True
        self.runtime._ui_exit = Mock()

    def test_each_tick_yields_after_one_command(self) -> None:
        first = WorkItem.with_budget(request(), 1.0)
        second = WorkItem.with_budget(request(), 1.0)
        self.runtime._work_queue.put_nowait(first)
        self.runtime._work_queue.put_nowait(second)
        self.assertGreater(self.runtime._ui_tick(), 0)
        self.assertEqual(first.phase, RequestPhase.COMPLETED)
        self.assertEqual(second.phase, RequestPhase.QUEUED)
        self.assertGreater(self.runtime._ui_tick(), 0)
        self.assertEqual(second.phase, RequestPhase.COMPLETED)
        self.assertEqual(self.runtime._work_queue.unfinished_tasks, 0)

    def test_idle_tick_never_waits_and_stalled_ui_is_unhealthy(self) -> None:
        self.runtime._pump_wakeup = Mock()
        self.runtime._last_pump = time.monotonic() - 10
        self.assertFalse(self.runtime._health_snapshot()["main_thread_alive"])
        self.assertGreater(self.runtime._ui_tick(), 0)
        self.runtime._pump_wakeup.wait.assert_not_called()
        self.assertTrue(self.runtime._health_snapshot()["main_thread_alive"])

    def test_shutdown_fails_queued_commands_and_stops_timer(self) -> None:
        item = WorkItem.with_budget(request(), 1.0)
        self.runtime._work_queue.put_nowait(item)
        self.runtime._handle_signal()
        self.assertIsNone(self.runtime._ui_tick())
        self.assertEqual(item.phase, RequestPhase.SHUTDOWN)
        self.runtime._handlers.dispatch.assert_not_called()
        self.runtime._ui_exit.assert_called_once_with(0)

    def test_runtime_failure_exits_instead_of_losing_timer_silently(self) -> None:
        self.runtime._server.alive = False
        with self.assertLogs("printable_bridge.runtime", level="ERROR"):
            self.assertIsNone(self.runtime._ui_tick())
        self.runtime._ui_exit.assert_called_once_with(1)

    def test_cleanup_failure_still_exits(self) -> None:
        self.runtime._handle_signal()
        self.runtime._handlers.close.side_effect = RuntimeError("cleanup failure")
        with self.assertLogs("printable_bridge.runtime", level="ERROR"):
            self.assertIsNone(self.runtime._ui_tick())
        self.runtime._ui_exit.assert_called_once_with(1)

    def test_ui_timer_is_registered_as_persistent(self) -> None:
        timers = Mock()
        with patch.object(self.runtime, "_start"):
            self.runtime.start_ui(timers, Mock())
        timers.register.assert_called_once_with(
            self.runtime._ui_tick, first_interval=0.01, persistent=True
        )

    def test_timer_registration_failure_cleans_up_and_propagates(self) -> None:
        timers = Mock()
        timers.register.side_effect = RuntimeError("registration failed")
        with patch.object(self.runtime, "_start"), self.assertRaises(RuntimeError):
            self.runtime.start_ui(timers, Mock())
        self.runtime._server.close_connections.assert_called_once_with()
        self.runtime._handlers.close.assert_called_once_with()

    def test_ui_launch_preserves_process_containment_options(self) -> None:
        with patch.dict("os.environ", {"PRINTABLE_BLENDER_MODE": "ui"}), patch(
            "printable_bridge.supervisor.BlenderSupervisor"
        ) as supervisor:
            supervisor.return_value.run.return_value = 0
            self.assertEqual(supervisor_main(["blender", "/opt/printable"]), 0)
        command = supervisor.call_args.args[0]
        self.assertNotIn("--background", command)
        self.assertIn("--disable-autoexec", command)
        self.assertIn("--offline-mode", command)

    def test_mode_is_explicit_and_background_remains_default(self) -> None:
        self.assertEqual(blender_mode({}), "background")
        with self.assertRaises(ConfigError):
            blender_mode({"PRINTABLE_BLENDER_MODE": "invalid"})


if __name__ == "__main__":
    unittest.main()
